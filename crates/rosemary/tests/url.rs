#![cfg(target_os = "macos")]

mod common;

const URL_APIS: &str = r#"
#import <Foundation/Foundation.h>
#include <stdio.h>
#include <string.h>

#define CHECK(condition) do { if (!(condition)) { printf("failed at line %d\n", __LINE__); return 1; } } while (0)

int main(int argc, char **argv) {
    @autoreleasepool {
        CHECK(argc == 2);
        NSString *path = [NSString stringWithUTF8String:argv[1]];
        NSURL *file = [[NSURL alloc] initFileURLWithPath:path];
        CHECK(file.hasDirectoryPath);
        NSError *sentinel = [NSError errorWithDomain:@"sentinel" code:42 userInfo:nil];
        NSError *error = sentinel;
        CHECK([file checkResourceIsReachableAndReturnError:&error]);
        CHECK(error == sentinel);
        CHECK([file checkResourceIsReachableAndReturnError:NULL]);
        NSURL *missing = [file URLByAppendingPathComponent:@"missing"];
        CHECK(![missing checkResourceIsReachableAndReturnError:&error]);
        CHECK([error.domain isEqualToString:NSCocoaErrorDomain]);
        CHECK(error.code == 260);
        CHECK([[error.userInfo objectForKey:NSFilePathErrorKey] isEqualToString:missing.path]);
        CHECK([[[error.userInfo objectForKey:NSURLErrorKey] absoluteString] isEqualToString:missing.absoluteString]);
        CHECK([[error.userInfo objectForKey:NSUnderlyingErrorKey] code] == 2);

        NSURL *url = [[NSURL alloc] initWithString:@"https://user%20name:p%40ss@[::1]:8080/a%20b/c.tar.gz/?x%20y=z#f%20g"];
        CHECK([url.path isEqualToString:@"/a b/c.tar.gz"]);
        CHECK([url.lastPathComponent isEqualToString:@"c.tar.gz"]);
        CHECK([url.pathExtension isEqualToString:@"gz"]);
        CHECK(url.pathComponents.count == 3);
        CHECK([[url.pathComponents objectAtIndex:1] isEqualToString:@"a b"]);
        CHECK([url.user isEqualToString:@"user name"]);
        CHECK([url.password isEqualToString:@"p%40ss"]);
        CHECK(url.port.integerValue == 8080);
        CHECK([url.query isEqualToString:@"x%20y=z"]);
        CHECK([url.fragment isEqualToString:@"f%20g"]);
        CHECK(url.absoluteURL == url);
        CHECK([url.URLByDeletingPathExtension.absoluteString isEqualToString:@"https://user%20name:p%40ss@[::1]:8080/a%20b/c.tar/?x%20y=z#f%20g"]);
        error = nil;
        CHECK(![url checkResourceIsReachableAndReturnError:&error]);
        CHECK(error.code == 262);

        NSURL *directory = [[NSURL alloc] initFileURLWithPath:@"/tmp/a #b" isDirectory:YES];
        CHECK(directory.hasDirectoryPath);
        char buffer[11];
        CHECK(![directory getFileSystemRepresentation:buffer maxLength:10]);
        CHECK([directory getFileSystemRepresentation:buffer maxLength:sizeof(buffer)]);
        CHECK(strcmp(buffer, "/tmp/a #b") == 0);
        CHECK([[NSURL URLWithString:@"foo/bar.txt"].resourceSpecifier isEqualToString:@"foo/bar.txt"]);
        [directory release];
        [url release];
        [file release];
        puts("url APIs ok");
    }
    return 0;
}
"#;

#[test]
fn builtin_url_methods_match_foundation_in_native_and_emulated_guests() {
    let Some(native) = common::build_native_with(
        "url_apis_native",
        URL_APIS,
        &["-x", "objective-c", "-framework", "Foundation"],
    ) else {
        skip!("cannot build native Foundation fixture")
    };
    let Some(emulated) = common::build_objc("url_apis_emulated", URL_APIS) else {
        skip!("cannot build x86_64 Foundation fixture")
    };
    let dir = std::env::temp_dir().join(format!("rosemary-url-guest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let host_path = dir.to_str().unwrap();
    let (status, expected) = common::run_native_with(&native, &[host_path]).unwrap();
    assert_eq!(status, Some(0), "{expected}");
    assert_eq!(expected, "url APIs ok\n");
    let guest_path = format!("/Volumes/Host{host_path}");
    for (backend, executable) in [("--native", native), ("--emulated", emulated)] {
        let output = common::driver_command()
            .env("ROSEMARY_OBJC", "builtin")
            .arg("run")
            .arg(backend)
            .arg(executable)
            .arg(&guest_path)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{backend}: {stdout}\n{stderr}"
        );
        assert_eq!(stdout, expected, "{backend}: {stderr}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}
