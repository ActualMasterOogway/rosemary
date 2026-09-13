//! Small RAII guards for the creation paths.
//!
//! Building a Vulkan object is usually several calls, any of which can fail
//! after the earlier ones have already handed back a handle. Writing the
//! unwind by hand means an `inspect_err` on every step and a leak as soon as
//! a step is added in the middle. A [`Guard`] instead owns what has been made
//! so far and destroys it on the way out of the function unless the caller
//! [`take`](Guard::take)s it, so the failure path is the default and only the
//! success path is written.

/// A value destroyed when it is dropped, unless it is taken first.
pub(crate) struct Guard<T, F: FnOnce(T)> {
    /// `None` only between `take` and the drop which follows it.
    value: Option<T>,
    destroy: Option<F>,
}

impl<T, F: FnOnce(T)> Guard<T, F> {
    pub(crate) fn new(value: T, destroy: F) -> Self {
        Self {
            value: Some(value),
            destroy: Some(destroy),
        }
    }

    /// Give up ownership: the value survives the guard.
    pub(crate) fn take(mut self) -> T {
        self.destroy = None;
        self.value.take().expect("a guard is taken once")
    }
}

impl<T, F: FnOnce(T)> std::ops::Deref for Guard<T, F> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value.as_ref().expect("a guard is taken once")
    }
}

impl<T, F: FnOnce(T)> Drop for Guard<T, F> {
    fn drop(&mut self) {
        if let (Some(destroy), Some(value)) = (self.destroy.take(), self.value.take()) {
            destroy(value);
        }
    }
}

/// Guard a whole collection built one fallible element at a time.
///
/// The elements made before a failure are destroyed in reverse order, which
/// is what a hand-written unwind would do and what the `?` in the middle of a
/// `collect` otherwise skips.
pub(crate) struct GuardedVec<T, F: Fn(&T)> {
    values: Vec<T>,
    destroy: F,
    armed: bool,
}

impl<T, F: Fn(&T)> GuardedVec<T, F> {
    pub(crate) fn new(destroy: F) -> Self {
        Self {
            values: Vec::new(),
            destroy,
            armed: true,
        }
    }

    pub(crate) fn push(&mut self, value: T) {
        self.values.push(value);
    }

    pub(crate) fn take(mut self) -> Vec<T> {
        self.armed = false;
        std::mem::take(&mut self.values)
    }
}

impl<T, F: Fn(&T)> Drop for GuardedVec<T, F> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for value in self.values.drain(..).rev() {
            (self.destroy)(&value);
        }
    }
}
