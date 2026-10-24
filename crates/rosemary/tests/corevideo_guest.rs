mod common;

use common::{build_native_with, build_x86_64, run_guest_env, run_native};

const PIXEL_BUFFER_GUEST: &str = r#"
#include <CoreFoundation/CoreFoundation.h>
#include <CoreVideo/CoreVideo.h>
#include <stdint.h>
#include <stdio.h>

#define CHECK(c) do { if (!(c)) { printf("corevideo check failed: %s\n", #c); return 1; } } while (0)

int main(void) {
    CVPixelBufferRef packed = NULL;
    CHECK(CVPixelBufferCreate(NULL, 7, 5, kCVPixelFormatType_32BGRA,
                              NULL, &packed) == kCVReturnSuccess);
    CHECK(packed != NULL);
    CHECK(CFGetTypeID(packed) == CVPixelBufferGetTypeID());
    CHECK(CVPixelBufferGetWidth(packed) == 7);
    CHECK(CVPixelBufferGetHeight(packed) == 5);
    CHECK(CVPixelBufferGetPixelFormatType(packed) == kCVPixelFormatType_32BGRA);
    CHECK(CVPixelBufferIsPlanar(packed) == 0);
    CHECK(CVPixelBufferGetPlaneCount(packed) == 0);
    CHECK(CVPixelBufferGetBytesPerRow(packed) >= 28);
    CHECK(CVPixelBufferLockBaseAddress(packed, 0) == kCVReturnSuccess);
    uint8_t *base = CVPixelBufferGetBaseAddress(packed);
    CHECK(base != NULL);
    base[0] = 0x35;
    base[CVPixelBufferGetBytesPerRow(packed) * 4 + 27] = 0x79;
    CHECK(CVPixelBufferUnlockBaseAddress(packed, 0) == kCVReturnSuccess);
    CHECK(CVPixelBufferLockBaseAddress(packed, kCVPixelBufferLock_ReadOnly) == kCVReturnSuccess);
    CHECK(((uint8_t *)CVPixelBufferGetBaseAddress(packed))[0] == 0x35);
    CHECK(((uint8_t *)CVPixelBufferGetBaseAddress(packed))[CVPixelBufferGetBytesPerRow(packed) * 4 + 27] == 0x79);
    CHECK(CVPixelBufferUnlockBaseAddress(packed, kCVPixelBufferLock_ReadOnly) == kCVReturnSuccess);
    CVPixelBufferRef retained = CVPixelBufferRetain(packed);
    CHECK(retained == packed);
    CVPixelBufferRelease(packed);
    CHECK(CVPixelBufferGetWidth(retained) == 7);
    CVBufferRelease(retained);

#ifdef ROSEMARY_GUEST
    // The CPU implementation must reject capabilities it cannot provide.
    const void *keys[] = { kCVPixelBufferIOSurfaceCoreAnimationCompatibilityKey };
    const void *values[] = { kCFBooleanTrue };
    CFDictionaryRef options = CFDictionaryCreate(NULL, keys, values, 1,
        &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CHECK(options != NULL);
    CVPixelBufferRef unsupported = NULL;
    CHECK(CVPixelBufferCreate(NULL, 7, 5, kCVPixelFormatType_32BGRA,
                              options, &unsupported) == kCVReturnUnsupported);
    CHECK(unsupported == NULL);
    CFRelease(options);
#endif

    CVPixelBufferRef planar = NULL;
    CHECK(CVPixelBufferCreate(NULL, 7, 5, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                              NULL, &planar) == kCVReturnSuccess);
    CHECK(CVPixelBufferIsPlanar(planar) != 0);
    CHECK(CVPixelBufferGetPlaneCount(planar) == 2);
    CHECK(CVPixelBufferGetWidthOfPlane(planar, 1) == 4);
    CHECK(CVPixelBufferGetHeightOfPlane(planar, 1) == 3);
    CHECK(CVPixelBufferLockBaseAddress(planar, 0) == kCVReturnSuccess);
    uint8_t *uv = CVPixelBufferGetBaseAddressOfPlane(planar, 1);
    CHECK(uv != NULL);
    CHECK(CVPixelBufferGetBytesPerRowOfPlane(planar, 1) >= 8);
    uv[0] = 0x41;
    CHECK(CVPixelBufferUnlockBaseAddress(planar, 0) == kCVReturnSuccess);
    CHECK(CVPixelBufferLockBaseAddress(planar, kCVPixelBufferLock_ReadOnly) == kCVReturnSuccess);
    CHECK(((uint8_t *)CVPixelBufferGetBaseAddressOfPlane(planar, 1))[0] == 0x41);
    CHECK(CVPixelBufferUnlockBaseAddress(planar, kCVPixelBufferLock_ReadOnly) == kCVReturnSuccess);
    CFRetain(planar);
    CVPixelBufferRelease(planar);
    CHECK(CVPixelBufferGetPlaneCount(planar) == 2);
    CFRelease(planar);
    puts("corevideo pixel buffer abi ok");
    return 0;
}
"#;

#[test]
fn emulated_x86_64_corevideo_pixel_buffers_cross_the_guest_abi() {
    let Some(path) = build_x86_64(
        "corevideo_pixelbuffer",
        PIXEL_BUFFER_GUEST,
        &[
            "-DROSEMARY_GUEST",
            "-framework",
            "CoreVideo",
            "-framework",
            "CoreFoundation",
        ],
    ) else {
        skip!("cannot build x86_64 Mach-O CoreVideo fixture");
    };
    let (status, out, err) = run_guest_env(&[("ROSEMARY_OBJC", "builtin")], &path, &[]);
    assert_eq!(status, Some(0), "stdout: {out}\nstderr: {err}");
    assert_eq!(out, "corevideo pixel buffer abi ok\n", "stderr: {err}");
}

#[test]
fn native_macos_corevideo_fixture_obeys_the_same_contract() {
    let Some(path) = build_native_with(
        "corevideo_pixelbuffer_native",
        PIXEL_BUFFER_GUEST,
        &["-framework", "CoreVideo", "-framework", "CoreFoundation"],
    ) else {
        skip!("cannot build a native macOS CoreVideo fixture");
    };
    let Some((status, out)) = run_native(&path) else {
        skip!("native Mach-O execution unavailable");
    };
    assert_eq!(status, Some(0), "stdout: {out}");
    assert_eq!(out, "corevideo pixel buffer abi ok\n");
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn native_aarch64_rosemary_corevideo_pixel_buffers_cross_the_guest_abi() {
    let Some(path) = build_native_with(
        "corevideo_pixelbuffer_arm64_rosemary",
        PIXEL_BUFFER_GUEST,
        &[
            "-DROSEMARY_GUEST",
            "-framework",
            "CoreVideo",
            "-framework",
            "CoreFoundation",
        ],
    ) else {
        skip!("cannot build native arm64 Mach-O CoreVideo fixture");
    };
    let output = common::driver_command()
        .env("ROSEMARY_OBJC", "builtin")
        .arg("run")
        .arg("--native")
        .arg(&path)
        .output()
        .expect("launch Rosemary native guest");
    let out = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {out}\nstderr: {err}"
    );
    assert_eq!(out, "corevideo pixel buffer abi ok\n", "stderr: {err}");
}

#[test]
fn x86_64_guest_registers_marshal_pixel_buffer_create_and_boolean_returns() {
    use machish::memory::{AddressSpace, BufferSpace, Placement};
    use microlift::arch::x86_64::context::RegisterContext;
    use rosemary::abi::{GuestMemory, marshal, table};

    const STACK: u64 = 0x2_3000_0000;
    let mut mem = BufferSpace::new(STACK);
    mem.reserve(4096, 16, Placement::Fixed(STACK))
        .expect("test guest stack");
    AddressSpace::write(&mut mem, STACK, &[0; 16]).expect("initialize stack");

    let abi = match table::lookup(0, "CVPixelBufferCreate") {
        table::Lookup::Known(abi) => abi,
        _ => panic!("missing CVPixelBufferCreate ABI declaration"),
    };
    let mut buffer: corefoundation::base::CFTypeRef = std::ptr::null();
    let mut regs = RegisterContext::new();
    regs.rsp = STACK;
    regs.rdi = 0;
    regs.rsi = 7;
    regs.rdx = 5;
    regs.rcx = u64::from(u32::from_be_bytes(*b"420v"));
    regs.r8 = 0;
    regs.r9 = (&mut buffer as *mut corefoundation::base::CFTypeRef) as u64;
    marshal::call(
        "CVPixelBufferCreate",
        abi,
        corevideo::pixel_buffer::CVPixelBufferCreate as *const () as usize as u64,
        &mut regs,
        &mem as &dyn GuestMemory,
    )
    .expect("ABI call succeeds");
    assert_eq!(regs.rax as u32 as i32, 0);
    assert!(!buffer.is_null());

    let bool_abi = match table::lookup(0, "CVPixelBufferIsPlanar") {
        table::Lookup::Known(abi) => abi,
        _ => panic!("missing CVPixelBufferIsPlanar ABI declaration"),
    };
    regs = RegisterContext::new();
    regs.rsp = STACK;
    regs.rdi = buffer as u64;
    marshal::call(
        "CVPixelBufferIsPlanar",
        bool_abi,
        corevideo::pixel_buffer::CVPixelBufferIsPlanar as *const () as usize as u64,
        &mut regs,
        &mem as &dyn GuestMemory,
    )
    .expect("Boolean ABI call succeeds");
    assert_eq!(regs.rax & 0xff, 1);
    unsafe { corevideo::pixel_buffer::CVPixelBufferRelease(buffer) };
}
