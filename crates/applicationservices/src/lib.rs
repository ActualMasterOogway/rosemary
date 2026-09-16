#[repr(i32)]
enum CGWindowLevelKey {
    BaseWindow = 0,
}

type CGWindowLevel = i32;

// FIXME (ACCURACY): STUB!
#[allow(non_snake_case)]
extern "C" fn CGWindowLevelForKey(_key: CGWindowLevelKey) -> CGWindowLevel {
    0
}

const NAMES: &[&str] = &["CGCursorIsVisible", "CGWindowLevelForKey"];

pub fn names() -> Vec<&'static str> {
    NAMES.iter().copied().collect()
}

pub fn symbol(name: &str) -> Option<u64> {
    Some(match name {
        "CGCursorIsVisible" => appkit::cursor::CGCursorIsVisible as *const () as u64,
        "CGWindowLevelForKey" => CGWindowLevelForKey as *const () as u64,
        _ => return None,
    })
}
