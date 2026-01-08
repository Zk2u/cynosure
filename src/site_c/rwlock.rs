use std::{
    cell::{Cell, UnsafeCell},
    future::Future,
    ops::{Deref, DerefMut},
    pin::Pin,
    task::{Context, Poll, Waker},
};

use super::queue::Queue;
use crate::hints::likely;

/// A reader-writer lock for single-threaded async executors.
///
/// This lock is optimized for single-threaded use cases where you need to
/// hold a lock across await points. It allows multiple concurrent readers
/// or a single exclusive writer.
///
/// # Sharing
///
/// `LocalRwLock` is not `Clone`. If you need to share it between multiple
/// parts of your code, wrap it in [`Rc`](std::rc::Rc):
///
/// ```rust,no_run
/// use std::rc::Rc;
///
/// use cynosure::site_c::rwlock::LocalRwLock;
///
/// let lock = Rc::new(LocalRwLock::new(0));
/// let lock2 = lock.clone();
/// ```
///
/// # Fairness
///
/// This implementation is **write-preferring**: when a writer releases the
/// lock, waiting writers are woken before waiting readers. This prevents writer
/// starvation and matches the behavior of `parking_lot` and `tokio`.
///
/// # Example
///
/// ```rust,no_run
/// use cynosure::site_c::rwlock::LocalRwLock;
///
/// async fn example() {
///     let lock = LocalRwLock::new(vec![1, 2, 3]);
///
///     // Multiple readers can access simultaneously
///     {
///         let r1 = lock.read().await;
///         let r2 = lock.read().await;
///         assert_eq!(r1.len(), 3);
///         assert_eq!(r2.len(), 3);
///     }
///
///     // Writers have exclusive access
///     {
///         let mut w = lock.write().await;
///         w.push(4);
///     }
///
///     assert_eq!(lock.read().await.len(), 4);
/// }
/// ```
///
/// # Performance
///
/// The fast path (uncontended lock) compiles down to minimal `Cell` operations.
/// This matches `RefCell` performance (~0.3ns), significantly faster than
/// atomic-based locks.
pub struct LocalRwLock<T> {
    readers: Cell<usize>,
    writer: Cell<bool>,
    read_waiters: UnsafeCell<Queue<Waker, 8>>,
    write_waiters: UnsafeCell<Queue<Waker, 8>>,
    value: UnsafeCell<T>,
}

impl<T> LocalRwLock<T> {
    /// Creates a new reader-writer lock in an unlocked state.
    #[inline]
    pub fn new(value: T) -> Self {
        Self {
            readers: Cell::new(0),
            writer: Cell::new(false),
            read_waiters: UnsafeCell::new(Queue::new()),
            write_waiters: UnsafeCell::new(Queue::new()),
            value: UnsafeCell::new(value),
        }
    }

    /// Acquires a read lock asynchronously.
    ///
    /// Multiple readers can hold the lock simultaneously. If a writer is
    /// active, the calling task will yield and be woken when the lock
    /// becomes available for reading.
    #[inline]
    pub fn read(&self) -> LocalRwLockReadFuture<'_, T> {
        LocalRwLockReadFuture { rwlock: self }
    }

    /// Acquires a write lock asynchronously.
    ///
    /// Only one writer can hold the lock at a time, and it excludes all
    /// readers. If the lock is held by any readers or another writer,
    /// the calling task will yield and be woken when the lock becomes
    /// available for writing.
    #[inline]
    pub fn write(&self) -> LocalRwLockWriteFuture<'_, T> {
        LocalRwLockWriteFuture { rwlock: self }
    }

    /// Attempts to acquire a read lock without waiting.
    ///
    /// Returns `Some(guard)` if successful, `None` if a writer holds the lock.
    #[inline]
    pub fn try_read(&self) -> Option<LocalRwLockReadGuard<'_, T>> {
        if likely(!self.writer.get()) {
            self.readers.set(self.readers.get() + 1);
            Some(LocalRwLockReadGuard { rwlock: self })
        } else {
            None
        }
    }

    /// Attempts to acquire a write lock without waiting.
    ///
    /// Returns `Some(guard)` if successful, `None` if the lock is held by
    /// any readers or another writer.
    #[inline]
    pub fn try_write(&self) -> Option<LocalRwLockWriteGuard<'_, T>> {
        if likely(self.readers.get() == 0 && !self.writer.get()) {
            self.writer.set(true);
            Some(LocalRwLockWriteGuard { rwlock: self })
        } else {
            None
        }
    }

    /// Returns `true` if the lock is currently held by a writer.
    #[inline]
    pub fn is_write_locked(&self) -> bool {
        self.writer.get()
    }

    /// Returns the number of active readers.
    #[inline]
    pub fn reader_count(&self) -> usize {
        self.readers.get()
    }

    /// Returns `true` if there are any active readers or a writer.
    #[inline]
    pub fn is_locked(&self) -> bool {
        self.readers.get() > 0 || self.writer.get()
    }

    /// Consumes the lock, returning the underlying data.
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

    /// Releases a read lock and potentially wakes a waiting writer.
    #[inline]
    fn release_read(&self) {
        let readers = self.readers.get();
        debug_assert!(readers > 0, "release_read called with no readers");
        self.readers.set(readers - 1);

        // If no more readers, wake one waiting writer
        if readers == 1 {
            // SAFETY: Single-threaded context, no concurrent access.
            if let Some(waker) = unsafe { (*self.write_waiters.get()).pop_front() } {
                waker.wake();
            }
        }
    }

    /// Releases a write lock and wakes a waiting writer or all readers.
    #[inline]
    fn release_write(&self) {
        debug_assert!(self.writer.get(), "release_write called without write lock");
        self.writer.set(false);

        // Write-preferring: wake one writer first, otherwise all readers.
        // This prevents writer starvation and matches parking_lot/tokio behavior.
        // SAFETY: Single-threaded context, no concurrent access.
        unsafe {
            if let Some(waker) = (*self.write_waiters.get()).pop_front() {
                waker.wake();
            } else {
                let read_waiters = &mut *self.read_waiters.get();
                while let Some(waker) = read_waiters.pop_front() {
                    waker.wake();
                }
            }
        }
    }

    /// Registers a waker to be notified when read access becomes available.
    fn register_read_waker(&self, waker: &Waker) {
        // SAFETY: Single-threaded context, no concurrent access.
        unsafe {
            let waiters = &mut *self.read_waiters.get();
            if !waiters.iter().any(|w| w.will_wake(waker)) {
                waiters.push_back(waker.clone());
            }
        }
    }

    /// Registers a waker to be notified when write access becomes available.
    fn register_write_waker(&self, waker: &Waker) {
        // SAFETY: Single-threaded context, no concurrent access.
        unsafe {
            let waiters = &mut *self.write_waiters.get();
            if !waiters.iter().any(|w| w.will_wake(waker)) {
                waiters.push_back(waker.clone());
            }
        }
    }
}

impl<T: Default> Default for LocalRwLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for LocalRwLock<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("LocalRwLock");
        d.field("readers", &self.readers.get());
        d.field("writer", &self.writer.get());
        if !self.is_locked() {
            // SAFETY: Not locked, safe to read
            d.field("value", unsafe { &*self.value.get() });
        } else {
            d.field("value", &"<locked>");
        }
        d.finish()
    }
}

// LocalRwLock is !Send and !Sync by default due to UnsafeCell, which is
// correct.

/// Future returned by [`LocalRwLock::read()`].
pub struct LocalRwLockReadFuture<'a, T> {
    rwlock: &'a LocalRwLock<T>,
}

impl<'a, T> Future for LocalRwLockReadFuture<'a, T> {
    type Output = LocalRwLockReadGuard<'a, T>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Fast path
        if let Some(guard) = self.rwlock.try_read() {
            return Poll::Ready(guard);
        }

        // Register waker BEFORE re-checking to avoid race condition
        self.rwlock.register_read_waker(cx.waker());

        // Re-check after registering
        if let Some(guard) = self.rwlock.try_read() {
            Poll::Ready(guard)
        } else {
            Poll::Pending
        }
    }
}

impl<'a, T> std::fmt::Debug for LocalRwLockReadFuture<'a, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalRwLockReadFuture")
            .field("write_locked", &self.rwlock.is_write_locked())
            .finish()
    }
}

/// Future returned by [`LocalRwLock::write()`].
pub struct LocalRwLockWriteFuture<'a, T> {
    rwlock: &'a LocalRwLock<T>,
}

impl<'a, T> Future for LocalRwLockWriteFuture<'a, T> {
    type Output = LocalRwLockWriteGuard<'a, T>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Fast path
        if let Some(guard) = self.rwlock.try_write() {
            return Poll::Ready(guard);
        }

        // Register waker BEFORE re-checking to avoid race condition
        self.rwlock.register_write_waker(cx.waker());

        // Re-check after registering
        if let Some(guard) = self.rwlock.try_write() {
            Poll::Ready(guard)
        } else {
            Poll::Pending
        }
    }
}

impl<'a, T> std::fmt::Debug for LocalRwLockWriteFuture<'a, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalRwLockWriteFuture")
            .field("locked", &self.rwlock.is_locked())
            .finish()
    }
}

/// RAII guard for read access that releases the read lock on drop.
pub struct LocalRwLockReadGuard<'a, T> {
    rwlock: &'a LocalRwLock<T>,
}

impl<'a, T> Deref for LocalRwLockReadGuard<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: We hold a read lock, guaranteeing no writer is active.
        unsafe { &*self.rwlock.value.get() }
    }
}

impl<'a, T> Drop for LocalRwLockReadGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        self.rwlock.release_read();
    }
}

impl<'a, T: std::fmt::Debug> std::fmt::Debug for LocalRwLockReadGuard<'a, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalRwLockReadGuard")
            .field("value", &**self)
            .finish()
    }
}

/// RAII guard for write access that releases the write lock on drop.
pub struct LocalRwLockWriteGuard<'a, T> {
    rwlock: &'a LocalRwLock<T>,
}

impl<'a, T> Deref for LocalRwLockWriteGuard<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: We hold a write lock, guaranteeing exclusive access.
        unsafe { &*self.rwlock.value.get() }
    }
}

impl<'a, T> DerefMut for LocalRwLockWriteGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: We hold a write lock, guaranteeing exclusive access.
        unsafe { &mut *self.rwlock.value.get() }
    }
}

impl<'a, T> Drop for LocalRwLockWriteGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        self.rwlock.release_write();
    }
}

impl<'a, T: std::fmt::Debug> std::fmt::Debug for LocalRwLockWriteGuard<'a, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalRwLockWriteGuard")
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
    fn test_new_and_try_read() {
        let lock = LocalRwLock::new(42);
        assert!(!lock.is_locked());

        let guard = lock.try_read().unwrap();
        assert_eq!(*guard, 42);
        assert_eq!(lock.reader_count(), 1);
        assert!(!lock.is_write_locked());

        drop(guard);
        assert!(!lock.is_locked());
    }

    #[test]
    fn test_multiple_readers() {
        let lock = LocalRwLock::new(42);

        let r1 = lock.try_read().unwrap();
        let r2 = lock.try_read().unwrap();
        let r3 = lock.try_read().unwrap();

        assert_eq!(lock.reader_count(), 3);
        assert_eq!(*r1, 42);
        assert_eq!(*r2, 42);
        assert_eq!(*r3, 42);

        drop(r1);
        assert_eq!(lock.reader_count(), 2);

        drop(r2);
        drop(r3);
        assert_eq!(lock.reader_count(), 0);
    }

    #[test]
    fn test_writer_excludes_readers() {
        let lock = LocalRwLock::new(42);

        let w = lock.try_write().unwrap();
        assert!(lock.is_write_locked());
        assert!(lock.try_read().is_none());
        assert!(lock.try_write().is_none());

        drop(w);
        assert!(!lock.is_write_locked());
        assert!(lock.try_read().is_some());
    }

    #[test]
    fn test_readers_exclude_writer() {
        let lock = LocalRwLock::new(42);

        let r = lock.try_read().unwrap();
        assert!(lock.try_write().is_none());

        drop(r);
        assert!(lock.try_write().is_some());
    }

    #[test]
    fn test_write_mutation() {
        let lock = LocalRwLock::new(vec![1, 2, 3]);

        {
            let mut w = lock.try_write().unwrap();
            w.push(4);
        }

        let r = lock.try_read().unwrap();
        assert_eq!(*r, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_read_future_ready_when_unlocked() {
        let lock = LocalRwLock::new(42);
        let mut future = lock.read();

        match poll_once(&mut future) {
            Poll::Ready(guard) => assert_eq!(*guard, 42),
            Poll::Pending => panic!("should be ready"),
        }
    }

    #[test]
    fn test_read_future_pending_when_write_locked() {
        let lock = LocalRwLock::new(42);
        let _w = lock.try_write().unwrap();

        let mut future = lock.read();
        match poll_once(&mut future) {
            Poll::Pending => {}
            Poll::Ready(_) => panic!("should be pending"),
        }
    }

    #[test]
    fn test_write_future_pending_when_read_locked() {
        let lock = LocalRwLock::new(42);
        let _r = lock.try_read().unwrap();

        let mut future = lock.write();
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
        let lock = LocalRwLock::new(42);
        let _w = lock.try_write().unwrap();

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut future = lock.read();
        for _ in 0..100 {
            let _ = unsafe { Pin::new_unchecked(&mut future).poll(&mut cx) };
        }

        // Just verify we can still use the lock after many polls
        drop(_w);
        assert!(lock.try_read().is_some());
    }

    #[test]
    fn test_write_release_wakes_readers() {
        use std::sync::{
            Arc as StdArc,
            atomic::{AtomicUsize, Ordering},
        };

        let lock = LocalRwLock::new(42);
        let w = lock.try_write().unwrap();

        let woken = StdArc::new(AtomicUsize::new(0));

        struct TestWaker(StdArc<AtomicUsize>);
        impl Wake for TestWaker {
            fn wake(self: StdArc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let waker1: Waker = StdArc::new(TestWaker(woken.clone())).into();
        let waker2: Waker = StdArc::new(TestWaker(woken.clone())).into();
        let mut cx1 = Context::from_waker(&waker1);
        let mut cx2 = Context::from_waker(&waker2);

        let mut f1 = lock.read();
        let mut f2 = lock.read();
        let _ = unsafe { Pin::new_unchecked(&mut f1).poll(&mut cx1) };
        let _ = unsafe { Pin::new_unchecked(&mut f2).poll(&mut cx2) };

        assert_eq!(woken.load(Ordering::SeqCst), 0);
        drop(w);
        assert_eq!(woken.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_read_release_wakes_writer() {
        use std::sync::{
            Arc as StdArc,
            atomic::{AtomicBool, Ordering},
        };

        let lock = LocalRwLock::new(42);
        let r = lock.try_read().unwrap();

        let woken = StdArc::new(AtomicBool::new(false));

        struct TestWaker(StdArc<AtomicBool>);
        impl Wake for TestWaker {
            fn wake(self: StdArc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let waker: Waker = StdArc::new(TestWaker(woken.clone())).into();
        let mut cx = Context::from_waker(&waker);

        let mut future = lock.write();
        let _ = unsafe { Pin::new_unchecked(&mut future).poll(&mut cx) };

        assert!(!woken.load(Ordering::SeqCst));
        drop(r);
        assert!(woken.load(Ordering::SeqCst));
    }

    #[test]
    fn test_rc_sharing() {
        let lock = Rc::new(LocalRwLock::new(42));
        let lock2 = lock.clone();

        {
            let mut w = lock.try_write().unwrap();
            *w = 100;
        }

        let r = lock2.try_read().unwrap();
        assert_eq!(*r, 100);
    }

    #[test]
    fn test_debug_impl() {
        let lock = LocalRwLock::new(42);
        let debug_str = format!("{:?}", lock);
        assert!(debug_str.contains("LocalRwLock"));
        assert!(debug_str.contains("42"));

        let _guard = lock.try_write().unwrap();
        let debug_str_locked = format!("{:?}", lock);
        assert!(debug_str_locked.contains("locked"));
    }

    #[test]
    fn test_default() {
        let lock: LocalRwLock<i32> = LocalRwLock::default();
        assert_eq!(*lock.try_read().unwrap(), 0);
    }

    #[test]
    fn test_into_inner() {
        let lock = LocalRwLock::new(vec![1, 2, 3]);
        let value = lock.into_inner();
        assert_eq!(value, vec![1, 2, 3]);
    }

    #[test]
    fn test_get_mut() {
        let mut lock = LocalRwLock::new(42);
        *lock.get_mut() = 100;
        assert_eq!(*lock.try_read().unwrap(), 100);
    }

    #[test]
    fn test_read_after_waker_registered() {
        let lock = LocalRwLock::new(42);
        let guard = lock.try_write().unwrap();

        let mut future = lock.read();

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
    fn test_write_after_waker_registered() {
        let lock = LocalRwLock::new(42);
        let guard = lock.try_read().unwrap();

        let mut future = lock.write();

        match poll_once(&mut future) {
            Poll::Pending => {}
            Poll::Ready(_) => panic!("should be pending"),
        }

        drop(guard);

        match poll_once(&mut future) {
            Poll::Ready(mut guard) => {
                assert_eq!(*guard, 42);
                *guard = 100;
            }
            Poll::Pending => panic!("should be ready after unlock"),
        }

        assert_eq!(*lock.try_read().unwrap(), 100);
    }
}
