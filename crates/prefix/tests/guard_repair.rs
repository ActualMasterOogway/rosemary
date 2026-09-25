mod common;

use std::ffi::CString;

use prefix::system::{self, SystemFiles};

#[test]
fn a_write_to_a_system_file_is_allowed_and_recorded() {
    let prefix = common::provisioned("repair", SystemFiles::Repair);
    let drive = prefix.drive();
    let system_file =
        CString::new(drive.join("usr/lib/libSystem.B.dylib").to_str().unwrap()).unwrap();
    let dotted = CString::new(
        drive
            .join("usr/lib/../lib/./libSystem.B.dylib")
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let user_file = CString::new(drive.join("usr/lib/mine.dylib").to_str().unwrap()).unwrap();

    assert_eq!(prefix::ns::may_write(&user_file), Ok(()));
    assert!(
        system::dirty(&prefix).is_empty(),
        "a user file is not a system file"
    );
    assert_eq!(prefix::ns::may_write(&system_file), Ok(()));
    assert_eq!(prefix::ns::may_write(&dotted), Ok(()));
    assert_eq!(
        system::dirty(&prefix),
        ["/usr/lib/libSystem.B.dylib".to_string()].into()
    );
    // Recorded once, however often it is written.
    let text = std::fs::read_to_string(system::dirty_path(&prefix)).unwrap();
    assert_eq!(text.lines().count(), 1);
    let _ = std::fs::remove_dir_all(prefix.directory());
}
