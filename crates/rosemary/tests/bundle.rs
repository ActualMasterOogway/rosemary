mod common;

/// Apple's CallMachOFramework pattern, which Feral's ports use to reach `free`:
/// find the frameworks folder on the "appropriate disk", open
/// `System.framework` in it as a bundle, and look a function up by name.
const SYSTEM_FRAMEWORK_C: &str = r#"
#include <CoreServices/CoreServices.h>
#include <stdio.h>
#include <stdlib.h>

typedef void (*free_fn)(void *);

int main(void) {
    FSRef folder;
    OSErr err = FSFindFolder(kOnAppropriateDisk, kFrameworksFolderType, true, &folder);
    printf("find %d\n", err);
    if (err) return 1;
    CFURLRef base = CFURLCreateFromFSRef(NULL, &folder);
    CFURLRef url = CFURLCreateCopyAppendingPathComponent(
        NULL, base, CFSTR("System.framework"), false);
    CFBundleRef bundle = CFBundleCreate(NULL, url);
    printf("bundle %d\n", bundle != NULL);
    if (!bundle) return 1;
    printf("loaded %d\n", CFBundleLoadExecutable(bundle));
    free_fn f = (free_fn)CFBundleGetFunctionPointerForName(bundle, CFSTR("free"));
    printf("free %d\n", f != NULL);
    if (f) f(malloc(16));
    printf("missing %d\n",
        CFBundleGetFunctionPointerForName(bundle, CFSTR("no_such_function_here")) != NULL);
    CFRelease(bundle);
    CFRelease(url);
    CFRelease(base);
    return 0;
}
"#;

const FLAGS: &[&str] = &[
    "-Wno-deprecated-declarations",
    "-framework",
    "CoreServices",
    "-framework",
    "CoreFoundation",
];

#[test]
fn a_function_is_found_in_system_framework_by_name() {
    let Some(guest) = common::build_x86_64("system_framework", SYSTEM_FRAMEWORK_C, FLAGS) else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = common::run_guest(&guest, &[]);
    assert_eq!(status, Some(0), "stdout: {out}\nstderr: {err}");
    assert_eq!(
        out, "find 0\nbundle 1\nloaded 1\nfree 1\nmissing 0\n",
        "stderr: {err}"
    );
}

#[test]
fn the_system_framework_lookup_agrees_with_the_host() {
    let Some(guest) = common::build_x86_64("system_framework", SYSTEM_FRAMEWORK_C, FLAGS) else {
        skip!("cannot build x86_64 fixtures")
    };
    let Some(native) =
        common::build_native_with("system_framework_native", SYSTEM_FRAMEWORK_C, FLAGS)
    else {
        skip!("cannot build native fixtures")
    };
    let (_, guest_out, err) = common::run_guest(&guest, &[]);
    let Some((_, native_out)) = common::run_native(&native) else {
        skip!("the native fixture would not run")
    };
    assert_eq!(guest_out, native_out, "stderr: {err}");
}
