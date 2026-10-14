use corefoundation::collections::{
    Number, boolean, new_number, new_number_of_type, number_type as t,
};
use objc::{object::Id, sel::Sel};

unsafe fn objc_type(receiver: Id) -> String {
    let sel = objc::sel::get("objCType");
    let class = unsafe { objc::object::class_of(receiver) }.expect("a live object");
    let imp = class
        .method(sel)
        .expect("missing objCType")
        .implementation();
    // SAFETY: `-objCType` returns a C string.
    let call: unsafe extern "C" fn(Id, Sel) -> *const std::ffi::c_char =
        unsafe { std::mem::transmute_copy(&imp) };
    let p = unsafe { call(receiver, sel) };
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_str()
        .unwrap()
        .to_owned()
}

#[test]
fn number_obj_c_type_follows_the_canonical_type() {
    foundation::bootstrap();
    let int = |kind| new_number_of_type(Number::Integer(1), kind) as Id;
    let float = |kind| new_number_of_type(Number::Float(1.0), kind) as Id;
    unsafe {
        assert_eq!(objc_type(int(t::SINT8)), "c");
        assert_eq!(objc_type(int(t::SINT16)), "s");
        assert_eq!(objc_type(int(t::SINT32)), "i");
        assert_eq!(objc_type(int(t::SINT64)), "q");
        assert_eq!(objc_type(float(t::FLOAT32)), "f");
        assert_eq!(objc_type(float(t::FLOAT64)), "d");
        assert_eq!(objc_type(new_number(Number::Integer(7)) as Id), "q");
        assert_eq!(objc_type(new_number(Number::Float(7.5)) as Id), "d");
        assert_eq!(objc_type(boolean(true) as Id), "c");
    }
}
