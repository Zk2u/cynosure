use std::{
    cell::{Cell, UnsafeCell},
    future::Future,
    ops::{Deref, DerefMut},
    pin::Pin,
    task::{Context, Poll, Waker},
};

use super::queue::Queue;
use crate::hints::{likely, unlikely};

/// A mutual exclusion primitive for single-threaded async executors.
///
/// This mutex is optimized for single-threaded use cases where you need to
/// hold a lock across await points. It has minimal overhead because it uses
/// non-atomic operations.
///
/// # Sharing
///
/// `LocalMutex` is not `Clone`. If you need to share it between multiple
/// parts of your code, wrap it in [`Rc`](std::rc::Rc):
///
/// ```rust,no_run
/// use std::rc::Rc;
///
/// use cynosure::site_c::mutex::LocalMutex;
///
/// let mutex = Rc::new(LocalMutex::new(0));
/// let mutex2 = mutex.clone();
/// ```
///
/// # Example
///
/// ```rust,no_run
/// use cynosure::site_c::mutex::LocalMutex;
///
/// async fn example() {
///     let mutex = LocalMutex::new(0);
///
///     {
///         let mut guard = mutex.lock().await;
///         *guard += 1;
///     } // Lock is released here
///
///     assert_eq!(*mutex.lock().await, 1);
/// }
/// ```
///
/// # Performance
///
/// The fast path (uncontended lock) compiles down to:
/// - One read-modify-write on a `Cell<bool>`
/// - Return guard
///
/// This matches `RefCell::borrow_mut` performance (~0.3ns), significantly
/// faster than atomic-based mutexes.
pub struct LocalMutex<T> {
    locked: Cell<bool>,
    waiters: UnsafeCell<Queue<Waker, 8>>,
    value: UnsafeCell<T>,
}

impl<T> LocalMutex<T> {
    /// Creates a new mutex in an unlocked state.
    #[inline]
    pub fn new(value: T) -> Self {
        Self {
            locked: Cell::new(false),
            waiters: UnsafeCell::new(Queue::new()),
            value: UnsafeCell::new(value),
        }
    }

    /// Acquires the mutex, returning a guard that releases it on drop.
    ///
    /// If the mutex is already locked, the calling task will yield and
    /// be woken when the mutex becomes available.
    #[inline]
    pub fn lock(&self) -> LocalMutexLockFuture<'_, T> {
        LocalMutexLockFuture { mutex: self }
    }

    /// Attempts to acquire the mutex without waiting.
    ///
    /// Returns `Some(guard)` if successful, `None` if already locked.
    #[inline]
    pub fn try_lock(&self) -> Option<LocalMutexGuard<'_, T>> {
        if likely(!self.locked.replace(true)) {
            Some(LocalMutexGuard { mutex: self })
        } else {
            None
        }
    }

    /// Returns true if the mutex is currently locked.
    #[inline]
    pub fn is_locked(&self) -> bool {
        self.locked.get()
    }

    /// Consumes the mutex, returning the underlying data.
    #[inline]
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this requires `&mut self`, no locking is needed.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    /// Releases the lock and wakes all waiting tasks.
    #[inline]
    fn unlock(&self) {
        self.locked.set(false);

        // SAFETY: Single-threaded context, no concurrent access.
        let waiters = unsafe { &mut *self.waiters.get() };
        // Fast path: skip when no waiters (common case)
        if unlikely(!waiters.is_empty()) {
            // Drain in place - avoids allocation from mem::take
            while let Some(waker) = waiters.pop_front() {
                waker.wake();
            }
        }
    }

    /// Registers a waker to be notified when the lock becomes available.
    fn register_waker(&self, waker: &Waker) {
        // SAFETY: Single-threaded context, no concurrent access.
        unsafe {
            let waiters = &mut *self.waiters.get();
            // Deduplicate to prevent unbounded growth
            if !waiters.iter().any(|w| w.will_wake(waker)) {
                waiters.push_back(waker.clone());
            }
        }
    }
}

impl<T: Default> Default for LocalMutex<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for LocalMutex<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("LocalMutex");
        d.field("locked", &self.locked.get());
        if !self.locked.get() {
            // SAFETY: Not locked, safe to read
            d.field("value", unsafe { &*self.value.get() });
        } else {
            d.field("value", &"<locked>");
        }
        d.finish()
    }
}

// LocalMutex is !Send and !Sync by default due to UnsafeCell, which is correct.

/// Future returned by [`LocalMutex::lock()`].
pub struct LocalMutexLockFuture<'a, T> {
    mutex: &'a LocalMutex<T>,
}

impl<'a, T> Future for LocalMutexLockFuture<'a, T> {
    type Output = LocalMutexGuard<'a, T>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Fast path - uncontended lock is the common case
        if let Some(guard) = self.mutex.try_lock() {
            return Poll::Ready(guard);
        }

        // Slow path: contended
        // Register waker BEFORE re-checking to avoid race condition
        self.mutex.register_waker(cx.waker());

        // Re-check after registering (unlikely to succeed if we got here)
        match self.mutex.try_lock() {
            Some(guard) => Poll::Ready(guard),
            None => Poll::Pending,
        }
    }
}

impl<'a, T> std::fmt::Debug for LocalMutexLockFuture<'a, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalMutexLockFuture")
            .field("locked", &self.mutex.is_locked())
            .finish()
    }
}

/// RAII guard that releases the mutex on drop.
pub struct LocalMutexGuard<'a, T> {
    mutex: &'a LocalMutex<T>,
}

impl<'a, T> Deref for LocalMutexGuard<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: We hold the lock, guaranteeing exclusive access.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<'a, T> DerefMut for LocalMutexGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: We hold the lock, guaranteeing exclusive access.
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<'a, T> Drop for LocalMutexGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

impl<'a, T: std::fmt::Debug> std::fmt::Debug for LocalMutexGuard<'a, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalMutexGuard")
            .field("value", &**self)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        rc::Rc,
        task::{Context, Poll, Wake, Waker},
    };

    use super::*;

    struct NoopWaker;
    impl Wake for NoopWaker {
        fn wake(self: std::sync::Arc<Self>) {}
    }

    fn noop_waker() -> Waker {
        std::sync::Arc::new(NoopWaker).into()
    }

    fn poll_once<F: Future>(f: &mut F) -> Poll<F::Output> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        unsafe { Pin::new_unchecked(f).poll(&mut cx) }
    }

    #[test]
    fn test_new_and_try_lock() {
        let mutex = LocalMutex::new(42);
        assert!(!mutex.is_locked());

        let guard = mutex.try_lock().unwrap();
        assert!(mutex.is_locked());
        assert_eq!(*guard, 42);

        drop(guard);
        assert!(!mutex.is_locked());
    }

    #[test]
    fn test_try_lock_fails_when_locked() {
        let mutex = LocalMutex::new(42);
        let _guard = mutex.try_lock().unwrap();
        assert!(mutex.try_lock().is_none());
    }

    #[test]
    fn test_lock_future_ready_when_unlocked() {
        let mutex = LocalMutex::new(42);
        let mut future = mutex.lock();

        match poll_once(&mut future) {
            Poll::Ready(guard) => assert_eq!(*guard, 42),
            Poll::Pending => panic!("should be ready"),
        }
    }

    #[test]
    fn test_lock_future_pending_when_locked() {
        let mutex = LocalMutex::new(42);
        let _guard = mutex.try_lock().unwrap();

        let mut future = mutex.lock();
        match poll_once(&mut future) {
            Poll::Pending => {}
            Poll::Ready(_) => panic!("should be pending"),
        }
    }

    #[test]
    fn test_repeated_polling_doesnt_panic() {
        // Test that repeatedly polling a pending lock future is safe
        // and doesn't cause unbounded growth (waker deduplication).
        // Note: We don't assert exact waker count as will_wake behavior
        // varies under Miri.
        let mutex = LocalMutex::new(42);
        let _guard = mutex.try_lock().unwrap();

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut future = mutex.lock();
        for _ in 0..100 {
            let _ = unsafe { Pin::new_unchecked(&mut future).poll(&mut cx) };
        }

        // Just verify we can still use the mutex after many polls
        drop(_guard);
        assert!(mutex.try_lock().is_some());
    }

    #[test]
    fn test_unlock_wakes_waiters() {
        use std::sync::{
            Arc as StdArc,
            atomic::{AtomicBool, Ordering},
        };

        let mutex = LocalMutex::new(42);
        let guard = mutex.try_lock().unwrap();

        let woken = StdArc::new(AtomicBool::new(false));
        let woken_clone = woken.clone();

        struct TestWaker(StdArc<AtomicBool>);
        impl Wake for TestWaker {
            fn wake(self: StdArc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let waker: Waker = StdArc::new(TestWaker(woken_clone)).into();
        let mut cx = Context::from_waker(&waker);

        let mut future = mutex.lock();
        let _ = unsafe { Pin::new_unchecked(&mut future).poll(&mut cx) };

        assert!(!woken.load(Ordering::SeqCst));
        drop(guard);
        assert!(woken.load(Ordering::SeqCst));
    }

    #[test]
    fn test_mutex_with_mutation() {
        let mutex = LocalMutex::new(vec![1, 2, 3]);

        {
            let mut guard = mutex.try_lock().unwrap();
            guard.push(4);
        }

        let guard = mutex.try_lock().unwrap();
        assert_eq!(*guard, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_rc_sharing() {
        let mutex = Rc::new(LocalMutex::new(42));
        let mutex2 = mutex.clone();

        {
            let mut guard = mutex.try_lock().unwrap();
            *guard = 100;
        }

        let guard = mutex2.try_lock().unwrap();
        assert_eq!(*guard, 100);
    }

    #[test]
    fn test_debug_impl() {
        let mutex = LocalMutex::new(42);
        let debug_str = format!("{:?}", mutex);
        assert!(debug_str.contains("LocalMutex"));
        assert!(debug_str.contains("42"));

        let _guard = mutex.try_lock().unwrap();
        let debug_str_locked = format!("{:?}", mutex);
        assert!(debug_str_locked.contains("locked"));
    }

    #[test]
    fn test_default() {
        let mutex: LocalMutex<i32> = LocalMutex::default();
        assert_eq!(*mutex.try_lock().unwrap(), 0);
    }

    #[test]
    fn test_into_inner() {
        let mutex = LocalMutex::new(vec![1, 2, 3]);
        let value = mutex.into_inner();
        assert_eq!(value, vec![1, 2, 3]);
    }

    #[test]
    fn test_get_mut() {
        let mut mutex = LocalMutex::new(42);
        *mutex.get_mut() = 100;
        assert_eq!(*mutex.try_lock().unwrap(), 100);
    }

    #[test]
    fn test_lock_after_waker_registered() {
        let mutex = LocalMutex::new(42);
        let guard = mutex.try_lock().unwrap();

        let mut future = mutex.lock();

        match poll_once(&mut future) {
            Poll::Pending => {}
            Poll::Ready(_) => panic!("should be pending"),
        }

        drop(guard);

        match poll_once(&mut future) {
            Poll::Ready(guard) => assert_eq!(*guard, 42),
            Poll::Pending => panic!("should be ready after unlock"),
        }
    }

    #[test]
    fn test_multiple_sequential_locks() {
        let mutex = LocalMutex::new(0);

        for i in 0..100 {
            let mut guard = mutex.try_lock().unwrap();
            assert_eq!(*guard, i);
            *guard += 1;
        }

        assert_eq!(*mutex.try_lock().unwrap(), 100);
    }
}
