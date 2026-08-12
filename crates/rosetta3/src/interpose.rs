//! Honouring `__DATA,__interpose`: a guest image that replaces a function every
//! other image calls.
//!
//! [`machish::extensions::interpose`] decodes the section into
//! `{ replacement, replacee }` pairs and stops. This applies them, which for
//! this runtime is a specific and slightly unusual act: the bindings being
//! rewritten currently hold **host** function pointers, and the replacement is
//! **guest** code. An interposed import therefore stops being a call out of the
//! guest and becomes a call from one part of the guest to another — which is
//! already a case the runtime knows
//! ([`Vm::call_site`](crate::vm::Vm) transfers rather than marshals, and
//! [`ImportPolicy`](crate::program::ImportPolicy) refuses to inline a guest
//! address) so nothing downstream needs to change.
//!
//! ## Why a loader cannot skip this
//!
//! Roblox ships mimalloc as an interposing dylib and imports twelve `mi_*`
//! entry points from it directly. Its own allocations go to `mi_malloc`; the
//! matching deallocations are written as plain `free`. That is only correct
//! because, natively, `free` *is* `mi_free` before a line of the program runs.
//! Ignore the section and the program links, starts, runs for a minute and then
//! aborts inside the system allocator on a pointer it never handed out —
//! `POINTER_BEING_FREED_WAS_NOT_ALLOCATED`, with no symbol in the report naming
//! anything to do with allocation.
//!
//! ## The rules, and which are dyld's
//!
//! - **The interposing image is exempt.** Its own imports keep resolving to
//!   whatever they resolved to, or `mi_free` would call itself.
//! - **The match is on the resolved address, not the symbol.** That is dyld's
//!   rule and it is the stronger one: it catches an import of an *alias* of the
//!   replaced function, which a name comparison would miss, and it cannot
//!   over-reach onto a same-named symbol from a different library, which a name
//!   comparison would hit.
//! - **Launch-time images only.** dyld ignores interposition from anything
//!   `dlopen`ed, because by then the bindings it would have rewritten have
//!   already been used, and a program whose first half frees with one allocator
//!   and second half with another is worse off than one that never interposed.
//!   This is called from the initial link for the same reason.

use std::collections::BTreeSet;

use machish::memory::Regions;
use machish::report::LinkReport;

/// Apply every interposition the link's images declare.
///
/// Returns how many bindings moved, which is worth printing once: an
/// interposing image that rewrites *nothing* is either an image whose replacee
/// resolved differently everywhere or a bug here, and both look identical from
/// the outside.
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
                // that answered was the host's. Left in place it would select a
                // host signature for a call that is now guest-to-guest — see
                // the same clearing in [`crate::cxxlib`].
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

/// Bindings that name a symbol some *other* binding had interposed, and were
/// left pointing at the host anyway.
///
/// This is the one failure mode a partial interposition has, and it is silent.
/// An interposed allocator only works if it is the allocator **everywhere**: if
/// one image's `_malloc` moves to `mi_malloc` and another's does not, the
/// program has two heaps, pointers cross between them, and what it looks like
/// is not a crash in either allocator but an object that was constructed
/// somewhere other than where it is read.
///
/// It can happen for a reason that has nothing to do with this pass: the match
/// is on the resolved address, and two images' imports of the same name resolve
/// to different addresses whenever one of them was answered by a shim and the
/// other by native passthrough. Nothing here can repair that — the right answer
/// depends on why the shim exists — so it is reported rather than fixed.
///
/// **The interposing images are excluded**, and not as a convenience: their own
/// imports pointing at the host is the exemption working. `mi_free` reaches the
/// real `free` through exactly such a binding, which is how mimalloc hands back
/// a pointer that predates it. Including them made this fire on every correct
/// run.
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
///
/// Separate from [`apply`] because the driver prints and the linker does not,
/// and because this is the view that answers "did it take?" — a program whose
/// allocator is being replaced under it should say so somewhere.
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
