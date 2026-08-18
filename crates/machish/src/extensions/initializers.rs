//! `S_MOD_INIT_FUNC_POINTERS` and `S_MOD_TERM_FUNC_POINTERS`.

use crate::error::Result;
use crate::extensions::{Extension, read_words};
use crate::image::MachObject;
use crate::memory::AddressSpace;
use crate::raw::Word;
use crate::report::ImageReport;
use crate::spec::MachSpec;
use crate::spec::constants::POINTER_SIZE;

/// Collects `__mod_init_func` and `__mod_term_func` into the report.
#[derive(Copy, Clone, Debug, Default)]
pub struct InitializersExtension;

impl<S: MachSpec> Extension<S> for InitializersExtension {
    fn name(&self) -> &'static str {
        "initializers"
    }

    fn claims(&self, obj: &MachObject<'_, S>) -> bool {
        obj.sections
            .iter()
            .any(|s| s.is_initializers() || s.is_mod_term())
    }

    fn observe(
        &self,
        obj: &MachObject<'_, S>,
        mem: &dyn AddressSpace,
        slide: Word,
        report: &mut ImageReport,
    ) -> Result<()> {
        report.initializers = collect_initializers(obj, mem, slide)?;
        report.terminators = collect_terminators(obj, mem, slide)?;
        Ok(())
    }
}

/// Every initializer, slid, in section order.
pub fn collect_initializers<S: MachSpec>(
    obj: &MachObject<'_, S>,
    mem: &dyn AddressSpace,
    slide: Word,
) -> Result<alloc::vec::Vec<Word>> {
    let image_base = obj.preferred_base()?;
    let mut out = alloc::vec::Vec::new();

    for sect in obj.sections.iter().filter(|s| s.is_initializers()) {
        if sect.is_mod_init() {
            let count = usize::try_from(sect.entry_count(POINTER_SIZE)).unwrap_or(0);
            out.extend(read_words(mem, sect.addr.wrapping_add(slide), count)?);
            continue;
        }
        // `__init_offsets`: 4-byte offsets from the image base. They are never
        // rebased.
        let count = usize::try_from(sect.entry_count(4)).unwrap_or(0);
        let base = sect.addr.wrapping_add(slide);
        for i in 0..count {
            let mut buf = [0u8; 4];
            mem.read(base.wrapping_add(i as u64 * 4), &mut buf)?;
            let offset = u64::from(u32::from_le_bytes(buf));
            out.push(image_base.wrapping_add(offset).wrapping_add(slide));
        }
    }

    // A null entry is padding a linker left behind, not something to call.
    out.retain(|&p| p != 0);
    Ok(out)
}

/// Every `__mod_term_func` entry, slid, in section order.
///
/// Reported, never run: images live for the namespace's lifetime and are never
/// unloaded, so there is no moment at which calling these would be correct.
///
/// Expect this to be empty for anything built with a current toolchain. Clang
/// has not emitted `__mod_term_func` for years - a `__attribute__((destructor))`
/// function is registered with `__cxa_atexit` by a constructor instead.
pub fn collect_terminators<S: MachSpec>(
    obj: &MachObject<'_, S>,
    mem: &dyn AddressSpace,
    slide: Word,
) -> Result<alloc::vec::Vec<Word>> {
    let mut out = alloc::vec::Vec::new();
    for sect in obj.sections.iter().filter(|s| s.is_mod_term()) {
        let count = usize::try_from(sect.entry_count(POINTER_SIZE)).unwrap_or(0);
        // Read from memory, where the rebase pass has already slid the stored
        // pointers; nothing further is added. `slide` locates the section,
        // not its contents.
        out.extend(read_words(mem, sect.addr.wrapping_add(slide), count)?);
    }
    out.retain(|&p| p != 0);
    Ok(out)
}
