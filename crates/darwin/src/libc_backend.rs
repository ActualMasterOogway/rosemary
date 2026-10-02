/// The libc implementation used for guest imports.
pub const VAR: &str = "ROSEMARY_LIBC";

pub fn using_builtin() -> bool {
    static CHOICE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CHOICE.get_or_init(|| match std::env::var(VAR).as_deref() {
        Ok("builtin") => true,
        Ok("native") if cfg!(target_os = "macos") => false,
        Ok(other) if !other.is_empty() => {
            panic!("{VAR}={other}: expected `native` or `builtin`")
        }
        _ => !cfg!(target_os = "macos"),
    })
}
