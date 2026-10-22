use super::*;

thread_local! {
    static RESIZES: std::cell::RefCell<Vec<(NSRect, NSSize)>> = const { std::cell::RefCell::new(Vec::new()) };
    static TRANSITION_REQUEST: std::cell::Cell<Option<(bool, NSRect)>> = const { std::cell::Cell::new(None) };
    static TRANSITION_ORIGIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TRANSITION_SEEN: std::cell::Cell<Option<(bool, NSRect, NSRect)>> = const { std::cell::Cell::new(None) };
}

unsafe extern "C" fn resized(receiver: Id, _sel: Sel, _notification: Id) {
    let actual = unsafe { frame(receiver, objc::sel::get("frame")) };
    let content = crate::view::frame_of(window_of(receiver).content_view).size;
    RESIZES.with_borrow_mut(|seen| seen.push((actual, content)));
}

unsafe extern "C" fn transition(receiver: Id, sel: Sel, _notification: Id) {
    let entering = sel == objc::sel::get("windowWillEnterFullScreen:");
    if let Some((wanted, requested)) = TRANSITION_REQUEST.get()
        && wanted == entering
    {
        TRANSITION_REQUEST.set(None);
        let was_fullscreen =
            unsafe { style_mask(receiver, objc::sel::get("styleMask")) } & style::FULL_SCREEN != 0;
        let before = unsafe { frame(receiver, objc::sel::get("frame")) };
        if TRANSITION_ORIGIN.get() {
            unsafe {
                set_frame_origin(
                    receiver,
                    objc::sel::get("setFrameOrigin:"),
                    requested.origin,
                )
            };
        } else {
            unsafe { set_frame(receiver, objc::sel::get("setFrame:"), requested) };
        }
        let after = unsafe { frame(receiver, objc::sel::get("frame")) };
        TRANSITION_SEEN.set(Some((was_fullscreen, before, after)));
    }
}

fn oracle_rows() -> impl Iterator<Item = Vec<&'static str>> {
    include_str!("../tests/fixtures/fullscreen-macos-26.5.tsv")
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| line.split('\t').collect())
}

#[test]
fn native_fullscreen_frame_constraints_match_the_mac_capture() {
    let host = NSRect {
        origin: NSPoint::default(),
        size: NSSize {
            width: 1440.0,
            height: 900.0,
        },
    };
    let mut count = 0;
    for row in oracle_rows().filter(|row| !matches!(row[2], "origin" | "content-size")) {
        let value = |index: usize| row[index].parse::<f64>().unwrap();
        let requested = NSRect {
            origin: NSPoint {
                x: value(4),
                y: value(5),
            },
            size: NSSize {
                width: value(6),
                height: value(7),
            },
        };
        let expected = NSRect {
            origin: NSPoint {
                x: value(8),
                y: value(9),
            },
            size: NSSize {
                width: value(10),
                height: value(11),
            },
        };
        assert_eq!(
            constrain_fullscreen_frame(requested, host),
            expected,
            "native case {} step {}",
            row[0],
            row[3]
        );
        count += 1;
    }
    assert_eq!(count, 18);
}

/// Run explicitly under a window manager, in its own process so SDL's
/// video thread and event queue are owned by this test throughout.
#[test]
#[ignore = "requires an X11/Wayland window manager; see tests/fixtures/fullscreen-native.md"]
fn native_fullscreen_oracle() {
    crate::bootstrap();
    let pool = objc::arc::pool_push();
    let class = objc::class::allocate_pair(
        objc::class::lookup(c"NSWindow"),
        c"NativeFullscreenOracleWindow",
        0,
    )
    .unwrap();
    class.add_method(
        objc::sel::get("windowDidResize:"),
        resized as objc::class::Imp,
        c"v24@0:8@16",
    );
    class.add_method(
        objc::sel::get("windowWillEnterFullScreen:"),
        transition as objc::class::Imp,
        c"v24@0:8@16",
    );
    class.add_method(
        objc::sel::get("windowWillExitFullScreen:"),
        transition as objc::class::Imp,
        c"v24@0:8@16",
    );
    objc::class::register_pair(class);
    let pump = || {
        while let Some(event) = sdl3::video::poll() {
            crate::event::deliver(event);
        }
    };
    let settle = |predicate: &dyn Fn() -> bool| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            pump();
            if predicate() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "window manager did not settle"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    let mut previous = "";
    let mut object: Id = std::ptr::null_mut();
    let mut restore = NSRect::default();
    let mut host = NSRect::default();
    let mut tested = 0;
    for row in oracle_rows() {
        if row[0] != previous {
            if !object.is_null() {
                unsafe {
                    toggle_full_screen(
                        object,
                        objc::sel::get("toggleFullScreen:"),
                        std::ptr::null_mut(),
                    )
                };
                settle(&|| {
                    !is_fullscreen(window_of(object).sdl) && host_frame(object) == Some(restore)
                });
                assert_eq!(unsafe { frame(object, objc::sel::get("frame")) }, restore);
                unsafe {
                    set_content_size(
                        object,
                        objc::sel::get("setContentSize:"),
                        NSSize {
                            width: 640.0,
                            height: 420.0,
                        },
                    )
                };
                settle(&|| {
                    sdl3::video::window_size(window_of(object).sdl)
                        == Some((640, 420 + content_inset(object) as i32))
                });
                assert_eq!(
                    crate::view::frame_of(window_of(object).content_view).size,
                    NSSize {
                        width: 640.0,
                        height: 420.0
                    }
                );
                objc::arc::release(object);
                pump();
            }
            object = objc::object::create_instance(class, 0);
            window_of(object).delegate = object;
            let content = NSRect {
                origin: NSPoint { x: 80.0, y: 100.0 },
                size: NSSize {
                    width: 720.0,
                    height: 460.0,
                },
            };
            assert!(unsafe { init_with_content_rect_for_tests(object, content) });
            let mask = style::TITLED
                | style::CLOSABLE
                | style::MINIATURIZABLE
                | style::RESIZABLE
                | if row[1] == "full-size-content" {
                    1 << 15
                } else {
                    0
                };
            unsafe { set_style_mask(object, objc::sel::get("setStyleMask:"), mask) };
            let sdl = window_of(object).sdl;
            sdl3::video::show_window(sdl);
            window_of(object).shown = 1;
            settle(&|| host_frame(object).is_some());
            restore = unsafe { frame(object, objc::sel::get("frame")) };
            unsafe {
                toggle_full_screen(
                    object,
                    objc::sel::get("toggleFullScreen:"),
                    std::ptr::null_mut(),
                )
            };
            let bounds = sdl3::video::display_bounds(sdl3::video::primary_display()).unwrap();
            settle(&|| {
                is_fullscreen(sdl)
                    && window_of(object).reported_fullscreen != 0
                    && sdl3::video::window_size(sdl) == Some((bounds.w, bounds.h))
            });
            host = host_frame(object).unwrap();
            previous = row[0];
        }
        let value = |index: usize| row[index].parse::<f64>().unwrap();
        // The capture's display was 1440x900. Keep the measured point
        // deltas when exercising a different host display size.
        let width = |v: f64| if v == 1440.0 { host.size.width } else { v };
        let height = |v: f64| {
            if v == 450.0 {
                host.size.height / 2.0
            } else {
                v + host.size.height - 900.0
            }
        };
        let args = NSRect {
            origin: NSPoint {
                x: value(4) + host.origin.x,
                y: value(5) + host.origin.y,
            },
            size: NSSize {
                width: width(value(6)),
                height: height(value(7)),
            },
        };
        let before = unsafe { frame(object, objc::sel::get("frame")) };
        RESIZES.with_borrow_mut(Vec::clear);
        unsafe {
            match row[2] {
                "origin" => {
                    set_frame_origin(object, objc::sel::get("setFrameOrigin:"), args.origin)
                }
                "content-size" => {
                    set_content_size(object, objc::sel::get("setContentSize:"), args.size)
                }
                "frame-display" => {
                    set_frame_display(object, objc::sel::get("setFrame:display:"), args, 1)
                }
                "frame-animate" => set_frame_display_animate(
                    object,
                    objc::sel::get("setFrame:display:animate:"),
                    args,
                    1,
                    1,
                ),
                "roblox-transition" => {
                    set_style_mask(
                        object,
                        objc::sel::get("setStyleMask:"),
                        window_of(object).style_mask & !(1 << 15),
                    );
                    set_frame(object, objc::sel::get("setFrame:"), args);
                }
                _ => set_frame(object, objc::sel::get("setFrame:"), args),
            }
        }
        let expected = NSRect {
            origin: NSPoint {
                x: value(8) + host.origin.x,
                y: value(9) + host.origin.y,
            },
            size: NSSize {
                width: width(value(10)),
                height: height(value(11)),
            },
        };
        assert_eq!(
            unsafe { frame(object, objc::sel::get("frame")) },
            expected,
            "native case {} step {}",
            row[0],
            row[3]
        );
        let view = window_of(object).content_view;
        assert_eq!(crate::view::frame_of(view).size, expected.size);
        let observations = RESIZES.with_borrow(Clone::clone);
        assert_eq!(
            observations.len(),
            usize::from(before.size != expected.size),
            "synchronous resize in native case {}",
            row[0]
        );
        if let Some((notified_frame, notified_size)) = observations.first() {
            assert_eq!(*notified_frame, expected);
            assert_eq!(*notified_size, expected.size);
        }
        let mapped = rect_to_surface(
            object,
            NSRect {
                origin: NSPoint::default(),
                size: expected.size,
            },
        )
        .unwrap();
        let density = sdl3::video::window_pixel_density(window_of(object).sdl) as f64;
        let presented = unsafe { crate::view::presentation_frame(view) }.unwrap();
        // Expected physical position comes from the native frame, not the
        // mapping helper under test. In the 31-point case the top is zero.
        let expected_x = expected.origin.x - host.origin.x;
        let expected_y =
            host.origin.y + host.size.height - expected.origin.y - expected.size.height;
        assert_eq!(
            (presented.x, presented.y),
            (
                (expected_x * density).round() as i32,
                (expected_y * density).round() as i32
            ),
            "native placement case {} step {}",
            row[0],
            row[3]
        );
        assert_eq!(
            (presented.x, presented.y),
            (
                (mapped.origin.x * density).round() as i32,
                (mapped.origin.y * density).round() as i32
            )
        );
        let backing = crate::backing::placed_rect_for_tests(object, view).unwrap();
        assert_eq!(
            (backing.x, backing.y, backing.width, backing.height),
            (
                i64::from(presented.x),
                i64::from(presented.y),
                i64::from(presented.width),
                i64::from(presented.height)
            )
        );
        let (_, ime) = unsafe { crate::view::input_frame(view) }.unwrap();
        assert_eq!(
            (ime.x, ime.y),
            (
                mapped.origin.x.round() as i32,
                mapped.origin.y.round() as i32
            )
        );
        let point =
            point_from_surface(object, mapped.origin.x + 17.0, mapped.origin.y + 23.0).unwrap();
        assert_eq!(
            point,
            NSPoint {
                x: 17.0,
                y: expected.size.height - 23.0
            }
        );
        assert_eq!(window_of(object).size_pending, 0);
        tested += 1;
    }
    assert_eq!(tested, 22);
    unsafe {
        toggle_full_screen(
            object,
            objc::sel::get("toggleFullScreen:"),
            std::ptr::null_mut(),
        )
    };
    settle(&|| !is_fullscreen(window_of(object).sdl) && host_frame(object) == Some(restore));
    unsafe {
        set_content_size(
            object,
            objc::sel::get("setContentSize:"),
            NSSize {
                width: 640.0,
                height: 420.0,
            },
        )
    };
    settle(&|| {
        sdl3::video::window_size(window_of(object).sdl)
            == Some((640, 420 + content_inset(object) as i32))
    });
    assert_eq!(
        crate::view::frame_of(window_of(object).content_view).size,
        NSSize {
            width: 640.0,
            height: 420.0
        }
    );
    objc::arc::release(object);

    // The native capture restores the pre-transition frame even when a
    // will-enter callback resizes the window, and will-exit still sees the
    // current fullscreen frame. Exercise those callback boundaries directly.
    for (entering, origin_only, oversized) in [
        (true, false, false),
        (false, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        TRANSITION_ORIGIN.set(origin_only);
        let object = objc::object::create_instance(class, 0);
        window_of(object).delegate = object;
        let content = NSRect {
            origin: NSPoint { x: 80.0, y: 100.0 },
            size: NSSize {
                width: 720.0,
                height: 460.0,
            },
        };
        assert!(unsafe { init_with_content_rect_for_tests(object, content) });
        let sdl = window_of(object).sdl;
        sdl3::video::show_window(sdl);
        settle(&|| host_frame(object).is_some());
        let restore = unsafe { frame(object, objc::sel::get("frame")) };
        let requested = NSRect {
            origin: NSPoint {
                x: restore.origin.x,
                y: restore.origin.y + 31.0,
            },
            size: NSSize {
                width: restore.size.width,
                height: restore.size.height - 31.0,
            },
        };
        if entering {
            TRANSITION_REQUEST.set(Some((true, requested)));
        }
        unsafe {
            toggle_full_screen(
                object,
                objc::sel::get("toggleFullScreen:"),
                std::ptr::null_mut(),
            )
        };
        settle(&|| is_fullscreen(sdl) && window_of(object).reported_fullscreen != 0);
        if entering {
            let (was_fullscreen, before, after) = TRANSITION_SEEN.take().unwrap();
            assert!(!was_fullscreen);
            assert_eq!(before, restore);
            assert_eq!(after, requested);
        } else {
            let full = host_frame(object).unwrap();
            TRANSITION_REQUEST.set(Some((
                false,
                NSRect {
                    origin: NSPoint {
                        x: full.origin.x + if origin_only { 23.0 } else { 0.0 },
                        y: full.origin.y + 31.0,
                    },
                    size: NSSize {
                        width: full.size.width,
                        height: full.size.height + if oversized { 71.0 } else { -31.0 },
                    },
                },
            )));
        }
        unsafe {
            toggle_full_screen(
                object,
                objc::sel::get("toggleFullScreen:"),
                std::ptr::null_mut(),
            )
        };
        settle(&|| {
            !is_fullscreen(sdl)
                && window_of(object).reported_fullscreen == 0
                && host_frame(object) == Some(restore)
        });
        if !entering {
            let (was_fullscreen, before, after) = TRANSITION_SEEN.take().unwrap();
            assert!(was_fullscreen);
            if origin_only {
                assert_eq!(after.size, before.size);
                assert_eq!(after.origin.x - before.origin.x, 23.0);
                assert_eq!(after.origin.y - before.origin.y, 31.0);
            } else {
                assert_eq!(
                    after.size.height - before.size.height,
                    if oversized { 71.0 } else { -31.0 }
                );
                assert_eq!(after.origin.y - before.origin.y, 31.0);
            }
        }
        assert_eq!(unsafe { frame(object, objc::sel::get("frame")) }, restore);
        objc::arc::release(object);
        pump();
    }
    eprintln!(
        "native comparison: 22 observations, 14 settled cases and post-exit resizes, 4 transition callback cases"
    );
    objc::arc::pool_pop(pool);
}

thread_local! {
    static FAILED_TRANSITIONS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
    static RECREATE_ON_NEXT_ENTER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static RELEASE_CALLBACK_SEEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

unsafe extern "C" fn recreate_on_will_enter(receiver: Id, _sel: Sel, _notification: Id) {
    if RECREATE_ON_NEXT_ENTER.with(|recreate| recreate.replace(false)) {
        assert!(rebuild_for_opengl(receiver));
    }
}

unsafe extern "C" fn release_on_will_enter(receiver: Id, _sel: Sel, _notification: Id) {
    // The callback consumes the guest-owned reference while the toggle runs.
    RELEASE_CALLBACK_SEEN.set(true);
    objc::arc::release(receiver);
}

unsafe extern "C" fn did_fail_enter(receiver: Id, _sel: Sel, window: Id) {
    assert_eq!(receiver, window);
    FAILED_TRANSITIONS.with(|count| count.update(|(enter, exit)| (enter + 1, exit)));
}

unsafe extern "C" fn did_fail_exit(receiver: Id, _sel: Sel, window: Id) {
    assert_eq!(receiver, window);
    FAILED_TRANSITIONS.with(|count| count.update(|(enter, exit)| (enter, exit + 1)));
}

#[test]
#[ignore = "requires an X11/Wayland window manager; see tests/fixtures/fullscreen-native.md"]
fn fullscreen_window_replacement_confinement_and_failures() {
    crate::bootstrap();
    let pool = objc::arc::pool_push();
    let class = objc::class::allocate_pair(
        objc::class::lookup(c"NSWindow"),
        c"NativeFullscreenLifecycleRegressionWindow",
        0,
    )
    .unwrap();
    class.add_method(
        objc::sel::get("windowWillEnterFullScreen:"),
        recreate_on_will_enter as objc::class::Imp,
        c"v24@0:8@16",
    );
    class.add_method(
        objc::sel::get("windowDidFailToEnterFullScreen:"),
        did_fail_enter as objc::class::Imp,
        c"v24@0:8@16",
    );
    class.add_method(
        objc::sel::get("windowDidFailToExitFullScreen:"),
        did_fail_exit as objc::class::Imp,
        c"v24@0:8@16",
    );
    objc::class::register_pair(class);
    let object = objc::object::create_instance(class, 0);
    window_of(object).delegate = object;
    assert!(unsafe {
        init_with_content_rect_for_tests(
            object,
            NSRect {
                origin: NSPoint { x: 90.0, y: 110.0 },
                size: NSSize {
                    width: 700.0,
                    height: 480.0,
                },
            },
        )
    });
    unsafe {
        set_style_mask(
            object,
            objc::sel::get("setStyleMask:"),
            style::TITLED | style::CLOSABLE | style::RESIZABLE,
        )
    };
    let original = window_of(object).sdl;
    sdl3::video::show_window(original);
    window_of(object).shown = 1;
    let restore = unsafe { frame(object, objc::sel::get("frame")) };
    let pump = || {
        while let Some(event) = sdl3::video::poll() {
            crate::event::deliver(event);
        }
    };
    let settle = |predicate: &dyn Fn() -> bool| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            pump();
            if predicate() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fullscreen did not settle"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };

    RECREATE_ON_NEXT_ENTER.with(|flag| flag.set(true));
    unsafe {
        toggle_full_screen(
            object,
            objc::sel::get("toggleFullScreen:"),
            std::ptr::null_mut(),
        )
    };
    let replacement = window_of(object).sdl;
    assert_ne!(replacement, original, "will-enter must rebuild SDL surface");
    assert_eq!(window_of(object).fullscreen, 1);
    settle(&|| window_of(object).reported_fullscreen != 0);
    assert!(is_fullscreen(replacement));
    assert_eq!(FAILED_TRANSITIONS.get(), (0, 0));
    let host = host_frame(object).unwrap();

    let confinement = NSRect {
        origin: NSPoint { x: 19.0, y: 12.0 },
        size: NSSize {
            width: 80.0,
            height: 100.0,
        },
    };
    unsafe {
        set_mouse_confinement_rect(
            object,
            objc::sel::get("setMouseConfinementRect:"),
            confinement,
        )
    };
    let before = mouse_confinement_in_surface(object).unwrap();
    let logical = NSRect {
        origin: NSPoint {
            x: host.origin.x,
            y: host.origin.y + 31.0,
        },
        size: NSSize {
            width: host.size.width,
            height: host.size.height - 31.0,
        },
    };
    unsafe { set_frame(object, objc::sel::get("setFrame:"), logical) };
    let after = mouse_confinement_in_surface(object).unwrap();
    assert_eq!(after.x, before.x);
    assert_eq!(after.y, before.y - 31);
    assert_eq!((after.w, after.h), (80, 100));
    assert_eq!(window_of(object).mouse_confinement, confinement);

    REJECT_NEXT_FULLSCREEN_REQUEST.with(|flag| flag.set(true));
    unsafe {
        toggle_full_screen(
            object,
            objc::sel::get("toggleFullScreen:"),
            std::ptr::null_mut(),
        )
    };
    assert!(is_fullscreen(window_of(object).sdl));
    assert_eq!(window_of(object).fullscreen_transition, 0);
    assert_eq!(FAILED_TRANSITIONS.get(), (0, 1));
    assert_eq!(unsafe { frame(object, objc::sel::get("frame")) }, logical);

    unsafe {
        toggle_full_screen(
            object,
            objc::sel::get("toggleFullScreen:"),
            std::ptr::null_mut(),
        )
    };
    settle(&|| window_of(object).reported_fullscreen == 0 && host_frame(object) == Some(restore));
    assert_eq!(window_of(object).mouse_confinement, confinement);
    let restored = mouse_confinement_in_surface(object).unwrap();
    assert_eq!(restored.y, restore.size.height.round() as i32 - 112);
    assert_eq!(unsafe { frame(object, objc::sel::get("frame")) }, restore);

    REJECT_NEXT_FULLSCREEN_REQUEST.with(|flag| flag.set(true));
    unsafe {
        toggle_full_screen(
            object,
            objc::sel::get("toggleFullScreen:"),
            std::ptr::null_mut(),
        )
    };
    assert!(!is_fullscreen(window_of(object).sdl));
    assert_eq!(window_of(object).fullscreen_transition, 0);
    assert_eq!(window_of(object).fullscreen_restore, NSRect::default());
    assert_eq!(FAILED_TRANSITIONS.get(), (1, 1));

    unsafe {
        toggle_full_screen(
            object,
            objc::sel::get("toggleFullScreen:"),
            std::ptr::null_mut(),
        )
    };
    settle(&|| window_of(object).reported_fullscreen != 0);
    unsafe {
        toggle_full_screen(
            object,
            objc::sel::get("toggleFullScreen:"),
            std::ptr::null_mut(),
        )
    };
    settle(&|| window_of(object).reported_fullscreen == 0 && host_frame(object) == Some(restore));
    objc::arc::release(object);
    pump();

    // A will-enter callback can also drop the caller's last NSWindow reference.
    // toggleFullScreen: must not dereference the object after it is finalized.
    let releasing_class = objc::class::allocate_pair(
        objc::class::lookup(c"NSWindow"),
        c"NativeFullscreenReleaseRegressionWindow",
        0,
    )
    .unwrap();
    releasing_class.add_method(
        objc::sel::get("windowWillEnterFullScreen:"),
        release_on_will_enter as objc::class::Imp,
        c"v24@0:8@16",
    );
    objc::class::register_pair(releasing_class);
    let released = objc::object::create_instance(releasing_class, 0);
    window_of(released).delegate = released;
    assert!(unsafe {
        init_with_content_rect_for_tests(
            released,
            NSRect {
                origin: NSPoint { x: 110.0, y: 120.0 },
                size: NSSize {
                    width: 500.0,
                    height: 360.0,
                },
            },
        )
    });
    sdl3::video::show_window(window_of(released).sdl);
    window_of(released).shown = 1;
    unsafe {
        toggle_full_screen(
            released,
            objc::sel::get("toggleFullScreen:"),
            std::ptr::null_mut(),
        )
    };
    assert!(RELEASE_CALLBACK_SEEN.get());
    pump();
    objc::arc::pool_pop(pool);
}
