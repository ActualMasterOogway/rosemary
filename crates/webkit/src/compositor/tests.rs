//! The compositor driven by a real Wayland client, as GTK drives it.

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use super::{Command, Compositor};

#[derive(Default)]
struct Client {
    configured: Option<u32>,
    size: (i32, i32),
    frame_done: bool,
    entered: bool,
    buttons: Vec<(u32, bool)>,
    keys: Vec<(u32, bool)>,
    keyboard_entered: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Client {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Client {
    fn event(
        client: &mut Self,
        _: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            client.configured = Some(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Client {
    fn event(
        client: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Configure { width, height, .. } = event {
            client.size = (width, height);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Client {
    fn event(
        client: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        client.frame_done = true;
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Client {
    fn event(
        client: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { .. } => client.entered = true,
            wl_pointer::Event::Button { button, state, .. } => client.buttons.push((
                button,
                state == WEnum::Value(wl_pointer::ButtonState::Pressed),
            )),
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Client {
    fn event(
        client: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { .. } => client.keyboard_entered = true,
            wl_keyboard::Event::Key { key, state, .. } => client
                .keys
                .push((key, state == WEnum::Value(wl_keyboard::KeyState::Pressed))),
            _ => {}
        }
    }
}

delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_surface::WlSurface);
delegate_noop!(Client: ignore wl_shm::WlShm);
delegate_noop!(Client: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Client: ignore wl_buffer::WlBuffer);
delegate_noop!(Client: ignore wl_seat::WlSeat);

/// Dispatch until `done` holds, or fail after a while.
fn until(queue: &mut EventQueue<Client>, client: &mut Client, done: impl Fn(&Client) -> bool) {
    let started = Instant::now();
    while !done(client) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the compositor did not answer"
        );
        queue.roundtrip(client).expect("the connection stays up");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn shm_file(bytes: &[u8]) -> OwnedFd {
    // SAFETY: a NUL-terminated name and no flags.
    let fd = unsafe { libc::memfd_create(c"rosetta3-test".as_ptr(), 0) };
    assert!(fd >= 0, "memfd_create");
    // SAFETY: a new descriptor this owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut file = std::fs::File::from(fd.try_clone().expect("dup"));
    std::io::Write::write_all(&mut file, bytes).expect("write the pixels");
    fd
}

#[test]
fn a_toplevel_is_drawn_throttled_and_given_input() {
    let compositor = Compositor::start(|| {}).expect("the compositor starts");
    let stream = UnixStream::connect(&compositor.socket).expect("connect");
    let connection = Connection::from_socket(stream).expect("a Wayland connection");
    let (globals, mut queue) = registry_queue_init::<Client>(&connection).expect("globals");
    let handle = queue.handle();
    let mut client = Client::default();
    let wl_compositor: wl_compositor::WlCompositor =
        globals.bind(&handle, 4..=6, ()).expect("wl_compositor");
    let shm: wl_shm::WlShm = globals.bind(&handle, 1..=1, ()).expect("wl_shm");
    let base: xdg_wm_base::XdgWmBase = globals.bind(&handle, 1..=6, ()).expect("xdg_wm_base");
    let seat: wl_seat::WlSeat = globals.bind(&handle, 5..=7, ()).expect("wl_seat");
    let _pointer = seat.get_pointer(&handle, ());
    let _keyboard = seat.get_keyboard(&handle, ());

    let surface = wl_compositor.create_surface(&handle, ());
    let xdg = base.get_xdg_surface(&surface, &handle, ());
    let _toplevel = xdg.get_toplevel(&handle, ());
    surface.commit();
    let id = surface.id().protocol_id();
    compositor.send(Command::Configure {
        surface: id,
        width: 2,
        height: 2,
        scale: 1.0,
        suspended: false,
    });
    until(&mut queue, &mut client, |c| {
        c.size == (2, 2) && c.configured.is_some()
    });
    xdg.ack_configure(client.configured.expect("configured"));

    // Two pixels of opaque blue over two of half-transparent red, B, G, R, A.
    let pixels: Vec<u8> = [
        [255, 0, 0, 255],
        [255, 0, 0, 255],
        [0, 0, 128, 128],
        [0, 0, 128, 128],
    ]
    .concat();
    let fd = shm_file(&pixels);
    let pool = shm.create_pool(fd.as_fd(), pixels.len() as i32, &handle, ());
    let buffer = pool.create_buffer(0, 2, 2, 8, wl_shm::Format::Argb8888, &handle, ());
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, 2, 2);
    surface.frame(&handle, ());
    surface.commit();
    connection.flush().expect("flush");

    let started = Instant::now();
    let frame = loop {
        queue.roundtrip(&mut client).expect("roundtrip");
        if let Some(frame) = compositor.take_frame(id) {
            break frame;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no frame arrived"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!((frame.width, frame.height), (2, 2));
    assert_eq!(frame.pixels, pixels);
    // Taken, so the next frame may be drawn.
    until(&mut queue, &mut client, |c| c.frame_done);

    // A second frame is held back until the first is taken.
    client.frame_done = false;
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, 2, 2);
    surface.frame(&handle, ());
    surface.commit();
    for _ in 0..10 {
        queue.roundtrip(&mut client).expect("roundtrip");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !client.frame_done,
        "a frame callback came before the frame was taken"
    );
    let started = Instant::now();
    while compositor.take_frame(id).is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no second frame"
        );
        queue.roundtrip(&mut client).expect("roundtrip");
    }
    until(&mut queue, &mut client, |c| c.frame_done);

    compositor.send(Command::Pointer {
        surface: id,
        x: 1.0,
        y: 1.0,
    });
    compositor.send(Command::Button {
        button: 0x110,
        pressed: true,
    });
    compositor.send(Command::Button {
        button: 0x110,
        pressed: false,
    });
    until(&mut queue, &mut client, |c| c.buttons.len() == 2);
    assert!(client.entered);
    assert_eq!(client.buttons, [(0x110, true), (0x110, false)]);

    compositor.send(Command::Focus { surface: Some(id) });
    compositor.send(Command::Key {
        key: 30,
        pressed: true,
    });
    compositor.send(Command::Key {
        key: 30,
        pressed: false,
    });
    until(&mut queue, &mut client, |c| c.keys.len() == 2);
    assert!(client.keyboard_entered);
    assert_eq!(client.keys, [(30, true), (30, false)]);
}
