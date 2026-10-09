use std::fs;
use std::path::Path;

/// A glibc symbol version, as in `GLIBC_2.39`. Patch levels (`GLIBC_2.2.5`) are
/// dropped: no release has ever differed only in one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl Version {
    /// `2.39`, `2.2.5` or `GLIBC_2.39`.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.strip_prefix("GLIBC_").unwrap_or(text);
        let mut parts = text.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        for rest in parts {
            rest.parse::<u32>().ok()?;
        }
        Some(Self { major, minor })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Where distributions keep the C library, most specific first.
#[cfg(target_arch = "x86_64")]
pub const LIBRARY_DIRS: &[&str] = &[
    "/lib/x86_64-linux-gnu",
    "/usr/lib/x86_64-linux-gnu",
    "/lib64",
    "/usr/lib64",
    "/usr/lib",
    "/lib",
];
#[cfg(target_arch = "aarch64")]
pub const LIBRARY_DIRS: &[&str] = &[
    "/lib/aarch64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
    "/lib64",
    "/usr/lib64",
    "/usr/lib",
    "/lib",
];
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub const LIBRARY_DIRS: &[&str] = &["/usr/lib", "/lib"];

/// The dynamic loader the real binary names as its interpreter.
#[cfg(target_arch = "x86_64")]
pub const HOST_LOADER: &str = "/lib64/ld-linux-x86-64.so.2";
#[cfg(target_arch = "aarch64")]
pub const HOST_LOADER: &str = "/lib/ld-linux-aarch64.so.1";
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub const HOST_LOADER: &str = "/lib/ld.so.1";

/// The newest glibc symbol version the host's C library defines, or `None`
/// when it has none (musl) or there is no C library where we look.
pub fn host_version() -> Option<Version> {
    LIBRARY_DIRS
        .iter()
        .find_map(|dir| defined_by(&Path::new(dir).join("libc.so.6")))
}

/// The newest `GLIBC_x.y` version a shared object defines.
pub fn defined_by(path: &Path) -> Option<Version> {
    highest_defined(&fs::read(path).ok()?)
}

const SHT_GNU_VERDEF: u32 = 0x6fff_fffd;

struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn array<const N: usize>(&self, at: usize) -> Option<[u8; N]> {
        self.0.get(at..at.checked_add(N)?)?.try_into().ok()
    }
    fn u16(&self, at: usize) -> Option<u16> {
        Some(u16::from_le_bytes(self.array(at)?))
    }
    fn u32(&self, at: usize) -> Option<u32> {
        Some(u32::from_le_bytes(self.array(at)?))
    }
    fn u64(&self, at: usize) -> Option<u64> {
        Some(u64::from_le_bytes(self.array(at)?))
    }
    fn offset(&self, at: usize) -> Option<usize> {
        usize::try_from(self.u64(at)?).ok()
    }
    fn cstr(&self, at: usize) -> Option<&str> {
        let tail = self.0.get(at..)?;
        let end = tail.iter().position(|&b| b == 0)?;
        std::str::from_utf8(&tail[..end]).ok()
    }
}

pub fn highest_defined(image: &[u8]) -> Option<Version> {
    let elf = Bytes(image);
    // ELFCLASS64, ELFDATA2LSB.
    if image.get(..4)? != b"\x7fELF" || *image.get(4)? != 2 || *image.get(5)? != 1 {
        return None;
    }
    let table = elf.offset(0x28)?;
    let entry = usize::from(elf.u16(0x3a)?);
    let count = usize::from(elf.u16(0x3c)?);
    let header = |index: usize| table.checked_add(index.checked_mul(entry)?);

    for index in 0..count {
        let section = header(index)?;
        if elf.u32(section + 4)? != SHT_GNU_VERDEF {
            continue;
        }
        let definitions = elf.offset(section + 0x18)?;
        let strings = elf.offset(header(elf.u32(section + 0x28)? as usize)? + 0x18)?;
        let mut best = None;
        let mut at = definitions;
        for _ in 0..elf.u32(section + 0x2c)? {
            // Verdef: the name is that of its first Verdaux.
            let aux = at.checked_add(elf.u32(at + 12)? as usize)?;
            let name = elf.cstr(strings.checked_add(elf.u32(aux)? as usize)?)?;
            if name.starts_with("GLIBC_") {
                best = best.max(Version::parse(name));
            }
            match elf.u32(at + 16)? {
                0 => break,
                next => at = at.checked_add(next as usize)?,
            }
        }
        return best;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("2.9") < v("2.10"));
        assert!(v("GLIBC_2.2.5") < v("2.3"));
        assert_eq!(v("GLIBC_2.39.1"), v("2.39"));
        assert_eq!(Version::parse("GLIBC_PRIVATE"), None);
        assert_eq!(Version::parse("2"), None);
    }

    #[test]
    fn rejects_what_is_not_a_64_bit_little_endian_elf() {
        assert_eq!(highest_defined(b""), None);
        assert_eq!(highest_defined(b"\x7fELF\x01\x01"), None);
        assert_eq!(highest_defined(&[0u8; 64]), None);
        // Truncated right after a plausible header must not panic.
        let mut cut = vec![0u8; 0x40];
        cut[..6].copy_from_slice(b"\x7fELF\x02\x01");
        cut[0x28] = 0xff;
        assert_eq!(highest_defined(&cut), None);
    }

    #[test]
    fn reads_the_libc_it_is_pointed_at() {
        // Hosts without a libc.so.6 at the usual places (NixOS) point this at
        // a glibc's own; unset, there is nothing to read.
        let Some(path) = std::env::var_os("LAUNCHER_TEST_LIBC") else {
            return;
        };
        let found = defined_by(Path::new(&path)).expect("a glibc with version definitions");
        assert!(
            found
                >= Version {
                    major: 2,
                    minor: 17
                },
            "{found}"
        );
        eprintln!("defined: {found}");
    }

    #[test]
    fn reads_the_hosts_libc_when_it_is_glibc() {
        if let Some(found) = host_version() {
            assert!(
                found
                    >= Version {
                        major: 2,
                        minor: 17
                    },
                "{found}"
            );
        }
    }
}
