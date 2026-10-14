#![allow(unsafe_op_in_unsafe_fn)]

use objc::{object::Id, sel::Sel};

unsafe fn call_void1(receiver: Id, selector: &str, arg: Id) {
    let sel = objc::sel::get(selector);
    let class = objc::object::class_of(receiver).expect("a live object");
    let imp = class
        .method(sel)
        .unwrap_or_else(|| panic!("missing {selector}"))
        .implementation();
    let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute_copy(&imp);
    f(receiver, sel, arg);
}

unsafe fn call1(receiver: Id, selector: &str, arg: Id) -> Id {
    let sel = objc::sel::get(selector);
    let class = objc::object::class_of(receiver).expect("a live object");
    let imp = class
        .method(sel)
        .unwrap_or_else(|| panic!("missing {selector}"))
        .implementation();
    let f: unsafe extern "C" fn(Id, Sel, Id) -> Id = std::mem::transmute_copy(&imp);
    f(receiver, sel, arg)
}

unsafe fn call_usize(receiver: Id, selector: &str, arg: usize) -> Id {
    let sel = objc::sel::get(selector);
    let class = objc::object::class_of(receiver).expect("a live object");
    let imp = class
        .method(sel)
        .unwrap_or_else(|| panic!("missing {selector}"))
        .implementation();
    let f: unsafe extern "C" fn(Id, Sel, usize) -> Id = std::mem::transmute_copy(&imp);
    f(receiver, sel, arg)
}

unsafe fn class_call1(name: &std::ffi::CStr, selector: &str, arg: Id) -> Id {
    let class = objc::class::lookup(name).unwrap();
    let sel = objc::sel::get(selector);
    let imp = class
        .metaclass()
        .method(sel)
        .expect("class method")
        .implementation();
    let f: unsafe extern "C" fn(Id, Sel, Id) -> Id = std::mem::transmute_copy(&imp);
    f(objc::class::ptr(class).cast(), sel, arg)
}

#[test]
fn mutable_string_initializers_produce_mutable_instances() {
    foundation::bootstrap();
    let pool = objc::arc::pool_push();

    let mut_class = objc::class::lookup(c"NSMutableString").unwrap();
    let mut_ptr = objc::class::ptr(mut_class).cast::<std::ffi::c_void>();

    let alloc_sel = objc::sel::get("alloc");
    let alloc_imp = mut_class
        .metaclass()
        .method(alloc_sel)
        .unwrap()
        .implementation();
    let alloc_fn: unsafe extern "C" fn(Id, Sel) -> Id =
        unsafe { std::mem::transmute_copy(&alloc_imp) };

    // 1. [[NSMutableString alloc] initWithCapacity:10]
    let alloced = unsafe { alloc_fn(mut_ptr, alloc_sel) };
    let s1 = unsafe { call_usize(alloced, "initWithCapacity:", 10) };
    let piece = foundation::string::from_str("hello ");
    unsafe { call_void1(s1, "appendString:", piece) };
    let piece2 = foundation::string::from_str("world");
    unsafe { call_void1(s1, "appendString:", piece2) };
    assert_eq!(unsafe { foundation::string::bytes(s1) }, b"hello world");

    // 2. [[NSMutableString alloc] initWithFormat:...]
    let alloced2 = unsafe { alloc_fn(mut_ptr, alloc_sel) };
    let fmt = foundation::string::from_str("init");
    let s2 = unsafe { call1(alloced2, "initWithFormat:", fmt) };
    let piece3 = foundation::string::from_str("ialized");
    unsafe { call_void1(s2, "appendString:", piece3) };
    assert_eq!(unsafe { foundation::string::bytes(s2) }, b"initialized");

    // 3. +[NSMutableString stringWithFormat:...]
    let s3 = unsafe { class_call1(c"NSMutableString", "stringWithFormat:", fmt) };
    unsafe { call_void1(s3, "appendString:", piece3) };
    assert_eq!(unsafe { foundation::string::bytes(s3) }, b"initialized");

    // 4. +[NSMutableString stringWithString:...]
    let src = foundation::string::from_str("base");
    let s4 = unsafe { class_call1(c"NSMutableString", "stringWithString:", src) };
    let piece4 = foundation::string::from_str("_appended");
    unsafe { call_void1(s4, "appendString:", piece4) };
    assert_eq!(unsafe { foundation::string::bytes(s4) }, b"base_appended");

    // 5. [[NSMutableString alloc] initWithString:...]
    let alloced3 = unsafe { alloc_fn(mut_ptr, alloc_sel) };
    let s5 = unsafe { call1(alloced3, "initWithString:", src) };
    unsafe { call_void1(s5, "appendString:", piece4) };
    assert_eq!(unsafe { foundation::string::bytes(s5) }, b"base_appended");

    objc::arc::pool_pop(pool);
}
