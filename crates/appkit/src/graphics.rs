//! Minimal image-backed CoreGraphics contexts used by Qt's Cocoa font probe.

#![allow(non_snake_case)]

use std::ffi::c_void;

use corefoundation::base::CFTypeRef;

#[repr(C)]
struct ColorSpace {
    isa: *const c_void,
}

#[repr(C)]
struct BitmapContext {
    isa: *const c_void,
    data: *mut c_void,
    width: usize,
    height: usize,
    bytes_per_row: usize,
}

pub fn install() {
    corefoundation::base::register(c"__CGColorSpace", None, size_of::<ColorSpace>(), true);
    corefoundation::base::register(c"__CGContext", None, size_of::<BitmapContext>(), true);
}

pub unsafe extern "C" fn CGColorSpaceCreateWithName(_name: CFTypeRef) -> CFTypeRef {
    let class = objc::class::lookup(c"__CGColorSpace").expect("installed during bootstrap");
    // SAFETY: `ColorSpace` is the registered class layout.
    unsafe { corefoundation::base::allocate::<ColorSpace>(class) as CFTypeRef }
}

pub unsafe extern "C" fn CGColorSpaceCreateDeviceRGB() -> CFTypeRef {
    unsafe { CGColorSpaceCreateWithName(std::ptr::null()) }
}

pub unsafe extern "C" fn CGColorSpaceRelease(space: CFTypeRef) {
    if !space.is_null() {
        unsafe { corefoundation::api::CFRelease(space) };
    }
}

pub unsafe extern "C" fn CGBitmapContextCreate(
    data: *mut c_void,
    width: usize,
    height: usize,
    _bits_per_component: usize,
    bytes_per_row: usize,
    _space: CFTypeRef,
    _bitmap_info: u32,
) -> CFTypeRef {
    if width == 0 || height == 0 || data.is_null() {
        return std::ptr::null();
    }
    let class = objc::class::lookup(c"__CGContext").expect("installed during bootstrap");
    // SAFETY: `BitmapContext` is the registered class layout.
    let context = unsafe { corefoundation::base::allocate::<BitmapContext>(class) };
    // SAFETY: this is a fresh allocation and all stored pointers remain caller-owned.
    unsafe {
        (*context).data = data;
        (*context).width = width;
        (*context).height = height;
        (*context).bytes_per_row = bytes_per_row;
    }
    context as CFTypeRef
}

pub unsafe extern "C" fn CGContextRelease(context: CFTypeRef) {
    if !context.is_null() {
        unsafe { corefoundation::api::CFRelease(context) };
    }
}

pub unsafe extern "C" fn CGContextTranslateCTM(_context: CFTypeRef, _x: f64, _y: f64) {}

pub unsafe extern "C" fn CGContextScaleCTM(_context: CFTypeRef, _x: f64, _y: f64) {}

pub unsafe extern "C" fn CGContextSetTextDrawingMode(_context: CFTypeRef, _mode: i32) {}

pub unsafe extern "C" fn CGContextSetGrayFillColor(_context: CFTypeRef, _gray: f64, _alpha: f64) {}

pub unsafe extern "C" fn CGContextSetShouldSmoothFonts(_context: CFTypeRef, _smooth: u8) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_context_wraps_nonempty_caller_storage() {
        crate::bootstrap();
        let mut pixels = [0_u32; 4];
        // SAFETY: `pixels` is writable for the described two rows.
        unsafe {
            let space = CGColorSpaceCreateDeviceRGB();
            let context = CGBitmapContextCreate(pixels.as_mut_ptr().cast(), 2, 2, 8, 8, space, 0);
            assert!(!context.is_null());
            CGContextTranslateCTM(context, 0.0, 2.0);
            CGContextScaleCTM(context, 1.0, -1.0);
            CGContextRelease(context);
            CGColorSpaceRelease(space);
        }
    }
}
