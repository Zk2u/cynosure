//! A counting semaphore for single-threaded async executors.
//!
//! Limits concurrency / provides backpressure (in-flight requests, resource
//! pools, rate caps). Like the [`LocalMutex`](super::mutex::LocalMutex) it is
//! single-core, non-atomic (`Cell`-based), `!Send`, and reuses the same waiter
//! machinery: a `(token, n, Waker)` queue, per-future tokens for exact
//! deregistration, FIFO `wake_next` torch-passing, and cancellation safety.
//!
//! Async [`acquire`](LocalSemaphore::acquire) is **FIFO-fair**: the front
//! waiter is satisfied before any later one, so a large request can't be
//! starved by a stream of small ones (it does mean head-of-line blocking, the
//! standard FIFO trade). [`try_acquire`](LocalSemaphore::try_acquire) is
//! opportunistic and may barge queued waiters.

use std::{
    cell::{Cell, UnsafeCell},
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use super::queue::Queue;
use crate::hints::likely;

/// A counting semaphore. Not `Clone`; wrap in [`Rc`](std::rc::Rc) to share.
pub struct LocalSemaphore {
    permits: Cell<usize>,
    // `(token, n_requested, waker)`. `token` identifies a future's own entry for
    // exact removal (futures of one task share a waker, so identity is the id).
    waiters: UnsafeCell<Queue<(u64, usize, Waker), 8>>,
    next_id: Cell<u64>,
    // `!Send`/`!Sync` (single-core).
    _nosync: PhantomData<*const ()>,
}

impl LocalSemaphore {
    /// Create a semaphore with `permits` initial permits.
    #[inline]
    pub fn new(permits: usize) -> Self {
        Self {
            permits: Cell::new(permits),
            waiters: UnsafeCell::new(Queue::new()),
            next_id: Cell::new(0),
            _nosync: PhantomData,
        }
    }

    /// Currently available permits.
    #[inline]
    pub fn available(&self) -> usize {
        self.permits.get()
    }

    /// Opportunistically take one permit without waiting (may barge waiters).
    #[inline]
    pub fn try_acquire(&self) -> Option<Permit<'_>> {
        self.try_acquire_many(1)
    }

    /// Opportunistically take `n` permits without waiting (may barge waiters).
    #[inline]
    pub fn try_acquire_many(&self, n: usize) -> Option<Permit<'_>> {
        let p = self.permits.get();
        if likely(p >= n) {
            self.permits.set(p - n);
            Some(Permit { sem: self, n })
        } else {
            None
        }
    }

    /// Acquire one permit, waiting (FIFO-fair) until it is available.
    #[inline]
    pub fn acquire(&self) -> Acquire<'_> {
        self.acquire_many(1)
    }

    /// Acquire `n` permits, waiting (FIFO-fair) until they are available.
    #[inline]
    pub fn acquire_many(&self, n: usize) -> Acquire<'_> {
        Acquire {
            sem: self,
            n,
            registered: None,
        }
    }

    /// Add `n` permits to the pool (e.g. grow a resource pool), waking waiters
    /// that can now proceed.
    #[inline]
    pub fn add_permits(&self, n: usize) {
        self.permits.set(self.permits.get() + n);
        // SAFETY: single-threaded; the borrow does not cross a callback.
        if !unsafe { (*self.waiters.get()).is_empty() } {
            self.wake_next();
        }
    }

    /// Fair check: can a request for `n` proceed right now? Only if enough
    /// permits AND either no one is queued, or `id` is the queue front.
    #[inline]
    fn can_acquire_fair(&self, n: usize, id: Option<u64>) -> bool {
        if self.permits.get() < n {
            return false;
        }
        // SAFETY: single-threaded; borrow does not outlive this call.
        let waiters = unsafe { &*self.waiters.get() };
        match waiters.iter().next() {
            None => true,                              // no one ahead
            Some((front, _, _)) => Some(*front) == id, // I'm the front
        }
    }

    /// Wake the FIFO-front waiter iff it can now be satisfied. Does **not** pop
    /// it — the future deregisters itself once it actually acquires. Never
    /// skips the front (FIFO / no large-request starvation). Cold: only
    /// reached when waiters exist (the uncontended release path skips it).
    #[cold]
    #[inline(never)]
    fn wake_next(&self) {
        // SAFETY: single-threaded; the borrow lives only for the peek/clone, not
        // across the `wake()` callback (which may re-enter the semaphore).
        let front = {
            let waiters = unsafe { &*self.waiters.get() };
            // Fast path: no waiters — the overwhelmingly common case on release.
            // Skips constructing the iterator + the permit re-read entirely.
            if waiters.is_empty() {
                return;
            }
            waiters
                .iter()
                .next()
                .filter(|(_, n, _)| self.permits.get() >= *n)
                .map(|(_, _, w)| w.clone())
        };
        if let Some(waker) = front {
            waker.wake();
        }
    }

    fn register(&self, slot: &mut Option<(u64, usize, Waker)>, n: usize, waker: &Waker) {
        // SAFETY: single-threaded; no borrow held across a callback.
        let waiters = unsafe { &mut *self.waiters.get() };
        if let Some((id, _, w)) = slot.as_ref()
            && w.will_wake(waker)
            && waiters.iter().any(|(qid, _, _)| qid == id)
        {
            return;
        }
        let id = match slot {
            Some((id, _, _)) => *id,
            None => {
                let id = self.next_id.get();
                self.next_id.set(id.wrapping_add(1));
                id
            }
        };
        waiters.retain(|(qid, _, _)| *qid != id);
        waiters.push_back((id, n, waker.clone()));
        *slot = Some((id, n, waker.clone()));
    }

    fn deregister(&self, slot: &mut Option<(u64, usize, Waker)>) {
        if let Some((id, _, _)) = slot.take() {
            // SAFETY: single-threaded; no borrow held across a callback.
            let waiters = unsafe { &mut *self.waiters.get() };
            waiters.retain(|(qid, _, _)| *qid != id);
        }
    }

    #[inline]
    fn release(&self, n: usize) {
        self.permits.set(self.permits.get() + n);
        // Fast path inlined into `Permit::drop`: no waiters means nothing to
        // wake, the overwhelmingly common uncontended release. SAFETY:
        // single-threaded; the borrow does not cross a callback.
        if !unsafe { (*self.waiters.get()).is_empty() } {
            self.wake_next();
        }
    }
}

impl std::fmt::Debug for LocalSemaphore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSemaphore")
            .field("available", &self.permits.get())
            .finish()
    }
}

/// Future returned by [`LocalSemaphore::acquire`]/[`acquire_many`].
///
/// [`acquire_many`]: LocalSemaphore::acquire_many
pub struct Acquire<'a> {
    sem: &'a LocalSemaphore,
    n: usize,
    registered: Option<(u64, usize, Waker)>,
}

impl<'a> Future for Acquire<'a> {
    type Output = Permit<'a>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut(); // Unpin
        let id = this.registered.as_ref().map(|(id, _, _)| *id);

        // Fast/fair path.
        if this.sem.can_acquire_fair(this.n, id) {
            this.sem.permits.set(this.sem.permits.get() - this.n);
            this.sem.deregister(&mut this.registered);
            // Remaining permits may satisfy the next waiter — cascade.
            this.sem.wake_next();
            return Poll::Ready(Permit {
                sem: this.sem,
                n: this.n,
            });
        }

        // Register, then re-check (now possibly the front).
        this.sem.register(&mut this.registered, this.n, cx.waker());
        let id = this.registered.as_ref().map(|(id, _, _)| *id);
        if this.sem.can_acquire_fair(this.n, id) {
            this.sem.permits.set(this.sem.permits.get() - this.n);
            this.sem.deregister(&mut this.registered);
            this.sem.wake_next();
            Poll::Ready(Permit {
                sem: this.sem,
                n: this.n,
            })
        } else {
            Poll::Pending
        }
    }
}

impl<'a> Drop for Acquire<'a> {
    fn drop(&mut self) {
        // Cancelled while queued: remove our entry and pass the turn on (we may
        // have been the front blocking others).
        if self.registered.is_some() {
            self.sem.deregister(&mut self.registered);
            self.sem.wake_next();
        }
    }
}

/// A held permit (or permits). Returns its permits to the semaphore on drop.
pub struct Permit<'a> {
    sem: &'a LocalSemaphore,
    n: usize,
}

impl<'a> Permit<'a> {
    /// Number of permits held.
    #[inline]
    pub fn count(&self) -> usize {
        self.n
    }

    /// Permanently drop these permits from the semaphore (do not return them).
    #[inline]
    pub fn forget(self) {
        std::mem::forget(self);
    }
}

impl<'a> Drop for Permit<'a> {
    #[inline]
    fn drop(&mut self) {
        self.sem.release(self.n);
    }
}

impl<'a> std::fmt::Debug for Permit<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Permit").field("count", &self.n).finish()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        pin::pin,
        sync::{
            Arc as StdArc,
            atomic::{AtomicBool, Ordering as AO},
        },
        task::Wake,
    };

    use super::*;

    struct FlagWaker(StdArc<AtomicBool>);
    impl Wake for FlagWaker {
        fn wake(self: StdArc<Self>) {
            self.0.store(true, AO::SeqCst);
        }
        fn wake_by_ref(self: &StdArc<Self>) {
            self.0.store(true, AO::SeqCst);
        }
    }
    fn flag_waker() -> (Waker, StdArc<AtomicBool>) {
        let f = StdArc::new(AtomicBool::new(false));
        (StdArc::new(FlagWaker(f.clone())).into(), f)
    }

    #[test]
    fn try_acquire_counts() {
        let s = LocalSemaphore::new(2);
        let a = s.try_acquire().unwrap();
        let b = s.try_acquire().unwrap();
        assert!(s.try_acquire().is_none());
        drop(a);
        assert_eq!(s.available(), 1);
        let _c = s.try_acquire().unwrap();
        drop(b);
        assert_eq!(s.available(), 1);
    }

    #[test]
    fn acquire_many_and_release() {
        let s = LocalSemaphore::new(3);
        let p = s.try_acquire_many(3).unwrap();
        assert_eq!(s.available(), 0);
        assert!(s.try_acquire().is_none());
        drop(p);
        assert_eq!(s.available(), 3);
    }

    #[test]
    fn acquire_waits_then_wakes_front() {
        let s = LocalSemaphore::new(0);
        let (w, flag) = flag_waker();
        let mut cx = Context::from_waker(&w);
        let mut f = pin!(s.acquire());
        assert!(f.as_mut().poll(&mut cx).is_pending());
        s.add_permits(1); // wakes front
        assert!(flag.load(AO::SeqCst));
        match f.as_mut().poll(&mut cx) {
            Poll::Ready(p) => assert_eq!(p.count(), 1),
            Poll::Pending => panic!("should acquire after add_permits"),
        }
    }

    /// FIFO: a large request at the front is not starved/skipped by a smaller
    /// one behind it, even though the small one *could* be satisfied.
    #[test]
    fn fifo_large_request_not_skipped() {
        let s = LocalSemaphore::new(0);
        let (w1, f1) = flag_waker();
        let (w2, f2) = flag_waker();
        let mut big = pin!(s.acquire_many(2)); // front, needs 2
        let mut small = pin!(s.acquire_many(1)); // behind, needs 1
        assert!(
            big.as_mut()
                .poll(&mut Context::from_waker(&w1))
                .is_pending()
        );
        assert!(
            small
                .as_mut()
                .poll(&mut Context::from_waker(&w2))
                .is_pending()
        );

        s.add_permits(1); // enough for `small` but NOT for the front `big`
        assert!(!f1.load(AO::SeqCst), "front not yet satisfiable");
        assert!(!f2.load(AO::SeqCst), "FIFO: small must not jump the front");

        s.add_permits(1); // now front `big` is satisfiable
        assert!(f1.load(AO::SeqCst), "front woken once satisfiable");
    }

    /// Cancelling a queued acquirer passes the turn on (torch-passing).
    #[test]
    fn cancellation_recovers() {
        let s = LocalSemaphore::new(0);
        let (w1, _f1) = flag_waker();
        {
            let mut f1 = pin!(s.acquire());
            assert!(f1.as_mut().poll(&mut Context::from_waker(&w1)).is_pending());
        } // cancelled
        let (w2, f2) = flag_waker();
        let mut f2fut = pin!(s.acquire());
        assert!(
            f2fut
                .as_mut()
                .poll(&mut Context::from_waker(&w2))
                .is_pending()
        );
        s.add_permits(1);
        assert!(
            f2.load(AO::SeqCst),
            "live waiter woken despite cancelled one"
        );
    }

    /// add_permits cascades through multiple satisfiable front waiters.
    #[test]
    fn cascade_wake() {
        let s = LocalSemaphore::new(0);
        let (w1, f1) = flag_waker();
        let (w2, f2) = flag_waker();
        let mut a = pin!(s.acquire());
        let mut b = pin!(s.acquire());
        assert!(a.as_mut().poll(&mut Context::from_waker(&w1)).is_pending());
        assert!(b.as_mut().poll(&mut Context::from_waker(&w2)).is_pending());
        s.add_permits(2);
        // front acquires and cascades to the next.
        let _pa = match a.as_mut().poll(&mut Context::from_waker(&w1)) {
            Poll::Ready(p) => p,
            Poll::Pending => panic!("a should acquire"),
        };
        assert!(f1.load(AO::SeqCst));
        assert!(f2.load(AO::SeqCst), "second waiter cascaded");
    }
}
