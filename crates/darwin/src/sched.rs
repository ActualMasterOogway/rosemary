//! Darwin scheduler policies, whose numeric values differ from Linux's.

#![cfg(not(target_os = "macos"))]

use machish::resolve::SyntheticDylib;

fn policy(policy: i32) -> Option<i32> {
    Some(match policy {
        1 => libc::SCHED_OTHER,
        2 => libc::SCHED_RR,
        4 => libc::SCHED_FIFO,
        _ => return None,
    })
}

unsafe extern "C" fn max(policy: i32) -> i32 {
    policy(policy).map_or_else(
        || {
            crate::errno::set(libc::EINVAL);
            -1
        },
        |policy| unsafe { libc::sched_get_priority_max(policy) },
    )
}

unsafe extern "C" fn min(policy: i32) -> i32 {
    policy(policy).map_or_else(
        || {
            crate::errno::set(libc::EINVAL);
            -1
        },
        |policy| unsafe { libc::sched_get_priority_min(policy) },
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
