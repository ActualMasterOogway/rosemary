//! The font atlas, rasterized by CoreText at the size the window asks for.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use super::sys::{self, CfRef, CgFloat, Point, Rect, Size};

/// The first and last character the atlas covers: printable ASCII.
const FIRST: u8 = 0x20;
const LAST: u8 = 0x7e;

/// Cells across the atlas. Sixteen keeps the texture close to square for every
/// size that gets asked for.
const COLUMNS: usize = 16;

/// The number of cells: one per character, plus one filled solid.
const CELLS: usize = (LAST - FIRST) as usize + 2;

/// The index of the solid cell - the one a rectangle samples.
const SOLID: usize = CELLS - 1;

/// A rasterized font at one size.
pub struct Atlas {
    /// Eight-bit coverage, one byte per texel, `width * height` of them.
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
    /// Cell size in texels, and the advance a character contributes to a line. The
    /// advance is smaller than the cell.
    pub cell_w: usize,
    pub cell_h: usize,
    pub advance: f32,
    /// Line height, which is the cell height less the pad.
    pub line_h: f32,
}

/// The texture coordinates of one cell, as `(u0, v0, u1, v1)`.
type Uv = (f32, f32, f32, f32);

impl Atlas {
    /// Where in the texture the glyph for `c` is.
    pub fn glyph(&self, c: u8) -> Uv {
        let index = match (FIRST..=LAST).contains(&c) {
            true => (c - FIRST) as usize,
            // A byte outside the range is a question mark, deliberately drawn
            // rather than skipped - a line that silently loses a character is a
            // line whose columns no longer align with the one above it.
            false => (b'?' - FIRST) as usize,
        };
        self.cell(index)
    }

    /// The single opaque texel a filled rectangle samples.
    pub fn solid(&self) -> (f32, f32) {
        let (u0, v0, u1, v1) = self.cell(SOLID);
        ((u0 + u1) / 2.0, (v0 + v1) / 2.0)
    }

    fn cell(&self, index: usize) -> Uv {
        let (col, row) = (index % COLUMNS, index / COLUMNS);
        let (w, h) = (self.width as f32, self.height as f32);
        (
            (col * self.cell_w) as f32 / w,
            (row * self.cell_h) as f32 / h,
            ((col + 1) * self.cell_w) as f32 / w,
            ((row + 1) * self.cell_h) as f32 / h,
        )
    }

    /// The width a string will occupy.
    pub fn measure(&self, s: &str) -> f32 {
        s.len() as f32 * self.advance
    }
}

/// `kCGImageAlphaOnly`. One byte per texel and no colour at all, which is exactly what
/// a coverage mask is and saves converting one out of an RGBA bitmap afterwards.
const ALPHA_ONLY: u32 = 7;

/// `kCTFontOrientationDefault`.
const ORIENTATION_DEFAULT: u32 = 0;

/// The typeface asked for. Monospaced on purpose: the panels are columns of
/// numbers, and a proportional font would make them jitter as the digits change.
const FACE: &str = "Menlo-Regular";

/// Build the atlas for `size`, or `None` if CoreText is not reachable.
fn rasterize(size: f32) -> Option<Atlas> {
    type FontCreate = unsafe extern "C" fn(CfRef, CgFloat, *const c_void) -> CfRef;
    type GetGlyphs = unsafe extern "C" fn(CfRef, *const u16, *mut u16, isize) -> u8;
    type GetAdvances = unsafe extern "C" fn(CfRef, u32, *const u16, *mut Size, isize) -> f64;
    type Metric = unsafe extern "C" fn(CfRef) -> CgFloat;
    type DrawGlyphs = unsafe extern "C" fn(CfRef, *const u16, *const Point, usize, CfRef);
    type BitmapCreate =
        unsafe extern "C" fn(*mut c_void, usize, usize, usize, usize, CfRef, u32) -> CfRef;
    type SetGray = unsafe extern "C" fn(CfRef, CgFloat, CgFloat);
    type SetFlag = unsafe extern "C" fn(CfRef, u8);
    type Release = unsafe extern "C" fn(CfRef);
    type ContextFillRect = unsafe extern "C" fn(CfRef, Rect);

    // SAFETY of every `function` below: each type above is transcribed from the
    // framework header that declares the symbol - `<CoreText/CTFont.h>` for the
    // `CTFont*` family and `<CoreGraphics/CGBitmapContext.h>` and
    // `<CoreGraphics/CGContext.h>` for the rest. They are public API, and have
    // been since 10.5.
    let font_create = unsafe { sys::function::<FontCreate>("CTFontCreateWithName") }?;
    let get_glyphs = unsafe { sys::function::<GetGlyphs>("CTFontGetGlyphsForCharacters") }?;
    let get_advances = unsafe { sys::function::<GetAdvances>("CTFontGetAdvancesForGlyphs") }?;
    let ascent = unsafe { sys::function::<Metric>("CTFontGetAscent") }?;
    let descent = unsafe { sys::function::<Metric>("CTFontGetDescent") }?;
    let draw_glyphs = unsafe { sys::function::<DrawGlyphs>("CTFontDrawGlyphs") }?;
    let bitmap_create = unsafe { sys::function::<BitmapCreate>("CGBitmapContextCreate") }?;
    let set_gray = unsafe { sys::function::<SetGray>("CGContextSetGrayFillColor") }?;
    let antialias = unsafe { sys::function::<SetFlag>("CGContextSetShouldAntialias") }?;
    let smooth = unsafe { sys::function::<SetFlag>("CGContextSetShouldSmoothFonts") }?;
    let fill_rect = unsafe { sys::function::<ContextFillRect>("CGContextFillRect") }?;
    let context_release = unsafe { sys::function::<Release>("CGContextRelease") }?;
    let cf_release = unsafe { sys::function::<Release>("CFRelease") }?;

    let name = sys::cf_string(FACE);
    // SAFETY: `name` is a `CFStringRef` or null, both of which
    // `CTFontCreateWithName` accepts - null asks for the system font, which is
    // the right answer if Menlo has somehow been removed. A null matrix means
    // the identity.
    let font = unsafe { font_create(name, CgFloat::from(size), std::ptr::null()) };
    if !name.is_null() {
        // SAFETY: `name` came from a `Create` function, so this balances it.
        unsafe { cf_release(name) };
    }
    if font.is_null() {
        return None;
    }

    // The characters, as UTF-16 - which for printable ASCII is the code point.
    let chars: Vec<u16> = (FIRST..=LAST).map(u16::from).collect();
    let mut glyphs = vec![0u16; chars.len()];
    // SAFETY: both slices are live and of the length passed. A `false` return
    // means some character had no glyph, which leaves that entry zero - the
    // missing-glyph glyph - and is drawn as such rather than refused.
    unsafe {
        get_glyphs(
            font,
            chars.as_ptr(),
            glyphs.as_mut_ptr(),
            chars.len() as isize,
        )
    };

    // One advance stands for all of them.
    let mut advances = vec![Size::default(); chars.len()];
    // SAFETY: as above, with a destination of the same length.
    unsafe {
        get_advances(
            font,
            ORIENTATION_DEFAULT,
            glyphs.as_ptr(),
            advances.as_mut_ptr(),
            glyphs.len() as isize,
        )
    };
    let advance = advances[(b'M' - FIRST) as usize].width.max(1.0);

    // SAFETY: `font` is a live `CTFontRef`.
    let (up, down) = unsafe { (ascent(font), descent(font)) };

    // A texel of pad on every side of a cell.
    const PAD: usize = 1;
    let cell_w = advance.ceil() as usize + PAD * 2;
    let cell_h = (up + down).ceil() as usize + PAD * 2;
    let rows = CELLS.div_ceil(COLUMNS);
    let (width, height) = (COLUMNS * cell_w, rows * cell_h);
    let mut pixels = vec![0u8; width * height];

    // A bitmap context over our own buffer: one byte per texel, no colour
    // space, rows exactly `width` bytes apart. Core Graphics stores the *first*
    // row of that buffer as the top of the image while addressing it with the
    // origin at the bottom, which is why the baseline below is computed from
    // the far edge.
    //
    // SAFETY: `pixels` is `width * height` bytes and outlives the context,
    // which is released before this function returns.
    let ctx = unsafe {
        bitmap_create(
            pixels.as_mut_ptr().cast(),
            width,
            height,
            8,
            width,
            std::ptr::null_mut(),
            ALPHA_ONLY,
        )
    };
    if ctx.is_null() {
        // SAFETY: `font` came from a `Create` function.
        unsafe { cf_release(font) };
        return None;
    }
    // SAFETY: `ctx` is a live context for all four calls. Antialiasing on and
    // font smoothing off: smoothing is subpixel antialiasing, which writes
    // three differently weighted channels, and this bitmap has one.
    unsafe {
        antialias(ctx, 1);
        smooth(ctx, 0);
        set_gray(ctx, 1.0, 1.0);
    }

    // Where each glyph goes.
    let positions: Vec<Point> = (0..glyphs.len())
        .map(|i| {
            let (col, row) = (i % COLUMNS, i / COLUMNS);
            Point {
                x: (col * cell_w + PAD) as CgFloat,
                y: (height - (row + 1) * cell_h) as CgFloat + down + PAD as CgFloat,
            }
        })
        .collect();
    // SAFETY: `glyphs` and `positions` are the same length, both live, and
    // `ctx` is the context they are drawn into.
    unsafe { draw_glyphs(font, glyphs.as_ptr(), positions.as_ptr(), glyphs.len(), ctx) };

    // The solid cell, filled through the same context so that it is opaque by
    // the same definition the glyphs are - a memset here would be a second
    // statement about what "fully covered" means.
    let (col, row) = (SOLID % COLUMNS, SOLID / COLUMNS);
    // SAFETY: `ctx` is live and the rectangle is inside it.
    unsafe {
        fill_rect(
            ctx,
            Rect {
                origin: Point {
                    x: (col * cell_w) as CgFloat,
                    y: (height - (row + 1) * cell_h) as CgFloat,
                },
                size: Size {
                    width: cell_w as CgFloat,
                    height: cell_h as CgFloat,
                },
            },
        )
    };

    // SAFETY: the context is done with - every drawing call above has already
    // written into `pixels` - and both objects came from `Create` functions.
    unsafe {
        context_release(ctx);
        cf_release(font);
    }

    Some(Atlas {
        pixels,
        width,
        height,
        cell_w,
        cell_h,
        advance: advance as f32,
        line_h: (up + down) as f32,
    })
}

/// Atlases already built, keyed by the pixel size that produced them.
static ATLASES: Mutex<Option<HashMap<u32, &'static Atlas>>> = Mutex::new(None);

/// The atlas for `size` pixels, building it if this is the first ask.
pub fn atlas(size: f32) -> Option<&'static Atlas> {
    let key = size.round().clamp(6.0, 96.0) as u32;
    let mut guard = ATLASES.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some(&found) = cache.get(&key) {
        return Some(found);
    }
    let built: &'static Atlas = Box::leak(Box::new(rasterize(key as f32)?));
    cache.insert(key, built);
    Some(built)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mean coverage of one cell, which is what "has ink" means here.
    fn ink(atlas: &Atlas, index: usize) -> f64 {
        let (col, row) = (index % COLUMNS, index / COLUMNS);
        let mut total = 0u64;
        for y in 0..atlas.cell_h {
            let start = (row * atlas.cell_h + y) * atlas.width + col * atlas.cell_w;
            total += atlas.pixels[start..start + atlas.cell_w]
                .iter()
                .map(|&p| u64::from(p))
                .sum::<u64>();
        }
        total as f64 / (atlas.cell_w * atlas.cell_h) as f64 / 255.0
    }

    /// CoreText draws, and draws where this module says it does.
    ///
    /// The three assertions are the three ways the atlas can be wrong without failing.
    #[test]
    fn rasterizes_into_the_cells_it_claims() {
        let Some(atlas) = atlas(16.0) else {
            // A machine without CoreText is a machine without an overlay, which
            // is a supported configuration rather than a failure.
            return;
        };
        assert_eq!(atlas.width, COLUMNS * atlas.cell_w);
        assert!(atlas.advance >= 1.0 && atlas.line_h >= 1.0);

        let cell = |c: u8| (c - FIRST) as usize;
        assert!(ink(atlas, cell(b'M')) > 0.05, "no ink in the cell for 'M'");
        assert_eq!(ink(atlas, cell(b' ')), 0.0, "the space glyph drew ink");
        assert_eq!(ink(atlas, SOLID), 1.0, "the solid cell is not opaque");
    }

    /// One atlas per size, and the same reference for the same size.
    #[test]
    fn caches_by_rounded_size() {
        let (Some(a), Some(b)) = (atlas(14.0), atlas(14.2)) else {
            return;
        };
        assert!(std::ptr::eq(a, b));
    }
}
