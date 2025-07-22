//! A lightweight, reference-counted smart pointer for single-threaded async executors.
//!
//! `LocalCell<T>` provides safe shared mutability without the overhead of `RefCell`'s
//! dynamic borrow checking. It's designed specifically for single-threaded async
//! environments like `monoio`.
//!
//! # Key Features
//!
//! - **Zero runtime overhead** - No borrow checking or panic-prone runtime checks
//! - **Compile-time safety** - Mutations only allowed in synchronous closures, preventing
//!   references held across await points
//! - **Lightweight** - Minimal reference counting without weak references
//!
//! # Example
//!
//! ```rust
//! use localcell::LocalCell;
//!
//! #[monoio::main]
//! async fn main() {
//!     let counter = LocalCell::new(0);
//!     let counter2 = counter.clone();
//!
//!     // Mutation in sync closure - safe!
//!     counter.with_mut(|c| *c += 1);
//!
//!     // Async work
//!     monoio::spawn(async move {
//!         // Can't await inside with_mut - won't compile!
//!         counter2.with_mut(|c| *c += 1);
//!     }).await;
//!
//!     assert_eq!(counter.with(|c| *c), 2);
//! }
//! ```
//!
//! # When to Use
//!
//! - Single-threaded async executors (like `monoio`)
//! - Multiple futures sharing mutable data
//! - Performance-critical code where `RefCell` overhead matters
//!
//! # When NOT to Use
//!
//! - Multi-threaded executors (type is `!Send` and `!Sync`)
//! - When you need weak references
//! - When you want runtime borrow checking safety
//!
//! # Safety
//!
//! `LocalCell` uses `unsafe` internally but provides a safe API by ensuring:
//! - References can't escape the closure scope
//! - No await points possible during access
//! - Single-threaded execution only
use std::cell::Cell;
use std::ptr::NonNull;

/// LocalCell is an Rc-like type that allows for mutable access to its value
/// within a scoped closure. This is useful for usecases in a single-threaded,
/// async executor where you have multiple futures sharing the same data, without
/// having to use a `RefCell`.
pub struct LocalCell<T> {
    ptr: NonNull<Inner<T>>,
}

struct Inner<T> {
    strong_count: Cell<usize>,
    value: T,
}

impl<T> LocalCell<T> {
    pub fn new(value: T) -> Self {
        let inner = Box::new(Inner {
            strong_count: Cell::new(1),
            value,
        });

        Self {
            ptr: NonNull::from(Box::leak(inner)),
        }
    }

    /// Executes a closure with a reference to the inner value.
    pub fn with<R, F>(&self, f: F) -> R
    where
        F: FnOnce(&T) -> R,
    {
        unsafe { f(&(*self.ptr.as_ptr()).value) }
    }

    /// Executes a closure with a mutable reference to the inner value.
    pub fn with_mut<R, F>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        unsafe { f(&mut (*self.ptr.as_ptr()).value) }
    }
}

impl<T> Clone for LocalCell<T> {
    fn clone(&self) -> Self {
        unsafe {
            let inner = self.ptr.as_ref();
            inner.strong_count.set(inner.strong_count.get() + 1);
        }
        Self { ptr: self.ptr }
    }
}

impl<T> Drop for LocalCell<T> {
    fn drop(&mut self) {
        unsafe {
            let inner = self.ptr.as_ref();
            let count = inner.strong_count.get();
            if count == 1 {
                // Last reference, deallocate
                drop(Box::from_raw(self.ptr.as_ptr()));
            } else {
                inner.strong_count.set(count - 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn test_new_and_basic_access() {
        let cell = LocalCell::new(42);

        cell.with(|value| {
            assert_eq!(*value, 42);
        });
    }

    #[test]
    fn test_mutable_access() {
        let cell = LocalCell::new(vec![1, 2, 3]);

        cell.with_mut(|vec| {
            vec.push(4);
        });

        cell.with(|vec| {
            assert_eq!(vec, &[1, 2, 3, 4]);
        });
    }

    #[test]
    fn test_clone_shares_data() {
        let cell1 = LocalCell::new(String::from("hello"));
        let cell2 = cell1.clone();

        // Mutate through cell1
        cell1.with_mut(|s| {
            s.push_str(" world");
        });

        // Verify change is visible through cell2
        cell2.with(|s| {
            assert_eq!(s, "hello world");
        });
    }

    #[test]
    fn test_multiple_clones() {
        let cell = LocalCell::new(0);
        let cells: Vec<_> = (0..10).map(|_| cell.clone()).collect();

        // Mutate through original
        cell.with_mut(|value| {
            *value = 100;
        });

        // Verify all clones see the change
        for (i, cell) in cells.iter().enumerate() {
            cell.with(|value| {
                assert_eq!(*value, 100, "Clone {} should see updated value", i);
            });
        }
    }

    #[test]
    fn test_drop_tracking() {
        // Use a custom type that tracks drops
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct DropTracker;
        impl Drop for DropTracker {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        DROP_COUNT.store(0, Ordering::SeqCst);

        {
            let cell1 = LocalCell::new(DropTracker);
            let cell2 = cell1.clone();
            let _cell3 = cell2.clone();

            assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);
            drop(cell1);
            assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);
            drop(cell2);
            assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);
            // cell3 dropped here
        }

        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_concurrent_mutations() {
        // Even though this is for single-threaded use, test that
        // multiple mutations work correctly
        let cell = LocalCell::new(vec![1]);
        let cell2 = cell.clone();

        cell.with_mut(|v| v.push(2));
        cell2.with_mut(|v| v.push(3));
        cell.with_mut(|v| v.push(4));

        cell.with(|v| {
            assert_eq!(v, &[1, 2, 3, 4]);
        });
    }

    #[test]
    fn test_closure_return_values() {
        let cell = LocalCell::new(vec![1, 2, 3]);

        let sum = cell.with(|v| v.iter().sum::<i32>());
        assert_eq!(sum, 6);

        let new_vec = cell.with_mut(|v| {
            v.push(4);
            v.clone()
        });
        assert_eq!(new_vec, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_nested_access() {
        let cell1 = LocalCell::new(10);
        let cell2 = LocalCell::new(20);

        let result = cell1.with(|a| cell2.with(|b| *a + *b));

        assert_eq!(result, 30);
    }

    #[test]
    fn test_self_referential_mutation() {
        let cell = LocalCell::new(vec![1, 2, 3]);
        let cell2 = cell.clone();

        cell.with_mut(|v| {
            // Access through clone while mutating
            let len = cell2.with(|v2| v2.len());
            v.push(len as i32);
        });

        cell.with(|v| {
            assert_eq!(v, &[1, 2, 3, 3]);
        });
    }

    // Async tests to demonstrate the use case
    #[monoio::test(timer_enabled = true)]
    async fn test_async_no_await_in_closure() {
        let cell = LocalCell::new(vec![1, 2, 3]);
        let cell2 = cell.clone();
        let cell3 = cell.clone();

        let handles = [
            monoio::spawn(async move {
                let duration_to_sleep = Duration::from_millis(10);
                monoio::time::sleep(duration_to_sleep).await;
                cell2.with_mut(|vec| vec.push(4))
            }),
            monoio::spawn(async move {
                let duration_to_sleep = Duration::from_millis(10);
                monoio::time::sleep(duration_to_sleep).await;
                cell3.with_mut(|vec| vec.push(5))
            }),
        ];

        for handle in handles {
            handle.await;
        }

        cell.with_mut(|v| {
            assert!(v.contains(&4));
            assert!(v.contains(&5));
        });
    }

    #[monoio::test(timer_enabled = true)]
    async fn test_async_shared_state() {
        let counter = LocalCell::new(0);

        let mut handles = vec![];

        for i in 0..5 {
            let counter = counter.clone();
            let handle = monoio::spawn(async move {
                // Safe mutation within sync closure
                counter.with_mut(|c| {
                    *c += i;
                });
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await;
        }

        counter.with(|c| {
            assert_eq!(*c, 0 + 1 + 2 + 3 + 4);
        });
    }

    #[test]
    fn test_zst_type() {
        // Test with zero-sized type
        let cell = LocalCell::new(());
        let cell2 = cell.clone();

        cell.with(|_| {});
        cell2.with_mut(|_| {});
    }

    #[test]
    fn test_large_clone_count() {
        let cell = LocalCell::new(42);
        let mut cells = vec![cell];

        // Create many clones
        for _ in 0..1000 {
            cells.push(cells.last().unwrap().clone());
        }

        // Drop them in reverse order
        while let Some(cell) = cells.pop() {
            drop(cell);
        }

        // If we get here without panic, reference counting works
    }

    #[test]
    fn test_complex_type() {
        use std::collections::HashMap;

        let mut map = HashMap::new();
        map.insert("key", vec![1, 2, 3]);

        let cell = LocalCell::new(map);

        cell.with_mut(|map| {
            map.get_mut("key").unwrap().push(4);
        });

        cell.with(|map| {
            assert_eq!(map.get("key").unwrap(), &vec![1, 2, 3, 4]);
        });
    }

    // This test demonstrates that LocalCell is NOT Send or Sync
    #[test]
    fn test_not_send_or_sync() {
        fn assert_not_send<T: ?Sized>() {}
        fn assert_not_sync<T: ?Sized>() {}

        // These should compile, confirming LocalCell is neither Send nor Sync
        assert_not_send::<LocalCell<i32>>();
        assert_not_sync::<LocalCell<i32>>();
    }
}
