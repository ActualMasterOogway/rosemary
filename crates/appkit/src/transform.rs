use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

unsafe extern "C" fn transform(_class: Id, _sel: Sel) -> Id {
    let class = objc::class::lookup(c"NSAffineTransform").expect("declared");
    let object = objc::object::create_instance(class, 0);
    // SAFETY: a fresh convenience-constructor result.
    unsafe { corefoundation::api::CFAutorelease(object as corefoundation::base::CFTypeRef) as Id }
}

unsafe extern "C" fn set(_receiver: Id, _sel: Sel) {}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSAffineTransform").expect("declared before its methods");
    class_methods!(class, {
        "transform" => transform, c"@16@0:8";
    });
    methods!(class, {
        "set" => set, c"v16@0:8";
    });
}
