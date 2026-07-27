//! A lightweight **single-consumer** MPSC channel.
//!
//! One small bounded queue shared by all producers: cloneable `Send` senders,
//! **global FIFO** across senders, and O(cap) memory per channel — the right
//! tool for the many small command/signal channels a thread-per-core system
//! runs, where latency, footprint, and cloneable producers matter more than raw
//! bandwidth. For a fixed 1:1 stream use [`RingBuf`](super::ringbuf); for a
//! single reply use [`oneshot`](super::oneshot).
//!
//! The queue is a `VecDeque` under a short hand-rolled **TTAS spinlock** (the
//! same shape kanal uses): producers serialize through a ~20 ns critical
//! section rather than a CAS-retry storm. The single consumer **batch-drains**
//! under one lock into a local staging buffer and serves from it lock-free
//! between drains, so it pays the lock once per batch, not per item.
//!
//! Backpressured senders park in a **wait-list held under the same lock** (a
//! single-threaded [`Queue`], since the spinlock serializes access) — so a
//! sender parks and a freed slot wakes it in *one* lock acquisition, with no
//! separate mutex and no cross-thread handshake (the lock orders it). The
//! consumer parks on a flag-gated `WaiterSlot`. `recv()` is **polite by
//! default** — it parks promptly, costing a shared reactor's co-located tasks
//! nothing while idle; `recv_hot()` opts into a short pre-park spin window for
//! consumers that own their thread (~70 ns catch vs the ~2 µs park path). Sync
//! `try_*` is the bare locked queue.

use std::{
    cell::UnsafeCell,
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

use super::{notify::WaiterSlot, padding::CachePadded};
use crate::{
    hints::{likely, unlikely},
    site_c::queue::Queue,
};

/// Backpressured senders, parked under the channel lock. Single-threaded access
/// (the spinlock serializes), so cynosure's non-atomic [`Queue`] is the right
/// container — inline for the first few waiters, spilling only under a flood.
struct Waiters {
    q: Queue<(u64, Waker), 8>,
    next_id: u64,
}

/// Spin a small pseudo-random count to desynchronize lock contenders (a cheap
/// atomic-LCG, as in kanal's `spin_rand`), so they don't re-CAS the freed lock
/// in lockstep and storm the cache line.
#[inline]
fn jitter() {
    use std::sync::atomic::AtomicU32;
    static SEED: AtomicU32 = AtomicU32::new(0x9e37_79b9);
    let s = SEED.fetch_add(0x6d2b_79f5, Ordering::Relaxed);
    let n = (s.wrapping_mul(2_654_435_761) >> 26) & 0x3F; // 0..63
    for _ in 0..n {
        std::hint::spin_loop();
    }
}

/// The producer-hot cluster: the lock and everything touched inside its
/// critical section. `repr(C)` pins the order so it all sits on the one cache
/// line the lock CAS already owns — the `len` publish and the queue-header
/// access are then hits on an owned line, not extra coherency traffic.
#[repr(C)]
struct Hot<T> {
    // A `VecDeque` guarded by a hand-rolled **TTAS spinlock** — the same shape as
    // kanal's lock. The critical section is one push/pop (~20 ns), so producers
    // serialize through a short lock instead of a CAS-retry storm. Spinning is on
    // a plain *load* (CAS only when the lock looks free), so contenders don't
    // storm the lock line; after a few spins they `yield_now`, so a descheduled
    // holder can't convoy them under oversubscription.
    locked: AtomicBool,
    // "Queue is non-empty" hint for the consumer's lock-free spin gate. Written
    // ONLY on the empty→non-empty transition (push finding the queue empty) and
    // cleared by the steal — so in steady throughput producers never touch it
    // and the gate polls a quiet line; a plain release store suffices (writers
    // hold the lock). Lives on the lock line: the transition writer already owns
    // that line, and the gate only polls while the queue is empty.
    nonempty: AtomicUsize,
    cap: usize,
    queue: UnsafeCell<VecDeque<T>>,
}

struct Inner<T> {
    hot: CachePadded<Hot<T>>,
    // Backpressured senders, guarded by `locked`. Parking-path only (cold), and
    // its inline waiter storage is large — kept off the hot line.
    waiters: UnsafeCell<Waiters>,
    // Consumer parks here (empty). Own line: the producer's `signal()` load must
    // not bounce with lock traffic, nor the armed-flag write with the CAS line.
    recv: CachePadded<WaiterSlot>,
    // Lifecycle (read-mostly): loaded by the consumer every non-staged poll;
    // padded away from the lock line so those loads don't false-share with the
    // producers' CAS storm.
    senders: CachePadded<AtomicUsize>, // live sender count (close detection)
    closed: AtomicBool,                // receiver dropped (shares the tail line)
}

// SAFETY: the spinlock makes every `queue`/`waiters` access exclusive; `T:
// Send`.
unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}

impl<T> Inner<T> {
    #[inline]
    fn lock(&self) {
        if self
            .hot
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.lock_slow();
        }
    }

    #[cold]
    fn lock_slow(&self) {
        // NOTE: the eager `yield_now` below is load-bearing. A "userspace-first"
        // variant (long jittered spin bursts, yield only every ~64th round) was
        // measured and is catastrophic under oversubscription — with 16 spinning
        // producers a descheduled lock *holder* convoys everyone (16/1024 fan-in
        // fell ~50 -> 5 M/s). Producers must cede the core quickly so the holder
        // can run.
        loop {
            let mut spins = 0u32;
            // Test-and-test-and-set: spin on a *load* so we don't storm the lock
            // line; only CAS once it reads free.
            while self.hot.locked.load(Ordering::Relaxed) {
                if spins < 8 {
                    spins += 1;
                    std::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                }
            }
            if self
                .hot
                .locked
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            // Lost the CAS race: jittered backoff so contenders don't re-attempt
            // the freed lock in lockstep (kanal's `spin_rand` trick).
            jitter();
        }
    }

    #[inline]
    fn unlock(&self) {
        self.hot.locked.store(false, Ordering::Release);
    }

    /// Enqueue without blocking. `Err(val)` if full (used by `try_send`).
    #[inline]
    fn try_push(&self, val: T) -> Result<(), T> {
        self.lock();
        // SAFETY: exclusive under the lock.
        let q = unsafe { &mut *self.hot.queue.get() };
        if unlikely(q.len() >= self.hot.cap) {
            self.unlock();
            return Err(val);
        }
        q.push_back(val);
        if unlikely(q.len() == 1) {
            // empty -> non-empty: raise the gate hint (transition only).
            self.hot.nonempty.store(1, Ordering::Release);
        }
        self.unlock();
        Ok(())
    }

    /// Enqueue as many of `items` (from the front) as fit, in **one** lock
    /// acquisition. Drains the accepted prefix out of `items` and returns how
    /// many were pushed; anything left in `items` didn't fit (channel full).
    fn try_push_many(&self, items: &mut Vec<T>) -> usize {
        if items.is_empty() {
            return 0;
        }
        self.lock();
        // SAFETY: exclusive under the lock.
        let q = unsafe { &mut *self.hot.queue.get() };
        let was_empty = q.is_empty();
        let k = (self.hot.cap - q.len()).min(items.len());
        if likely(k > 0) {
            q.extend(items.drain(..k)); // moves owned `T`s, FIFO-ordered
            if unlikely(was_empty) {
                // empty -> non-empty: raise the gate hint (transition only).
                self.hot.nonempty.store(1, Ordering::Release);
            }
        }
        self.unlock();
        k
    }

    /// `send`'s push: enqueue if there's room, else park `waker` in the
    /// under-lock wait-list and return the parker's id — all in one lock
    /// acquisition. The lock serializes the full-check and the consumer's
    /// drain, so the wakeup can't be lost (no separate SeqCst handshake
    /// needed).
    ///
    /// Deregistering a previous parking is deliberately a *separate*
    /// [`remove_waiter`](Self::remove_waiter) call rather than merged in here:
    /// releasing the lock between the deregister and this push lets the
    /// consumer's buffer-steal interleave, and that steal is what frees the
    /// queue for producers to proceed. Measured: merging the two into one
    /// longer critical section costs 12–19% multi-producer throughput.
    #[inline]
    fn push_or_park(&self, val: T, waker: &Waker) -> Result<(), PushParked<T>> {
        self.lock();
        if unlikely(self.closed.load(Ordering::Relaxed)) {
            self.unlock();
            return Err(PushParked::Closed(val));
        }
        // SAFETY: exclusive under the lock.
        let q = unsafe { &mut *self.hot.queue.get() };
        if likely(q.len() < self.hot.cap) {
            q.push_back(val);
            if unlikely(q.len() == 1) {
                // empty -> non-empty: raise the gate hint (transition only).
                self.hot.nonempty.store(1, Ordering::Release);
            }
            self.unlock();
            return Ok(());
        }
        let w = unsafe { &mut *self.waiters.get() };
        let id = w.next_id;
        w.next_id = w.next_id.wrapping_add(1);
        w.q.push_back((id, waker.clone()));
        self.unlock();
        Err(PushParked::Full(val, id))
    }

    /// Deregister a parked sender (cancellation or before re-parking).
    fn remove_waiter(&self, id: u64) {
        self.lock();
        // SAFETY: exclusive under the lock.
        let w = unsafe { &mut *self.waiters.get() };
        w.q.retain(|(i, _)| *i != id);
        self.unlock();
    }

    /// **Buffer-steal** (single-consumer only): swap the *entire* shared queue
    /// out for the consumer's empty `staging` in O(1) — three words, not
    /// O(batch) of element moves — so the consumer barely touches the lock.
    /// It then drains the stolen queue privately, lock-free. A general MPMC
    /// consumer can't do this; a lone consumer owns the whole queue.
    /// `staging` must be empty on entry. Wakes one parked sender to start
    /// refilling the now-empty buffer (it bursts the rest). Returns the
    /// number stolen.
    fn steal_into(&self, staging: &mut VecDeque<T>) -> usize {
        self.lock();
        // SAFETY: exclusive under the lock.
        let q = unsafe { &mut *self.hot.queue.get() };
        let n = q.len();
        if unlikely(n == 0) {
            self.unlock();
            return 0;
        }
        std::mem::swap(q, staging); // staging <- all items, q <- empty
        self.hot.nonempty.store(0, Ordering::Release); // stole everything: queue is empty
        let w = unsafe { &mut *self.waiters.get() };
        let waker = w.q.pop_front().map(|(_, wk)| wk);
        self.unlock();
        if let Some(wk) = waker {
            wk.wake(); // wake outside the lock
        }
        n
    }

    /// Wake one parked sender (liveness: the consumer is about to sleep on an
    /// empty queue, so any leftover waiter has room to push now).
    fn wake_one_waiter(&self) {
        self.lock();
        // SAFETY: exclusive under the lock.
        let w = unsafe { &mut *self.waiters.get() };
        let waker = w.q.pop_front().map(|(_, wk)| wk);
        self.unlock();
        if let Some(wk) = waker {
            wk.wake();
        }
    }

    /// Wake every parked sender (the receiver dropped — they must observe
    /// closure).
    fn wake_all_waiters(&self) {
        self.lock();
        // SAFETY: exclusive under the lock.
        let w = unsafe { &mut *self.waiters.get() };
        let mut wakers = Vec::new();
        while let Some((_, wk)) = w.q.pop_front() {
            wakers.push(wk);
        }
        self.unlock();
        for wk in wakers {
            wk.wake();
        }
    }
}

/// Outcome of [`Inner::push_or_park`] when the item wasn't sent.
enum PushParked<T> {
    Full(T, u64), // parked in the wait-list with this id
    Closed(T),    // receiver gone
}

/// Create a bounded single-consumer channel (capacity at least 1). Clone the
/// [`Sender`] freely; there is one [`Receiver`].
pub fn bounded<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let cap = capacity.max(1);
    let inner = Arc::new(Inner {
        hot: CachePadded::new(Hot {
            locked: AtomicBool::new(false),
            nonempty: AtomicUsize::new(0),
            cap,
            queue: UnsafeCell::new(VecDeque::with_capacity(cap)),
        }),
        waiters: UnsafeCell::new(Waiters {
            q: Queue::new(),
            next_id: 0,
        }),
        recv: CachePadded::new(WaiterSlot::new()),
        senders: CachePadded::new(AtomicUsize::new(1)),
        closed: AtomicBool::new(false),
    });
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver {
            inner,
            staging: VecDeque::new(),
        },
    )
}

/// Returned by [`Sender::try_send`].
#[derive(Debug, PartialEq, Eq)]
pub enum TrySendError<T> {
    /// The channel is at capacity.
    Full(T),
    /// The receiver has been dropped.
    Closed(T),
}

/// Returned by [`Sender::send`] — the receiver was dropped.
#[derive(Debug, PartialEq, Eq)]
pub struct SendError<T>(pub T);

/// Returned by [`Receiver::try_recv`].
#[derive(Debug, PartialEq, Eq)]
pub enum TryRecvError {
    /// No item is currently available.
    Empty,
    /// All senders have been dropped and the channel is drained.
    Closed,
}

/// A cloneable, `Send` producer.
pub struct Sender<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.inner.senders.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Sender<T> {
    /// Enqueue without waiting.
    #[inline]
    pub fn try_send(&self, val: T) -> Result<(), TrySendError<T>> {
        if unlikely(self.inner.closed.load(Ordering::Acquire)) {
            return Err(TrySendError::Closed(val));
        }
        match self.inner.try_push(val) {
            Ok(()) => {
                self.inner.recv.signal();
                Ok(())
            }
            Err(val) if self.inner.closed.load(Ordering::Acquire) => Err(TrySendError::Closed(val)),
            Err(val) => Err(TrySendError::Full(val)),
        }
    }

    /// Enqueue a **batch** without waiting: push as many of `items` (from the
    /// front) as currently fit, in one lock acquisition and with one consumer
    /// wakeup for the whole batch. Drains the accepted prefix out of `items`
    /// and returns how many were sent; anything left in `items` didn't fit.
    ///
    /// This amortizes the per-item lock + signal cost — the throughput path for
    /// bursty producers (e.g. a per-core broadcast fan-out). Returns `0` if the
    /// receiver has been dropped (items are left untouched).
    #[inline]
    pub fn try_send_many(&self, items: &mut Vec<T>) -> usize {
        if unlikely(self.inner.closed.load(Ordering::Acquire)) {
            return 0;
        }
        let k = self.inner.try_push_many(items);
        if k > 0 {
            self.inner.recv.signal(); // one signal for the whole batch
        }
        k
    }

    /// Enqueue, waiting on backpressure if the channel is full.
    #[inline]
    pub fn send(&self, val: T) -> Send_<'_, T> {
        Send_ {
            inner: &self.inner,
            val: Some(val),
            parked_id: None,
        }
    }

    /// `true` once the receiver has been dropped.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.inner.senders.fetch_sub(1, Ordering::SeqCst) == 1 {
            // Last sender gone: wake the consumer so it observes closure.
            self.inner.recv.signal();
        }
    }
}

/// Future for [`Sender::send`]. Re-checks per poll (never holds queue state
/// across a suspend), and parks in the under-lock wait-list so a freed slot
/// wakes exactly one waiting sender (no herd). `parked_id` lets it deregister
/// on cancellation.
pub struct Send_<'a, T> {
    inner: &'a Inner<T>,
    val: Option<T>,
    parked_id: Option<u64>,
}

impl<T> Unpin for Send_<'_, T> {}

impl<'a, T> Future for Send_<'a, T> {
    type Output = Result<(), SendError<T>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let val = match this.val.take() {
            Some(v) => v,
            None => return Poll::Ready(Ok(())),
        };
        // Drop any registration from a previous poll (woken: already popped, a
        // no-op; spurious re-poll: avoids a duplicate). Kept separate from the push
        // below so the consumer's steal can interleave between the two locks.
        if let Some(id) = this.parked_id.take() {
            this.inner.remove_waiter(id);
        }
        // Push if there's room, else park in the wait-list. The lock serializes
        // against the consumer's drain, so the wakeup can't be lost.
        match this.inner.push_or_park(val, cx.waker()) {
            Ok(()) => {
                this.inner.recv.signal();
                Poll::Ready(Ok(()))
            }
            Err(PushParked::Closed(v)) => Poll::Ready(Err(SendError(v))),
            Err(PushParked::Full(v, id)) => {
                this.parked_id = Some(id);
                this.val = Some(v);
                Poll::Pending
            }
        }
    }
}

impl<'a, T> Drop for Send_<'a, T> {
    fn drop(&mut self) {
        // Cancelled while parked: remove our waker so it can't absorb a wake.
        if let Some(id) = self.parked_id.take() {
            self.inner.remove_waiter(id);
        }
    }
}

/// Spin budget the consumer burns trying to catch an item before parking.
/// Pure `spin_loop` — **no `yield_now`**: yielding the OS thread inside
/// `poll()` stalls co-located tasks on a shared executor (measured: removing
/// the yields was free on dedicated-thread executors and recovered up to +40%
/// on a tokio worker pool).
const RECV_SPIN: usize = 192;

/// The single consumer. Not `Clone` — which is what licenses the local,
/// lock-free `staging` it serves from between batch drains.
pub struct Receiver<T> {
    inner: Arc<Inner<T>>,
    staging: VecDeque<T>, // consumer-local; drained from the shared queue in batches
}

impl<T> Receiver<T> {
    /// Next staged item, stealing the whole shared queue when staging runs dry.
    #[inline]
    fn next_staged(&mut self) -> Option<T> {
        if let Some(v) = self.staging.pop_front() {
            return Some(v); // common: served from local staging, no lock
        }
        if self.inner.steal_into(&mut self.staging) > 0 {
            return self.staging.pop_front();
        }
        None
    }

    /// Dequeue without waiting.
    #[inline]
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        if let Some(v) = self.next_staged() {
            return Ok(v);
        }
        if self.inner.senders.load(Ordering::Acquire) == 0 {
            // Re-check: a sender may have pushed then dropped.
            return self.next_staged().ok_or(TryRecvError::Closed);
        }
        Err(TryRecvError::Empty)
    }

    /// Dequeue a **batch** without waiting: append up to `max` ready items to
    /// `out` and return how many were appended (`0` if nothing is ready).
    ///
    /// A single `steal_into` brings the whole shared queue
    /// into local staging under one lock; the drain then serves from staging
    /// lock-free — so a burst is received for ~one lock acquisition instead of
    /// one per item. FIFO order is preserved.
    #[inline]
    pub fn recv_many(&mut self, out: &mut Vec<T>, max: usize) -> usize {
        let mut n = 0;
        while n < max {
            match self.next_staged() {
                Some(v) => {
                    out.push(v);
                    n += 1;
                }
                None => break,
            }
        }
        n
    }

    /// Dequeue, waiting until an item arrives. `None` once all senders have
    /// dropped **and** the channel is drained.
    ///
    /// **Polite by default**: parks promptly when the channel is empty instead
    /// of spinning inside `poll()`, so a consumer embedded in a shared reactor
    /// (the normal thread-per-core deployment) costs its co-located tasks
    /// nothing while idle. In-reactor delivery latency remains excellent (the
    /// wake path is flag-gated and direct); if the consumer effectively owns
    /// its thread and you want the last ~microsecond of wakeup latency, use
    /// [`recv_hot`](Self::recv_hot).
    #[inline]
    pub fn recv(&mut self) -> Recv<'_, T> {
        Recv {
            rx: self,
            spin: false,
        }
    }

    /// Like [`recv`](Self::recv), but burns a short busy-spin window before
    /// parking, catching an imminent item without a futex/task-wake round trip
    /// (~70 ns vs ~2 µs for the park path on a dedicated thread).
    ///
    /// **Opt-in for consumers that own their thread** (a dedicated control
    /// thread, a strict request/response ping-pong): the spin window runs
    /// inside `poll()`, so on a shared executor it steals that time from every
    /// co-located task each time the channel goes empty — measured as an ~80×
    /// reduction in neighbor task throughput under paced traffic. Adaptive
    /// spin heuristics were tried and cannot distinguish the two deployments
    /// reliably; the caller can, hence this explicit method.
    #[inline]
    pub fn recv_hot(&mut self) -> Recv<'_, T> {
        Recv {
            rx: self,
            spin: true,
        }
    }

    /// `true` once all senders have dropped.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.inner.senders.load(Ordering::Acquire) == 0
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        // Wake *all* blocked senders so they observe closure and return `SendError`
        // (wake-one would strand the rest).
        self.inner.wake_all_waiters();
    }
}

/// Future for [`Receiver::recv`] / [`Receiver::recv_hot`].
pub struct Recv<'a, T> {
    rx: &'a mut Receiver<T>,
    spin: bool, // recv_hot: busy-spin window before parking
}

impl<T> Unpin for Recv<'_, T> {}

impl<'a, T> Future for Recv<'a, T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let rx = &mut *this.rx;
        // Fast path (serves from local staging without the lock).
        if let Some(v) = rx.next_staged() {
            return Poll::Ready(Some(v));
        }
        if rx.inner.senders.load(Ordering::Acquire) == 0 {
            return Poll::Ready(rx.next_staged());
        }
        // recv_hot only: spin before parking, catching an imminent item without
        // a futex/task-wake round trip. Not part of default recv() — the window
        // runs inside poll(), so on a shared reactor it taxes co-located tasks
        // (~80× neighbor-throughput under paced traffic, measured). Adaptive
        // gates were prototyped three ways (spin-catch hysteresis, arm-recheck
        // re-warm, time-based re-warm) and each failed a regime: consumer-local
        // signals cannot split "hot stream mid-jitter" from "paced stream" —
        // so the CALLER chooses, via recv() vs recv_hot().
        if this.spin {
            for _i in 0..RECV_SPIN {
                #[allow(clippy::collapsible_if)] // if-let inner; collapsing needs let-chains
                if rx.inner.hot.nonempty.load(Ordering::Acquire) > 0 {
                    if let Some(v) = rx.next_staged() {
                        return Poll::Ready(Some(v));
                    }
                }
                if rx.inner.senders.load(Ordering::Acquire) == 0 {
                    return Poll::Ready(rx.next_staged());
                }
                std::hint::spin_loop();
            }
        }
        // Park, then re-check (SB-free handshake).
        rx.inner.recv.arm(cx.waker());
        if let Some(v) = rx.next_staged() {
            rx.inner.recv.disarm();
            return Poll::Ready(Some(v));
        }
        if rx.inner.senders.load(Ordering::SeqCst) == 0 {
            rx.inner.recv.disarm();
            return Poll::Ready(rx.next_staged());
        }
        // About to sleep empty: ensure one parked producer is woken so it refills
        // and signals us (liveness; flag-gated, free when none parked).
        rx.inner.wake_one_waiter();
        Poll::Pending
    }
}

impl<'a, T> Drop for Recv<'a, T> {
    fn drop(&mut self) {
        // Cancelled while parked: don't leave a stale waker armed.
        self.rx.inner.recv.disarm();
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::atomic::AtomicU64, thread};

    use super::*;
    use crate::blocking::block_on;

    #[test]
    fn basic_send_recv() {
        let (tx, mut rx) = bounded::<u32>(4);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        assert_eq!(rx.try_recv(), Ok(1));
        assert_eq!(rx.try_recv(), Ok(2));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn batch_send_recv_fifo_and_partial() {
        let (tx, mut rx) = bounded::<u32>(4);

        // Partial fill: cap 4, offer 6 -> 4 sent, 2 left (the last two).
        let mut items = vec![1, 2, 3, 4, 5, 6];
        assert_eq!(tx.try_send_many(&mut items), 4);
        assert_eq!(items, vec![5, 6], "unsent remainder retained, in order");

        // Batch drain preserves FIFO; `max` caps the take.
        let mut out = Vec::new();
        assert_eq!(rx.recv_many(&mut out, 2), 2);
        assert_eq!(out, vec![1, 2]);
        assert_eq!(rx.recv_many(&mut out, 99), 2, "drains what's left");
        assert_eq!(out, vec![1, 2, 3, 4]);
        assert_eq!(rx.recv_many(&mut out, 99), 0, "empty now");

        // The retained remainder now fits.
        assert_eq!(tx.try_send_many(&mut items), 2);
        assert!(items.is_empty());
        out.clear();
        assert_eq!(rx.recv_many(&mut out, 10), 2);
        assert_eq!(out, vec![5, 6]);
    }

    #[test]
    fn batch_send_after_receiver_dropped_is_noop() {
        let (tx, rx) = bounded::<u32>(4);
        drop(rx);
        let mut items = vec![1, 2, 3];
        assert_eq!(tx.try_send_many(&mut items), 0);
        assert_eq!(items, vec![1, 2, 3], "items untouched when closed");
    }

    #[test]
    fn batch_empty_input() {
        let (tx, mut rx) = bounded::<u32>(4);
        let mut empty: Vec<u32> = Vec::new();
        assert_eq!(tx.try_send_many(&mut empty), 0);
        let mut out = Vec::new();
        assert_eq!(rx.recv_many(&mut out, 0), 0);
        assert_eq!(rx.recv_many(&mut out, 10), 0);
    }

    #[test]
    fn full_then_drain() {
        let (tx, mut rx) = bounded::<u32>(2); // rounds to 2
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        assert_eq!(tx.try_send(3), Err(TrySendError::Full(3)));
        assert_eq!(rx.try_recv(), Ok(1));
        tx.try_send(3).unwrap();
        assert_eq!(rx.try_recv(), Ok(2));
        assert_eq!(rx.try_recv(), Ok(3));
    }

    #[test]
    fn fifo_per_producer_and_close() {
        let (tx, mut rx) = bounded::<u32>(8);
        let tx2 = tx.clone();
        tx.try_send(10).unwrap();
        tx2.try_send(20).unwrap();
        drop(tx);
        drop(tx2);
        let mut got = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(v) => got.push(v),
                Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Empty) => unreachable!(),
            }
        }
        got.sort();
        assert_eq!(got, vec![10, 20]);
    }

    #[test]
    fn recv_parks_then_wakes() {
        let (tx, mut rx) = bounded::<u32>(4);
        let h = thread::spawn(move || block_on(rx.recv()));
        thread::sleep(std::time::Duration::from_millis(20));
        tx.try_send(99).unwrap();
        assert_eq!(h.join().unwrap(), Some(99));
    }

    #[test]
    fn multi_producer_no_loss() {
        const NP: u64 = 8;
        const PER: u64 = 50_000;
        let (tx, mut rx) = bounded::<u64>(64);
        let ps: Vec<_> = (0..NP)
            .map(|_| {
                let tx = tx.clone();
                thread::spawn(move || {
                    block_on(async move {
                        for i in 0..PER {
                            tx.send(i).await.unwrap();
                        }
                    })
                })
            })
            .collect();
        drop(tx);
        let sum = AtomicU64::new(0);
        let mut count = 0u64;
        block_on(async {
            while let Some(v) = rx.recv().await {
                sum.fetch_add(v, Ordering::Relaxed);
                count += 1;
            }
        });
        for p in ps {
            p.join().unwrap();
        }
        assert_eq!(count, NP * PER);
        assert_eq!(sum.load(Ordering::Relaxed), NP * (PER * (PER - 1) / 2));
    }
}
