//! `OSStatus` values, as the framework's headers define them.
//!
//! A guest checks these by number and branches on them, so they are the
//! interface: an invented one is a program taking the wrong path for a reason
//! nothing can name.

/// `noErr`.
pub const SUCCESS: i32 = 0;
/// `paramErr`.
pub const PARAM_ERROR: i32 = -50;
/// `kAudioUnitErr_InvalidProperty`.
pub const INVALID_PROPERTY: i32 = -10879;
/// `kAudioUnitErr_InvalidParameter`.
pub const INVALID_PARAMETER: i32 = -10878;
/// `kAudioUnitErr_InvalidPropertyValue`.
pub const INVALID_PROPERTY_VALUE: i32 = -10851;
/// `kAudioUnitErr_FormatNotSupported`.
pub const FORMAT_NOT_SUPPORTED: i32 = -10868;
/// `kAudioUnitErr_Uninitialized`.
pub const UNINITIALIZED: i32 = -10867;
/// `kAudioUnitErr_CannotDoInCurrentContext`.
pub const CANNOT_DO_IN_CURRENT_CONTEXT: i32 = -10863;
/// `kAudioUnitErr_InvalidElement`, which stands in for "not a unit of this
/// runtime's" - there is no `badComponentInstance` in the AudioUnit range.
pub const INVALID_COMPONENT: i32 = -10877;
