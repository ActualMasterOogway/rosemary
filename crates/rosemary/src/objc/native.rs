use std::ffi::{CString, c_char, c_void};

use machish::report::LinkReport;

// Named, unlike the rest of this crate's libobjc calls: these two are *private*
// exports with no `dlsym`-able counterpart in [`crate::objc::host`], and they
// only ever mean the host's own runtime - a host-architecture image is dyld's
// to map, not ours.
#[link(name = "objc")]
unsafe extern "C" {
    /// `void _objc_map_images(unsigned, const char *const [], const struct mach_header *const [])`
    ///
    /// dyld's own `mapped` notification, exported. Everything a compiled image
    /// needs done to it - selectors uniqued, class names published, categories
    /// attached, protocols interned - happens in here, against real arm64
    /// metadata, so none of [`crate::objc`]'s reconstruction applies.
    fn _objc_map_images(count: u32, paths: *const *const c_char, mhdrs: *const *const c_void);

    /// `void _objc_load_image(const char *, const struct mach_header *)` - the
    /// `init` notification, which is what runs `+load`.
    fn _objc_load_image(path: *const c_char, mh: *const c_void);
}

/// Notify libobjc of every image the loader mapped, then run their `+load`s.
///
/// Dependencies first, which is the order `report.images` holds reversed.
pub fn map_images(report: &LinkReport) {
    let named: Vec<(CString, *const c_void)> = report
        .images
        .iter()
        .rev()
        .filter_map(|i| {
            let path = CString::new(i.name.as_str()).ok()?;
            Some((path, i.base as usize as *const c_void))
        })
        .collect();
    if named.is_empty() {
        return;
    }
    let paths: Vec<*const c_char> = named.iter().map(|(p, _)| p.as_ptr()).collect();
    let mhdrs: Vec<*const c_void> = named.iter().map(|(_, m)| *m).collect();

    // SAFETY: each `base` is the mach header of an image this process mapped
    // and linked, whose segments keep the relative layout `getsectiondata`
    // reconstructs the slide from, and the two arrays are `count` long.
    unsafe {
        _objc_map_images(paths.len() as u32, paths.as_ptr(), mhdrs.as_ptr());
        for (path, mh) in &named {
            _objc_load_image(path.as_ptr(), *mh);
        }
    }
}
