mod common;

// The guest runs on its process's main thread, as SDL's Cocoa driver requires.
const SOURCE: &str = r#"
#import <AppKit/AppKit.h>
#import <Metal/Metal.h>
#import <QuartzCore/CAMetalLayer.h>
#include <stdio.h>

#define CHECK(value) do { if (!(value)) { fprintf(stderr, "line %d: %s\n", __LINE__, #value); return 1; } } while (0)

int main(void) {
    [NSApplication sharedApplication];
    id<MTLDevice> device = MTLCreateSystemDefaultDevice();
    CHECK(device);
    NSError *error = nil;
    id<MTLLibrary> library = [device newLibraryWithSource:
        @"kernel void clear(device uint* output [[buffer(0)]]) { output[0] = 42; }"
        options:nil error:&error];
    CHECK(library && !error);
    id<MTLFunction> function = [library newFunctionWithName:@"clear"];
    id<MTLComputePipelineState> pipeline = [device newComputePipelineStateWithFunction:function error:&error];
    CHECK(pipeline && !error);
    id<MTLBuffer> buffer = [device newBufferWithLength:16 options:MTLResourceStorageModeShared];
    id<MTLCommandQueue> queue = [device newCommandQueue];
    CHECK(buffer && queue);
    id<MTLCommandBuffer> command = [queue commandBuffer];
    id<MTLComputeCommandEncoder> compute = [command computeCommandEncoder];
    [compute setComputePipelineState:pipeline];
    [compute setBuffer:buffer offset:0 atIndex:0];
    [compute dispatchThreadgroups:MTLSizeMake(1, 1, 1) threadsPerThreadgroup:MTLSizeMake(1, 1, 1)];
    [compute endEncoding];
    [command commit];
    [command waitUntilCompleted];
    CHECK(command.status == MTLCommandBufferStatusCompleted);
    CHECK(*(unsigned int *)buffer.contents == 42);

    NSRect rect = NSMakeRect(0, 0, 320, 240);
    NSWindow *window = [[NSWindow alloc] initWithContentRect:rect
        styleMask:NSWindowStyleMaskTitled backing:NSBackingStoreBuffered defer:NO];
    NSView *view = [[NSView alloc] initWithFrame:rect];
    [window setContentView:view];
    CAMetalLayer *layer = [CAMetalLayer layer];
    layer.device = device;
    layer.pixelFormat = MTLPixelFormatBGRA8Unorm;
    layer.drawableSize = CGSizeMake(320, 240);
    [view setLayer:layer];
    [view setWantsLayer:YES];
    [window orderFront:nil];
    for (int frame = 0; frame < 12; ++frame) {
        id<CAMetalDrawable> drawable = [layer nextDrawable];
        CHECK(drawable);
        MTLRenderPassDescriptor *pass = [MTLRenderPassDescriptor renderPassDescriptor];
        pass.colorAttachments[0].texture = drawable.texture;
        pass.colorAttachments[0].loadAction = MTLLoadActionClear;
        pass.colorAttachments[0].storeAction = MTLStoreActionStore;
        pass.colorAttachments[0].clearColor = MTLClearColorMake(0.25, 0.5, 0.75, 1.0);
        command = [queue commandBuffer];
        id<MTLRenderCommandEncoder> render = [command renderCommandEncoderWithDescriptor:pass];
        CHECK(render);
        [render endEncoding];
        [command presentDrawable:drawable];
        [command commit];
        [command waitUntilCompleted];
        CHECK(command.status == MTLCommandBufferStatusCompleted);
    }
    [window close];
    puts("translated compute and presentation verified");
    return 0;
}
"#;

fn fixture() -> String {
    static FIXTURE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    FIXTURE
        .get_or_init(|| {
            let host_arch = if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "x86_64"
            };
            common::build_x86_64(
                "builtin_metal",
                SOURCE,
                &[
                    "-x",
                    "objective-c",
                    "-framework",
                    "AppKit",
                    "-framework",
                    "Metal",
                    "-framework",
                    "QuartzCore",
                    "-arch",
                    host_arch,
                ],
            )
            .expect("compile the Metal fixture")
        })
        .clone()
}

fn verify(engine: &str) {
    if std::env::var_os("ROSEMARY_TEST_VULKAN").is_none() {
        skip!("set ROSEMARY_TEST_VULKAN=1 with a GPU and graphical session")
    }
    if !common::can_build_x86_64() {
        skip!("cannot build Mach-O Metal fixture")
    }
    let path = fixture();
    let output = common::driver_command()
        .args(["run", engine, &path])
        .env("ROSEMARY_OBJC", "builtin")
        .env("ROSEMARY_BUNDLED", "1")
        .env("ROSEMARY_PREFIX", "off")
        .env("ROSEMARY_TRACE_METAL", "1")
        .output()
        .expect("run the Metal fixture");
    let out = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{engine}: stdout: {out}\nstderr: {err}"
    );
    assert!(
        out.contains("translated compute and presentation verified"),
        "{out}\n{err}"
    );
    assert!(
        err.contains("first window swapchain"),
        "no swapchain: {err}"
    );
    assert!(!err.contains("presenting through a readback"), "{err}");
    assert!(!err.contains("swapchain presentation failed"), "{err}");
}

#[test]
fn an_emulated_guest_runs_translated_compute_and_presents() {
    verify("--emulated");
}

#[cfg(target_os = "macos")]
#[test]
fn a_native_guest_runs_translated_compute_and_presents() {
    verify("--native");
}
