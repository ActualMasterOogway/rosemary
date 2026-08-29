pub mod application;
pub mod classes;
mod define;
pub mod nib;

pub fn bootstrap() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        foundation::bootstrap();
        classes::install();
        application::install();
    });
}

macro_rules! at {
    ($f:expr) => {
        $f as *const () as usize as u64
    };
}

pub fn symbol(name: &str) -> Option<u64> {
    bootstrap();
    Some(match name {
        "NSApplicationMain" => at!(application::NSApplicationMain),
        _ => return None,
    })
}

pub const NAMES: &[&str] = &["NSApplicationMain"];

pub fn ns_app_cell() -> u64 {
    corefoundation::base::global_cell("_NSApp", 0)
}

pub fn data_symbols() -> Vec<(String, u64)> {
    bootstrap();
    let mut out = classes::class_data();
    out.push(("_NSApp".to_string(), ns_app_cell()));
    for name in NOTIFICATION_NAMES {
        let symbol = format!("_{name}");
        let object = foundation::notification::name_object(name);
        out.push((
            symbol.clone(),
            corefoundation::base::global_cell(&symbol, object),
        ));
    }
    out
}

pub const NOTIFICATION_NAMES: &[&str] = &[
    application::WILL_FINISH_LAUNCHING,
    application::DID_FINISH_LAUNCHING,
    application::WILL_TERMINATE,
    application::DID_BECOME_ACTIVE,
    application::WILL_RESIGN_ACTIVE,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_name_resolves() {
        let missing: Vec<&str> = NAMES.iter().copied().filter(|n| symbol(n).is_none()).collect();
        assert!(missing.is_empty(), "listed but unresolved: {missing:?}");
    }

    /// A data symbol is the address of a word holding the value, never the value
    /// itself: the guest loads through it. The check is that reading the cell
    /// gives back an object, which is what a guest's second load does.
    #[test]
    fn a_notification_name_global_is_a_cell_holding_the_string() {
        let symbols = data_symbols();
        let (_, cell) = symbols
            .iter()
            .find(|(n, _)| n == "_NSApplicationDidFinishLaunchingNotification")
            .expect("published");
        // SAFETY: the cell is a word this crate leaked.
        let object = unsafe { *(*cell as *const u64) } as objc::object::Id;
        assert!(!object.is_null());
        // SAFETY: an object this runtime allocated.
        let class = unsafe { objc::object::class_of(object) }.expect("a class");
        assert!(class.chain().any(|c| c.name_str().contains("String")), "{}", class.name_str());
    }
}
