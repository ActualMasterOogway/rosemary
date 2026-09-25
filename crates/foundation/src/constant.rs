use std::ffi::c_char;

use objc::class::Class;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

#[repr(C)]
struct ConstantDouble {
    isa: *const Class,
    value: f64,
}

#[repr(C)]
struct ConstantFloat {
    isa: *const Class,
    value: f32,
}

#[repr(C)]
struct ConstantInteger {
    isa: *const Class,
    encoding: *const c_char,
    value: i64,
}

/// The value one of these holds, as a double and as an integer.
///
/// # Safety
///
/// `receiver` must be an instance of the class the methods below are installed on.
unsafe fn double_of(receiver: Id) -> f64 {
    unsafe { (*(receiver as *const ConstantDouble)).value }
}

unsafe fn float_of(receiver: Id) -> f32 {
    unsafe { (*(receiver as *const ConstantFloat)).value }
}

unsafe fn integer_of(receiver: Id) -> i64 {
    unsafe { (*(receiver as *const ConstantInteger)).value }
}

/// Build the six accessors a number answers, for one storage shape.
///
/// A macro because the six are the same six each time and the only thing that
/// changes is how the value is read - which is exactly the case a list of
/// hand-written near-copies hides.
macro_rules! accessors {
    ($module:ident, $read:expr, $encoding:expr) => {
        mod $module {
            use super::*;

            unsafe extern "C" fn double_value(receiver: Id, _sel: Sel) -> f64 {
                $read(receiver) as f64
            }
            unsafe extern "C" fn float_value(receiver: Id, _sel: Sel) -> f32 {
                $read(receiver) as f32
            }
            unsafe extern "C" fn long_long_value(receiver: Id, _sel: Sel) -> i64 {
                $read(receiver) as i64
            }
            unsafe extern "C" fn int_value(receiver: Id, _sel: Sel) -> i32 {
                $read(receiver) as i32
            }
            unsafe extern "C" fn bool_value(receiver: Id, _sel: Sel) -> bool {
                $read(receiver) as i64 != 0
            }
            unsafe extern "C" fn obj_c_type(receiver: Id, _sel: Sel) -> *const c_char {
                $encoding(receiver)
            }

            /// Every accessor above reads the receiver's own storage, which is
            /// the layout its class was registered with. The `unsafe` is in the
            /// reader passed to this macro.
            ///
            /// Image-resident: there is nothing to free and no count to keep.
            unsafe extern "C" fn retain(receiver: Id, _sel: Sel) -> Id {
                receiver
            }
            unsafe extern "C" fn release(_receiver: Id, _sel: Sel) {}
            unsafe extern "C" fn retain_count(_receiver: Id, _sel: Sel) -> usize {
                usize::MAX
            }

            pub(super) fn install(name: &'static std::ffi::CStr, size: usize) {
                let number =
                    objc::class::lookup(c"NSNumber").expect("CoreFoundation registered NSNumber");
                let class = corefoundation::base::register(name, Some(number), size, false);
                methods!(class, {
                    "doubleValue" => double_value, c"d16@0:8";
                    "floatValue" => float_value, c"f16@0:8";
                    "longLongValue" => long_long_value, c"q16@0:8";
                    "integerValue" => long_long_value, c"q16@0:8";
                    "longValue" => long_long_value, c"q16@0:8";
                    "unsignedIntegerValue" => long_long_value, c"Q16@0:8";
                    "intValue" => int_value, c"i16@0:8";
                    "boolValue" => bool_value, c"B16@0:8";
                    "objCType" => obj_c_type, c"*16@0:8";
                    "retain" => retain, c"@16@0:8";
                    "autorelease" => retain, c"@16@0:8";
                    "release" => release, c"v16@0:8";
                    "dealloc" => release, c"v16@0:8";
                    "retainCount" => retain_count, c"Q16@0:8"
                });
            }
        }
    };
}

fn fixed(encoding: &'static std::ffi::CStr) -> impl Fn(Id) -> *const c_char {
    move |_| encoding.as_ptr()
}

accessors!(doubles, |r| unsafe { double_of(r) }, fixed(c"d"));
accessors!(floats, |r| unsafe { float_of(r) }, fixed(c"f"));
accessors!(integers, |r| unsafe { integer_of(r) }, |r: Id| {
    // The integer form carries its own, because `@(5)` and `@(5L)` are
    // different widths and a caller switching on `-objCType` has to see which.
    // SAFETY: an `NSConstantIntegerNumber`.
    unsafe { (*(r as *const ConstantInteger)).encoding }
});

/// Every class this module owns.
pub const CLASSES: &[&str] = &[
    "NSConstantDoubleNumber",
    "NSConstantFloatNumber",
    "NSConstantIntegerNumber",
];

pub(crate) fn install() {
    doubles::install(c"NSConstantDoubleNumber", size_of::<ConstantDouble>());
    floats::install(c"NSConstantFloatNumber", size_of::<ConstantFloat>());
    integers::install(c"NSConstantIntegerNumber", size_of::<ConstantInteger>());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shapes are the compiler's, so the sizes are asserted rather than
    /// chosen: a wrong one reads the value out of the wrong word.
    #[test]
    fn the_layouts_are_the_ones_clang_emits() {
        assert_eq!(size_of::<ConstantDouble>(), 16);
        assert_eq!(size_of::<ConstantFloat>(), 16);
        assert_eq!(size_of::<ConstantInteger>(), 24);
        assert_eq!(std::mem::offset_of!(ConstantDouble, value), 8);
        assert_eq!(std::mem::offset_of!(ConstantFloat, value), 8);
        // The integer's value is the *third* word: the second is its encoding.
        assert_eq!(std::mem::offset_of!(ConstantInteger, value), 16);
    }

    /// An object laid out as the compiler would lay one out reads back.
    #[test]
    fn a_constant_double_reads_its_second_word() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSConstantDoubleNumber").expect("registered");
        let literal = ConstantDouble {
            isa: objc::class::ptr(class),
            value: 9.25,
        };
        // SAFETY: exactly the layout the class was registered with.
        unsafe {
            let object = &literal as *const ConstantDouble as Id;
            assert_eq!(double_of(object), 9.25);
            assert_eq!(crate::real_of(object), Some(9.25));
            assert_eq!(crate::number_of(object), Some(9));
        }
    }
}
