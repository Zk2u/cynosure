//! Various cell-like types for single-threaded async executors.

use std::{cell::UnsafeCell, rc::Rc};

/// ScopedCell is a [RefCell](std::cell::RefCell)-like type that allows for mutable access to its value
/// within a scoped closure. This is useful for usecases in a single-threaded,
/// async executor where you have multiple futures sharing the same data,
/// wanting to mutate it without needing to hold a mutable reference to it over an await point.
/// ScopedCell usually has identical or slightly better performance than RefCell,
/// but with a slightly different access pattern which is more ergonomic for single-threaded async code.
///
/// **IMPORTANT**: You must NOT access a `ScopedCell` through multiple clones
/// simultaneously (e.g., calling `with()` on one clone while inside a
/// `with_mut()` closure on another clone that points to the same data). This
/// violates Rust's aliasing rules and results in undefined behavior.
///
/// ```rust,no_run
/// use cynosure::site_c::cell::ScopedCell;
///
/// let cell = ScopedCell::rc(vec![1, 2, 3]);
/// let cell2 = cell.clone();
///
/// // This is UNSOUND and will cause undefined behavior:
/// cell.with_mut(|v| {
///     let len = cell2.with(|v2| v2.len()); // <- Aliasing violation!
///     v.push(len as i32);
/// });
/// ```
pub struct ScopedCell<T> {
    value: UnsafeCell<T>,
}

impl<T> ScopedCell<T> {
    pub fn new(value: T) -> Self {
        Self {
            value: value.into(),
        }
    }

    /// Shortcut for creating a new `ScopedCell` inside a reference-counted pointer.
    pub fn rc(value: T) -> Rc<Self> {
        Rc::new(Self::new(value))
    }

    /// Executes a closure with a reference to the inner value.
    pub fn with<R, F>(&self, f: F) -> R
    where
        F: FnOnce(&T) -> R,
    {
        f(unsafe { &*self.value.get() })
    }

    /// Executes a closure with a mutable reference to the inner value.
    pub fn with_mut<R, F>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        f(unsafe { &mut *self.value.get() })
    }

    /// Consumes the ScopedCell, returning the inner value if it is the only
    /// reference, or an error if there are other references.
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for ScopedCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.with(|value| f.debug_struct("ScopedCell").field("value", value).finish())
    }
}

// #[cfg(test)]
// mod tests {
//     use std::{
//         sync::atomic::{AtomicUsize, Ordering},
//         time::Duration,
//     };

//     use super::*;

//     #[test]
//     fn test_new_and_basic_access() {
//         let cell = ScopedCell::new(42);

//         cell.with(|value| {
//             assert_eq!(*value, 42);
//         });
//     }

//     #[test]
//     fn test_mutable_access() {
//         let cell = ScopedCell::new(vec![1, 2, 3]);

//         cell.with_mut(|vec| {
//             vec.push(4);
//         });

//         cell.with(|vec| {
//             assert_eq!(vec, &[1, 2, 3, 4]);
//         });
//     }

//     #[test]
//     fn test_clone_shares_data() {
//         let cell1 = ScopedCell::rc(String::from("hello"));
//         let cell2 = cell1.clone();

//         // Mutate through cell1
//         cell1.with_mut(|s| {
//             s.push_str(" world");
//         });

//         // Verify change is visible through cell2
//         cell2.with(|s| {
//             assert_eq!(s, "hello world");
//         });
//     }

//     #[test]
//     fn test_multiple_clones() {
//         let cell = ScopedCell::rc(0);
//         let cells: Vec<_> = (0..10).map(|_| cell.clone()).collect();

//         // Mutate through original
//         cell.with_mut(|value| {
//             *value = 100;
//         });

//         // Verify all clones see the change
//         for (i, cell) in cells.iter().enumerate() {
//             cell.with(|value| {
//                 assert_eq!(*value, 100, "Clone {} should see updated value", i);
//             });
//         }
//     }

//     #[test]
//     fn test_drop_tracking() {
//         // Use a custom type that tracks drops
//         static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

//         struct DropTracker;
//         impl Drop for DropTracker {
//             fn drop(&mut self) {
//                 DROP_COUNT.fetch_add(1, Ordering::SeqCst);
//             }
//         }

//         DROP_COUNT.store(0, Ordering::SeqCst);

//         {
//             let cell1 = ScopedCell::rc(DropTracker);
//             let cell2 = cell1.clone();
//             let _cell3 = cell2.clone();

//             assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);
//             drop(cell1);
//             assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);
//             drop(cell2);
//             assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);
//             // cell3 dropped here
//         }

//         assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);
//     }

//     #[test]
//     fn test_concurrent_mutations() {
//         // Even though this is for single-threaded use, test that
//         // multiple mutations work correctly
//         let cell = ScopedCell::rc(vec![1]);
//         let cell2 = cell.clone();

//         cell.with_mut(|v| v.push(2));
//         cell2.with_mut(|v| v.push(3));
//         cell.with_mut(|v| v.push(4));

//         cell.with(|v| {
//             assert_eq!(v, &[1, 2, 3, 4]);
//         });
//     }

//     #[test]
//     fn test_closure_return_values() {
//         let cell = ScopedCell::new(vec![1, 2, 3]);

//         let sum = cell.with(|v| v.iter().sum::<i32>());
//         assert_eq!(sum, 6);

//         let new_vec = cell.with_mut(|v| {
//             v.push(4);
//             v.clone()
//         });
//         assert_eq!(new_vec, vec![1, 2, 3, 4]);
//     }

//     #[test]
//     fn test_nested_access() {
//         let cell1 = ScopedCell::new(10);
//         let cell2 = ScopedCell::new(20);

//         let result = cell1.with(|a| cell2.with(|b| *a + *b));

//         assert_eq!(result, 30);
//     }

//     // Async tests to demonstrate the use case
//     #[monoio::test(timer_enabled = true)]
//     async fn test_async_no_await_in_closure() {
//         let cell = ScopedCell::rc(vec![1, 2, 3]);
//         let cell2 = cell.clone();
//         let cell3 = cell.clone();

//         let handles = [
//             monoio::spawn(async move {
//                 let duration_to_sleep = Duration::from_millis(10);
//                 monoio::time::sleep(duration_to_sleep).await;
//                 cell2.with_mut(|vec| vec.push(4))
//             }),
//             monoio::spawn(async move {
//                 let duration_to_sleep = Duration::from_millis(10);
//                 monoio::time::sleep(duration_to_sleep).await;
//                 cell3.with_mut(|vec| vec.push(5))
//             }),
//         ];

//         for handle in handles {
//             handle.await;
//         }

//         cell.with_mut(|v| {
//             assert!(v.contains(&4));
//             assert!(v.contains(&5));
//         });
//     }

//     #[monoio::test(timer_enabled = true)]
//     async fn test_async_shared_state() {
//         let counter = ScopedCell::rc(0);

//         let mut handles = vec![];

//         for i in 0..5 {
//             let counter = counter.clone();
//             let handle = monoio::spawn(async move {
//                 // Safe mutation within sync closure
//                 counter.with_mut(|c| {
//                     *c += i;
//                 });
//             });
//             handles.push(handle);
//         }

//         for handle in handles {
//             handle.await;
//         }

//         counter.with(|c| {
//             assert_eq!(*c, 0 + 1 + 2 + 3 + 4);
//         });
//     }

//     #[test]
//     fn test_zst_type() {
//         // Test with zero-sized type
//         let cell = ScopedCell::rc(());
//         let cell2 = cell.clone();

//         cell.with(|_| {});
//         cell2.with_mut(|_| {});
//     }

//     #[test]
//     fn test_large_clone_count() {
//         let cell = ScopedCell::rc(42);
//         let mut cells = vec![cell];

//         // Create many clones
//         for _ in 0..1000 {
//             cells.push(cells.last().unwrap().clone());
//         }

//         // Drop them in reverse order
//         while let Some(cell) = cells.pop() {
//             drop(cell);
//         }

//         // If we get here without panic, reference counting works
//     }

//     #[test]
//     fn test_complex_type() {
//         use std::collections::HashMap;

//         let mut map = HashMap::new();
//         map.insert("key", vec![1, 2, 3]);

//         let cell = ScopedCell::new(map);

//         cell.with_mut(|map| {
//             map.get_mut("key").unwrap().push(4);
//         });

//         cell.with(|map| {
//             assert_eq!(map.get("key").unwrap(), &vec![1, 2, 3, 4]);
//         });
//     }

//     // This test demonstrates that ScopedCell is NOT Send or Sync
//     #[test]
//     fn test_not_send_or_sync() {
//         fn assert_not_send<T: ?Sized>() {}
//         fn assert_not_sync<T: ?Sized>() {}

//         // These should compile, confirming ScopedCell is neither Send nor Sync
//         assert_not_send::<ScopedCell<i32>>();
//         assert_not_sync::<ScopedCell<i32>>();
//     }

//     #[test]
//     fn test_into_inner_single_reference() {
//         let cell = ScopedCell::new(vec![1, 2, 3]);

//         let inner = cell.into_inner();
//         assert_eq!(inner, vec![1, 2, 3]);
//     }

//     #[test]
//     fn test_debug_implementation() {
//         let cell = ScopedCell::new(42);
//         let debug_output = format!("{:?}", cell);

//         // Should contain the struct name, count, and value
//         assert!(debug_output.contains("ScopedCell"));
//         assert!(debug_output.contains("value: 42"));
//     }

//     #[test]
//     fn test_debug_with_multiple_references() {
//         let cell = ScopedCell::rc(String::from("hello"));
//         let cell2 = cell.clone();

//         let debug_output = format!("{:?}", cell);

//         // Should show count of 2 and the string value
//         assert!(debug_output.contains("ScopedCell"));
//         assert!(debug_output.contains("hello"));

//         // Both references should show the same debug info
//         let debug_output2 = format!("{:?}", cell2);
//         assert_eq!(debug_output, debug_output2);
//     }

//     #[test]
//     fn test_debug_with_complex_type() {
//         use std::collections::HashMap;

//         let mut map = HashMap::new();
//         map.insert("key", vec![1, 2, 3]);

//         let cell = ScopedCell::new(map);
//         let debug_output = format!("{:?}", cell);

//         // Should contain the struct info and nested data
//         assert!(debug_output.contains("ScopedCell"));
//         assert!(debug_output.contains("key"));
//     }

//     #[test]
//     fn test_debug_output_format() {
//         let cell = ScopedCell::rc(vec![1, 2, 3]);
//         let cell2 = cell.clone();

//         println!("Single reference: {:?}", ScopedCell::new(42));
//         println!("Multiple references: {:?}", cell);
//         println!(
//             "Complex type: {:?}",
//             ScopedCell::new(std::collections::HashMap::from([("key", "value")]))
//         );

//         // Just verify it doesn't panic
//         assert!(format!("{:?}", cell2).len() > 0);
//     }
// }
