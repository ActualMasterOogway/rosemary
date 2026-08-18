//! Process queries whose host answers describe Rosetta 3 rather than the guest.

use microlift::arch::x86_64::context::RegisterContext;

use super::bad_pointer;
use crate::abi::{GuestMemory, Result};

/// `int proc_pidpath(int pid, void *buffer, uint32_t buffersize)`.
///
/// The host implementation would identify the arm64 mirror executable.
pub fn proc_pidpath(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    let Some(path) = crate::dyld::executable_path() else {
        crate::shims::set_int_return(ctx, -1);
        return Ok(());
    };
    let bytes = path.as_bytes();
    let capacity = ctx.rdx as usize;
    if ctx.rsi == 0 || capacity <= bytes.len() {
        crate::shims::set_int_return(ctx, -1);
        return Ok(());
    }
    mem.write(ctx.rsi, bytes)
        .map_err(|_| bad_pointer(symbol, ctx.rsi))?;
    mem.write(ctx.rsi + bytes.len() as u64, &[0])
        .map_err(|_| bad_pointer(symbol, ctx.rsi + bytes.len() as u64))?;
    crate::shims::set_int_return(ctx, bytes.len() as i32);
    Ok(())
}

// `pthread_get_stackaddr_np` and `pthread_get_stacksize_np` live in
// [`super::machthread`], which has the `pthread_t` -> thread lookup.

/// The guest has no separately protected JIT mapping to toggle.
pub fn pthread_jit_write_protect_np(
    _symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    crate::shims::set_int_return(ctx, 0);
    Ok(())
}

pub type Shim = fn(&str, &mut RegisterContext, &mut dyn GuestMemory) -> Result<()>;

pub fn shim_for(name: &str) -> Option<Shim> {
    match name {
        "proc_pidpath" => Some(proc_pidpath),
        "pthread_jit_write_protect_np" => Some(pthread_jit_write_protect_np),
        _ => None,
    }
}
