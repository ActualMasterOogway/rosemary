//! Modal alerts drawn by [`chrome::alert`] in a window of their own, in place
//! of the host toolkit's message box: they look like a Mac's and take a Mac's
//! buttons, which a native box can only approximate.
//!
//! Usable before the rest of AppKit is: nothing here needs a guest.

use std::sync::Mutex;

use chrome::alert::{Alert, AlertLayout, Part};
use chrome::{Canvas, Image, Point, Rect, Theme, View};

/// What to show. The first button is the default.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// The window's title, for a taskbar; the alert itself shows none.
    pub title: String,
    pub message: String,
    pub informative: String,
    pub buttons: Vec<String>,
    /// The button Escape and closing the window answer. The one titled Cancel
    /// when not given.
    pub cancel: Option<usize>,
    /// Show the application's icon, as `NSAlert` does.
    pub application_icon: bool,
}

/// The button areas of each open dialog's window, for [`hit_test`], which
/// SDL calls with nothing else to go on.
static BUTTONS: Mutex<Vec<(usize, Vec<Rect>)>> = Mutex::new(Vec::new());

unsafe extern "C" fn hit_test(
    window: sdl3::Window,
    point: *const sdl3::video::Point,
    _data: *mut std::ffi::c_void,
) -> sdl3::video::HitTestResult {
    use sdl3::video::HitTestResult as R;
    // SAFETY: SDL passes the point being tested.
    let point = unsafe { *point };
    let point = Point::new(point.x as f32, point.y as f32);
    let dialogs = BUTTONS.lock().unwrap_or_else(|e| e.into_inner());
    let on_button = dialogs
        .iter()
        .find(|(sdl, _)| *sdl == window as usize)
        .is_some_and(|(_, rects)| rects.iter().any(|rect| rect.contains(point)));
    // Everything that is not a button moves the window, as a borderless
    // alert has no titlebar to hold.
    match on_button {
        true => R::Normal,
        false => R::Draggable,
    }
}

fn icon() -> Option<Image> {
    let bitmap = crate::icon::current()?;
    Some(Image {
        width: bitmap.width as usize,
        height: bitmap.height as usize,
        pixels: bitmap.pixels,
    })
}

fn theme() -> Theme {
    Theme::for_appearance(sdl3::video::system_theme() == sdl3::video::SystemTheme::Dark)
}

struct Dialog {
    window: sdl3::Window,
    id: u32,
    alert: Alert,
    layout: AlertLayout,
    theme: Theme,
}

impl Dialog {
    fn paint(&self) {
        let Some((width, _)) = sdl3::video::window_size(self.window) else {
            return;
        };
        sdl3::video::present_window(self.window, |surface| {
            let scale = match width > 0 {
                true => surface.width as f32 / width as f32,
                false => 1.0,
            };
            let length = surface.pitch as usize * surface.height as usize;
            // SAFETY: SDL reported `pitch` bytes per row for `height` rows,
            // and the surface is locked for the duration of this call.
            let pixels = unsafe { std::slice::from_raw_parts_mut(surface.pixels, length) };
            let mut canvas = Canvas::new(
                pixels,
                surface.width as usize,
                surface.height as usize,
                surface.pitch as usize,
                scale,
            );
            canvas.clear(chrome::Color::rgba(0, 0, 0, 0.0));
            self.alert.paint(&mut canvas, &self.layout, &self.theme);
            dump(self.id, surface, pixels);
            true
        });
    }

    fn button_at(&self, x: f32, y: f32) -> Option<usize> {
        self.alert
            .hit(&self.layout, Point::new(x, y))
            .map(|Part::Button(at)| at)
    }

    fn set_pressed(&mut self, pressed: Option<usize>) {
        if self.alert.pressed != pressed {
            self.alert.pressed = pressed;
            self.paint();
        }
    }
}

/// `ROSETTA3_STORE_OUT=DIR` writes each dialog as it is painted, as the menu
/// panels are, for a session with no screen to look at.
fn dump(id: u32, surface: &sdl3::video::Presentation, pixels: &[u8]) {
    let Some(directory) = std::env::var_os("ROSETTA3_STORE_OUT") else {
        return;
    };
    let (width, height) = (surface.width as usize, surface.height as usize);
    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
    for row in 0..height {
        let start = row * surface.pitch as usize;
        for texel in pixels[start..start + width * 4].as_chunks::<4>().0 {
            out.extend_from_slice(&[texel[2], texel[1], texel[0]]);
        }
    }
    let _ = std::fs::write(
        std::path::Path::new(&directory).join(format!("dialog-{id}.ppm")),
        out,
    );
}

/// The window a dialog is modal for: the key window, or any of the program's.
fn parent() -> sdl3::Window {
    let key = crate::window::key_window();
    // SAFETY: nil or a window this crate made.
    let sdl = unsafe { crate::window::sdl_window(key) };
    match sdl.is_null() {
        true => crate::window::sdl_windows()
            .into_iter()
            .next()
            .unwrap_or(std::ptr::null_mut()),
        false => sdl,
    }
}

/// Show `request` and wait for a button: its index, or `None` when no dialog
/// could be shown at all, which is the caller's cue to ask some other way.
///
/// Blocks the calling thread, which must be the main one, as `-runModal`
/// does. Input meant for the program's other windows is dropped meanwhile;
/// the rest of what happens to them is still delivered.
pub fn run(request: &Request) -> Option<usize> {
    if request.buttons.is_empty() || chrome::Font::system(400).is_none() {
        return None;
    }
    crate::decor::close_menus();
    // Hints SDL reads as video starts, which this may be the first to do.
    crate::decor::install();
    let started_video = !sdl3::video::video_up();

    let transparent = sdl3::video::is_wayland();
    let alert = Alert {
        message: request.message.clone(),
        informative: request.informative.clone(),
        buttons: request.buttons.clone(),
        icon: match request.application_icon {
            true => icon(),
            false => None,
        },
        pressed: None,
        rounded: transparent,
    };
    let theme = theme();
    let layout = alert.layout(Rect::default(), &theme);
    let cancel = request.cancel.or_else(|| alert.cancel_button());

    let title = std::ffi::CString::new(request.title.replace('\0', " ")).unwrap_or_default();
    let mut flags = sdl3::video::flags::HIDDEN
        | sdl3::video::flags::BORDERLESS
        | sdl3::video::flags::HIGH_PIXEL_DENSITY;
    if transparent {
        flags |= sdl3::video::flags::TRANSPARENT;
    }
    let size = layout.bounds.size();
    let Some(window) = sdl3::video::create_window(
        &title,
        size.width.ceil() as i32,
        size.height.ceil() as i32,
        flags,
    ) else {
        eprintln!(
            "rosetta3: appkit: could not open a dialog: {}",
            sdl3::error()
        );
        if started_video {
            sdl3::video::quit_video();
        }
        return None;
    };
    let parent = parent();
    if !parent.is_null() {
        sdl3::video::set_window_modal_for(window, parent);
    }
    crate::icon::apply(window);
    center(window, parent, size.width, size.height);
    BUTTONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((window as usize, layout.buttons.clone()));
    sdl3::video::set_window_hit_test(window, Some(hit_test));
    sdl3::video::show_window(window);
    sdl3::video::raise_window(window);

    let mut dialog = Dialog {
        window,
        id: sdl3::video::window_id(window),
        alert,
        layout,
        theme,
    };
    dialog.paint();
    // A program that hid the pointer, as a game does, would leave nothing to
    // press the buttons with.
    // SAFETY: reads a counter.
    let hidden = !unsafe { crate::cursor::CGCursorIsVisible() };
    if hidden {
        sdl3::video::set_cursor_visible(true);
    }
    let (answer, quit) = modal_loop(&mut dialog, cancel);
    if hidden {
        sdl3::video::set_cursor_visible(false);
    }

    BUTTONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(sdl, _)| *sdl != window as usize);
    sdl3::video::set_window_hit_test(window, None);
    sdl3::video::destroy_window(window);
    if started_video {
        sdl3::video::quit_video();
    } else if quit {
        // The quit was the program's, and only postponed.
        crate::event::deliver(sdl3::video::Event::Quit);
    }
    Some(answer)
}

/// Over the parent's middle, or the primary display's. Wayland places a
/// window itself and ignores this.
fn center(window: sdl3::Window, parent: sdl3::Window, width: f32, height: f32) {
    let area = match parent.is_null() {
        false => sdl3::video::window_position(parent).zip(sdl3::video::window_size(parent)),
        true => None,
    }
    .map(|((x, y), (w, h))| (x, y, w, h))
    .or_else(|| {
        let bounds = sdl3::video::display_usable_bounds(sdl3::video::primary_display())?;
        Some((bounds.x, bounds.y, bounds.w, bounds.h))
    });
    if let Some((x, y, w, h)) = area {
        let left = x + (w - width as i32) / 2;
        // A little above the middle, where a Mac puts an alert.
        let top = y + ((h - height as i32) / 3).max(0);
        sdl3::video::set_window_position(window, left, top);
    }
}

/// Turn the event loop until a button is chosen; the answer, and whether a
/// quit arrived meanwhile.
fn modal_loop(dialog: &mut Dialog, cancel: Option<usize>) -> (usize, bool) {
    use sdl3::keycodes::scancode;
    use sdl3::video::Event;

    // What closing the window answers: Cancel, or the only button there is.
    let dismissal = cancel.or((dialog.alert.buttons.len() == 1).then_some(0));
    // The button a press began on, which a release has to end on too.
    let mut armed: Option<usize> = None;
    loop {
        // A thread waiting on a GL context the main thread holds would wait
        // for as long as the dialog is up.
        crate::opengl::release_for_other_threads();
        let Some(event) = sdl3::video::wait(50) else {
            continue;
        };
        let ours = |window: u32| window == dialog.id;
        match event {
            Event::MouseButton {
                window,
                button: sdl3::mouse::LEFT,
                down,
                x,
                y,
                ..
            } if ours(window) => {
                let at = dialog.button_at(x, y);
                if down {
                    armed = at;
                    dialog.set_pressed(at);
                } else {
                    dialog.set_pressed(None);
                    if let Some(chosen) = armed.take().filter(|&armed| at == Some(armed)) {
                        return (chosen, false);
                    }
                }
            }
            Event::MouseMotion { window, x, y, .. } if ours(window) => {
                if let Some(armed) = armed {
                    let over = dialog.button_at(x, y) == Some(armed);
                    dialog.set_pressed(over.then_some(armed));
                }
            }
            Event::Key {
                window,
                scancode,
                down: true,
                ..
            } if ours(window) => match scancode {
                scancode::RETURN | scancode::KP_ENTER => return (0, false),
                scancode::ESCAPE => {
                    if let Some(cancel) = cancel {
                        return (cancel, false);
                    }
                }
                _ => {}
            },
            Event::WindowClosed { window } if ours(window) => {
                if let Some(answer) = dismissal {
                    return (answer, false);
                }
            }
            Event::WindowResized { window, .. } | Event::WindowBackingChanged { window }
                if ours(window) =>
            {
                dialog.paint();
            }
            Event::Other(sdl3::video::event_type::WINDOW_EXPOSED) => dialog.paint(),
            Event::Other(sdl3::video::event_type::SYSTEM_THEME_CHANGED) => {
                dialog.theme = theme();
                dialog.paint();
            }
            Event::Quit => {
                return (dismissal.unwrap_or(dialog.alert.buttons.len() - 1), true);
            }
            // The program's windows take no input while the dialog is up,
            // as under `-runModal`.
            Event::Key { .. }
            | Event::Text { .. }
            | Event::TextEditing { .. }
            | Event::MouseMotion { .. }
            | Event::MouseButton { .. }
            | Event::MouseWheel { .. } => {}
            Event::WindowClosed { .. } | Event::WindowHitTest { .. } => {}
            Event::WindowFocus { window, .. }
            | Event::WindowMouse { window, .. }
            | Event::WindowState { window }
                if ours(window) => {}
            other => crate::event::deliver(other),
        }
    }
}
