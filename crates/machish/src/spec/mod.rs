pub mod arch;
pub mod constants;

/// Everything the loader needs to know about a target Mach-O flavor.
pub trait MachSpec: Copy + Clone {
    /// Expected `cputype` (`CPU_TYPE_X86_64`, `CPU_TYPE_ARM64`, ...).
    const CPU_TYPE: i32;

    /// Segment alignment granularity for this architecture.
    ///
    /// Note this is the *image's* granularity, not the host's. On Apple Silicon
    /// the host enforces protection at 16 KiB while an x86_64 guest image is
    /// laid out at 4 KiB; reconciling the two is
    /// [`crate::memory::ProtectionPolicy`]'s job, not this one's.
    fn page_size(&self) -> u64;

    /// Whether `cpusubtype` is an acceptable slice for this spec.
    fn accepts_subtype(&self, _cpusubtype: i32) -> bool {
        true
    }

    /// Preference among acceptable slices; the highest rank wins.
    fn subtype_rank(&self, _cpusubtype: i32) -> u32 {
        0
    }

    /// Whether this image's code is executable by the host CPU.
    ///
    /// `false` for a guest architecture, and that is the interesting case: an
    /// x86-64 image on an arm64 host is data, so the loader never requests
    /// `exec` permission for it and never needs `MAP_JIT`.
    fn natively_executable(&self) -> bool {
        false
    }
}

/// Wraps any [`MachSpec`] to override its [`page_size`](MachSpec::page_size),
/// delegating everything else.
///
/// Mirrors `selfish::spec::PageSized` (`selfish/src/spec/mod.rs:82-122`).
///
/// ```
/// # #[cfg(feature = "arch-x86_64")] {
/// use machish::spec::{arch::X86_64, MachSpec, PageSized};
/// let spec = PageSized::new(X86_64, 0x4000);
/// assert_eq!(spec.page_size(), 0x4000);
/// # }
/// ```
#[derive(Copy, Clone, Debug)]
pub struct PageSized<S> {
    inner: S,
    page_size: u64,
}

impl<S> PageSized<S> {
    pub const fn new(inner: S, page_size: u64) -> Self {
        PageSized { inner, page_size }
    }

    pub fn inner(&self) -> &S {
        &self.inner
    }
}

impl<S: MachSpec> MachSpec for PageSized<S> {
    const CPU_TYPE: i32 = S::CPU_TYPE;

    fn page_size(&self) -> u64 {
        self.page_size
    }

    fn accepts_subtype(&self, cpusubtype: i32) -> bool {
        self.inner.accepts_subtype(cpusubtype)
    }

    fn subtype_rank(&self, cpusubtype: i32) -> u32 {
        self.inner.subtype_rank(cpusubtype)
    }

    fn natively_executable(&self) -> bool {
        self.inner.natively_executable()
    }
}

/// Wraps any [`MachSpec`] to declare its images host-executable, re-enabling
/// the paths that assume loaded code can be called directly.
///
/// Only meaningful when the spec's architecture *is* the host's - loading a
/// native arm64 image on Apple Silicon. The guest path never wants this.
#[derive(Copy, Clone, Debug)]
pub struct NativelyExecutable<S>(pub S);

impl<S: MachSpec> MachSpec for NativelyExecutable<S> {
    const CPU_TYPE: i32 = S::CPU_TYPE;

    fn page_size(&self) -> u64 {
        self.0.page_size()
    }

    fn accepts_subtype(&self, cpusubtype: i32) -> bool {
        self.0.accepts_subtype(cpusubtype)
    }

    fn subtype_rank(&self, cpusubtype: i32) -> u32 {
        self.0.subtype_rank(cpusubtype)
    }

    fn natively_executable(&self) -> bool {
        true
    }
}
