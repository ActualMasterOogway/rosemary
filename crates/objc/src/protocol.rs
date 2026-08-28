//! Protocols.
//!
//! A protocol carries method *descriptions* - a selector and a type encoding,
//! with no implementation - which is all a guest asking
//! `protocol_getMethodDescription` wants back.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char};
use std::sync::{LazyLock, RwLock};

use crate::sel::Sel;

/// One declared method: what to send, and what its signature is.
///
/// `#[repr(C)]` and returned by value, because `protocol_getMethodDescription`
/// returns a two-word struct and the caller reads it out of the registers the C
/// ABI puts it in.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Description {
    pub sel: Sel,
    pub types: *const c_char,
}

/// A protocol.
pub struct Protocol {
    name: CString,
    /// Keyed by `(selector, required, instance)`, the three coordinates
    /// `protocol_getMethodDescription` is asked for.
    contents: RwLock<Contents>,
    registered: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct Contents {
    methods: HashMap<(usize, bool, bool), &'static CStr>,
    adopted: Vec<&'static Protocol>,
}

// SAFETY: `name` is immutable, the rest is behind a lock or an atomic, and the
// pointers are to leaked values that outlive the process.
unsafe impl Sync for Protocol {}
unsafe impl Send for Protocol {}

impl Protocol {
    pub fn name(&self) -> *const c_char {
        self.name.as_ptr()
    }

    pub fn add_method(&self, sel: Sel, types: &CStr, required: bool, instance: bool) {
        let types: &'static CStr = Box::leak(CString::from(types).into_boxed_c_str());
        self.contents
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .methods
            .insert((sel as usize, required, instance), types);
    }

    pub fn add_protocol(&self, other: &'static Protocol) {
        self.contents
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .adopted
            .push(other);
    }

    /// The description of `sel`, or one with two nulls if this protocol has none.
    ///
    /// Two nulls rather than an absence, because that is the shape
    /// `protocol_getMethodDescription` returns by value.
    pub fn method(&self, sel: Sel, required: bool, instance: bool) -> Description {
        let contents = self.contents.read().unwrap_or_else(|e| e.into_inner());
        match contents.methods.get(&(sel as usize, required, instance)) {
            Some(types) => Description {
                sel,
                types: types.as_ptr(),
            },
            None => Description {
                sel: std::ptr::null(),
                types: std::ptr::null(),
            },
        }
    }

    /// Whether this protocol adopts `other`, directly or through another.
    pub fn conforms_to(&self, other: &Protocol) -> bool {
        let contents = self.contents.read().unwrap_or_else(|e| e.into_inner());
        contents
            .adopted
            .iter()
            .any(|p| std::ptr::eq(*p, other) || p.conforms_to(other))
    }
}

static PROTOCOLS: LazyLock<RwLock<HashMap<String, &'static Protocol>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// A protocol by name, or `None`.
pub fn lookup(name: &CStr) -> Option<&'static Protocol> {
    let name = name.to_str().ok()?;
    PROTOCOLS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .copied()
}

/// Allocate an unregistered protocol, or `None` if the name is taken.
pub fn allocate(name: &CStr) -> Option<&'static Protocol> {
    if lookup(name).is_some() {
        return None;
    }
    Some(Box::leak(Box::new(Protocol {
        name: CString::from(name),
        contents: RwLock::new(Contents::default()),
        registered: std::sync::atomic::AtomicBool::new(false),
    })))
}

/// Make `protocol` findable by name.
pub fn register(protocol: &'static Protocol) {
    if protocol
        .registered
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        return;
    }
    let name = protocol.name.to_string_lossy().into_owned();
    PROTOCOLS
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name, protocol);
}
