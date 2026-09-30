use super::*;

use corefoundation::callbacks::Policy;
use corefoundation::collections::{Number, new_dictionary, new_number};

fn properties(pairs: &[(&str, i64)]) -> CFTypeRef {
    corefoundation::bootstrap();
    let pairs: Vec<(CFTypeRef, CFTypeRef)> = pairs
        .iter()
        .map(|(name, value)| (key(name), new_number(Number::Integer(*value))))
        .collect();
    new_dictionary(
        &pairs,
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        false,
    )
}

/// What Qt's `QIOSurfaceGraphicsBuffer` asks for.
fn bgra(width: i64, height: i64) -> CFTypeRef {
    // SAFETY: keys this crate made.
    let row = unsafe { IOSurfaceAlignProperty(key(BYTES_PER_ROW), width as usize * 4) };
    let size = unsafe { IOSurfaceAlignProperty(key(ALLOC_SIZE), row * height as usize) };
    let options = properties(&[
        (WIDTH, width),
        (HEIGHT, height),
        (PIXEL_FORMAT, i64::from(PIXEL_FORMAT_BGRA)),
        (BYTES_PER_ELEMENT, 4),
        (BYTES_PER_ROW, row as i64),
        (ALLOC_SIZE, size as i64),
    ]);
    // SAFETY: a dictionary of numbers.
    unsafe { IOSurfaceCreate(options) }
}

#[test]
fn a_surface_has_the_layout_it_was_aligned_for() {
    let surface = bgra(600, 300);
    assert!(!surface.is_null());
    // SAFETY: a surface this test made.
    unsafe {
        let row = IOSurfaceAlignProperty(key(BYTES_PER_ROW), 600 * 4);
        assert_eq!(row % ROW_ALIGNMENT, 0);
        assert!(row >= 2400);
        assert_eq!(IOSurfaceGetBytesPerRow(surface), row);
        assert_eq!(IOSurfaceGetWidth(surface), 600);
        assert_eq!(IOSurfaceGetHeight(surface), 300);
        assert_eq!(IOSurfaceGetBytesPerElement(surface), 4);
        assert_eq!(IOSurfaceGetPixelFormat(surface), PIXEL_FORMAT_BGRA);
        assert!(IOSurfaceGetAllocSize(surface) >= row * 300);
        assert_eq!(IOSurfaceGetAllocSize(surface) % PAGE, 0);
        let base = IOSurfaceGetBaseAddress(surface) as *mut u8;
        assert!(!base.is_null());
        // The whole of the last row is writable.
        base.add(row * 299 + 2399).write(7);
        assert_eq!(
            IOSurfaceGetTypeID(),
            corefoundation::api::CFGetTypeID(surface)
        );
        corefoundation::api::CFRelease(surface);
    }
}

#[test]
fn a_surface_without_a_size_is_not_made() {
    // SAFETY: a dictionary of numbers.
    assert!(unsafe { IOSurfaceCreate(properties(&[(WIDTH, 4)])) }.is_null());
    assert!(unsafe { IOSurfaceCreate(properties(&[(WIDTH, 4), (HEIGHT, -1)])) }.is_null());
}

#[test]
fn a_planar_surface_is_refused() {
    corefoundation::bootstrap();
    let planes = corefoundation::collections::new_array(&[], Policy::from_type_callbacks(), false);
    let options = new_dictionary(
        &[
            (key(WIDTH), new_number(Number::Integer(4))),
            (key(HEIGHT), new_number(Number::Integer(4))),
            (key(PLANE_INFO), planes),
        ],
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a dictionary.
    assert!(unsafe { IOSurfaceCreate(options) }.is_null());
}

#[test]
fn defaults_are_one_byte_per_element() {
    // SAFETY: a dictionary of numbers.
    let surface = unsafe { IOSurfaceCreate(properties(&[(WIDTH, 3), (HEIGHT, 2)])) };
    // SAFETY: a surface this test made.
    unsafe {
        assert_eq!(IOSurfaceGetBytesPerElement(surface), 1);
        assert_eq!(IOSurfaceGetElementWidth(surface), 1);
        assert_eq!(IOSurfaceGetBytesPerRow(surface), ROW_ALIGNMENT);
        corefoundation::api::CFRelease(surface);
    }
}

#[test]
fn a_write_unlock_moves_the_seed_and_a_read_unlock_does_not() {
    let surface = bgra(2, 2);
    let mut seed = u32::MAX;
    // SAFETY: a surface this test made.
    unsafe {
        assert_eq!(IOSurfaceUnlock(surface, 0, &mut seed), NOT_LOCKED);
        assert_eq!(IOSurfaceLock(surface, 0, &mut seed), SUCCESS);
        assert_eq!(seed, 0);
        assert_eq!(IOSurfaceUnlock(surface, 0, &mut seed), SUCCESS);
        assert_eq!(seed, 1);
        IOSurfaceLock(surface, LOCK_READ_ONLY, std::ptr::null_mut());
        IOSurfaceUnlock(surface, LOCK_READ_ONLY, &mut seed);
        assert_eq!(seed, 1);
        assert_eq!(IOSurfaceGetSeed(surface), 1);
        corefoundation::api::CFRelease(surface);
    }
}

#[test]
fn the_use_count_is_what_in_use_reports() {
    let surface = bgra(2, 2);
    // SAFETY: a surface this test made.
    unsafe {
        assert_eq!(IOSurfaceIsInUse(surface), 0);
        IOSurfaceIncrementUseCount(surface);
        assert_eq!(IOSurfaceIsInUse(surface), 1);
        assert_eq!(IOSurfaceGetUseCount(surface), 1);
        IOSurfaceDecrementUseCount(surface);
        IOSurfaceDecrementUseCount(surface);
        assert_eq!(IOSurfaceGetUseCount(surface), 0, "never below zero");
        corefoundation::api::CFRelease(surface);
    }
}

#[test]
fn values_are_set_replaced_copied_and_removed() {
    let surface = bgra(2, 2);
    let name = key("IOSurfaceColorSpace");
    let first = new_number(Number::Integer(1));
    let second = new_number(Number::Integer(2));
    // SAFETY: a surface and CF objects this test made.
    unsafe {
        IOSurfaceSetValue(surface, name, first);
        IOSurfaceSetValue(surface, name, second);
        let copied = IOSurfaceCopyValue(surface, name);
        assert_eq!(copied, second);
        corefoundation::api::CFRelease(copied);
        let all = IOSurfaceCopyAllValues(surface);
        assert_eq!(corefoundation::api::CFDictionaryGetCount(all), 1);
        corefoundation::api::CFRelease(all);
        IOSurfaceRemoveValue(surface, name);
        assert!(IOSurfaceCopyValue(surface, name).is_null());
        corefoundation::api::CFRelease(surface);
    }
}

#[test]
fn a_surface_is_found_by_its_id_until_it_goes() {
    let surface = bgra(2, 2);
    // SAFETY: a surface this test made.
    unsafe {
        let id = IOSurfaceGetID(surface);
        assert_ne!(id, 0);
        let found = IOSurfaceLookup(id);
        assert_eq!(found, surface);
        corefoundation::api::CFRelease(found);
        corefoundation::api::CFRelease(surface);
        assert!(IOSurfaceLookup(id).is_null());
    }
}

#[test]
fn every_key_is_published_as_a_cell_holding_its_string() {
    let symbols = data_symbols();
    let (_, cell) = symbols
        .iter()
        .find(|(name, _)| name == "_kIOSurfaceBytesPerRow")
        .expect("published");
    // SAFETY: a cell `global_cell` leaked, holding a string.
    let string = unsafe { *(*cell as *const u64) } as CFTypeRef;
    // SAFETY: two CF strings.
    assert_ne!(
        unsafe { corefoundation::api::CFEqual(string, key(BYTES_PER_ROW)) },
        0
    );
}
