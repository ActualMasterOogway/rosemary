//! CoreGraphics color spaces, data providers and images. Contexts are in
//! [`crate::cgcontext`].

#![allow(non_snake_case)]

use std::ffi::c_void;

use corefoundation::base::CFTypeRef;

#[repr(C)]
struct ColorSpace {
    isa: *const c_void,
}

/// `CGDataProviderReleaseDataCallback`.
type ReleaseData = Option<unsafe extern "C" fn(*mut c_void, *const c_void, usize)>;

#[repr(C)]
struct DataProvider {
    isa: *const c_void,
    info: *mut c_void,
    data: *const u8,
    size: usize,
    release: ReleaseData,
}

/// A `CGImage` over the bytes of a data provider, which it retains.
#[repr(C)]
struct Image {
    isa: *const c_void,
    width: usize,
    height: usize,
    bits_per_component: usize,
    bits_per_pixel: usize,
    bytes_per_row: usize,
    bitmap_info: u32,
    provider: CFTypeRef,
    space: CFTypeRef,
}

pub fn install() {
    corefoundation::base::register(c"__CGColorSpace", None, size_of::<ColorSpace>(), true);
    let provider =
        corefoundation::base::register(c"__CGDataProvider", None, size_of::<DataProvider>(), true);
    provider.replace_method(
        objc::sel::get("dealloc"),
        provider_dealloc as *const c_void,
        c"v16@0:8",
    );
    let image = corefoundation::base::register(c"__CGImage", None, size_of::<Image>(), true);
    image.replace_method(
        objc::sel::get("dealloc"),
        image_dealloc as *const c_void,
        c"v16@0:8",
    );
}

/// Hands the bytes back to their owner, which is how Qt learns that the
/// `QImage` a `CGImage` was made over may be detached again.
unsafe extern "C" fn provider_dealloc(receiver: objc::object::Id, _sel: objc::sel::Sel) {
    // SAFETY: `-dealloc` of a `__CGDataProvider`, whose body is `DataProvider`.
    unsafe {
        let provider = &mut *(receiver as *mut DataProvider);
        if let Some(release) = provider.release.take() {
            release(provider.info, provider.data.cast(), provider.size);
        }
        objc::arc::finalize(receiver);
    }
}

unsafe extern "C" fn image_dealloc(receiver: objc::object::Id, _sel: objc::sel::Sel) {
    // SAFETY: `-dealloc` of a `__CGImage`, whose body is `Image`.
    unsafe {
        let image = &mut *(receiver as *mut Image);
        for object in [image.provider, image.space] {
            if !object.is_null() {
                corefoundation::api::CFRelease(object);
            }
        }
        image.provider = std::ptr::null();
        image.space = std::ptr::null();
        objc::arc::finalize(receiver);
    }
}

pub unsafe extern "C" fn CGDataProviderCreateWithData(
    info: *mut c_void,
    data: *const c_void,
    size: usize,
    release: ReleaseData,
) -> CFTypeRef {
    let class = objc::class::lookup(c"__CGDataProvider").expect("installed during bootstrap");
    // SAFETY: `DataProvider` is the registered class layout.
    let provider = unsafe { corefoundation::base::allocate::<DataProvider>(class) };
    // SAFETY: a fresh allocation.
    unsafe {
        (*provider).info = info;
        (*provider).data = data.cast();
        (*provider).size = size;
        (*provider).release = release;
    }
    provider as CFTypeRef
}

pub unsafe extern "C" fn CGDataProviderRelease(provider: CFTypeRef) {
    if !provider.is_null() {
        unsafe { corefoundation::api::CFRelease(provider) };
    }
}

fn is_image(image: CFTypeRef) -> bool {
    let class = objc::class::lookup(c"__CGImage").expect("installed during bootstrap");
    // SAFETY: null or an object a guest received from this runtime.
    !image.is_null() && unsafe { corefoundation::base::is_kind_of(image, class) }
}

fn image_of<'a>(image: CFTypeRef) -> Option<&'a Image> {
    // SAFETY: checked to be a `__CGImage`, whose body is `Image`.
    is_image(image).then(|| unsafe { &*(image as *const Image) })
}

#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn CGImageCreate(
    width: usize,
    height: usize,
    bits_per_component: usize,
    bits_per_pixel: usize,
    bytes_per_row: usize,
    space: CFTypeRef,
    bitmap_info: u32,
    provider: CFTypeRef,
    _decode: *const f64,
    _should_interpolate: u8,
    _intent: i32,
) -> CFTypeRef {
    let class = objc::class::lookup(c"__CGDataProvider").expect("installed during bootstrap");
    // SAFETY: null or an object a guest received from this runtime.
    if provider.is_null() || !unsafe { corefoundation::base::is_kind_of(provider, class) } {
        return std::ptr::null();
    }
    if width == 0 || height == 0 || bits_per_pixel == 0 {
        return std::ptr::null();
    }
    let class = objc::class::lookup(c"__CGImage").expect("installed during bootstrap");
    // SAFETY: `Image` is the registered class layout.
    let image = unsafe { corefoundation::base::allocate::<Image>(class) };
    // SAFETY: a fresh allocation; both retained objects are live.
    unsafe {
        *image = Image {
            isa: (*image).isa,
            width,
            height,
            bits_per_component,
            bits_per_pixel,
            bytes_per_row,
            bitmap_info,
            provider: corefoundation::api::CFRetain(provider),
            space: if space.is_null() {
                space
            } else {
                corefoundation::api::CFRetain(space)
            },
        };
    }
    image as CFTypeRef
}

/// The pixels stay shared with the original: only the color space differs,
/// and nothing here color-manages.
pub unsafe extern "C" fn CGImageCreateCopyWithColorSpace(
    image: CFTypeRef,
    space: CFTypeRef,
) -> CFTypeRef {
    let Some(original) = image_of(image) else {
        return std::ptr::null();
    };
    unsafe {
        CGImageCreate(
            original.width,
            original.height,
            original.bits_per_component,
            original.bits_per_pixel,
            original.bytes_per_row,
            space,
            original.bitmap_info,
            original.provider,
            std::ptr::null(),
            0,
            0,
        )
    }
}

pub unsafe extern "C" fn CGImageRetain(image: CFTypeRef) -> CFTypeRef {
    if image.is_null() {
        return image;
    }
    unsafe { corefoundation::api::CFRetain(image) }
}

pub unsafe extern "C" fn CGImageRelease(image: CFTypeRef) {
    if !image.is_null() {
        unsafe { corefoundation::api::CFRelease(image) };
    }
}

pub unsafe extern "C" fn CGImageGetWidth(image: CFTypeRef) -> usize {
    image_of(image).map_or(0, |image| image.width)
}

pub unsafe extern "C" fn CGImageGetHeight(image: CFTypeRef) -> usize {
    image_of(image).map_or(0, |image| image.height)
}

pub unsafe extern "C" fn CGImageGetBitsPerComponent(image: CFTypeRef) -> usize {
    image_of(image).map_or(0, |image| image.bits_per_component)
}

pub unsafe extern "C" fn CGImageGetAlphaInfo(image: CFTypeRef) -> u32 {
    image_of(image).map_or(0, |image| image.bitmap_info & ALPHA_INFO_MASK)
}

pub unsafe extern "C" fn CGImageGetColorSpace(image: CFTypeRef) -> CFTypeRef {
    image_of(image).map_or(std::ptr::null(), |image| image.space)
}

const ALPHA_INFO_MASK: u32 = 0x1f;
const BYTE_ORDER_MASK: u32 = 0x7000;
const BYTE_ORDER_32_LITTLE: u32 = 0x2000;

/// A `CGImage`'s pixels as premultiplied B, G, R, A rows, top row first -
/// the layout [`crate::backing`] composites in.
///
/// `None` for a pixel layout this does not decode, which is anything but
/// eight bits per component in three or four components.
pub(crate) fn image_bgra(image: CFTypeRef) -> Option<(usize, usize, Vec<u8>)> {
    let image = image_of(image)?;
    // SAFETY: a `__CGImage` holds a retained `__CGDataProvider`.
    let provider = unsafe { &*(image.provider as *const DataProvider) };
    if image.bits_per_component != 8 || provider.data.is_null() {
        return None;
    }
    let channels = match image.bits_per_pixel {
        32 => 4,
        24 => 3,
        _ => return None,
    };
    let stride = image.bytes_per_row.max(image.width * channels);
    let needed = stride
        .checked_mul(image.height - 1)?
        .checked_add(image.width * channels)?;
    if provider.size < needed {
        return None;
    }
    // SAFETY: the provider was created over `size` readable bytes, which the
    // check above shows cover every row.
    let bytes = unsafe { std::slice::from_raw_parts(provider.data, needed) };
    let alpha = image.bitmap_info & ALPHA_INFO_MASK;
    let little = image.bitmap_info & BYTE_ORDER_MASK == BYTE_ORDER_32_LITTLE;
    // `First` alphas (and `NoneSkipFirst`) lead the component order; the
    // component order is reversed in memory for a little-endian 32-bit image.
    let alpha_first = matches!(alpha, 2 | 4 | 6);
    let has_alpha = matches!(alpha, 1..=4);
    let premultiplied = matches!(alpha, 1 | 2);
    let mut out = vec![0u8; image.width * image.height * 4];
    for row in 0..image.height {
        let from = &bytes[row * stride..row * stride + image.width * channels];
        let to = &mut out[row * image.width * 4..(row + 1) * image.width * 4];
        for (source, target) in from.chunks_exact(channels).zip(to.chunks_exact_mut(4)) {
            let mut ordered = [0u8; 4];
            ordered[..channels].copy_from_slice(source);
            if little && channels == 4 {
                ordered.reverse();
            }
            let (a, r, g, b) = match (channels, alpha_first) {
                (4, true) => (ordered[0], ordered[1], ordered[2], ordered[3]),
                (4, false) => (ordered[3], ordered[0], ordered[1], ordered[2]),
                _ => (255, ordered[0], ordered[1], ordered[2]),
            };
            let a = if has_alpha { a } else { 255 };
            let scale = |c: u8| match premultiplied || a == 255 {
                true => c.min(a),
                false => ((u16::from(c) * u16::from(a) + 127) / 255) as u8,
            };
            target.copy_from_slice(&[scale(b), scale(g), scale(r), a]);
        }
    }
    Some((image.width, image.height, out))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_over_little_endian_premultiplied_first_reads_as_bgra() {
        crate::bootstrap();
        static RELEASED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        unsafe extern "C" fn release(_info: *mut c_void, _data: *const c_void, size: usize) {
            RELEASED.store(size, std::sync::atomic::Ordering::SeqCst);
        }
        // 0xAARRGGBB in host (little) order: two pixels, opaque red and
        // half-transparent green.
        let pixels: [u32; 2] = [0xffff_0000, 0x8000_8000];
        // SAFETY: the provider is released before `pixels` goes out of scope.
        unsafe {
            let provider = CGDataProviderCreateWithData(
                std::ptr::null_mut(),
                pixels.as_ptr().cast(),
                8,
                Some(release),
            );
            let image = CGImageCreate(
                2,
                1,
                8,
                32,
                8,
                std::ptr::null(),
                0x2002,
                provider,
                std::ptr::null(),
                0,
                0,
            );
            CGDataProviderRelease(provider);
            assert_eq!(CGImageGetWidth(image), 2);
            let copy = CGImageCreateCopyWithColorSpace(image, std::ptr::null());
            CGImageRelease(image);
            let (width, height, bgra) = image_bgra(copy).expect("decodable");
            assert_eq!((width, height), (2, 1));
            assert_eq!(bgra, [0, 0, 0xff, 0xff, 0, 0x80, 0, 0x80]);
            assert_eq!(RELEASED.load(std::sync::atomic::Ordering::SeqCst), 0);
            CGImageRelease(copy);
            assert_eq!(RELEASED.load(std::sync::atomic::Ordering::SeqCst), 8);
        }
    }
}
