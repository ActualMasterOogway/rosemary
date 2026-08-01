//! The `LC_DYLD_INFO(_ONLY)` opcode virtual machines.
//!
//! Four independent bytecode streams — `rebase`, `bind`, `weak_bind`,
//! `lazy_bind` — each a program for a tiny stack-free machine whose registers
//! are `(segment index, segment offset, library ordinal, symbol name, type,
//! addend)`. The instruction set is a run-length encoder: `DO_BIND` emits one
//! fixup at the current address, and the `..._ULEB_TIMES_SKIPPING_ULEB` forms
//! emit a strided run, which is how a table of a hundred pointer slots
//! compresses to about four bytes.
//!
//! The three bind streams share one machine because they *are* one machine with
//! one difference: what a `DO_BIND` means. `bind` produces load-time bindings,
//! `weak_bind` produces coalescing candidates, and `lazy_bind` produces what
//! dyld would have bound on first call. We bind all three eagerly, so the
//! distinction survives only as [`BindKind`], which is what the embedder needs
//! to decide how to intercept the call site.
//!
//! ## Where `lazy_bind` differs in shape
//!
//! `lazy_bind` is not one program but a concatenation of independent ones, each
//! entered at a byte offset baked into a `__stub_helper` entry. So it resets
//! its registers at every `DONE` rather than stopping — a machine that halted
//! at the first `DONE`, as the other three streams do, would decode exactly one
//! of `/bin/ls`'s eighty-four lazy bindings and report success.

use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::fixups::{BindKind, Fixup, FixupDecoder, LibraryRef, SymbolRef};
use crate::image::MachObject;
use crate::raw::{Cursor, View, Word};
use crate::spec::MachSpec;
use crate::spec::constants as c;

/// Ceiling on fixups decoded from one image. A run-length opcode takes a
/// `uleb128` count, so a two-byte program can ask for 2^64 fixups; without a
/// cap that is an out-of-memory abort rather than a parse error.
const MAX_FIXUPS: usize = 1 << 24;

/// The `LC_DYLD_INFO(_ONLY)` decoder.
#[derive(Copy, Clone, Debug, Default)]
pub struct DyldInfoFixups;

impl<S: MachSpec> FixupDecoder<S> for DyldInfoFixups {
    fn name(&self) -> &'static str {
        "dyld_info"
    }

    fn claims(&self, obj: &MachObject<'_, S>) -> bool {
        obj.dyld_info().is_some_and(|d| {
            d.rebase_size != 0 || d.bind_size != 0 || d.weak_bind_size != 0 || d.lazy_bind_size != 0
        })
    }

    fn decode<'a>(&self, obj: &MachObject<'a, S>, out: &mut Vec<Fixup<'a>>) -> Result<()> {
        let Some(info) = obj.dyld_info() else {
            return Ok(());
        };

        if let Some(stream) = substream(obj, info.rebase_off, info.rebase_size)? {
            run_rebase(obj, stream, out)?;
        }
        for (off, size, kind) in [
            (info.bind_off, info.bind_size, BindKind::NonLazy),
            (
                info.weak_bind_off,
                info.weak_bind_size,
                BindKind::WeakCoalesce,
            ),
            (info.lazy_bind_off, info.lazy_bind_size, BindKind::Lazy),
        ] {
            if let Some(stream) = substream(obj, off, size)? {
                run_bind(obj, stream, kind, out)?;
            }
        }
        Ok(())
    }
}

/// A sub-view of the image limited to one stream's declared extent.
///
/// Narrowing the view rather than passing offsets is what makes an opcode's
/// operand physically unable to read another stream's bytes: a runaway ULEB
/// hits the end of *this* stream and errors.
fn substream<'a, S: MachSpec>(
    obj: &MachObject<'a, S>,
    off: u32,
    size: u32,
) -> Result<Option<View<'a>>> {
    if size == 0 {
        return Ok(None);
    }
    Ok(Some(obj.view().subview(
        off as usize,
        size as usize,
        "dyld_info stream",
    )?))
}

/// The register file both machines share.
struct Machine<'a, 'o, S: MachSpec> {
    obj: &'o MachObject<'a, S>,
    seg_index: usize,
    seg_offset: u64,
    /// `false` until a `SET_SEGMENT_AND_OFFSET` has run. A stream that emits a
    /// fixup before setting an address is malformed, and defaulting to segment
    /// zero would silently write into `__PAGEZERO`'s address range.
    positioned: bool,
}

impl<'a, S: MachSpec> Machine<'a, '_, S> {
    /// The unslid VA the machine currently points at, validated to hold a
    /// whole pointer inside the segment.
    fn address(&self) -> Result<Word> {
        if !self.positioned {
            return Err(Error::UnknownFixup {
                format: "dyld_info",
                value: 0,
            });
        }
        let seg = self
            .obj
            .segments
            .get(self.seg_index)
            .ok_or(Error::BadLoadCommand {
                why: "fixup segment index out of range",
            })?;
        let end = self
            .seg_offset
            .checked_add(c::POINTER_SIZE)
            .ok_or(Error::OutOfBounds {
                addr: self.seg_offset,
                len: 8,
            })?;
        if end > seg.vmsize {
            return Err(Error::OutOfBounds {
                addr: seg.vmaddr.wrapping_add(self.seg_offset),
                len: 8,
            });
        }
        Ok(seg.vmaddr.wrapping_add(self.seg_offset))
    }

    fn set_segment(&mut self, index: usize, offset: u64) {
        self.seg_index = index;
        self.seg_offset = offset;
        self.positioned = true;
    }

    /// Move the cursor by `by` bytes, **wrapping**.
    ///
    /// `ADD_ADDR_ULEB` is spelled unsigned but is used signed: `ld` emits a
    /// backwards step as a ULEB of the two's-complement value, so a jump back
    /// 232 bytes arrives here as `0xffff_ffff_ffff_ff18`. dyld adds it into a
    /// `uintptr_t` and lets it wrap, and images in the wild depend on that —
    /// DELTARUNE's runner binds a `__DATA` slot below the one before it dozens
    /// of times. Rejecting the overflow refused the whole image before it ran.
    ///
    /// Wrapping is safe because it is not what makes the address legal:
    /// [`Self::address`] re-checks segment bounds at every fixup, so a step that
    /// wraps past zero (or a genuinely runaway one) lands out of range and
    /// errors there, with the offending address in the diagnostic.
    fn advance(&mut self, by: u64) {
        self.seg_offset = self.seg_offset.wrapping_add(by);
    }
}

fn push<'a>(out: &mut Vec<Fixup<'a>>, fixup: Fixup<'a>) -> Result<()> {
    if out.len() >= MAX_FIXUPS {
        return Err(Error::TooDeep("fixup count"));
    }
    out.push(fixup);
    Ok(())
}

/// The rebase machine: slide every pointer whose address it walks over.
fn run_rebase<'a, S: MachSpec>(
    obj: &MachObject<'a, S>,
    stream: View<'a>,
    out: &mut Vec<Fixup<'a>>,
) -> Result<()> {
    let mut m = Machine {
        obj,
        seg_index: 0,
        seg_offset: 0,
        positioned: false,
    };
    let mut cur = Cursor::new(stream);

    // Emit `count` pointer-sized rebases, advancing by `8 + skip` each time.
    macro_rules! emit {
        ($count:expr, $skip:expr) => {{
            let (count, skip): (u64, u64) = ($count, $skip);
            for _ in 0..count {
                push(out, Fixup::Rebase { at: m.address()? })?;
                m.advance(c::POINTER_SIZE.wrapping_add(skip));
            }
        }};
    }

    while !cur.at_end() {
        let byte = cur.u8()?;
        let opcode = byte & c::REBASE_OPCODE_MASK;
        let imm = byte & c::REBASE_IMMEDIATE_MASK;

        match opcode {
            c::REBASE_OPCODE_DONE => break,

            c::REBASE_OPCODE_SET_TYPE_IMM => {
                // A 64-bit image only ever rebases pointers. The 32-bit
                // `TEXT_ABSOLUTE32`/`TEXT_PCREL32` types would need us to patch
                // instruction immediates, which for a guest image being handed
                // to a recompiler is a different operation entirely — better to
                // refuse than to write eight bytes over four bytes of code.
                if imm != c::REBASE_TYPE_POINTER {
                    return Err(Error::UnknownFixup {
                        format: "rebase type",
                        value: u32::from(imm),
                    });
                }
            }

            c::REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB => {
                let offset = cur.uleb128()?;
                m.set_segment(usize::from(imm), offset);
            }

            c::REBASE_OPCODE_ADD_ADDR_ULEB => {
                let by = cur.uleb128()?;
                m.advance(by);
            }

            c::REBASE_OPCODE_ADD_ADDR_IMM_SCALED => {
                m.advance(u64::from(imm) * c::POINTER_SIZE);
            }

            c::REBASE_OPCODE_DO_REBASE_IMM_TIMES => emit!(u64::from(imm), 0),
            c::REBASE_OPCODE_DO_REBASE_ULEB_TIMES => {
                let count = cur.uleb128()?;
                emit!(count, 0);
            }
            c::REBASE_OPCODE_DO_REBASE_ADD_ADDR_ULEB => {
                let skip = cur.uleb128()?;
                emit!(1, skip);
            }
            c::REBASE_OPCODE_DO_REBASE_ULEB_TIMES_SKIPPING_ULEB => {
                let count = cur.uleb128()?;
                let skip = cur.uleb128()?;
                emit!(count, skip);
            }

            other => {
                return Err(Error::UnknownFixup {
                    format: "rebase opcode",
                    value: u32::from(other),
                });
            }
        }
    }
    Ok(())
}

/// The bind machine, shared by all three bind streams.
fn run_bind<'a, S: MachSpec>(
    obj: &MachObject<'a, S>,
    stream: View<'a>,
    kind: BindKind,
    out: &mut Vec<Fixup<'a>>,
) -> Result<()> {
    let lazy = kind == BindKind::Lazy;
    let mut m = Machine {
        obj,
        seg_index: 0,
        seg_offset: 0,
        positioned: false,
    };
    let mut cur = Cursor::new(stream);

    let mut ordinal: i32 = 0;
    let mut name: &'a str = "";
    let mut named = false;
    let mut weak_import = false;
    let mut addend: i64 = 0;

    macro_rules! emit {
        ($count:expr, $skip:expr) => {{
            let (count, skip): (u64, u64) = ($count, $skip);
            if !named {
                return Err(Error::UnknownFixup {
                    format: "bind without a symbol",
                    value: 0,
                });
            }
            let symbol = SymbolRef {
                name,
                library: library_ref(obj, ordinal),
                weak_import,
            };
            for _ in 0..count {
                push(
                    out,
                    Fixup::Bind {
                        at: m.address()?,
                        symbol,
                        addend,
                        kind,
                    },
                )?;
                m.advance(c::POINTER_SIZE.wrapping_add(skip));
            }
        }};
    }

    while !cur.at_end() {
        let byte = cur.u8()?;
        let opcode = byte & c::BIND_OPCODE_MASK;
        let imm = byte & c::BIND_IMMEDIATE_MASK;

        match opcode {
            c::BIND_OPCODE_DONE => {
                if !lazy {
                    break;
                }
                // In `lazy_bind`, `DONE` terminates one stub's private program
                // and the next one starts immediately after. Reset rather than
                // stop; see the module note.
                ordinal = 0;
                name = "";
                named = false;
                weak_import = false;
                addend = 0;
                m.positioned = false;
            }

            c::BIND_OPCODE_SET_DYLIB_ORDINAL_IMM => ordinal = i32::from(imm),
            c::BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB => {
                let v = cur.uleb128()?;
                ordinal = i32::try_from(v).map_err(|_| Error::BadLibraryOrdinal(-1))?;
            }
            c::BIND_OPCODE_SET_DYLIB_SPECIAL_IMM => {
                // The special ordinals are negative, encoded as a sign-extended
                // 4-bit field: immediate 0xf means -1, not 15.
                ordinal = if imm == 0 {
                    0
                } else {
                    (imm | 0xf0) as i8 as i32
                };
            }

            c::BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM => {
                weak_import = imm & c::BIND_SYMBOL_FLAGS_WEAK_IMPORT != 0;
                name = cur.cstr()?;
                named = true;
            }

            c::BIND_OPCODE_SET_TYPE_IMM => {
                if imm != c::BIND_TYPE_POINTER {
                    return Err(Error::UnknownFixup {
                        format: "bind type",
                        value: u32::from(imm),
                    });
                }
            }

            c::BIND_OPCODE_SET_ADDEND_SLEB => addend = cur.sleb128()?,

            c::BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB => {
                let offset = cur.uleb128()?;
                m.set_segment(usize::from(imm), offset);
            }

            c::BIND_OPCODE_ADD_ADDR_ULEB => {
                let by = cur.uleb128()?;
                m.advance(by);
            }

            c::BIND_OPCODE_DO_BIND => emit!(1, 0),
            c::BIND_OPCODE_DO_BIND_ADD_ADDR_ULEB => {
                let skip = cur.uleb128()?;
                emit!(1, skip);
            }
            c::BIND_OPCODE_DO_BIND_ADD_ADDR_IMM_SCALED => {
                emit!(1, u64::from(imm) * c::POINTER_SIZE);
            }
            c::BIND_OPCODE_DO_BIND_ULEB_TIMES_SKIPPING_ULEB => {
                let count = cur.uleb128()?;
                let skip = cur.uleb128()?;
                emit!(count, skip);
            }

            c::BIND_OPCODE_THREADED => {
                // Threaded rebase/bind is the arm64e encoding, and arm64e means
                // pointer authentication, which is out of scope. Reject it by
                // name so the diagnostic says what is actually unsupported.
                return Err(Error::UnknownFixup {
                    format: "threaded bind (arm64e)",
                    value: u32::from(byte),
                });
            }

            other => {
                return Err(Error::UnknownFixup {
                    format: "bind opcode",
                    value: u32::from(other),
                });
            }
        }
    }
    Ok(())
}

/// Turn a bind ordinal into the library reference it names.
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
        // Any other negative value is a special ordinal from a future dyld.
        other => LibraryRef::UnknownOrdinal(other as u16),
    }
}
