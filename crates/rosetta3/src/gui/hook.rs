//! Where a frame ends, on both graphics APIs a Mac program uses.
//!
//! ## The one thing an overlay has to find
//!
//! Not the window, and not the swap chain: the **moment the guest has finished
//! drawing and has not yet shown it**. There is exactly one such moment per API
//! and both of them are calls the guest makes out of itself, which is the kind
//! of thing this runtime already intercepts for a living.
//!
//! | API | the call | how it is caught |
//! |---|---|---|
//! | Metal | `-[MTLCommandBuffer presentDrawable:]` | a selector, in [`crate::objc::dispatch`] |
//! | OpenGL | `-[NSOpenGLContext flushBuffer]` | the same |
//! | OpenGL | `CGLFlushDrawable` | a shim, in [`crate::shims`] |
//!
//! Both OpenGL entry points are needed and they are not alternatives: AppKit's
//! wrapper is what a Cocoa program calls, and a Unity player written before
//! Metal talks to CGL directly — which is the same split
//! [`crate::abi::table`] already documents around the `CGL*` family.
//!
//! ## What this costs when the overlay is doing nothing
//!
//! For the shim: nothing measurable, because it is one call per frame. For the
//! selector hook: a load of a cached array and at most four integer compares,
//! per message the guest sends. That is a real cost on a real hot path, so it
//! is paid in the cheapest form there is — the selectors are **uniqued
//! pointers**, compared as integers, rather than names compared as strings. A
//! `sel_getName` here would be a string comparison per message and would be
//! indefensible.
//!
//! ## What is not caught, and why that is stated rather than fixed
//!
//! `-[CAMetalDrawable present]`, sent directly rather than through a command
//! buffer. At that point the guest's command buffer has already been committed
//! and there is nothing left to encode into: drawing would need a command
//! buffer of this runtime's own, racing the guest for the same texture with no
//! ordering between them. A program that presents that way gets no overlay, and
//! that is the correct outcome rather than a corrupted frame.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{GuestMemory, Result};
use crate::shims::coregraphics::Shim;

/// The selectors a present goes through, uniqued once.
///
/// Registering a selector *creates* it if the runtime has not seen it, which is
/// harmless — a selector is a string in a table — and means this works the same
/// whether or not the guest has ever sent one.
struct Selectors {
    /// The three spellings that carry a command buffer *and* a drawable.
    present_drawable: [u64; 3],
    /// The three that a drawable answers on its own, with the guest's command
    /// buffer already committed. See [`super::metal::present_standalone`].
    present_self: [u64; 3],
    /// Not a present at all, and watched anyway: it is where the guest's command
    /// queue is learned, which is what the previous line needs.
    commit: u64,
    flush_buffer: u64,
}

fn selectors() -> &'static Selectors {
    static SELECTORS: std::sync::OnceLock<Selectors> = std::sync::OnceLock::new();
    SELECTORS.get_or_init(|| {
        let sel = |name| super::objc::sel(name) as u64;
        Selectors {
            present_drawable: [
                sel("presentDrawable:"),
                sel("presentDrawable:atTime:"),
                sel("presentDrawable:afterMinimumDuration:"),
            ],
            present_self: [
                sel("present"),
                sel("presentAtTime:"),
                sel("presentAfterMinimumDuration:"),
            ],
            commit: sel("commit"),
            flush_buffer: sel("flushBuffer"),
        }
    })
}

/// Selectors reported the first time the guest sends one, under
/// `ROSETTA3_GUI_DEBUG`.
///
/// **Written because a guest that draws nothing tells you nothing.** Roblox
/// produced no overlay and no diagnostic at all, which narrowed the problem to
/// one thing — the hook was not firing — and left the actual question wide open:
/// *what is it sending instead?* From outside the process that is unanswerable,
/// and the guess this file had already committed to in prose ("a program that
/// presents that way gets no overlay") was worth exactly as much as any other
/// guess.
///
/// So the debug build watches the whole family and says which spellings a guest
/// really uses. Pointer comparisons against uniqued selectors, so it costs a
/// dozen integer compares per message and only when asked for.
const WATCHED: &[&str] = &[
    "presentDrawable:",
    "presentDrawable:atTime:",
    "presentDrawable:afterMinimumDuration:",
    "present",
    "presentAtTime:",
    "presentAfterMinimumDuration:",
    "commit",
    "commandQueue",
    "nextDrawable",
    "currentDrawable",
    "flushBuffer",
    "CGLContextObj",
];

fn watched() -> &'static [(u64, &'static str, std::sync::atomic::AtomicBool)] {
    static TABLE: std::sync::OnceLock<Vec<(u64, &'static str, std::sync::atomic::AtomicBool)>> =
        std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        WATCHED
            .iter()
            .map(|&name| {
                (
                    super::objc::sel(name) as u64,
                    name,
                    std::sync::atomic::AtomicBool::new(false),
                )
            })
            .collect()
    })
}

/// Report the first time the guest sends each watched selector.
fn observe(sel: u64) {
    for (candidate, name, said) in watched() {
        if *candidate == sel && !said.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!("rosetta3: gui  the guest sends -[... {name}]");
        }
    }
}

/// Called for every message the guest sends, before it is dispatched.
///
/// `receiver` is the receiver as the *host* runtime knows it, and `argument` is
/// the first argument — the drawable, for the Metal spellings, and unused for
/// `flushBuffer`. Both are already in the caller's hands at the point this is
/// reached, so the hook costs no work of its own to obtain them.
///
/// Before, rather than after, and for a different reason on each API. Metal: the
/// command buffer is still open, so an encoder appended here runs before the
/// present the guest just scheduled. OpenGL: the back buffer still holds the
/// frame, and after the flush it does not.
pub fn before_send(sel: u64, receiver: u64, argument: u64) {
    if !super::armed() {
        return;
    }
    let selectors = selectors();
    if super::debugging() {
        observe(sel);
    }
    if selectors.present_drawable.contains(&sel) {
        super::guarded(|| {
            // SAFETY: this is the `presentDrawable:` the guest is sending, on
            // the thread sending it, so the receiver is its command buffer and
            // the argument is the drawable — which is exactly what
            // [`super::metal::present`] requires.
            unsafe { super::metal::present(receiver as *mut _, argument as *mut _) };
        });
        return;
    }
    if selectors.present_self.contains(&sel) {
        super::guarded(|| {
            // SAFETY: a live receiver on the thread messaging it, which is all
            // `present_standalone` asks — it checks for itself whether the
            // receiver is really a drawable, because `present` is a common
            // enough name that some other class may have one.
            unsafe { super::metal::present_standalone(receiver as *mut _) };
        });
        return;
    }
    if sel == selectors.commit {
        super::guarded(|| {
            // SAFETY: as above; `note_queue` checks whether the receiver is
            // really a command buffer before messaging it further.
            unsafe { super::metal::note_queue(receiver as *mut _) };
        });
        return;
    }
    if sel == selectors.flush_buffer {
        // The context's identity is its `CGLContextObj`, which is also what the
        // `CGLFlushDrawable` path has — so a program that uses both spellings
        // for one context shares one set of overlay resources rather than
        // building two.
        //
        // SAFETY: `-CGLContextObj` is `NSOpenGLContext`'s, takes nothing and
        // returns an opaque pointer. The receiver responds to `flushBuffer`,
        // which is that class's method.
        let context = unsafe {
            super::objc::send0::<*mut std::ffi::c_void>(
                receiver as *mut _,
                super::objc::sel("CGLContextObj"),
            )
        };
        super::guarded(|| super::gl::present(context as usize));
    }
}

// ------------------------------------------------------------- the C path ---

/// `CGLError CGLFlushDrawable(CGLContextObj ctx)`
///
/// Shimmed rather than passed through, which has one consequence worth naming:
/// [`crate::shims::is_shimmed`] refuses to let the lifter promote a shimmed
/// import, so this call now takes a slow VM exit where it used to be a fast
/// one. Once per frame, against a call that is about to block on the display,
/// which is the cheapest place in a program to spend an exit.
fn flush_drawable(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    let context = ctx.rdi;
    if super::armed() {
        super::guarded(|| super::gl::present(context as usize));
    }
    let host = crate::shims::host::host_or_refuse(symbol)?;
    // SAFETY: the symbol [`shim_for`] matched, whose declaration in
    // `<OpenGL/CGLCurrent.h>` is `CGLError (*)(CGLContextObj)` — one pointer in,
    // an `int` back. Resolved out of an OpenGL framework this process has
    // loaded, which the guest's own context brought in.
    let f: extern "C" fn(u64) -> i32 =
        unsafe { core::mem::transmute::<u64, extern "C" fn(u64) -> i32>(host) };
    crate::shims::set_int_return(ctx, f(context));
    Ok(())
}

/// Whether `name` (underscore already stripped) is one of this module's.
///
/// Only when the overlay is switched on. A run with `ROSETTA3_GUI=0` leaves the
/// import exactly as it was — classified by the table, promotable by the
/// lifter — because an instrument that changes the program's translation when
/// it is turned off is an instrument nobody can trust.
pub fn shim_for(name: &str) -> Option<Shim> {
    if !super::enabled() {
        return None;
    }
    match name {
        "CGLFlushDrawable" => Some(flush_drawable),
        _ => None,
    }
}
