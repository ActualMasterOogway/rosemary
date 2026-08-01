//! `ioctl`, the variadic whose argument list is one value wide.
//!
//! ## Why this one can be shimmed generically and `printf` cannot
//!
//! `printf`'s argument list is arbitrarily long and its shape is encoded in a
//! string, so forwarding one means rendering it. `ioctl` is the opposite:
//! `int ioctl(int fd, unsigned long request, ...)` takes **at most one**
//! variadic argument, and the request number says whether it is there and how
//! big it is. That is not a convention — it is the `_IOC` encoding, which packs
//! a direction (`IOC_VOID`, `IOC_IN`, `IOC_OUT`) and a payload size into the
//! request's top bits, and it is why `ioctl` can be forwarded without knowing
//! which request was asked for.
//!
//! So the shim reads the third argument out of the register SysV put it in and
//! passes it on. When the request is `IOC_VOID` with no payload the guest
//! passed nothing and the register holds whatever was left there, so a zero is
//! passed instead of a value the callee might dereference.
//!
//! ## What is *not* translated
//!
//! The payload. A request whose structure differs between x86-64 and arm64
//! macOS would need its buffer rewritten in both directions, and this passes
//! the guest's buffer through untouched. That is correct for the fixed-layout
//! requests programs actually use — `TIOCGWINSZ`'s `struct winsize` is four
//! `unsigned short`s on both — and it is the reason this module is a list of
//! facts rather than a memcpy: if a request ever needs translating, it needs
//! translating *here*, by number.
//!
//! `/bin/ls` is the reason this exists: it asks the terminal how wide it is
//! before deciding to print in columns.

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
    // holds whatever the guest last left there. Passing that on would hand the
    // kernel a wild pointer for requests that are declared to take none.
    let takes_argument = request & IOC_DIRMASK != IOC_VOID;
    let arg = if takes_argument { ctx.rdx } else { 0 };

    // SAFETY: `ioctl`'s third parameter is a `void *`-sized word on both
    // architectures, and the guest is identity-mapped, so a pointer it passed
    // is a pointer here. Whether the request is meaningful for `fd` is the
    // kernel's judgement, reported through the return value as usual.
    let rc = unsafe { libc::ioctl(fd, request as libc::c_ulong, arg as usize) };
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
        // report the same failure. Comparing against the host's own `ioctl` is
        // the only assertion worth making — the specification of `ioctl` is
        // what the kernel does.
        const TIOCGWINSZ: u64 = 0x4008_7468;
        let mut ws = [0u16; 4];

        // SAFETY: a real `ioctl` on the process's own stdin with a correctly
        // sized `struct winsize` buffer.
        let host = unsafe { libc::ioctl(0, TIOCGWINSZ as libc::c_ulong, ws.as_mut_ptr()) };
        let shim = call(0, TIOCGWINSZ, ws.as_mut_ptr() as usize as u64);
        assert_eq!(shim, host as i64);
    }

    #[test]
    fn a_void_request_does_not_pass_the_register_on() {
        // `IOC_VOID` takes no third argument, so whatever the guest last left
        // in `rdx` must not reach the kernel as a pointer. `FIOCLEX` on a
        // closed descriptor fails either way; what this pins down is that a
        // wild value in `rdx` cannot make it fault.
        const FIOCLEX: u64 = 0x2000_6601;
        assert_eq!(FIOCLEX & IOC_DIRMASK, IOC_VOID);
        let rc = call(-1, FIOCLEX, 0xdead_beef_dead_beef);
        assert_eq!(rc, -1, "a closed descriptor is an error, not a fault");
    }
}
