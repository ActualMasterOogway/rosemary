//! `ROSETTA3_NO_PASSTHROUGH`: refuse the host fallback entirely.
//!
//! [`crate::symbol`] answers a guest import from this tree first and only then
//! from the host, so a name nobody here looked at is bound to this host's own
//! libc and runs. That is the right default - most of libc does agree - but it
//! means "which calls is the guest making that this tree does not implement" is
//! answerable only by reading [`crate::passthrough`] against a trace.
//!
//! With this set the fallback is gone: an import this tree does not implement is
//! refused by name, the same way a [`crate::divergent`] one is, so the run stops
//! at the first undescribed symbol instead of running on the host's version of
//! it. The point is to produce the list, not to run the guest - a real program
//! refuses long before `main`.
//!
//! Nothing on Darwin, where the host *is* the platform being emulated.
//!
//! What this withdraws is the *fallback*: the host provider at the end of the
//! link chain, and the global `dlsym` behind [`crate::symbol`]. Three lookups
//! stay, because none of them is one - the functions hand-written shims forward
//! to ([`crate::shim_symbol`]), the GL entry points `rosetta3::opengl` takes
//! from the driver SDL made the context against, and a library the guest itself
//! opened by name. Each is a named choice of a host function, which is what a
//! shim is.

/// The variable that turns the refusal on. Any value but `0`.
pub const VAR: &str = "ROSETTA3_NO_PASSTHROUGH";

/// Whether this run refuses to answer a guest import with a host symbol.
pub fn enabled() -> bool {
    #[cfg(target_os = "macos")]
    {
        false
    }
    #[cfg(not(target_os = "macos"))]
    {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            std::env::var_os(VAR).is_some_and(|value| value != "0" && !value.is_empty())
        })
    }
}
