//! How hard Cranelift is asked to work on the code it emits.

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
    /// Slowest by a wide margin. It is here as a bisection tool.
    None,
    /// `speed` — Cranelift's `opt_level=speed`.
    Speed,
    /// `all` — `speed`, plus host-specific choices. The default.
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
    /// `All` deliberately maps to `speed` and not `speed_and_size`.
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
        // because it is not an optimisation setting.
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
#[cfg(target_arch = "aarch64")]
const TUNABLE_FEATURES: &[&str] = &["has_lse", "has_fp16", "has_dotprod", "has_i8mm"];

/// The host target features this crate has an opinion about.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const TUNABLE_FEATURES: &[&str] = &[];

/// [`TUNABLE_FEATURES`], each paired with whether this CPU has it.
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
/// **This is the single most expensive thing translation does.** Cranelift runs the
/// verifier at several points in a compile — after legalisation, after the egraph pass,
/// after register allocation — and each run is a full walk of the function checking
/// that every value is dominated by its definition, that types agree, that the CFG is
/// consistent.
///
/// ```text
///                     verifier on   verifier off
/// compile                 754 ms         405 ms
/// wall, process start     821 ms         466 ms
/// ```
///
/// **1.9× off the compile and 1.8× off the run**, for a check that says nothing about
/// the guest program and everything about whether microlift emitted well-formed IR —
/// which is a property of *microlift's source*, not of the bytes it was pointed at, and
/// so is answered by the lifter's own test suite rather than by every region a shipped
/// binary translates.
pub fn verify() -> bool {
    #[dynamic]
    static VERIFY: bool = match std::env::var(VERIFY_ENV) {
        Err(_) => cfg!(debug_assertions),
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => {
                // Not fatal, but not silent.
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

/// Whether a translated region may resolve its own successor rather than returning to
/// the run loop for it — microlift's `LifterConfig::block_link`.
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

/// Which guest registers this runtime keeps in **host registers** across a link, or
/// `None` to resolve links through the host as before.
///
/// **The set is a policy and a guess, and it is deliberately overridable.** There are
/// five slots to spend on this host (aarch64's tail convention passes six integer
/// values and the context pointer takes one), and which five are worth it is a property
/// of the guest program rather than of the mechanism.
///
/// - `RSP` — every `call`, `ret`, `push` and `pop` in the profile touches it.
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

/// Parse a `ROSETTA3_PINNED_REGS` list.
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
pub fn isa_settings() -> &'static [(&'static str, &'static str)] {
    #[dynamic]
    static SETTINGS: Vec<(&'static str, &'static str)> = opt_level().isa_settings();
    &SETTINGS
}

/// A JIT backend configured for this run.
///
/// ## Why it holds an `errno` guard
///
/// Compiling runs *on a guest thread*, between two guest instructions, and `errno` is
/// passthrough — so anything this does to `errno` is something the guest reads as its
/// own.
pub fn backend() -> microlift::Result<CraneliftBackend> {
    let _errno = crate::thread::Errno::held();
    CraneliftBackend::with_flags(flags(), isa_settings())
}

/// How the frontend is told which machine a `Function` is being built for.
///
/// Built from `cranelift-native`, which is where microlift's [`CraneliftBackend`] gets
/// its ISA too, so the function and the backend it is handed to cannot disagree about
/// the machine.
pub fn frontend_config() -> microlift::Result<TargetFrontendConfig> {
    // The same guard, for the same reason and the same call: see [`backend`].
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
    /// The whole safety of turning the verifier off in a shipped binary is that the
    /// suite which would catch an invalid lift still runs with it on — so this pins the
    /// coupling rather than leaving it to be read off `flags()`.
    #[test]
    fn the_default_is_the_build_profile() {
        if std::env::var_os(VERIFY_ENV).is_none() {
            assert_eq!(verify(), cfg!(debug_assertions));
        }
    }

    /// A backend has to be constructible at every level, since an unknown ISA flag
    /// combination fails at `finish()` rather than at `set()`.
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

    /// Detection is per-run state, so it must be the *same* per-run state everywhere.
    #[test]
    fn the_cached_settings_are_the_level_settings() {
        assert_eq!(isa_settings(), opt_level().isa_settings().as_slice());
    }
}
