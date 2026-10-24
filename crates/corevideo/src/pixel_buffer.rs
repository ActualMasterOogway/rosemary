//! CPU-backed CoreVideo pixel buffers.
//!
//! The storage is owned by a CF/Objective-C object so CFRelease, CVBufferRelease
//! and CVPixelBufferRelease all participate in the same reference count.

#![allow(non_snake_case)]

use std::alloc::{Layout, alloc_zeroed, dealloc as free_storage};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Mutex, OnceLock};

use corefoundation::base::CFTypeRef;
use objc::class::Class;
use objc::object::Id;

const SUCCESS: i32 = 0;
const INVALID_ARGUMENT: i32 = -6661;
const ALLOCATION_FAILED: i32 = -6662;
const UNSUPPORTED: i32 = -6663;
const INVALID_PIXEL_FORMAT: i32 = -6680;
const INVALID_SIZE: i32 = -6681;
const INVALID_ATTRIBUTES: i32 = -6682;

const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
const NV12_VIDEO: u32 = u32::from_be_bytes(*b"420v");
const NV12_FULL: u32 = u32::from_be_bytes(*b"420f");
const I420_VIDEO: u32 = u32::from_be_bytes(*b"y420");
const I420_FULL: u32 = u32::from_be_bytes(*b"f420");
const READ_ONLY: u64 = 1;
const DEFAULT_ALIGNMENT: usize = 64;
const MAX_STORAGE: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Plane {
    offset: usize,
    width: usize,
    height: usize,
    row_bytes: usize,
}

#[derive(Default)]
struct Locks {
    readers: usize,
    writers: usize,
}

struct Storage {
    // The logical base may be offset into the heap allocation when an
    // attribute requires a non-power-of-two address multiple.
    ptr: NonNull<u8>,
    allocation: NonNull<u8>,
    layout: Layout,
}

impl Storage {
    fn new(size: usize, alignment: usize) -> Result<Self, i32> {
        let total = size.checked_add(alignment - 1).ok_or(ALLOCATION_FAILED)?;
        if total > MAX_STORAGE {
            return Err(ALLOCATION_FAILED);
        }
        let layout =
            Layout::from_size_align(total, DEFAULT_ALIGNMENT).map_err(|_| ALLOCATION_FAILED)?;
        // SAFETY: nonzero, checked length and valid Layout alignment.
        let allocation = NonNull::new(unsafe { alloc_zeroed(layout) }).ok_or(ALLOCATION_FAILED)?;
        let Some(base) = aligned(allocation.as_ptr() as usize, alignment) else {
            // SAFETY: the allocation was obtained with this exact Layout.
            unsafe { free_storage(allocation.as_ptr(), layout) };
            return Err(ALLOCATION_FAILED);
        };
        // SAFETY: overallocated by alignment - 1 bytes and the aligned region
        // is exactly size bytes long, entirely within the heap allocation.
        let ptr = unsafe {
            NonNull::new_unchecked(allocation.as_ptr().add(base - allocation.as_ptr() as usize))
        };
        Ok(Self {
            ptr,
            allocation,
            layout,
        })
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        // SAFETY: allocation and Layout are unchanged since allocation.
        unsafe { free_storage(self.allocation.as_ptr(), self.layout) };
    }
}

struct State {
    width: usize,
    height: usize,
    format: u32,
    planes: Vec<Plane>,
    row_bytes: usize,
    // Never resized: C clients retain pointers to its pixels until final release.
    data: Storage,
    data_size: usize,
    locks: Mutex<Locks>,
}

#[repr(C)]
struct Buffer {
    isa: *const c_void,
    state: *mut State,
}

fn class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let addr = *CLASS.get_or_init(|| {
        corefoundation::bootstrap();
        let class =
            corefoundation::base::register(c"__NSCVPixelBuffer", None, size_of::<Buffer>(), true);
        class.replace_method(
            objc::sel::get("dealloc"),
            dealloc as *const c_void,
            c"v16@0:8",
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: the registered class lives for the process.
    unsafe { &*(addr as *const Class) }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: objc::sel::Sel) {
    // SAFETY: this instance has a State allocated with Box::into_raw at creation.
    let buffer = receiver as *mut Buffer;
    unsafe {
        let state = std::mem::replace(&mut (*buffer).state, std::ptr::null_mut());
        if !state.is_null() {
            drop(Box::from_raw(state));
        }
        objc::arc::finalize(receiver);
    }
}

fn state<'a>(buffer: CFTypeRef) -> Option<&'a State> {
    // SAFETY: caller holds a live CF object; check its class before inspecting it.
    if buffer.is_null() || !unsafe { corefoundation::base::is_kind_of(buffer, class()) } {
        return None;
    }
    // SAFETY: Buffer layout and State pointer are initialized by CVPixelBufferCreate.
    unsafe { (*(buffer as *const Buffer)).state.as_ref() }
}

fn aligned(value: usize, alignment: usize) -> Option<usize> {
    let remainder = value % alignment;
    value.checked_add(if remainder == 0 {
        0
    } else {
        alignment - remainder
    })
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn lcm(a: usize, b: usize) -> Option<usize> {
    (a / gcd(a, b)).checked_mul(b)
}

fn key(text: &str) -> CFTypeRef {
    corefoundation::base::interned_name(text) as usize as CFTypeRef
}

fn attribute(attrs: CFTypeRef, name: &str) -> CFTypeRef {
    if attrs.is_null() {
        return std::ptr::null();
    }
    // SAFETY: the caller provided a CFDictionary, and the key is a CFString.
    unsafe { corefoundation::api::CFDictionaryGetValue(attrs, key(name)) }
}

fn alignment_attribute(attrs: CFTypeRef, name: &str) -> Result<usize, i32> {
    let value = attribute(attrs, name);
    // The CoreVideo allocator reads these CFNumbers as integer multiples:
    // floating values truncate toward zero, and zero selects the default.
    // Non-numeric CF values also select that default on macOS.
    let number = if value.is_null() {
        None
    } else {
        // SAFETY: the guest supplied a CF value from the attributes dictionary.
        unsafe { corefoundation::collections::number(value) }
    };
    let multiple = match number {
        Some(corefoundation::collections::Number::Integer(n)) => n,
        Some(corefoundation::collections::Number::Float(n)) => {
            if n.is_infinite() {
                return Err(ALLOCATION_FAILED);
            }
            n as i64
        }
        None => 0,
    };
    if multiple < 0 {
        return Err(ALLOCATION_FAILED);
    }
    if multiple == 0 {
        return Ok(DEFAULT_ALIGNMENT);
    }
    usize::try_from(multiple).map_err(|_| ALLOCATION_FAILED)
}

fn requires_unsupported_backing(attrs: CFTypeRef) -> Result<bool, i32> {
    let iosurface = attribute(attrs, "IOSurfaceProperties");
    if !iosurface.is_null() {
        // A requested IOSurface must be described by a CFDictionary, even
        // though this CPU-only implementation cannot provide that backing.
        if unsafe { corefoundation::collections::dictionary(iosurface) }.is_none() {
            return Err(INVALID_ATTRIBUTES);
        }
        return Ok(true);
    }
    for name in [
        "OpenGLCompatibility",
        "OpenGLESCompatibility",
        "MetalCompatibility",
        "OpenGLTextureCacheCompatibility",
        "OpenGLESTextureCacheCompatibility",
        "IOSurfaceCoreAnimationCompatibility",
        "IOSurfaceOpenGLFBOCompatibility",
        "IOSurfaceOpenGLESFBOCompatibility",
        "IOSurfaceOpenGLTextureCompatibility",
        "IOSurfaceOpenGLESTextureCompatibility",
        "IOSurfacePurgeable",
    ] {
        let value = attribute(attrs, name);
        if !value.is_null() {
            // SAFETY: the guest supplied a CF object.
            if unsafe { corefoundation::api::CFGetTypeID(value) }
                != unsafe { corefoundation::api::CFBooleanGetTypeID() }
            {
                return Err(INVALID_ATTRIBUTES);
            }
            // SAFETY: checked for CFBoolean above.
            if unsafe { corefoundation::api::CFBooleanGetValue(value) } != 0 {
                return Ok(true);
            }
        }
    }
    // CPU-backed BGRA and YCbCr planes can be used by software CoreGraphics
    // consumers; these flags are not a request for GPU/IOSurface storage.
    for name in ["CGBitmapContextCompatibility", "CGImageCompatibility"] {
        let value = attribute(attrs, name);
        if !value.is_null()
            && unsafe { corefoundation::api::CFGetTypeID(value) }
                != unsafe { corefoundation::api::CFBooleanGetTypeID() }
        {
            return Err(INVALID_ATTRIBUTES);
        }
    }
    for name in [
        "ExtendedPixelsLeft",
        "ExtendedPixelsTop",
        "ExtendedPixelsRight",
        "ExtendedPixelsBottom",
    ] {
        let value = attribute(attrs, name);
        if !value.is_null() {
            // SAFETY: the guest supplied an attribute CFNumber.
            let n =
                unsafe { corefoundation::collections::number(value) }.ok_or(INVALID_ATTRIBUTES)?;
            let corefoundation::collections::Number::Integer(n) = n else {
                return Err(INVALID_ATTRIBUTES);
            };
            if n < 0 {
                return Err(INVALID_ATTRIBUTES);
            }
            if n != 0 {
                return Ok(true);
            }
        }
    }
    let allocator = attribute(attrs, "MemoryAllocator");
    if !allocator.is_null() && !corefoundation::api::supports_malloc_allocator(allocator.cast()) {
        // Nonstandard callback allocators are represented by CF objects, but
        // their allocation callbacks are not implemented in CoreFoundation.
        if unsafe { corefoundation::api::CFGetTypeID(allocator) }
            != unsafe { corefoundation::api::CFAllocatorGetTypeID() }
        {
            return Err(INVALID_ATTRIBUTES);
        }
        return Ok(true);
    }
    Ok(false)
}

fn layout(
    width: usize,
    height: usize,
    format: u32,
    attrs: CFTypeRef,
) -> Result<(Vec<Plane>, usize, usize, usize), i32> {
    if width == 0 || height == 0 {
        return Err(INVALID_SIZE);
    }
    if !matches!(
        format,
        BGRA | NV12_VIDEO | NV12_FULL | I420_VIDEO | I420_FULL
    ) {
        return Err(INVALID_PIXEL_FORMAT);
    }
    let row_alignment = lcm(
        DEFAULT_ALIGNMENT,
        alignment_attribute(attrs, "BytesPerRowAlignment")?,
    )
    .ok_or(ALLOCATION_FAILED)?;
    let plane_alignment = if format == BGRA {
        DEFAULT_ALIGNMENT
    } else {
        alignment_attribute(attrs, "PlaneAlignment")?
    };
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    let dimensions: Vec<(usize, usize, usize)> = match format {
        BGRA => vec![(width, height, 4)],
        NV12_VIDEO | NV12_FULL => vec![(width, height, 1), (chroma_width, chroma_height, 2)],
        _ => vec![
            (width, height, 1),
            (chroma_width, chroma_height, 1),
            (chroma_width, chroma_height, 1),
        ],
    };
    let planar = dimensions.len() > 1;
    let header_len = if planar { dimensions.len() * 8 } else { 0 };
    let mut offset = aligned(header_len, plane_alignment).ok_or(INVALID_SIZE)?;
    let mut planes = Vec::with_capacity(dimensions.len());
    for (plane_width, plane_height, bytes_per_pixel) in dimensions {
        let minimum = plane_width
            .checked_mul(bytes_per_pixel)
            .ok_or(INVALID_SIZE)?;
        let row_bytes = aligned(minimum, row_alignment).ok_or(INVALID_SIZE)?;
        let len = row_bytes.checked_mul(plane_height).ok_or(INVALID_SIZE)?;
        offset = aligned(offset, plane_alignment).ok_or(INVALID_SIZE)?;
        if offset > i32::MAX as usize || row_bytes > u32::MAX as usize {
            return Err(INVALID_SIZE);
        }
        planes.push(Plane {
            offset,
            width: plane_width,
            height: plane_height,
            row_bytes,
        });
        offset = offset.checked_add(len).ok_or(INVALID_SIZE)?;
    }
    if offset > MAX_STORAGE {
        // Valid dimensions may exceed this implementation's allocation cap.
        // Report memory exhaustion, not malformed dimensions.
        return Err(ALLOCATION_FAILED);
    }
    // For planar buffers Apple's rowBytes is an envelope covering all planes.
    let row_bytes = if planar {
        offset.div_ceil(height)
    } else {
        planes[0].row_bytes
    };
    // Pointer alignment and row-byte divisibility are different contracts.
    // The base must align to each requested plane multiple while retaining
    // the runtime's minimum 64-byte backing alignment.
    let address_alignment = lcm(DEFAULT_ALIGNMENT, row_alignment)
        .and_then(|a| lcm(a, plane_alignment))
        .ok_or(ALLOCATION_FAILED)?;
    Ok((planes, row_bytes, offset, address_alignment))
}

/// CVReturn CVPixelBufferCreate(CFAllocatorRef, size_t, size_t, OSType,
/// CFDictionaryRef, CVPixelBufferRef *).
///
/// # Safety
/// Attributes must be a CFDictionary or NULL; out must be writable.
pub unsafe extern "C" fn CVPixelBufferCreate(
    allocator: CFTypeRef,
    width: usize,
    height: usize,
    format: u32,
    attrs: CFTypeRef,
    out: *mut CFTypeRef,
) -> i32 {
    if out.is_null() {
        return INVALID_ARGUMENT;
    }
    // SAFETY: caller supplied a writable pointer.
    unsafe { *out = std::ptr::null() };
    if !corefoundation::api::supports_malloc_allocator(allocator.cast()) {
        return UNSUPPORTED;
    }
    if !attrs.is_null() && unsafe { corefoundation::collections::dictionary(attrs) }.is_none() {
        return INVALID_ATTRIBUTES;
    }
    match requires_unsupported_backing(attrs) {
        Ok(true) => return UNSUPPORTED,
        Err(error) => return error,
        Ok(false) => {}
    }
    let (planes, row_bytes, size, storage_alignment) = match layout(width, height, format, attrs) {
        Ok(layout) => layout,
        Err(error) => return error,
    };
    let data = match Storage::new(size, storage_alignment) {
        Ok(data) => data,
        Err(error) => return error,
    };
    if planes.len() > 1 {
        for (i, plane) in planes.iter().enumerate() {
            // CVPlanarComponentInfo is (int32_t offset, uint32_t rowBytes), BE.
            let base = i * 8;
            // SAFETY: descriptor is inside the checked allocation.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (plane.offset as i32).to_be_bytes().as_ptr(),
                    data.ptr.as_ptr().add(base),
                    4,
                );
                std::ptr::copy_nonoverlapping(
                    (plane.row_bytes as u32).to_be_bytes().as_ptr(),
                    data.ptr.as_ptr().add(base + 4),
                    4,
                );
            }
        }
    }
    // SAFETY: Buffer has the exact registered class layout, and owns its State.
    let buffer = unsafe { corefoundation::base::allocate::<Buffer>(class()) };
    if buffer.is_null() {
        return ALLOCATION_FAILED;
    }
    let state = Box::new(State {
        width,
        height,
        format,
        planes,
        row_bytes,
        data,
        data_size: size,
        locks: Mutex::new(Locks::default()),
    });
    // SAFETY: fresh object, fully initialized before publication.
    unsafe {
        (*buffer).state = Box::into_raw(state);
        *out = buffer as CFTypeRef;
    }
    SUCCESS
}

/// CFTypeRef CVBufferRetain(CVBufferRef). Shares CoreFoundation ownership.
pub unsafe extern "C" fn CVBufferRetain(buffer: CFTypeRef) -> CFTypeRef {
    if buffer.is_null() {
        return buffer;
    }
    // SAFETY: a live CF-backed CVBuffer passed by the caller.
    unsafe { corefoundation::api::CFRetain(buffer) }
}

pub unsafe extern "C" fn CVBufferRelease(buffer: CFTypeRef) {
    if !buffer.is_null() {
        // SAFETY: a live CF-backed CVBuffer passed by the caller.
        unsafe { corefoundation::api::CFRelease(buffer) };
    }
}

pub unsafe extern "C" fn CVPixelBufferRetain(buffer: CFTypeRef) -> CFTypeRef {
    unsafe { CVBufferRetain(buffer) }
}

pub unsafe extern "C" fn CVPixelBufferRelease(buffer: CFTypeRef) {
    unsafe { CVBufferRelease(buffer) }
}

pub unsafe extern "C" fn CVPixelBufferGetTypeID() -> usize {
    corefoundation::base::type_id(class())
}

pub unsafe extern "C" fn CVPixelBufferGetWidth(buffer: CFTypeRef) -> usize {
    state(buffer).map_or(0, |s| s.width)
}

pub unsafe extern "C" fn CVPixelBufferGetHeight(buffer: CFTypeRef) -> usize {
    state(buffer).map_or(0, |s| s.height)
}

pub unsafe extern "C" fn CVPixelBufferGetPixelFormatType(buffer: CFTypeRef) -> u32 {
    state(buffer).map_or(0, |s| s.format)
}

pub unsafe extern "C" fn CVPixelBufferIsPlanar(buffer: CFTypeRef) -> u8 {
    state(buffer).is_some_and(|s| s.planes.len() > 1) as u8
}

pub unsafe extern "C" fn CVPixelBufferGetPlaneCount(buffer: CFTypeRef) -> usize {
    state(buffer).map_or(0, |s| {
        if s.planes.len() > 1 {
            s.planes.len()
        } else {
            0
        }
    })
}

pub unsafe extern "C" fn CVPixelBufferGetBytesPerRow(buffer: CFTypeRef) -> usize {
    state(buffer).map_or(0, |s| s.row_bytes)
}

pub unsafe extern "C" fn CVPixelBufferGetDataSize(buffer: CFTypeRef) -> usize {
    state(buffer).map_or(0, |s| s.data_size)
}

pub unsafe extern "C" fn CVPixelBufferGetWidthOfPlane(buffer: CFTypeRef, i: usize) -> usize {
    state(buffer)
        .and_then(|s| (s.planes.len() > 1).then(|| s.planes.get(i)).flatten())
        .map_or(0, |p| p.width)
}

pub unsafe extern "C" fn CVPixelBufferGetHeightOfPlane(buffer: CFTypeRef, i: usize) -> usize {
    state(buffer)
        .and_then(|s| (s.planes.len() > 1).then(|| s.planes.get(i)).flatten())
        .map_or(0, |p| p.height)
}

pub unsafe extern "C" fn CVPixelBufferGetBytesPerRowOfPlane(buffer: CFTypeRef, i: usize) -> usize {
    state(buffer)
        .and_then(|s| (s.planes.len() > 1).then(|| s.planes.get(i)).flatten())
        .map_or(0, |p| p.row_bytes)
}

fn base_address(buffer: CFTypeRef, plane: Option<usize>) -> *mut c_void {
    let Some(state) = state(buffer) else {
        return std::ptr::null_mut();
    };
    // The storage pointer is stable; callers must synchronize pixel access.
    let ptr = state.data.ptr.as_ptr();
    match plane {
        None => ptr.cast(),
        Some(i) if state.planes.len() > 1 => state
            .planes
            .get(i)
            .map_or(std::ptr::null_mut(), |p| unsafe {
                ptr.add(p.offset).cast()
            }),
        Some(_) => std::ptr::null_mut(),
    }
}

pub unsafe extern "C" fn CVPixelBufferGetBaseAddress(buffer: CFTypeRef) -> *mut c_void {
    base_address(buffer, None)
}

pub unsafe extern "C" fn CVPixelBufferGetBaseAddressOfPlane(
    buffer: CFTypeRef,
    i: usize,
) -> *mut c_void {
    base_address(buffer, Some(i))
}

/// Lock flags: 0 means CPU writing, 1 means read only; unlock must match.
pub unsafe extern "C" fn CVPixelBufferLockBaseAddress(buffer: CFTypeRef, flags: u64) -> i32 {
    if flags & !READ_ONLY != 0 {
        return INVALID_ARGUMENT;
    }
    let Some(state) = state(buffer) else {
        return INVALID_ARGUMENT;
    };
    let mut locks = state.locks.lock().unwrap_or_else(|e| e.into_inner());
    if flags == READ_ONLY {
        let Some(n) = locks.readers.checked_add(1) else {
            return INVALID_ARGUMENT;
        };
        locks.readers = n;
    } else {
        let Some(n) = locks.writers.checked_add(1) else {
            return INVALID_ARGUMENT;
        };
        locks.writers = n;
    }
    SUCCESS
}

pub unsafe extern "C" fn CVPixelBufferUnlockBaseAddress(buffer: CFTypeRef, flags: u64) -> i32 {
    if flags & !READ_ONLY != 0 {
        return INVALID_ARGUMENT;
    }
    let Some(state) = state(buffer) else {
        return INVALID_ARGUMENT;
    };
    let mut locks = state.locks.lock().unwrap_or_else(|e| e.into_inner());
    let count = if flags == READ_ONLY {
        &mut locks.readers
    } else {
        &mut locks.writers
    };
    if *count == 0 {
        return INVALID_ARGUMENT;
    }
    *count -= 1;
    SUCCESS
}

/// A CPU-allocated CVPixelBuffer has no IOSurface backing.
pub unsafe extern "C" fn CVPixelBufferGetIOSurface(_buffer: CFTypeRef) -> CFTypeRef {
    std::ptr::null()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(w: usize, h: usize, format: u32) -> CFTypeRef {
        let mut out = std::ptr::null();
        assert_eq!(
            unsafe {
                CVPixelBufferCreate(std::ptr::null(), w, h, format, std::ptr::null(), &mut out)
            },
            SUCCESS
        );
        assert!(!out.is_null());
        out
    }

    #[test]
    fn bgra_stride_lock_and_cf_ownership() {
        let b = create(7, 5, BGRA);
        unsafe {
            assert_eq!(CVPixelBufferGetWidth(b), 7);
            assert_eq!(CVPixelBufferGetHeight(b), 5);
            assert_eq!(CVPixelBufferGetPixelFormatType(b), BGRA);
            assert_eq!(CVPixelBufferGetBytesPerRow(b), 64);
            assert_eq!(CVPixelBufferGetDataSize(b), 320);
            assert_eq!(CVPixelBufferGetPlaneCount(b), 0);
            assert_eq!(CVPixelBufferIsPlanar(b), 0);
            assert_eq!(
                CVPixelBufferGetBaseAddressOfPlane(b, 0),
                std::ptr::null_mut()
            );
            assert_eq!(
                corefoundation::api::CFGetTypeID(b),
                CVPixelBufferGetTypeID()
            );
            assert_eq!(CVPixelBufferLockBaseAddress(b, 0), SUCCESS);
            let ptr = CVPixelBufferGetBaseAddress(b).cast::<u8>();
            *ptr = 37;
            assert_eq!(CVPixelBufferUnlockBaseAddress(b, 1), INVALID_ARGUMENT);
            assert_eq!(*ptr, 37);
            assert_eq!(CVPixelBufferUnlockBaseAddress(b, 0), SUCCESS);
            assert_eq!(CVPixelBufferUnlockBaseAddress(b, 0), INVALID_ARGUMENT);
            assert_eq!(CVPixelBufferLockBaseAddress(b, 2), INVALID_ARGUMENT);
            let retained = CVBufferRetain(b);
            assert_eq!(retained, b);
            CVPixelBufferRelease(b);
            assert_eq!(CVPixelBufferGetWidth(retained), 7);
            CVBufferRelease(retained);
        }
    }

    #[test]
    fn planar_offsets_and_dimensions_are_consistent() {
        for (fmt, expected) in [
            (NV12_VIDEO, 2),
            (NV12_FULL, 2),
            (I420_VIDEO, 3),
            (I420_FULL, 3),
        ] {
            let b = create(7, 5, fmt);
            unsafe {
                assert_eq!(CVPixelBufferIsPlanar(b), 1);
                assert_eq!(CVPixelBufferGetPlaneCount(b), expected);
                assert_eq!(CVPixelBufferLockBaseAddress(b, READ_ONLY), SUCCESS);
                let base = CVPixelBufferGetBaseAddress(b).cast::<u8>();
                assert!(!base.is_null());
                for i in 0..expected {
                    let plane = CVPixelBufferGetBaseAddressOfPlane(b, i);
                    assert!(!plane.is_null());
                    let offset = u32::from_be_bytes(
                        std::slice::from_raw_parts(base.add(i * 8), 4)
                            .try_into()
                            .unwrap(),
                    ) as usize;
                    let stride = u32::from_be_bytes(
                        std::slice::from_raw_parts(base.add(i * 8 + 4), 4)
                            .try_into()
                            .unwrap(),
                    ) as usize;
                    assert_eq!(offset, plane as usize - base as usize);
                    assert_eq!(stride, CVPixelBufferGetBytesPerRowOfPlane(b, i));
                    assert_eq!(
                        CVPixelBufferGetWidthOfPlane(b, i),
                        if i == 0 { 7 } else { 4 }
                    );
                    assert_eq!(
                        CVPixelBufferGetHeightOfPlane(b, i),
                        if i == 0 { 5 } else { 3 }
                    );
                    assert!(
                        offset + stride * CVPixelBufferGetHeightOfPlane(b, i)
                            <= CVPixelBufferGetDataSize(b)
                    );
                }
                assert_eq!(
                    CVPixelBufferGetBaseAddressOfPlane(b, expected),
                    std::ptr::null_mut()
                );
                assert_eq!(CVPixelBufferUnlockBaseAddress(b, 0), INVALID_ARGUMENT);
                assert_eq!(CVPixelBufferUnlockBaseAddress(b, READ_ONLY), SUCCESS);
                CVPixelBufferRelease(b);
            }
        }
    }

    #[test]
    fn creation_refuses_invalid_sizes_formats_and_requests() {
        unsafe {
            let mut out = 1usize as CFTypeRef;
            assert_eq!(
                CVPixelBufferCreate(std::ptr::null(), 0, 5, BGRA, std::ptr::null(), &mut out),
                INVALID_SIZE
            );
            assert!(out.is_null());
            assert_eq!(
                CVPixelBufferCreate(
                    std::ptr::null(),
                    usize::MAX,
                    1,
                    BGRA,
                    std::ptr::null(),
                    &mut out
                ),
                INVALID_SIZE
            );
            assert_eq!(
                CVPixelBufferCreate(
                    std::ptr::null(),
                    2,
                    2,
                    0x12345678,
                    std::ptr::null(),
                    &mut out
                ),
                INVALID_PIXEL_FORMAT
            );
            assert_eq!(
                CVPixelBufferCreate(
                    std::ptr::null(),
                    2,
                    2,
                    BGRA,
                    std::ptr::null(),
                    std::ptr::null_mut()
                ),
                INVALID_ARGUMENT
            );
            assert_eq!(
                CVPixelBufferLockBaseAddress(std::ptr::null(), 0),
                INVALID_ARGUMENT
            );
        }
    }

    #[test]
    fn requested_alignments_and_unsupported_backings() {
        use corefoundation::callbacks::Policy;
        use corefoundation::collections::number_type;

        fn attrs(name: &str, value: CFTypeRef) -> CFTypeRef {
            let policy = Policy::from_type_callbacks();
            corefoundation::collections::new_dictionary(
                &[(key(name), value)],
                policy,
                policy,
                false,
            )
        }

        unsafe {
            let alignment: i64 = 128;
            let number = corefoundation::api::CFNumberCreate(
                std::ptr::null(),
                number_type::SINT64,
                (&alignment as *const i64).cast(),
            );
            let dictionary = attrs("BytesPerRowAlignment", number);
            let mut out = std::ptr::null();
            assert_eq!(
                CVPixelBufferCreate(std::ptr::null(), 7, 5, BGRA, dictionary, &mut out),
                SUCCESS
            );
            assert_eq!(CVPixelBufferGetBytesPerRow(out), 128);
            assert_eq!((CVPixelBufferGetBaseAddress(out) as usize) % 128, 0);
            CVPixelBufferRelease(out);
            corefoundation::api::CFRelease(dictionary);
            corefoundation::api::CFRelease(number);

            let alignment: i64 = 256;
            let number = corefoundation::api::CFNumberCreate(
                std::ptr::null(),
                number_type::SINT64,
                (&alignment as *const i64).cast(),
            );
            let dictionary = attrs("PlaneAlignment", number);
            assert_eq!(
                CVPixelBufferCreate(std::ptr::null(), 7, 5, NV12_VIDEO, dictionary, &mut out),
                SUCCESS
            );
            for i in 0..2 {
                assert_eq!(
                    (CVPixelBufferGetBaseAddressOfPlane(out, i) as usize) % 256,
                    0
                );
            }
            CVPixelBufferRelease(out);
            corefoundation::api::CFRelease(dictionary);
            corefoundation::api::CFRelease(number);

            let dictionary = attrs(
                "OpenGLCompatibility",
                corefoundation::collections::boolean(true),
            );
            assert_eq!(
                CVPixelBufferCreate(std::ptr::null(), 7, 5, BGRA, dictionary, &mut out),
                UNSUPPORTED
            );
            assert!(out.is_null());
            corefoundation::api::CFRelease(dictionary);

            let empty = corefoundation::collections::new_dictionary(
                &[],
                Policy::from_type_callbacks(),
                Policy::from_type_callbacks(),
                false,
            );
            let dictionary = attrs("IOSurfaceProperties", empty);
            assert_eq!(
                CVPixelBufferCreate(std::ptr::null(), 7, 5, BGRA, dictionary, &mut out),
                UNSUPPORTED
            );
            corefoundation::api::CFRelease(dictionary);
            corefoundation::api::CFRelease(empty);

            let bogus_attrs_number: i64 = 8;
            let bogus_attrs = corefoundation::api::CFNumberCreate(
                std::ptr::null(),
                number_type::SINT64,
                (&bogus_attrs_number as *const i64).cast(),
            );
            assert_eq!(
                CVPixelBufferCreate(std::ptr::null(), 7, 5, BGRA, bogus_attrs, &mut out),
                INVALID_ATTRIBUTES
            );
            corefoundation::api::CFRelease(bogus_attrs);
        }
    }

    #[test]
    fn narrow_large_dimensions_are_not_artificially_rejected() {
        unsafe {
            for (width, height) in [
                (16_384, 1),
                (16_385, 1),
                (20_000, 1),
                (1, 20_000),
                (1, 32_768),
            ] {
                let mut out = std::ptr::null();
                assert_eq!(
                    CVPixelBufferCreate(
                        std::ptr::null(),
                        width,
                        height,
                        BGRA,
                        std::ptr::null(),
                        &mut out
                    ),
                    SUCCESS,
                    "{width}x{height}"
                );
                assert_eq!(CVPixelBufferGetWidth(out), width);
                assert_eq!(CVPixelBufferGetHeight(out), height);
                CVPixelBufferRelease(out);
            }
            let mut out = std::ptr::null();
            assert_eq!(
                CVPixelBufferCreate(
                    std::ptr::null(),
                    0,
                    20_000,
                    BGRA,
                    std::ptr::null(),
                    &mut out
                ),
                INVALID_SIZE
            );
            assert!(out.is_null());
        }
    }

    #[test]
    fn standard_malloc_allocators_and_software_graphics_flags_are_accepted() {
        use corefoundation::callbacks::Policy;
        let policy = Policy::from_type_callbacks();
        unsafe {
            for kind in [
                corefoundation::api::StandardAllocator::SystemDefault,
                corefoundation::api::StandardAllocator::Malloc,
                corefoundation::api::StandardAllocator::MallocZone,
            ] {
                let allocator = corefoundation::api::standard_allocator(kind);
                assert!(!allocator.is_null());
                let mut out = std::ptr::null();
                assert_eq!(
                    CVPixelBufferCreate(allocator, 7, 5, BGRA, std::ptr::null(), &mut out),
                    SUCCESS
                );
                CVPixelBufferRelease(out);
                let attrs = corefoundation::collections::new_dictionary(
                    &[(key("MemoryAllocator"), allocator.cast())],
                    policy,
                    policy,
                    false,
                );
                assert_eq!(
                    CVPixelBufferCreate(std::ptr::null(), 7, 5, BGRA, attrs, &mut out),
                    SUCCESS
                );
                CVPixelBufferRelease(out);
                corefoundation::api::CFRelease(attrs);
            }
            let mut out = std::ptr::null();
            assert_eq!(
                CVPixelBufferCreate(
                    corefoundation::api::standard_allocator(
                        corefoundation::api::StandardAllocator::Null
                    ),
                    7,
                    5,
                    BGRA,
                    std::ptr::null(),
                    &mut out
                ),
                UNSUPPORTED
            );
            assert!(out.is_null());
            let invalid_number: i64 = 1;
            let wrong_type = corefoundation::api::CFNumberCreate(
                std::ptr::null(),
                corefoundation::collections::number_type::SINT64,
                (&invalid_number as *const i64).cast(),
            );
            for name in ["CGBitmapContextCompatibility", "CGImageCompatibility"] {
                let malformed = corefoundation::collections::new_dictionary(
                    &[(key(name), wrong_type)],
                    policy,
                    policy,
                    false,
                );
                assert_eq!(
                    CVPixelBufferCreate(std::ptr::null(), 7, 5, BGRA, malformed, &mut out,),
                    INVALID_ATTRIBUTES,
                    "{name} malformed type"
                );
                assert!(out.is_null());
                corefoundation::api::CFRelease(malformed);
            }
            corefoundation::api::CFRelease(wrong_type);
            for key_name in ["CGBitmapContextCompatibility", "CGImageCompatibility"] {
                let attrs = corefoundation::collections::new_dictionary(
                    &[(key(key_name), corefoundation::collections::boolean(true))],
                    policy,
                    policy,
                    false,
                );
                for format in [BGRA, NV12_VIDEO, NV12_FULL, I420_VIDEO, I420_FULL] {
                    assert_eq!(
                        CVPixelBufferCreate(std::ptr::null(), 7, 5, format, attrs, &mut out),
                        SUCCESS,
                        "{key_name} {format:#x}"
                    );
                    CVPixelBufferRelease(out);
                }
                corefoundation::api::CFRelease(attrs);
            }
        }
    }

    #[test]
    fn malformed_attributes_and_allocation_limit_fail_without_publishing() {
        use corefoundation::callbacks::Policy;
        use corefoundation::collections::number_type;

        unsafe {
            let mut out = 1usize as CFTypeRef;
            // The dimensions are legal but the allocation exceeds the CPU cap.
            assert_eq!(
                CVPixelBufferCreate(
                    std::ptr::null(),
                    20_000,
                    20_000,
                    BGRA,
                    std::ptr::null(),
                    &mut out,
                ),
                ALLOCATION_FAILED
            );
            assert!(out.is_null());

            let policy = Policy::from_type_callbacks();
            for (alignment, expected) in [(0_i64, SUCCESS), (-1, ALLOCATION_FAILED)] {
                let number = corefoundation::api::CFNumberCreate(
                    std::ptr::null(),
                    number_type::SINT64,
                    (&alignment as *const i64).cast(),
                );
                let attrs = corefoundation::collections::new_dictionary(
                    &[(key("BytesPerRowAlignment"), number)],
                    policy,
                    policy,
                    false,
                );
                out = 1usize as CFTypeRef;
                assert_eq!(
                    CVPixelBufferCreate(std::ptr::null(), 4, 4, BGRA, attrs, &mut out),
                    expected,
                    "alignment={alignment}"
                );
                if expected == SUCCESS {
                    assert_eq!(CVPixelBufferGetBytesPerRow(out), DEFAULT_ALIGNMENT);
                    CVPixelBufferRelease(out);
                } else {
                    assert!(out.is_null());
                }
                corefoundation::api::CFRelease(attrs);
                corefoundation::api::CFRelease(number);
            }

            let not_dictionary = corefoundation::api::CFNumberCreate(
                std::ptr::null(),
                number_type::SINT64,
                (&1_i64 as *const i64).cast(),
            );
            let attrs = corefoundation::collections::new_dictionary(
                &[(key("IOSurfaceProperties"), not_dictionary)],
                policy,
                policy,
                false,
            );
            assert_eq!(
                CVPixelBufferCreate(std::ptr::null(), 4, 4, BGRA, attrs, &mut out),
                INVALID_ATTRIBUTES
            );
            assert!(out.is_null());
            corefoundation::api::CFRelease(attrs);
            corefoundation::api::CFRelease(not_dictionary);
        }
    }

    #[test]
    fn capability_requirements_reject_without_compatible_backing() {
        use corefoundation::callbacks::Policy;
        use corefoundation::collections::number_type;
        let policy = Policy::from_type_callbacks();
        let mut out = std::ptr::null();
        let true_value = corefoundation::collections::boolean(true);
        let false_value = corefoundation::collections::boolean(false);
        unsafe {
            for name in [
                "OpenGLCompatibility",
                "OpenGLESCompatibility",
                "MetalCompatibility",
                "OpenGLTextureCacheCompatibility",
                "OpenGLESTextureCacheCompatibility",
                "IOSurfaceCoreAnimationCompatibility",
                "IOSurfaceOpenGLFBOCompatibility",
                "IOSurfaceOpenGLESFBOCompatibility",
                "IOSurfaceOpenGLTextureCompatibility",
                "IOSurfaceOpenGLESTextureCompatibility",
                "IOSurfacePurgeable",
            ] {
                let required = corefoundation::collections::new_dictionary(
                    &[(key(name), true_value)],
                    policy,
                    policy,
                    false,
                );
                assert_eq!(
                    CVPixelBufferCreate(std::ptr::null(), 7, 5, BGRA, required, &mut out),
                    UNSUPPORTED,
                    "{name}"
                );
                assert!(out.is_null(), "{name}");
                corefoundation::api::CFRelease(required);

                let not_required = corefoundation::collections::new_dictionary(
                    &[(key(name), false_value)],
                    policy,
                    policy,
                    false,
                );
                assert_eq!(
                    CVPixelBufferCreate(std::ptr::null(), 7, 5, BGRA, not_required, &mut out),
                    SUCCESS,
                    "{name} false"
                );
                CVPixelBufferRelease(out);
                corefoundation::api::CFRelease(not_required);
                out = std::ptr::null();
            }
            let custom = corefoundation::api::CFAllocatorCreate(std::ptr::null(), std::ptr::null());
            assert!(!custom.is_null());
            assert_eq!(
                CVPixelBufferCreate(custom, 7, 5, BGRA, std::ptr::null(), &mut out),
                UNSUPPORTED
            );
            assert!(out.is_null());
            let alloc_attrs = corefoundation::collections::new_dictionary(
                &[(key("MemoryAllocator"), custom)],
                policy,
                policy,
                false,
            );
            assert_eq!(
                CVPixelBufferCreate(std::ptr::null(), 7, 5, BGRA, alloc_attrs, &mut out),
                UNSUPPORTED
            );
            assert!(out.is_null());
            corefoundation::api::CFRelease(alloc_attrs);
            corefoundation::api::CFRelease(custom);

            let wrong: i64 = 1;
            let number = corefoundation::api::CFNumberCreate(
                std::ptr::null(),
                number_type::SINT64,
                (&wrong as *const i64).cast(),
            );
            for name in ["MemoryAllocator", "IOSurfaceCoreAnimationCompatibility"] {
                let bad_attrs = corefoundation::collections::new_dictionary(
                    &[(key(name), number)],
                    policy,
                    policy,
                    false,
                );
                assert_eq!(
                    CVPixelBufferCreate(std::ptr::null(), 7, 5, BGRA, bad_attrs, &mut out),
                    INVALID_ATTRIBUTES,
                    "{name} type"
                );
                assert!(out.is_null());
                corefoundation::api::CFRelease(bad_attrs);
            }
            corefoundation::api::CFRelease(number);
        }
    }

    #[test]
    fn arbitrary_positive_row_and_plane_multiples() {
        use corefoundation::callbacks::Policy;
        use corefoundation::collections::number_type;
        let policy = Policy::from_type_callbacks();
        unsafe {
            for (name, multiple) in [
                ("BytesPerRowAlignment", 3_i64),
                ("BytesPerRowAlignment", 24_i64),
                ("BytesPerRowAlignment", 8192_i64),
                ("PlaneAlignment", 3_i64),
                ("PlaneAlignment", 24_i64),
                ("PlaneAlignment", 8192_i64),
            ] {
                let number = corefoundation::api::CFNumberCreate(
                    std::ptr::null(),
                    number_type::SINT64,
                    (&multiple as *const i64).cast(),
                );
                let attrs = corefoundation::collections::new_dictionary(
                    &[(key(name), number)],
                    policy,
                    policy,
                    false,
                );
                let mut out = std::ptr::null();
                assert_eq!(
                    CVPixelBufferCreate(std::ptr::null(), 7, 5, I420_VIDEO, attrs, &mut out),
                    SUCCESS,
                    "{name}={multiple}"
                );
                let base = CVPixelBufferGetBaseAddress(out) as usize;
                for i in 0..3 {
                    let plane = CVPixelBufferGetBaseAddressOfPlane(out, i) as usize;
                    let row_bytes = CVPixelBufferGetBytesPerRowOfPlane(out, i);
                    assert!(plane > base);
                    if name == "PlaneAlignment" {
                        assert_eq!(plane % multiple as usize, 0);
                    } else {
                        assert_eq!(row_bytes % multiple as usize, 0);
                    }
                    assert!(
                        plane - base + row_bytes * CVPixelBufferGetHeightOfPlane(out, i)
                            <= CVPixelBufferGetDataSize(out)
                    );
                }
                CVPixelBufferRelease(out);
                corefoundation::api::CFRelease(attrs);
                corefoundation::api::CFRelease(number);
            }
        }
    }

    #[test]
    fn numeric_alignment_attributes_match_macos_conversions() {
        use corefoundation::callbacks::Policy;
        use corefoundation::collections::number_type;
        let policy = Policy::from_type_callbacks();
        unsafe {
            // Match native CoreVideo's CFNumber-to-integer truncation and its
            // 64-byte row minimum for 7x5 packed and planar buffers.
            for (name, format, numeric, expected_status, expected_stride) in [
                ("BytesPerRowAlignment", BGRA, 0.0_f64, SUCCESS, 64),
                ("BytesPerRowAlignment", BGRA, 0.5, SUCCESS, 64),
                ("BytesPerRowAlignment", BGRA, -0.5, SUCCESS, 64),
                ("BytesPerRowAlignment", BGRA, 1.9, SUCCESS, 64),
                ("BytesPerRowAlignment", BGRA, 3.5, SUCCESS, 192),
                ("BytesPerRowAlignment", BGRA, 63.9, SUCCESS, 4032),
                ("BytesPerRowAlignment", BGRA, 64.0, SUCCESS, 64),
                ("BytesPerRowAlignment", BGRA, 65.1, SUCCESS, 4160),
                ("BytesPerRowAlignment", BGRA, -1.0, ALLOCATION_FAILED, 0),
                ("BytesPerRowAlignment", BGRA, -1.5, ALLOCATION_FAILED, 0),
                ("BytesPerRowAlignment", NV12_VIDEO, 0.0, SUCCESS, 64),
                ("BytesPerRowAlignment", NV12_VIDEO, 3.5, SUCCESS, 192),
                (
                    "BytesPerRowAlignment",
                    NV12_VIDEO,
                    -1.0,
                    ALLOCATION_FAILED,
                    0,
                ),
                ("PlaneAlignment", NV12_VIDEO, 0.0, SUCCESS, 64),
                ("PlaneAlignment", NV12_VIDEO, 3.5, SUCCESS, 64),
                ("PlaneAlignment", NV12_VIDEO, -0.5, SUCCESS, 64),
                ("PlaneAlignment", NV12_VIDEO, -1.0, ALLOCATION_FAILED, 0),
                ("PlaneAlignment", NV12_VIDEO, -1.5, ALLOCATION_FAILED, 0),
                // Packed formats do not have per-plane alignment requirements.
                ("PlaneAlignment", BGRA, -1.0, SUCCESS, 64),
            ] {
                let value = corefoundation::api::CFNumberCreate(
                    std::ptr::null(),
                    number_type::FLOAT64,
                    (&numeric as *const f64).cast(),
                );
                let attrs = corefoundation::collections::new_dictionary(
                    &[(key(name), value)],
                    policy,
                    policy,
                    false,
                );
                let mut out = 1usize as CFTypeRef;
                assert_eq!(
                    CVPixelBufferCreate(std::ptr::null(), 7, 5, format, attrs, &mut out),
                    expected_status,
                    "{name}={numeric} format={format:#x}",
                );
                if expected_status == SUCCESS {
                    let stride = if format == BGRA {
                        CVPixelBufferGetBytesPerRow(out)
                    } else {
                        CVPixelBufferGetBytesPerRowOfPlane(out, 0)
                    };
                    assert_eq!(
                        stride, expected_stride,
                        "{name}={numeric} format={format:#x}"
                    );
                    CVPixelBufferRelease(out);
                } else {
                    assert!(out.is_null(), "{name}={numeric} format={format:#x}");
                }
                corefoundation::api::CFRelease(attrs);
                corefoundation::api::CFRelease(value);
            }
            // An integer CFNumber uses the same zero and negative CVReturn
            // rules for both alignment keys on planar buffers.
            for (name, multiple, expected) in [
                ("BytesPerRowAlignment", 0_i64, SUCCESS),
                ("PlaneAlignment", 0_i64, SUCCESS),
                ("BytesPerRowAlignment", -1_i64, ALLOCATION_FAILED),
                ("PlaneAlignment", -1_i64, ALLOCATION_FAILED),
            ] {
                let value = corefoundation::api::CFNumberCreate(
                    std::ptr::null(),
                    number_type::SINT64,
                    (&multiple as *const i64).cast(),
                );
                let attrs = corefoundation::collections::new_dictionary(
                    &[(key(name), value)],
                    policy,
                    policy,
                    false,
                );
                let mut out = 1usize as CFTypeRef;
                assert_eq!(
                    CVPixelBufferCreate(std::ptr::null(), 7, 5, NV12_VIDEO, attrs, &mut out),
                    expected,
                    "{name}={multiple}"
                );
                if expected == SUCCESS {
                    assert_eq!(CVPixelBufferGetBytesPerRowOfPlane(out, 0), 64);
                    CVPixelBufferRelease(out);
                } else {
                    assert!(out.is_null());
                }
                corefoundation::api::CFRelease(attrs);
                corefoundation::api::CFRelease(value);
            }
        }
    }

    #[test]
    fn row_alignment_preserves_corevideo_minimum_stride() {
        use corefoundation::callbacks::Policy;
        use corefoundation::collections::number_type;
        let policy = Policy::from_type_callbacks();
        unsafe {
            for (multiple, expected) in [
                (1_i64, 64_usize),
                (3, 192),
                (24, 192),
                (8192, 8192),
                (4097, 262208),
            ] {
                let n = corefoundation::api::CFNumberCreate(
                    std::ptr::null(),
                    number_type::SINT64,
                    (&multiple as *const i64).cast(),
                );
                let attrs = corefoundation::collections::new_dictionary(
                    &[(key("BytesPerRowAlignment"), n)],
                    policy,
                    policy,
                    false,
                );
                let mut out = std::ptr::null();
                assert_eq!(
                    CVPixelBufferCreate(std::ptr::null(), 7, 5, BGRA, attrs, &mut out),
                    SUCCESS
                );
                assert_eq!(
                    CVPixelBufferGetBytesPerRow(out),
                    expected,
                    "multiple={multiple}"
                );
                CVPixelBufferRelease(out);
                corefoundation::api::CFRelease(attrs);
                corefoundation::api::CFRelease(n);
            }
        }
    }

    #[test]
    fn repeated_lifetimes_and_planar_pointer_bounds() {
        for _ in 0..128 {
            let b = create(1, 1, I420_VIDEO);
            unsafe {
                assert_eq!(CVPixelBufferGetPlaneCount(b), 3);
                assert_eq!(CVPixelBufferLockBaseAddress(b, 0), SUCCESS);
                for plane in 0..3 {
                    let ptr = CVPixelBufferGetBaseAddressOfPlane(b, plane).cast::<u8>();
                    assert!(!ptr.is_null());
                    assert_eq!(*ptr, 0);
                    *ptr = (plane + 1) as u8;
                }
                assert_eq!(
                    CVPixelBufferGetBaseAddressOfPlane(b, usize::MAX),
                    std::ptr::null_mut()
                );
                assert_eq!(CVPixelBufferGetWidthOfPlane(b, usize::MAX), 0);
                assert_eq!(CVPixelBufferUnlockBaseAddress(b, 0), SUCCESS);
                assert_eq!(CVPixelBufferLockBaseAddress(b, READ_ONLY), SUCCESS);
                for plane in 0..3 {
                    assert_eq!(
                        *CVPixelBufferGetBaseAddressOfPlane(b, plane).cast::<u8>(),
                        (plane + 1) as u8
                    );
                }
                assert_eq!(CVPixelBufferUnlockBaseAddress(b, READ_ONLY), SUCCESS);
                let retain = corefoundation::api::CFRetain(b);
                CVPixelBufferRelease(b);
                assert_eq!(CVPixelBufferGetDataSize(retain), 256);
                corefoundation::api::CFRelease(retain);
            }
        }
    }

    #[test]
    fn concurrent_retention_and_read_only_locks() {
        let b = create(2, 2, BGRA);
        let address = b as usize;
        let threads: Vec<_> = (0..6)
            .map(|_| {
                std::thread::spawn(move || {
                    let ptr = address as CFTypeRef;
                    unsafe {
                        let retained = CVPixelBufferRetain(ptr);
                        for _ in 0..100 {
                            assert_eq!(CVPixelBufferLockBaseAddress(retained, READ_ONLY), SUCCESS);
                            assert!(!CVPixelBufferGetBaseAddress(retained).is_null());
                            assert_eq!(
                                CVPixelBufferUnlockBaseAddress(retained, READ_ONLY),
                                SUCCESS
                            );
                        }
                        CVPixelBufferRelease(retained);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        unsafe { CVPixelBufferRelease(b) };
    }
}
