use machish::report::{ImageReport, LinkReport};

use super::object::{self, Kind};

/// A segment that is not a mapping: the address space reserved below the image.
const PAGEZERO: &str = "__PAGEZERO";

/// Announce every image in `report`. `root` is the path the program was loaded
/// from, which the root image's name may not be.
pub fn register_all(report: &LinkReport, root: &str) {
    if !super::enabled() {
        return;
    }
    for (index, image) in report.images.iter().enumerate() {
        let path = match index {
            0 => root,
            _ => &image.name,
        };
        register(image, path);
    }
}

fn register(image: &ImageReport, path: &str) {
    let stem = image.name.rsplit('/').next().unwrap_or(&image.name);

    let sections: Vec<object::Section> = image
        .segments
        .iter()
        .filter(|s| s.name != PAGEZERO && s.vmsize != 0)
        // Mapped, not resident: nothing travels with the object, because LLDB
        // reads a loaded section out of the live process and these run to a
        // hundred megabytes.
        .map(|s| {
            object::Section::mapped(
                match s.executable {
                    true => Kind::Code,
                    false => Kind::Data,
                },
                &s.name,
                s.vmaddr,
                12,
                s.vmsize,
            )
        })
        .collect();

    if sections.is_empty() {
        return;
    }

    let mut syms = symbols(image, path, stem);
    // So the image is named even when it yielded no symbols at all, and so
    // every address in it resolves to something rather than to the last symbol
    // of some other module.
    syms.push(object::Sym {
        name: stem.to_string(),
        sect: 1,
        value: sections[0].addr,
    });

    let syms = syms
        .into_iter()
        .filter_map(|mut sym| {
            sym.sect = section_of(&sections, sym.value)?;
            Some(sym)
        })
        .collect::<Vec<_>>();

    super::publish(object::write(&sections, &syms));
}

/// The 1-based index of the section holding `va`.
fn section_of(sections: &[object::Section], va: u64) -> Option<u8> {
    let index = sections
        .iter()
        .position(|s| va >= s.addr && va < s.addr.wrapping_add(s.size))?;
    u8::try_from(index + 1).ok()
}

/// What this image can be made to say about its own addresses.
///
/// Every symbol's *name* carries the unslid address, because that is the number
/// a disassembler shows for the same instruction and the slide is this run's
/// alone.
fn symbols(image: &ImageReport, path: &str, stem: &str) -> Vec<object::Sym> {
    let mut syms = Vec::new();

    // Objective-C first, so a method's own name wins over the function start at
    // the same address.
    if let Some(objc) = &image.objc {
        for class in &objc.classes {
            for (method, kind) in class.ro.methods.iter().map(|m| (m, '-')).chain(
                class
                    .meta
                    .iter()
                    .flat_map(|m| &m.ro.methods)
                    .map(|m| (m, '+')),
            ) {
                syms.push(object::Sym {
                    name: format!("{kind}[{} {}]", class.ro.name, method.name),
                    sect: 1,
                    value: method.imp,
                });
            }
        }
        for category in &objc.categories {
            for method in category
                .instance_methods
                .iter()
                .chain(&category.class_methods)
            {
                syms.push(object::Sym {
                    name: format!("[{}({}) {}]", stem, category.name, method.name),
                    sect: 1,
                    value: method.imp,
                });
            }
        }
    }

    // `LC_FUNCTION_STARTS` survives stripping
    for start in function_starts(path) {
        syms.push(object::Sym {
            name: format!("{stem}+{start:#x}"),
            sect: 1,
            value: start.wrapping_add(image.slide),
        });
    }

    let mut seen = std::collections::HashSet::new();
    syms.retain(|sym| seen.insert(sym.value));
    syms
}

/// The image's function starts, unslid. Empty if the file cannot be re-read or
/// does not record them.
fn function_starts(path: &str) -> Vec<u64> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(object) = machish::image::MachObject::parse_any(&bytes, crate::exec::native::HOST_SPEC)
    else {
        return Vec::new();
    };
    machish::image::function_starts(&object).unwrap_or_default()
}
