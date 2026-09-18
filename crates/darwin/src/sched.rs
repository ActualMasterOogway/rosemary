//! Darwin scheduler policies, whose numeric values differ from Linux's.

#![cfg(not(target_os = "macos"))]

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

exports::exports! {
    functions {
        "sched_get_priority_max" => max,
        "sched_get_priority_min" => min,
    }
}
