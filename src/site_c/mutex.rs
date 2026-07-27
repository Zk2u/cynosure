//! `LocalMutex` — a single-threaded async mutex.
//!
//! Non-atomic (`Cell`-based), so it sits near the `RefCell` floor while staying
//! usable across `.await` points, which a plain `RefCell` borrow is not.

use std::{
    cell::{Cell, UnsafeCell},
    future::Future,
    ops::{Deref, DerefMut},
    pin::Pin,
    task::{Context, Poll, Waker},
};

use super::queue::Queue;
use crate::hints::likely;

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
    // Waiters are `(id, waker)`: the `id` is a per-future token (NOT the waker)
    // used to identify and remove exactly that future's entry. Two futures of
    // the same lock driven by one task share a waker, so `will_wake` can't tell
    // their entries apart — the id can.
    waiters: UnsafeCell<Queue<(u64, Waker), 8>>,
    next_id: Cell<u64>,
    value: UnsafeCell<T>,
}

impl<T> LocalMutex<T> {
    /// Creates a new mutex in an unlocked state.
    #[inline]
    pub fn new(value: T) -> Self {
        Self {
            locked: Cell::new(false),
            waiters: UnsafeCell::new(Queue::new()),
            next_id: Cell::new(0),
            value: UnsafeCell::new(value),
        }
    }

    /// Acquires the mutex, returning a guard that releases it on drop.
    ///
    /// If the mutex is already locked, the calling task will yield and
    /// be woken when the mutex becomes available.
    #[inline]
    pub fn lock(&self) -> LocalMutexLockFuture<'_, T> {
        LocalMutexLockFuture {
            mutex: self,
            registered: None,
        }
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

    /// Releases the lock and wakes the next waiter (if any).
    #[inline]
    fn unlock(&self) {
        self.locked.set(false);
        self.wake_next();
    }

    /// Wakes the single next waiter, but only if the lock is currently free.
    ///
    /// Called both on release and when a pending lock future is cancelled
    /// (dropped), so a cancelled-but-already-notified waiter passes the turn to
    /// the next one — otherwise waking exactly one waiter could strand the rest.
    #[inline]
    fn wake_next(&self) {
        if self.locked.get() {
            return;
        }
        // SAFETY: single-threaded; the borrow lives only for the pop, so no
        // borrow of the queue is held across the `wake()` callback (which may
        // re-enter the mutex).
        let next = unsafe { (*self.waiters.get()).pop_front() };
        if let Some((_, waker)) = next {
            waker.wake();
        }
    }

    /// Registers (or refreshes) this future's waker, tracking its `(id, waker)`
    /// in `slot` so exactly its own entry can be removed again on cancellation.
    /// Keeps at most one queue entry per live future.
    fn register(&self, slot: &mut Option<(u64, Waker)>, waker: &Waker) {
        // SAFETY: single-threaded; no borrow held across a callback.
        let waiters = unsafe { &mut *self.waiters.get() };
        // Fast path: skip re-registering only if our entry is *still queued*
        // with an equivalent waker. (`wake_next` pops the woken entry, so after
        // a wake-then-barge we must fall through and re-add it.)
        if let Some((id, w)) = slot.as_ref()
            && w.will_wake(waker)
            && waiters.iter().any(|(qid, _)| qid == id)
        {
            return;
        }
        let id = match slot {
            Some((id, _)) => *id,
            None => {
                let id = self.next_id.get();
                self.next_id.set(id.wrapping_add(1));
                id
            }
        };
        waiters.retain(|(qid, _)| *qid != id);
        waiters.push_back((id, waker.clone()));
        *slot = Some((id, waker.clone()));
    }

    /// Removes this future's entry from the queue (on cancel/acquire).
    fn deregister(&self, slot: &mut Option<(u64, Waker)>) {
        if let Some((id, _)) = slot.take() {
            // SAFETY: single-threaded; no borrow held across a callback.
            let waiters = unsafe { &mut *self.waiters.get() };
            waiters.retain(|(qid, _)| *qid != id);
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
    /// This future's `(id, waker)` entry in the mutex's queue, if registered.
    /// Tracked so exactly this entry can be removed on cancellation (drop).
    registered: Option<(u64, Waker)>,
}

impl<'a, T> Future for LocalMutexLockFuture<'a, T> {
    type Output = LocalMutexGuard<'a, T>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // The future is Unpin (it holds only a reference and an `Option<Waker>`).
        let this = self.get_mut();

        // Fast path - uncontended lock is the common case
        if let Some(guard) = this.mutex.try_lock() {
            this.mutex.deregister(&mut this.registered);
            return Poll::Ready(guard);
        }

        // Slow path: register our waker BEFORE re-checking to avoid a missed
        // wake, then re-check.
        this.mutex.register(&mut this.registered, cx.waker());
        match this.mutex.try_lock() {
            Some(guard) => {
                this.mutex.deregister(&mut this.registered);
                Poll::Ready(guard)
            }
            None => Poll::Pending,
        }
    }
}

impl<'a, T> Drop for LocalMutexLockFuture<'a, T> {
    fn drop(&mut self) {
        // If cancelled while still queued, remove our entry and pass the turn on
        // (in case we were the one that had been woken).
        if self.registered.is_some() {
            self.mutex.deregister(&mut self.registered);
            self.mutex.wake_next();
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
    fn test_reentrant_waker_during_unlock_no_alias() {
        // A waker whose `wake()` re-enters the mutex: it acquires and instantly
        // releases the lock, which triggers a *nested* `unlock` that drains the
        // waiter queue. If `unlock` held a `&mut` borrow of the queue across
        // `wake()`, this nested drain would alias it (UB caught by Miri). The
        // fix re-borrows per pop, so no borrow is live across the callback.
        use std::{
            sync::atomic::{AtomicBool, Ordering},
            task::{RawWaker, RawWakerVTable},
        };

        static REENTERED: AtomicBool = AtomicBool::new(false);

        unsafe fn clone(p: *const ()) -> RawWaker {
            RawWaker::new(p, &VT)
        }
        unsafe fn wake(p: *const ()) {
            unsafe { wake_by_ref(p) }
        }
        unsafe fn wake_by_ref(p: *const ()) {
            REENTERED.store(true, Ordering::SeqCst);
            let mutex = unsafe { &*(p as *const LocalMutex<i32>) };
            // Reentrant acquire + release -> nested unlock drains the queue.
            if let Some(g) = mutex.try_lock() {
                drop(g);
            }
        }
        unsafe fn drop_fn(_p: *const ()) {}
        static VT: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_fn);

        let mutex = LocalMutex::new(0);
        let p = &mutex as *const LocalMutex<i32> as *const ();
        let reentrant = unsafe { Waker::from_raw(RawWaker::new(p, &VT)) };
        let noop = noop_waker();

        let guard = mutex.try_lock().unwrap();

        // Register the reentrant waker first, then a plain one.
        let mut f1 = mutex.lock();
        let mut cx1 = Context::from_waker(&reentrant);
        assert!(unsafe { Pin::new_unchecked(&mut f1).poll(&mut cx1) }.is_pending());

        let mut f2 = mutex.lock();
        let mut cx2 = Context::from_waker(&noop);
        assert!(unsafe { Pin::new_unchecked(&mut f2).poll(&mut cx2) }.is_pending());

        // Releasing drains: pops f1's reentrant waker and calls wake(), which
        // nests another unlock popping f2's waker.
        drop(guard);

        assert!(REENTERED.load(Ordering::SeqCst));
        assert!(mutex.try_lock().is_some());
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

    // --- wake-one / cancellation regression tests ---

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

    /// Unlock wakes exactly ONE waiter (FIFO), not all of them — no thundering
    /// herd.
    #[test]
    fn mutex_unlock_wakes_one_not_all() {
        let m = LocalMutex::new(0i32);
        let g = m.try_lock().unwrap();

        let (w1, f1) = flag_waker();
        let (w2, f2) = flag_waker();
        let (w3, f3) = flag_waker();
        let mut futs = [m.lock(), m.lock(), m.lock()];
        assert!(poll_with(&mut futs[0], &w1).is_pending());
        assert!(poll_with(&mut futs[1], &w2).is_pending());
        assert!(poll_with(&mut futs[2], &w3).is_pending());

        drop(g); // unlock
        let woken = [&f1, &f2, &f3]
            .iter()
            .filter(|f| f.load(AO::SeqCst))
            .count();
        assert_eq!(woken, 1, "exactly one waiter should be woken, not all");
        assert!(
            f1.load(AO::SeqCst),
            "the FIFO-front waiter is the one woken"
        );
    }

    /// A cancelled waiter doesn't break wake delivery: the live waiter is still
    /// woken (here via wake-one + drop torch-passing).
    #[test]
    fn mutex_cancellation_recovers() {
        let m = LocalMutex::new(0i32);
        let g = m.try_lock().unwrap();

        let (w1, _f1) = flag_waker();
        {
            let mut f1 = m.lock();
            assert!(poll_with(&mut f1, &w1).is_pending());
            // cancelled (dropped) -> waker removed
        }
        let (w2, f2) = flag_waker();
        let mut f2fut = m.lock();
        assert!(poll_with(&mut f2fut, &w2).is_pending());

        drop(g);
        assert!(
            f2.load(AO::SeqCst),
            "live waiter must be woken despite the cancelled one"
        );
    }

    /// Two lock futures of the SAME mutex driven by ONE task (so they share a
    /// waker): when the first acquires and deregisters, the second must NOT be
    /// stranded. (`will_wake` can't distinguish them — identity is by token.)
    #[test]
    fn mutex_two_futures_one_task_both_progress() {
        let m = LocalMutex::new(0i32);
        let g = m.try_lock().unwrap();
        let (w, flag) = flag_waker();
        let mut cx = Context::from_waker(&w);

        let mut f1 = std::pin::pin!(m.lock());
        let mut f2 = std::pin::pin!(m.lock());
        assert!(f1.as_mut().poll(&mut cx).is_pending());
        assert!(f2.as_mut().poll(&mut cx).is_pending());

        drop(g); // wakes the task
        let g1 = match f1.as_mut().poll(&mut cx) {
            Poll::Ready(g) => g,
            Poll::Pending => panic!("f1 should acquire after unlock"),
        };
        assert!(f2.as_mut().poll(&mut cx).is_pending());

        // Releasing must wake f2's entry — which the buggy `will_wake` removal
        // had deleted along with f1's.
        flag.store(false, AO::SeqCst);
        drop(g1);
        assert!(
            flag.load(AO::SeqCst),
            "f2 must be woken on release (shared-waker stranding)"
        );
    }

    /// A waiter that is woken (and popped from the queue) but then *barged* — a
    /// new acquirer grabs the freed lock before the waiter re-polls — must be
    /// re-queued on its next poll, not silently dropped from the queue.
    #[test]
    fn mutex_woken_then_barged_is_requeued() {
        let m = LocalMutex::new(0i32);
        let g = m.try_lock().unwrap();
        let (w, flag) = flag_waker();
        let mut cx = Context::from_waker(&w);

        let mut f = std::pin::pin!(m.lock());
        assert!(f.as_mut().poll(&mut cx).is_pending());

        drop(g); // wakes f (and pops it from the queue)
        let g2 = m.try_lock().unwrap(); // barge: grab the free lock first
        // f re-polls, can't acquire, and MUST re-register.
        assert!(f.as_mut().poll(&mut cx).is_pending());

        flag.store(false, AO::SeqCst);
        drop(g2);
        assert!(
            flag.load(AO::SeqCst),
            "woken-then-barged waiter must be re-queued and re-woken"
        );
    }
}
