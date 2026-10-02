use forcefeedback::*;
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::Duration;

unsafe extern "C" fn rumble(context: *mut c_void, low: u16, high: u16) -> bool {
    unsafe { &*(context as *const Mutex<Vec<(u16, u16)>>) }
        .lock()
        .unwrap()
        .push((low, high));
    true
}
fn uuid(index: u8) -> *mut c_void {
    corefoundation::uuid::constant([
        0xe5, 0x59, 0xc4, index, 0xc5, 0xcd, 0x11, 0xd6, 0x8a, 0x1c, 0, 3, 0x93, 0x53, 0xbd, 0,
    ])
    .cast_mut()
}

#[test]
fn rumble_capabilities_parameters_and_effect_lifetimes_are_consistent() {
    if !sdl3::available() || !sdl3::virtual_joystick::available() {
        eprintln!("skipped: SDL3 virtual joystick support unavailable");
        return;
    }
    corefoundation::bootstrap();
    sdl3::init_controllers().unwrap();
    let calls = Mutex::new(Vec::<(u16, u16)>::new());
    let mut descriptor = sdl3::virtual_joystick::Desc {
        kind: 1,
        naxes: 6,
        nbuttons: 15,
        vendor_id: 0x1234,
        product_id: 0x1000,
        name: c"FF regression".as_ptr(),
        ..Default::default()
    };
    let unsupported = sdl3::virtual_joystick::attach(&descriptor).unwrap();
    unsafe {
        assert_eq!(FFIsForceFeedback(unsupported), 0x80004002u32 as i32);
    }
    let mut device = std::ptr::null_mut();
    unsafe {
        assert_eq!(
            FFCreateDevice(unsupported, &mut device),
            0x80004002u32 as i32
        );
    }
    assert!(device.is_null());
    sdl3::virtual_joystick::detach(unsupported);
    descriptor.rumble = Some(rumble);
    descriptor.userdata = (&calls as *const Mutex<Vec<(u16, u16)>>).cast_mut().cast();
    let id = sdl3::virtual_joystick::attach(&descriptor).unwrap();
    unsafe {
        assert_eq!(FFIsForceFeedback(id), 0);
        assert_eq!(FFCreateDevice(id, &mut device), 0);
    }
    let mut caps = FFCAPABILITIES::default();
    unsafe {
        assert_eq!(FFDeviceGetForceFeedbackCapabilities(device, &mut caps), 0);
    }
    assert_eq!(caps.playbackCapacity, 1);
    assert_eq!(caps.supportedEffects, 0x7f);
    assert_eq!(caps.emulatedEffects, 0x7f);
    assert_eq!(&caps.ffAxes[..2], &[0, 4]);
    let mut magnitude = -8000i32;
    let mut definition = FFEFFECT {
        dwSize: size_of::<FFEFFECT>() as u32,
        dwDuration: 1_000_000,
        dwGain: 5000,
        dwTriggerButton: u32::MAX,
        cbTypeSpecificParams: 4,
        lpvTypeSpecificParams: (&mut magnitude as *mut i32).cast(),
        ..Default::default()
    };
    let mut effect = std::ptr::null_mut();
    unsafe {
        assert_eq!(
            FFDeviceCreateEffect(device, uuid(0x67), &definition, &mut effect),
            0x80040306u32 as i32
        );
    }
    assert!(effect.is_null());
    unsafe {
        assert_eq!(
            FFDeviceCreateEffect(device, uuid(0x60), &definition, &mut effect),
            0
        );
        assert_eq!(FFEffectStart(effect, 1, 0), 0);
    }
    assert!(calls.lock().unwrap().contains(&(26214, 26214)));
    let mut status = 0;
    unsafe {
        assert_eq!(FFEffectGetEffectStatus(effect, &mut status), 0);
    }
    assert_eq!(status, 3);
    let gain = 2500u32;
    unsafe {
        assert_eq!(
            FFDeviceSetForceFeedbackProperty(device, 1, (&gain as *const u32).cast()),
            0
        );
    }
    assert_eq!(calls.lock().unwrap().last(), Some(&(6553, 6553)));

    // Updating gain must not inspect unselected type-specific parameters.
    let gain_only = FFEFFECT {
        dwSize: size_of::<FFEFFECT>() as u32,
        dwGain: 10_000,
        ..Default::default()
    };
    unsafe {
        assert_eq!(FFEffectSetParameters(effect, &gain_only, 4), 0);
    }
    assert_eq!(calls.lock().unwrap().last(), Some(&(13107, 13107)));
    unsafe {
        assert_eq!(FFDeviceSendForceFeedbackCommand(device, 4), 0);
        assert_eq!(FFEffectGetEffectStatus(effect, &mut status), 0);
    }
    assert_eq!(status, 2);
    std::thread::sleep(Duration::from_millis(5));
    unsafe {
        assert_eq!(FFDeviceSendForceFeedbackCommand(device, 8), 0);
        assert_eq!(FFEffectGetEffectStatus(effect, &mut status), 0);
    }
    assert_eq!(status, 3);
    unsafe {
        assert_eq!(FFDeviceSendForceFeedbackCommand(device, 32), 0);
        assert_eq!(FFEffectGetEffectStatus(effect, &mut status), 0);
    }
    assert_eq!(status, 3, "muting does not pause the effect clock");
    assert_eq!(calls.lock().unwrap().last(), Some(&(0, 0)));
    unsafe {
        assert_eq!(FFDeviceSendForceFeedbackCommand(device, 16), 0);
    }
    assert_eq!(calls.lock().unwrap().last(), Some(&(13107, 13107)));

    let mut second = std::ptr::null_mut();
    unsafe {
        assert_eq!(
            FFDeviceCreateEffect(device, uuid(0x60), &definition, &mut second),
            0
        );
        assert_eq!(FFEffectStart(second, 1, 0), 0x80040201u32 as i32);
        assert_eq!(FFEffectStart(second, 1, 1), 0);
        assert_eq!(FFEffectGetEffectStatus(effect, &mut status), 0);
    }
    assert_eq!(status, 2, "solo start stops previous playback");
    let count = calls.lock().unwrap().len();
    unsafe {
        assert_eq!(FFDeviceReleaseEffect(device, effect), 0);
    }
    assert_eq!(
        calls.lock().unwrap().len(),
        count,
        "releasing an inactive effect must not stop another effect"
    );
    // Suppressing download preserves both current output and the uploaded
    // parameters used by a subsequent NODOWNLOAD start.
    let count = calls.lock().unwrap().len();
    unsafe {
        assert_eq!(FFEffectSetParameters(second, &gain_only, 4 | 0x80000000), 3);
        assert_ne!(FFEffectSetParameters(second, &gain_only, 0xa0000004), 0);
    }
    assert_eq!(calls.lock().unwrap().len(), count);
    unsafe {
        assert_eq!(FFEffectStart(second, 1, 0x80000000), 0);
    }
    assert_eq!(calls.lock().unwrap().last(), Some(&(6553, 6553)));
    unsafe {
        assert_eq!(FFEffectDownload(second), 0);
    }
    assert_eq!(calls.lock().unwrap().last(), Some(&(13107, 13107)));
    unsafe {
        assert_eq!(FFEffectUnload(second), 0);
        assert_eq!(FFEffectStart(second, 1, 0x80000000), 0x80040203u32 as i32);
        assert_eq!(FFEffectDownload(second), 0);
        assert_eq!(FFEffectStart(second, u32::MAX, 0), 0);
        assert_eq!(FFDeviceSendForceFeedbackCommand(device, 2), 0);
        assert_eq!(FFEffectGetEffectStatus(second, &mut status), 0);
    }
    assert_eq!(status, 2);

    let full_gain = 10_000u32;
    unsafe {
        assert_eq!(
            FFDeviceSetForceFeedbackProperty(device, 1, (&full_gain as *const u32).cast()),
            0
        );
    }
    let mut ramp = [-10_000i32, 2000];
    let mut periodic = [2000i32, -1000, 0, 100_000];
    for (kind, params, size, strength) in [
        (0x61, ramp.as_mut_ptr(), 8, 39321),
        (0x62, periodic.as_mut_ptr(), 16, 19660),
        (0x63, periodic.as_mut_ptr(), 16, 19660),
        (0x64, periodic.as_mut_ptr(), 16, 19660),
        (0x65, periodic.as_mut_ptr(), 16, 19660),
        (0x66, periodic.as_mut_ptr(), 16, 19660),
    ] {
        let approximation = FFEFFECT {
            dwGain: 10_000,
            cbTypeSpecificParams: size,
            lpvTypeSpecificParams: params.cast(),
            ..definition
        };
        let mut other = std::ptr::null_mut();
        unsafe {
            assert_eq!(
                FFDeviceCreateEffect(device, uuid(kind), &approximation, &mut other),
                0
            );
            assert_eq!(FFEffectStart(other, 1, 0), 0);
        }
        assert_eq!(calls.lock().unwrap().last(), Some(&(strength, strength)));
        unsafe {
            assert_eq!(FFDeviceReleaseEffect(device, other), 0);
        }
    }
    let mut incomplete = std::ptr::null_mut();
    unsafe {
        assert_eq!(
            FFDeviceCreateEffect(device, uuid(0x60), std::ptr::null(), &mut incomplete),
            0
        );
        assert_eq!(
            FFEffectSetParameters(incomplete, &gain_only, 4),
            0x80040206u32 as i32
        );
        assert_eq!(FFEffectDownload(incomplete), 0x80040206u32 as i32);
        assert_eq!(FFDeviceReleaseEffect(device, incomplete), 0);
    }
    definition.dwDuration = 2000;
    unsafe {
        assert_eq!(FFEffectSetParameters(second, &definition, 1), 0);
        assert_eq!(FFEffectStart(second, 1, 0), 0);
    }
    std::thread::sleep(Duration::from_millis(8));
    sdl3::update_joysticks();
    unsafe {
        assert_eq!(FFEffectGetEffectStatus(second, &mut status), 0);
    }
    assert_eq!(status, 2, "expired effects must stop reporting PLAYING");
    let mut state = 0;
    unsafe {
        assert_eq!(FFDeviceGetForceFeedbackState(device, &mut state), 0);
    }
    assert_ne!(state & 2, 0);
    unsafe {
        assert_eq!(FFDeviceSendForceFeedbackCommand(device, 1), 0);
        assert_eq!(FFDeviceGetForceFeedbackState(device, &mut state), 0);
        assert_ne!(FFEffectStart(second, 1, 0), 0);
    }
    assert_eq!(
        state & (1 | 2 | 32),
        1 | 2 | 32,
        "reset clears effects and disables actuators"
    );
    unsafe {
        assert_eq!(FFReleaseDevice(device), 0);
    }
    let count = calls.lock().unwrap().len();
    unsafe {
        assert_ne!(FFEffectStart(second, 1, 0), 0);
        assert_ne!(FFEffectGetEffectStatus(second, &mut status), 0);
        assert_ne!(FFEffectStop(second), 0);
    }
    assert_eq!(
        calls.lock().unwrap().len(),
        count,
        "released device handles must not reach SDL"
    );
    // Disconnection keeps the opaque references safe while reporting loss.
    let mut disconnected_device = std::ptr::null_mut();
    let mut disconnected_effect = std::ptr::null_mut();
    unsafe {
        assert_eq!(FFCreateDevice(id, &mut disconnected_device), 0);
        assert_eq!(
            FFDeviceCreateEffect(
                disconnected_device,
                uuid(0x60),
                &definition,
                &mut disconnected_effect
            ),
            0
        );
    }
    assert!(sdl3::virtual_joystick::detach(id));
    sdl3::update_joysticks();
    unsafe {
        assert_eq!(
            FFEffectStart(disconnected_effect, 1, 0),
            0x80040209u32 as i32
        );
        assert_eq!(
            FFEffectGetEffectStatus(disconnected_effect, &mut status),
            0x80040209u32 as i32
        );
        assert_eq!(
            FFDeviceGetForceFeedbackState(disconnected_device, &mut state),
            0
        );
        assert_ne!(state & 0x80000000, 0);
        assert_eq!(FFReleaseDevice(disconnected_device), 0);
    }
}
