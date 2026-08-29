//! `NSImage`, as far as a runtime with no raster pipeline can take one.
//!
//! An image here holds the bytes it was made from and can say how big they are.
//! It cannot draw, composite or re-encode them, because doing any of that means
//! decoding - and a decoder for every format `NSImage` accepts is a library, not
//! a module.
//!
//! The split is where it is because *size* is answerable exactly from a
//! container header and everything else is not. A program that only measures an
//! image gets the right answer; one that asks for pixels is refused by name,
//! with the format it handed in, which is the queue for whoever adds a decoder.
//!
//! Deltarune is why this exists at all, and its `splash.png` is why the header
//! sniffing is not just PNG: the file is a Windows BMP under a `.png` name. The
//! runner's own PNG reader says "Not a PNG" - correctly - and falls back here.

use std::ffi::c_void;

use foundation::geometry::NSSize;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

#[repr(C)]
pub struct Image {
    isa: *const c_void,
    /// The bytes, retained as the `NSData` the guest handed over.
    data: Id,
    size: NSSize,
}

pub const INSTANCE_SIZE: usize = size_of::<Image>();

/// A container this module recognises, and the size it declares.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Recognised {
    format: &'static str,
    size: NSSize,
}

/// What `bytes` is, by its header alone.
///
/// Header sniffing rather than decoding: every format below states its
/// dimensions in a fixed place near the front, so the size is a read rather than
/// an inference. A container that is not one of these is `None`, and `None` is
/// what makes `-initWithData:` answer nil.
fn recognise(bytes: &[u8]) -> Option<Recognised> {
    let be32 = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    let le32 = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };

    // PNG: the eight-byte signature, then an `IHDR` whose first two fields are
    // the dimensions.
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(Recognised {
            format: "PNG",
            size: NSSize {
                width: be32(16)? as f64,
                height: be32(20)? as f64,
            },
        });
    }
    // BMP: `BM`, then a DIB header whose width and height are signed and whose
    // height is negative for a top-down image - so the magnitude is the size.
    if bytes.starts_with(b"BM") {
        let width = le32(18)? as i32;
        let height = le32(22)? as i32;
        return Some(Recognised {
            format: "BMP",
            size: NSSize {
                width: width.unsigned_abs() as f64,
                height: height.unsigned_abs() as f64,
            },
        });
    }
    // GIF: `GIF87a`/`GIF89a`, then two little-endian sixteen-bit fields.
    if bytes.starts_with(b"GIF8") {
        let le16 = |at: usize| -> Option<u16> {
            Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
        };
        return Some(Recognised {
            format: "GIF",
            size: NSSize {
                width: le16(6)? as f64,
                height: le16(8)? as f64,
            },
        });
    }
    // ICNS and JPEG state their sizes inside a chunk walk rather than at a fixed
    // offset. Recognised so the diagnostic can name them, with no size - which
    // `-size` reports as zero, the same as Apple does for an image it has not
    // measured yet.
    if bytes.starts_with(b"icns") {
        return Some(Recognised {
            format: "ICNS",
            size: NSSize::default(),
        });
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some(Recognised {
            format: "JPEG",
            size: NSSize::default(),
        });
    }
    None
}

fn image_of<'a>(receiver: Id) -> &'a mut Image {
    // SAFETY: an image of this crate's.
    unsafe { &mut *(receiver as *mut Image) }
}

/// The bytes an image holds.
fn bytes_of<'a>(receiver: Id) -> &'a [u8] {
    let data = image_of(receiver).data;
    if data.is_null() {
        return &[];
    }
    // SAFETY: an `NSData` this object retained.
    match unsafe { corefoundation::collections::data(data as *const c_void) } {
        Some(bytes) => bytes.as_slice(),
        None => &[],
    }
}

/// `-initWithData:`.
///
/// **Nil for data this runtime does not recognise**, which is `NSImage`'s own
/// contract rather than a shortcut: Apple returns nil for data it cannot make an
/// image of, and a program handing over an arbitrary file is expected to check.
/// What would be a guess is answering non-nil and then drawing nothing.
unsafe extern "C" fn init_with_data(receiver: Id, _sel: Sel, data: Id) -> Id {
    let image = image_of(receiver);
    if data.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: null or an `NSData`.
    let Some(bytes) = (unsafe { corefoundation::collections::data(data as *const c_void) }) else {
        return std::ptr::null_mut();
    };
    let Some(recognised) = recognise(bytes) else {
        return std::ptr::null_mut();
    };
    image.data = objc::arc::retain(data);
    image.size = recognised.size;
    receiver
}

unsafe extern "C" fn init_with_contents_of_file(receiver: Id, sel: Sel, path: Id) -> Id {
    // SAFETY: an `NSString`, or nil.
    let path = unsafe { foundation::string::bytes(path) };
    let Ok(path) = String::from_utf8(path) else {
        return std::ptr::null_mut();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return std::ptr::null_mut();
    };
    let data = corefoundation::collections::new_data(bytes);
    // SAFETY: `-initWithData:` is `id (id, SEL, id)` and the data is a fresh
    // object this call owns a reference to.
    let image = unsafe { init_with_data(receiver, sel, data as Id) };
    // SAFETY: as above; the image retained it if it kept it.
    unsafe { corefoundation::api::CFRelease(data) };
    image
}

unsafe extern "C" fn size(receiver: Id, _sel: Sel) -> NSSize {
    image_of(receiver).size
}

unsafe extern "C" fn set_size(receiver: Id, _sel: Sel, size: NSSize) {
    image_of(receiver).size = size;
}

/// Everything that still needs a decoded raster this module cannot produce.
///
/// One implementation for the whole family, naming the selector it was reached
/// by and the format that was handed in - which together are the whole of what
/// somebody adding a decoder needs to know.
unsafe extern "C" fn needs_pixels(receiver: Id, sel: Sel) -> ! {
    // SAFETY: a registered selector is a NUL-terminated interned name.
    let name = unsafe { std::ffi::CStr::from_ptr(sel) }.to_string_lossy();
    let format = recognise(bytes_of(receiver)).map_or("unrecognised data", |r| r.format);
    objc::fail!(
        "-[NSImage {name}]: this runtime has no decoder for {format} - see appkit::raster for the \
         ones it does have"
    );
}

/// `-TIFFRepresentation`: the image, decoded and re-encoded.
///
/// The round trip is the guest's design rather than this runtime's. A program
/// that wants an image's pixels asks `NSImage` for a TIFF and hands that to
/// `NSBitmapImageRep`, so both ends of the conversion are here and
/// `appkit::raster` keeps them agreeing.
///
/// Nil when the container has no decoder, which is what Apple answers for an
/// image it could not read - and the guest checks, because it has just come
/// down this path from its own reader failing.
unsafe extern "C" fn tiff_representation(receiver: Id, _sel: Sel) -> Id {
    let Some(bitmap) = crate::raster::decode(bytes_of(receiver)) else {
        return std::ptr::null_mut();
    };
    let tiff = corefoundation::collections::new_data(crate::raster::encode_tiff(&bitmap));
    // SAFETY: a fresh object.
    unsafe { corefoundation::api::CFAutorelease(tiff) as Id }
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSImage").expect("declared before its methods");
    methods!(class, {
        "initWithData:" => init_with_data, c"@24@0:8@16";
        "initWithContentsOfFile:" => init_with_contents_of_file, c"@24@0:8@16";
        "size" => size, c"{CGSize=dd}16@0:8";
        "setSize:" => set_size, c"v32@0:8{CGSize=dd}16";
    });
    // The pixel family, each reached by the same implementation so that the one
    // that is wanted first names itself.
    methods!(class, {
        "TIFFRepresentation" => tiff_representation, c"@16@0:8";
    });
    // The pixel family, each reached by the same implementation so that the one
    // that is wanted first names itself. The encodings are still each method's
    // own: the runtime marshals a guest's send by decoding exactly this string,
    // and the shared implementation does not make the arguments go away.
    for (selector, types) in [
        ("lockFocus", c"v16@0:8"),
        ("unlockFocus", c"v16@0:8"),
        ("representations", c"@16@0:8"),
        (
            "CGImageForProposedRect:context:hints:",
            c"^v40@0:8^{CGRect={CGPoint=dd}{CGSize=dd}}16@24@32",
        ),
        (
            "bestRepresentationForRect:context:hints:",
            c"@64@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16@48@56",
        ),
        (
            "drawInRect:fromRect:operation:fraction:",
            c"v96@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16{CGRect={CGPoint=dd}{CGSize=dd}}48Q80d88",
        ),
    ] {
        class.replace_method(
            objc::sel::get(selector),
            needs_pixels as *const c_void,
            types,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two containers Deltarune actually hands over, measured from their
    /// headers - and the BMP is the one that matters, because the file is called
    /// `splash.png`.
    #[test]
    fn a_header_says_how_big_the_image_is() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(
            recognise(&png).map(|r| (r.format, r.size.width, r.size.height)),
            Some(("PNG", 640.0, 480.0))
        );

        let mut bmp = b"BM".to_vec();
        bmp.resize(18, 0);
        bmp.extend_from_slice(&150i32.to_le_bytes());
        // Negative, as a top-down BMP writes it: the size is the magnitude.
        bmp.extend_from_slice(&(-57i32).to_le_bytes());
        assert_eq!(
            recognise(&bmp).map(|r| (r.format, r.size.width, r.size.height)),
            Some(("BMP", 150.0, 57.0))
        );
    }

    /// Anything else is `None`, which is what makes `-initWithData:` nil rather
    /// than an image that would draw nothing.
    #[test]
    fn unrecognised_data_is_not_an_image() {
        assert_eq!(recognise(b"not an image at all"), None);
        assert_eq!(recognise(b""), None);
        // A truncated header is not a size to be guessed at either.
        assert_eq!(recognise(b"\x89PNG\r\n\x1a\n"), None);
    }
}

/// `NSBitmapImageRep`: a decoded raster a program can read or write directly.
///
/// The one AppKit class in this tree a guest gets *memory* out of rather than
/// answers. `-bitmapData` is a pointer the guest reads pixel by pixel and hands
/// to `glTexImage2D`, so the buffer has to stay put for as long as the rep does
/// and its layout has to be exactly what the accessors say it is.
pub mod bitmap {
    use super::*;
    use crate::define::{class_methods, methods};

    #[repr(C)]
    pub struct BitmapRep {
        isa: *const c_void,
        /// The pixels, owned here. Leaked with the object rather than freed at
        /// `-dealloc`, because the guest may still be holding the pointer
        /// `-bitmapData` gave it - Apple's rep owns its buffer for exactly as
        /// long as the rep, and this runtime has no `-dealloc` to hang the free
        /// on that would be reached at the right time.
        pixels: *mut Vec<u8>,
        /// A buffer the *guest* supplied, which this object must not free and
        /// must not copy: the guest writes through it.
        supplied: *mut u8,
        width: usize,
        height: usize,
        samples: usize,
        bits_per_sample: usize,
        bits_per_pixel: usize,
        bytes_per_row: usize,
        has_alpha: usize,
    }

    pub const INSTANCE_SIZE: usize = size_of::<BitmapRep>();

    fn rep_of<'a>(receiver: Id) -> &'a mut BitmapRep {
        // SAFETY: a bitmap rep of this crate's.
        unsafe { &mut *(receiver as *mut BitmapRep) }
    }

    /// Fill in the fields a decoded bitmap determines.
    fn adopt(receiver: Id, bitmap: crate::raster::Bitmap) -> Id {
        let rep = rep_of(receiver);
        rep.width = bitmap.width as usize;
        rep.height = bitmap.height as usize;
        rep.samples = 4;
        rep.bits_per_sample = 8;
        rep.bits_per_pixel = 32;
        rep.bytes_per_row = bitmap.bytes_per_row();
        rep.has_alpha = 1;
        rep.pixels = Box::into_raw(Box::new(bitmap.pixels));
        receiver
    }

    unsafe extern "C" fn init_with_data(receiver: Id, _sel: Sel, data: Id) -> Id {
        if data.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: null or an `NSData`.
        let Some(bytes) = (unsafe { corefoundation::collections::data(data as *const c_void) })
        else {
            return std::ptr::null_mut();
        };
        match crate::raster::decode(bytes) {
            Some(bitmap) => adopt(receiver, bitmap),
            None => std::ptr::null_mut(),
        }
    }

    /// The eleven-argument designated initialiser.
    ///
    /// `planes` is the guest's own memory when it supplies one and a request to
    /// allocate when it is null or points at null - Apple's rule, and the reason
    /// the two cases are kept apart here rather than always copying: a guest that
    /// passed its own buffer goes on writing through it, and a copy would leave
    /// it drawing into memory nothing reads.
    #[expect(clippy::too_many_arguments, reason = "the C method has eleven")]
    unsafe extern "C" fn init_with_planes(
        receiver: Id,
        _sel: Sel,
        planes: *mut *mut u8,
        width: isize,
        height: isize,
        bits_per_sample: isize,
        samples: isize,
        has_alpha: u8,
        is_planar: u8,
        _colour_space: Id,
        _format: usize,
        bytes_per_row: isize,
        bits_per_pixel: isize,
    ) -> Id {
        if is_planar != 0 {
            objc::fail!(
                "-[NSBitmapImageRep initWithBitmapDataPlanes:...]: a planar rep keeps each \
                 channel in its own buffer, and this runtime only has the interleaved layout"
            );
        }
        if width <= 0 || height <= 0 || bits_per_sample != 8 || !matches!(samples, 3 | 4) {
            objc::fail!(
                "-[NSBitmapImageRep initWithBitmapDataPlanes:...]: this runtime makes \
                 eight-bit-per-sample reps of three or four samples, not {samples} samples of \
                 {bits_per_sample} bits"
            );
        }
        let rep = rep_of(receiver);
        rep.width = width as usize;
        rep.height = height as usize;
        rep.samples = samples as usize;
        rep.bits_per_sample = 8;
        rep.has_alpha = (has_alpha != 0) as usize;
        rep.bits_per_pixel = match bits_per_pixel > 0 {
            true => bits_per_pixel as usize,
            false => samples as usize * 8,
        };
        rep.bytes_per_row = match bytes_per_row > 0 {
            true => bytes_per_row as usize,
            false => rep.width * rep.bits_per_pixel / 8,
        };

        // SAFETY: `planes` is null or an array of at least one pointer, which is
        // what the method's contract says.
        let supplied = match planes.is_null() {
            true => std::ptr::null_mut(),
            false => unsafe { *planes },
        };
        match supplied.is_null() {
            true => rep.pixels = Box::into_raw(Box::new(vec![0u8; rep.bytes_per_row * rep.height])),
            false => rep.supplied = supplied,
        }
        receiver
    }

    unsafe extern "C" fn bitmap_data(receiver: Id, _sel: Sel) -> *mut u8 {
        let rep = rep_of(receiver);
        if !rep.supplied.is_null() {
            return rep.supplied;
        }
        match rep.pixels.is_null() {
            true => std::ptr::null_mut(),
            // SAFETY: a buffer this object leaked, which outlives every caller.
            false => unsafe { (*rep.pixels).as_mut_ptr() },
        }
    }

    unsafe extern "C" fn pixels_wide(receiver: Id, _sel: Sel) -> isize {
        rep_of(receiver).width as isize
    }

    unsafe extern "C" fn pixels_high(receiver: Id, _sel: Sel) -> isize {
        rep_of(receiver).height as isize
    }

    unsafe extern "C" fn samples_per_pixel(receiver: Id, _sel: Sel) -> isize {
        rep_of(receiver).samples as isize
    }

    unsafe extern "C" fn bits_per_sample(receiver: Id, _sel: Sel) -> isize {
        rep_of(receiver).bits_per_sample as isize
    }

    unsafe extern "C" fn bits_per_pixel(receiver: Id, _sel: Sel) -> isize {
        rep_of(receiver).bits_per_pixel as isize
    }

    unsafe extern "C" fn bytes_per_row(receiver: Id, _sel: Sel) -> isize {
        rep_of(receiver).bytes_per_row as isize
    }

    unsafe extern "C" fn has_alpha(receiver: Id, _sel: Sel) -> u8 {
        (rep_of(receiver).has_alpha != 0) as u8
    }

    unsafe extern "C" fn is_planar(_receiver: Id, _sel: Sel) -> u8 {
        0
    }

    unsafe extern "C" fn rep_size(receiver: Id, _sel: Sel) -> NSSize {
        let rep = rep_of(receiver);
        NSSize {
            width: rep.width as f64,
            height: rep.height as f64,
        }
    }

    unsafe extern "C" fn tiff_representation(receiver: Id, _sel: Sel) -> Id {
        let rep = rep_of(receiver);
        // SAFETY: `-bitmapData` is `void * (id, SEL)` and this module's own.
        let data = unsafe { bitmap_data(receiver, objc::sel::get("bitmapData")) };
        if data.is_null() || rep.samples != 4 {
            return std::ptr::null_mut();
        }
        // SAFETY: the buffer is `bytes_per_row * height` bytes, which is what
        // both initialisers established.
        let pixels =
            unsafe { std::slice::from_raw_parts(data, rep.bytes_per_row * rep.height) }.to_vec();
        let bitmap = crate::raster::Bitmap {
            width: rep.width as u32,
            height: rep.height as u32,
            pixels,
        };
        let tiff = corefoundation::collections::new_data(crate::raster::encode_tiff(&bitmap));
        // SAFETY: a fresh object.
        unsafe { corefoundation::api::CFAutorelease(tiff) as Id }
    }

    pub(crate) fn install() {
        let class =
            objc::class::lookup(c"NSBitmapImageRep").expect("declared before its methods");
        class_methods!(class, {
            "imageRepWithData:" => init_with_data, c"@24@0:8@16";
        });
        methods!(class, {
            "initWithData:" => init_with_data, c"@24@0:8@16";
            "initWithBitmapDataPlanes:pixelsWide:pixelsHigh:bitsPerSample:samplesPerPixel:hasAlpha:isPlanar:colorSpaceName:bitmapFormat:bytesPerRow:bitsPerPixel:"
                => init_with_planes, c"@104@0:8^*16q24q32q40q48c56c64@72Q80q88q96";
            "bitmapData" => bitmap_data, c"^v16@0:8";
            "pixelsWide" => pixels_wide, c"q16@0:8";
            "pixelsHigh" => pixels_high, c"q16@0:8";
            "samplesPerPixel" => samples_per_pixel, c"q16@0:8";
            "bitsPerSample" => bits_per_sample, c"q16@0:8";
            "bitsPerPixel" => bits_per_pixel, c"q16@0:8";
            "bytesPerRow" => bytes_per_row, c"q16@0:8";
            "hasAlpha" => has_alpha, c"c16@0:8";
            "isPlanar" => is_planar, c"c16@0:8";
            "size" => rep_size, c"{CGSize=dd}16@0:8";
            "TIFFRepresentation" => tiff_representation, c"@16@0:8";
        });
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The pipeline the runner uses, end to end: an image made from a BMP,
        /// asked for a TIFF, handed to a rep, and read as pixels.
        #[test]
        fn a_bmp_reaches_the_guest_as_rgba_through_a_tiff() {
            crate::bootstrap();
            let mut bmp = b"BM".to_vec();
            bmp.resize(10, 0);
            bmp.extend_from_slice(&54u32.to_le_bytes());
            bmp.extend_from_slice(&40u32.to_le_bytes());
            bmp.extend_from_slice(&1i32.to_le_bytes());
            bmp.extend_from_slice(&1i32.to_le_bytes());
            bmp.extend_from_slice(&1u16.to_le_bytes());
            bmp.extend_from_slice(&24u16.to_le_bytes());
            bmp.extend_from_slice(&0u32.to_le_bytes());
            bmp.resize(54, 0);
            // One pixel: blue, green, red in BMP order.
            bmp.extend_from_slice(&[0x10, 0x20, 0x30, 0]);

            let data = corefoundation::collections::new_data(bmp);
            let image = objc::object::create_instance(
                objc::class::lookup(c"NSImage").expect("NSImage"),
                0,
            );
            // SAFETY: `-initWithData:` is `id (id, SEL, id)`.
            let image = unsafe {
                super::super::init_with_data(image, objc::sel::get("initWithData:"), data as Id)
            };
            assert!(!image.is_null(), "a BMP is an image this runtime recognises");

            // SAFETY: `-TIFFRepresentation` is `id (id, SEL)`.
            let tiff = unsafe {
                super::super::tiff_representation(image, objc::sel::get("TIFFRepresentation"))
            };
            assert!(!tiff.is_null());

            let rep = objc::object::create_instance(
                objc::class::lookup(c"NSBitmapImageRep").expect("NSBitmapImageRep"),
                0,
            );
            // SAFETY: `-initWithData:` is `id (id, SEL, id)`.
            let rep = unsafe { init_with_data(rep, objc::sel::get("initWithData:"), tiff) };
            assert!(!rep.is_null(), "the rep reads the TIFF this tree wrote");

            // SAFETY: `-bitmapData` is `void * (id, SEL)`.
            let pixels = unsafe { bitmap_data(rep, objc::sel::get("bitmapData")) };
            // SAFETY: one RGBA pixel, as the rep reports.
            let rgba = unsafe { std::slice::from_raw_parts(pixels, 4) };
            assert_eq!(rgba, &[0x30, 0x20, 0x10, 0xff], "red, green, blue, opaque");
            // SAFETY: both are `NSInteger (id, SEL)`.
            unsafe {
                assert_eq!(pixels_wide(rep, objc::sel::get("pixelsWide")), 1);
                assert_eq!(samples_per_pixel(rep, objc::sel::get("samplesPerPixel")), 4);
            }
        }

        /// A buffer the guest supplied is the buffer `-bitmapData` gives back,
        /// not a copy of it - the guest draws through that pointer.
        #[test]
        fn a_supplied_plane_is_not_copied() {
            crate::bootstrap();
            let mut buffer = vec![0u8; 16];
            let mut planes = [buffer.as_mut_ptr()];
            let rep = objc::object::create_instance(
                objc::class::lookup(c"NSBitmapImageRep").expect("NSBitmapImageRep"),
                0,
            );
            // SAFETY: the prototype is the eleven-argument initialiser's.
            let rep = unsafe {
                init_with_planes(
                    rep,
                    objc::sel::get("initWithBitmapDataPlanes:"),
                    planes.as_mut_ptr(),
                    2,
                    2,
                    8,
                    4,
                    1,
                    0,
                    std::ptr::null_mut(),
                    0,
                    8,
                    32,
                )
            };
            // SAFETY: `-bitmapData` is `void * (id, SEL)`.
            let got = unsafe { bitmap_data(rep, objc::sel::get("bitmapData")) };
            assert_eq!(got, buffer.as_mut_ptr());
        }
    }
}
