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
//! Two knobs, not one. `opt_level` is a Cranelift setting and reaches the
//! backend as a [`settings::Flags`]. The architecture-specific half is a
//! *lifter* decision — [`LifterConfig::use_vector_xmm`] makes an `xmm` register
//! one `I64X2` SSA variable instead of a scalar lo/hi pair, which is only worth
//! doing when the host has the vector unit to lower it onto. Both are answered
//! from the same setting so that one environment variable describes the whole
//! pipeline rather than half of it.
//!
//! [`LifterConfig::use_vector_xmm`]: microlift::core::LifterConfig::use_vector_xmm

use std::sync::OnceLock;

use cranelift_codegen::settings::{self, Configurable};
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
    None,
    /// `speed` — Cranelift's `opt_level=speed`.
    ///
    /// The level microlift's IR is shaped for: dead-code elimination over the
    /// SSA variables standing in for guest side effects is where most of the
    /// per-instruction emulation goes away.
    Speed,
    /// `all` — `speed`, plus host-specific choices at *lift* time. The default.
    ///
    /// Currently that means vector `xmm` state when the host is detected to
    /// support it. Unlike the Cranelift setting, this changes the IR itself, so
    /// it is the one level that is not purely a lowering decision.
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

/// A JIT backend configured for this run.
///
/// Everything that compiles goes through here rather than
/// `CraneliftBackend::new()`, which would take Cranelift's unoptimised default.
pub fn backend() -> microlift::Result<CraneliftBackend> {
    CraneliftBackend::with_flags(flags())
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
    /// flag combination fails at `finish()` rather than at `set()`.
    #[test]
    fn every_level_builds_a_backend() {
        for level in [OptLevel::None, OptLevel::Speed, OptLevel::All] {
            assert!(
                CraneliftBackend::with_flags(level.flags()).is_ok(),
                "{} did not build a backend",
                level.as_str()
            );
        }
    }
}
