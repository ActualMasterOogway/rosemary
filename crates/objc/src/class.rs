//! Classes, metaclasses, ivars and methods.
//!
//! The first two words of a class are its metaclass and its superclass, and the
//! first word of an instance is its class, because guest code builds objects
//! with that layout by hand - a `__cfstring` literal in the guest's `__DATA` is
//! four words beginning with a class pointer, and nothing gets a chance to
//! translate it. Everything past those words is this crate's own business.
//!
//! Two invariants the rest of the runtime leans on, restated from the crate doc
//! because this is where they are kept:
//!
//! * [`method_implementation`] returns the resolved `IMP` itself, so a caller can
//!   re-probe it to notice that a method was swizzled.
//! * [`Method::implementation`] returns the *exact* pointer [`Class::add_method`]
//!   was given. A `Method` is a stable handle whose `IMP` moves, never a wrapper
//!   around one.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, RwLock};

use crate::sel::Sel;

/// A method implementation.
pub type Imp = *const c_void;

/// A method: a selector, a type encoding, and an `IMP` that can be replaced.
///
/// Leaked and never freed, so the handle a caller holds stays valid across every
/// later change to the class.
pub struct Method {
    pub sel: Sel,
    types: CString,
    imp: AtomicPtr<c_void>,
}

impl Method {
    /// The `IMP`, exactly as it was installed.
    pub fn implementation(&self) -> Imp {
        self.imp.load(Ordering::Acquire) as Imp
    }

    /// Replace the `IMP`, returning the one that was there.
    pub fn set_implementation(&self, imp: Imp) -> Imp {
        let previous = self.imp.swap(imp as *mut c_void, Ordering::AcqRel);
        bump_generation();
        previous as Imp
    }

    pub fn types(&self) -> *const c_char {
        self.types.as_ptr()
    }
}

/// An instance variable: where it sits in an instance, and how big it is.
pub struct Ivar {
    name: CString,
    /// Bytes from the start of the object. Written once, before registration.
    offset: usize,
    size: usize,
    types: CString,
}

impl Ivar {
    pub fn name(&self) -> *const c_char {
        self.name.as_ptr()
    }

    pub fn offset(&self) -> isize {
        self.offset as isize
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// The ivar's type encoding, which is what a key-value coder reads to know
    /// how wide the slot is and how to box what it finds there.
    pub fn types(&self) -> &CStr {
        &self.types
    }
}

/// The mutable half of a class.
#[derive(Default)]
struct Contents {
    methods: HashMap<usize, &'static Method>,
    ivars: Vec<&'static Ivar>,
    protocols: Vec<&'static crate::protocol::Protocol>,
}

/// A class.
///
/// `#[repr(C)]` for the first two fields only: an object's `isa` and a class's
/// `superclass` are read by hand often enough that their offsets are ABI.
#[repr(C)]
pub struct Class {
    /// The metaclass. A class is an object, and this is its `isa`.
    isa: AtomicPtr<Class>,
    superclass: AtomicPtr<Class>,
    name: CString,
    /// Bytes an instance occupies, `isa` included. Grows as ivars are added, and
    /// is fixed at registration.
    instance_size: AtomicUsize,
    is_meta: bool,
    /// Whether `objc_registerClassPair` has run. An unregistered class is not
    /// findable by name and must not be instantiated.
    registered: AtomicUsize,
    contents: RwLock<Contents>,
}

// SAFETY: every field is either immutable after construction or behind a lock or
// an atomic. The raw pointers are to leaked `Class`es, which outlive the process.
unsafe impl Sync for Class {}
unsafe impl Send for Class {}

/// Bumped whenever a method resolution anywhere could have changed.
///
/// A caller that caches a lookup compares this; see [`generation`].
static GENERATION: AtomicUsize = AtomicUsize::new(1);

fn bump_generation() {
    GENERATION.fetch_add(1, Ordering::AcqRel);
}

/// A counter that changes whenever any method table does.
pub fn generation() -> usize {
    GENERATION.load(Ordering::Acquire)
}

impl Class {
    fn new(name: CString, superclass: *mut Class, is_meta: bool, instance_size: usize) -> &'static Class {
        Box::leak(Box::new(Class {
            isa: AtomicPtr::new(std::ptr::null_mut()),
            superclass: AtomicPtr::new(superclass),
            name,
            instance_size: AtomicUsize::new(instance_size),
            is_meta,
            registered: AtomicUsize::new(0),
            contents: RwLock::new(Contents::default()),
        }))
    }

    pub fn name(&self) -> *const c_char {
        self.name.as_ptr()
    }

    pub fn name_str(&self) -> &str {
        self.name.to_str().unwrap_or("<non-utf8 class name>")
    }

    pub fn is_meta(&self) -> bool {
        self.is_meta
    }

    pub fn is_registered(&self) -> bool {
        self.registered.load(Ordering::Acquire) != 0
    }

    pub fn superclass(&self) -> Option<&'static Class> {
        let p = self.superclass.load(Ordering::Acquire);
        // SAFETY: null, or a leaked `Class` that outlives the process.
        (!p.is_null()).then(|| unsafe { &*(p as *const Class) })
    }

    /// The metaclass, which is where class methods live.
    pub fn metaclass(&self) -> &'static Class {
        let p = self.isa.load(Ordering::Acquire);
        assert!(!p.is_null(), "every class pair has a metaclass");
        // SAFETY: set at allocation to a leaked `Class`, and never cleared.
        unsafe { &*(p as *const Class) }
    }

    pub fn instance_size(&self) -> usize {
        self.instance_size.load(Ordering::Acquire)
    }

    /// This class and every superclass, nearest first.
    pub fn chain(&'static self) -> impl Iterator<Item = &'static Class> {
        let mut next = Some(self);
        std::iter::from_fn(move || {
            let current = next?;
            next = current.superclass();
            Some(current)
        })
    }

    /// The method this class itself declares for `sel`, ignoring superclasses.
    pub fn own_method(&self, sel: Sel) -> Option<&'static Method> {
        self.contents
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .methods
            .get(&(sel as usize))
            .copied()
    }

    /// The method `sel` resolves to, searching superclasses.
    pub fn method(&'static self, sel: Sel) -> Option<&'static Method> {
        self.chain().find_map(|c| c.own_method(sel))
    }

    /// Install `imp` for `sel`, failing if this class already declares it.
    ///
    /// `false` rather than an error, because that is what the C API returns and
    /// because a duplicate is the guest declaring the same selector twice.
    pub fn add_method(&self, sel: Sel, imp: Imp, types: &CStr) -> bool {
        let mut contents = self.contents.write().unwrap_or_else(|e| e.into_inner());
        if contents.methods.contains_key(&(sel as usize)) {
            return false;
        }
        let method: &'static Method = Box::leak(Box::new(Method {
            sel,
            types: CString::from(types),
            imp: AtomicPtr::new(imp as *mut c_void),
        }));
        contents.methods.insert(sel as usize, method);
        drop(contents);
        bump_generation();
        true
    }

    /// Install `imp` for `sel` whether or not it is already declared, returning
    /// the `IMP` that was there.
    pub fn replace_method(&self, sel: Sel, imp: Imp, types: &CStr) -> Imp {
        if let Some(existing) = self.own_method(sel) {
            return existing.set_implementation(imp);
        }
        self.add_method(sel, imp, types);
        std::ptr::null()
    }

    /// Append an ivar. Only before registration, and never on a metaclass.
    pub fn add_ivar(&self, name: &CStr, size: usize, alignment: u8, types: &CStr) -> bool {
        if self.is_registered() || self.is_meta {
            return false;
        }
        let mut contents = self.contents.write().unwrap_or_else(|e| e.into_inner());
        if contents.ivars.iter().any(|i| i.name.as_c_str() == name) {
            return false;
        }
        // `alignment` is log2, as `class_addIvar` documents. Apple caps it too.
        let align = 1usize << alignment.min(15);
        let start = self.instance_size.load(Ordering::Acquire);
        let offset = start.next_multiple_of(align);
        let ivar: &'static Ivar = Box::leak(Box::new(Ivar {
            name: CString::from(name),
            offset,
            size,
            types: CString::from(types),
        }));
        contents.ivars.push(ivar);
        self.instance_size.store(offset + size, Ordering::Release);
        true
    }

    /// The ivar this class or a superclass declares under `name`.
    pub fn ivar(&'static self, name: &CStr) -> Option<&'static Ivar> {
        self.chain().find_map(|c| {
            c.contents
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .ivars
                .iter()
                .find(|i| i.name.as_c_str() == name)
                .copied()
        })
    }

    pub fn add_protocol(&self, protocol: &'static crate::protocol::Protocol) -> bool {
        let mut contents = self.contents.write().unwrap_or_else(|e| e.into_inner());
        if contents.protocols.iter().any(|p| std::ptr::eq(*p, protocol)) {
            return false;
        }
        contents.protocols.push(protocol);
        true
    }

    /// Whether this class or a superclass adopts `protocol`.
    pub fn conforms_to(&'static self, protocol: &crate::protocol::Protocol) -> bool {
        self.chain().any(|c| {
            c.contents
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .protocols
                .iter()
                .any(|p| std::ptr::eq(*p, protocol) || p.conforms_to(protocol))
        })
    }
}

/// A class by name, or `None` if none is registered.
///
/// Does not bootstrap: [`crate::bootstrap`] itself reaches this while installing
/// `NSObject`, and a `Once` re-entered from inside its own initializer does not
/// return.
pub fn lookup(name: &CStr) -> Option<&'static Class> {
    let name = name.to_str().ok()?;
    CLASSES
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .copied()
}

/// Every registered class, by name.
static CLASSES: LazyLock<RwLock<HashMap<String, &'static Class>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Guards a class pair being built, so two threads cannot allocate the same name.
static ALLOCATION: Mutex<()> = Mutex::new(());

/// Allocate an unregistered class and its metaclass.
///
/// `None` when the name is taken, which is what `objc_allocateClassPair`'s null
/// return means. `superclass` of `None` makes a root class.
pub fn allocate_pair(
    superclass: Option<&'static Class>,
    name: &CStr,
    extra: usize,
) -> Option<&'static Class> {
    let _guard = ALLOCATION.lock().unwrap_or_else(|e| e.into_inner());
    if lookup(name).is_some() {
        return None;
    }
    let name = CString::from(name);

    // An instance starts where the superclass's ended - at `isa` for a root.
    let instance_start = superclass.map_or(size_of::<usize>(), |s| s.instance_size());
    let class = Class::new(
        name.clone(),
        superclass.map_or(std::ptr::null_mut(), |s| ptr(s)),
        false,
        instance_start + extra,
    );

    // The metaclass's superclass is the superclass's metaclass, and a root
    // metaclass's superclass is the root class itself - which is what makes
    // `+alloc` findable from a metaclass.
    let meta_super = match superclass {
        Some(s) => ptr(s.metaclass()),
        None => ptr(class),
    };
    let meta = Class::new(name, meta_super, true, size_of::<usize>());

    // Every metaclass's `isa` is the *root* metaclass, and the root metaclass's
    // is itself. That is what stops `object_getClass` recursing forever.
    let root_meta = match superclass {
        Some(s) => root_metaclass(s),
        None => meta,
    };
    meta.isa.store(ptr(root_meta), Ordering::Release);
    class.isa.store(ptr(meta), Ordering::Release);
    Some(class)
}

/// The metaclass at the top of `class`'s chain.
fn root_metaclass(class: &'static Class) -> &'static Class {
    let mut root = class;
    while let Some(next) = root.superclass() {
        root = next;
    }
    root.metaclass()
}

/// Make `class` findable by name and instantiable.
pub fn register_pair(class: &'static Class) {
    if class.registered.swap(1, Ordering::AcqRel) != 0 {
        return;
    }
    class.metaclass().registered.store(1, Ordering::Release);
    with_classes(|classes| {
        classes.insert(class.name_str().to_string(), class);
    });
}

/// Forget a class pair that was allocated and never registered.
pub fn dispose_pair(class: &'static Class) {
    if class.is_registered() {
        with_classes(|classes| {
            classes.remove(class.name_str());
        });
    }
    // The `Class` itself stays leaked: a pointer to it may have escaped, and a
    // dangling class pointer is far worse than a few hundred bytes.
}

fn with_classes<R>(body: impl FnOnce(&mut HashMap<String, &'static Class>) -> R) -> R {
    body(&mut CLASSES.write().unwrap_or_else(|e| e.into_inner()))
}

/// A `&'static Class` as the raw pointer the C API passes around.
pub fn ptr(class: &'static Class) -> *mut Class {
    class as *const Class as *mut Class
}

/// The class a raw pointer names.
///
/// # Safety
///
/// `p` must be null or a pointer this crate produced.
pub unsafe fn from_ptr(p: *const Class) -> Option<&'static Class> {
    // SAFETY: the caller's obligation.
    (!p.is_null()).then(|| unsafe { &*p })
}

/// The `IMP` `sel` resolves to on `class`, or null.
///
/// Returns the resolved `IMP` itself. A caller may cache it and re-probe to
/// notice a swizzle, which is exactly what `rosetta3`'s dispatch plan does, and
/// only holds because nothing is wrapped on the way out.
pub fn method_implementation(class: &'static Class, sel: Sel) -> Imp {
    class
        .method(sel)
        .map_or(std::ptr::null(), |m| m.implementation())
}
