#![allow(unsafe_op_in_unsafe_fn)]

use objc::{object::Id, sel::Sel};

fn pool() -> *mut std::ffi::c_void {
    foundation::bootstrap();
    objc::arc::pool_push()
}

unsafe fn array_of(items: &[&str]) -> Id {
    let parts: Vec<_> = items
        .iter()
        .map(|s| foundation::string::from_str(s) as corefoundation::base::CFTypeRef)
        .collect();
    corefoundation::collections::new_array(
        &parts,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    ) as Id
}

unsafe fn strings(array: Id) -> Vec<String> {
    let array = corefoundation::collections::array(array.cast()).unwrap();
    array
        .values
        .iter()
        .map(|p| String::from_utf8(foundation::string::bytes(*p as Id)).unwrap())
        .collect()
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

unsafe fn class_call1(name: &std::ffi::CStr, selector: &str, arg: Id) -> Id {
    let class = objc::class::lookup(name).unwrap();
    let sel = objc::sel::get(selector);
    let imp = class
        .metaclass()
        .method(sel)
        .expect("class method")
        .implementation();
    let f: unsafe extern "C" fn(Id, Sel, Id) -> Id = unsafe { std::mem::transmute_copy(&imp) };
    unsafe { f(objc::class::ptr(class).cast(), sel, arg) }
}

unsafe fn call0(receiver: Id, selector: &str) -> Id {
    let sel = objc::sel::get(selector);
    let class = objc::object::class_of(receiver).expect("a live object");
    let imp = class
        .method(sel)
        .unwrap_or_else(|| panic!("missing {selector}"))
        .implementation();
    let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute_copy(&imp);
    f(receiver, sel)
}

fn text(id: Id) -> String {
    String::from_utf8(unsafe { foundation::string::bytes(id) }).unwrap()
}

#[test]
fn path_with_components_joins_parts() {
    let _p = pool();
    unsafe {
        let join = |parts: &[&str]| {
            let result = class_call1(c"NSString", "pathWithComponents:", array_of(parts));
            text(result)
        };
        assert_eq!(join(&["/", "usr", "lib"]), "/usr/lib");
        assert_eq!(join(&["usr", "lib", "/"]), "usr/lib/");
        assert_eq!(join(&[]), "");
    }
}

#[test]
fn path_with_components_round_trips_path_components() {
    let _p = pool();
    unsafe {
        let path = foundation::string::from_str("/Applications/Safari.app/");
        let parts = call0(path, "pathComponents");
        let joined = class_call1(c"NSString", "pathWithComponents:", parts);
        assert_eq!(text(joined), "/Applications/Safari.app/");
    }
}

#[test]
fn strings_by_appending_paths_appends_each() {
    let _p = pool();
    unsafe {
        let base = foundation::string::from_str("/tmp");
        let out = call1(base, "stringsByAppendingPaths:", array_of(&["a", "b/c"]));
        assert_eq!(strings(out), ["/tmp/a", "/tmp/b/c"]);
    }
}

#[test]
fn abbreviates_home_as_a_whole_component() {
    let _p = pool();
    unsafe {
        let home = foundation::home_directory();
        let inside = foundation::string::from_str(&format!("{home}/Documents"));
        assert_eq!(
            text(call0(inside, "stringByAbbreviatingWithTildeInPath")),
            "~/Documents"
        );
        let exact = foundation::string::from_str(&home);
        assert_eq!(
            text(call0(exact, "stringByAbbreviatingWithTildeInPath")),
            "~"
        );
        let sibling = foundation::string::from_str(&format!("{home}x/y"));
        assert_eq!(
            text(call0(sibling, "stringByAbbreviatingWithTildeInPath")),
            format!("{home}x/y")
        );
    }
}
