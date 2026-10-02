//! A private Wayland compositor that WebKitGTK's windows live in.
//!
//! GTK connects to this rather than to the host's display, so each web view is
//! a real toplevel with a frame clock, keyboard focus and popups, and nothing
//! about it depends on the host session. What the windows draw comes back as
//! pixels for `appkit::backing`'s overlays; input goes in as seat events.
//!
//! The compositor runs on its own thread: GTK makes blocking round trips to
//! its display, which would never return if the server were only serviced by
//! the thread GTK itself runs on.

// Only Linux has WebKitGTK, and smithay is a Linux dependency; elsewhere the
// compositor never starts and most of this is unused.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

mod composite;
#[cfg(target_os = "linux")]
mod gpu;
#[cfg(target_os = "linux")]
mod monitor;
#[cfg(target_os = "linux")]
mod state;
#[cfg(all(test, target_os = "linux"))]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
use smithay::reexports::calloop::channel::Sender;

/// A toplevel's latest picture, in pixels.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub picture: appkit::backing::Picture,
}

/// Where windows are composed.
pub enum Composition {
    /// On the CPU, from shared memory only.
    Cpu,
    /// On the GPU the presenter samples dma-bufs on, or else any.
    Gpu(Option<appkit::backing::Importer>),
}

/// A toplevel is named by its `wl_surface`'s protocol id, which GTK's side
/// reads with `wl_proxy_get_id`: there is only one client.
pub type SurfaceId = u32;

/// The host display a window is on, which its output stands for.
#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    /// SDL's name for it, which is usually the connector's, as `DP-1`.
    pub name: Option<String>,
    /// The mode, in pixels.
    pub width: i32,
    pub height: i32,
    /// In millihertz.
    pub refresh: u32,
}

pub enum Command {
    /// The size, in logical pixels, and scale the window is shown at, and
    /// the display it is on; a suspended window is not shown and gets no
    /// frame callbacks.
    Configure {
        surface: SurfaceId,
        width: i32,
        height: i32,
        scale: f64,
        suspended: bool,
        monitor: Option<Monitor>,
        /// Whether its pictures may be dma-bufs rather than pixels.
        dmabuf: bool,
    },
    /// The last frame of `surface` was taken, so it may draw the next.
    Taken { surface: SurfaceId },
    /// The view is gone; drop anything held for its surface.
    Forget { surface: SurfaceId },
    /// The pointer is at `x`, `y` in the window's logical pixels.
    Pointer { surface: SurfaceId, x: f64, y: f64 },
    /// A `BTN_*` code.
    Button { button: u32, pressed: bool },
    /// Pixel deltas, and whole-notch steps in 120ths when the device has them.
    Axis {
        dx: f64,
        dy: f64,
        steps: Option<(i32, i32)>,
    },
    /// An evdev key code.
    Key { key: u32, pressed: bool },
    /// Which window has the keyboard, if any.
    Focus { surface: Option<SurfaceId> },
    /// The host's clipboard, offered to GTK as the selection: MIME type and
    /// bytes.
    Offer { entries: Vec<(String, Vec<u8>)> },
}

pub enum Event {
    /// The pointer over `surface` wants this cursor, by its CSS name, or
    /// none at all.
    Cursor {
        surface: SurfaceId,
        name: Option<&'static str>,
    },
    /// GTK set the selection; these are its contents in the types asked for.
    Selection { entries: Vec<(String, Vec<u8>)> },
}

/// What the compositor thread hands back.
struct Shared {
    frames: Mutex<HashMap<SurfaceId, Frame>>,
    events: Mutex<Vec<Event>>,
    /// Called from the compositor thread when there is something to take.
    wake: fn(),
}

pub struct Compositor {
    #[cfg(target_os = "linux")]
    commands: Sender<Command>,
    shared: Arc<Shared>,
    /// The socket GTK connects to, which is removed once it has.
    pub socket: PathBuf,
    /// Whether windows are composed on a GPU, and GTK may render on it.
    pub gpu: bool,
}

impl Compositor {
    #[cfg(not(target_os = "linux"))]
    pub fn start(_wake: fn(), _composition: Composition) -> Result<Compositor, String> {
        Err("web views need WebKitGTK, which only a Linux host has".to_string())
    }

    /// Start the compositor, listening at a fresh socket. `wake` is called
    /// from its thread whenever a frame or event is ready.
    #[cfg(target_os = "linux")]
    pub fn start(wake: fn(), composition: Composition) -> Result<Compositor, String> {
        let shared = Arc::new(Shared {
            frames: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            wake,
        });
        let socket = socket_path();
        let _ = std::fs::remove_file(&socket);
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread_shared = shared.clone();
        let thread_socket = socket.clone();
        std::thread::Builder::new()
            .name("rosemary-webkit-compositor".to_string())
            .spawn(move || state::run(thread_socket, thread_shared, composition, sender))
            .map_err(|e| format!("could not start the compositor thread: {e}"))?;
        let (commands, gpu) = receiver
            .recv()
            .map_err(|_| "the compositor thread exited while starting".to_string())??;
        Ok(Compositor {
            commands,
            shared,
            socket,
            gpu,
        })
    }

    pub fn send(&self, command: Command) {
        // A send fails only once the compositor thread is gone, and then there
        // is nobody left to tell.
        #[cfg(target_os = "linux")]
        let _ = self.commands.send(command);
        #[cfg(not(target_os = "linux"))]
        drop(command);
    }

    /// The newest picture of `surface` not yet taken.
    pub fn take_frame(&self, surface: SurfaceId) -> Option<Frame> {
        let frame = lock(&self.shared.frames).remove(&surface);
        if frame.is_some() {
            self.send(Command::Taken { surface });
        }
        frame
    }

    pub fn take_events(&self) -> Vec<Event> {
        std::mem::take(&mut *lock(&self.shared.events))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Somewhere only this user can reach, named so two compositors cannot
/// collide, whether in two processes or in one.
fn socket_path() -> PathBuf {
    static STARTED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let serial = STARTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = match serial {
        0 => format!("rosemary-webkit-{}", std::process::id()),
        _ => format!("rosemary-webkit-{}-{serial}", std::process::id()),
    };
    directory.join(name)
}
