use std::ffi::{CString, c_void};

use machish::resolve::{LazyDylib, Resolved};

pub fn interposed() -> bool {
    !cfg!(target_os = "macos")
}

pub const INSTALL_PATHS: &[&str] = &[
    "/System/Library/Frameworks/OpenAL.framework/Versions/A/OpenAL",
    "/System/Library/Frameworks/OpenAL.framework/Versions/Current/OpenAL",
    "/System/Library/Frameworks/OpenAL.framework/OpenAL",
    "OpenAL",
];

const HOST_LIBRARIES: [&str; 2] = ["libopenal.so.1", "libopenal.so"];

pub fn provider() -> LazyDylib {
    let mut lib = LazyDylib::new(INSTALL_PATHS[0], open, resolve);
    for name in &INSTALL_PATHS[1..] {
        lib.alias(name);
    }
    lib.flat(interposed());
    lib
}

pub fn symbol(name: &str) -> Option<u64> {
    if !interposed() || !is_openal(name) || !open() {
        return None;
    }
    address(name)
}

fn is_openal(name: &str) -> bool {
    let rest = match name.strip_prefix("alc") {
        Some(rest) => rest,
        None => match name.strip_prefix("al") {
            Some(rest) => rest,
            None => return false,
        },
    };
    rest.starts_with(|c: char| c.is_ascii_uppercase())
}

fn handle() -> *mut c_void {
    static HANDLE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *HANDLE.get_or_init(|| {
        for name in HOST_LIBRARIES {
            let Ok(name) = CString::new(name) else {
                continue;
            };
            let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if !handle.is_null() {
                return handle as usize;
            }
        }
        eprintln!(
            "rosetta3: no OpenAL on this host ({}); the guest's OpenAL imports will be \
             refused and it will run without sound",
            HOST_LIBRARIES.join(", "),
        );
        0
    }) as *mut c_void
}

fn open() -> bool {
    !handle().is_null()
}

fn resolve(symbol: &str) -> Option<Resolved> {
    let name = symbol.strip_prefix('_')?;
    is_openal(name)
        .then(|| address(name))
        .flatten()
        .map(Resolved::function)
}

fn address(name: &str) -> Option<u64> {
    let handle = handle();
    if handle.is_null() {
        return None;
    }
    let c = CString::new(name).ok()?;
    // SAFETY: a live handle from `dlopen` and a C string.
    let found = unsafe { libc::dlsym(handle, c.as_ptr()) };
    match found.is_null() {
        true => None,
        false => Some(found as usize as u64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The prefix test admits the specification's names and not the C
    /// library's, which is the whole reason it is not `starts_with("al")`.
    #[test]
    fn tells_openal_from_the_c_library() {
        assert!(is_openal("alGenSources"));
        assert!(is_openal("alcOpenDevice"));
        assert!(is_openal("alIsExtensionPresent"));
        assert!(!is_openal("alarm"));
        assert!(!is_openal("aligned_alloc"));
        assert!(!is_openal("alloca"));
        assert!(!is_openal("al"));
    }
}
