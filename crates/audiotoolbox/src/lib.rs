#![allow(clippy::missing_safety_doc)]

//! `AudioToolbox`, which here is `AUGraph` and nothing else.
//!
//! The `AudioUnit` and `AudioConverter` halves of the framework live in
//! [`coreaudio`]; this is the graph that ties units together. See [`augraph`].

pub mod augraph;

pub fn names() -> Vec<&'static str> {
    augraph::NAMES.to_vec()
}

pub fn symbol(name: &str) -> Option<u64> {
    augraph::symbol(name)
}
