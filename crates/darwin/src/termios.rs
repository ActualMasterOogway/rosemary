#![cfg(not(target_os = "macos"))]

use std::ffi::c_int;

/// Darwin's `termios`, whose speeds follow the control-character array.
#[repr(C)]
struct Termios {
    pub c_iflag: u64,
    pub c_oflag: u64,
    pub c_cflag: u64,
    pub c_lflag: u64,
    pub c_cc: [u8; 20],
    pub c_ispeed: u64,
    pub c_ospeed: u64,
}

#[cfg(target_os = "linux")]
fn map_to_darwin(host: libc::tcflag_t, pairs: &[(u64, libc::tcflag_t)]) -> u64 {
    pairs.iter().fold(0, |out, (darwin, linux)| {
        out | if host & linux != 0 { *darwin } else { 0 }
    })
}

#[cfg(target_os = "linux")]
fn map_to_host(darwin: u64, pairs: &[(u64, libc::tcflag_t)]) -> libc::tcflag_t {
    pairs.iter().fold(0, |out, (darwin_bit, linux)| {
        out | if darwin & darwin_bit != 0 { *linux } else { 0 }
    })
}

#[cfg(target_os = "linux")]
const IFLAGS: &[(u64, libc::tcflag_t)] = &[
    (0x1, libc::IGNBRK),
    (0x2, libc::BRKINT),
    (0x4, libc::IGNPAR),
    (0x8, libc::PARMRK),
    (0x10, libc::INPCK),
    (0x20, libc::ISTRIP),
    (0x40, libc::INLCR),
    (0x80, libc::IGNCR),
    (0x100, libc::ICRNL),
    (0x200, libc::IXON),
    (0x400, libc::IXOFF),
    (0x800, libc::IXANY),
    (0x2000, libc::IMAXBEL),
];
#[cfg(target_os = "linux")]
const OFLAGS: &[(u64, libc::tcflag_t)] = &[(0x1, libc::OPOST), (0x2, libc::ONLCR)];
#[cfg(target_os = "linux")]
const CFLAGS: &[(u64, libc::tcflag_t)] = &[
    (0x400, libc::CSTOPB),
    (0x800, libc::CREAD),
    (0x1000, libc::PARENB),
    (0x2000, libc::PARODD),
    (0x4000, libc::HUPCL),
    (0x8000, libc::CLOCAL),
];
#[cfg(target_os = "linux")]
const LFLAGS: &[(u64, libc::tcflag_t)] = &[
    (0x1, libc::ECHOKE),
    (0x2, libc::ECHOE),
    (0x4, libc::ECHOK),
    (0x8, libc::ECHO),
    (0x10, libc::ECHONL),
    (0x20, libc::ECHOPRT),
    (0x40, libc::ECHOCTL),
    (0x80, libc::ISIG),
    (0x100, libc::ICANON),
    (0x400, libc::IEXTEN),
    (0x00400000, libc::TOSTOP),
    (0x00800000, libc::FLUSHO),
    (0x20000000, libc::PENDIN),
    (0x80000000, libc::NOFLSH),
];

#[cfg(target_os = "linux")]
fn speed_to_darwin(speed: libc::speed_t) -> u64 {
    for (darwin, linux) in SPEEDS {
        if speed == *linux {
            return *darwin;
        }
    }
    speed as u64
}

#[cfg(target_os = "linux")]
fn speed_to_host(speed: u64) -> libc::speed_t {
    for (darwin, linux) in SPEEDS {
        if speed == *darwin {
            return *linux;
        }
    }
    speed as libc::speed_t
}

#[cfg(target_os = "linux")]
const SPEEDS: &[(u64, libc::speed_t)] = &[
    (0, libc::B0),
    (50, libc::B50),
    (75, libc::B75),
    (110, libc::B110),
    (134, libc::B134),
    (150, libc::B150),
    (200, libc::B200),
    (300, libc::B300),
    (600, libc::B600),
    (1200, libc::B1200),
    (1800, libc::B1800),
    (2400, libc::B2400),
    (4800, libc::B4800),
    (9600, libc::B9600),
    (19200, libc::B19200),
    (38400, libc::B38400),
    (57600, libc::B57600),
    (115200, libc::B115200),
    (230400, libc::B230400),
];

#[cfg(target_os = "linux")]
const CC: &[(usize, usize)] = &[
    (0, libc::VEOF),
    (1, libc::VEOL),
    (2, libc::VEOL2),
    (3, libc::VERASE),
    (4, libc::VWERASE),
    (5, libc::VKILL),
    (6, libc::VREPRINT),
    (8, libc::VINTR),
    (9, libc::VQUIT),
    (10, libc::VSUSP),
    (12, libc::VSTART),
    (13, libc::VSTOP),
    (14, libc::VLNEXT),
    (15, libc::VDISCARD),
    (16, libc::VMIN),
    (17, libc::VTIME),
];

#[cfg(target_os = "linux")]
unsafe extern "C" fn tcgetattr(fd: c_int, out: *mut Termios) -> c_int {
    let mut host = std::mem::MaybeUninit::<libc::termios>::uninit();
    let status = unsafe { libc::tcgetattr(fd, host.as_mut_ptr()) };
    if status != 0 {
        return status;
    }
    let host = unsafe { host.assume_init() };
    let mut cc = [0; 20];
    for (darwin, linux) in CC {
        cc[*darwin] = host.c_cc[*linux];
    }
    let char_size = host.c_cflag & libc::CSIZE;
    let darwin_char_size = match char_size {
        libc::CS6 => 0x100,
        libc::CS7 => 0x200,
        libc::CS8 => 0x300,
        _ => 0,
    };
    unsafe {
        out.write(Termios {
            c_iflag: map_to_darwin(host.c_iflag, IFLAGS),
            c_oflag: map_to_darwin(host.c_oflag, OFLAGS),
            c_cflag: map_to_darwin(host.c_cflag, CFLAGS) | darwin_char_size,
            c_lflag: map_to_darwin(host.c_lflag, LFLAGS),
            c_cc: cc,
            c_ispeed: speed_to_darwin(libc::cfgetispeed(&host)),
            c_ospeed: speed_to_darwin(libc::cfgetospeed(&host)),
        })
    };
    0
}

#[cfg(target_os = "linux")]
unsafe extern "C" fn tcsetattr(fd: c_int, action: c_int, input: *const Termios) -> c_int {
    let input = unsafe { &*input };
    let mut host = unsafe { std::mem::MaybeUninit::<libc::termios>::zeroed().assume_init() };
    host.c_iflag = map_to_host(input.c_iflag, IFLAGS);
    host.c_oflag = map_to_host(input.c_oflag, OFLAGS);
    host.c_cflag = map_to_host(input.c_cflag, CFLAGS)
        | match input.c_cflag & 0x300 {
            0x100 => libc::CS6,
            0x200 => libc::CS7,
            0x300 => libc::CS8,
            _ => libc::CS5,
        };
    host.c_lflag = map_to_host(input.c_lflag, LFLAGS);
    for (darwin, linux) in CC {
        host.c_cc[*linux] = input.c_cc[*darwin];
    }
    unsafe {
        libc::cfsetispeed(&mut host, speed_to_host(input.c_ispeed));
        libc::cfsetospeed(&mut host, speed_to_host(input.c_ospeed));
    }
    let host_action = match action {
        0 => libc::TCSANOW,
        1 => libc::TCSADRAIN,
        2 => libc::TCSAFLUSH,
        _ => action,
    };
    unsafe { libc::tcsetattr(fd, host_action, &host) }
}

// The remaining non-Darwin hosts are the BSDs, which order the fields the same
// way but keep `tcflag_t` at 32 bits where Darwin has 64 - so this is a
// placeholder for a host nothing builds for yet rather than a checked cast.
#[cfg(not(target_os = "linux"))]
pub unsafe extern "C" fn tcgetattr(fd: c_int, out: *mut Termios) -> c_int {
    unsafe { libc::tcgetattr(fd, out.cast()) }
}

#[cfg(not(target_os = "linux"))]
pub unsafe extern "C" fn tcsetattr(fd: c_int, action: c_int, input: *const Termios) -> c_int {
    unsafe { libc::tcsetattr(fd, action, input.cast()) }
}

exports::exports! {
    functions {
        "tcgetattr" => tcgetattr,
        "tcsetattr" => tcsetattr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_terminal_entry_is_published() {
        for name in ["tcgetattr", "tcsetattr"] {
            assert!(symbol(name).is_some(), "{name}");
        }
    }
}
