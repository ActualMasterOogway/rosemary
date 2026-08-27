//! `uname` and `compat_mode`: what the system says it is, and which century it is in.

use machish::resolve::SyntheticDylib;

use crate::sysctl::{
    CTL_HW, CTL_KERN, HW_MACHINE, KERN_HOSTNAME, KERN_OSRELEASE, KERN_OSTYPE, KERN_VERSION,
};

/// `_SYS_NAMELEN`, the width of every field in Darwin's `struct utsname`.
///
/// The reason `uname` cannot be passed through on Linux: glibc's fields are 65 bytes
/// and musl's are 65 too, so the host would write five short strings into a 1280-byte
/// buffer at the wrong five offsets.
pub const SYS_NAMELEN: usize = 256;

/// Darwin's `struct utsname`. Five fixed-width strings, nothing else.
#[repr(C)]
pub struct Utsname {
    pub sysname: [u8; SYS_NAMELEN],
    pub nodename: [u8; SYS_NAMELEN],
    pub release: [u8; SYS_NAMELEN],
    pub version: [u8; SYS_NAMELEN],
    pub machine: [u8; SYS_NAMELEN],
}

/// `int uname(struct utsname *)`.
///
/// Filled from this crate's own [`crate::sysctl`] rather than from the host, which is
/// what keeps `uname -m` and `sysctl hw.machine` agreeing: both are questions about
/// the guest, and answering them from two places is how they come to disagree.
///
/// # Safety
///
/// `out` points at a writable Darwin `struct utsname`.
unsafe extern "C" fn uname(out: *mut Utsname) -> i32 {
    if out.is_null() {
        crate::errno::set(libc::EFAULT);
        return -1;
    }
    let fields = [
        (0usize, ask(&[CTL_KERN, KERN_OSTYPE])),
        (1, ask(&[CTL_KERN, KERN_HOSTNAME])),
        (2, ask(&[CTL_KERN, KERN_OSRELEASE])),
        (3, ask(&[CTL_KERN, KERN_VERSION])),
        (4, ask(&[CTL_HW, HW_MACHINE])),
    ];
    for (index, value) in fields {
        // SAFETY: `out` is a writable `utsname`, whose five fields are contiguous
        // arrays of `SYS_NAMELEN` bytes each; `index` is one of the five.
        let field = unsafe { (out as *mut u8).add(index * SYS_NAMELEN) };
        let bytes = value.as_bytes();
        let n = bytes.len().min(SYS_NAMELEN - 1);
        // SAFETY: `field` is `SYS_NAMELEN` writable bytes and `n` is at most one less.
        unsafe {
            std::ptr::write_bytes(field, 0, SYS_NAMELEN);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), field, n);
        }
    }
    0
}

/// One `sysctl` string, or the empty string if nothing answers.
///
/// Goes through the exported `sysctl` rather than around it, so a guest that reads a
/// key directly and a guest that calls `uname` cannot be told different things.
fn ask(mib: &[i32]) -> String {
    let Some(sysctl) = crate::sysctl::symbol("sysctl") else {
        return String::new();
    };
    // SAFETY: `sysctl::symbol` returns this crate's own `sysctl`, whose prototype is
    // the one named here.
    let f: unsafe extern "C" fn(
        *const libc::c_int,
        libc::c_uint,
        *mut libc::c_void,
        *mut libc::size_t,
        *mut libc::c_void,
        libc::size_t,
    ) -> i32 = unsafe { std::mem::transmute(sysctl as usize as *const ()) };

    let mut buf = [0u8; SYS_NAMELEN];
    let mut len: libc::size_t = buf.len();
    // SAFETY: `mib` is live for its own length and `buf` for `len` bytes.
    let rc = unsafe {
        f(
            mib.as_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return String::new();
    }
    let end = buf[..len].iter().position(|b| *b == 0).unwrap_or(len);
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// `int compat_mode(const char *command, const char *mode)`.
///
/// Darwin-only, and there is nothing on Linux to pass it through to. A command asks
/// which of two historical behaviours it should present; `uname` asks about
/// `"unix2003"` and prints its fields in a different order depending on the answer.
///
/// A modern macOS answers 1 for `unix2003` unless `COMMAND_MODE` says `legacy`, which
/// is the rule reproduced here - `COMMAND_MODE` being the environment variable the
/// real implementation reads.
///
/// The comparison is case-insensitive because the callers are: `/usr/bin/uname` asks
/// about `"Unix2003"` with a capital U, and an exact match against the lowercase
/// spelling answers 0 - which is a legacy `uname -a`, with a `-p` field real macOS
/// does not print.
unsafe extern "C" fn compat_mode(
    _command: *const libc::c_char,
    mode: *const libc::c_char,
) -> libc::c_int {
    if mode.is_null() {
        return 0;
    }
    // SAFETY: the caller states `mode` is NUL-terminated.
    let mode = unsafe { std::ffi::CStr::from_ptr(mode) };
    let legacy = std::env::var("COMMAND_MODE").is_ok_and(|m| m.eq_ignore_ascii_case("legacy"));
    let mode = mode.to_bytes();
    if mode.eq_ignore_ascii_case(b"unix2003") {
        return libc::c_int::from(!legacy);
    }
    libc::c_int::from(mode.eq_ignore_ascii_case(b"legacy") && legacy)
}

pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "uname" => uname as *const (),
        "compat_mode" => compat_mode as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

pub fn provide(lib: &mut SyntheticDylib) {
    for name in ["uname", "compat_mode"] {
        crate::define_fn(lib, &format!("_{name}"), symbol(name).unwrap() as *const ());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysctl::{GuestArch, lock_guest_arch};

    fn field(u: &Utsname, index: usize) -> String {
        // SAFETY: the five fields are contiguous `SYS_NAMELEN` arrays and `index` is
        // one of the five.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (u as *const Utsname as *const u8).add(index * SYS_NAMELEN),
                SYS_NAMELEN,
            )
        };
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(SYS_NAMELEN);
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    }

    fn called() -> Utsname {
        // SAFETY: zero is a valid `utsname` - five empty strings.
        let mut u: Utsname = unsafe { std::mem::zeroed() };
        // SAFETY: `u` is a live writable `utsname`.
        assert_eq!(unsafe { uname(&mut u) }, 0);
        u
    }

    #[test]
    fn the_system_is_darwin_and_the_machine_is_the_guest_s() {
        let _held = lock_guest_arch(GuestArch::X86_64);
        let u = called();
        assert_eq!(field(&u, 0), "Darwin");
        assert_eq!(field(&u, 4), "x86_64");
        assert!(!field(&u, 2).is_empty(), "a release is reported");
    }

    /// `uname` and a direct `sysctl` are the same question, and must not be capable
    /// of different answers.
    #[test]
    fn uname_agrees_with_sysctl() {
        let _held = lock_guest_arch(GuestArch::Arm64);
        let u = called();
        assert_eq!(field(&u, 4), ask(&[CTL_HW, HW_MACHINE]));
    }

    /// Every field must be NUL-terminated within its own 256 bytes, because the guest
    /// reads each as a C string starting at a fixed offset.
    #[test]
    fn every_field_is_terminated_inside_its_own_width() {
        let u = called();
        // SAFETY: `u` is a live `utsname`, read as its own bytes.
        let bytes = unsafe {
            std::slice::from_raw_parts(&u as *const Utsname as *const u8, size_of::<Utsname>())
        };
        for i in 0..5 {
            let f = &bytes[i * SYS_NAMELEN..(i + 1) * SYS_NAMELEN];
            assert!(f.contains(&0), "field {i} runs into the next");
        }
    }

    #[test]
    fn the_struct_is_the_width_darwin_declares() {
        assert_eq!(size_of::<Utsname>(), 5 * 256);
    }

    /// The capitalised spelling is not a corner case - it is the one `/usr/bin/uname`
    /// actually passes, and answering 0 to it prints a field real macOS does not.
    #[test]
    fn unix2003_is_in_effect_whatever_case_it_is_asked_in() {
        let cmd = std::ffi::CString::new("bin/uname").unwrap();
        for spelling in ["unix2003", "Unix2003", "UNIX2003"] {
            let mode = std::ffi::CString::new(spelling).unwrap();
            // SAFETY: both arguments are NUL-terminated and outlive the call.
            assert_eq!(unsafe { compat_mode(cmd.as_ptr(), mode.as_ptr()) }, 1, "{spelling}");
        }
        let legacy = std::ffi::CString::new("legacy").unwrap();
        // SAFETY: as above.
        unsafe {
            assert_eq!(compat_mode(cmd.as_ptr(), legacy.as_ptr()), 0);
            assert_eq!(compat_mode(cmd.as_ptr(), std::ptr::null()), 0);
        }
    }
}
