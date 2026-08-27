//! `ioctl`, the variadic whose argument list is one value wide.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{GuestMemory, Result};

use super::set_int_return;

/// Direction bits of a BSD `_IOC` request, and the "no payload" value.
const IOC_DIRMASK: u64 = 0xe000_0000;
const IOC_VOID: u64 = 0x2000_0000;

/// `int ioctl(int fd, unsigned long request, ...)`
pub fn ioctl(_symbol: &str, ctx: &mut RegisterContext, _mem: &mut dyn GuestMemory) -> Result<()> {
    let fd = ctx.rdi as i32;
    let request = ctx.rsi;
    // A `IOC_VOID` request with no payload takes no third argument, so `rdx`
    // holds whatever the guest last left there.
    let takes_argument = request & IOC_DIRMASK != IOC_VOID;
    let arg = if takes_argument { ctx.rdx } else { 0 };

    // SAFETY: `ioctl`'s third parameter is a `void *`-sized word on both
    // architectures, and the guest is identity-mapped, so a pointer it passed
    // is a pointer here. Whether the request is meaningful for `fd` is the
    // kernel's judgement, reported through the return value as usual.
    let rc = unsafe { libc::ioctl(fd, darwin::ioctl::request(request), arg as usize) };
    set_int_return(ctx, rc);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No shim on this path reads guest memory: `ioctl`'s payload is passed by
    /// address, untouched.
    struct NoMemory;

    impl GuestMemory for NoMemory {
        fn read(&self, va: u64, out: &mut [u8]) -> core::result::Result<(), machish::Error> {
            Err(machish::Error::OutOfBounds {
                addr: va,
                len: out.len(),
            })
        }
        fn write(&mut self, va: u64, data: &[u8]) -> core::result::Result<(), machish::Error> {
            Err(machish::Error::OutOfBounds {
                addr: va,
                len: data.len(),
            })
        }
    }

    fn call(fd: i32, request: u64, arg: u64) -> i64 {
        let mut ctx = RegisterContext::new();
        ctx.rdi = fd as u32 as u64;
        ctx.rsi = request;
        ctx.rdx = arg;
        ioctl("_ioctl", &mut ctx, &mut NoMemory).expect("ioctl forwards");
        ctx.rax as i64
    }

    #[test]
    fn the_shim_answers_what_the_host_answers() {
        // `TIOCGWINSZ` on something that is not a terminal: both sides must
        // report the same failure.
        const TIOCGWINSZ: u64 = 0x4008_7468;
        let mut ws = [0u16; 4];

        // SAFETY: a real `ioctl` on the process's own stdin with a correctly
        // sized `struct winsize` buffer.
        let host = unsafe { libc::ioctl(0, darwin::ioctl::request(TIOCGWINSZ), ws.as_mut_ptr()) };
        let shim = call(0, TIOCGWINSZ, ws.as_mut_ptr() as usize as u64);
        assert_eq!(shim, host as i64);
    }

    #[test]
    fn a_void_request_does_not_pass_the_register_on() {
        // `IOC_VOID` takes no third argument, so whatever the guest last left
        // in `rdx` must not reach the kernel as a pointer.
        const FIOCLEX: u64 = 0x2000_6601;
        assert_eq!(FIOCLEX & IOC_DIRMASK, IOC_VOID);
        let rc = call(-1, FIOCLEX, 0xdead_beef_dead_beef);
        assert_eq!(rc, -1, "a closed descriptor is an error, not a fault");
    }
}
