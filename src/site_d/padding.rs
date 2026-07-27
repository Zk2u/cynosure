//! `CachePadded<T>` — aligns a value to its own cache line.
//!
//! Used to keep independently-written atomics off each other's lines; false
//! sharing between two hot indices costs far more than the padding.

use std::ops::{Deref, DerefMut};

/// Padding to prevent false sharing between atomic variables
#[repr(align(64))]
#[derive(Default)]
pub struct CachePadded<T> {
    value: T,
}

impl<T> CachePadded<T> {
    /// Wrap `value` on its own cache line.
    pub fn new(value: T) -> Self {
        Self { value }
    }

    /// Unwrap, discarding the padding.
    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T> Deref for CachePadded<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> DerefMut for CachePadded<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

impl<T> AsRef<T> for CachePadded<T> {
    fn as_ref(&self) -> &T {
        &self.value
    }
}

impl<T> AsMut<T> for CachePadded<T> {
    fn as_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T> From<T> for CachePadded<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}
