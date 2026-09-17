//! The part of the Security framework a game reaches: the keychain.

pub mod keychain;

/// The C entry points this crate answers.
pub fn symbol(name: &str) -> Option<u64> {
    keychain::symbol(name)
}

/// Every name [`symbol`] answers.
pub fn names() -> impl Iterator<Item = &'static str> {
    keychain::NAMES.iter().copied()
}

/// The `kSec*` key globals, each a cell holding an `NSString *`.
pub fn data_symbols() -> Vec<(String, u64)> {
    foundation::bootstrap();
    keychain::KEYS
        .iter()
        .map(|(symbol, text)| {
            (
                (*symbol).to_string(),
                corefoundation::base::global_cell(
                    symbol,
                    foundation::notification::name_object(text),
                ),
            )
        })
        .collect()
}
