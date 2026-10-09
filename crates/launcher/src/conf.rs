//! The bundle's description of itself, `share/rosemary/bundle.conf`.
//!
//! `key=value` lines, `#` comments. Kept this plain because the launcher has
//! no dependencies to parse anything richer with.

use crate::glibc::Version;
use std::path::Path;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Conf {
    /// The newest glibc symbol version anything in the bundle needs. A host
    /// whose glibc defines at least this can run the bundle's libraries.
    pub required_glibc: Option<Version>,
    /// Variables to set unless the caller already did, as `(name, value)`
    /// with `@ROOT@` standing for the bundle root.
    pub default_env: Vec<(String, String)>,
}

impl Conf {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    pub fn parse(text: &str) -> Self {
        let mut conf = Self::default();
        for line in text.lines() {
            let line = line.trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            if key == "required_glibc" {
                conf.required_glibc = Version::parse(value);
            } else if let Some(name) = key.strip_prefix("default_env.") {
                conf.default_env.push((name.to_string(), value.to_string()));
            }
        }
        conf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_requirement_and_default_environment() {
        let conf =
            Conf::parse("# comment\nrequired_glibc = 2.42\ndefault_env.A=@ROOT@/x\n\nnonsense\n");
        assert_eq!(conf.required_glibc, Version::parse("2.42"));
        assert_eq!(
            conf.default_env,
            [("A".to_string(), "@ROOT@/x".to_string())]
        );
    }

    #[test]
    fn a_missing_file_is_an_empty_conf() {
        assert_eq!(
            Conf::load(Path::new("/nonexistent/bundle.conf")),
            Conf::default()
        );
    }
}
