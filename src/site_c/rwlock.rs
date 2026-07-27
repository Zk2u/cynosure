//! `LocalRwLock` — a single-threaded async reader-writer lock, write-preferring.
//!
//! Non-atomic like [`LocalMutex`](super::mutex), and likewise held across
//! `.await`.

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
/// This implementation is **write-preferring**, like `parking_lot`: while a
/// writer is queued, new `read().await` acquisitions defer to it (and on
/// release a waiting writer is woken before any readers), so writers are not
/// starved by a stream of readers. The trade-off is that readers may be starved
/// under a continuous stream of writers. The non-blocking [`try_read`] remains
/// opportunistic and ignores queued writers.
///
/// [`try_read`]: LocalRwLock::try_read
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
    // `(id, waker)`: the `id` is a per-future token (NOT the waker), so a
    // future's entry can be removed unambiguously even when two futures of this
    // lock are driven by one task (and thus share a waker).
    read_waiters: UnsafeCell<Queue<(u64, Waker), 8>>,
    write_waiters: UnsafeCell<Queue<(u64, Waker), 8>>,
    next_id: Cell<u64>,
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
            next_id: Cell::new(0),
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
        LocalRwLockReadFuture {
            rwlock: self,
            registered: None,
        }
    }

    /// Acquires a write lock asynchronously.
    ///
    /// Only one writer can hold the lock at a time, and it excludes all
    /// readers. If the lock is held by any readers or another writer,
    /// the calling task will yield and be woken when the lock becomes
    /// available for writing.
    #[inline]
    pub fn write(&self) -> LocalRwLockWriteFuture<'_, T> {
        LocalRwLockWriteFuture {
            rwlock: self,
            registered: None,
        }
    }

    /// Attempts to acquire a read lock without waiting.
    ///
    /// Returns `Some(guard)` if successful, `None` if a writer holds the lock.
    ///
    /// This is *opportunistic*: it succeeds whenever no writer is active, even
    /// if a writer is queued. The async [`read`](Self::read) path, by contrast,
    /// is write-preferring and defers to queued writers.
    #[inline]
    pub fn try_read(&self) -> Option<LocalRwLockReadGuard<'_, T>> {
        if likely(!self.writer.get()) {
            self.readers.set(self.readers.get() + 1);
            Some(LocalRwLockReadGuard { rwlock: self })
        } else {
            None
        }
    }

    /// Write-preferring read acquisition used by the async `read()` future:
    /// succeeds only if no writer is active AND none is queued, so a waiting
    /// writer is not starved by a stream of new readers.
    #[inline]
    fn try_read_fair(&self) -> Option<LocalRwLockReadGuard<'_, T>> {
        // SAFETY: single-threaded; transient borrow.
        let no_writer_queued = unsafe { (*self.write_waiters.get()).is_empty() };
        if !self.writer.get() && no_writer_queued {
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

    /// Releases a read lock.
    #[inline]
    fn release_read(&self) {
        let readers = self.readers.get();
        debug_assert!(readers > 0, "release_read called with no readers");
        self.readers.set(readers - 1);
        self.wake_next();
    }

    /// Releases a write lock.
    #[inline]
    fn release_write(&self) {
        debug_assert!(self.writer.get(), "release_write called without write lock");
        self.writer.set(false);
        self.wake_next();
    }

    /// Wakes the next eligible waiter(s), but only if the lock is fully free.
    ///
    /// Write-preferring: a single waiting writer is woken if any; otherwise all
    /// waiting readers are woken (they can share). Called on every release and
    /// on cancellation of a pending future, so a cancelled-but-notified waiter
    /// passes the turn to the next one. Because cancelled futures deregister
    /// their wakers, a non-empty `write_waiters` always means a *live* writer is
    /// queued.
    #[inline]
    fn wake_next(&self) {
        if self.writer.get() || self.readers.get() > 0 {
            return; // lock still held; the holder(s) will wake on release
        }
        // SAFETY: single-threaded; each borrow lives only for its pop, so no
        // borrow is held across the re-entrant `wake()` callback.
        let next_writer = unsafe { (*self.write_waiters.get()).pop_front() };
        if let Some((_, waker)) = next_writer {
            waker.wake();
            return;
        }
        loop {
            let next = unsafe { (*self.read_waiters.get()).pop_front() };
            let Some((_, waker)) = next else { break };
            waker.wake();
        }
    }
}

/// Registers (or refreshes) a future's `(id, waker)` in `cell`, tracking it in
/// `slot` so exactly that future's entry can be removed on cancellation. Keeps
/// at most one entry per live future, so no stale (cancelled) wakers accumulate
/// and a non-empty queue reliably means a live waiter. The `id` (not the waker)
/// is the identity, so two futures of this lock sharing one task's waker get
/// distinct, independently-removable entries.
fn register_in(
    cell: &UnsafeCell<Queue<(u64, Waker), 8>>,
    next_id: &Cell<u64>,
    slot: &mut Option<(u64, Waker)>,
    waker: &Waker,
) {
    // SAFETY: single-threaded; no borrow held across a callback.
    let q = unsafe { &mut *cell.get() };
    // Fast path: skip re-registering only if our entry is *still queued* with an
    // equivalent waker. (`wake_next` pops the woken entry, so after a
    // wake-then-barge we must fall through and re-add it.)
    if let Some((id, w)) = slot.as_ref()
        && w.will_wake(waker)
        && q.iter().any(|(qid, _)| qid == id)
    {
        return;
    }
    let id = match slot {
        Some((id, _)) => *id,
        None => {
            let id = next_id.get();
            next_id.set(id.wrapping_add(1));
            id
        }
    };
    q.retain(|(qid, _)| *qid != id);
    q.push_back((id, waker.clone()));
    *slot = Some((id, waker.clone()));
}

/// Removes a future's entry from `cell` (on cancel/acquire).
fn deregister_in(cell: &UnsafeCell<Queue<(u64, Waker), 8>>, slot: &mut Option<(u64, Waker)>) {
    if let Some((id, _)) = slot.take() {
        // SAFETY: single-threaded; no borrow held across a callback.
        let q = unsafe { &mut *cell.get() };
        q.retain(|(qid, _)| *qid != id);
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
    /// `(id, waker)` entry in `read_waiters`, tracked for removal on cancel.
    registered: Option<(u64, Waker)>,
}

impl<'a, T> Future for LocalRwLockReadFuture<'a, T> {
    type Output = LocalRwLockReadGuard<'a, T>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut(); // future is Unpin

        // Fast path (write-preferring: defers to queued writers).
        if let Some(guard) = this.rwlock.try_read_fair() {
            deregister_in(&this.rwlock.read_waiters, &mut this.registered);
            return Poll::Ready(guard);
        }

        // Register, then re-check.
        register_in(
            &this.rwlock.read_waiters,
            &this.rwlock.next_id,
            &mut this.registered,
            cx.waker(),
        );
        if let Some(guard) = this.rwlock.try_read_fair() {
            deregister_in(&this.rwlock.read_waiters, &mut this.registered);
            Poll::Ready(guard)
        } else {
            Poll::Pending
        }
    }
}

impl<'a, T> Drop for LocalRwLockReadFuture<'a, T> {
    fn drop(&mut self) {
        if self.registered.is_some() {
            deregister_in(&self.rwlock.read_waiters, &mut self.registered);
            self.rwlock.wake_next();
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
    /// `(id, waker)` entry in `write_waiters`, tracked for removal on cancel.
    registered: Option<(u64, Waker)>,
}

impl<'a, T> Future for LocalRwLockWriteFuture<'a, T> {
    type Output = LocalRwLockWriteGuard<'a, T>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut(); // future is Unpin

        // Fast path
        if let Some(guard) = this.rwlock.try_write() {
            deregister_in(&this.rwlock.write_waiters, &mut this.registered);
            return Poll::Ready(guard);
        }

        // Register, then re-check.
        register_in(
            &this.rwlock.write_waiters,
            &this.rwlock.next_id,
            &mut this.registered,
            cx.waker(),
        );
        if let Some(guard) = this.rwlock.try_write() {
            deregister_in(&this.rwlock.write_waiters, &mut this.registered);
            Poll::Ready(guard)
        } else {
            Poll::Pending
        }
    }
}

impl<'a, T> Drop for LocalRwLockWriteFuture<'a, T> {
    fn drop(&mut self) {
        if self.registered.is_some() {
            deregister_in(&self.rwlock.write_waiters, &mut self.registered);
            self.rwlock.wake_next();
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

    // --- cancellation / fairness regression tests ---

    use std::sync::atomic::{AtomicBool, Ordering as AO};

    struct FlagWaker(std::sync::Arc<AtomicBool>);
    impl Wake for FlagWaker {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.store(true, AO::SeqCst);
        }
        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.0.store(true, AO::SeqCst);
        }
    }
    fn flag_waker() -> (Waker, std::sync::Arc<AtomicBool>) {
        let f = std::sync::Arc::new(AtomicBool::new(false));
        (std::sync::Arc::new(FlagWaker(f.clone())).into(), f)
    }
    fn poll_with(fut: &mut (impl Future + Unpin), w: &Waker) -> Poll<()> {
        let mut cx = Context::from_waker(w);
        Pin::new(fut).poll(&mut cx).map(|_| ())
    }

    /// Regression for the writer-cancellation deadlock: a cancelled queued
    /// writer no longer strands a live one — its waker is removed on drop, so
    /// release wakes the live writer.
    #[test]
    fn rwlock_writer_cancellation_recovers() {
        let lock = LocalRwLock::new(0i32);
        let r = lock.try_read().unwrap(); // readers = 1

        // W1 registers, then is cancelled (dropped) -> its waker is removed.
        let (w1_waker, _w1) = flag_waker();
        {
            let mut w1 = lock.write();
            assert!(poll_with(&mut w1, &w1_waker).is_pending());
        }

        // W2 registers and stays live.
        let (w2_waker, w2_flag) = flag_waker();
        let mut w2 = lock.write();
        assert!(poll_with(&mut w2, &w2_waker).is_pending());

        drop(r); // release: must wake the live W2 (not the removed W1)
        assert!(
            w2_flag.load(AO::SeqCst),
            "live writer W2 must be woken after the read lock is released"
        );
        // And W2 can now acquire.
        match poll_with(&mut w2, &w2_waker) {
            Poll::Ready(()) => {}
            Poll::Pending => panic!("W2 should acquire the now-free lock"),
        }
    }

    /// If every woken writer is cancelled, the turn passes through to waiting
    /// readers (torch-passing on drop), so readers aren't stranded.
    #[test]
    fn rwlock_cancelled_writer_passes_turn_to_readers() {
        let lock = LocalRwLock::new(0i32);
        let r = lock.try_read().unwrap();

        // One queued writer and one queued reader (reader defers, write-pref).
        let (ww, _wf) = flag_waker();
        let mut w = lock.write();
        assert!(poll_with(&mut w, &ww).is_pending());

        let (rw, r_flag) = flag_waker();
        let mut rd = lock.read();
        assert!(poll_with(&mut rd, &rw).is_pending());

        drop(r); // wakes the writer W
        // Now cancel the writer before it acquires; its drop must pass the turn
        // to the waiting reader.
        drop(w);
        assert!(
            r_flag.load(AO::SeqCst),
            "reader must be woken once the only writer is cancelled"
        );
    }

    /// Write-preferring: a new `read().await` defers while a writer is queued.
    #[test]
    fn rwlock_read_defers_to_queued_writer() {
        let lock = LocalRwLock::new(0i32);
        let _r = lock.try_read().unwrap(); // a reader holds

        let (ww, _wf) = flag_waker();
        let mut w = lock.write();
        assert!(poll_with(&mut w, &ww).is_pending()); // writer queued

        // A new async reader must NOT jump ahead of the queued writer.
        let (rw, _rf) = flag_waker();
        let mut rd = lock.read();
        assert!(
            poll_with(&mut rd, &rw).is_pending(),
            "read().await must defer to a queued writer (write-preferring)"
        );
        // ...though the opportunistic try_read still succeeds.
        assert!(lock.try_read().is_some());
    }

    /// Two writer futures of the SAME lock driven by ONE task (shared waker):
    /// when the first acquires and deregisters, the second must NOT be stranded.
    #[test]
    fn rwlock_two_writers_one_task_both_progress() {
        let lock = LocalRwLock::new(0i32);
        let g = lock.try_write().unwrap();
        let (w, flag) = flag_waker();
        let mut cx = Context::from_waker(&w);

        let mut f1 = std::pin::pin!(lock.write());
        let mut f2 = std::pin::pin!(lock.write());
        assert!(f1.as_mut().poll(&mut cx).is_pending());
        assert!(f2.as_mut().poll(&mut cx).is_pending());

        drop(g);
        let g1 = match f1.as_mut().poll(&mut cx) {
            Poll::Ready(g) => g,
            Poll::Pending => panic!("f1 should acquire after unlock"),
        };
        assert!(f2.as_mut().poll(&mut cx).is_pending());

        flag.store(false, AO::SeqCst);
        drop(g1);
        assert!(
            flag.load(AO::SeqCst),
            "f2 writer must be woken on release (shared-waker stranding)"
        );
    }

    /// A woken (and popped) writer that is then barged by a new writer must be
    /// re-queued on its next poll, not dropped from the queue.
    #[test]
    fn rwlock_woken_then_barged_writer_requeued() {
        let lock = LocalRwLock::new(0i32);
        let g = lock.try_write().unwrap();
        let (w, flag) = flag_waker();
        let mut cx = Context::from_waker(&w);

        let mut f = std::pin::pin!(lock.write());
        assert!(f.as_mut().poll(&mut cx).is_pending());

        drop(g); // wakes f (pops it)
        let g2 = lock.try_write().unwrap(); // barge
        assert!(f.as_mut().poll(&mut cx).is_pending()); // must re-register

        flag.store(false, AO::SeqCst);
        drop(g2);
        assert!(
            flag.load(AO::SeqCst),
            "woken-then-barged writer must be re-queued and re-woken"
        );
    }
}
