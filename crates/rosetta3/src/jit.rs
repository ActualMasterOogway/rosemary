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

use std::sync::OnceLock;

use cranelift_codegen::isa::TargetFrontendConfig;
use cranelift_codegen::settings::{self, Configurable};
use microlift::MicroliftError;
use microlift::backend::CraneliftBackend;

/// The environment variable this module reads.
pub const OPT_ENV: &str = "ROSETTA3_JIT_OPT";

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
const TUNABLE_FEATURES: &[&str] = &["has_lse", "has_fp16"];

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
            // FEAT_LSE: the atomics a guest `lock`-prefixed instruction lowers
            // to, as single instructions instead of a load-exclusive loop.
            ("has_lse", value(has!("lse"))),
            ("has_fp16", value(has!("fp16"))),
            ("has_dotprod", value(has!("dotprod"))),
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
    static LEVEL: OnceLock<OptLevel> = OnceLock::new();
    *LEVEL.get_or_init(|| match std::env::var(OPT_ENV) {
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
    })
}

/// The Cranelift flags every backend in this process is built with.
pub fn flags() -> settings::Flags {
    static FLAGS: OnceLock<settings::Flags> = OnceLock::new();
    FLAGS.get_or_init(|| opt_level().flags()).clone()
}

/// The ISA settings every backend in this process is built with.
///
/// Cached for the same reason as [`flags`], plus one of its own: the values are
/// CPU detection, and detection that ran once cannot disagree with itself
/// between two regions of the same run.
pub fn isa_settings() -> &'static [(&'static str, &'static str)] {
    static SETTINGS: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    SETTINGS.get_or_init(|| opt_level().isa_settings())
}

/// A JIT backend configured for this run.
///
/// Everything that compiles goes through here rather than
/// `CraneliftBackend::new()`, which would take Cranelift's unoptimised default.
pub fn backend() -> microlift::Result<CraneliftBackend> {
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
