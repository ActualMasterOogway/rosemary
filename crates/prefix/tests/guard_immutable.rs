mod common;

use std::ffi::CString;

use prefix::system::SystemFiles;

#[test]
fn a_write_to_a_system_file_is_refused() {
    let prefix = common::provisioned("immutable", SystemFiles::Immutable);
    let drive = prefix.drive();
    let system_file =
        CString::new(drive.join("usr/lib/libSystem.B.dylib").to_str().unwrap()).unwrap();
    let user_file = CString::new(drive.join("usr/lib/mine.dylib").to_str().unwrap()).unwrap();
    assert_eq!(prefix::ns::may_write(&system_file), Err(libc::EROFS));
    assert_eq!(prefix::ns::may_write(&user_file), Ok(()));
    let _ = std::fs::remove_dir_all(prefix.directory());
}
