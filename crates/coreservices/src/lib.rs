#![allow(
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    non_snake_case
)]

//! The part of CoreServices a Carbon-era game still reaches.
//!
//! Life Is Strange is Unreal Engine 3, whose Mac backend predates the
//! deprecation of the File Manager, so it asks Carbon where to put its saves
//! before it asks anything else.

pub mod events;
pub mod files;
pub mod process;

exports::group! {
    events,
    files,
    process,
}
