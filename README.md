# Cynosure

A group of high performance, lightweight datastructures mostly optimized for usage in single-threaded
async executors, but some structures can be used elsewhere. Zero dependencies.

The crate is split into `site_c` and `site_d`. `site_c` are single-thread only, and `site_d`
primitives work across multiple threads.

## `site_c`

- `LocalCell`: `Rc<RefCell<T>>`-like structure without runtime checks with scoped mutable access.
Intentionally doesn't work over await points.
- `LocalMutex`: Fast single-threaded mutex. Use when you _do_ want to hold over await points.
- `LocalRwLock`: Fast single-threaded reader-writer lock. Allows multiple concurrent readers or one exclusive writer.
- `Queue`: Double-ended queue that stores up to N items inline before spilling to heap.

## `site_d`

- `RingBuf`: lock-free SPSC ring buffer with async and sync support.
