use core::ffi::{c_int, c_void};

use crate::signal::{Code, Context};

pub const SIGEMT: Option<c_int> = None;
pub const SIGINFO: Option<c_int> = None;

const ILL: [Code; 8] = [
    Code::IllegalOpcode,
    Code::IllegalOperand,
    Code::IllegalAddressing,
    Code::IllegalTrap,
    Code::PrivilegedOpcode,
    Code::PrivilegedRegister,
    Code::Coprocessor,
    Code::BadStack,
];

const FPE: [Code; 8] = [
    Code::IntegerDivide,
    Code::IntegerOverflow,
    Code::FloatDivide,
    Code::FloatOverflow,
    Code::FloatUnderflow,
    Code::FloatInexact,
    Code::FloatInvalid,
    Code::FloatSubscript,
];

pub fn code(signo: c_int, code: c_int) -> Code {
    match code {
        libc::SI_USER => Code::User,
        libc::SI_TKILL => Code::Thread,
        libc::SI_QUEUE => Code::Queue,
        libc::SI_TIMER => Code::Timer,
        libc::SI_ASYNCIO => Code::AsyncIo,
        libc::SI_MESGQ => Code::MessageQueue,
        libc::SI_KERNEL | libc::SI_SIGIO => Code::Kernel,
        _ => crate::signal::by_signal(signo, code, &ILL, &FPE),
    }
}

#[cfg(target_arch = "x86_64")]
mod machine {
    use super::*;
    use crate::signal::Machine;

    const GPR: [c_int; 16] = [
        libc::REG_RAX,
        libc::REG_RCX,
        libc::REG_RDX,
        libc::REG_RBX,
        libc::REG_RSP,
        libc::REG_RBP,
        libc::REG_RSI,
        libc::REG_RDI,
        libc::REG_R8,
        libc::REG_R9,
        libc::REG_R10,
        libc::REG_R11,
        libc::REG_R12,
        libc::REG_R13,
        libc::REG_R14,
        libc::REG_R15,
    ];

    pub unsafe fn context(raw: *const c_void) -> Option<Context> {
        // SAFETY: the caller's live `ucontext_t`.
        let uc = unsafe { &*raw.cast::<libc::ucontext_t>() };
        let reg = |i: c_int| uc.uc_mcontext.gregs[i as usize] as u64;
        // cs, gs, fs, ss, a 16-bit selector each.
        let segments = reg(libc::REG_CSGSFS);
        let fxsave = match uc.uc_mcontext.fpregs.is_null() {
            true => None,
            // SAFETY: the kernel's `FXSAVE` image, 512 bytes.
            false => Some(unsafe { uc.uc_mcontext.fpregs.cast::<[u8; 512]>().read_unaligned() }),
        };
        Some(Context {
            machine: Machine {
                gpr: GPR.map(reg),
                rip: reg(libc::REG_RIP),
                rflags: reg(libc::REG_EFL),
                cs: segments as u16,
                gs: (segments >> 16) as u16,
                fs: (segments >> 32) as u16,
                trapno: reg(libc::REG_TRAPNO) as u32,
                err: reg(libc::REG_ERR),
                fault_address: reg(libc::REG_CR2),
                fxsave,
            },
            mask: uc.uc_sigmask,
            stack: uc.uc_stack,
        })
    }

    pub unsafe fn set_context(raw: *mut c_void, context: &Context) {
        // SAFETY: the caller's live `ucontext_t`.
        let uc = unsafe { &mut *raw.cast::<libc::ucontext_t>() };
        let machine = &context.machine;
        for (value, i) in machine.gpr.iter().zip(GPR) {
            uc.uc_mcontext.gregs[i as usize] = *value as i64;
        }
        uc.uc_mcontext.gregs[libc::REG_RIP as usize] = machine.rip as i64;
        uc.uc_mcontext.gregs[libc::REG_EFL as usize] = machine.rflags as i64;
        if let Some(fxsave) = machine.fxsave
            && !uc.uc_mcontext.fpregs.is_null()
        {
            // SAFETY: as in `context`.
            unsafe {
                uc.uc_mcontext
                    .fpregs
                    .cast::<[u8; 512]>()
                    .write_unaligned(fxsave)
            };
        }
        uc.uc_sigmask = context.mask;
    }
}

#[cfg(target_arch = "aarch64")]
mod machine {
    use super::*;
    use crate::signal::{Fpsimd, Machine};

    // The tail of `mcontext_t` is a list of tagged records, `struct
    // _aarch64_ctx` in the kernel's `sigcontext.h`, ended by a zero magic.
    // The kernel aligns it to 16; `libc` does not.
    const RESERVED: usize =
        (core::mem::offset_of!(libc::mcontext_t, pstate) + 8).next_multiple_of(16);
    const RESERVED_SIZE: usize = 4096;
    const FPSIMD_MAGIC: u32 = 0x4650_8001;
    const ESR_MAGIC: u32 = 0x4553_5201;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Head {
        magic: u32,
        size: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FpsimdRecord {
        head: Head,
        fpsr: u32,
        fpcr: u32,
        vregs: [u128; 32],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct EsrRecord {
        head: Head,
        esr: u64,
    }

    /// The record tagged `magic`, if the kernel wrote one.
    unsafe fn record(mc: *const libc::mcontext_t, magic: u32) -> Option<*mut u8> {
        let base = mc.cast::<u8>().cast_mut().wrapping_add(RESERVED);
        let mut at = 0;
        while at + size_of::<Head>() <= RESERVED_SIZE {
            // SAFETY: within the reserved area, checked above.
            let head = unsafe { base.add(at).cast::<Head>().read_unaligned() };
            if head.magic == magic {
                return Some(base.wrapping_add(at));
            }
            if head.magic == 0 || head.size == 0 {
                return None;
            }
            at += head.size as usize;
        }
        None
    }

    pub unsafe fn context(raw: *const c_void) -> Option<Context> {
        // SAFETY: the caller's live `ucontext_t`.
        let uc = unsafe { &*raw.cast::<libc::ucontext_t>() };
        let mc = &uc.uc_mcontext;
        // SAFETY: records the kernel wrote, each read at its own size.
        let (fpsimd, esr) = unsafe {
            (
                record(mc, FPSIMD_MAGIC).map(|at| {
                    let r = at.cast::<FpsimdRecord>().read_unaligned();
                    Fpsimd {
                        v: r.vregs,
                        fpsr: r.fpsr,
                        fpcr: r.fpcr,
                    }
                }),
                record(mc, ESR_MAGIC).map_or(0, |at| at.cast::<EsrRecord>().read_unaligned().esr),
            )
        };
        Some(Context {
            machine: Machine {
                x: mc.regs,
                sp: mc.sp,
                pc: mc.pc,
                pstate: mc.pstate,
                fault_address: mc.fault_address,
                esr,
                fpsimd,
            },
            mask: uc.uc_sigmask,
            stack: uc.uc_stack,
        })
    }

    pub unsafe fn set_context(raw: *mut c_void, context: &Context) {
        // SAFETY: the caller's live `ucontext_t`.
        let uc = unsafe { &mut *raw.cast::<libc::ucontext_t>() };
        let machine = &context.machine;
        uc.uc_mcontext.regs = machine.x;
        uc.uc_mcontext.sp = machine.sp;
        uc.uc_mcontext.pc = machine.pc;
        uc.uc_mcontext.pstate = machine.pstate;
        // FIXME (ACCURACY): with live SVE state the kernel restores the vector
        // registers from the SVE record and ignores these.
        if let Some(fpsimd) = machine.fpsimd
            && let Some(at) = unsafe { record(&uc.uc_mcontext, FPSIMD_MAGIC) }
        {
            // SAFETY: the record found above, written at its own size.
            unsafe {
                let mut r = at.cast::<FpsimdRecord>().read_unaligned();
                r.vregs = fpsimd.v;
                r.fpsr = fpsimd.fpsr;
                r.fpcr = fpsimd.fpcr;
                at.cast::<FpsimdRecord>().write_unaligned(r);
            }
        }
        uc.uc_sigmask = context.mask;
    }

    const _: () = assert!(RESERVED == 288);
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod machine {
    use super::*;

    pub unsafe fn context(_: *const c_void) -> Option<Context> {
        None
    }

    pub unsafe fn set_context(_: *mut c_void, _: &Context) {}
}

pub use machine::{context, set_context};

/// Linux's realtime signals, counted down from the top, where glibc's own
/// two at the bottom are not.
pub fn private_signal(index: u8) -> Option<c_int> {
    let signo = libc::SIGRTMAX() - 2 - c_int::from(index);
    (signo > libc::SIGRTMIN()).then_some(signo)
}

pub fn send_to_thread(tid: i32, signo: c_int) -> std::io::Result<()> {
    // SAFETY: plain integers; `tgkill` names this process so a recycled tid
    // in another one is never signalled.
    let rc = unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, signo) };
    match rc {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}
