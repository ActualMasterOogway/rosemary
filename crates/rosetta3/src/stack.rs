//! The guest's stack, and the process arguments laid out on it.
//!
//! One per *thread*, since milestone 4 — [`crate::thread`] owns the lifetime.
//! Only the one made for the thread that starts the program has `argv` on it;
//! see [`GuestStack::bare`] for why a spawned thread's does not.
//!
//! ## Why the stack is guest memory and not a `Vec`
//!
//! Anything the guest can address, a shim may be handed a pointer into. A
//! `snprintf` writing into a `char buf[64]` is writing to the guest's stack, and
//! the shim that services it checks the pointer against the guest's mapped
//! regions before touching it. A stack that is not one of those regions would
//! make every such call fail — so the stack is reserved from the same kind of
//! address space the images are, and joins them in
//! [`GuestView`](crate::vm::GuestView).
//!
//! It is its own space rather than another region of the program's, because it
//! is not part of the program: nothing about it is described by the Mach-O, its
//! contents are not a lifter input, and it must not appear among the read-only
//! segment hints microlift folds constants out of. (It is writable, and folding
//! a value the guest can store over is exactly the bug that hint must not have.)
//!
//! ## The low page is left unmapped on purpose
//!
//! A stack that silently wraps into whatever is below it turns a guest
//! recursion bug into corrupted unrelated memory. The lowest page is
//! `PROT_NONE`, so overflow faults.
//!
//! ## What `main` is actually called with
//!
//! An `LC_MAIN` image's entry point is a C function taking
//! `(argc, argv, envp, apple)` — dyld calls it and passes its return value to
//! `exit`. So the arguments go in *registers*, per the guest's SysV ABI, and
//! this module's job is only to have somewhere to point them at: the three NULL
//! -terminated pointer arrays and the bytes they name, written into the top of
//! the stack in the order the kernel would.
//!
//! `apple[]` is the array macOS appends after `envp`. Its first entry is the
//! executable's path — `_NSGetExecutablePath` and `dyld` read it — and the rest
//! are opaque `key=value` strings a program has no business depending on. One
//! entry and a terminator is what a guest can actually observe.
//!
//! An `LC_UNIXTHREAD` image would instead expect this material *on* the stack in
//! the kernel's own layout, with `argc` at `[rsp]`. That form is deliberately
//! not built: no toolchain has emitted it for a macOS executable in fifteen
//! years, and building a layout nothing can produce a fixture for would be
//! untested code. `GuestStack::place_args` is where it would go.

use std::ffi::CString;
use std::ops::Range;

use machish::memory::{AddressSpace, MmapSpace, Placement, Prot, Regions};

use crate::vm::VmError;

/// Default guest stack, matching the 8 MiB macOS gives a main thread.
pub const DEFAULT_STACK: usize = 8 * 1024 * 1024;

/// The value a guest function's return address is set to before it is entered.
///
/// Microlift compares a popped return address against this and reports
/// [`ExitReason::Return`](microlift::core::exit::ExitReason::Return) on a match,
/// which is how the runtime tells "the function I called has finished" from
/// "control left this region". It is fixed in the lifter, not configurable, so
/// it is repeated here rather than derived.
pub const RETURN_SENTINEL: u64 = 0xFFFF_FFFF_FFFF_FFFF;

/// A guest stack, with the process arguments written into its top.
pub struct GuestStack {
    /// The reservation this stack owns, or `None` when the *guest* supplied the
    /// memory — `pthread_attr_setstack`. See [`GuestStack::supplied`].
    space: Option<MmapSpace>,
    base: u64,
    size: usize,
    /// Highest usable address, 16-byte aligned.
    top: u64,
    /// Where the argument material ends and free stack begins.
    args_at: u64,
    argc: u64,
    argv: u64,
    envp: u64,
    apple: u64,
}

impl GuestStack {
    /// Reserve `size` bytes and lay out `args` and the host's environment.
    ///
    /// The environment is the host's, unmodified, which is the honest choice
    /// under high-level emulation: the guest's `getenv` is the *host's*
    /// `getenv`, reading the host's `environ`, so handing the guest a different
    /// set on the stack would produce a program that disagrees with itself.
    pub fn new(size: usize, exe_path: &str, args: &[String]) -> Result<Self, VmError> {
        let mut stack = Self::bare(size)?;
        stack.place_args(exe_path, args)?;
        Ok(stack)
    }

    /// Reserve `size` bytes with nothing laid out on them.
    ///
    /// What a thread the guest spawned needs: a `pthread` start routine is
    /// called with one argument in `rdi` and has no `argc`, no `argv` and no
    /// `apple` — so laying them out again per thread would be building a second
    /// copy of the process's arguments that nothing can reach.
    ///
    /// `size` is the whole **reservation**, of which the lowest page is the
    /// guard and the rest is stack. That is the kernel's arrangement for a main
    /// thread — 8 MiB with the guard inside it, so `pthread_get_stacksize_np`
    /// reports one page less — and it is deliberately *not* libpthread's
    /// arrangement for a spawned thread, where the requested size is the usable
    /// part and the guard is extra. Which of the two a caller wants is a
    /// question about the API it is standing in for, so the caller adds the page
    /// when it means the second: see
    /// [`StackRequest`](crate::thread::StackRequest).
    pub fn bare(size: usize) -> Result<Self, VmError> {
        let page = machish::memory::host_page_size() as usize;
        let size = size.next_multiple_of(page).max(page * 4);

        let mut space = MmapSpace::new();
        let reservation = space
            .reserve(size, page, Placement::Anywhere)
            .map_err(|source| VmError::Stack { source })?;
        // Guard page: overflow faults instead of corrupting whatever the kernel
        // happened to map below us.
        space
            .protect(reservation, page, Prot::NONE)
            .map_err(|source| VmError::Stack { source })?;

        let top = (reservation + size as u64) & !0xf;
        Ok(GuestStack {
            space: Some(space),
            // The guard page is not stack, and saying so is what keeps
            // `pthread_get_stacksize_np` agreeing with the host's answer for a
            // thread of the same size — and what keeps the region list from
            // offering a shim a pointer into a `PROT_NONE` page.
            base: reservation + page as u64,
            size: size - page,
            top,
            args_at: top,
            argc: 0,
            argv: 0,
            envp: 0,
            apple: 0,
        })
    }

    /// A stack over memory the **guest** already owns, running down from `top`.
    ///
    /// What `pthread_attr_setstack` and `pthread_attr_setstackaddr` describe: a
    /// program that manages its own thread stacks — a scripting runtime placing
    /// a coroutine's, a game pooling them — hands `pthread_create` a buffer it
    /// allocated, and its frames belong *there*. Under this runtime the guest's
    /// frames are the ones that count, so the buffer is used for exactly what
    /// the guest asked it to be used for and the host thread keeps a stack of
    /// its own for the JIT's frames. See [`crate::shims::pthread`].
    ///
    /// Nothing is reserved, protected or freed here, and all three are
    /// deliberate: the memory is the guest's, it is already mapped, and a guard
    /// page written over the page below it would be written over whatever the
    /// guest put there. An overflow of a supplied stack corrupts the guest's own
    /// memory, which is precisely what it does natively.
    ///
    /// `top` is exclusive — the highest address *above* the stack, which is what
    /// Darwin's `pthread_get_stackaddr_np` reports and what
    /// `pthread_attr_getstackaddr` reads back.
    pub fn supplied(top: u64, size: u64) -> Self {
        let top = top & !0xf;
        let base = top.saturating_sub(size);
        GuestStack {
            space: None,
            base,
            size: (top - base) as usize,
            top,
            args_at: top,
            argc: 0,
            argv: 0,
            envp: 0,
            apple: 0,
        }
    }

    /// Write the strings and the three pointer arrays into the top of the stack.
    ///
    /// Only ever reached from [`GuestStack::new`], which builds a stack this
    /// runtime reserved — a guest-supplied one is a `pthread` stack and has no
    /// `argv` to lay out.
    fn place_args(&mut self, exe_path: &str, args: &[String]) -> Result<(), VmError> {
        let space = self
            .space
            .as_mut()
            .expect("argument material is only laid out on a stack this runtime reserved");
        let env: Vec<CString> = std::env::vars_os()
            .filter_map(|(k, v)| {
                CString::new(format!("{}={}", k.to_string_lossy(), v.to_string_lossy())).ok()
            })
            .collect();
        let argv: Vec<CString> = args
            .iter()
            .filter_map(|a| CString::new(a.as_str()).ok())
            .collect();
        let apple: Vec<CString> =
            std::iter::once(CString::new(exe_path).unwrap_or_default()).collect();

        // Strings first, growing downward from the top, then the arrays that
        // point at them. Order is not observable; adjacency is not promised.
        let mut cursor = self.top;
        let write_str = |s: &CString, cursor: &mut u64, this: &mut MmapSpace| {
            let bytes = s.as_bytes_with_nul();
            *cursor -= bytes.len() as u64;
            this.write(*cursor, bytes).map(|()| *cursor)
        };

        let mut argv_ptrs = Vec::new();
        for s in &argv {
            argv_ptrs.push(
                write_str(s, &mut cursor, space).map_err(|source| VmError::Stack { source })?,
            );
        }
        let mut env_ptrs = Vec::new();
        for s in &env {
            env_ptrs.push(
                write_str(s, &mut cursor, space).map_err(|source| VmError::Stack { source })?,
            );
        }
        let mut apple_ptrs = Vec::new();
        for s in &apple {
            apple_ptrs.push(
                write_str(s, &mut cursor, space).map_err(|source| VmError::Stack { source })?,
            );
        }

        cursor &= !0xf;
        let write_array = |ptrs: &[u64], cursor: &mut u64, this: &mut MmapSpace| {
            let mut buf = Vec::with_capacity((ptrs.len() + 1) * 8);
            for p in ptrs {
                buf.extend_from_slice(&p.to_le_bytes());
            }
            buf.extend_from_slice(&0u64.to_le_bytes());
            *cursor -= buf.len() as u64;
            *cursor &= !0x7;
            this.write(*cursor, &buf).map(|()| *cursor)
        };

        self.apple = write_array(&apple_ptrs, &mut cursor, space)
            .map_err(|source| VmError::Stack { source })?;
        self.envp = write_array(&env_ptrs, &mut cursor, space)
            .map_err(|source| VmError::Stack { source })?;
        self.argv = write_array(&argv_ptrs, &mut cursor, space)
            .map_err(|source| VmError::Stack { source })?;
        self.argc = argv_ptrs.len() as u64;
        self.args_at = cursor & !0xf;
        Ok(())
    }

    /// `(argc, argv, envp, apple)`, the four arguments an `LC_MAIN` entry point
    /// and every initializer are called with.
    pub fn main_args(&self) -> (u64, u64, u64, u64) {
        (self.argc, self.argv, self.envp, self.apple)
    }

    /// The stack pointer a guest function should be entered with.
    ///
    /// SysV AMD64 requires `rsp + 8` to be 16-byte aligned at a function's first
    /// instruction — that is, the `call` that got there pushed eight bytes onto
    /// a 16-byte-aligned stack. The caller pushes the return address into the
    /// word this points at, so it is returned already adjusted.
    pub fn initial_rsp(&self) -> u64 {
        (self.args_at & !0xf) - 8
    }

    /// The address range the stack occupies.
    pub fn range(&self) -> Range<u64> {
        self.base..self.base + self.size as u64
    }

    /// Whether `va` is a stack address.
    pub fn contains(&self, va: u64) -> bool {
        self.range().contains(&va)
    }

    /// The stack's mapped bytes, for [`GuestView`](crate::vm::GuestView).
    ///
    /// Empty for a guest-supplied stack, which is not this runtime's mapping to
    /// describe: it is already inside the guest's own memory, and
    /// [`GuestView`](crate::vm::GuestView) reaches it the way it reaches every
    /// other spawned thread's stack — by probing the page. See that type's note
    /// on what is deliberately not in the region list.
    pub fn regions(&self) -> Vec<(u64, &[u8])> {
        self.space
            .as_ref()
            .map(Regions::regions)
            .unwrap_or_default()
    }
}
