use static_init::dynamic;

/// The libc implementation used for guest imports.
pub const VAR: &str = "ROSEMARY_LIBC";

#[inline(always)]
pub fn using_builtin() -> bool {
    if cfg!(target_os = "macos") {
        #[dynamic]
        static CHOICE: bool = match std::env::var(VAR).as_deref() {
            Ok("builtin") => true,
            Ok("native") => false,
            Ok(other) if !other.is_empty() => {
                panic!("{VAR}={other}: expected `native` or `builtin`")
            }
            _ => false,
        };
        *CHOICE
    } else {
        true
    }
}
