use core::ffi::c_void;

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{Callbacks, GuestMemory, Result};

use super::set_int_return;

/// `kern_return_t IOServiceAddMatchingNotification( IONotificationPortRef, const
/// io_name_t, CFDictionaryRef, IOServiceMatchingCallback, void *, io_iterator_t *)`
type AddMatchingNotificationFn = unsafe extern "C" fn(
    *mut c_void,
    *const c_void,
    *const c_void,
    *mut c_void,
    *mut c_void,
    *mut u32,
) -> i32;

fn host_function() -> Option<AddMatchingNotificationFn> {
    static FOUND: std::sync::OnceLock<Option<AddMatchingNotificationFn>> =
        std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        let name = c"IOServiceAddMatchingNotification";
        // SAFETY: `name` is NUL-terminated and RTLD_DEFAULT is the process's
        // loaded frameworks. The type is the declaration from IOKitLib.h.
        let address = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
        (!address.is_null()).then(|| unsafe {
            core::mem::transmute::<*mut c_void, AddMatchingNotificationFn>(address)
        })
    })
}

fn add_matching_notification(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let Some(host) = host_function() else {
        return Err(crate::abi::AbiError::Unsatisfied {
            symbol: symbol.to_string(),
        });
    };

    // The callback is the fourth argument.
    let callback = callbacks.substitute(symbol, ctx.rcx)?;
    let status = unsafe {
        host(
            ctx.rdi as *mut c_void,
            ctx.rsi as *const c_void,
            ctx.rdx as *const c_void,
            callback as *mut c_void,
            ctx.r8 as *mut c_void,
            ctx.r9 as *mut u32,
        )
    };
    set_int_return(ctx, status);
    Ok(())
}

pub type Shim = crate::signal::Shim;

pub fn shim_for(name: &str) -> Option<Shim> {
    match name {
        "IOServiceAddMatchingNotification" => Some(add_matching_notification),
        _ => None,
    }
}
