use objc::{object::Id, sel::Sel};

#[test]
fn reachability_and_directory_inference_use_the_guest_namespace() {
    let root = std::env::temp_dir().join(format!("rosemary-url-prefix-{}", std::process::id()));
    // SAFETY: this integration-test process has one test; the namespace has not
    // been initialized and no other thread accesses these variables.
    unsafe { std::env::set_var("ROSEMARY_PREFIX", &root) };
    foundation::bootstrap();
    let pool = objc::arc::pool_push();
    let guest = "/rosemary-url-resource";
    std::fs::create_dir_all(prefix::ns::host_path(guest)).unwrap();
    unsafe {
        let class = objc::class::lookup(c"NSURL").unwrap();
        let class_id = objc::class::ptr(class) as Id;
        let sel = objc::sel::get("fileURLWithPath:");
        let make: unsafe extern "C" fn(Id, Sel, Id) -> Id =
            std::mem::transmute(class.metaclass().method(sel).unwrap().implementation());
        let path = foundation::string::from_str(guest);
        let url = make(class_id, sel, path);
        objc::arc::release(path);
        assert_eq!(
            corefoundation::url::as_url(url.cast()).unwrap().text,
            "file:///rosemary-url-resource/"
        );
        let sel = objc::sel::get("checkResourceIsReachableAndReturnError:");
        let check: unsafe extern "C" fn(Id, Sel, *mut Id) -> u8 = std::mem::transmute(
            corefoundation::url::class()
                .method(sel)
                .unwrap()
                .implementation(),
        );
        assert_eq!(check(url, sel, std::ptr::null_mut()), 1);
        let missing =
            corefoundation::url::new_file_url("/rosemary-url-resource/missing", false) as Id;
        let mut error = std::ptr::null_mut();
        assert_eq!(check(missing, sel, &mut error), 0);
        let sel = objc::sel::get("userInfo");
        let info: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(
            objc::object::class_of(error)
                .unwrap()
                .method(sel)
                .unwrap()
                .implementation(),
        );
        let key = foundation::string::from_str("NSFilePath");
        let value = corefoundation::api::CFDictionaryGetValue(info(error, sel).cast(), key.cast());
        assert_eq!(
            foundation::string::bytes(value as Id),
            b"/rosemary-url-resource/missing"
        );
        objc::arc::release(key);
        objc::arc::release(missing);
    }
    objc::arc::pool_pop(pool);
    std::fs::remove_dir_all(root).unwrap();
}
