use std::ffi::c_int;

/// https://github.com/apple/darwin-libpthread/blob/2b46cbcc56ba33791296cd9714b2c90dae185ec7/src/pthread.c#L1614
///
/// On some operating systems, particularly Windows and some BSDs, inputting a NULL `policy` or `param` is considered invalid and not optional.
/// Additionally, SCHED_ constants are different between systems.
pub unsafe extern "C" fn pthread_getschedparam(
    thread: libc::pthread_t,
    policy: *mut c_int,
    param: *mut libc::sched_param,
) -> c_int {
    let mut host_policy: i32 = 0;
    let mut host_param = std::mem::MaybeUninit::<libc::sched_param>::uninit();
    let result =
        unsafe { libc::pthread_getschedparam(thread, &mut host_policy, host_param.as_mut_ptr()) };
    if result != 0 {
        // MAYBE_INACCURATE: Darwin can only return ESRCH - do we emulate those semantics?
        return crate::errno::from_host(result);
    }
    unsafe {
        if !param.is_null() {
            *param = host_param.assume_init();
        }
        if !policy.is_null() {
            *policy = super::consts::sched_policy_from_host(host_policy);
        }
    }

    result
}

pub unsafe extern "C" fn pthread_setschedparam(
    thread: libc::pthread_t,
    policy: c_int,
    param: *mut libc::sched_param,
) -> i32 {
    super::consts::sched_policy_to_host(policy).map_or_else(
        || {
            crate::errno::set(libc::EINVAL);
            -1
        },
        |policy| unsafe { libc::pthread_setschedparam(thread, policy, param) },
    )
}
