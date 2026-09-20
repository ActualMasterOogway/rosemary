//! The part of the Security framework a game reaches: the keychain.

pub mod codesign;
pub mod keychain;

exports::group! {
    codesign,
    keychain,
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
