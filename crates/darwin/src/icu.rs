//! The few `libicucore` entry points programs call directly.
//!
//! A Mac's `libicucore.A.dylib` exports ICU without the version suffix the
//! base's own ICU builds carry, so a guest's import cannot bind there.

/// The first code point of each run of ten `Nd` (decimal digit) characters,
/// zero through nine, as of Unicode 15.1.
const DIGIT_ZEROS: &[u32] = &[
    0x0030, 0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66, 0x0CE6,
    0x0D66, 0x0DE6, 0x0E50, 0x0ED0, 0x0F20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80,
    0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0,
    0xFF10, 0x104A0, 0x10D30, 0x11066, 0x110F0, 0x11136, 0x111D0, 0x112F0, 0x11450, 0x114D0,
    0x11650, 0x116C0, 0x11730, 0x118E0, 0x11950, 0x11C50, 0x11D50, 0x11DA0, 0x11F50, 0x16A60,
    0x16AC0, 0x16B50, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0,
    0x1E950, 0x1FBF0,
];

/// `int32_t u_charDigitValue(UChar32 c)`: the decimal digit value of `c`, or
/// -1 when it is not a decimal digit.
#[allow(non_snake_case)]
pub extern "C" fn u_charDigitValue(c: i32) -> i32 {
    let Ok(c) = u32::try_from(c) else {
        return -1;
    };
    DIGIT_ZEROS
        .iter()
        .find(|zero| (**zero..**zero + 10).contains(&c))
        .map_or(-1, |zero| (c - zero) as i32)
}

exports::exports! {
    functions {
        "u_charDigitValue" => u_charDigitValue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digit_values_cover_every_script_and_nothing_else() {
        assert_eq!(u_charDigitValue('7' as i32), 7);
        assert_eq!(u_charDigitValue(0x0663), 3, "Arabic-Indic three");
        assert_eq!(u_charDigitValue(0xFF19), 9, "fullwidth nine");
        assert_eq!(u_charDigitValue('a' as i32), -1);
        assert_eq!(u_charDigitValue('/' as i32), -1);
        assert_eq!(u_charDigitValue(-5), -1);
    }
}
