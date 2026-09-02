//! Honouring `__DATA,__interpose`: a guest image that replaces a function every other
//! image calls.

use std::collections::BTreeSet;

use machish::memory::Regions;
use machish::report::LinkReport;

/// Set to skip every interposition, leaving each image bound to the host's own.
///
/// A debugging lever, and the one worth reaching for first when a program
/// corrupts its heap. The interposition a guest declares is nearly always an
/// allocator replacing `malloc`/`free`, and a replacement allocator is silent
/// about a double free where glibc's own aborts with a sentence naming it - so
/// turning the replacement off does not fix the bug, it moves the report from
/// "a corrupted free list, thousands of allocations later" to "free(): invalid
/// pointer, here".
///
/// Not safe in general: an image that interposes for a reason other than
/// allocation gets its replacement dropped.
pub const NO_INTERPOSE_ENV: &str = "ROSETTA3_NO_INTERPOSE";

/// Set to print every binding an interposition moves.
///
/// The question it answers is "did the replacement allocator reach *this*
/// image": a program with two allocators is one where some image's `free` was
/// missed, and [`missed`] can only see the ones that stayed pointing at the
/// host under a symbol something else moved.
pub const TRACE_ENV: &str = "ROSETTA3_TRACE_INTERPOSE";

fn tracing() -> bool {
    std::env::var_os(TRACE_ENV).is_some()
}

fn disabled() -> bool {
    std::env::var_os(NO_INTERPOSE_ENV).is_some_and(|v| v != "0")
}

/// Apply every interposition the link's images declare.
pub fn apply<M: Regions>(report: &mut LinkReport, memory: &mut M) -> Result<usize, machish::Error> {
    if disabled() {
        eprintln!(
            "rosetta3: interpose: every interposition skipped ({NO_INTERPOSE_ENV} is set) - the \
             program is using the host's allocator throughout"
        );
        return Ok(0);
    }
    // Gathered before the loop because the loop mutates `report.images`, and
    // because an image may interpose a function another image also interposes -
    // in which case dyld applies both, in load order, and so does this.
    let requests: Vec<(u64, machish::report::Interposition)> = report
        .images
        .iter()
        .flat_map(|i| i.interpositions.iter().map(move |p| (i.base, *p)))
        .collect();
    if requests.is_empty() {
        return Ok(0);
    }

    let mut rebound = 0;
    // Which symbols actually moved. Kept so the split check below can ask the
    // one question that matters about a partial interposition - see [`missed`].
    let mut moved: BTreeSet<String> = BTreeSet::new();
    for (from, pair) in requests {
        for image in &mut report.images {
            if image.base == from {
                continue;
            }
            for binding in &mut image.bindings {
                if binding.resolved != pair.replacee {
                    continue;
                }
                if tracing() {
                    eprintln!(
                        "rosetta3: interpose: {} in {} -> {:#x}",
                        binding.symbol,
                        image.name.rsplit('/').next().unwrap_or(&image.name),
                        pair.replacement
                    );
                }
                memory.write(binding.slot_va, &pair.replacement.to_le_bytes())?;
                binding.resolved = pair.replacement;
                // The tag names the *provider* that answered, and the provider
                // that answered was the host's.
                binding.tag = 0;
                moved.insert(binding.symbol.clone());
                rebound += 1;
            }
        }
    }
    let interposers: BTreeSet<u64> = report
        .images
        .iter()
        .filter(|i| !i.interpositions.is_empty())
        .map(|i| i.base)
        .collect();
    for line in missed(report, &moved, &interposers) {
        eprintln!("rosetta3: interpose: {line}");
    }
    Ok(rebound)
}


/// The `malloc`/`free` the guest's own images call, once every interposition
/// has been applied.
///
/// Not the same pair this process uses. A guest that ships an allocator has
/// replaced them for its own images, and the frameworks in this tree are *not*
/// among those images - they are host code, linked against the host's libc
/// before the guest existed. So a block one of them hands the guest to `free`
/// is freed by an allocator that never made it; see
/// [`objc::allocator`] for what that looks like when it goes wrong.
///
/// Read out of the bindings rather than by name, because the answer wanted is
/// "what does the guest's code actually call", which is exactly what a binding
/// records - after [`apply`] has had its say.
pub fn guest_allocator(report: &LinkReport) -> Option<(u64, u64)> {
    let resolved = |name: &str| {
        report
            .images
            .iter()
            .flat_map(|image| &image.bindings)
            .find(|binding| darwin::base_name(&binding.symbol) == name && binding.resolved != 0)
            .map(|binding| binding.resolved)
    };
    Some((resolved("malloc")?, resolved("free")?))
}

/// Point this tree's frameworks at the guest's allocator, so a block they hand
/// it can be freed by the allocator that made it.
///
/// The **native backend only**. Under emulation the guest's allocator is
/// x86-64 this process cannot enter without the run loop, so calling it from a
/// framework would be a jump into unlifted bytes; nothing is registered there
/// and the host's own is used, which is what every other host call in that
/// backend already does.
pub fn adopt_guest_allocator(report: &LinkReport) {
    let Some((malloc, free)) = guest_allocator(report) else {
        return;
    };
    objc::allocator::register(malloc as usize, free as usize);
    darwin::heap::register(malloc as usize, free as usize);
    if tracing() {
        eprintln!("rosetta3: interpose: guest allocator malloc {malloc:#x}, free {free:#x}");
    }
}

/// Bindings that name a symbol some *other* binding had interposed, and were left
/// pointing at the host anyway.
fn missed(
    report: &LinkReport,
    moved: &BTreeSet<String>,
    interposers: &BTreeSet<u64>,
) -> Vec<String> {
    let guest: Vec<(u64, u64)> = report
        .images
        .iter()
        .flat_map(|i| &i.segments)
        .map(|s| (s.vmaddr, s.vmaddr.wrapping_add(s.vmsize)))
        .collect();
    let in_guest = |addr: u64| guest.iter().any(|(lo, hi)| addr >= *lo && addr < *hi);

    let mut out = Vec::new();
    for image in &report.images {
        if interposers.contains(&image.base) {
            continue;
        }
        for binding in &image.bindings {
            if !moved.contains(&binding.symbol) || in_guest(binding.resolved) {
                continue;
            }
            out.push(format!(
                "{} in {} still resolves to {:#x}, outside the guest - \
                 this program has two allocators",
                binding.symbol,
                image.name.rsplit('/').next().unwrap_or(&image.name),
                binding.resolved
            ));
        }
    }
    out
}

/// One line per interposing image, for the inspect transcript.
pub fn describe(report: &LinkReport) -> Vec<String> {
    report
        .images
        .iter()
        .filter(|i| !i.interpositions.is_empty())
        .map(|i| {
            format!(
                "{} interposes {} function(s)",
                i.name,
                i.interpositions.len()
            )
        })
        .collect()
}
