//! The policies that wrap a sink, and the one that records for a test.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::Exports;

/// Refuse a null address rather than publishing a slot that faults at the call.
///
/// A module may name an entry point the host turns out not to have - a `dlsym`
/// that came back null, a `cfg` that left the implementation out. Binding the
/// import to zero is worse than leaving it unbound: an unresolved import is
/// refused by name at the call, where a zero one is a jump to address zero with
/// nothing to say which symbol it was.
pub struct NonNull<'a>(pub &'a mut dyn Exports);

impl Exports for NonNull<'_> {
    fn function(&mut self, symbol: &str, address: u64) {
        if address != 0 {
            self.0.function(symbol, address);
        }
    }

    fn data(&mut self, symbol: &str, address: u64) {
        if address != 0 {
            self.0.data(symbol, address);
        }
    }
}

/// Publish everything `deny` does not name.
///
/// `ROSETTA3_WITHHOLD`'s half of the A/B this tree could not otherwise run: a
/// framework answer that is subtly wrong is invisible, and the way to find one
/// is to compare against a runtime that does not answer at all.
pub struct Denying<'a> {
    pub inner: &'a mut dyn Exports,
    pub deny: fn(&str) -> bool,
}

impl Exports for Denying<'_> {
    fn function(&mut self, symbol: &str, address: u64) {
        if !(self.deny)(symbol) {
            self.inner.function(symbol, address);
        }
    }

    fn data(&mut self, symbol: &str, address: u64) {
        if !(self.deny)(symbol) {
            self.inner.data(symbol, address);
        }
    }
}

/// Everything published, in order, for a test that asks about the whole set.
///
/// The question a hand-written provider could not be asked: whether two
/// modules define the same symbol. The library underneath is a map, so the
/// second definition simply replaces the first and nothing says so.
#[derive(Default)]
pub struct Recording {
    pub functions: Vec<(String, u64)>,
    pub data: Vec<(String, u64)>,
}

impl Recording {
    /// Every symbol published, function and data together.
    pub fn symbols(&self) -> Vec<&str> {
        self.functions
            .iter()
            .chain(&self.data)
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

impl Exports for Recording {
    fn function(&mut self, symbol: &str, address: u64) {
        self.functions.push((symbol.to_string(), address));
    }

    fn data(&mut self, symbol: &str, address: u64) {
        self.data.push((symbol.to_string(), address));
    }
}

/// The loader's library as a sink.
#[cfg(feature = "machish")]
impl Exports for machish::resolve::SyntheticDylib {
    fn function(&mut self, symbol: &str, address: u64) {
        self.define_fn(symbol, address as usize as *const ());
    }

    fn data(&mut self, symbol: &str, address: u64) {
        self.define_data(symbol, address);
    }
}
