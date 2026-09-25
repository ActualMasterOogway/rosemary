use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use static_init::dynamic;

static LIVE: LazyLock<Mutex<HashMap<&'static str, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[inline]
pub fn enabled() -> bool {
    #[dynamic]
    static ON: bool = std::env::var_os("ROSETTA3_OBJC_CENSUS").is_some_and(|value| value != "0");
    *ON
}

fn adjust(class: &'static crate::class::Class, delta: i64) {
    let name = class.name_str();
    let mut live = LIVE.lock().unwrap_or_else(|error| error.into_inner());
    *live.entry(name).or_insert(0) += delta;
}

/// One instance of `class` was allocated.
pub fn allocated(class: &'static crate::class::Class) {
    if enabled() {
        adjust(class, 1);
        report();
    }
}

/// One instance of `class` was freed.
///
/// # Safety
///
/// `object` must still be readable: its `isa` is what names the class.
pub unsafe fn freed(object: crate::object::Id) {
    if !enabled() {
        return;
    }
    if let Some(class) = unsafe { crate::object::class_of(object) } {
        adjust(class, -1);
    }
}

fn report() {
    static SINCE: Mutex<Option<Instant>> = Mutex::new(None);
    let mut since = SINCE.lock().unwrap_or_else(|error| error.into_inner());
    let start = *since.get_or_insert_with(Instant::now);
    if start.elapsed() < Duration::from_secs(5) {
        return;
    }
    *since = Some(Instant::now());
    let live = LIVE.lock().unwrap_or_else(|error| error.into_inner());
    let mut counts: Vec<_> = live.iter().map(|(name, count)| (*count, *name)).collect();
    counts.sort_unstable_by(|a, b| b.cmp(a));
    let line = counts
        .iter()
        .take(12)
        .map(|(count, name)| format!("{name}={count}"))
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!("rosetta3: objc: live {line}");
}
