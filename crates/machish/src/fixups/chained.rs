use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::fixups::{BindKind, Fixup, FixupDecoder, LibraryRef, SymbolRef};
use crate::image::MachObject;
use crate::raw::{View, Word};
use crate::spec::MachSpec;
use crate::spec::constants as c;

/// Cap corrupt chains well above the 4096 links possible in a 16 KiB page.
const MAX_CHAIN_LINKS: usize = 1 << 16;

/// Ceiling on the import table, which is `uint32`-counted on disk.
const MAX_IMPORTS: usize = 1 << 22;

/// The `LC_DYLD_CHAINED_FIXUPS` decoder.
#[derive(Copy, Clone, Debug, Default)]
pub struct ChainedFixups;

impl<S: MachSpec> FixupDecoder<S> for ChainedFixups {
    fn name(&self) -> &'static str {
        "chained"
    }

    fn claims(&self, obj: &MachObject<'_, S>) -> bool {
        obj.chained_fixups().is_some_and(|d| d.datasize != 0)
    }

    fn decode<'a>(&self, obj: &MachObject<'a, S>, out: &mut Vec<Fixup<'a>>) -> Result<()> {
        let Some(data) = obj.chained_fixups() else {
            return Ok(());
        };
        let Some(range) = data.range() else {
            return Ok(());
        };

        // Validate offsets against the fixups command, not the entire file.
        let header = obj
            .view()
            .subview(range.start, range.end - range.start, "chained fixups")?;

        let starts_offset = header.u32(4)? as usize;
        let imports_offset = header.u32(8)? as usize;
        let symbols_offset = header.u32(12)? as usize;
        let imports_count = header.u32(16)? as usize;
        let imports_format = header.u32(20)?;

        if imports_count > MAX_IMPORTS {
            return Err(Error::TooDeep("chained import count"));
        }
        let imports = ImportTable {
            header,
            offset: imports_offset,
            symbols: symbols_offset,
            count: imports_count,
            format: imports_format,
        };
        imports.validate()?;

        decode_starts(obj, &header, starts_offset, &imports, out)
    }
}

/// Walk `dyld_chained_starts_in_image` and each segment's chains.
fn decode_starts<'a, S: MachSpec>(
    obj: &MachObject<'a, S>,
    header: &View<'a>,
    starts_offset: usize,
    imports: &ImportTable<'a>,
    out: &mut Vec<Fixup<'a>>,
) -> Result<()> {
    let seg_count = header.u32(starts_offset)? as usize;
    if seg_count > obj.segments.len() {
        return Err(Error::BadLoadCommand {
            why: "chained starts segment count",
        });
    }

    for i in 0..seg_count {
        let seg_info = header.u32(starts_offset + 4 + i * 4)? as usize;
        // Zero is the "this segment has no fixups" sentinel, which is how
        // `__TEXT` and `__LINKEDIT` are spelled.
        if seg_info == 0 {
            continue;
        }
        let at = starts_offset
            .checked_add(seg_info)
            .ok_or(Error::Truncated {
                what: "chained starts_in_segment",
            })?;
        decode_segment(obj, header, at, i, imports, out)?;
    }
    Ok(())
}

/// Walk one `dyld_chained_starts_in_segment`.
fn decode_segment<'a, S: MachSpec>(
    obj: &MachObject<'a, S>,
    header: &View<'a>,
    at: usize,
    seg_index: usize,
    imports: &ImportTable<'a>,
    out: &mut Vec<Fixup<'a>>,
) -> Result<()> {
    // `page_count` is at +20, after the four-byte `max_valid_pointer`.
    let page_size = u64::from(header.u16(at + 4)?);
    let pointer_format = header.u16(at + 6)?;
    let segment_offset = header.u64(at + 8)?;
    let page_count = header.u16(at + 20)? as usize;

    if page_size == 0 || !page_size.is_power_of_two() {
        return Err(Error::BadLoadCommand {
            why: "chained page size",
        });
    }
    if c::is_arm64e_pointer_format(pointer_format) {
        // The high bits carry a PAC signature, not a target address.
        return Err(Error::UnknownFixup {
            format: "arm64e chained pointer (pointer authentication)",
            value: u32::from(pointer_format),
        });
    }
    if !matches!(
        pointer_format,
        c::DYLD_CHAINED_PTR_64 | c::DYLD_CHAINED_PTR_64_OFFSET
    ) {
        return Err(Error::UnknownFixup {
            format: "chained pointer format",
            value: u32::from(pointer_format),
        });
    }

    let seg = obj.segments.get(seg_index).ok_or(Error::BadLoadCommand {
        why: "chained segment index out of range",
    })?;
    // The fixup's image-relative offset takes precedence over segment `vmaddr`.
    let image_base = obj.preferred_base()?;
    let seg_start = image_base.wrapping_add(segment_offset);

    let starts_at = at + 22;
    for page in 0..page_count {
        let start = header.u16(starts_at + page * 2)?;
        if start == c::DYLD_CHAINED_PTR_START_NONE {
            continue;
        }
        let page_base = seg_start.wrapping_add(page as u64 * page_size);

        if start & c::DYLD_CHAINED_PTR_START_MULTI == 0 {
            walk_chain(
                obj,
                page_base + u64::from(start),
                pointer_format,
                imports,
                seg,
                out,
            )?;
            continue;
        }
        // Overflow form: the low bits index further into the same array, where
        // a list of starts for this page runs until one has the LAST bit set.
        let mut idx = (start & !c::DYLD_CHAINED_PTR_START_MULTI) as usize;
        loop {
            let entry = header.u16(starts_at + idx * 2)?;
            let offset = entry & !c::DYLD_CHAINED_PTR_START_LAST;
            walk_chain(
                obj,
                page_base + u64::from(offset),
                pointer_format,
                imports,
                seg,
                out,
            )?;
            if entry & c::DYLD_CHAINED_PTR_START_LAST != 0 {
                break;
            }
            idx += 1;
            if idx >= page_count.saturating_mul(2) + MAX_CHAIN_LINKS {
                return Err(Error::TooDeep("chained page starts"));
            }
        }
    }
    Ok(())
}

/// Follow one chain from `first`, emitting a fixup per link.
fn walk_chain<'a, S: MachSpec>(
    obj: &MachObject<'a, S>,
    first: Word,
    pointer_format: u16,
    imports: &ImportTable<'a>,
    seg: &crate::image::Segment<'a>,
    out: &mut Vec<Fixup<'a>>,
) -> Result<()> {
    let image_base = obj.preferred_base()?;
    let mut at = first;

    for _ in 0..MAX_CHAIN_LINKS {
        if !seg.contains(at) {
            return Err(Error::OutOfBounds { addr: at, len: 8 });
        }
        let off = obj
            .vmaddr_to_off(at)
            .ok_or(Error::OutOfBounds { addr: at, len: 8 })?;
        let raw = obj.view().u64(off)?;

        // Bit 63 selects bind versus rebase in every 64-bit format.
        if raw >> 63 & 1 == 1 {
            let ordinal = (raw & 0x00ff_ffff) as u32;
            // An 8-bit *unsigned* inline addend, distinct from the import
            // table's signed one; the two add.
            let inline_addend = (raw >> 24 & 0xff) as i64;
            let (symbol, table_addend) = imports.get(obj, ordinal)?;
            out.push(Fixup::Bind {
                at,
                symbol,
                addend: table_addend.wrapping_add(inline_addend),
                kind: BindKind::Chained,
            });
        } else {
            let target = raw & 0x0000_000f_ffff_ffff; // 36 bits
            let high8 = raw >> 36 & 0xff;
            let target = match pointer_format {
                // `_64` stores the unslid VA outright.
                c::DYLD_CHAINED_PTR_64 => target,
                // `_64_OFFSET` stores an offset from the image base.
                _ => image_base.wrapping_add(target),
            };
            out.push(Fixup::RebaseTo {
                at,
                target: target | high8 << 56,
            });
        }

        let next = raw >> 51 & 0xfff;
        if next == 0 {
            return Ok(());
        }
        at = at
            .checked_add(next * c::CHAINED_NEXT_STRIDE)
            .ok_or(Error::OutOfBounds { addr: at, len: 8 })?;
    }
    // `next` is always positive here, so a chain cannot literally loop - but it
    // can be made absurdly long, and the segment bounds check above only stops
    // it once it wanders out. Cap it.
    Err(Error::TooDeep("chained fixup chain"))
}

/// The `dyld_chained_import*` table and its string pool.
struct ImportTable<'a> {
    header: View<'a>,
    offset: usize,
    symbols: usize,
    count: usize,
    format: u32,
}

impl<'a> ImportTable<'a> {
    /// Bytes per entry for this format. The three formats differ only in how
    /// much addend and ordinal they can hold.
    fn stride(&self) -> Result<usize> {
        Ok(match self.format {
            c::DYLD_CHAINED_IMPORT => 4,
            c::DYLD_CHAINED_IMPORT_ADDEND => 8,
            c::DYLD_CHAINED_IMPORT_ADDEND64 => 16,
            other => {
                return Err(Error::UnknownFixup {
                    format: "chained imports",
                    value: other,
                });
            }
        })
    }

    /// Check the whole table is inside the command's extent, once, so per-entry
    /// reads do not each have to reason about it.
    fn validate(&self) -> Result<()> {
        if self.count == 0 {
            return Ok(());
        }
        let bytes = self
            .count
            .checked_mul(self.stride()?)
            .ok_or(Error::Truncated {
                what: "chained imports",
            })?;
        self.header.slice(self.offset, bytes, "chained imports")?;
        Ok(())
    }

    /// Entry `index`, as a symbol reference and a signed addend.
    fn get<S: MachSpec>(
        &self,
        obj: &MachObject<'a, S>,
        index: u32,
    ) -> Result<(SymbolRef<'a>, i64)> {
        let index = index as usize;
        if index >= self.count {
            return Err(Error::BadLibraryOrdinal(-1));
        }
        let at = self.offset + index * self.stride()?;

        let (ordinal, weak_import, name_offset, addend) = match self.format {
            c::DYLD_CHAINED_IMPORT => {
                let v = self.header.u32(at)?;
                (
                    i32::from((v & 0xff) as u8 as i8),
                    v >> 8 & 1 != 0,
                    (v >> 9) as usize,
                    0i64,
                )
            }
            c::DYLD_CHAINED_IMPORT_ADDEND => {
                let v = self.header.u32(at)?;
                let addend = self.header.i32(at + 4)?;
                (
                    i32::from((v & 0xff) as u8 as i8),
                    v >> 8 & 1 != 0,
                    (v >> 9) as usize,
                    i64::from(addend),
                )
            }
            _ => {
                let v = self.header.u64(at)?;
                let addend = self.header.u64(at + 8)? as i64;
                (
                    i32::from((v & 0xffff) as u16 as i16),
                    v >> 16 & 1 != 0,
                    (v >> 32) as usize,
                    addend,
                )
            }
        };

        let name =
            self.header.cstr(
                self.symbols
                    .checked_add(name_offset)
                    .ok_or(Error::Truncated {
                        what: "chained import name",
                    })?,
            )?;

        Ok((
            SymbolRef {
                name,
                library: library_ref(obj, ordinal),
                weak_import,
            },
            addend,
        ))
    }
}

/// Turn a chained import's library ordinal into a library reference.
///
/// The special ordinals are the same three the opcode encoding uses, which is
/// the one place the two formats speak a shared vocabulary rather than two
/// spellings of one.
fn library_ref<'a, S: MachSpec>(obj: &MachObject<'a, S>, ordinal: i32) -> LibraryRef<'a> {
    match ordinal {
        c::BIND_SPECIAL_DYLIB_SELF => LibraryRef::SelfImage,
        c::BIND_SPECIAL_DYLIB_MAIN_EXECUTABLE => LibraryRef::MainExecutable,
        c::BIND_SPECIAL_DYLIB_FLAT_LOOKUP => LibraryRef::Flat,
        c::BIND_SPECIAL_DYLIB_WEAK_LOOKUP => LibraryRef::WeakLookup,
        n if n > 0 => {
            let n = u16::try_from(n).unwrap_or(u16::MAX);
            match obj.dependency_by_ordinal(n) {
                Some(dep) => LibraryRef::InstallName(dep.install_name),
                None => LibraryRef::UnknownOrdinal(n),
            }
        }
        other => LibraryRef::UnknownOrdinal(other as u16),
    }
}
