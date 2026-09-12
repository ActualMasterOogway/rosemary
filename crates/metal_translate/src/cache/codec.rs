//! The on-disk entry format.
//!
//! A cache entry is the whole `TranslatedLibrary`, so the codec has to
//! carry every reflection field the runtime binds resources from. The tag
//! tables below are what make an enum's numbering part of the format
//! rather than of the Rust definition.

use super::*;

pub(super) fn encode(key: &[u8; 32], library: &TranslatedLibrary) -> Vec<u8> {
    let mut payload = Vec::new();
    write_u32(&mut payload, library.entries.len() as u32);
    for entry in &library.entries {
        write_string(&mut payload, &entry.name);
        payload.push(stage_tag(entry.stage));
        write_u32(&mut payload, entry.words.len() as u32);
        for word in &entry.words {
            write_u32(&mut payload, *word);
        }
        write_u32(&mut payload, entry.resources.len() as u32);
        for resource in &entry.resources {
            payload.push(stage_tag(resource.stage));
            payload.push(resource_kind_tag(resource.kind));
            write_u32(&mut payload, resource.metal_index);
            write_u32(&mut payload, resource.descriptor_set);
            write_u32(&mut payload, resource.binding);
            write_u32(&mut payload, resource.count);
            payload.push(access_tag(resource.access));
            payload.push(u8::from(resource.sampled));
        }
        write_strings(&mut payload, &entry.required.capabilities);
        write_strings(&mut payload, &entry.required.extensions);
    }
    write_u32(&mut payload, library.function_constants.len() as u32);
    for constant in &library.function_constants {
        write_u32(&mut payload, constant.index);
        write_string(&mut payload, &constant.name);
        write_string(&mut payload, &constant.encoding);
    }

    let mut bytes = Vec::with_capacity(payload.len() + 112);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&sha256::digest(CACHE_VERSION.as_bytes()));
    bytes.extend_from_slice(key);
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&sha256::digest(&payload));
    bytes.extend_from_slice(&payload);
    bytes
}

pub(super) fn decode(bytes: &[u8], key: &[u8; 32]) -> Option<TranslatedLibrary> {
    let mut reader = Reader::new(bytes);
    if reader.take(MAGIC.len())? != MAGIC {
        return None;
    }
    // The key already covers the crate version, so this only catches a file
    // moved between version directories by hand.
    if reader.take(32)? != sha256::digest(CACHE_VERSION.as_bytes()) {
        return None;
    }
    if reader.take(32)? != key {
        return None;
    }
    let length = usize::try_from(reader.u64()?).ok()?;
    let checksum = reader.take(32)?.to_vec();
    let payload = reader.take(length)?;
    if !reader.at_end() || sha256::digest(payload) != checksum.as_slice() {
        return None;
    }

    let mut reader = Reader::new(payload);
    let count = reader.u32()? as usize;
    let mut entries = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let name = reader.string()?;
        let stage = stage_of_tag(reader.u8()?)?;
        let word_count = reader.u32()? as usize;
        let mut words = Vec::with_capacity(word_count.min(1 << 16));
        for _ in 0..word_count {
            words.push(reader.u32()?);
        }
        let resource_count = reader.u32()? as usize;
        let mut resources = Vec::with_capacity(resource_count.min(1024));
        for _ in 0..resource_count {
            resources.push(ResourceBinding {
                stage: stage_of_tag(reader.u8()?)?,
                kind: resource_kind_of_tag(reader.u8()?)?,
                metal_index: reader.u32()?,
                descriptor_set: reader.u32()?,
                binding: reader.u32()?,
                count: reader.u32()?,
                access: access_of_tag(reader.u8()?)?,
                sampled: reader.boolean()?,
            });
        }
        entries.push(EntryArtifact {
            name,
            stage,
            words,
            resources,
            required: RequiredFeatures {
                capabilities: reader.strings()?,
                extensions: reader.strings()?,
            },
        });
    }
    let count = reader.u32()? as usize;
    let mut function_constants = Vec::with_capacity(count.min(256));
    for _ in 0..count {
        function_constants.push(FunctionConstantInfo {
            index: reader.u32()?,
            name: reader.string()?,
            encoding: reader.string()?,
        });
    }
    reader.at_end().then_some(TranslatedLibrary {
        entries,
        function_constants,
    })
}

fn write_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn write_string(bytes: &mut Vec<u8>, text: &str) {
    write_u32(bytes, text.len() as u32);
    bytes.extend_from_slice(text.as_bytes());
}

fn write_strings(bytes: &mut Vec<u8>, texts: &[String]) {
    write_u32(bytes, texts.len() as u32);
    for text in texts {
        write_string(bytes, text);
    }
}

/// A bounds-checked reader: a corrupt entry answers `None` and is discarded,
/// and no length in the file is ever trusted enough to index with.
struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let end = self.offset.checked_add(length)?;
        let slice = self.bytes.get(self.offset..end)?;
        self.offset = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn boolean(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn string(&mut self) -> Option<String> {
        let length = self.u32()? as usize;
        std::str::from_utf8(self.take(length)?)
            .ok()
            .map(str::to_owned)
    }

    fn strings(&mut self) -> Option<Vec<String>> {
        let count = self.u32()? as usize;
        let mut texts = Vec::with_capacity(count.min(256));
        for _ in 0..count {
            texts.push(self.string()?);
        }
        Some(texts)
    }

    fn at_end(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

fn stage_tag(stage: Stage) -> u8 {
    match stage {
        Stage::Vertex => 0,
        Stage::Fragment => 1,
        Stage::Kernel => 2,
        Stage::Visible => 3,
        Stage::Intersection => 4,
        Stage::Mesh => 5,
        Stage::Object => 6,
        Stage::Tile => 7,
    }
}

fn stage_of_tag(tag: u8) -> Option<Stage> {
    Some(match tag {
        0 => Stage::Vertex,
        1 => Stage::Fragment,
        2 => Stage::Kernel,
        3 => Stage::Visible,
        4 => Stage::Intersection,
        5 => Stage::Mesh,
        6 => Stage::Object,
        7 => Stage::Tile,
        _ => return None,
    })
}

fn resource_kind_tag(kind: ResourceKind) -> u8 {
    match kind {
        ResourceKind::Buffer => 0,
        ResourceKind::Texture => 1,
        ResourceKind::Sampler => 2,
        ResourceKind::AccelerationStructure => 3,
        ResourceKind::VisibleFunctionTable => 4,
        ResourceKind::IntersectionFunctionTable => 5,
        ResourceKind::ArgumentBuffer => 6,
        ResourceKind::Imageblock => 7,
    }
}

fn resource_kind_of_tag(tag: u8) -> Option<ResourceKind> {
    Some(match tag {
        0 => ResourceKind::Buffer,
        1 => ResourceKind::Texture,
        2 => ResourceKind::Sampler,
        3 => ResourceKind::AccelerationStructure,
        4 => ResourceKind::VisibleFunctionTable,
        5 => ResourceKind::IntersectionFunctionTable,
        6 => ResourceKind::ArgumentBuffer,
        7 => ResourceKind::Imageblock,
        _ => return None,
    })
}

fn access_tag(access: Access) -> u8 {
    match access {
        Access::Sample => 0,
        Access::Read => 1,
        Access::Write => 2,
        Access::ReadWrite => 3,
    }
}

fn access_of_tag(tag: u8) -> Option<Access> {
    Some(match tag {
        0 => Access::Sample,
        1 => Access::Read,
        2 => Access::Write,
        3 => Access::ReadWrite,
        _ => return None,
    })
}
