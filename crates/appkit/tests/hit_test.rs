use std::ffi::{CStr, c_char, c_int, c_void};
use std::process::Command;

use foundation::geometry::NSRect;
use objc::{object::Id, sel::Sel};

// SDL drivers differ in when they send WINDOW_HIT_TEST. Keep the pointer on
// a draggable title, with no buttons held, and exercise the real event pump.
#[test]
fn hovering_over_a_title_does_not_zoom_the_window() {
    const CHILD_ENV: &str = "ROSEMARY_HIT_TEST_CHILD";
    let library = if cfg!(target_os = "macos") {
        "libSDL3.0.dylib"
    } else {
        "libSDL3.so.0"
    };
    let Ok(driver) = std::env::var(CHILD_ENV) else {
        let dir = std::env::temp_dir().join(format!("rosemary-hit-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("sdl.c");
        std::fs::write(
            &source,
            r#"
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

static uint64_t flags;
static int width, height, hits, zooms, restores, hit_test_installed;
static char window;
bool SDL_InitSubSystem(unsigned subsystems) { return true; }
const char *SDL_GetCurrentVideoDriver(void) { return getenv("ROSEMARY_HIT_TEST_CHILD"); }
void *SDL_CreateWindow(const char *title, int w, int h, uint64_t f) {
    width = w; height = h; flags = f; return &window;
}
unsigned SDL_GetWindowID(void *w) { return 1; }
uint64_t SDL_GetWindowFlags(void *w) { return flags; }
bool SDL_GetWindowSize(void *w, int *x, int *y) { *x = width; *y = height; return true; }
bool SDL_GetWindowSizeInPixels(void *w, int *x, int *y) { return SDL_GetWindowSize(w, x, y); }
const char *SDL_GetWindowTitle(void *w) { return "Hover test"; }
bool SDL_ShowWindow(void *w) { flags &= ~8; return true; }
bool SDL_SetWindowHitTest(void *w, void *callback, void *data) {
    hit_test_installed = callback != NULL; return true;
}
unsigned SDL_GetMouseState(float *x, float *y) { *x = 200; *y = 14; return 0; }
bool SDL_MaximizeWindow(void *w) { ++zooms; flags |= 0x80; return true; }
bool SDL_RestoreWindow(void *w) { ++restores; flags &= ~0x80; return true; }
bool SDL_PollEvent(void *event) {
    if (!hits) return false;
    --hits;
    memset(event, 0, 128);
    uint32_t type = 0x211, id = 1;
    memcpy(event, &type, sizeof(type));
    memcpy((char *)event + 16, &id, sizeof(id));
    return true;
}
void RosemaryTestHitTests(void) { hits = 2; }
int RosemaryTestZooms(void) { return zooms; }
int RosemaryTestRestores(void) { return restores; }
int RosemaryTestHitTestInstalled(void) { return hit_test_installed; }
"#,
        )
        .unwrap();
        let build = Command::new("cc")
            .args(if cfg!(target_os = "macos") {
                vec!["-dynamiclib"]
            } else {
                vec!["-shared", "-fPIC"]
            })
            .arg(&source)
            .arg("-o")
            .arg(dir.join(library))
            .output()
            .expect("C compiler");
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        for driver in ["cocoa", "windows", "wayland", "x11"] {
            let child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "hovering_over_a_title_does_not_zoom_the_window",
                    "--nocapture",
                ])
                .env(CHILD_ENV, driver)
                .env("ROSEMARY_DECORATIONS", "client")
                .env("ROSEMARY_MENU", "none")
                .env("ROSEMARY_WINDOW_SIZE", "400x300")
                .env(
                    if cfg!(target_os = "macos") {
                        "DYLD_LIBRARY_PATH"
                    } else {
                        "LD_LIBRARY_PATH"
                    },
                    &dir,
                )
                .output()
                .unwrap();
            assert!(
                child.status.success(),
                "{driver}: stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&child.stdout),
                String::from_utf8_lossy(&child.stderr)
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
        return;
    };

    appkit::bootstrap();
    // SAFETY: the loaded library is this test's SDL fixture, and each symbol
    // and Objective-C method is called with its declared signature.
    unsafe {
        let library = std::ffi::CString::new(library).unwrap();
        let handle = dlopen(library.as_ptr(), 2);
        assert!(!handle.is_null());
        let hits: unsafe extern "C" fn() =
            std::mem::transmute(symbol(handle, c"RosemaryTestHitTests"));
        let zooms: unsafe extern "C" fn() -> c_int =
            std::mem::transmute(symbol(handle, c"RosemaryTestZooms"));
        let restores: unsafe extern "C" fn() -> c_int =
            std::mem::transmute(symbol(handle, c"RosemaryTestRestores"));
        let installed: unsafe extern "C" fn() -> c_int =
            std::mem::transmute(symbol(handle, c"RosemaryTestHitTestInstalled"));

        let window = objc::object::create_instance(objc::class::lookup(c"NSWindow").unwrap(), 0);
        let sel = objc::sel::get("initWithContentRect:styleMask:backing:defer:");
        let class = objc::object::class_of(window).unwrap();
        let init: unsafe extern "C" fn(Id, Sel, NSRect, usize, usize, u8) -> Id =
            std::mem::transmute(objc::class::method_implementation(class, sel));
        let window = init(
            window,
            sel,
            NSRect::new(0.0, 0.0, 400.0, 300.0),
            1 | 8,
            0,
            0,
        );
        let sel = objc::sel::get("makeKeyAndOrderFront:");
        let show: unsafe extern "C" fn(Id, Sel, Id) =
            std::mem::transmute(objc::class::method_implementation(class, sel));
        show(window, sel, std::ptr::null_mut());
        appkit::event::pump(std::ptr::null_mut());
        assert_eq!(installed(), 1, "the window has a draggable titlebar");

        hits();
        appkit::event::pump(std::ptr::null_mut());
        if driver == "x11" {
            // X11 emits this event only on left-button down, even though SDL
            // consumes the press before updating its cached button state.
            assert_eq!(zooms(), 1, "two X11 titlebar presses still zoom");
            hits();
            appkit::event::pump(std::ptr::null_mut());
            assert_eq!(restores(), 1, "another double click restores");
        } else {
            assert_eq!(zooms(), 0, "hover hit tests must not become double clicks");
            assert_eq!(restores(), 0);
        }
        objc::arc::release(window);
    }
}

unsafe extern "C" {
    fn dlopen(name: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}

unsafe fn symbol(handle: *mut c_void, name: &CStr) -> *mut c_void {
    let found = unsafe { dlsym(handle, name.as_ptr()) };
    assert!(!found.is_null(), "missing symbol {name:?}");
    found
}
