use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{Callbacks, GuestMemory, Result};

use super::coregraphics::CgPoint;
use super::host::host_or_refuse;

/// `CMTime`, laid out the way both architectures lay it out.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CmTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

/// Bytes long, and the reason this module exists: over sixteen.
const CM_TIME_SIZE: usize = 24;

impl CmTime {
    fn from_le_bytes(bytes: &[u8]) -> Self {
        let word = |at: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&bytes[at..at + 8]);
            u64::from_le_bytes(w)
        };
        let middle = word(8);
        CmTime {
            value: word(0) as i64,
            timescale: middle as u32 as i32,
            flags: (middle >> 32) as u32,
            epoch: word(16) as i64,
        }
    }

    fn to_le_bytes(self) -> [u8; CM_TIME_SIZE] {
        let mut out = [0u8; CM_TIME_SIZE];
        out[0..8].copy_from_slice(&self.value.to_le_bytes());
        out[8..12].copy_from_slice(&self.timescale.to_le_bytes());
        out[12..16].copy_from_slice(&self.flags.to_le_bytes());
        out[16..24].copy_from_slice(&self.epoch.to_le_bytes());
        out
    }
}

/// Read the `CMTime` the guest left `offset` bytes into its argument area.
fn time_argument(
    symbol: &str,
    ctx: &RegisterContext,
    mem: &mut dyn GuestMemory,
    offset: u64,
) -> Result<CmTime> {
    let bytes = mem.read_bytes(ctx.rsp.wrapping_add(offset), CM_TIME_SIZE, symbol)?;
    Ok(CmTime::from_le_bytes(&bytes))
}

/// Place a `CMTime` the way a SysV AMD64 MEMORY return is placed.
///
/// The buffer is the hidden first argument the guest passed in `rdi`, and `rax`
/// gets it back - which is what a MEMORY-returning callee must do and what a
/// caller reading the result through `rax` depends on.
fn return_time(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    time: CmTime,
) -> Result<()> {
    let buffer = ctx.rdi;
    if buffer == 0 {
        return Err(super::bad_pointer(symbol, 0));
    }
    mem.write_bytes(buffer, &time.to_le_bytes(), symbol)?;
    ctx.rax = buffer;
    Ok(())
}

/// `CMTime CMTimeMake(int64_t value, int32_t timescale)`
///
/// The hidden return buffer takes `rdi`, so `value` is in `rsi` and `timescale`
/// in `edx` - one register along from where a reader counting parameters would
/// look for them.
fn time_make(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    // SAFETY: the symbol [`shim_for`] matched, which really is
    // `CMTime (*)(int64_t, int32_t)`, resolved out of a CoreMedia this process
    // has loaded. Declaring the aggregate return leaves the `x8` placement to
    // the compiler.
    let f: extern "C" fn(i64, i32) -> CmTime =
        unsafe { core::mem::transmute::<u64, extern "C" fn(i64, i32) -> CmTime>(host) };
    let time = f(ctx.rsi as i64, ctx.rdx as i32);
    return_time(symbol, ctx, mem, time)
}

/// `Float64 CMTimeGetSeconds(CMTime time)`
///
/// The time is the only argument and is entirely on the guest's stack, so no integer
/// register is involved at all. **Forwarded rather than divided**.
fn time_get_seconds(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    let time = time_argument(symbol, ctx, mem, 0)?;
    // SAFETY: the symbol [`shim_for`] matched, which is `Float64 (*)(CMTime)`.
    let f: extern "C" fn(CmTime) -> f64 =
        unsafe { core::mem::transmute::<u64, extern "C" fn(CmTime) -> f64>(host) };
    ctx.xmm[0] = [f(time).to_bits(), 0];
    Ok(())
}

/// `CMTime CMSampleBufferGetPresentationTimeStamp(CMSampleBufferRef sbuf)`
///
/// A MEMORY return in front of one handle: the buffer is `rdi` and the sample
/// buffer `rsi`.
fn presentation_time_stamp(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    // SAFETY: the symbol [`shim_for`] matched, which is
    // `CMTime (*)(CMSampleBufferRef)`. The sample buffer is an opaque handle
    // CoreMedia made.
    let f: extern "C" fn(u64) -> CmTime =
        unsafe { core::mem::transmute::<u64, extern "C" fn(u64) -> CmTime>(host) };
    let time = f(ctx.rsi);
    return_time(symbol, ctx, mem, time)
}

/// `OSStatus VTCompressionSessionCompleteFrames(VTCompressionSessionRef session,
/// CMTime completeUntilPresentationTimeStamp)`
///
/// The session keeps `rdi` - a MEMORY argument consumes no register - and the
/// time is the whole of the stack argument area.
fn complete_frames(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    let until = time_argument(symbol, ctx, mem, 0)?;
    // SAFETY: the symbol [`shim_for`] matched, which is
    // `OSStatus (*)(VTCompressionSessionRef, CMTime)`.
    let f: extern "C" fn(u64, CmTime) -> i32 =
        unsafe { core::mem::transmute::<u64, extern "C" fn(u64, CmTime) -> i32>(host) };
    ctx.rax = f(ctx.rdi, until) as u32 as u64;
    Ok(())
}

/// `OSStatus VTCompressionSessionEncodeFrame(VTCompressionSessionRef session,
/// CVImageBufferRef imageBuffer, CMTime presentationTimeStamp, CMTime duration,
/// CFDictionaryRef frameProperties, void *sourceFrameRefcon, VTEncodeInfoFlags
/// *infoFlagsOut)`
///
/// Seven parameters, two of them twenty-four bytes on the stack, and the reason the
/// register mapping is written out here rather than left to a reader:
///
/// `sourceFrameRefcon` is an opaque token VideoToolbox hands straight back to the
/// output callback and never dereferences, so it crosses as it is.
fn encode_frame(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    let pts = time_argument(symbol, ctx, mem, 0)?;
    let duration = time_argument(symbol, ctx, mem, CM_TIME_SIZE as u64)?;
    // SAFETY: the symbol [`shim_for`] matched, whose declaration is the one
    // above. The session and image buffer are handles VideoToolbox and
    // CoreVideo made; the output callback was substituted for a thunk when the
    // session was created.
    let f: extern "C" fn(u64, u64, CmTime, CmTime, u64, u64, u64) -> i32 = unsafe {
        core::mem::transmute::<u64, extern "C" fn(u64, u64, CmTime, CmTime, u64, u64, u64) -> i32>(
            host,
        )
    };
    ctx.rax = f(ctx.rdi, ctx.rsi, pts, duration, ctx.rdx, ctx.rcx, ctx.r8) as u32 as u64;
    Ok(())
}

/// `CGSize CVImageBufferGetEncodedSize(CVImageBufferRef imageBuffer)`
fn encoded_size(symbol: &str, ctx: &mut RegisterContext, _mem: &mut dyn GuestMemory) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    // SAFETY: the symbol [`shim_for`] matched, which is
    // `CGSize (*)(CVImageBufferRef)`, resolved out of a CoreVideo this process
    // has loaded.
    let f: extern "C" fn(u64) -> CgPoint =
        unsafe { core::mem::transmute::<u64, extern "C" fn(u64) -> CgPoint>(host) };
    let size = f(ctx.rdi);
    ctx.xmm[0] = [size.x.to_bits(), 0];
    ctx.xmm[1] = [size.y.to_bits(), 0];
    Ok(())
}

/// `CAFrameRateRange`, as AAPCS64 returns one: three `float`s, `s0`–`s2`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CaFrameRateRange {
    minimum: f32,
    maximum: f32,
    preferred: f32,
}

/// `CAFrameRateRange CAFrameRateRangeMake(float minimum, float maximum, float
/// preferred)`
fn frame_rate_range_make(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    let arg = |i: usize| f32::from_bits(ctx.xmm[i][0] as u32);
    // SAFETY: the symbol [`shim_for`] matched, which is
    // `CAFrameRateRange (*)(float, float, float)`, resolved out of a QuartzCore
    // this process has loaded.
    let f: extern "C" fn(f32, f32, f32) -> CaFrameRateRange = unsafe {
        core::mem::transmute::<u64, extern "C" fn(f32, f32, f32) -> CaFrameRateRange>(host)
    };
    let range = f(arg(0), arg(1), arg(2));
    ctx.xmm[0] = [
        u64::from(range.minimum.to_bits()) | (u64::from(range.maximum.to_bits()) << 32),
        0,
    ];
    ctx.xmm[1] = [u64::from(range.preferred.to_bits()), 0];
    Ok(())
}

/// `VTDecompressionOutputCallbackRecord` - a function pointer and its refcon.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct OutputCallbackRecord {
    callback: u64,
    refcon: u64,
}

/// `OSStatus VTDecompressionSessionCreate(CFAllocatorRef allocator,
/// CMVideoFormatDescriptionRef videoFormatDescription, CFDictionaryRef
/// videoDecoderSpecification, CFDictionaryRef destinationImageBufferAttributes, const
/// VTDecompressionOutputCallbackRecord *outputCallback, VTDecompressionSessionRef
/// *decompressionSessionOut)`
///
/// The compression side takes its callback as an ordinary argument and is a `cbsig!`
/// entry; this one takes it *inside a two-word record*, which the marshaller's
/// per-argument substitution cannot see into - it would replace the **record pointer**
/// with a thunk and VideoToolbox would read a trampoline's first eight bytes as the
/// callback.
///
/// So the record is read out of guest memory, its callback replaced with a thunk, and
/// the host handed a copy that lives here rather than in the guest's address space.
fn decompression_session_create(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    let record_va = ctx.r8;
    let mut record = OutputCallbackRecord::default();
    if record_va != 0 {
        let bytes = mem.read_bytes(record_va, 16, symbol)?;
        let word = |at: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&bytes[at..at + 8]);
            u64::from_le_bytes(w)
        };
        record.callback = match word(0) {
            0 => 0,
            guest => callbacks.substitute(symbol, guest)?,
        };
        record.refcon = word(8);
    }
    let record_ptr = match record_va {
        0 => core::ptr::null(),
        _ => &record as *const OutputCallbackRecord,
    };
    // SAFETY: the symbol [`shim_for`] matched, whose declaration is the one
    // above. The record's callback is a native thunk by the lines above, and the
    // record itself outlives the call - which is all VideoToolbox reads it for.
    let f: extern "C" fn(u64, u64, u64, u64, *const OutputCallbackRecord, u64) -> i32 = unsafe {
        core::mem::transmute::<
            u64,
            extern "C" fn(u64, u64, u64, u64, *const OutputCallbackRecord, u64) -> i32,
        >(host)
    };
    ctx.rax = f(ctx.rdi, ctx.rsi, ctx.rdx, ctx.rcx, record_ptr, ctx.r9) as u32 as u64;
    Ok(())
}

/// One of this module's implementations that needs no [`Callbacks`].
pub type Shim = super::coregraphics::Shim;

/// Whether `name` (underscore already stripped) is one of this module's plain
/// shims.
pub fn shim_for(name: &str) -> Option<Shim> {
    Some(match name {
        "CMTimeMake" => time_make,
        "CMTimeGetSeconds" => time_get_seconds,
        "CMSampleBufferGetPresentationTimeStamp" => presentation_time_stamp,
        "VTCompressionSessionCompleteFrames" => complete_frames,
        "VTCompressionSessionEncodeFrame" => encode_frame,
        "CVImageBufferGetEncodedSize" => encoded_size,
        "CAFrameRateRangeMake" => frame_rate_range_make,
        _ => return None,
    })
}

/// Whether `name` is this module's one callback-carrying shim.
pub fn callback_shim_for(name: &str) -> Option<crate::signal::Shim> {
    match name {
        "VTDecompressionSessionCreate" => Some(decompression_session_create),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout claim every function here rests on, which a wrong answer would
    /// violate silently.
    #[test]
    fn cm_time_is_the_layout_both_architectures_use() {
        assert_eq!(core::mem::size_of::<CmTime>(), CM_TIME_SIZE);
        let time = CmTime {
            value: 0x0102_0304_0506_0708,
            timescale: 600,
            flags: 1,
            epoch: -1,
        };
        let bytes = time.to_le_bytes();
        assert_eq!(&bytes[0..8], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&bytes[8..12], &600i32.to_le_bytes());
        assert_eq!(&bytes[12..16], &1u32.to_le_bytes());
        assert_eq!(&bytes[16..24], &(-1i64).to_le_bytes());
        let back = CmTime::from_le_bytes(&bytes);
        assert_eq!(back.value, time.value);
        assert_eq!(back.timescale, time.timescale);
        assert_eq!(back.flags, time.flags);
        assert_eq!(back.epoch, time.epoch);
    }

    /// Twelve bytes and three members, which is what makes the SysV return
    /// packing - two members in `xmm0`, one in `xmm1` - the right arrangement.
    #[test]
    fn a_frame_rate_range_is_three_floats() {
        assert_eq!(core::mem::size_of::<CaFrameRateRange>(), 12);
    }
}
