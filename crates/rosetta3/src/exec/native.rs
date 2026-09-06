//! Running a host-architecture image: link it against the real dylibs and call it.

use machish::image::EntryPoint;
use machish::loader::Loader;
use machish::memory::MmapSpace;
use machish::report::LinkReport;
use machish::resolve::{NativeDylibProvider, SymbolProvider, SyntheticDylib, eager_binding_shims};
use machish::spec::constants::{CPU_SUBTYPE_ARM64E, subtype_of};
use machish::spec::{MachSpec, NativelyExecutable};

use crate::cxxlib;
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

/// Whether the native backend can run this slice - the same question the
/// loader will ask when it selects one.
///
/// A matching instruction set is not on its own enough, because the code is
/// still Darwin's: it reads its thread-specific data straight out of the
/// thread pointer register. [`darwin::tls::can_enter_darwin`] is what says the
/// host can give it that array, and it is the narrower of the two conditions.
/// On aarch64 Linux it is false - Darwin arm64 code reads `TPIDRRO_EL0`, which
/// Linux does not let userspace write - so a universal Mach-O there takes the
/// emulated backend's x86-64 slice instead.
pub fn runnable(slice: &machish::fat::Slice) -> bool {
    darwin::tls::can_enter_darwin()
        && slice.cputype == HostSpec::CPU_TYPE
        && HOST_SPEC.accepts_subtype(slice.cpusubtype)
}

/// A program of the host's own architecture, mapped executable and called directly.
pub struct Native {
    report: LinkReport,
    /// The mappings the report's addresses point into, and what says an address
    /// is the program's. Dropping it would unmap the program.
    memory: MmapSpace,
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
    pub fn load(path: &str, args: &[std::ffi::OsString]) -> Result<Self, VmError> {
        // Before anything can hand the program a symbol. A trampoline is five
        // bytes of x86-64 that turn a call back into an import for the lifter to
        // intercept, and the native backend intercepts nothing: a `dlsym` result
        // here has to be the host function itself. The program is this host's
        // own architecture, so on aarch64 it would `blr` into x86-64 bytes and
        // on x86-64 it would call a bridge with no run loop behind it.
        crate::trampoline::disarm();

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
        // `__TEXT` RX and `__DATA` RW, which is what the image asked for -
        // measured on this host: an anonymous RW mapping does become RX.
        let mut memory = MmapSpace::new();
        // Nothing here needs virtualizing: the image is host code calling host
        // libraries, so the denylist that exists to force an emulated guest onto
        // a shim would only refuse symbols nothing else answers.
        let mut host = NativeDylibProvider::system();
        host.allow_everything();
        // ...except the names whose Darwin and host ABIs differ, which
        // `allow_everything` would otherwise re-admit. Empty on macOS, where this
        // image really is calling the libraries it was linked against.
        darwin::divergent::deny_in(&mut host);
        // And, under the builtin runtime, the two families whose objects would
        // otherwise be Apple's. Both are no-ops under `native`, and both must
        // come after `allow_everything`, which clears the prefix list.
        crate::objc::host::deny_foreign_classes(&mut host);
        crate::frameworks::deny_host_frameworks(&mut host);
        crate::opengl::deny_host_gl(&mut host);

        let eager = eager_binding_shims();
        let tlv = tlv_provider();
        // The symbols libSystem exports and no other host libc does:
        // `___stack_chk_guard`, `__NSGetExecutablePath`, `_environ`, the Mach
        // task port. On macOS the host provider answers every one of them, which
        // is why this chain went without it; off Darwin nothing does, and an
        // unbound `___stack_chk_guard` faults in the first prologue the program
        // enters. Same provider the emulated chain uses, in the same position.
        let runtime = crate::shims::runtime::provider();
        let platform = darwin::provider();
        let objc = crate::objc::dispatch::provider();
        let frameworks = crate::frameworks::provider();
        let dyld = crate::dyld::provider();
        let opengl = crate::opengl::provider();
        // Under `native` the image is host code calling the libraries it was
        // linked against, and the shortest chain that works is the honest one.
        // Under `builtin` it must reach this tree's runtime and frameworks
        // instead - including `dlopen`, or the first `dlopen("CoreFoundation")`
        // puts Apple's classes in front of a program that has ours.
        let mut providers: Vec<&dyn SymbolProvider> = match crate::frameworks::builtin() {
            true => vec![
                &eager,
                &tlv,
                &runtime,
                &platform,
                &objc,
                &frameworks,
                &dyld,
                &opengl,
            ],
            false => vec![&eager, &tlv, &runtime, &platform],
        };
        // Last, and only when the host's own libraries are allowed to answer at
        // all. This backend is the one an x86-64 guest gets on an x86-64 host, so
        // leaving it out of `darwin::strict` left the mode answering nothing on
        // the configuration it exists for: every name reached the host's libc and
        // the program ran to completion. Same rule and same order as the emulated
        // chain - see [`crate::program::HostProviders::chain`].
        match darwin::strict::enabled() {
            true => crate::program::announce_strict(),
            false => providers.push(&host),
        }

        // A native image is of this host's architecture by definition, which is what
        // makes it the native backend's. See [`darwin::sysctl`].
        darwin::sysctl::set_guest_arch(match cfg!(target_arch = "aarch64") {
            true => darwin::sysctl::GuestArch::Arm64,
            false => darwin::sysctl::GuestArch::X86_64,
        });
        // And that its instructions are this CPU's, which is what lets
        // `hw.optional.*` report what this CPU has rather than the guest
        // architecture's baseline. See `darwin::sysctl::optional_feature`.
        #[cfg(not(target_os = "macos"))]
        darwin::sysctl::set_runs_host_instructions(true);

        // The image's C++ runtime is the vendored one of this host's
        // architecture, on macOS as well as off it. Off Darwin there is no
        // `/usr/lib/libc++.1.dylib` to link against at all; on macOS there is,
        // but binding to it would put the guest's C++ allocations outside
        // [`crate::interpose`]'s reach. See [`crate::cxxlib`].
        let mut report = Loader::new(HOST_SPEC)
            .link_with(
                &bytes,
                path,
                &mut memory,
                &mut crate::cxxlib::source(cxxlib::HOST),
                &providers,
            )
            .map_err(|e| VmError::Load {
                path: path.to_string(),
                reason: e.to_string(),
            })?;

        // The unwinder and the C++ ABI, whose names the host provider would
        // otherwise claim with its own implementations. Before anything reads
        // the report, and in the emulated backend's order.
        let load = |e: machish::Error| VmError::Load {
            path: path.to_string(),
            reason: e.to_string(),
        };
        crate::cxxlib::bind_unwinder_to_the_vendored_one(&mut report, &mut memory, cxxlib::HOST)
            .map_err(load)?;
        crate::cxxlib::bind_cxxabi_to_the_vendored_one(&mut report, &mut memory, cxxlib::HOST)
            .map_err(load)?;
        crate::cxxlib::bind_objc_exceptions_to_the_vendored_cxxabi(
            &mut report,
            &mut memory,
            cxxlib::HOST,
        )
        .map_err(load)?;

        // `libmimalloc.3.dylib` replaces `free` for every image that binds it,
        // and a `malloc` from one allocator freed by the other aborts inside
        // libmalloc rather than anywhere near the allocation.
        crate::interpose::apply(&mut report, &mut memory).map_err(|e| VmError::Load {
            path: path.to_string(),
            reason: e.to_string(),
        })?;
        // After the interpositions, because the answer wanted is the allocator
        // the guest's code *ends up* calling. This backend only: see
        // [`crate::interpose::adopt_guest_allocator`].
        crate::hostheap::apply(&mut report, &mut memory).map_err(|e| VmError::Load {
            path: path.to_string(),
            reason: e.to_string(),
        })?;
        crate::heapcheck::install(&mut report, &mut memory).map_err(|e| VmError::Load {
            path: path.to_string(),
            reason: e.to_string(),
        })?;
        crate::interpose::adopt_guest_allocator(&report);

        // Say what did not bind, now that every repair above has had its say and
        // `missing` means missing. The emulated backend has always done this;
        // this one had not, and the difference is the whole diagnostic: a native
        // image calls straight through its stub, so an unbound import is a jump
        // to zero with no runtime in the way to name the symbol. Every failure
        // this backend reported was "SIGSEGV at 0x0" with the answer sitting in
        // the link report the whole time.
        let unbound = crate::program::announce_unbound_imports(&report, path);
        crate::program::announce_passthrough_imports(&report, path);
        if !unbound.is_empty() && crate::program::strict_imports() {
            return Err(VmError::Load {
                path: path.to_string(),
                reason: format!(
                    "{} unbound import(s), the first being {} ({} is set)",
                    unbound.len(),
                    unbound[0].1.symbol,
                    crate::program::STRICT_IMPORTS_ENV,
                ),
            });
        }

        flush_icache(&report);
        crate::tls::register(&report);
        // The dynamic loader needs the executable itself as the first flat
        // namespace. Plugins such as `libYoYoGamepad` import C++ definitions
        // from the runner rather than from one of their dependencies.
        crate::dyld::set_executable_path(path, report.root().map_or(0, |i| i.slide));
        if let Some(root) = report.root() {
            crate::dyld::set_executable_header(root.base);
        }
        // Before the program runs, because the first thing it may do is throw.
        crate::dyld::register_mapped_images(&report);
        // And to LLDB, which otherwise has nothing to name a frame in the
        // program with: these images were mapped here rather than by dyld.
        crate::lldb::register_images(&report, path);

        let stack = GuestStack::new(crate::stack::DEFAULT_STACK, path, args)?;
        // What `_NSGetArgc`/`_NSGetArgv` answer with, now that there is a layout
        // for them to point into.
        let (argc, argv, _, _) = stack.main_args();
        crate::shims::runtime::set_process_args(argc, argv);
        Ok(Native {
            report,
            memory,
            stack,
        })
    }
}

impl Execution for Native {
    fn engine(&self) -> crate::exec::Engine {
        crate::exec::Engine::Native
    }

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
        // The image is Darwin code, so for the duration of the call this thread's
        // thread pointer must be the Darwin TSD array rather than the host's:
        // `pthread_self`, `errno` and the stack canary are all direct loads off it
        // and never reach a shim. Compiles away on macOS, where it is already.
        let _darwin = darwin::tls::DarwinThread::enter();
        Ok(f(a[0], a[1], a[2], a[3], a[4], a[5]))
    }

    fn prepare(&self) -> Result<(), VmError> {
        if crate::frameworks::builtin() {
            return self.realize_objc();
        }
        // Registering the image's classes with the host's Objective-C runtime, which
        // is a step only a host that has one can take.
        #[cfg(target_os = "macos")]
        crate::objc::native::map_images(&self.report);
        Ok(())
    }

    fn enter_main(&self) -> Result<u64, VmError> {
        let root = self
            .report
            .root()
            .ok_or(VmError::NoCode { pc: 0, from: None })?;
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

impl Native {
    /// Realize the image's Objective-C metadata in this tree's own runtime.
    ///
    /// What `_objc_map_images` does for the host's libobjc, minus the part dyld
    /// would have done: the classes are rebuilt through the public C API, and
    /// the guest's own references to them are pointed at the result.
    fn realize_objc(&self) -> Result<(), VmError> {
        // Before the program can send one: a format method reached as an `IMP`
        // has no variadic tail without this.
        crate::shims::variadic::install();
        let rt = Box::leak(Box::new(crate::objc::register(self)?));
        crate::objc::publish(rt);
        crate::objc::rewrite_refs(self, rt)?;
        // `+load` before the C constructors of the same image, which is dyld's
        // order and the one a program that uses both depends on.
        crate::objc::call_load_methods(self, rt)?;
        Ok(())
    }
}

/// The program's memory, for the passes that write back into its own metadata.
///
/// A native image is mapped at the addresses it was linked for, so a guest
/// address is a host one and the copy is a `memcpy`. The loader's regions are
/// still what says an address is *in* the program: a metadata pointer that is
/// not belongs in a diagnostic rather than in a fault.
struct HostView<'a>(&'a MmapSpace);

impl HostView<'_> {
    fn span(&self, va: u64, len: usize) -> Result<*mut u8, machish::Error> {
        use machish::memory::Regions;
        let outside = || machish::Error::Memory("the address is in no mapped region");
        let (base, bytes) = self.0.region_at(va).ok_or_else(outside)?;
        let offset = usize::try_from(va - base).map_err(|_| outside())?;
        if bytes.len() - offset < len {
            return Err(outside());
        }
        Ok(va as usize as *mut u8)
    }
}

impl crate::abi::GuestMemory for HostView<'_> {
    fn read(&self, va: u64, out: &mut [u8]) -> Result<(), machish::Error> {
        let src = self.span(va, out.len())?;
        // SAFETY: `span` checked the whole range lies in a region the loader
        // mapped, and the mapping outlives this view.
        unsafe { core::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), out.len()) };
        Ok(())
    }

    fn write(&mut self, va: u64, data: &[u8]) -> Result<(), machish::Error> {
        let dst = self.span(va, data.len())?;
        // SAFETY: as above. Every caller writes into `__DATA`, which the loader
        // left writable.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len()) };
        Ok(())
    }
}

impl crate::objc::Realizer for Native {
    fn report(&self) -> &LinkReport {
        &self.report
    }

    fn memory(&self) -> Box<dyn crate::abi::GuestMemory + '_> {
        Box::new(HostView(&self.memory))
    }

    fn is_guest_address(&self, va: u64) -> bool {
        use machish::memory::Regions;
        self.memory.region_at(va).is_some()
    }

    fn call(&self, f: u64, args: &[u64]) -> Result<u64, VmError> {
        Execution::call(self, f, args)
    }

    /// The address itself. A native image's methods are host functions with the
    /// host's ABI, so there is nothing for a trampoline to convert.
    fn imp_for(
        &self,
        m: &machish::extensions::objc::ObjcMethod,
    ) -> Result<(u64, Option<&'static str>), &'static str> {
        Ok((m.imp, None))
    }

    /// The registered class. Nothing intercepts the sends a native image makes,
    /// so the guest's own `objc_class` would be a receiver this runtime never
    /// made - see [`crate::objc::rewrite_refs`], which is the same argument for
    /// the references the compiler emitted.
    fn class_value(&self, rt: &crate::objc::ObjcRuntime, guest_va: u64) -> u64 {
        rt.host_or(guest_va)
    }
}

/// `__tlv_bootstrap`, from this runtime rather than from dyld.
///
/// dyld initialises the descriptors of the images *it* loaded; these were loaded
/// here, so their thunks still hold the bootstrap pointer and dyld's own
/// implementation aborts the process through `_tlv_bootstrap_error` on the first
/// thread-local read. Flat, so it answers a two-level import naming libSystem.
///
/// Ahead of [`crate::shims::runtime::provider`] in the chain, which defines the
/// same names for an emulated guest: that one is the plain function, and a native
/// image reaches its thread-locals through the thunk.
fn tlv_provider() -> SyntheticDylib {
    let mut lib = SyntheticDylib::new("<rosetta3 tlv>");
    lib.flat(true);
    for name in machish::extensions::tlv::BOOTSTRAP_SYMBOLS {
        lib.define_fn(name, crate::tls::bootstrap_thunk as *const ());
    }
    lib
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
        // invalidating cache lines over it changes nothing about its contents.
        unsafe {
            invalidate_icache(seg.vmaddr as usize as *mut u8, seg.vmsize as usize);
        }
    }
}

/// Invalidate the instruction cache over `len` bytes at `start`.
///
/// Darwin publishes `sys_icache_invalidate` for this; elsewhere it is the compiler
/// builtin `__clear_cache`, which is in `libgcc`/`compiler-rt` rather than in libc and
/// so is declared here rather than taken from the `libc` crate.
///
/// # Safety
///
/// `start..start+len` is mapped.
unsafe fn invalidate_icache(start: *mut u8, len: usize) {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn sys_icache_invalidate(start: *mut core::ffi::c_void, len: usize);
        }
        // SAFETY: the caller's guarantee.
        unsafe { sys_icache_invalidate(start as *mut core::ffi::c_void, len) };
    }
    #[cfg(not(target_os = "macos"))]
    {
        unsafe extern "C" {
            fn __clear_cache(start: *mut core::ffi::c_char, end: *mut core::ffi::c_char);
        }
        // SAFETY: as above. `__clear_cache` takes the half-open range rather than a
        // base and a length, which is the only difference between the two.
        unsafe {
            __clear_cache(
                start as *mut core::ffi::c_char,
                start.add(len) as *mut core::ffi::c_char,
            )
        };
    }
}
