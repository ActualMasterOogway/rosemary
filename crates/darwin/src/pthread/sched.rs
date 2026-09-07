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
    let mut translated = unsafe { host_param.assume_init() };
    translated.sched_priority =
        super::consts::sched_priority_from_host(translated.sched_priority, host_policy);
    unsafe {
        if !param.is_null() {
            *param = translated;
        }
        if !policy.is_null() {
            *policy = super::consts::sched_policy_from_host(host_policy);
        }
    }

    0
}

pub unsafe extern "C" fn pthread_setschedparam(
    thread: libc::pthread_t,
    policy: c_int,
    param: *const libc::sched_param,
) -> i32 {
    let Some(host_policy) = super::consts::sched_policy_to_host(policy) else {
        return super::consts::err::EINVAL;
    };
    if param.is_null() {
        return super::consts::err::EINVAL;
    }
    // SAFETY: the caller supplied a readable scheduling parameter.
    let mut host_param = unsafe { *param };
    let Some(priority) =
        super::consts::sched_priority_to_host(host_param.sched_priority, host_policy)
    else {
        return super::consts::err::EINVAL;
    };
    host_param.sched_priority = priority;
    crate::errno::from_host(unsafe {
        libc::pthread_setschedparam(thread, host_policy, &host_param)
    })
}
