//! Honouring `__DATA,__interpose`: a guest image that replaces a function every other
//! image calls.

use std::collections::BTreeSet;

use machish::memory::Regions;
use machish::report::LinkReport;

/// Apply every interposition the link's images declare.
///
/// Returns how many bindings moved, which is worth printing once.
pub fn apply<M: Regions>(report: &mut LinkReport, memory: &mut M) -> Result<usize, machish::Error> {
    // Gathered before the loop because the loop mutates `report.images`, and
    // because an image may interpose a function another image also interposes —
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
    // one question that matters about a partial interposition — see [`missed`].
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

/// Bindings that name a symbol some *other* binding had interposed, and were left
/// pointing at the host anyway.
///
/// This is the one failure mode a partial interposition has, and it is silent. An
/// interposed allocator only works if it is the allocator **everywhere**.
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
                "{} in {} still resolves to {:#x}, outside the guest — \
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
