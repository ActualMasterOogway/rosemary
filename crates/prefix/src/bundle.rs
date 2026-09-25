use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::format::{Invalid, MANIFEST_LIMIT, SPEC_LIMIT};

pub const SPEC: &str = "spec.yaml";
pub const MANIFEST: &str = "manifest.yaml";
pub const ARTIFACT: &str = "rootfs.tar.gz";
const MEMBERS: [&str; 3] = [SPEC, MANIFEST, ARTIFACT];

const EOCD: u32 = 0x0605_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const LOCAL: u32 = 0x0403_4b50;

/// Where the three members were copied to.
#[derive(Debug, Clone)]
pub struct Opened {
    pub spec: PathBuf,
    pub manifest: PathBuf,
    pub artifact: PathBuf,
}

struct Member {
    name: String,
    size: u64,
    crc: u32,
    local: u64,
}

/// Copy the three members of the bundle at `path` into `into`, which must not
/// exist. `artifact_limit` bounds the artifact when a trusted spec gives its
/// size.
pub fn open(path: &Path, into: &Path, artifact_limit: Option<u64>) -> Result<Opened, Invalid> {
    let bad = |message: &str| Invalid::new(format!("bundle: {message}"));
    let io = |e: std::io::Error| Invalid::new(format!("bundle: {}: {e}", path.display()));
    let mut file = File::open(path).map_err(io)?;
    let length = file.metadata().map_err(io)?.len();

    // The end-of-central-directory record is the last 22 bytes plus a comment
    // of at most 64 KiB.
    let tail_length = length.min(22 + 0xffff);
    file.seek(SeekFrom::Start(length - tail_length))
        .map_err(io)?;
    let mut tail = vec![0; tail_length as usize];
    file.read_exact(&mut tail).map_err(io)?;
    let at = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| u32_at(&tail, i) == EOCD)
        .ok_or_else(|| bad("not a zip file"))?;
    let eocd = &tail[at..];
    let (disk, cd_disk) = (u16_at(eocd, 4), u16_at(eocd, 6));
    let (count_here, count) = (u16_at(eocd, 8), u16_at(eocd, 10));
    let cd_size = u32_at(eocd, 12) as u64;
    let cd_offset = u32_at(eocd, 16) as u64;
    if disk != 0 || cd_disk != 0 || count_here != count {
        return Err(bad("multi-disk archives are not supported"));
    }
    if count == 0xffff || cd_offset == 0xffff_ffff || cd_size == 0xffff_ffff {
        return Err(bad("zip64 is not supported"));
    }
    if cd_offset + cd_size > length {
        return Err(bad("central directory is out of bounds"));
    }
    if count as usize != MEMBERS.len() {
        return Err(bad(&format!(
            "members must be exactly {}; found {count}",
            MEMBERS.join(", ")
        )));
    }

    file.seek(SeekFrom::Start(cd_offset)).map_err(io)?;
    let mut cd = vec![0; cd_size as usize];
    file.read_exact(&mut cd).map_err(io)?;
    let mut members = Vec::new();
    let mut pos = 0;
    for _ in 0..count {
        if pos + 46 > cd.len() || u32_at(&cd, pos) != CENTRAL {
            return Err(bad("malformed central directory"));
        }
        let flags = u16_at(&cd, pos + 8);
        let method = u16_at(&cd, pos + 10);
        let crc = u32_at(&cd, pos + 16);
        let compressed = u32_at(&cd, pos + 20) as u64;
        let size = u32_at(&cd, pos + 24) as u64;
        let name_length = u16_at(&cd, pos + 28) as usize;
        let extra_length = u16_at(&cd, pos + 30) as usize;
        let comment_length = u16_at(&cd, pos + 32) as usize;
        let local = u32_at(&cd, pos + 42) as u64;
        let name = cd
            .get(pos + 46..pos + 46 + name_length)
            .ok_or_else(|| bad("malformed central directory"))?;
        let name = String::from_utf8_lossy(name).into_owned();
        if flags & 1 != 0 {
            return Err(bad(&format!("{name} is encrypted")));
        }
        if method != 0 || compressed != size {
            return Err(bad(&format!(
                "{name} is compressed; a bundle's members are stored"
            )));
        }
        if size == 0xffff_ffff || local == 0xffff_ffff {
            return Err(bad("zip64 is not supported"));
        }
        members.push(Member {
            name,
            size,
            crc,
            local,
        });
        pos += 46 + name_length + extra_length + comment_length;
    }
    let mut names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
    names.sort_unstable();
    let mut wanted = MEMBERS;
    wanted.sort_unstable();
    if names != wanted {
        return Err(bad(&format!(
            "members must be exactly {}; found {}",
            MEMBERS.join(", "),
            names.join(", ")
        )));
    }

    std::fs::create_dir(into).map_err(|e| Invalid::new(format!("{}: {e}", into.display())))?;
    for member in &members {
        let limit = match member.name.as_str() {
            SPEC => Some(SPEC_LIMIT),
            MANIFEST => Some(MANIFEST_LIMIT),
            _ => artifact_limit,
        };
        if limit.is_some_and(|limit| member.size > limit) {
            return Err(bad(&format!(
                "{} is larger than {} bytes",
                member.name,
                limit.unwrap_or_default()
            )));
        }
        file.seek(SeekFrom::Start(member.local)).map_err(io)?;
        let mut header = [0u8; 30];
        file.read_exact(&mut header).map_err(io)?;
        if u32_at(&header, 0) != LOCAL {
            return Err(bad(&format!("{}: malformed local header", member.name)));
        }
        let name_length = u16_at(&header, 26) as u64;
        let extra_length = u16_at(&header, 28) as u64;
        let mut local_name = vec![0; name_length as usize];
        file.read_exact(&mut local_name).map_err(io)?;
        if local_name != member.name.as_bytes() {
            return Err(bad(&format!(
                "{}: local header names a different file",
                member.name
            )));
        }
        let start = member.local + 30 + name_length + extra_length;
        if start + member.size > length {
            return Err(bad(&format!("{} is out of bounds", member.name)));
        }
        file.seek(SeekFrom::Start(start)).map_err(io)?;
        let out_path = into.join(&member.name);
        let mut out = File::create_new(&out_path)
            .map_err(|e| Invalid::new(format!("{}: {e}", out_path.display())))?;
        let mut crc = flate2::Crc::new();
        let mut remaining = member.size;
        let mut buffer = vec![0; 1 << 20];
        while remaining > 0 {
            let n = remaining.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..n]).map_err(io)?;
            crc.update(&buffer[..n]);
            out.write_all(&buffer[..n])
                .map_err(|e| Invalid::new(format!("{}: {e}", out_path.display())))?;
            remaining -= n as u64;
        }
        if crc.sum() != member.crc {
            return Err(bad(&format!("{} fails its CRC", member.name)));
        }
    }
    Ok(Opened {
        spec: into.join(SPEC),
        manifest: into.join(MANIFEST),
        artifact: into.join(ARTIFACT),
    })
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::verify::tests::scratch;

    /// A stored zip, laid out the way Python's `zipfile` writes one.
    pub(crate) fn zip(members: &[(&str, &[u8])], method: u16) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, content) in members {
            let mut crc = flate2::Crc::new();
            crc.update(content);
            let offset = out.len() as u32;
            let size = content.len() as u32;
            out.extend_from_slice(&LOCAL.to_le_bytes());
            out.extend_from_slice(&[20, 0, 0, 0]);
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0, 0, 0x21, 0]);
            out.extend_from_slice(&crc.sum().to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(content);

            central.extend_from_slice(&CENTRAL.to_le_bytes());
            central.extend_from_slice(&[20, 3, 20, 0, 0, 0]);
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0, 0, 0x21, 0]);
            central.extend_from_slice(&crc.sum().to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 8]);
            central.extend_from_slice(&0o100644u32.wrapping_shl(16).to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&EOCD.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        let count = (members.len() as u16).to_le_bytes();
        out.extend_from_slice(&count);
        out.extend_from_slice(&count);
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    #[test]
    fn a_bundle_opens_into_its_three_files() {
        let at = scratch("bundle-good");
        let path = at.join("b.zip");
        std::fs::write(
            &path,
            zip(
                &[(SPEC, b"s"), (MANIFEST, b"m"), (ARTIFACT, b"artifact")],
                0,
            ),
        )
        .unwrap();
        let opened = open(&path, &at.join("out"), None).expect("opens");
        assert_eq!(std::fs::read(opened.artifact).unwrap(), b"artifact");
        assert_eq!(std::fs::read(opened.spec).unwrap(), b"s");
        let _ = std::fs::remove_dir_all(&at);
    }

    #[test]
    fn a_bundle_is_exactly_three_stored_members() {
        let at = scratch("bundle-bad");
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("missing", zip(&[(SPEC, b"s"), (MANIFEST, b"m")], 0)),
            (
                "extra",
                zip(
                    &[(SPEC, b"s"), (MANIFEST, b"m"), (ARTIFACT, b"a"), ("x", b"")],
                    0,
                ),
            ),
            (
                "renamed",
                zip(
                    &[(SPEC, b"s"), (MANIFEST, b"m"), ("../rootfs.tar.gz", b"a")],
                    0,
                ),
            ),
            (
                "deflated",
                zip(&[(SPEC, b"s"), (MANIFEST, b"m"), (ARTIFACT, b"a")], 8),
            ),
            ("garbage", b"not a zip at all, not even close".to_vec()),
        ];
        for (index, (name, bytes)) in cases.iter().enumerate() {
            let path = at.join(format!("{index}.zip"));
            std::fs::write(&path, bytes).unwrap();
            assert!(
                open(&path, &at.join(format!("out{index}")), None).is_err(),
                "{name} was accepted"
            );
        }
        // Too large for the trusted spec's size.
        let path = at.join("big.zip");
        std::fs::write(
            &path,
            zip(
                &[(SPEC, b"s"), (MANIFEST, b"m"), (ARTIFACT, b"0123456789")],
                0,
            ),
        )
        .unwrap();
        assert!(open(&path, &at.join("outbig"), Some(4)).is_err());
        let _ = std::fs::remove_dir_all(&at);
    }
}
