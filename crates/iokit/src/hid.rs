use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::AtomicU8;

use corefoundation::base::CFTypeID;

static DEVICE_TYPE: AtomicU8 = AtomicU8::new(0);
static ELEMENT_TYPE: AtomicU8 = AtomicU8::new(0);

pub fn device_type_id() -> CFTypeID {
    std::ptr::addr_of!(DEVICE_TYPE) as CFTypeID
}

pub fn element_type_id() -> CFTypeID {
    std::ptr::addr_of!(ELEMENT_TYPE) as CFTypeID
}

pub type RemovalCallback = unsafe extern "C" fn(*mut c_void, i32, *mut c_void);

pub mod page {
    pub const GENERIC_DESKTOP: u32 = 0x01;
    pub const BUTTON: u32 = 0x09;
}

pub mod usage {
    pub const X: u32 = 0x30;
    pub const Y: u32 = 0x31;
    pub const Z: u32 = 0x32;
    pub const RX: u32 = 0x33;
    pub const RY: u32 = 0x34;
    pub const RZ: u32 = 0x35;
    pub const HATSWITCH: u32 = 0x39;
    pub const GAMEPAD: u32 = 0x05;
    pub const JOYSTICK: u32 = 0x04;
}

pub mod element_type {
    pub const INPUT_MISC: u32 = 1;
    pub const INPUT_BUTTON: u32 = 2;
    pub const INPUT_AXIS: u32 = 3;
    pub const COLLECTION: u32 = 513;
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Source {
    Axis(i32),
    Button(i32),
    Hat(i32),
    /// The device's root collection, which has a usage but no value.
    Collection,
}

#[derive(Clone, Debug)]
pub struct Element {
    pub cookie: u32,
    pub kind: u32,
    pub page: u32,
    pub usage: u32,
    pub logical_min: i64,
    pub logical_max: i64,
    pub source: Source,
}

/// Axes a HID gamepad reports, in the order SDL numbers them.
const AXIS_USAGES: &[u32] = &[
    usage::X,
    usage::Y,
    usage::Z,
    usage::RX,
    usage::RY,
    usage::RZ,
];

pub struct Device {
    pub id: sdl3::JoystickId,
    pub joystick: sdl3::Joystick,
    pub name: String,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
    pub elements: Vec<Element>,
    /// The value each element last reported, so a change can be noticed.
    last: Mutex<Vec<i64>>,
    /// A stable pointer per element, which is what an `IOHIDElementRef` is.
    ///
    /// Filled in once the device is inside its `Arc`, because each one holds the
    /// device's address and that is not known before then. See
    /// [`Device::publish`].
    handles: std::sync::OnceLock<Vec<crate::value::ElementRef>>,
    removal: Mutex<(Option<RemovalCallback>, usize)>,
}

// SAFETY: `joystick` is an SDL handle used only from the thread that opened it -
// which is the run loop's, since that is where the device list is serviced. The
// rest is plain data behind a lock.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    /// Open `id` and describe it as a tree of elements.
    pub fn open(id: sdl3::JoystickId) -> Option<std::sync::Arc<Device>> {
        let joystick = sdl3::open(id)?;
        // SAFETY: a joystick just opened and not yet closed.
        let (name, vendor, product, version) = unsafe {
            (
                sdl3::name(joystick),
                sdl3::vendor(joystick),
                sdl3::product(joystick),
                sdl3::version(joystick),
            )
        };
        // SAFETY: as above.
        let (axes, buttons, hats) = unsafe {
            (
                sdl3::axes(joystick),
                sdl3::buttons(joystick),
                sdl3::hats(joystick),
            )
        };
        let elements = describe(axes, buttons, hats);
        let last = Mutex::new(vec![i64::MIN; elements.len()]);
        let device = std::sync::Arc::new(Device {
            id,
            joystick,
            name,
            vendor,
            product,
            version,
            elements,
            last,
            handles: std::sync::OnceLock::new(),
            removal: Mutex::new((None, 0)),
        });
        device.publish();
        Some(device)
    }

    /// Give every element a stable handle, now that the device has an address.
    fn publish(self: &std::sync::Arc<Device>) {
        let base = std::sync::Arc::as_ptr(self);
        let handles: Vec<crate::value::ElementRef> = self
            .elements
            .iter()
            .map(|e| crate::value::ElementRef {
                device: base,
                cookie: e.cookie,
            })
            .collect();
        // The device and every element handle are things a guest may `CFRetain`
        // - see `corefoundation::base::note_opaque` - and both live as long as
        // the device does, which is as long as the process.
        corefoundation::base::note_typed_opaque(base as *const std::ffi::c_void, device_type_id());
        let _ = self.handles.set(handles);
        for handle in self.handles.get().into_iter().flatten() {
            corefoundation::base::note_typed_opaque(
                std::ptr::from_ref(handle) as *const std::ffi::c_void,
                element_type_id(),
            );
        }
    }

    /// The `IOHIDElementRef` for one element.
    pub fn handle(&self, cookie: u32) -> *const std::ffi::c_void {
        self.handles
            .get()
            .and_then(|h| h.get(cookie as usize))
            .map_or(std::ptr::null(), |r| {
                r as *const _ as *const std::ffi::c_void
            })
    }

    pub fn element(&self, cookie: u32) -> Option<&Element> {
        self.elements.get(cookie as usize)
    }

    pub fn set_removal_callback(&self, callback: Option<RemovalCallback>, context: usize) {
        *self.removal.lock().unwrap_or_else(|e| e.into_inner()) = (callback, context);
    }

    pub fn notify_removed(&self) {
        let (callback, context) = *self.removal.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(callback) = callback {
            // SAFETY: the registration supplies an IOHIDCallback and its
            // untouched context. The device remains alive for this call.
            unsafe {
                callback(
                    context as *mut c_void,
                    0,
                    std::ptr::from_ref(self).cast_mut().cast(),
                )
            };
        }
    }

    /// What an element reads right now.
    pub fn value(&self, element: &Element) -> i64 {
        match element.source {
            // SAFETY: a joystick this device opened and has not closed.
            Source::Axis(i) => unsafe { sdl3::axis(self.joystick, i) as i64 },
            // SAFETY: as above.
            Source::Button(i) => unsafe { sdl3::button(self.joystick, i) as i64 },
            // SAFETY: as above.
            Source::Hat(i) => hat_to_direction(unsafe { sdl3::hat(self.joystick, i) }),
            Source::Collection => 0,
        }
    }

    /// Every element whose value changed since the last time this was asked.
    ///
    /// The device is polled and the changes reported, rather than SDL's events
    /// being read: a HID input-value callback is *per element*, and an SDL event
    /// is per axis or per button with no element behind it. Polling the elements
    /// is what turns one into the other, and it is also what makes a device that
    /// was already moved when the guest registered its callback report its
    /// current state rather than nothing.
    pub fn changed(&self) -> Vec<(u32, i64)> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        for element in &self.elements {
            if element.source == Source::Collection {
                continue;
            }
            let now = self.value(element);
            let slot = &mut last[element.cookie as usize];
            if *slot != now {
                *slot = now;
                out.push((element.cookie, now));
            }
        }
        out
    }
}

fn hat_to_direction(bits: u8) -> i64 {
    const UP: u8 = 0x01;
    const RIGHT: u8 = 0x02;
    const DOWN: u8 = 0x04;
    const LEFT: u8 = 0x08;
    match bits {
        UP => 0,
        b if b == UP | RIGHT => 1,
        RIGHT => 2,
        b if b == DOWN | RIGHT => 3,
        DOWN => 4,
        b if b == DOWN | LEFT => 5,
        LEFT => 6,
        b if b == UP | LEFT => 7,
        _ => 8,
    }
}

/// The element tree for a device with this many axes, buttons and hats.
///
/// The root collection is element zero, so a guest walking children from the
/// top finds everything - which is what `IOHIDElementGetChildren` is for.
pub fn describe(axes: i32, buttons: i32, hats: i32) -> Vec<Element> {
    let mut out = vec![Element {
        cookie: 0,
        kind: element_type::COLLECTION,
        page: page::GENERIC_DESKTOP,
        usage: usage::GAMEPAD,
        logical_min: 0,
        logical_max: 0,
        source: Source::Collection,
    }];
    for i in 0..axes.max(0) {
        out.push(Element {
            cookie: out.len() as u32,
            kind: element_type::INPUT_AXIS,
            page: page::GENERIC_DESKTOP,
            // Past the six named axes a device is reporting something this
            // runtime has no HID usage for, so it gets none rather than a
            // neighbour's - a guest matching on usage will skip it, which is
            // better than it reading the wrong stick.
            usage: AXIS_USAGES.get(i as usize).copied().unwrap_or(0),
            logical_min: i16::MIN as i64,
            logical_max: i16::MAX as i64,
            source: Source::Axis(i),
        });
    }
    for i in 0..hats.max(0) {
        out.push(Element {
            cookie: out.len() as u32,
            kind: element_type::INPUT_MISC,
            page: page::GENERIC_DESKTOP,
            usage: usage::HATSWITCH,
            logical_min: 0,
            // Seven directions, and the eighth value is "centred".
            logical_max: 7,
            source: Source::Hat(i),
        });
    }
    for i in 0..buttons.max(0) {
        out.push(Element {
            cookie: out.len() as u32,
            kind: element_type::INPUT_BUTTON,
            page: page::BUTTON,
            // HID numbers buttons from one.
            usage: i as u32 + 1,
            logical_min: 0,
            logical_max: 1,
            source: Source::Button(i),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape a guest walks: a root collection, then axes on the
    /// generic-desktop page, then a hat, then buttons numbered from one.
    #[test]
    fn a_gamepads_elements_are_the_ones_a_guest_matches_on() {
        let elements = describe(6, 15, 1);
        assert_eq!(elements.len(), 1 + 6 + 1 + 15);

        assert_eq!(elements[0].kind, element_type::COLLECTION);
        assert_eq!(elements[0].usage, usage::GAMEPAD);

        // The two sticks and the two triggers, in HID's own order.
        let axes: Vec<u32> = elements[1..7].iter().map(|e| e.usage).collect();
        assert_eq!(
            axes,
            vec![
                usage::X,
                usage::Y,
                usage::Z,
                usage::RX,
                usage::RY,
                usage::RZ
            ]
        );
        assert!(
            elements[1..7]
                .iter()
                .all(|e| e.kind == element_type::INPUT_AXIS)
        );
        assert_eq!(elements[1].logical_min, -32768);
        assert_eq!(elements[1].logical_max, 32767);

        let hat = &elements[7];
        assert_eq!(hat.usage, usage::HATSWITCH);
        assert_eq!((hat.logical_min, hat.logical_max), (0, 7));

        // Buttons on their own page, numbered from one, and each element's
        // cookie is its own index - which is what makes a stored cookie find
        // the same element later.
        assert_eq!(elements[8].page, page::BUTTON);
        assert_eq!(elements[8].usage, 1);
        assert!(
            elements
                .iter()
                .enumerate()
                .all(|(i, e)| e.cookie == i as u32)
        );
    }

    /// A hat's bitfield becomes HID's clockwise-from-north direction, and
    /// centred is the value past the logical maximum.
    #[test]
    fn a_hat_reads_as_a_direction() {
        assert_eq!(hat_to_direction(0x01), 0, "up");
        assert_eq!(hat_to_direction(0x03), 1, "up and right");
        assert_eq!(hat_to_direction(0x02), 2, "right");
        assert_eq!(hat_to_direction(0x0c), 5, "down and left");
        assert_eq!(hat_to_direction(0x08), 6, "left");
        assert_eq!(hat_to_direction(0x00), 8, "centred");
        // An impossible combination is centred rather than a direction it is
        // not: up and down at once is a device fault, not a diagonal.
        assert_eq!(hat_to_direction(0x05), 8, "up and down");
    }

    /// A device with more axes than HID has named usages leaves the extra ones
    /// unnamed rather than reusing a usage that means something else.
    #[test]
    fn an_extra_axis_gets_no_usage_rather_than_a_wrong_one() {
        let elements = describe(8, 0, 0);
        assert_eq!(elements[7].usage, 0);
        assert_eq!(elements[8].usage, 0);
    }
}
