//! Site C primitives are single-threaded only primitives.
#[cfg(feature = "mutex")]
pub mod mutex;
#[cfg(feature = "queue")]
pub mod queue;
#[cfg(feature = "rwlock")]
pub mod rwlock;
