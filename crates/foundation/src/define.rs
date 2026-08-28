//! Declaring a class's methods.
//!
//! Two macros, local to this crate, in the spirit of the `table!` in
//! `darwin::pthread`: the point is that the list reads as a list.
//!
//! They are deliberately *not* the general `objc_class!` the roadmap wants -
//! that one should be extracted from these once there are enough of them to see
//! the shape, rather than designed before any of them exist.
//!
//! Instance and class methods are separate spellings rather than a marker on one
//! list. Adding a class method to the class itself is silently an *instance*
//! method, with no diagnostic anywhere, so the two cases are made to look
//! different at the call site.

/// Install instance methods on a class.
macro_rules! methods {
    ($class:expr, { $( $sel:literal => $imp:expr, $types:literal );* $(;)? }) => {{
        let class: &'static objc::class::Class = $class;
        $(
            class.replace_method(
                objc::sel::get($sel),
                $imp as *const std::ffi::c_void,
                $types,
            );
        )*
    }};
}

/// Install class methods, which live on the metaclass.
macro_rules! class_methods {
    ($class:expr, { $( $sel:literal => $imp:expr, $types:literal );* $(;)? }) => {{
        let class: &'static objc::class::Class = $class;
        let meta = class.metaclass();
        $(
            meta.replace_method(
                objc::sel::get($sel),
                $imp as *const std::ffi::c_void,
                $types,
            );
        )*
    }};
}

pub(crate) use {class_methods, methods};
