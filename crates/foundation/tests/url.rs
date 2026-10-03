use objc::{object::Id, sel::Sel};

struct Pool(*mut std::ffi::c_void);

impl Pool {
    fn new() -> Self {
        foundation::bootstrap();
        Self(objc::arc::pool_push())
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        objc::arc::pool_pop(self.0);
    }
}

unsafe fn implementation<T: Copy>(receiver: Id, selector: &str) -> (T, Sel) {
    let sel = objc::sel::get(selector);
    let class = unsafe { objc::object::class_of(receiver) }.expect("a live object");
    let method = class
        .method(sel)
        .unwrap_or_else(|| panic!("missing {selector}"));
    let imp = method.implementation();
    assert_eq!(std::mem::size_of::<T>(), std::mem::size_of_val(&imp));
    // SAFETY: callers specify the documented signature for this selector.
    (unsafe { std::mem::transmute_copy(&imp) }, sel)
}

unsafe fn object(receiver: Id, selector: &str) -> Id {
    let (call, sel) =
        unsafe { implementation::<unsafe extern "C" fn(Id, Sel) -> Id>(receiver, selector) };
    unsafe { call(receiver, sel) }
}

unsafe fn string(receiver: Id, selector: &str) -> String {
    String::from_utf8(unsafe { foundation::string::bytes(object(receiver, selector)) }).unwrap()
}

unsafe fn integer(receiver: Id, selector: &str) -> i64 {
    let (call, sel) =
        unsafe { implementation::<unsafe extern "C" fn(Id, Sel) -> i64>(receiver, selector) };
    unsafe { call(receiver, sel) }
}

unsafe fn reachable(receiver: Id, error: *mut Id) -> u8 {
    let (call, sel) = unsafe {
        implementation::<unsafe extern "C" fn(Id, Sel, *mut Id) -> u8>(
            receiver,
            "checkResourceIsReachableAndReturnError:",
        )
    };
    unsafe { call(receiver, sel, error) }
}

unsafe fn info_value(error: Id, key: &str) -> Id {
    let info = unsafe { object(error, "userInfo") };
    let key = foundation::string::from_str(key);
    let value = unsafe { corefoundation::api::CFDictionaryGetValue(info.cast(), key.cast()) as Id };
    objc::arc::release(key);
    value
}

struct Directory(std::path::PathBuf);

impl Directory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("rosemary-url-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn guest_path(&self, name: &str) -> String {
        prefix::ns::to_guest(&self.0.join(name))
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn reachability_checks_entries_and_preserves_success_error_output() {
    let _pool = Pool::new();
    let dir = Directory::new("entries");
    std::fs::write(dir.0.join("a #b?c"), b"contents").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("absent", dir.0.join("dangling")).unwrap();
        std::os::unix::fs::symlink("loop", dir.0.join("loop")).unwrap();
    }
    let mut names = vec!["", "a #b?c"];
    #[cfg(unix)]
    names.extend(["dangling", "loop"]);
    // SAFETY: URLs and error pointers are live for these registered method calls.
    unsafe {
        let sentinel = foundation::error::make("sentinel", 42, "unchanged");
        for name in names {
            let path = dir.guest_path(name);
            let url = corefoundation::url::new_file_url(&path, name.is_empty()) as Id;
            let mut error = sentinel;
            assert_eq!(reachable(url, &mut error), 1, "{name}");
            assert_eq!(error, sentinel);
            assert_eq!(reachable(url, std::ptr::null_mut()), 1);
            objc::arc::release(url);
        }
        let path = dir.guest_path("a #b?c");
        let file_url = corefoundation::url::new_file_url(&path, false);
        let text = format!(
            "{}?ignored#ignored",
            corefoundation::url::as_url(file_url).unwrap().text
        );
        corefoundation::api::CFRelease(file_url);
        let url = corefoundation::url::new(&text) as Id;
        assert_eq!(reachable(url, std::ptr::null_mut()), 1);
        objc::arc::release(url);
    }
}

#[test]
fn reachability_reports_cocoa_and_underlying_posix_errors() {
    let _pool = Pool::new();
    let dir = Directory::new("errors");
    std::fs::write(dir.0.join("file"), b"contents").unwrap();
    // SAFETY: URLs, strings and NSError outputs have their documented types.
    unsafe {
        for (name, code, errno) in [
            ("missing", 260, libc::ENOENT),
            ("file/child", 256, libc::ENOTDIR),
        ] {
            let path = dir.guest_path(name);
            let url = corefoundation::url::new_file_url(&path, false) as Id;
            let mut error = std::ptr::null_mut();
            assert_eq!(reachable(url, &mut error), 0);
            assert!(!error.is_null());
            assert_eq!(string(error, "domain"), "NSCocoaErrorDomain");
            assert_eq!(integer(error, "code"), code);
            assert_eq!(info_value(error, "NSURL"), url);
            assert_eq!(
                foundation::string::bytes(info_value(error, "NSFilePath")),
                path.as_bytes()
            );
            let underlying = info_value(error, "NSUnderlyingError");
            assert_eq!(string(underlying, "domain"), "NSPOSIXErrorDomain");
            assert_eq!(
                integer(underlying, "code"),
                i64::from(darwin::errno::from_host(errno))
            );
            assert_eq!(reachable(url, std::ptr::null_mut()), 0);
            objc::arc::release(url);
            // The autoreleased error's userInfo keeps the URL alive.
            assert_eq!(string(info_value(error, "NSURL"), "path"), path);
        }
        let url = corefoundation::url::new("https://example.test/existing") as Id;
        let mut error = std::ptr::null_mut();
        assert_eq!(reachable(url, &mut error), 0);
        assert_eq!(integer(error, "code"), 262);
        assert_eq!(string(error, "domain"), "NSCocoaErrorDomain");
        assert_eq!(reachable(url, std::ptr::null_mut()), 0);
        objc::arc::release(url);
    }
}

#[test]
fn url_accessors_preserve_encoded_components_and_decode_paths() {
    let _pool = Pool::new();
    // SAFETY: method signatures below match Apple's NSURL declarations.
    unsafe {
        let text = "https://user%20name:p%40ss@[::1]:8080/a%20b/c.tar.gz/?x%20y=z#f%20g";
        let url = corefoundation::url::new(text) as Id;
        for (selector, expected) in [
            ("path", "/a b/c.tar.gz"),
            ("lastPathComponent", "c.tar.gz"),
            ("pathExtension", "gz"),
            ("user", "user name"),
            ("password", "p%40ss"),
            ("query", "x%20y=z"),
            ("fragment", "f%20g"),
            ("resourceSpecifier", &text[6..]),
        ] {
            assert_eq!(string(url, selector), expected, "{selector}");
        }
        assert_eq!(object(url, "absoluteURL"), url);
        assert_eq!(integer(object(url, "port"), "integerValue"), 8080);
        let (call, sel) =
            implementation::<unsafe extern "C" fn(Id, Sel) -> u8>(url, "hasDirectoryPath");
        assert_eq!(call(url, sel), 1);
        let parts = object(url, "pathComponents");
        let parts = corefoundation::collections::array(parts.cast()).unwrap();
        let parts: Vec<_> = parts
            .values
            .iter()
            .map(|part| foundation::string::bytes(*part as Id))
            .collect();
        assert_eq!(
            parts,
            [b"/".to_vec(), b"a b".to_vec(), b"c.tar.gz".to_vec()]
        );
        let deleted = object(url, "URLByDeletingPathExtension");
        assert_eq!(
            string(deleted, "absoluteString"),
            "https://user%20name:p%40ss@[::1]:8080/a%20b/c.tar/?x%20y=z#f%20g"
        );
        objc::arc::release(url);

        let url = corefoundation::url::new("https://example.test/#fragment?inside") as Id;
        assert!(object(url, "query").is_null());
        assert!(object(url, "port").is_null());
        assert!(object(url, "user").is_null());
        assert!(object(url, "password").is_null());
        objc::arc::release(url);
    }
}

#[test]
fn deleting_extensions_preserves_directories_dotfiles_and_url_suffixes() {
    let _pool = Pool::new();
    unsafe {
        for (text, extension, deleted) in [
            ("file:///", "", "file:///"),
            ("file:///tmp/.hidden", "", "file:///tmp/.hidden"),
            ("file:///tmp/.hidden.txt/", "txt", "file:///tmp/.hidden/"),
            ("file:///tmp/a.", "", "file:///tmp/a."),
            ("file:///tmp/a%2Fb.txt", "txt", "file:///tmp/a%2Fb"),
            ("file:///tmp/a.tar.gz?x#y", "gz", "file:///tmp/a.tar?x#y"),
            ("foo/bar.txt", "txt", "foo/bar"),
            ("https://example.test", "", "https://example.test"),
        ] {
            let url = corefoundation::url::new(text) as Id;
            assert_eq!(string(url, "pathExtension"), extension);
            assert_eq!(
                string(object(url, "URLByDeletingPathExtension"), "absoluteString"),
                deleted
            );
            let cf = corefoundation::api::CFURLCreateCopyDeletingPathExtension(
                std::ptr::null(),
                url.cast(),
            );
            assert_eq!(corefoundation::url::as_url(cf).unwrap().text, deleted);
            corefoundation::api::CFRelease(cf);
            objc::arc::release(url);
        }
    }
}

#[test]
fn filesystem_representation_requires_space_for_terminator() {
    let _pool = Pool::new();
    unsafe {
        let url = corefoundation::url::new_file_url("/tmp/a #b", false) as Id;
        let (call, sel) = implementation::<
            unsafe extern "C" fn(Id, Sel, *mut std::ffi::c_char, usize) -> u8,
        >(url, "getFileSystemRepresentation:maxLength:");
        let mut buffer = [0x55u8; 20];
        assert_eq!(call(url, sel, buffer.as_mut_ptr().cast(), 9), 0);
        assert_eq!(buffer, [0x55; 20]);
        assert_eq!(call(url, sel, buffer.as_mut_ptr().cast(), 10), 1);
        assert_eq!(&buffer[..10], b"/tmp/a #b\0");
        assert_eq!(buffer[10], 0x55);
        assert_eq!(call(url, sel, std::ptr::null_mut(), 0), 0);
        objc::arc::release(url);
        let url = corefoundation::url::new("https://example.test/tmp/a") as Id;
        assert_eq!(call(url, sel, buffer.as_mut_ptr().cast(), buffer.len()), 0);
        objc::arc::release(url);
    }
}

#[test]
fn initializers_return_owned_concrete_urls() {
    let pool = Pool::new();
    unsafe {
        let class = objc::class::lookup(c"NSURL").unwrap();
        let class = objc::class::ptr(class) as Id;
        let text = foundation::string::from_str("https://example.test/path?x#y");
        let allocated = object(class, "alloc");
        let (call, sel) =
            implementation::<unsafe extern "C" fn(Id, Sel, Id) -> Id>(allocated, "initWithString:");
        let url = call(allocated, sel, text);
        objc::arc::release(text);
        let path = foundation::string::from_str("/tmp/a b");
        let allocated = object(class, "alloc");
        let (call, sel) = implementation::<unsafe extern "C" fn(Id, Sel, Id, u8) -> Id>(
            allocated,
            "initFileURLWithPath:isDirectory:",
        );
        let directory = call(allocated, sel, path, 1);
        objc::arc::release(path);
        drop(pool);
        assert_eq!(
            corefoundation::url::as_url(url.cast()).unwrap().text,
            "https://example.test/path?x#y"
        );
        assert_eq!(
            corefoundation::url::as_url(directory.cast()).unwrap().text,
            "file:///tmp/a%20b/"
        );
        objc::arc::release(url);
        objc::arc::release(directory);
    }
}
