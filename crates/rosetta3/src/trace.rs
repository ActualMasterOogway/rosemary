//! `ROSETTA3_TRACE_CALL`: reporting an intercepted call by name.

use microlift::arch::x86_64::context::RegisterContext;
use static_init::dynamic;

/// Whether `ROSETTA3_TRACE_CALL` asked for this symbol.
pub fn watching(symbol: &str) -> bool {
    #[dynamic]
    static WANTED: Vec<String> = wanted(std::env::var("ROSETTA3_TRACE_CALL").ok().as_deref());
    matches(&WANTED, symbol)
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

/// Whether `symbol` - as the Mach-O spells it - is one of `wanted`.
fn matches(wanted: &[String], symbol: &str) -> bool {
    !wanted.is_empty() && wanted.iter().any(|w| w == symbol.trim_start_matches('_'))
}

/// Report the call about to be serviced.
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
fn thread_id() -> u32 {
    darwin::pthread::id()
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
        // `open` must not draw in `opendir` - a trace that watches more than it
        // was asked to is a trace nobody can read.
        let list = wanted(Some("open"));
        assert!(!matches(&list, "_opendir"));
        assert!(!matches(&list, "_openat"));
    }
}
