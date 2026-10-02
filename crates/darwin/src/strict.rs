/// The variable that turns the refusal on. Any value but `0`.
pub const VAR: &str = "ROSEMARY_NO_PASSTHROUGH";

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
