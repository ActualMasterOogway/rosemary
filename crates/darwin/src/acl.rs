#![cfg(not(target_os = "macos"))]

use std::ffi::{c_char, c_int, c_void};

/// The answer for every file: it has no extended ACL.
fn no_acl() -> *mut c_void {
    crate::errno::set(libc::ENOENT);
    std::ptr::null_mut()
}

/// A call given an `acl_t` or entry this runtime never made.
fn not_an_acl() -> c_int {
    crate::errno::set(libc::EINVAL);
    -1
}

/// `acl_t acl_get_link_np(const char *path, acl_type_t type)`
pub extern "C" fn acl_get_link_np(_path: *const c_char, _kind: c_int) -> *mut c_void {
    no_acl()
}

/// `acl_t acl_get_file(const char *path, acl_type_t type)`
pub extern "C" fn acl_get_file(_path: *const c_char, _kind: c_int) -> *mut c_void {
    no_acl()
}

/// `acl_t acl_get_fd_np(int fd, acl_type_t type)`
pub extern "C" fn acl_get_fd_np(_fd: c_int, _kind: c_int) -> *mut c_void {
    no_acl()
}

/// `int acl_free(void *obj)`: null is accepted, as it is everywhere else.
pub extern "C" fn acl_free(obj: *mut c_void) -> c_int {
    match obj.is_null() {
        true => 0,
        false => not_an_acl(),
    }
}

/// `int acl_get_entry(acl_t, int, acl_entry_t *)`
pub extern "C" fn acl_get_entry(_acl: *mut c_void, _id: c_int, _entry: *mut *mut c_void) -> c_int {
    not_an_acl()
}

/// `int acl_get_tag_type(acl_entry_t, acl_tag_t *)`
pub extern "C" fn acl_get_tag_type(_entry: *mut c_void, _tag: *mut c_int) -> c_int {
    not_an_acl()
}

/// `int acl_get_permset(acl_entry_t, acl_permset_t *)`
pub extern "C" fn acl_get_permset(_entry: *mut c_void, _set: *mut *mut c_void) -> c_int {
    not_an_acl()
}

/// `int acl_get_flagset_np(void *, acl_flagset_t *)`
pub extern "C" fn acl_get_flagset_np(_obj: *mut c_void, _set: *mut *mut c_void) -> c_int {
    not_an_acl()
}

/// `int acl_get_perm_np(acl_permset_t, acl_perm_t)`
pub extern "C" fn acl_get_perm_np(_set: *mut c_void, _perm: c_int) -> c_int {
    not_an_acl()
}

/// `int acl_get_flag_np(acl_flagset_t, acl_flag_t)`
pub extern "C" fn acl_get_flag_np(_set: *mut c_void, _flag: c_int) -> c_int {
    not_an_acl()
}

/// `void *acl_get_qualifier(acl_entry_t)`
pub extern "C" fn acl_get_qualifier(_entry: *mut c_void) -> *mut c_void {
    not_an_acl();
    std::ptr::null_mut()
}

/// `int mbr_identifier_translate(int id_type, const void *identifier,
/// size_t identifier_size, int target_type, void **result, int *rec_type)`
///
/// Returns an error number rather than setting `errno`. There is no directory
/// service behind this host's users to translate through, so every identifier
/// is one it has no record of.
pub extern "C" fn mbr_identifier_translate(
    _id_type: c_int,
    _identifier: *const c_void,
    _identifier_size: usize,
    _target_type: c_int,
    _result: *mut *mut c_void,
    _rec_type: *mut c_int,
) -> c_int {
    libc::ENOENT
}

exports::exports! {
    functions {
        "acl_get_link_np" => acl_get_link_np,
        "acl_get_file" => acl_get_file,
        "acl_get_fd_np" => acl_get_fd_np,
        "acl_free" => acl_free,
        "acl_get_entry" => acl_get_entry,
        "acl_get_tag_type" => acl_get_tag_type,
        "acl_get_permset" => acl_get_permset,
        "acl_get_flagset_np" => acl_get_flagset_np,
        "acl_get_perm_np" => acl_get_perm_np,
        "acl_get_flag_np" => acl_get_flag_np,
        "acl_get_qualifier" => acl_get_qualifier,
        "mbr_identifier_translate" => mbr_identifier_translate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_has_no_extended_acl() {
        assert!(acl_get_link_np(c"/".as_ptr(), 0x100).is_null());
        assert_eq!(crate::errno::get(), libc::ENOENT);
        assert_eq!(acl_free(std::ptr::null_mut()), 0);
    }
}
