//! `NSOpenGLPixelFormat` and `NSOpenGLContext`, over SDL's GL.

use std::ffi::c_void;

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

#[repr(C)]
pub struct PixelFormat {
    isa: *const c_void,
    /// The `NSOpenGLPixelFormatAttribute` list, terminated as the guest wrote
    /// it. Leaked with the object, which never outlives the process.
    attributes: *mut Vec<u32>,
}

#[repr(C)]
pub struct Context {
    isa: *const c_void,
    sdl: sdl3::GlContext,
    /// The view, retained: a context outliving its view and then flushing would
    /// swap a window that had gone.
    view: Id,
    format: Id,
}

pub const PIXEL_FORMAT_INSTANCE_SIZE: usize = size_of::<PixelFormat>();
pub const CONTEXT_INSTANCE_SIZE: usize = size_of::<Context>();

/// `NSOpenGLPixelFormatAttribute`, the ones a program in this tree has asked
/// for.
///
/// The list is not the whole header. An attribute nobody has used is one whose
/// SDL counterpart has not been checked, and guessing at it would ask SDL for a
/// visual the program did not request.
mod attribute {
    pub const DOUBLE_BUFFER: u32 = 5;
    pub const STEREO: u32 = 6;
    pub const COLOR_SIZE: u32 = 8;
    pub const ALPHA_SIZE: u32 = 11;
    pub const DEPTH_SIZE: u32 = 12;
    pub const STENCIL_SIZE: u32 = 13;
    pub const ACCUM_SIZE: u32 = 14;
    pub const SAMPLE_BUFFERS: u32 = 55;
    pub const SAMPLES: u32 = 56;
    pub const ACCELERATED: u32 = 73;
    pub const NO_RECOVERY: u32 = 72;
    pub const OPENGL_PROFILE: u32 = 99;
    pub const SCREEN_MASK: u32 = 84;

    /// The `NSOpenGLProfileVersion*` values `OPENGL_PROFILE` takes.
    pub const PROFILE_LEGACY: u32 = 0x1000;
    pub const PROFILE_3_2_CORE: u32 = 0x3200;
    pub const PROFILE_4_1_CORE: u32 = 0x4100;

    /// Whether the attribute is followed by a value rather than standing alone.
    ///
    /// Getting this wrong reads the *next attribute* as this one's value and
    /// then skips a real attribute, so the list silently becomes a different
    /// list. Every entry above is classified here, and one that is not is
    /// refused by number.
    pub fn takes_value(attribute: u32) -> Option<bool> {
        Some(match attribute {
            DOUBLE_BUFFER | STEREO | ACCELERATED | NO_RECOVERY => false,
            COLOR_SIZE | ALPHA_SIZE | DEPTH_SIZE | STENCIL_SIZE | ACCUM_SIZE | SAMPLE_BUFFERS
            | SAMPLES | OPENGL_PROFILE | SCREEN_MASK => true,
            _ => return None,
        })
    }
}

fn format_of<'a>(receiver: Id) -> &'a mut PixelFormat {
    // SAFETY: a pixel format of this crate's.
    unsafe { &mut *(receiver as *mut PixelFormat) }
}

fn context_of<'a>(receiver: Id) -> &'a mut Context {
    // SAFETY: a context of this crate's.
    unsafe { &mut *(receiver as *mut Context) }
}

unsafe extern "C" fn init_with_attributes(receiver: Id, _sel: Sel, attributes: *const u32) -> Id {
    let mut list = Vec::new();
    if !attributes.is_null() {
        // A pixel format attribute list is terminated by a zero, and Apple's
        // own headers cap it at nothing in particular - so this stops at the
        // terminator or at a length no real list reaches, rather than walking
        // guest memory until it faults.
        for i in 0..256 {
            // SAFETY: the caller passed a zero-terminated list, and the bound
            // stops a missing terminator from running off the end.
            let value = unsafe { *attributes.add(i) };
            if value == 0 {
                break;
            }
            list.push(value);
        }
    }
    format_of(receiver).attributes = Box::into_raw(Box::new(list));
    receiver
}

/// Apply the format's attributes to SDL, for the context about to be created.
///
/// Every attribute the list holds is either mapped or **refused by number**.
/// Silently skipping one is how a program that asked for a depth buffer gets a
/// context without one and fails much later, drawing with no depth test and no
/// diagnostic anywhere.
fn apply(format: Id) {
    use sdl3::video::attr;
    let Some(list) = (match format.is_null() {
        true => None,
        // SAFETY: a pixel format this crate made; the pointer is its own.
        false => unsafe { format_of(format).attributes.as_ref() },
    }) else {
        return;
    };

    let mut i = 0;
    while i < list.len() {
        let name = list[i];
        let Some(takes_value) = attribute::takes_value(name) else {
            objc::fail!(
                "-[NSOpenGLContext initWithFormat:]: pixel format attribute {name} is not one \
                 this runtime knows, and skipping it would change the list"
            );
        };
        let value = match takes_value {
            true => *list.get(i + 1).unwrap_or(&0) as i32,
            false => 1,
        };
        i += 1 + takes_value as usize;

        match name {
            attribute::DOUBLE_BUFFER => sdl3::video::gl_set_attribute(attr::DOUBLEBUFFER, 1),
            attribute::STEREO => sdl3::video::gl_set_attribute(attr::STEREO, 1),
            attribute::ACCELERATED => sdl3::video::gl_set_attribute(attr::ACCELERATED_VISUAL, 1),
            // "Do not fall back to software" is SDL's accelerated-visual
            // request said the other way round.
            attribute::NO_RECOVERY => sdl3::video::gl_set_attribute(attr::ACCELERATED_VISUAL, 1),
            attribute::COLOR_SIZE => sdl3::video::gl_set_attribute(attr::BUFFER_SIZE, value),
            attribute::ALPHA_SIZE => sdl3::video::gl_set_attribute(attr::ALPHA_SIZE, value),
            attribute::DEPTH_SIZE => sdl3::video::gl_set_attribute(attr::DEPTH_SIZE, value),
            attribute::STENCIL_SIZE => sdl3::video::gl_set_attribute(attr::STENCIL_SIZE, value),
            attribute::SAMPLE_BUFFERS => {
                sdl3::video::gl_set_attribute(attr::MULTISAMPLEBUFFERS, value)
            }
            attribute::SAMPLES => sdl3::video::gl_set_attribute(attr::MULTISAMPLESAMPLES, value),
            // An accumulation buffer is not a thing SDL asks the driver for by
            // total size, and no GL 2.1 program in this tree has used one.
            attribute::ACCUM_SIZE => true,
            // The screen mask names a `CGDirectDisplayID` to prefer, which SDL
            // decides from where the window is instead.
            attribute::SCREEN_MASK => true,
            attribute::OPENGL_PROFILE => match value as u32 {
                attribute::PROFILE_LEGACY => {
                    sdl3::video::gl_set_attribute(attr::CONTEXT_PROFILE_MASK, sdl3::video::profile::COMPATIBILITY)
                }
                attribute::PROFILE_3_2_CORE => {
                    sdl3::video::gl_set_attribute(attr::CONTEXT_PROFILE_MASK, sdl3::video::profile::CORE)
                        && sdl3::video::gl_set_attribute(attr::CONTEXT_MAJOR_VERSION, 3)
                        && sdl3::video::gl_set_attribute(attr::CONTEXT_MINOR_VERSION, 2)
                }
                attribute::PROFILE_4_1_CORE => {
                    sdl3::video::gl_set_attribute(attr::CONTEXT_PROFILE_MASK, sdl3::video::profile::CORE)
                        && sdl3::video::gl_set_attribute(attr::CONTEXT_MAJOR_VERSION, 4)
                        && sdl3::video::gl_set_attribute(attr::CONTEXT_MINOR_VERSION, 1)
                }
                other => objc::fail!(
                    "-[NSOpenGLContext initWithFormat:]: NSOpenGLPFAOpenGLProfile {other:#x} is \
                     not a profile this runtime maps"
                ),
            },
            _ => unreachable!("every attribute above is one `takes_value` classified"),
        };
    }
}

unsafe extern "C" fn init_with_format(receiver: Id, _sel: Sel, format: Id, _share: Id) -> Id {
    let context = context_of(receiver);
    context.format = objc::arc::retain(format);
    receiver
}

/// `-setView:`, which is where the SDL context is actually made.
///
/// SDL needs a window to create a GL context against and AppKit does not, so
/// this is the first moment both halves are known.
unsafe extern "C" fn set_view(receiver: Id, _sel: Sel, view: Id) {
    let context = context_of(receiver);
    objc::arc::release(context.view);
    context.view = objc::arc::retain(view);
    if context.sdl.is_null() {
        // SAFETY: `view` is null or an object this runtime allocated.
        let window = unsafe { crate::view::window_of(view) };
        // SAFETY: null or a window this crate made.
        let sdl_window = unsafe { crate::window::sdl_window(window) };
        if sdl_window.is_null() {
            objc::fail!(
                "-[NSOpenGLContext setView:]: the view is not in a window this runtime made, so \
                 there is nothing to create a GL context against"
            );
        }
        apply(context.format);
        let Some(sdl) = sdl3::video::gl_create_context(sdl_window) else {
            objc::fail!(
                "-[NSOpenGLContext setView:]: SDL could not create a GL context: {}",
                sdl3::error()
            );
        };
        context.sdl = sdl;
    }
}

unsafe extern "C" fn view(receiver: Id, _sel: Sel) -> Id {
    context_of(receiver).view
}

/// The context that was made current last, which `+currentContext` answers.
static CURRENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The SDL window a context draws into, or null.
fn window_of(context: &Context) -> sdl3::Window {
    // SAFETY: the view is null or an object this runtime allocated.
    let window = unsafe { crate::view::window_of(context.view) };
    // SAFETY: null or a window this crate made.
    unsafe { crate::window::sdl_window(window) }
}

unsafe extern "C" fn make_current_context(receiver: Id, _sel: Sel) {
    let context = context_of(receiver);
    if context.sdl.is_null() {
        objc::fail!(
            "-[NSOpenGLContext makeCurrentContext]: this context has no view, so it was never \
             created"
        );
    }
    let window = window_of(context);
    if !sdl3::video::gl_make_current(window, context.sdl) {
        objc::fail!(
            "-[NSOpenGLContext makeCurrentContext]: SDL could not make the context current: {}",
            sdl3::error()
        );
    }
    CURRENT.store(receiver as usize, std::sync::atomic::Ordering::Release);
}

unsafe extern "C" fn current_context(_class: Id, _sel: Sel) -> Id {
    CURRENT.load(std::sync::atomic::Ordering::Acquire) as Id
}

unsafe extern "C" fn clear_current_context(_class: Id, _sel: Sel) {
    sdl3::video::gl_make_current(std::ptr::null_mut(), std::ptr::null_mut());
    CURRENT.store(0, std::sync::atomic::Ordering::Release);
}

/// `-flushBuffer`: the swap that puts a frame on the screen.
unsafe extern "C" fn flush_buffer(receiver: Id, _sel: Sel) {
    let context = context_of(receiver);
    let window = window_of(context);
    if !window.is_null() {
        sdl3::video::swap_window(window);
    }
}

/// `-setValues:forParameter:`, of which only the swap interval is honoured.
///
/// `NSOpenGLContextParameterSwapInterval` is 222 and is the one a program sets;
/// the rest describe a surface AppKit composites and SDL does not. Refused by
/// number rather than ignored, because a program setting a parameter expects it
/// to take.
unsafe extern "C" fn set_values(receiver: Id, _sel: Sel, values: *const i32, parameter: isize) {
    let _ = receiver;
    const SWAP_INTERVAL: isize = 222;
    const SURFACE_OPACITY: isize = 236;
    // SAFETY: the guest passed a readable array of at least one integer, which
    // every parameter here takes.
    let value = match values.is_null() {
        true => 0,
        false => unsafe { *values },
    };
    match parameter {
        SWAP_INTERVAL => {
            sdl3::video::gl_set_swap_interval(value);
        }
        // Opacity is the compositor's, and SDL's window is opaque.
        SURFACE_OPACITY => {}
        other => objc::fail!(
            "-[NSOpenGLContext setValues:forParameter:]: parameter {other} is not one this \
             runtime maps"
        ),
    }
}

unsafe extern "C" fn update(_receiver: Id, _sel: Sel) {}

unsafe extern "C" fn pixel_format(receiver: Id, _sel: Sel) -> Id {
    context_of(receiver).format
}

pub(crate) fn install() {
    let format = objc::class::lookup(c"NSOpenGLPixelFormat").expect("declared before its methods");
    methods!(format, {
        "initWithAttributes:" => init_with_attributes, c"@24@0:8^I16";
    });

    let context = objc::class::lookup(c"NSOpenGLContext").expect("declared before its methods");
    class_methods!(context, {
        "currentContext" => current_context, c"@16@0:8";
        "clearCurrentContext" => clear_current_context, c"v16@0:8";
    });
    methods!(context, {
        "initWithFormat:shareContext:" => init_with_format, c"@32@0:8@16@24";
        "setView:" => set_view, c"v24@0:8@16";
        "view" => view, c"@16@0:8";
        "pixelFormat" => pixel_format, c"@16@0:8";
        "makeCurrentContext" => make_current_context, c"v16@0:8";
        "flushBuffer" => flush_buffer, c"v16@0:8";
        "update" => update, c"v16@0:8";
        "setValues:forParameter:" => set_values, c"v32@0:8^i16q24";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every attribute the mapping handles is one `takes_value` classifies, and
    /// the two lists cannot drift: an attribute in one and not the other is
    /// either a refusal for something that works or a silent skip.
    #[test]
    fn every_mapped_attribute_is_classified() {
        for name in [
            attribute::DOUBLE_BUFFER,
            attribute::STEREO,
            attribute::COLOR_SIZE,
            attribute::ALPHA_SIZE,
            attribute::DEPTH_SIZE,
            attribute::STENCIL_SIZE,
            attribute::ACCUM_SIZE,
            attribute::SAMPLE_BUFFERS,
            attribute::SAMPLES,
            attribute::ACCELERATED,
            attribute::NO_RECOVERY,
            attribute::OPENGL_PROFILE,
            attribute::SCREEN_MASK,
        ] {
            assert!(
                attribute::takes_value(name).is_some(),
                "{name} is mapped but not classified"
            );
        }
    }

    /// An attribute nobody has described is refused rather than guessed at,
    /// which is what stops a list from being silently reinterpreted.
    #[test]
    fn an_unknown_attribute_is_not_classified() {
        assert_eq!(attribute::takes_value(199), None);
    }

    /// The list a program hands in is the list that comes back, terminator and
    /// all - which is what `apply` walks.
    #[test]
    fn an_attribute_list_is_read_up_to_its_terminator() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSOpenGLPixelFormat").expect("NSOpenGLPixelFormat");
        let object = objc::object::create_instance(class, 0);
        let attributes = [
            attribute::DOUBLE_BUFFER,
            attribute::COLOR_SIZE,
            24,
            attribute::DEPTH_SIZE,
            24,
            0,
            99,
        ];
        // SAFETY: `-initWithAttributes:` is `id (id, SEL, const uint32_t *)`.
        unsafe {
            init_with_attributes(
                object,
                objc::sel::get("initWithAttributes:"),
                attributes.as_ptr(),
            )
        };
        // SAFETY: the list this call just made.
        let list = unsafe { &*format_of(object).attributes };
        assert_eq!(list.as_slice(), &attributes[..5]);
    }
}
