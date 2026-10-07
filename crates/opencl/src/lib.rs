#![allow(
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    non_snake_case
)]

//! OpenCL, with no platform.
//!
//! There is no OpenCL runtime behind this tree, so the framework answers what a
//! machine with no ICD loader answers: enumeration finds nothing, and any handle a
//! caller could still hold is invalid. A program probing for a compute device
//! (fastfetch counting a GPU's compute units, for one) reads that and carries on,
//! where a null import would be a fault.

use std::ffi::c_void;

type ClInt = i32;

const CL_INVALID_PLATFORM: ClInt = -32;
const CL_INVALID_DEVICE: ClInt = -33;
/// `CL_PLATFORM_NOT_FOUND_KHR`: what an ICD loader with no installed driver
/// returns from `clGetPlatformIDs`.
const CL_PLATFORM_NOT_FOUND_KHR: ClInt = -1001;

/// `cl_int clGetPlatformIDs(cl_uint, cl_platform_id *, cl_uint *)`.
pub unsafe extern "C" fn clGetPlatformIDs(
    _entries: u32,
    _platforms: *mut *mut c_void,
    count: *mut u32,
) -> ClInt {
    if !count.is_null() {
        // SAFETY: the caller's out-parameter.
        unsafe { *count = 0 };
    }
    CL_PLATFORM_NOT_FOUND_KHR
}

/// `cl_int clGetPlatformInfo(...)`: every platform handle is invalid, as none is
/// ever handed out.
pub unsafe extern "C" fn clGetPlatformInfo(
    _platform: *mut c_void,
    _param: u32,
    _size: usize,
    _value: *mut c_void,
    _returned: *mut usize,
) -> ClInt {
    CL_INVALID_PLATFORM
}

/// `cl_int clGetDeviceIDs(cl_platform_id, cl_device_type, cl_uint, cl_device_id *, cl_uint *)`.
pub unsafe extern "C" fn clGetDeviceIDs(
    _platform: *mut c_void,
    _kind: u64,
    _entries: u32,
    _devices: *mut *mut c_void,
    count: *mut u32,
) -> ClInt {
    if !count.is_null() {
        // SAFETY: the caller's out-parameter.
        unsafe { *count = 0 };
    }
    CL_INVALID_PLATFORM
}

/// `cl_int clGetDeviceInfo(...)`.
pub unsafe extern "C" fn clGetDeviceInfo(
    _device: *mut c_void,
    _param: u32,
    _size: usize,
    _value: *mut c_void,
    _returned: *mut usize,
) -> ClInt {
    CL_INVALID_DEVICE
}

exports::exports! {
    functions {
        "clGetPlatformIDs" => clGetPlatformIDs,
        "clGetPlatformInfo" => clGetPlatformInfo,
        "clGetDeviceIDs" => clGetDeviceIDs,
        "clGetDeviceInfo" => clGetDeviceInfo,
    }
}
