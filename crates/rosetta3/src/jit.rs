//! How hard Cranelift is asked to work on the code it emits.
//!
//! Every native instruction this crate runs comes out of Cranelift — the lifted
//! guest regions in [`crate::cache`] and the host→guest trampolines in
//! [`crate::thunk`] — and Cranelift's own default is `opt_level=none`. That
//! default is wrong here for a specific reason: microlift models an
//! instruction's side effects as SSA variables (flags, the parts of a vector
//! register, the intermediate values a semantic handler writes), so a *lot* of
//! what a region lifts to is dead the moment the next instruction overwrites
//! it. Optimising is not a few percent on top of the emulation, it is what
//! removes the emulation nobody asked for.
//!
//! `ROSETTA3_JIT_OPT` picks the level; it is read once, on first use, and
//! applies to a whole run. See [`OptLevel`] for the values.
//!
//! `ROSETTA3_JIT_VERIFY` is the other knob, and it is orthogonal to the level:
//! whether Cranelift's IR verifier runs. See [`verify`] — it is the single
//! largest thing this crate pays for translation.
//!
//! Three knobs, not one, and they sit at three different points in the
//! pipeline. `opt_level` is a Cranelift *shared* setting and reaches the backend
//! as a [`settings::Flags`]. The ISA settings ([`OptLevel::isa_settings`]) are
//! per-architecture target-feature knobs and decide which instructions the
//! lowering is allowed to reach for. The third is a *lifter* decision —
//! [`LifterConfig::use_vector_xmm`] makes an `xmm` register one `I64X2` SSA
//! variable instead of a scalar lo/hi pair, which is only worth doing when the
//! host has the vector unit to lower it onto. All three are answered from the
//! same setting so that one environment variable describes the whole pipeline
//! rather than a third of it.
//!
//! [`LifterConfig::use_vector_xmm`]: microlift::core::LifterConfig::use_vector_xmm

use cranelift_codegen::isa::TargetFrontendConfig;
use cranelift_codegen::settings::{self, Configurable};
use microlift::MicroliftError;
use microlift::arch::x86_64::pin;
use microlift::backend::CraneliftBackend;
use microlift::core::pin::PinnedAbi;
use static_init::dynamic;

/// The environment variables this module reads.
pub const OPT_ENV: &str = "ROSETTA3_JIT_OPT";
pub const VERIFY_ENV: &str = "ROSETTA3_JIT_VERIFY";
pub const DEAD_FLAGS_ENV: &str = "ROSETTA3_DEAD_FLAGS";
/// Turns [`block_linking`] off. Named for what setting it does, since the
/// feature itself is on by default.
pub const NO_BLOCK_LINK_ENV: &str = "ROSETTA3_NO_BLOCK_LINK";
/// Turns [`pinned_link_abi`] off, leaving links resolved by the host.
pub const NO_PINNED_LINK_ENV: &str = "ROSETTA3_NO_PINNED_LINK";
/// Replaces the pinned register set: a comma-separated list of x86-64 register
/// names, e.g. `ROSETTA3_PINNED_REGS=rsp,rax,rdi,rsi,rbp`.
pub const PINNED_REGS_ENV: &str = "ROSETTA3_PINNED_REGS";

/// What to ask of the JIT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptLevel {
    /// `none` — emit what the lifter produced, unoptimised.
    ///
    /// Slowest by a wide margin. It is here as a bisection tool: if a region
    /// misbehaves at `speed` and behaves at `none`, the fault is in the
    /// lowering rather than the lift. That is the unlikely outcome — by the
    /// time code is lowered, a lifter bug is already baked into the IR and no
    /// optimisation level will hide or reveal it — but it is not a question
    /// that can be answered without the level existing.
    ///
    /// It also pins the ISA to its architectural baseline (see
    /// [`OptLevel::isa_settings`]), which makes the same bisection available one
    /// level down: a region that only misbehaves once the host's target features
    /// are switched on is a bug in a feature-specific lowering rule, not in the
    /// lift.
    None,
    /// `speed` — Cranelift's `opt_level=speed`.
    ///
    /// The level microlift's IR is shaped for: dead-code elimination over the
    /// SSA variables standing in for guest side effects is where most of the
    /// per-instruction emulation goes away.
    ///
    /// Target features are left as `cranelift-native` detected them: this level
    /// asks for fast code without rosetta3 having an opinion about the host.
    Speed,
    /// `all` — `speed`, plus host-specific choices. The default.
    ///
    /// Two of them. The ISA settings become rosetta3's own reading of the host
    /// CPU rather than a dependency's ([`OptLevel::isa_settings`]), and the
    /// lifter is allowed to keep `xmm` state vectorial when the host has the
    /// unit to lower it onto. The second changes the IR itself, so this is the
    /// one level that is not purely a lowering decision.
    #[default]
    All,
}

impl OptLevel {
    /// Parse an `ROSETTA3_JIT_OPT` value. Case- and whitespace-insensitive.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Some(OptLevel::None),
            "speed" => Some(OptLevel::Speed),
            "all" => Some(OptLevel::All),
            _ => None,
        }
    }

    /// The name of this level, as it is written in the environment.
    pub fn as_str(self) -> &'static str {
        match self {
            OptLevel::None => "none",
            OptLevel::Speed => "speed",
            OptLevel::All => "all",
        }
    }

    /// The Cranelift `opt_level` setting this level maps to.
    ///
    /// `All` deliberately maps to `speed` and not `speed_and_size`: the
    /// difference between the two is a size/speed tradeoff, and what `all` adds
    /// over `speed` is host-specific *lifting*, not a different tradeoff.
    fn cranelift_opt_level(self) -> &'static str {
        match self {
            OptLevel::None => "none",
            OptLevel::Speed | OptLevel::All => "speed",
        }
    }

    /// Whether host-specific lift decisions are allowed at this level.
    pub fn host_specific(self) -> bool {
        matches!(self, OptLevel::All)
    }

    /// The Cranelift flags to build a backend with.
    pub fn flags(self) -> settings::Flags {
        let mut builder = settings::builder();
        // The only way this fails is a setting Cranelift does not have or a
        // value it does not accept, both of which are fixed constants above.
        builder
            .set("opt_level", self.cranelift_opt_level())
            .expect("cranelift accepts its own opt_level values");
        // Set at every level rather than inside the `Speed | All` arm below,
        // because it is not an optimisation setting: it says whether this build
        // checks microlift's output, and `none` is the level a lifter bug is
        // bisected at, so it is the last one that should stop checking. See
        // [`verify`].
        builder
            .set("enable_verifier", if verify() { "true" } else { "false" })
            .expect("cranelift accepts its own enable_verifier values");
        // We don't process this, so enabling it just wastes cycles.
        builder
            .set("machine_code_cfg_info", "false")
            .expect("failed to turn off machine code cfg info");
        // These aren't necessary since the guest shares our entire address space anyway.
        builder
            .set("enable_heap_access_spectre_mitigation", "false")
            .expect("failed to turn off heap access pass");
        builder
            .set("enable_table_access_spectre_mitigation", "false")
            .expect("failed to turn off table access pass");
        match self {
            OptLevel::Speed | OptLevel::All => {
                builder
                    .set("enable_alias_analysis", "true")
                    .expect("couldn't enable alias analysis");
                builder
                    .set("regalloc_algorithm", "backtracking")
                    .expect("couldn't enable backtracking regalloc");
            }
            OptLevel::None => {}
        };
        settings::Flags::new(builder)
    }

    /// The ISA-specific `(name, value)` pairs to build a backend with.
    ///
    /// These are the target-feature knobs of the host architecture — `has_avx`,
    /// `has_lse`, and friends — and they decide which instructions the lowering
    /// may reach for. They are *overrides*: microlift builds the ISA from
    /// `cranelift-native`, which has already inferred these from the running
    /// CPU, so a pair here overrules that inference rather than adding to a bare
    /// baseline. Which is why the three levels differ in what they say and not
    /// just in how much:
    ///
    /// - [`OptLevel::None`] forces every tunable feature off, leaving the
    ///   architectural baseline the un-inferred ISA would have used. Only
    ///   features are cleared — knobs that are ABI or correctness decisions
    ///   rather than tuning (`has_cmpxchg16b`, the pointer-authentication and
    ///   return-address-signing settings macOS requires) are never touched at
    ///   any level, since turning those off would change what the code *means*.
    /// - [`OptLevel::Speed`] says nothing and keeps the inference.
    /// - [`OptLevel::All`] states each feature explicitly, from this crate's own
    ///   detection ([`host_features`]). The values agree with the inference on
    ///   any host where both look at the same bit; the point is that at the
    ///   level whose job is to exploit the host, what was exploited is decided
    ///   and auditable here rather than inherited from a dependency's idea of
    ///   the machine.
    ///
    /// Only pairs valid for the host architecture are produced — an unknown
    /// name is an error from `CraneliftBackend::with_flags`, not a no-op — so
    /// every list here is built under `cfg(target_arch)`.
    pub fn isa_settings(self) -> Vec<(&'static str, &'static str)> {
        match self {
            OptLevel::None => TUNABLE_FEATURES.iter().map(|&f| (f, "false")).collect(),
            OptLevel::Speed => Vec::new(),
            OptLevel::All => host_features(),
        }
    }
}

/// The host target features this crate has an opinion about.
///
/// Deliberately the tuning half of the ISA settings and nothing else; see
/// [`OptLevel::isa_settings`] for what is left out and why.
#[cfg(target_arch = "x86_64")]
const TUNABLE_FEATURES: &[&str] = &[
    "has_sse3",
    "has_ssse3",
    "has_sse41",
    "has_sse42",
    "has_popcnt",
    "has_lzcnt",
    "has_bmi1",
    "has_bmi2",
    "has_avx",
    "has_avx2",
    "has_fma",
    "has_avx512f",
    "has_avx512vl",
    "has_avx512dq",
    "has_avx512bitalg",
    "has_avx512vbmi",
];

/// The host target features this crate has an opinion about.
///
/// `has_pauth` and the `sign_return_address*` settings are absent on purpose:
/// on macOS they are how the platform ABI wants return addresses signed, not a
/// speed/size tradeoff, so no optimisation level gets to answer them.
#[cfg(target_arch = "aarch64")]
const TUNABLE_FEATURES: &[&str] = &["has_lse", "has_fp16", "has_dotprod", "has_i8mm"];

/// The host target features this crate has an opinion about.
///
/// Empty on architectures microlift is not built to JIT for: the ISA settings
/// are per-architecture names, and naming one the host does not have is an
/// error rather than a setting that gets ignored.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const TUNABLE_FEATURES: &[&str] = &[];

/// [`TUNABLE_FEATURES`], each paired with whether this CPU has it.
///
/// Runtime detection, not `cfg!(target_feature)`: the binary is built once for
/// a whole architecture, and the point is to compile for the machine that is
/// running rather than for the one that compiled.
fn host_features() -> Vec<(&'static str, &'static str)> {
    /// Both spellings Cranelift accepts for a boolean setting.
    fn value(present: bool) -> &'static str {
        if present { "true" } else { "false" }
    }

    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::is_x86_feature_detected as has;
        vec![
            ("has_sse3", value(has!("sse3"))),
            ("has_ssse3", value(has!("ssse3"))),
            ("has_sse41", value(has!("sse4.1"))),
            ("has_sse42", value(has!("sse4.2"))),
            ("has_popcnt", value(has!("popcnt"))),
            ("has_lzcnt", value(has!("lzcnt"))),
            ("has_bmi1", value(has!("bmi1"))),
            ("has_bmi2", value(has!("bmi2"))),
            ("has_avx", value(has!("avx"))),
            ("has_avx2", value(has!("avx2"))),
            ("has_fma", value(has!("fma"))),
            ("has_avx512f", value(has!("avx512f"))),
            ("has_avx512vl", value(has!("avx512vl"))),
            ("has_avx512dq", value(has!("avx512dq"))),
            ("has_avx512bitalg", value(has!("avx512bitalg"))),
            ("has_avx512vbmi", value(has!("avx512vbmi"))),
        ]
    }
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::is_aarch64_feature_detected as has;
        vec![
            ("has_lse", value(has!("lse"))),
            ("has_fp16", value(has!("fp16"))),
            ("has_dotprod", value(has!("dotprod"))),
            ("has_i8mm", value(has!("i8mm"))),
        ]
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = value(false);
        Vec::new()
    }
}

/// The level this run is using.
///
/// Read once and cached: a backend is built per translated region, and rereading
/// the environment mid-run would let two regions in one process disagree about
/// how they were compiled.
pub fn opt_level() -> OptLevel {
    #[dynamic]
    static LEVEL: OptLevel = match std::env::var(OPT_ENV) {
        Err(_) => OptLevel::default(),
        Ok(value) => OptLevel::parse(&value).unwrap_or_else(|| {
            // Not fatal, but not silent either: a typo'd level that quietly
            // fell back would look like the level had no effect.
            eprintln!(
                "rosetta3: {OPT_ENV}={value:?} is not one of none, speed, all; using {}",
                OptLevel::default().as_str()
            );
            OptLevel::default()
        }),
    };
    *LEVEL
}

/// Whether Cranelift's IR verifier runs on everything this crate compiles.
///
/// **This is the single most expensive thing translation does.** Cranelift runs
/// the verifier at several points in a compile — after legalisation, after the
/// egraph pass, after register allocation — and each run is a full walk of the
/// function checking that every value is dominated by its definition, that
/// types agree, that the CFG is consistent. Measured on a 600-function x86-64
/// fixture, 601 regions and 39,085 guest instructions, same binary and machine:
///
/// ```text
///                     verifier on   verifier off
/// compile                 754 ms         405 ms
/// wall, process start     821 ms         466 ms
/// ```
///
/// **1.9× off the compile and 1.8× off the run**, for a check that says nothing
/// about the guest program and everything about whether microlift emitted
/// well-formed IR — which is a property of *microlift's source*, not of the
/// bytes it was pointed at, and so is answered by the lifter's own test suite
/// rather than by every region a shipped binary translates.
///
/// So the default is the build profile, which is exactly where FEX puts the
/// same decision: its `IRValidation` pass is inserted by `PassManager` under
/// `#if defined(ASSERTIONS_ENABLED)` and compiled out of a release build
/// entirely (`FEXCore/Source/Interface/IR/PassManager.cpp`). A debug build —
/// which is every `cargo test` in this workspace — verifies as it always has,
/// so the suite that would catch an invalid lift is unchanged. A release build
/// does not.
///
/// `ROSETTA3_JIT_VERIFY` overrides it in both directions, and both directions
/// are wanted: `=1` on a release build is how a lifting bug that only appears
/// in a long real-world run gets a diagnosis better than "wrong answer", and
/// `=0` on a debug build is how translation-heavy debugging stays bearable.
///
/// Read once, for the same reason [`opt_level`] is: two regions of one process
/// compiled under different rules is a difference nobody could later account
/// for.
pub fn verify() -> bool {
    #[dynamic]
    static VERIFY: bool = match std::env::var(VERIFY_ENV) {
        Err(_) => cfg!(debug_assertions),
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => {
                // Not fatal, but not silent: a typo'd value that quietly fell
                // back would look like the knob had no effect — and in the
                // direction that matters, it would look like verification was
                // on when it was not.
                eprintln!(
                    "rosetta3: {VERIFY_ENV}={value:?} is not a boolean; using {}",
                    cfg!(debug_assertions)
                );
                cfg!(debug_assertions)
            }
        },
    };
    *VERIFY
}

/// Whether microlift is asked to skip the IR for flag writes nothing reads.
///
/// `LifterConfig::eliminate_dead_flags` — microlift's port of FEX's
/// `RedundantFlagCalculationElimination`. It is the lift-side half of the same
/// problem [`verify`] is the compile-side half of: an `ADD` emits the IR for
/// all six status flags, and the ones nothing reads are deleted by Cranelift
/// only after being optimised and register-allocated first.
///
/// **Off by default, and this crate does not decide otherwise**, because
/// microlift's own default is off and for a reason it states: soundness rests
/// on a hand-audited table of what each instruction's handler does to the
/// flags, and a wrong entry is a wrong answer rather than an error. The
/// analysis around that table is conservative — a call, a return, an exit to
/// this runtime, or an instruction nobody classified keeps every flag — so the
/// exposure is the table and nothing else. `ROSETTA3_DEAD_FLAGS=1` is how a run
/// opts in while that table earns its confidence.
pub fn eliminate_dead_flags() -> bool {
    #[dynamic]
    static ON: bool = match std::env::var(DEAD_FLAGS_ENV) {
        Err(_) => false,
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => {
                eprintln!("rosetta3: {DEAD_FLAGS_ENV}={value:?} is not a boolean; using false");
                false
            }
        },
    };
    *ON
}

/// Whether a translated region may resolve its own successor rather than
/// returning to the run loop for it — microlift's `LifterConfig::block_link`.
///
/// **On by default**, unlike the two knobs above, because it is the answer to
/// the largest single item in this runtime's profile rather than an experiment:
/// fifteen million region entries a second, three quarters of which are a `ret`
/// or a call whose entire servicing was "enter what is at `rip`". See
/// `docs/HOTSPOTS.md`.
///
/// `ROSETTA3_NO_BLOCK_LINK=1` turns it off, and that is what it is for: the
/// linked and unlinked runs of the same program must agree on everything except
/// the number of entries that reached the run loop, and a claim like that is
/// only worth making if it can be run both ways. It also makes bisecting a
/// misbehaving program against the mechanism a variable rather than a rebuild.
pub fn block_linking() -> bool {
    #[dynamic]
    static ON: bool = match std::env::var(NO_BLOCK_LINK_ENV) {
        Err(_) => true,
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => false,
            "0" | "false" | "no" | "off" | "" => true,
            _ => {
                eprintln!(
                    "rosetta3: {NO_BLOCK_LINK_ENV}={value:?} is not a boolean; linking stays on"
                );
                true
            }
        },
    };
    *ON
}

/// Which guest registers this runtime keeps in **host registers** across a
/// link, or `None` to resolve links through the host as before.
///
/// This is the second shape of block linking (microlift's
/// [`core::pin`](microlift::core::pin)): with an ABI declared, a region that
/// resolves its own successor *branches* to it with these registers still in
/// place, so the register-file fill and spill is paid once per chain of regions
/// instead of once per region. `docs/HOTSPOTS.md` §5.1 is what that costs
/// today — about ninety host instructions of fill and the same again of spill,
/// at fifteen million region entries a second.
///
/// **The set is a policy and a guess, and it is deliberately overridable.**
/// There are five slots to spend on this host (aarch64's tail convention passes
/// six integer values and the context pointer takes one), and which five are
/// worth it is a property of the guest program rather than of the mechanism.
/// These are chosen for the profile in `docs/HOTSPOTS.md`, which is 92% calls
/// and returns through a C++ engine:
///
/// - `RSP` — every `call`, `ret`, `push` and `pop` in the profile touches it.
/// - `RDI` — the `this` pointer of every method call, and SysV argument 1.
/// - `RAX` — the return value, so live across exactly the `ret` edges that link.
/// - `RBP` — the frame pointer, which Apple's x86-64 ABI requires be maintained.
/// - `RSI` — SysV argument 2.
///
/// `ROSETTA3_PINNED_REGS=rsp,rax,…` replaces the list wholesale, so the guess
/// can be measured against alternatives without a rebuild;
/// `ROSETTA3_NO_PINNED_LINK=1` returns to host-resolved links, which is the
/// bisection knob between the two shapes of linking.
pub fn pinned_link_abi() -> Option<PinnedAbi> {
    #[dynamic]
    static ABI: Option<PinnedAbi> = {
        if !boolean_env(NO_PINNED_LINK_ENV, false, "pinned links stay on") {
            return None;
        }
        let abi = match std::env::var(PINNED_REGS_ENV) {
            Err(_) => PinnedAbi::new()
                .with(pin::RSP)
                .with(pin::RDI)
                .with(pin::RAX)
                .with(pin::RBP)
                .with(pin::RSI),
            Ok(list) => match parse_pinned_regs(&list) {
                Some(abi) => abi,
                None => return None,
            },
        };
        // A slot past the host's budget is not an error in microlift - it is a
        // stack slot, which is slower than leaving the register in the context
        // and invisible in anything but a profile. So it is an error here.
        if let Err(e) = abi.check_host_budget() {
            eprintln!("rosetta3: {e}; pinned links are off");
            return None;
        }
        Some(abi)
    };
    *ABI
}

/// Parse a `ROSETTA3_PINNED_REGS` list. `None` - with a message - for anything
/// it does not recognise, since a silently shorter ABI would be measured as a
/// result rather than as a typo.
fn parse_pinned_regs(list: &str) -> Option<PinnedAbi> {
    let mut abi = PinnedAbi::new();
    for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let reg = match name.to_ascii_lowercase().as_str() {
            "rax" => pin::RAX,
            "rcx" => pin::RCX,
            "rdx" => pin::RDX,
            "rbx" => pin::RBX,
            "rsp" => pin::RSP,
            "rbp" => pin::RBP,
            "rsi" => pin::RSI,
            "rdi" => pin::RDI,
            "r8" => pin::R8,
            "r9" => pin::R9,
            "r10" => pin::R10,
            "r11" => pin::R11,
            "r12" => pin::R12,
            "r13" => pin::R13,
            "r14" => pin::R14,
            "r15" => pin::R15,
            other => {
                eprintln!(
                    "rosetta3: {PINNED_REGS_ENV} names {other:?}, which is not an x86-64 \
                     general-purpose register; pinned links are off"
                );
                return None;
            }
        };
        if abi.contains(reg) {
            eprintln!("rosetta3: {PINNED_REGS_ENV} names {name} twice; pinned links are off");
            return None;
        }
        abi = abi.with(reg);
    }
    (!abi.is_empty()).then_some(abi)
}

/// Read a boolean environment variable that *disables* something, returning
/// whether the thing stays on.
fn boolean_env(name: &str, unset: bool, on_bad_value: &str) -> bool {
    match std::env::var(name) {
        Err(_) => !unset,
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => false,
            "0" | "false" | "no" | "off" | "" => true,
            _ => {
                eprintln!("rosetta3: {name}={value:?} is not a boolean; {on_bad_value}");
                true
            }
        },
    }
}

/// The Cranelift flags every backend in this process is built with.
pub fn flags() -> settings::Flags {
    #[dynamic]
    static FLAGS: settings::Flags = opt_level().flags();
    FLAGS.clone()
}

/// The ISA settings every backend in this process is built with.
///
/// Cached for the same reason as [`flags`], plus one of its own: the values are
/// CPU detection, and detection that ran once cannot disagree with itself
/// between two regions of the same run.
pub fn isa_settings() -> &'static [(&'static str, &'static str)] {
    #[dynamic]
    static SETTINGS: Vec<(&'static str, &'static str)> = opt_level().isa_settings();
    &SETTINGS
}

/// A JIT backend configured for this run.
///
/// Everything that compiles goes through here rather than
/// `CraneliftBackend::new()`, which would take Cranelift's unoptimised default.
///
/// ## Why it holds an `errno` guard
///
/// Compiling runs *on a guest thread*, between two guest instructions, and
/// `errno` is passthrough — so anything this does to `errno` is something the
/// guest reads as its own. Building the ISA does one thing to it: `cranelift-
/// native` detects the host's optional arm64 features with `sysctlbyname`, and
/// a probe for a name this machine does not have fails with `ENOENT`.
///
/// It happens once per process, because the feature detection underneath caches
/// its answers — and once is enough to be exactly the worst case. The first
/// region compiled is the first region *run*, so a guest whose `main` began
/// `fd = open(...); printf("%d", errno)` saw a 2 before it had made a call that
/// could have set one. See [`crate::thread::Errno`], and
/// `tests/thread.rs`'s `a_guest_thread_sees_its_own_errno`, which is that
/// program.
pub fn backend() -> microlift::Result<CraneliftBackend> {
    let _errno = crate::thread::Errno::held();
    CraneliftBackend::with_flags(flags(), isa_settings())
}

/// How the frontend is told which machine a [`Function`] is being built for.
///
/// The IR this crate writes by hand — the trampolines in [`crate::thunk`] — is
/// built with `cranelift_frontend::FunctionBuilder`, and since Cranelift 0.134
/// the frontend no longer infers the target: the pointer type is an explicit
/// argument to the stack-slot accessors and to `FunctionBuilder::finalize`.
/// This is the answer, and it has to be the *host's*, because the pointer being
/// described is the one the trampoline itself runs with — not the guest's,
/// which happens to be the same width and is otherwise unrelated.
///
/// Built from `cranelift-native`, which is where microlift's
/// [`CraneliftBackend`] gets its ISA too, so the function and the backend it is
/// handed to cannot disagree about the machine. The ISA settings of
/// [`isa_settings`] are deliberately not applied: they are target-*feature*
/// knobs, and nothing in a frontend config — calling convention, pointer width,
/// page size — is a function of which instructions the lowering may use.
///
/// [`Function`]: cranelift_codegen::ir::Function
pub fn frontend_config() -> microlift::Result<TargetFrontendConfig> {
    // The same guard, for the same reason and the same call: see [`backend`].
    // This one is reached from [`crate::thunk`], which builds a trampoline while
    // a guest thread is inside a host function waiting for its callback.
    let _errno = crate::thread::Errno::held();
    let isa = cranelift_native::builder()
        .map_err(|e| MicroliftError::CraneliftError(e.to_string()))?
        .finish(flags())
        .map_err(|e| MicroliftError::CraneliftError(e.to_string()))?;
    Ok(isa.frontend_config())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_names() {
        assert_eq!(OptLevel::parse("none"), Some(OptLevel::None));
        assert_eq!(OptLevel::parse("speed"), Some(OptLevel::Speed));
        assert_eq!(OptLevel::parse(" ALL\n"), Some(OptLevel::All));
        assert_eq!(OptLevel::parse("fast"), None);
        assert_eq!(OptLevel::parse(""), None);
    }

    #[test]
    fn all_is_the_default_and_the_only_host_specific_level() {
        assert_eq!(OptLevel::default(), OptLevel::All);
        assert!(OptLevel::All.host_specific());
        assert!(!OptLevel::Speed.host_specific());
        assert!(!OptLevel::None.host_specific());
    }

    /// The flags are what actually reach Cranelift, so assert on them and not
    /// just on the mapping function.
    #[test]
    fn levels_reach_cranelift_as_opt_level() {
        assert!(OptLevel::None.flags().opt_level() == settings::OptLevel::None);
        assert!(OptLevel::Speed.flags().opt_level() == settings::OptLevel::Speed);
        assert!(OptLevel::All.flags().opt_level() == settings::OptLevel::Speed);
    }

    /// The verifier is a property of the build and not of the optimisation
    /// level, so every level has to agree with [`verify`] about it — including
    /// `none`, which is the level a lifting bug is bisected at and therefore the
    /// one it would be worst to stop checking.
    #[test]
    fn the_verifier_is_the_same_answer_at_every_level() {
        for level in [OptLevel::None, OptLevel::Speed, OptLevel::All] {
            assert_eq!(
                level.flags().enable_verifier(),
                verify(),
                "{} disagrees about the verifier",
                level.as_str()
            );
        }
    }

    /// And with nothing in the environment, that answer is the build profile.
    ///
    /// The whole safety of turning the verifier off in a shipped binary is that
    /// the suite which would catch an invalid lift still runs with it on — so
    /// this pins the coupling rather than leaving it to be read off `flags()`.
    /// A plain `cargo test` is a debug build and therefore verifies; `cargo test
    /// --release` deliberately does not, which is how the *release* pipeline
    /// gets exercised at all.
    #[test]
    fn the_default_is_the_build_profile() {
        if std::env::var_os(VERIFY_ENV).is_none() {
            assert_eq!(verify(), cfg!(debug_assertions));
        }
    }

    /// A backend has to be constructible at every level, since an unknown ISA
    /// flag combination fails at `finish()` rather than at `set()`. This is also
    /// what catches a name in [`TUNABLE_FEATURES`] that the host's Cranelift
    /// backend does not have — `set()` rejects those, and it is the only check
    /// that the lists are spelled the way Cranelift spells them.
    #[test]
    fn every_level_builds_a_backend() {
        for level in [OptLevel::None, OptLevel::Speed, OptLevel::All] {
            assert!(
                CraneliftBackend::with_flags(level.flags(), &level.isa_settings()).is_ok(),
                "{} did not build a backend",
                level.as_str()
            );
        }
    }

    /// The three levels are three different statements about the host, not one
    /// statement at three volumes.
    #[test]
    fn levels_differ_in_what_they_say_about_the_host() {
        // `none` pins the baseline: every tunable named, all off.
        let baseline = OptLevel::None.isa_settings();
        assert_eq!(baseline.len(), TUNABLE_FEATURES.len());
        assert!(baseline.iter().all(|&(_, value)| value == "false"));

        // `speed` defers to cranelift-native's inference by saying nothing.
        assert!(OptLevel::Speed.isa_settings().is_empty());

        // `all` answers the same set of knobs, from detection.
        let host = OptLevel::All.isa_settings();
        assert_eq!(host.len(), TUNABLE_FEATURES.len());
        for &(name, _) in &host {
            assert!(
                TUNABLE_FEATURES.contains(&name),
                "{name} is tuned but not baselined"
            );
        }
        assert!(
            host.iter()
                .all(|&(_, value)| value == "true" || value == "false")
        );
    }

    /// The knobs that are ABI or correctness decisions stay out of the tuning,
    /// at every level — a JIT that stopped signing return addresses because the
    /// user asked for `none` would be a different ABI, not a slower one.
    #[test]
    fn abi_settings_are_not_tunable() {
        for level in [OptLevel::None, OptLevel::Speed, OptLevel::All] {
            for &(name, _) in &level.isa_settings() {
                assert!(
                    !matches!(
                        name,
                        "has_cmpxchg16b"
                            | "has_pauth"
                            | "use_bti"
                            | "sign_return_address"
                            | "sign_return_address_all"
                            | "sign_return_address_with_bkey"
                    ),
                    "{} touches {name}",
                    level.as_str()
                );
            }
        }
    }

    /// Detection is per-run state, so it must be the *same* per-run state
    /// everywhere: a region compiled with AVX and one compiled without, in one
    /// process, is a bug that would only show up as a mismatched trace.
    #[test]
    fn the_cached_settings_are_the_level_settings() {
        assert_eq!(isa_settings(), opt_level().isa_settings().as_slice());
    }
}
