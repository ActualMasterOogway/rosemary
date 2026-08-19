//! Running a host-architecture image: link it against the real dylibs and call it.

use machish::image::EntryPoint;
use machish::loader::Loader;
use machish::memory::MmapSpace;
use machish::namespace::FsDylibSource;
use machish::report::LinkReport;
use machish::resolve::{NativeDylibProvider, SymbolProvider, eager_binding_shims};
use machish::spec::constants::{CPU_SUBTYPE_ARM64E, subtype_of};
use machish::spec::{MachSpec, NativelyExecutable};

use crate::exec::Execution;
use crate::stack::GuestStack;
use crate::vm::VmError;

/// A host-executable spec, minus the subtypes this runtime cannot in fact run.
///
/// `Arm64::accepts_subtype` takes arm64e, and machish refuses its authenticated
/// chained pointers by name. Narrowing here rather than failing later is what
/// sends a fat binary with an arm64e slice and an x86-64 one to the emulated
/// backend instead of stopping on `/bin/ls`.
#[derive(Copy, Clone, Debug)]
pub struct HostRunnable<S>(pub S);

impl<S: MachSpec> MachSpec for HostRunnable<S> {
    const CPU_TYPE: i32 = S::CPU_TYPE;

    fn page_size(&self) -> u64 {
        self.0.page_size()
    }

    fn accepts_subtype(&self, cpusubtype: i32) -> bool {
        self.0.accepts_subtype(cpusubtype) && subtype_of(cpusubtype) != CPU_SUBTYPE_ARM64E
    }

    fn subtype_rank(&self, cpusubtype: i32) -> u32 {
        self.0.subtype_rank(cpusubtype)
    }

    fn natively_executable(&self) -> bool {
        self.0.natively_executable()
    }
}

#[cfg(target_arch = "aarch64")]
pub type HostSpec = HostRunnable<NativelyExecutable<machish::spec::arch::Arm64>>;
#[cfg(target_arch = "aarch64")]
pub const HOST_SPEC: HostSpec = HostRunnable(NativelyExecutable(machish::spec::arch::Arm64));

#[cfg(not(target_arch = "aarch64"))]
pub type HostSpec = HostRunnable<NativelyExecutable<machish::spec::arch::X86_64>>;
#[cfg(not(target_arch = "aarch64"))]
pub const HOST_SPEC: HostSpec = HostRunnable(NativelyExecutable(machish::spec::arch::X86_64));

/// Whether the native backend can run this slice — the same question the
/// loader will ask when it selects one.
pub fn runnable(slice: &machish::fat::Slice) -> bool {
    slice.cputype == HostSpec::CPU_TYPE && HOST_SPEC.accepts_subtype(slice.cpusubtype)
}

/// A program of the host's own architecture, mapped executable and called directly.
pub struct Native {
    report: LinkReport,
    /// The mappings the report's addresses point into. Never read through
    /// again; dropping it would unmap the program.
    _memory: MmapSpace,
    /// Only the `argc`/`argv`/`envp`/`apple` block. Nothing runs on it: an
    /// `LC_MAIN` image is entered by a call, on the host's own stack.
    stack: GuestStack,
}

// SAFETY: none of the three fields is mutated after construction, and the raw
// pointers inside the two `MmapSpace`s are handles to mappings that live as long
// as the `Native`. What the program does to its own memory once running is the
// program's business, exactly as it would be outside this runtime.
unsafe impl Sync for Native {}

impl Native {
    /// Load and link `path`, with `args` laid out for its entry point.
    pub fn load(path: &str, args: &[String]) -> Result<Self, VmError> {
        let bytes = std::fs::read(path).map_err(|e| VmError::Load {
            path: path.to_string(),
            reason: e.to_string(),
        })?;

        // Deliberately *not* `MmapSpace::executable`, which implies `MAP_JIT`.
        // MAP_JIT gates every store on the per-thread write-protect toggle, and
        // a program storing to its own globals is not a JIT writing code: with
        // the toggle on, the first `__DATA` write is a SIGBUS, and with it off
        // this thread has no W^X and any thread the program starts has the
        // fault back. A plain mapping plus the loader's own `mprotect` gives
        // `__TEXT` RX and `__DATA` RW, which is what the image asked for —
        // measured on this host: an anonymous RW mapping does become RX.
        let mut memory = MmapSpace::new();
        let mut source = FsDylibSource::new();
        // Nothing here needs virtualizing: the image is host code calling host
        // libraries, so the denylist that exists to force an emulated guest onto
        // a shim would only refuse symbols nothing else answers.
        let mut host = NativeDylibProvider::system();
        host.allow_everything();
        let eager = eager_binding_shims();
        let providers: Vec<&dyn SymbolProvider> = vec![&eager, &host];

        let report = Loader::new(HOST_SPEC)
            .link_with(&bytes, path, &mut memory, &mut source, &providers)
            .map_err(|e| VmError::Load {
                path: path.to_string(),
                reason: e.to_string(),
            })?;

        flush_icache(&report);

        let stack = GuestStack::new(crate::stack::DEFAULT_STACK, path, args)?;
        Ok(Native {
            report,
            _memory: memory,
            stack,
        })
    }
}

impl Execution for Native {
    fn report(&self) -> &LinkReport {
        &self.report
    }

    fn main_args(&self) -> (u64, u64, u64, u64) {
        self.stack.main_args()
    }

    fn call(&self, entry: u64, args: &[u64]) -> Result<u64, VmError> {
        if entry == 0 {
            return Err(VmError::NoCode { pc: 0, from: None });
        }
        let mut a = [0u64; 6];
        for (slot, &arg) in a.iter_mut().zip(args) {
            *slot = arg;
        }
        // Six arguments always, whatever the callee's arity: under both AAPCS64
        // and SysV a non-variadic callee ignores the registers it did not
        // declare, so this costs five moves and needs no signature.
        //
        // SAFETY: `entry` is an address the loader mapped executable from an
        // image of this host's architecture, so it is a real function; calling
        // it with more integer registers set than it reads is what the two ABIs
        // above permit.
        let f: extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64 =
            unsafe { std::mem::transmute(entry as usize as *const ()) };
        Ok(f(a[0], a[1], a[2], a[3], a[4], a[5]))
    }

    fn prepare(&self) -> Result<(), VmError> {
        crate::objc::native::map_images(&self.report);
        Ok(())
    }

    fn enter_main(&self) -> Result<u64, VmError> {
        let root = self.report.root().ok_or(VmError::NoCode { pc: 0, from: None })?;
        if let Some(EntryPoint::UnixThread { .. }) = root.entry_command {
            return Err(VmError::Unsupported {
                what: "an LC_UNIXTHREAD entry point, which is entered on a stack of its own \
                       rather than by a call"
                    .to_string(),
            });
        }
        let entry = root.entry.ok_or(VmError::NoCode { pc: 0, from: None })?;
        let (argc, argv, envp, apple) = self.main_args();
        self.call(entry, &[argc, argv, envp, apple])
    }
}

unsafe extern "C" {
    fn sys_icache_invalidate(start: *mut core::ffi::c_void, len: usize);
}

/// Flush the instruction cache over every executable segment.
///
/// `MmapSpace` only does this for `MAP_JIT` writes, and these are ordinary ones.
/// The pages are freshly mapped, so what is being guarded against is the kernel
/// handing back a physical page whose lines some earlier process left behind.
fn flush_icache(report: &LinkReport) {
    for seg in report.images.iter().flat_map(|i| i.segments.iter()) {
        if !seg.executable || seg.vmsize == 0 {
            continue;
        }
        // SAFETY: the range is a segment the loader just mapped and wrote, and
        // `sys_icache_invalidate` only invalidates cache lines over it.
        unsafe {
            sys_icache_invalidate(seg.vmaddr as usize as *mut _, seg.vmsize as usize);
        }
    }
}
