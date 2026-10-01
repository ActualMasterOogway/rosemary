//! How an `NSTextField` looks - its font, colours and alignment - and the
//! drawing and size that follow from them.
//!
//! One line of text, drawn by [`chrome`]'s renderer: what a label is. An
//! editable field shows its editor's text the same way while it is not being
//! edited.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use foundation::geometry::{NSPoint, NSRect, NSSize};
use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;
use crate::view;

/// What a field was configured with. Colours and the font are retained.
#[derive(Clone, Copy)]
struct Look {
    font: usize,
    text_color: usize,
    background: usize,
    draws_background: bool,
    bordered: bool,
    alignment: isize,
}

impl Default for Look {
    fn default() -> Self {
        Look {
            font: 0,
            text_color: 0,
            background: 0,
            draws_background: false,
            bordered: false,
            alignment: NATURAL,
        }
    }
}

/// `NSTextAlignment` on macOS, where right is 1 and centre 2.
const RIGHT: isize = 1;
const CENTER: isize = 2;
const NATURAL: isize = 4;

/// The room `NSTextFieldCell` leaves either side of its text.
const PADDING: f64 = 2.0;

/// AppKit's system font size, for a field given no font.
const SYSTEM_SIZE: f64 = 13.0;

fn looks() -> &'static Mutex<HashMap<usize, Look>> {
    static LOOKS: OnceLock<Mutex<HashMap<usize, Look>>> = OnceLock::new();
    LOOKS.get_or_init(Default::default)
}

fn look(receiver: Id) -> Look {
    looks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(receiver as usize))
        .copied()
        .unwrap_or_default()
}

fn update(receiver: Id, with: impl FnOnce(&mut Look)) {
    with(
        looks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(receiver as usize)
            .or_default(),
    );
    changed(receiver);
}

/// Redraw a field whose text or look changed, and let the stacks and
/// constraints it sits in size it again.
pub(crate) fn changed(receiver: Id) {
    crate::backing::needs_display(receiver);
    crate::layout::solve();
}

/// Replace a retained object in a field's look.
fn store(receiver: Id, value: Id, slot: impl FnOnce(&mut Look) -> &mut usize) {
    let retained = objc::arc::retain(value) as usize;
    let mut old = 0;
    update(receiver, |look| {
        old = std::mem::replace(slot(look), retained)
    });
    objc::arc::release(old as Id);
}

pub(crate) fn forget(receiver: Id) {
    let gone = looks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(receiver as usize));
    if let Some(look) = gone {
        for object in [look.font, look.text_color, look.background] {
            objc::arc::release(object as Id);
        }
    }
}

/// The font a field draws with, and its size in points.
fn font_of(look: &Look) -> Option<(Arc<chrome::Font>, f64)> {
    if let Some((face, size)) = crate::coretext::face_of(look.font as *const _)
        && let Some(font) = chrome::Font::from_faces(vec![face])
    {
        return Some((Arc::new(font), if size > 0.0 { size } else { SYSTEM_SIZE }));
    }
    Some((chrome::Font::system(400)?, SYSTEM_SIZE))
}

fn color_of(color: usize, default: chrome::Color) -> chrome::Color {
    if color == 0 {
        return default;
    }
    // SAFETY: a colour the guest set, retained here.
    let (r, g, b, a) = unsafe { crate::color::components(color as Id) };
    chrome::Color {
        r: r as f32,
        g: g as f32,
        b: b as f32,
        a: a as f32,
    }
}

fn is_text_field(object: Id) -> bool {
    // SAFETY: null or an object this runtime allocated.
    unsafe { objc::object::class_of(object) }.is_some_and(|c| {
        objc::class::lookup(c"NSTextField").is_some_and(|k| c.chain().any(|x| std::ptr::eq(x, k)))
    })
}

/// The size a field's text needs, which is what AppKit's
/// `-intrinsicContentSize` says and what a stack sizes a label to.
pub(crate) fn intrinsic_size(receiver: Id) -> Option<NSSize> {
    if !is_text_field(receiver) {
        return None;
    }
    let look = look(receiver);
    let (font, size) = font_of(&look)?;
    let text = crate::text::field_text(receiver);
    let (ascent, descent) = font.metrics(size as f32);
    Some(NSSize {
        width: (f64::from(font.measure(&text, size as f32)) + 2.0 * PADDING).ceil(),
        height: f64::from(ascent + descent).ceil(),
    })
}

unsafe extern "C" fn intrinsic_content_size(receiver: Id, _sel: Sel) -> NSSize {
    intrinsic_size(receiver).unwrap_or(NSSize {
        // `NSViewNoIntrinsicMetric`.
        width: -1.0,
        height: -1.0,
    })
}

unsafe extern "C" fn draw_rect(receiver: Id, _sel: Sel, _dirty: NSRect) {
    let look = look(receiver);
    let bounds = view::bounds_of(receiver);
    let density = crate::backing::focus_density();
    let width = (bounds.size.width * density).ceil() as usize;
    let height = (bounds.size.height * density).ceil() as usize;
    if width == 0 || height == 0 {
        return;
    }
    let mut pixels = vec![0u8; width * height * 4];
    let mut canvas = chrome::Canvas::new(&mut pixels, width, height, width * 4, density as f32);
    let (w, h) = (bounds.size.width as f32, bounds.size.height as f32);
    if look.draws_background && look.background != 0 {
        let fill = color_of(look.background, chrome::Color::rgba(0, 0, 0, 0.0));
        canvas.fill_rect(chrome::Rect::new(0.0, 0.0, w, h), fill);
    }
    if look.bordered {
        let edge = chrome::Color::rgba(0, 0, 0, 0.25);
        canvas.stroke_rounded_rect(
            chrome::Rect::new(0.5, 0.5, w - 1.0, h - 1.0),
            0.0,
            1.0,
            edge,
        );
    }
    let text = crate::text::field_text(receiver);
    if let Some((font, size)) = font_of(&look)
        && !text.is_empty()
    {
        let size = size as f32;
        let room = (w - 2.0 * PADDING as f32).max(0.0);
        let line = font.truncate(&text, size, room);
        let advance = font.measure(&line, size);
        let x = match look.alignment {
            RIGHT => w - PADDING as f32 - advance,
            CENTER => (w - advance) / 2.0,
            _ => PADDING as f32,
        };
        let baseline = font.centered_baseline(size, 0.0, h);
        let color = color_of(look.text_color, chrome::Color::rgba(0, 0, 0, 0.85));
        canvas.draw_text(&line, &font, size, chrome::Point::new(x, baseline), color);
    }
    crate::backing::draw_image(
        &pixels,
        width,
        height,
        bounds.size,
        bounds,
        NSRect {
            origin: NSPoint::default(),
            size: bounds.size,
        },
        // Source-over.
        2,
        1.0,
        true,
    );
}

unsafe extern "C" fn set_font(receiver: Id, _sel: Sel, font: Id) {
    store(receiver, font, |look| &mut look.font);
}

unsafe extern "C" fn font(receiver: Id, _sel: Sel) -> Id {
    look(receiver).font as Id
}

unsafe extern "C" fn set_text_color(receiver: Id, _sel: Sel, color: Id) {
    store(receiver, color, |look| &mut look.text_color);
}

unsafe extern "C" fn text_color(receiver: Id, _sel: Sel) -> Id {
    look(receiver).text_color as Id
}

unsafe extern "C" fn set_background_color(receiver: Id, _sel: Sel, color: Id) {
    store(receiver, color, |look| &mut look.background);
}

unsafe extern "C" fn background_color(receiver: Id, _sel: Sel) -> Id {
    look(receiver).background as Id
}

unsafe extern "C" fn set_draws_background(receiver: Id, _sel: Sel, flag: u8) {
    update(receiver, |look| look.draws_background = flag != 0);
}

unsafe extern "C" fn draws_background(receiver: Id, _sel: Sel) -> u8 {
    u8::from(look(receiver).draws_background)
}

unsafe extern "C" fn set_bordered(receiver: Id, _sel: Sel, flag: u8) {
    update(receiver, |look| look.bordered = flag != 0);
}

unsafe extern "C" fn is_bordered(receiver: Id, _sel: Sel) -> u8 {
    u8::from(look(receiver).bordered)
}

unsafe extern "C" fn set_alignment(receiver: Id, _sel: Sel, alignment: isize) {
    update(receiver, |look| look.alignment = alignment);
}

unsafe extern "C" fn alignment(receiver: Id, _sel: Sel) -> isize {
    look(receiver).alignment
}

pub(crate) fn install() {
    let field = objc::class::lookup(c"NSTextField").expect("declared before its methods");
    methods!(field, {
        "setFont:" => set_font, c"v24@0:8@16";
        "font" => font, c"@16@0:8";
        "setTextColor:" => set_text_color, c"v24@0:8@16";
        "textColor" => text_color, c"@16@0:8";
        "setBackgroundColor:" => set_background_color, c"v24@0:8@16";
        "backgroundColor" => background_color, c"@16@0:8";
        "setDrawsBackground:" => set_draws_background, c"v20@0:8c16";
        "drawsBackground" => draws_background, c"c16@0:8";
        "setBordered:" => set_bordered, c"v20@0:8c16";
        "isBordered" => is_bordered, c"c16@0:8";
        "setAlignment:" => set_alignment, c"v24@0:8q16";
        "alignment" => alignment, c"q16@0:8";
        "intrinsicContentSize" => intrinsic_content_size, c"{CGSize=dd}16@0:8";
        "drawRect:" => draw_rect, c"v48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_is_as_wide_as_its_text_and_centres_it() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSTextField").expect("NSTextField");
        let label = objc::object::create_instance(class, 0);
        let empty = intrinsic_size(label).expect("a text field has a size");
        let text = foundation::string::from_str("Messages");
        // SAFETY: each method is sent with the prototype it is installed with.
        unsafe {
            crate::text::set_field_text(label, text);
            set_alignment(label, objc::sel::get("setAlignment:"), CENTER);
        }
        objc::arc::release(text);
        let sized = intrinsic_size(label).expect("a text field has a size");
        assert!(sized.width > empty.width);
        assert_eq!(sized.height, empty.height);
        assert_eq!(look(label).alignment, CENTER);
        forget(label);
    }
}
