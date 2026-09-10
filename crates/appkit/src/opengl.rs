//! `NSOpenGLPixelFormat` and `NSOpenGLContext`, over SDL's GL.

use std::ffi::c_void;

use objc::object::Id;
use objc::sel::Sel;
use static_init::dynamic;

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
    /// The `CGLContextObj` behind the SDL context, recorded the first time it is
    /// made current - see [`cgl_context`].
    cgl: *mut c_void,
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
    // Every `NSOpenGLPixelFormatAttribute` in `NSOpenGL.h`, whether or not it
    // maps to anything. The list has to be complete rather than convenient:
    // `takes_value` is what tells the walk how far to step, so an attribute
    // missing from it stops the walk - and one classified wrongly reads the
    // next attribute as this one's value and silently turns the list into a
    // different list.
    pub const ALL_RENDERERS: u32 = 1;
    pub const TRIPLE_BUFFER: u32 = 3;
    pub const DOUBLE_BUFFER: u32 = 5;
    pub const STEREO: u32 = 6;
    pub const AUX_BUFFERS: u32 = 7;
    pub const COLOR_SIZE: u32 = 8;
    pub const ALPHA_SIZE: u32 = 11;
    pub const DEPTH_SIZE: u32 = 12;
    pub const STENCIL_SIZE: u32 = 13;
    pub const ACCUM_SIZE: u32 = 14;
    pub const MINIMUM_POLICY: u32 = 51;
    pub const MAXIMUM_POLICY: u32 = 52;
    pub const OFF_SCREEN: u32 = 53;
    pub const FULL_SCREEN: u32 = 54;
    pub const SAMPLE_BUFFERS: u32 = 55;
    pub const SAMPLES: u32 = 56;
    pub const AUX_DEPTH_STENCIL: u32 = 57;
    pub const COLOR_FLOAT: u32 = 58;
    pub const MULTISAMPLE: u32 = 59;
    pub const SUPERSAMPLE: u32 = 60;
    pub const SAMPLE_ALPHA: u32 = 61;
    pub const RENDERER_ID: u32 = 70;
    pub const SINGLE_RENDERER: u32 = 71;
    pub const NO_RECOVERY: u32 = 72;
    pub const ACCELERATED: u32 = 73;
    pub const CLOSEST_POLICY: u32 = 74;
    pub const ROBUST: u32 = 75;
    pub const BACKING_STORE: u32 = 76;
    pub const MP_SAFE: u32 = 78;
    pub const WINDOW: u32 = 80;
    pub const MULTI_SCREEN: u32 = 81;
    pub const COMPLIANT: u32 = 83;
    pub const SCREEN_MASK: u32 = 84;
    pub const PIXEL_BUFFER: u32 = 90;
    pub const REMOTE_PIXEL_BUFFER: u32 = 91;
    pub const ALLOW_OFFLINE_RENDERERS: u32 = 96;
    pub const ACCELERATED_COMPUTE: u32 = 97;
    pub const OPENGL_PROFILE: u32 = 99;
    pub const VIRTUAL_SCREEN_COUNT: u32 = 128;

    /// The `NSOpenGLProfileVersion*` values `OPENGL_PROFILE` takes.
    pub const PROFILE_LEGACY: u32 = 0x1000;
    pub const PROFILE_3_2_CORE: u32 = 0x3200;
    pub const PROFILE_4_1_CORE: u32 = 0x4100;

    /// The attributes that are followed by a value, per the header's own
    /// comments - "number of", "bit mask of", "request ... by ID".
    pub const VALUED: &[u32] = &[
        AUX_BUFFERS,
        COLOR_SIZE,
        ALPHA_SIZE,
        DEPTH_SIZE,
        STENCIL_SIZE,
        ACCUM_SIZE,
        SAMPLE_BUFFERS,
        SAMPLES,
        RENDERER_ID,
        SCREEN_MASK,
        OPENGL_PROFILE,
        VIRTUAL_SCREEN_COUNT,
    ];

    /// The attributes that stand alone, each meaning "yes" by being present.
    pub const STANDALONE: &[u32] = &[
        ALL_RENDERERS,
        TRIPLE_BUFFER,
        DOUBLE_BUFFER,
        STEREO,
        MINIMUM_POLICY,
        MAXIMUM_POLICY,
        OFF_SCREEN,
        FULL_SCREEN,
        AUX_DEPTH_STENCIL,
        COLOR_FLOAT,
        MULTISAMPLE,
        SUPERSAMPLE,
        SAMPLE_ALPHA,
        SINGLE_RENDERER,
        NO_RECOVERY,
        ACCELERATED,
        CLOSEST_POLICY,
        ROBUST,
        BACKING_STORE,
        MP_SAFE,
        WINDOW,
        MULTI_SCREEN,
        COMPLIANT,
        PIXEL_BUFFER,
        REMOTE_PIXEL_BUFFER,
        ALLOW_OFFLINE_RENDERERS,
        ACCELERATED_COMPUTE,
    ];

    /// Whether the attribute is followed by a value rather than standing alone.
    pub fn takes_value(attribute: u32) -> Option<bool> {
        if VALUED.contains(&attribute) {
            return Some(true);
        }
        STANDALONE.contains(&attribute).then_some(false)
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
            // The ones SDL asks the driver for directly.
            attribute::DOUBLE_BUFFER => sdl3::video::gl_set_attribute(attr::DOUBLEBUFFER, 1),
            attribute::TRIPLE_BUFFER => sdl3::video::gl_set_attribute(attr::DOUBLEBUFFER, 1),
            attribute::STEREO => sdl3::video::gl_set_attribute(attr::STEREO, 1),
            attribute::COLOR_SIZE => sdl3::video::gl_set_attribute(attr::BUFFER_SIZE, value),
            attribute::ALPHA_SIZE => sdl3::video::gl_set_attribute(attr::ALPHA_SIZE, value),
            attribute::DEPTH_SIZE => sdl3::video::gl_set_attribute(attr::DEPTH_SIZE, value),
            attribute::STENCIL_SIZE => sdl3::video::gl_set_attribute(attr::STENCIL_SIZE, value),
            attribute::SAMPLE_BUFFERS | attribute::MULTISAMPLE => {
                sdl3::video::gl_set_attribute(attr::MULTISAMPLEBUFFERS, value.max(1))
            }
            attribute::SAMPLES => sdl3::video::gl_set_attribute(attr::MULTISAMPLESAMPLES, value),
            // "Accelerated", "no recovery" and "no software renderer" are three
            // spellings of the same request.
            attribute::ACCELERATED | attribute::NO_RECOVERY | attribute::ACCELERATED_COMPUTE => {
                sdl3::video::gl_set_attribute(attr::ACCELERATED_VISUAL, 1)
            }
            attribute::OPENGL_PROFILE => match value as u32 {
                attribute::PROFILE_LEGACY => sdl3::video::gl_set_attribute(
                    attr::CONTEXT_PROFILE_MASK,
                    sdl3::video::profile::COMPATIBILITY,
                ),
                attribute::PROFILE_3_2_CORE => {
                    sdl3::video::gl_set_attribute(
                        attr::CONTEXT_PROFILE_MASK,
                        sdl3::video::profile::CORE,
                    ) && sdl3::video::gl_set_attribute(attr::CONTEXT_MAJOR_VERSION, 3)
                        && sdl3::video::gl_set_attribute(attr::CONTEXT_MINOR_VERSION, 2)
                }
                attribute::PROFILE_4_1_CORE => {
                    sdl3::video::gl_set_attribute(
                        attr::CONTEXT_PROFILE_MASK,
                        sdl3::video::profile::CORE,
                    ) && sdl3::video::gl_set_attribute(attr::CONTEXT_MAJOR_VERSION, 4)
                        && sdl3::video::gl_set_attribute(attr::CONTEXT_MINOR_VERSION, 1)
                }
                other => objc::fail!(
                    "-[NSOpenGLContext initWithFormat:]: NSOpenGLPFAOpenGLProfile {other:#x} is \
                     not a profile this runtime maps"
                ),
            },

            // ## Carried, and deliberately not asked for
            //
            // Each of these describes a choice SDL has already made, or a
            // renderer selection that means nothing when there is one driver.
            // They are listed rather than caught by a wildcard so that the set
            // is a decision a reader can check against the header, which is the
            // whole point of refusing an attribute that is in neither list.
            //
            // The policies say how to pick among visuals SDL picks for us; the
            // renderer selectors name a specific GPU; a window, an offscreen or
            // a pixel buffer is a *drawable kind*, and the drawable here is
            // always the window SDL made; an accumulation buffer is not
            // something SDL requests by total size, and no GL 2.1 program in
            // this tree has used one.
            attribute::ALL_RENDERERS
            | attribute::AUX_BUFFERS
            | attribute::ACCUM_SIZE
            | attribute::MINIMUM_POLICY
            | attribute::MAXIMUM_POLICY
            | attribute::CLOSEST_POLICY
            | attribute::AUX_DEPTH_STENCIL
            | attribute::COLOR_FLOAT
            | attribute::SUPERSAMPLE
            | attribute::SAMPLE_ALPHA
            | attribute::RENDERER_ID
            | attribute::SINGLE_RENDERER
            | attribute::ROBUST
            | attribute::BACKING_STORE
            | attribute::MP_SAFE
            | attribute::COMPLIANT
            | attribute::MULTI_SCREEN
            | attribute::ALLOW_OFFLINE_RENDERERS
            | attribute::SCREEN_MASK
            | attribute::VIRTUAL_SCREEN_COUNT
            | attribute::WINDOW
            | attribute::OFF_SCREEN
            | attribute::FULL_SCREEN
            | attribute::PIXEL_BUFFER
            | attribute::REMOTE_PIXEL_BUFFER => true,

            _ => unreachable!("every attribute above is one `takes_value` classified"),
        };
    }
}

unsafe extern "C" fn init_with_format(receiver: Id, _sel: Sel, format: Id, _share: Id) -> Id {
    let context = context_of(receiver);
    context.format = objc::arc::retain(format);
    receiver
}

/// `-setView:`, which records the drawable and nothing more.
///
/// Creating the SDL context here would be the obvious place and is the wrong
/// one: AppKit lets a program make a context and send it `-makeCurrentContext`
/// *before* it has a view - the runner does exactly that - and a context created
/// only on `-setView:` is nil at the moment it is first used.
unsafe extern "C" fn set_view(receiver: Id, _sel: Sel, view: Id) {
    let context = context_of(receiver);
    objc::arc::release(context.view);
    context.view = objc::arc::retain(view);
}

/// `-clearDrawable`, which only lets go of the view.
///
/// The SDL context stays. It is not bound to a window until it is made current,
/// so a later `-setView:` replaces the drawable on its own and there is nothing
/// here that tearing the context down would fix.
unsafe extern "C" fn clear_drawable(receiver: Id, _sel: Sel) {
    let context = context_of(receiver);
    objc::arc::release(context.view);
    context.view = std::ptr::null_mut();
}

/// The window a context should draw into: its view's, or - before it has one -
/// whichever window the program has made.
///
/// SDL needs a window to *create* a context against, and AppKit does not. The
/// window is not part of the context on either side, though: `SDL_GL_MakeCurrent`
/// takes it again on every call, so binding a context made against one window to
/// another later is what the API is for rather than a workaround.
/// The `NSWindow` a context draws into, which is the one its view is in and
/// otherwise the program's current window.
fn drawable_window(context: &Context) -> Id {
    // SAFETY: the view is null or an object this runtime allocated.
    let window = unsafe { crate::view::window_of(context.view) };
    // SAFETY: null or a window this crate made.
    if !unsafe { crate::window::sdl_window(window) }.is_null() {
        return window;
    }
    match crate::window::key_window().is_null() {
        false => crate::window::key_window(),
        true => crate::window::all()
            .last()
            .copied()
            .unwrap_or(std::ptr::null_mut()),
    }
}

fn drawable(context: &Context) -> sdl3::Window {
    // SAFETY: null or a window this crate made.
    unsafe { crate::window::sdl_window(drawable_window(context)) }
}

/// Make the SDL context, if it is not there yet.
fn ensure(receiver: Id, sent: &str) -> sdl3::Window {
    let context = context_of(receiver);
    let object = drawable_window(context);
    // SAFETY: null or a window this crate made.
    if unsafe { crate::window::sdl_window(object) }.is_null() {
        objc::fail!(
            "-[NSOpenGLContext {sent}]: there is no window to create a GL context against - the \
             program has not made one yet"
        );
    }
    // A window is not made for OpenGL until a guest asks for a GL context,
    // because a window made for OpenGL cannot afterwards be presented to
    // with Vulkan. This is the asking, so the window is rebuilt now.
    if !crate::window::rebuild_for_opengl(object) {
        objc::fail!(
            "-[NSOpenGLContext {sent}]: SDL could not make this window an OpenGL window: {}",
            sdl3::error()
        );
    }
    // The rebuild replaces it, so it is read back rather than remembered.
    // SAFETY: a window this crate made, checked above.
    let window = unsafe { crate::window::sdl_window(object) };
    if context.sdl.is_null() {
        apply(context.format);
        let Some(sdl) = sdl3::video::gl_create_context(window) else {
            objc::fail!(
                "-[NSOpenGLContext {sent}]: SDL could not create a GL context: {}",
                sdl3::error()
            );
        };
        context.sdl = sdl;
    }
    window
}

unsafe extern "C" fn view(receiver: Id, _sel: Sel) -> Id {
    context_of(receiver).view
}

/// The context that was made current last, which `+currentContext` answers.
static CURRENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe extern "C" fn make_current_context(receiver: Id, _sel: Sel) {
    let window = ensure(receiver, "makeCurrentContext");
    let context = context_of(receiver);
    if !sdl3::video::gl_make_current(window, context.sdl) {
        objc::fail!(
            "-[NSOpenGLContext makeCurrentContext]: SDL could not make the context current: {}",
            sdl3::error()
        );
    }
    CURRENT.store(receiver as usize, std::sync::atomic::Ordering::Release);
    if context.cgl.is_null() {
        context.cgl = current_cgl_context();
    }
}

/// The CGL context current on this thread, or null where there is no CGL.
///
/// Through SDL's loader rather than a link: `CGLGetCurrentContext` lives in the
/// OpenGL framework, which is what SDL opened to make the context, and asking
/// SDL for it keeps this file building where that framework does not exist.
fn current_cgl_context() -> *mut c_void {
    let Some(f) = sdl3::video::gl_proc_address(c"CGLGetCurrentContext") else {
        return std::ptr::null_mut();
    };
    // SAFETY: `CGLContextObj CGLGetCurrentContext(void)`.
    unsafe {
        let f: unsafe extern "C" fn() -> *mut c_void = std::mem::transmute(f);
        f()
    }
}

/// `-CGLContextObj`, the context SDL made underneath this one.
///
/// Recorded when the context is made current, because the only way to name
/// SDL's is to ask the thread it is current on. A receiver that has never been
/// current is made current here: the alternative is answering null, and the CGL
/// calls a program makes with it - `CGLEnable`, `CGLSetParameter` - would then
/// silently do nothing to the context it meant.
unsafe extern "C" fn cgl_context(receiver: Id, sel: Sel) -> *mut c_void {
    if context_of(receiver).cgl.is_null() {
        // SAFETY: the receiver is a context of this crate's.
        unsafe { make_current_context(receiver, sel) };
    }
    context_of(receiver).cgl
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
    let window = drawable(context_of(receiver));
    if !window.is_null() {
        sdl3::video::swap_window(window);
    }
}

#[inline(always)]
fn swap_interval() -> Option<i32> {
    #[dynamic]
    static SWAP_INTERVAL: Option<i32> = {
        if let Ok(no_vsync) = std::env::var("ROSETTA3_SWAP_INTERVAL") {
            no_vsync.parse().ok()
        } else {
            None
        }
    };
    *SWAP_INTERVAL
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
            let actual_value = swap_interval().unwrap_or(value);
            sdl3::video::gl_set_swap_interval(actual_value);
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
        "clearDrawable" => clear_drawable, c"v16@0:8";
        "CGLContextObj" => cgl_context, c"^v16@0:8";
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

    /// No attribute is in both lists, and every one is in a list.
    ///
    /// The two are what the walk steps by, so an attribute in neither stops a
    /// program that was going to work and one in both is a step of the wrong
    /// length - after which every later attribute is read as something else.
    #[test]
    fn the_two_classifications_are_a_partition() {
        for name in attribute::VALUED {
            assert!(
                !attribute::STANDALONE.contains(name),
                "{name} is in both lists"
            );
            assert_eq!(attribute::takes_value(*name), Some(true), "{name}");
        }
        for name in attribute::STANDALONE {
            assert_eq!(attribute::takes_value(*name), Some(false), "{name}");
        }
    }

    /// And the union is the whole of `NSOpenGL.h`, which is what stops a program
    /// being refused for an attribute Apple has had since 10.0.
    ///
    /// The count rather than the values: the values are the two lists above, and
    /// asserting them against themselves would say nothing.
    #[test]
    fn every_attribute_the_header_declares_is_classified() {
        assert_eq!(
            attribute::VALUED.len() + attribute::STANDALONE.len(),
            39,
            "NSOpenGL.h declares thirty-nine pixel format attributes"
        );
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
