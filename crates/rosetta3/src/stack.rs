use std::ffi::{CString, OsString};
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;

use machish::memory::{AddressSpace, MmapSpace, Placement, Prot, Regions};

use crate::vm::VmError;

/// Default guest stack, matching the 8 MiB macOS gives a main thread.
pub const DEFAULT_STACK: usize = 8 * 1024 * 1024;

/// Bytes reserved below a guest stack that the guest may not use.
///
/// A guest-visible number, not a host one: `pthread_get_stacksize_np` on a main thread
/// returns `DEFAULT_STACK` less this, and a guest that reads sixteen kilobytes on real
/// hardware has to read sixteen kilobytes here. Deriving it from the host page size
/// gets the arm64 host right by coincidence - there a page *is* 16 KiB - and an x86-64
/// host wrong by three pages.
///
/// Raised to a whole host page when a host ever has a larger one, because the guard is
/// also a real `PROT_NONE` mapping.
pub fn guard() -> usize {
    const GUEST_GUARD: usize = 16 * 1024;
    GUEST_GUARD.max(machish::memory::host_page_size() as usize)
}

/// A guest stack, with the process arguments written into its top.
pub struct GuestStack {
    /// The reservation this stack owns, or `None` when the *guest* supplied the
    /// memory - `pthread_attr_setstack`. See [`GuestStack::supplied`].
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
    /// The `rsp` an `LC_UNIXTHREAD` entry point is entered with: the word holding
    /// `argc`, immediately below the `argv` array.
    kernel_sp: u64,
}

impl GuestStack {
    /// Reserve `size` bytes and lay out `args` and the host's environment.
    pub fn new(size: usize, exe_path: &str, args: &[OsString]) -> Result<Self, VmError> {
        let mut stack = Self::bare(size)?;
        stack.place_args(exe_path, args)?;
        Ok(stack)
    }

    /// Reserve `size` bytes with nothing laid out on them.
    pub fn bare(size: usize) -> Result<Self, VmError> {
        let page = machish::memory::host_page_size() as usize;
        let guard = guard();
        // The floor leaves usable stack under either term: four host pages is
        // what this has always reserved, and twice the guard is what keeps an
        // x86-64 host - where four pages is the guard - from reserving none.
        let size = size.next_multiple_of(page).max((page * 4).max(guard * 2));

        let mut space = MmapSpace::new();
        let reservation = space
            .reserve(size, page, Placement::Anywhere)
            .map_err(|source| VmError::Stack { source })?;
        // Guard region: overflow faults instead of corrupting whatever the
        // kernel happened to map below us.
        space
            .protect(reservation, guard, Prot::NONE)
            .map_err(|source| VmError::Stack { source })?;

        let top = (reservation + size as u64) & !0xf;
        Ok(GuestStack {
            space: Some(space),
            // The guard region is not stack, and saying so is what keeps
            // `pthread_get_stacksize_np` agreeing with the host's answer for a
            // thread of the same size - and what keeps the region list from
            // offering a shim a pointer into a `PROT_NONE` page.
            base: reservation + guard as u64,
            size: size - guard,
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
    fn place_args(&mut self, exe_path: &str, args: &[OsString]) -> Result<(), VmError> {
        let space = self
            .space
            .as_mut()
            .expect("argument material is only laid out on a stack this runtime reserved");
        // The guest's environment, not this process's: they differ in a
        // prefix, and `envp` has to agree with `getenv`.
        let env = darwin::guest_environment();
        // Bytes, not text. A process argument is an arbitrary byte string on
        // both platforms - Crashpad passes its crash handler annotations read
        // straight out of `sysctl` - and converting one through `String`
        // either loses bytes or refuses to run.
        let argv: Vec<CString> = args
            .iter()
            .filter_map(|a| CString::new(a.as_bytes()).ok())
            .collect();
        let apple: Vec<CString> =
            std::iter::once(CString::new(exe_path).unwrap_or_default()).collect();

        // Strings first, growing downward from the top, then the arrays that
        // point at them.
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
        // terminator is followed by `envp` and `envp`'s by `apple`.
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

    /// The `rsp` to enter an `LC_UNIXTHREAD` entry point with, or `None` on a stack
    /// with no argument material on it.
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
    /// instruction - that is, the `call` that got there pushed eight bytes onto a
    /// 16-byte-aligned stack.
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

    /// The frame an `LC_UNIXTHREAD` entry point walks, walked the same way it walks it.
    #[test]
    fn the_kernel_frame_is_what_start_walks() {
        let args = ["prog".into(), "one".into(), "two".into()] as [OsString; 3];
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

        // `envp = argv + (argc + 1) * 8` - computed, not searched for, so the
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

    /// The alignment padding is chosen from the *total* number of pointers, so it has
    /// to hold whatever the environment happens to contain.
    #[test]
    fn the_entry_rsp_is_aligned_for_either_parity() {
        for n in 1..=4 {
            let args: Vec<OsString> = (0..n).map(|i| OsString::from(format!("arg{i}"))).collect();
            let stack = GuestStack::new(DEFAULT_STACK, "/tmp/prog", &args).expect("reserved");
            let sp = stack.kernel_sp().expect("arguments were laid out");
            assert_eq!(sp % 16, 0, "argc-at-rsp is aligned with {n} arguments");
            assert_eq!(word(&stack, sp), n as u64);
        }
    }

    /// A spawned thread's stack has no arguments on it, so it has no frame to
    /// report - and reporting one would be a second copy of the process's
    /// arguments that nothing can reach. See [`GuestStack::bare`].
    #[test]
    fn a_bare_stack_has_no_kernel_frame() {
        let stack = GuestStack::bare(DEFAULT_STACK).expect("reserved");
        assert!(stack.kernel_sp().is_none());
    }
}
