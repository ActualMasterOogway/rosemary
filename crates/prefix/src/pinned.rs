use crate::format::{Invalid, Spec};

include!(concat!(env!("OUT_DIR"), "/pinned.rs"));

/// The pinned spec's text for `arch` (MiniDarwin's spelling: `x86_64`,
/// `aarch64`).
pub fn text(arch: &str) -> Option<&'static str> {
    ALL.iter().find(|(a, _)| *a == arch).map(|(_, text)| *text)
}

pub fn spec(arch: &str) -> Option<Result<Spec, Invalid>> {
    text(arch).map(|text| Spec::parse(text.as_bytes(), &format!("pinned {arch} spec")))
}

/// The architectures a spec is pinned for.
pub fn architectures() -> impl Iterator<Item = &'static str> {
    ALL.iter().map(|(arch, _)| *arch)
}

/// This host's architecture in MiniDarwin's spelling, which is the default
/// base for a new prefix: a prefix is made before any guest is known. Rust's
/// names for the two architectures MiniDarwin builds are Nix's too.
pub fn host_arch() -> &'static str {
    std::env::consts::ARCH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_architectures_are_pinned() {
        assert!(text("x86_64").is_some());
        assert!(text("aarch64").is_some());
        assert!(text("ppc").is_none());
    }
}
