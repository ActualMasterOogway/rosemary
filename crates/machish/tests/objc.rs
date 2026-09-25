#![cfg(all(feature = "std", feature = "arch-x86_64", feature = "ext-objc"))]

mod common;

use machish::extensions::objc::{ObjcClass, ObjcImage, ObjcMethod};
use machish::fixups::decode_image;
use machish::image::{MachObject, SymbolTable};
use machish::loader::Loader;
use machish::memory::{AddressSpace, BufferSpace};
use machish::resolve::{NativeDylibProvider, SymbolProvider, eager_binding_shims};
use machish::spec::arch::X86_64;

/// A class with an ivar, a property, an instance method, a class method, and a
/// protocol - one of each thing a `class_ro_t` can point at, so a decoder that
/// reads one field at the wrong offset cannot pass by having nothing there.
const CLASS_SRC: &str = r#"
#import <Foundation/Foundation.h>
@protocol Doubling
- (int)twice:(int)x;
@optional
- (int)maybe;
@end
@interface Greeter : NSObject <Doubling>
{ int _n; }
@property (nonatomic) int n;
- (int)twice:(int)x;
+ (int)zero;
@end
@implementation Greeter
- (int)twice:(int)x { return x * 2; }
+ (int)zero { return 0; }
@end
int main(void) { return 0; }
"#;

const CATEGORY_SRC: &str = r#"
#import <Foundation/Foundation.h>
@interface NSString (Doubling)
- (NSUInteger)doubledLength;
@end
@implementation NSString (Doubling)
- (NSUInteger)doubledLength { return [self length] * 2; }
@end
int main(void) { return 0; }
"#;

fn link(name: &str, source: &str) -> Option<(Vec<u8>, machish::report::LinkReport)> {
    let bytes = common::build_objc_x86_64(name, source)?;
    let mut mem = BufferSpace::new(0x1_0000_0000);
    let shims = eager_binding_shims();
    let native = NativeDylibProvider::system();
    let providers: [&dyn SymbolProvider; 2] = [&shims, &native];

    let report = Loader::new(X86_64)
        .link(&bytes, &format!("/tmp/{name}"), &mut mem, &providers)
        .unwrap();
    Some((bytes, report))
}

fn objc_of(name: &str, source: &str) -> Option<(Vec<u8>, u64, ObjcImage)> {
    let (bytes, report) = link(name, source)?;
    let img = report.root().unwrap();
    let slide = img.slide;
    Some((
        bytes.clone(),
        slide,
        img.objc.clone().expect("the fixture declares classes"),
    ))
}

#[test]
fn a_class_is_decoded_whole() {
    let Some((_, _, objc)) = objc_of("objc_class", CLASS_SRC) else {
        skip!("cc -arch x86_64 with Foundation unavailable")
    };

    assert!(
        objc.refusals.is_empty(),
        "unexpected refusals: {:?}",
        objc.refusals
    );

    let g = objc
        .classes
        .iter()
        .find(|c| c.ro.name == "Greeter")
        .expect("the fixture defines Greeter");

    // The instance half. `twice:` is declared; `n`/`setN:` are the property's
    // synthesized accessors, which is why the count is three and not one.
    let mut names: Vec<&str> = g.ro.methods.iter().map(|m| m.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["n", "setN:", "twice:"]);

    // The class half lives on the metaclass and nowhere else. A decoder that
    // stops at the class reports zero class methods and no error.
    let meta = g.meta.as_ref().expect("the metaclass is in this image");
    assert!(meta.ro.is_meta(), "the metaclass's ro must carry RO_META");
    assert_eq!(
        meta.ro
            .methods
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>(),
        ["zero"]
    );
    assert_eq!(
        meta.ro.name, g.ro.name,
        "a metaclass shares its class's name"
    );

    assert_eq!(g.ro.ivars.len(), 1);
    assert_eq!(g.ro.ivars[0].name, "_n");
    assert_eq!(g.ro.ivars[0].size, 4, "an `int` ivar");

    assert_eq!(g.ro.properties.len(), 1);
    assert_eq!(g.ro.properties[0].name, "n");

    assert_eq!(g.ro.protocols.len(), 1, "Greeter conforms to Doubling");

    let d = objc
        .protocols
        .iter()
        .find(|p| p.name == "Doubling")
        .expect("the fixture defines the Doubling protocol");
    assert_eq!(
        d.instance_methods
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>(),
        ["twice:"]
    );
    // `@optional` is a separate list, and conflating the two would make every
    // optional method look required - which is a decision the runtime makes
    // differently.
    assert_eq!(
        d.optional_instance_methods
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>(),
        ["maybe"]
    );
}

#[test]
fn every_method_carries_its_type_encoding() {
    let Some((_, _, objc)) = objc_of("objc_types", CLASS_SRC) else {
        skip!("cc -arch x86_64 with Foundation unavailable")
    };
    let g = objc
        .classes
        .iter()
        .find(|c| c.ro.name == "Greeter")
        .unwrap();

    let twice = g.ro.methods.iter().find(|m| m.name == "twice:").unwrap();
    // `i` returned; `@` self, `:` the selector, `i` the argument. The digits are
    // frame offsets, which vary with the ABI and are not asserted on.
    assert!(
        twice.types.starts_with('i'),
        "returns int, got {:?}",
        twice.types
    );
    let letters: String = twice
        .types
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .collect();
    assert_eq!(letters, "i@:i");

    let set_n = g.ro.methods.iter().find(|m| m.name == "setN:").unwrap();
    let letters: String = set_n
        .types
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .collect();
    assert_eq!(letters, "v@:i", "a setter returns void and takes one int");

    for m in &g.ro.methods {
        assert!(!m.types.is_empty(), "{} has no type encoding", m.name);
    }
}

#[test]
fn every_implementation_is_a_guest_text_address() {
    let Some((bytes, slide, objc)) = objc_of("objc_imps", CLASS_SRC) else {
        skip!("cc -arch x86_64 with Foundation unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let text: Vec<_> = obj
        .sections
        .iter()
        .filter(|s| s.seg_name == "__TEXT")
        .collect();

    let mut n = 0;
    for imp in objc.implementations() {
        let unslid = imp.wrapping_sub(slide);
        assert!(
            text.iter().any(|s| s.contains(unslid)),
            "imp {imp:#x} is not in __TEXT"
        );
        n += 1;
    }
    assert_eq!(n, 4, "three instance methods and one class method");
}

/// A category's target class is a *bind*, so on a host class it resolves to a
/// host address - and the decoder must report it rather than chase it.
#[test]
fn a_category_on_a_host_class_names_a_host_address() {
    let Some((bytes, slide, objc)) = objc_of("objc_category", CATEGORY_SRC) else {
        skip!("cc -arch x86_64 with Foundation unavailable")
    };
    assert!(
        objc.refusals.is_empty(),
        "unexpected refusals: {:?}",
        objc.refusals
    );

    let cat = objc.categories.first().expect("a category on NSString");
    assert_eq!(cat.name, "Doubling");
    assert_eq!(
        cat.instance_methods
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>(),
        ["doubledLength"]
    );

    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let unslid = cat.class.wrapping_sub(slide);
    assert!(
        obj.section_at(unslid).is_none(),
        "NSString is the host's class; {:#x} must not be in the guest image",
        cat.class
    );
    // And the method it adds is still ours.
    let imp = cat.instance_methods[0].imp;
    assert!(
        obj.section_at(imp.wrapping_sub(slide)).is_some(),
        "the imp is guest code"
    );
}

/// The one pointer in a class that is *not* a pointer: `bits` carries three
/// runtime flags in its low bits, and masking them off is the difference
/// between finding the `class_ro_t` and reading eight bytes before it.
#[test]
fn the_class_ro_is_found_through_the_masked_bits_word() {
    let Some((bytes, slide, objc)) = objc_of("objc_bits", CLASS_SRC) else {
        skip!("cc -arch x86_64 with Foundation unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let g = objc
        .classes
        .iter()
        .find(|c| c.ro.name == "Greeter")
        .unwrap();

    // The `objc_class` itself is in `__objc_data`, its read-only half in
    // `__objc_const`. Two different sections, which is what makes this
    // checkable without knowing either address.
    let class_sect = obj
        .section_at(g.va.wrapping_sub(slide))
        .expect("the class is in this image");
    let ro_sect = obj
        .section_at(g.ro.va.wrapping_sub(slide))
        .expect("the ro is in this image");
    assert_eq!(class_sect.sect_name, "__objc_data");
    assert_eq!(ro_sect.sect_name, "__objc_const");
    assert_eq!(
        g.ro.va & 7,
        0,
        "the ro pointer is aligned; no flag bits survived"
    );

    // A root class derives from nothing; Greeter does not, so its superclass is
    // the bound address of the host's NSObject.
    assert!(!g.ro.is_root());
    assert_ne!(g.superclass, 0);
    assert!(obj.section_at(g.superclass.wrapping_sub(slide)).is_none());
}

#[test]
fn selector_reference_slots_are_reported_by_address() {
    const SRC: &str = r#"
    #import <Foundation/Foundation.h>
    @interface Greeter : NSObject
    - (int)twice:(int)x;
    @end
    @implementation Greeter
    - (int)twice:(int)x { return x * 2; }
    @end
    int main(void) {
        Greeter *g = [Greeter new];
        return [g twice:21];
    }
    "#;
    let Some((bytes, slide, objc)) = objc_of("objc_selrefs", SRC) else {
        skip!("cc -arch x86_64 with Foundation unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    assert!(!objc.selector_refs.is_empty(), "the fixture sends messages");
    for &slot in &objc.selector_refs {
        let sect = obj
            .section_at(slot.wrapping_sub(slide))
            .expect("slot is in this image");
        assert_eq!(sect.sect_name, "__objc_selrefs");
    }
    // `[Greeter new]` reads the class from a classref slot rather than from a
    // symbol, which is why the runtime has to fix those up too.
    for &slot in &objc.class_refs {
        let sect = obj
            .section_at(slot.wrapping_sub(slide))
            .expect("slot is in this image");
        assert_eq!(sect.sect_name, "__objc_classrefs");
    }
}

/// The image-info flag is load-bearing, not informational: a `category_t` has
/// no size word, so this bit is the only thing that says whether its seventh
/// pointer exists.
#[test]
fn the_image_info_flag_decides_how_wide_a_category_is() {
    let Some((_, _, objc)) = objc_of("objc_imageinfo", CATEGORY_SRC) else {
        skip!("cc -arch x86_64 with Foundation unavailable")
    };
    let info = objc
        .image_info
        .expect("any image with __objc_* has an imageinfo section");
    assert!(
        info.has_category_class_properties(),
        "every current toolchain sets it; flags were {:#x}",
        info.flags
    );
}

#[test]
fn both_fixup_encodings_decode_to_the_same_metadata() {
    let Some(chained) = common::build_objc_x86_64("objc_chained", CLASS_SRC) else {
        skip!("cc -arch x86_64 with Foundation unavailable")
    };
    let Some(classic) =
        common::build_objc_x86_64_flags("objc_classic", CLASS_SRC, &["-Wl,-no_fixup_chains"])
    else {
        skip!("this toolchain cannot emit LC_DYLD_INFO")
    };

    let decode = |bytes: &[u8], name: &str| -> ObjcImage {
        let mut mem = BufferSpace::new(0x1_0000_0000);
        let shims = eager_binding_shims();
        let native = NativeDylibProvider::system();
        let providers: [&dyn SymbolProvider; 2] = [&shims, &native];
        let report = Loader::new(X86_64)
            .link(bytes, &format!("/tmp/{name}"), &mut mem, &providers)
            .unwrap();
        report.root().unwrap().objc.clone().unwrap()
    };

    let a = decode(&chained, "objc_chained");
    let b = decode(&classic, "objc_classic");

    // Addresses differ between the two links, so compare the parts that are
    // facts about the source rather than about the linker.
    let names = |o: &ObjcImage| {
        let mut v: Vec<String> = o
            .classes
            .iter()
            .flat_map(|c| {
                c.ro.methods
                    .iter()
                    .chain(c.meta.iter().flat_map(|m| &m.ro.methods))
                    .map(move |m| format!("{}::{} {}", c.ro.name, m.name, m.types))
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(names(&a), names(&b));
    assert!(a.refusals.is_empty() && b.refusals.is_empty());
    assert_eq!(
        a.classes.iter().map(|c| c.ro.ivars.len()).sum::<usize>(),
        b.classes.iter().map(|c| c.ro.ivars.len()).sum::<usize>(),
    );
}

/// An image with no Objective-C in it reports nothing, and reports it as
/// *nothing* rather than as an empty decode: the extension does not claim an
/// image with no `__objc_*` section, so the field stays `None`.
#[test]
fn a_plain_c_image_declares_nothing() {
    let Some(bytes) = common::build_x86_64("objc_none", "int main(void){return 0;}", &[]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let mut mem = BufferSpace::new(0x1_0000_0000);
    let shims = eager_binding_shims();
    let native = NativeDylibProvider::system();
    let providers: [&dyn SymbolProvider; 2] = [&shims, &native];
    let report = Loader::new(X86_64)
        .link(&bytes, "/tmp/objc_none", &mut mem, &providers)
        .unwrap();
    assert!(report.root().unwrap().objc.is_none());
}

/// Totality: the decoder must not panic, hang, or allocate unboundedly on
/// arbitrary bytes. The same sweep `tests/malformed.rs` makes for the parsers,
/// aimed at the one decoder that follows pointers it reads out of memory.
#[test]
fn corrupting_the_metadata_never_panics() {
    let Some(bytes) = common::build_objc_x86_64("objc_fuzz", CLASS_SRC) else {
        skip!("cc -arch x86_64 with Foundation unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    // Only the metadata sections are corrupted. Corrupting the whole file would
    // mostly produce images that fail to parse, which tests the parser again
    // rather than this.
    let ranges: Vec<_> = obj
        .sections
        .iter()
        .filter(|s| s.sect_name.starts_with("__objc_") && !s.is_zerofill())
        .map(|s| (s.offset as usize, s.size as usize))
        .collect();
    assert!(!ranges.is_empty());

    for (off, size) in ranges {
        for i in (0..size).step_by(3) {
            for pattern in [0xffu8, 0x00, 0x80] {
                let mut corrupt = bytes.clone();
                if off + i >= corrupt.len() {
                    continue;
                }
                corrupt[off + i] = pattern;

                let mut mem = BufferSpace::new(0x1_0000_0000);
                let shims = eager_binding_shims();
                let native = NativeDylibProvider::system();
                let providers: [&dyn SymbolProvider; 2] = [&shims, &native];
                // Ok or Err, never a panic and never a hang.
                let _ = Loader::new(X86_64).link(&corrupt, "/tmp/objc_fuzz", &mut mem, &providers);
            }
        }
    }
}

#[test]
fn both_method_list_encodings_decode_to_the_same_methods() {
    let Some((_, _, program)) = objc_of("objc_lists_exe", CLASS_SRC) else {
        skip!("cc -arch x86_64 with Foundation unavailable")
    };
    let Some(bytes) = common::build_objc_x86_64_flags(
        "libobjc_lists.dylib",
        CLASS_SRC,
        &["-dynamiclib", "-install_name", "@rpath/libobjc_lists.dylib"],
    ) else {
        skip!("cc -arch x86_64 with Foundation unavailable")
    };
    let mut mem = BufferSpace::new(0x2_0000_0000);
    let shims = eager_binding_shims();
    let native = NativeDylibProvider::system();
    let providers: [&dyn SymbolProvider; 2] = [&shims, &native];
    let report = Loader::new(X86_64)
        .link(&bytes, "/tmp/libobjc_lists.dylib", &mut mem, &providers)
        .unwrap();
    let img = report.root().unwrap();
    let dylib = img.objc.clone().expect("the fixture declares classes");

    let named = |o: &ObjcImage, name: &str| {
        o.classes
            .iter()
            .find(|c| c.ro.name == name)
            .unwrap_or_else(|| panic!("{name} is missing"))
            .clone()
    };
    let (a, b) = (named(&program, "Greeter"), named(&dylib, "Greeter"));

    // The dylib really is the encoding under test: `baseMethods` is at +32 in
    // the `class_ro_t`, and the list's first word carries the flag.
    let mut buf = [0u8; 8];
    mem.read(b.ro.va + 32, &mut buf).unwrap();
    let list_va = u64::from_le_bytes(buf);
    let mut word = [0u8; 4];
    mem.read(list_va, &mut word).unwrap();
    assert_ne!(
        u32::from_le_bytes(word) & 0x8000_0000,
        0,
        "the dylib was expected to carry relative method lists"
    );

    // Selectors and type encodings must match exactly. The `imp` addresses
    // cannot: the two images are different files, mapped at different bases.
    fn names(methods: &[ObjcMethod]) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = methods
            .iter()
            .map(|m| (m.name.clone(), m.types.clone()))
            .collect();
        v.sort();
        v
    }
    let class_methods = |c: &ObjcClass| c.meta.as_ref().map(|m| names(&m.ro.methods));
    assert!(
        !names(&a.ro.methods).is_empty(),
        "the program decoded no methods"
    );
    assert_eq!(names(&a.ro.methods), names(&b.ro.methods));
    assert_eq!(class_methods(&a), class_methods(&b), "class methods differ");
    assert!(dylib.refusals.is_empty(), "{:?}", dylib.refusals);

    // And every relative `imp` really lands in the dylib's own `__text`, which
    // is the offset arithmetic's only load-bearing claim.
    let text = MachObject::parse_any(&bytes, X86_64)
        .unwrap()
        .section("__TEXT", "__text")
        .map(|s| (s.addr + img.slide, s.size))
        .expect("__text");
    for m in
        b.ro.methods
            .iter()
            .chain(b.meta.iter().flat_map(|x| x.ro.methods.iter()))
    {
        assert!(
            m.imp >= text.0 && m.imp < text.0 + text.1,
            "{} resolved to {:#x}, outside __text",
            m.name,
            m.imp
        );
    }
}

#[test]
fn the_class_list_agrees_with_the_symbol_table() {
    let Some((bytes, slide, objc)) = objc_of("objc_symbols", CLASS_SRC) else {
        skip!("cc -arch x86_64 with Foundation unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let symtab = obj.symtab().expect("a linked image has a symbol table");
    let table = SymbolTable::new(*obj.view(), symtab, obj.dysymtab()).unwrap();

    let address_of = |want: &str| -> Option<u64> {
        table
            .iter()
            .find(|(_, s)| table.name(s).ok() == Some(want))
            .map(|(_, s)| s.n_value)
    };

    for c in &objc.classes {
        let want = format!("_OBJC_CLASS_$_{}", c.ro.name);
        let at = address_of(&want).unwrap_or_else(|| panic!("{want} is not in the symbol table"));
        assert_eq!(
            at,
            c.va.wrapping_sub(slide),
            "{want} and __objc_classlist disagree about where {} is",
            c.ro.name
        );

        let meta = c.meta.as_ref().expect("the metaclass is in this image");
        let want = format!("_OBJC_METACLASS_$_{}", c.ro.name);
        let at = address_of(&want).unwrap_or_else(|| panic!("{want} is not in the symbol table"));
        assert_eq!(at, meta.va.wrapping_sub(slide));
    }
}

#[test]
fn the_metadata_pointers_are_all_rebases() {
    let Some(bytes) = common::build_objc_x86_64("objc_rebases", CLASS_SRC) else {
        skip!("cc -arch x86_64 with Foundation unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let (_, fixups) = decode_image(&obj).expect("the image has fixups");
    assert!(
        fixups
            .iter()
            .any(|f| !matches!(f, machish::fixups::Fixup::Bind { .. })),
        "an Objective-C image is full of internal pointers, so it must rebase"
    );
}
