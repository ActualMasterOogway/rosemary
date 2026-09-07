//! Darwin scheduler policies, whose numeric values differ from Linux's.

#![cfg(not(target_os = "macos"))]

use machish::resolve::SyntheticDylib;

use crate::pthread;

unsafe extern "C" fn max(policy: i32) -> i32 {
    pthread::consts::sched_policy_to_host(policy).map_or_else(
        || {
            crate::errno::set(libc::EINVAL);
            -1
        },
        |_| pthread::consts::DARWIN_PRIORITY_MAX,
    )
}

unsafe extern "C" fn min(policy: i32) -> i32 {
    pthread::consts::sched_policy_to_host(policy).map_or_else(
        || {
            crate::errno::set(libc::EINVAL);
            -1
        },
        |_| pthread::consts::DARWIN_PRIORITY_MIN,
    )
}

pub fn symbol(name: &str) -> Option<u64> {
    let f = match name {
        "sched_get_priority_max" => max as *const (),
        "sched_get_priority_min" => min as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

pub fn provide(lib: &mut SyntheticDylib) {
    for name in ["sched_get_priority_max", "sched_get_priority_min"] {
        crate::define_fn(lib, &format!("_{name}"), symbol(name).unwrap() as *const ());
    }
}
