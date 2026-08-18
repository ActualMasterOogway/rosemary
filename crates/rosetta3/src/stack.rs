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
//! ## And what an `LC_UNIXTHREAD` entry point is called with, which is not that
//!
//! An `LC_UNIXTHREAD` image expects the same material *on* the stack in the
//! kernel's own layout — `argc` at `[rsp]`, then the three arrays end to end —
//! and reads it from there itself. `crt1.o`'s `start` is the whole of it:
//!
//! ```text
//! pushq $0                  ; a null return address, so a frame walk stops
//! movq  %rsp, %rbp
//! movq  0x8(%rbp), %rdi     ; argc, from the word the entry rsp pointed at
//! leaq  0x10(%rbp), %rsi    ; argv, immediately above it
//! ...                       ; envp = argv + (argc + 1) * 8, then scan for the
//!                           ; terminator to find apple[]
//! callq _main
//! ```
//!
//! Nothing about that is optional: `argc` is read from memory, not a register,
//! and every other argument is computed *from* it. So the three arrays are laid
//! out adjacently in that order and `argc` is written in the word below them,
//! which is [`GuestStack::kernel_sp`] — the `rsp` such an entry point must be
//! entered with. See [`crate::startup`], which is where the choice between the
//! two is made.
//!
//! This used to be deliberately absent, on the grounds that no toolchain had
//! emitted the form in fifteen years. That was wrong about the shipped
//! software: A Hat in Time is an `LC_UNIXTHREAD` executable built in 2017
//! against the 10.7 SDK, and it ran anyway only because the return address the
//! runtime pushed was then `0xFFFF_FFFF_FFFF_FFFF` — whose low 32 bits `start`
//! increments to zero, so `(argc + 1) * 8` was zero, so the scan started
//! somewhere readable and stopped at the first zero qword it found. `main` got
//! four garbage arguments and did not look at them. Giving the return stub a
//! real address (see [`crate::retstub`]) made the same arithmetic a multiply of
//! a pointer, and the scan walked off the stack.

use std::ffi::CString;
use std::ops::Range;

use machish::memory::{AddressSpace, MmapSpace, Placement, Prot, Regions};

use crate::vm::VmError;

/// Default guest stack, matching the 8 MiB macOS gives a main thread.
pub const DEFAULT_STACK: usize = 8 * 1024 * 1024;

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
    /// The `rsp` an `LC_UNIXTHREAD` entry point is entered with: the word
    /// holding `argc`, immediately below the `argv` array. Zero on a stack with
    /// no argument material laid out on it.
    kernel_sp: u64,
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
            kernel_sp: 0,
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
            kernel_sp: 0,
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
        // point at them. The strings' order is not observable, but the arrays'
        // **is**: an `LC_UNIXTHREAD` entry point finds `envp` by walking off the
        // end of `argv` and `apple` by walking off the end of `envp`, so the
        // three have to be adjacent and in that order. See the module note.
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

        // The three arrays are written end to end, highest first, so `argv`'s
        // terminator is followed by `envp` and `envp`'s by `apple`. `argc` goes
        // in the word below `argv`, and that word is the `rsp` an
        // `LC_UNIXTHREAD` entry is entered with — so it is what has to come out
        // sixteen-byte aligned, not the top of the block. Every array is a
        // multiple of eight bytes long, so one padding word decides it.
        cursor &= !0xf;
        let words = apple_ptrs.len() + env_ptrs.len() + argv_ptrs.len() + 3;
        if words.is_multiple_of(2) {
            cursor -= 8;
        }
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

        // `argc` in the word below `argv`, completing the kernel's frame.
        let sp = self.argv - 8;
        space
            .write(sp, &self.argc.to_le_bytes())
            .map_err(|source| VmError::Stack { source })?;
        self.kernel_sp = sp;
        debug_assert_eq!(sp % 16, 0, "the kernel frame's rsp is 16-byte aligned");

        // Free stack begins below the frame, so entering an `LC_MAIN` program
        // does not write over the `argc` an `LC_UNIXTHREAD` one would read.
        self.args_at = sp & !0xf;
        Ok(())
    }

    /// The `rsp` to enter an `LC_UNIXTHREAD` entry point with, or `None` on a
    /// stack with no argument material on it.
    ///
    /// The word it points at is `argc`; `argv`, `envp` and `apple` follow it
    /// end to end. See the module note for what reads them.
    pub fn kernel_sp(&self) -> Option<u64> {
        (self.kernel_sp != 0).then_some(self.kernel_sp)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Read the qword at `va` out of the stack's own mapping.
    fn word(stack: &GuestStack, va: u64) -> u64 {
        let mut buf = [0u8; 8];
        stack
            .space
            .as_ref()
            .expect("a stack this test reserved")
            .read(va, &mut buf)
            .expect("inside the stack");
        u64::from_le_bytes(buf)
    }

    /// The frame an `LC_UNIXTHREAD` entry point walks, walked the same way it
    /// walks it: `argc` at `rsp`, `argv` above it, `envp` past `argv`'s
    /// terminator, `apple` past `envp`'s. This is the layout A Hat in Time's
    /// `start` reads, and the adjacency is the whole of it — three arrays that
    /// merely exist would leave `envp` and `apple` unreachable.
    #[test]
    fn the_kernel_frame_is_what_start_walks() {
        let args = ["prog".to_string(), "one".to_string(), "two".to_string()];
        let stack = GuestStack::new(DEFAULT_STACK, "/tmp/prog", &args).expect("reserved");

        let sp = stack.kernel_sp().expect("arguments were laid out");
        assert_eq!(sp % 16, 0, "the entry rsp is 16-byte aligned");

        let argc = word(&stack, sp);
        assert_eq!(argc, 3);
        let (rec_argc, rec_argv, rec_envp, rec_apple) = stack.main_args();
        assert_eq!(rec_argc, argc, "the register form and the stack form agree");

        // `argv` is the word above `argc`, which is what `leaq 0x10(%rbp)` means
        // once `start`'s own push is accounted for.
        let argv = sp + 8;
        assert_eq!(argv, rec_argv);

        // `envp = argv + (argc + 1) * 8` — computed, not searched for, so the
        // terminator has to be exactly there.
        assert_eq!(word(&stack, argv + argc * 8), 0, "argv is terminated");
        let envp = argv + (argc + 1) * 8;
        assert_eq!(envp, rec_envp, "envp follows argv with no gap");

        // `apple` is found by scanning `envp` for its terminator, which is the
        // loop that ran off the end of the stack when the frame was absent.
        let mut p = envp;
        while word(&stack, p) != 0 {
            p += 8;
        }
        assert_eq!(p + 8, rec_apple, "apple follows envp with no gap");

        // Free stack starts below the frame, so entering guest code cannot
        // write over the `argc` the entry point has not read yet.
        assert!(stack.initial_rsp() < sp);
    }

    /// The alignment padding is chosen from the *total* number of pointers, so
    /// it has to hold whatever the environment happens to contain. Both parities
    /// of `argv` exercise it against one fixed environment.
    #[test]
    fn the_entry_rsp_is_aligned_for_either_parity() {
        for n in 1..=4 {
            let args: Vec<String> = (0..n).map(|i| format!("arg{i}")).collect();
            let stack = GuestStack::new(DEFAULT_STACK, "/tmp/prog", &args).expect("reserved");
            let sp = stack.kernel_sp().expect("arguments were laid out");
            assert_eq!(sp % 16, 0, "argc-at-rsp is aligned with {n} arguments");
            assert_eq!(word(&stack, sp), n as u64);
        }
    }

    /// A spawned thread's stack has no arguments on it, so it has no frame to
    /// report — and reporting one would be a second copy of the process's
    /// arguments that nothing can reach. See [`GuestStack::bare`].
    #[test]
    fn a_bare_stack_has_no_kernel_frame() {
        let stack = GuestStack::bare(DEFAULT_STACK).expect("reserved");
        assert!(stack.kernel_sp().is_none());
    }
}
