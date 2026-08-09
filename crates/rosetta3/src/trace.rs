//! `ROSETTA3_TRACE_CALL`: reporting an intercepted call by name.
//!
//! The C-side twin of `ROSETTA3_TRACE_MSG` (see [`crate::objc::dispatch`]), and
//! it exists for the same question in the other language: *which* call did the
//! guest make, with what arguments, on which thread, and what did it get back.
//!
//! **A list of symbols rather than a switch**, for the reason the message trace
//! gives: a program makes millions of calls and the interesting ones are a
//! handful of names known in advance. The case this was built for was
//! Subnautica read as a lost wakeup — every thread parked in
//! `dispatch_semaphore_wait` and nobody apparently signalling — and what the
//! trace actually established is that waits and signals *balance*, on all
//! twelve of the guest's semaphores, so the reading was wrong. Tracing
//! everything would have answered that too, at a hundred megabytes a second,
//! on a run slow enough that the timing-dependent failure stops happening.
//!
//! ```text
//! ROSETTA3_TRACE_CALL=dispatch_semaphore_wait,dispatch_semaphore_signal
//! ```
//!
//! A name matches with or without the Mach-O's leading underscore, because the
//! symbol the runtime holds is `_dispatch_semaphore_wait` and the name anyone
//! types is the C one.
//!
//! **Where the output goes is the guest's business, not this runtime's.** These
//! are `eprintln!`s on fd 2, and a guest may have reopened fd 2 by the time any
//! of them run: a Unity player points it at `Player.log` during start-up, so a
//! traced run of one prints to that file and *nothing* to the terminal. An
//! empty terminal is not an empty trace.
//!
//! ## Why this reaches into the lifter
//!
//! An import with a known signature is *inlined* by microlift — that is what
//! [`crate::ImportPolicy::FastWhenKnown`] means — and an inlined call never
//! reaches [`crate::abi::call_import_with`], which is where the trace is. A
//! first version of this printed nothing at all for exactly the symbols worth
//! watching, since a symbol boring enough to have a table entry is a symbol
//! boring enough to promote. So a traced symbol is **not promoted**: see
//! [`ImportPolicy::promotion_for`](crate::ImportPolicy). The cost is one exit
//! per call, on the calls the user asked to see.

use microlift::arch::x86_64::context::RegisterContext;

/// Whether `ROSETTA3_TRACE_CALL` asked for this symbol.
///
/// Read once. The variable is the user's statement about the whole run, and a
/// lift decision made at minute one has to agree with a trace decision made at
/// minute ten — see the note above on promotion.
pub fn watching(symbol: &str) -> bool {
    static WANTED: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let wanted =
        WANTED.get_or_init(|| wanted(std::env::var("ROSETTA3_TRACE_CALL").ok().as_deref()));
    matches(wanted, symbol)
}

/// The symbol list `ROSETTA3_TRACE_CALL`'s value names, C names without the
/// Mach-O's leading underscore.
fn wanted(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().trim_start_matches('_').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whether `symbol` — as the Mach-O spells it — is one of `wanted`.
fn matches(wanted: &[String], symbol: &str) -> bool {
    !wanted.is_empty() && wanted.iter().any(|w| w == symbol.trim_start_matches('_'))
}

/// Report the call about to be serviced.
///
/// The first four integer arguments, unconditionally, without consulting the
/// symbol's signature: this runs *before* the call, so the alternative is
/// looking a signature up in order to print fewer registers, and a call being
/// traced is one whose signature is often the thing in doubt. A register that
/// is not an argument prints as whatever it held, which is what a register
/// dump means everywhere else.
pub fn entering(symbol: &str, ctx: &RegisterContext) {
    eprintln!(
        "rosetta3: call [{:#x}] {symbol}({:#x}, {:#x}, {:#x}, {:#x})",
        thread_id(),
        ctx.rdi,
        ctx.rsi,
        ctx.rdx,
        ctx.rcx,
    );
}

/// Report what the call returned, or that it did not.
///
/// A refusal is printed too. Without it a traced symbol that has no signature
/// looks from the log exactly like one that was called and returned nothing —
/// and "the call never happened" is the more interesting of the two.
pub fn leaving<T>(symbol: &str, ctx: &RegisterContext, result: &Result<T, crate::abi::AbiError>) {
    match result {
        Ok(_) => eprintln!(
            "rosetta3: call [{:#x}] {symbol} -> {:#x}",
            thread_id(),
            ctx.rax
        ),
        Err(e) => eprintln!("rosetta3: call [{:#x}] {symbol} !! {e}", thread_id()),
    }
}

/// This thread, as a number small enough to read across a log.
///
/// The Mach port name rather than `pthread_self`, so a line here and a thread
/// named by [`crate::thread`]'s registry — which keys on the same port — are
/// the same thread without a translation step.
fn thread_id() -> u32 {
    // SAFETY: `pthread_self` is always valid on a running thread and
    // `pthread_mach_thread_np` reads the port name out of it.
    unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) }
}

#[cfg(test)]
mod tests {
    use super::{matches, wanted};

    #[test]
    fn unset_watches_nothing() {
        assert!(!matches(&wanted(None), "_dispatch_semaphore_wait"));
        assert!(!matches(&wanted(Some("")), "_dispatch_semaphore_wait"));
    }

    #[test]
    fn a_name_matches_with_or_without_the_underscore() {
        let list = wanted(Some("dispatch_semaphore_wait, _open"));
        assert!(matches(&list, "_dispatch_semaphore_wait"));
        assert!(matches(&list, "_open"));
        assert!(matches(&list, "open"));
    }

    #[test]
    fn a_prefix_is_not_a_match() {
        // `open` must not draw in `opendir` — a trace that watches more than it
        // was asked to is a trace nobody can read.
        let list = wanted(Some("open"));
        assert!(!matches(&list, "_opendir"));
        assert!(!matches(&list, "_openat"));
    }
}
