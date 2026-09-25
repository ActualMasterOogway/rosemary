use std::ffi::c_void;

const CHILD: &str = "ROSETTA3_FOUNDATION_OBJC_DEBUG_CHILD";

#[test]
fn pointer_value_survives_its_debug_checked_dealloc_chain() {
    if std::env::var_os(CHILD).is_some() {
        objc::debug::enable();
        foundation::bootstrap();
        let pool = objc::arc::pool_push();
        let class = objc::class::lookup(c"NSValue").expect("NSValue");
        let selector = objc::sel::get("valueWithPointer:");
        // SAFETY: this is the declared signature of +valueWithPointer:, and
        // the opaque pointer is deliberately not an owned allocation.
        let value = unsafe {
            let send: unsafe extern "C" fn(
                objc::object::Id,
                objc::sel::Sel,
                *const c_void,
            ) -> objc::object::Id = std::mem::transmute(objc::msg::entry());
            send(
                objc::class::ptr(class) as objc::object::Id,
                selector,
                0x10usize as *const c_void,
            )
        };
        assert!(!value.is_null());
        objc::arc::pool_pop(pool);
        return;
    }

    let output = std::process::Command::new(std::env::current_exe().expect("current test binary"))
        .args([
            "--exact",
            "pointer_value_survives_its_debug_checked_dealloc_chain",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .expect("run hardening child");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
