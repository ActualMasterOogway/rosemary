//! Every image with thread-locals, and the key its blocks hang off.

use std::sync::{Arc, RwLock};

use darwin::tls::key::Key;
use machish::report::{ImageReport, TlvTemplate};

/// Largest thread block this will allocate.
const MAX_BLOCK: u64 = 64 * 1024 * 1024;

/// One image's thread-local block, as [`machish`] described it.
#[derive(Clone)]
pub(super) struct Image {
    /// The key whose value on a thread is that thread's block for this image.
    pub key: Key,
    /// Slid VA the initialised prefix is copied from.
    pub initial_va: u64,
    pub initial_size: usize,
    pub total_size: usize,
    /// Byte alignment of the block.
    pub align: usize,
    /// Constructor addresses in section order, run in reverse.
    pub init_funcs: Arc<[u64]>,
}

/// Every registered image, in registration order. Append-only, and read only on a
/// thread's first touch of an image - the fast path never comes here.
static IMAGES: RwLock<Vec<Image>> = RwLock::new(Vec::new());

/// The layout `image` describes, if it has a block worth allocating.
pub(super) fn layout(image: &ImageReport) -> Option<TlvTemplate> {
    let t = image.tlv_template?;
    let usable = !image.thread_locals.is_empty() && t.total_size != 0 && t.total_size <= MAX_BLOCK;
    usable.then_some(t)
}

/// Record `image`'s block under a fresh key, which is what its descriptors will carry.
pub(super) fn add(image: &ImageReport, t: TlvTemplate) -> Result<Key, i32> {
    // `free`, as dyld's `tlv_free` is: a block is a `malloc`-family result and
    // nothing else owns it. See [`super::block`].
    let key = Key::create(Some(free_block))?;
    let entry = Image {
        key,
        initial_va: t.initial_va,
        initial_size: t.initial_size.min(t.total_size) as usize,
        total_size: t.total_size as usize,
        align: 1usize << t.align.min(16),
        init_funcs: image.tlv_init_funcs.iter().copied().collect(),
    };
    IMAGES
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .push(entry);
    Ok(key)
}

/// The image whose blocks hang off `key`.
pub(super) fn by_key(key: u32) -> Option<Image> {
    IMAGES
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|i| i.key.index() == key)
        .cloned()
}

/// A block key's destructor.
///
/// # Safety
///
/// Called by the key's teardown with a value [`super::block`] stored, which came
/// from `posix_memalign`.
unsafe extern "C" fn free_block(block: *mut core::ffi::c_void) {
    // SAFETY: the caller's guarantee.
    unsafe { libc::free(block) };
}

#[cfg(test)]
pub(super) fn add_for_test(
    initial_va: u64,
    initial_size: usize,
    total_size: usize,
    init_funcs: &[u64],
) -> Key {
    let key = Key::create(Some(free_block)).expect("a key is available");
    IMAGES.write().unwrap().push(Image {
        key,
        initial_va,
        initial_size,
        total_size,
        align: 16,
        init_funcs: init_funcs.into(),
    });
    key
}
