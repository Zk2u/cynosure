//! A single-value, single-use cross-core channel.
//!
//! The sender delivers **exactly one** value; the receiver awaits it. This is
//! the lightweight reply path for request/response across cores — far cheaper
//! than a whole [`RingBuf`](super::ringbuf) when you only need one value back.
//!
//! Only the receiver ever parks, so it carries a single
//! `WaiterSlot` and upholds the same store-buffer-
//! free wakeup handshake: the sender publishes the value with a `SeqCst`
//! transition and `signal()`s; the receiver `arm`s then re-checks `SeqCst`.
//!
//! ```
//! # use cynosure::site_d::oneshot::oneshot;
//! # async fn ex() {
//! let (tx, rx) = oneshot::<u32>();
//! tx.send(42).unwrap();
//! assert_eq!(rx.await, Ok(42));
//! # }
//! ```

use std::{
    cell::UnsafeCell,
    future::Future,
    mem::MaybeUninit,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    task::{Context, Poll},
};

use super::notify::WaiterSlot;
use crate::hints::unlikely;

const VALUE_SET: u8 = 1 << 0;
const SENDER_DROPPED: u8 = 1 << 1;
const RECEIVER_DROPPED: u8 = 1 << 2;
const TAKEN: u8 = 1 << 3;

struct Inner<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    state: AtomicU8,
    recv: WaiterSlot, // only the receiver ever parks
}

// SAFETY: the value crosses cores exactly once, its visibility gated by the
// `SeqCst` `state` transitions; the cell is never aliased concurrently (single
// sender writes before `VALUE_SET`, single receiver reads after observing it).
unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}

impl<T> Inner<T> {
    /// Move the value out, exactly once. Returns `None` if already taken.
    ///
    /// `s` is the state the caller just loaded (having observed `VALUE_SET`).
    /// A plain load + store replaces the previous `fetch_or(TAKEN)` RMW: once
    /// `VALUE_SET` is observed there are **no concurrent writers** of `state` —
    /// the sender is done (`Sender::drop` after a successful send takes the
    /// no-write branch, same-thread program order), and every `TAKEN`/receiver
    /// transition happens on this single receiver thread. `Inner::drop` reads
    /// via `get_mut` behind the `Arc` teardown's release/acquire, so a
    /// `Relaxed` store is sufficient and compiles to a plain `strb` instead
    /// of an `ldsetalb` RMW on AArch64.
    #[inline]
    fn take_value(&self, s: u8) -> Option<T> {
        if unlikely(s & TAKEN != 0) {
            None
        } else {
            debug_assert!(s & VALUE_SET != 0);
            self.state.store(s | TAKEN, Ordering::Relaxed);
            // SAFETY: `VALUE_SET` was observed and `TAKEN` was not yet set, so the
            // cell holds an initialized value that we now uniquely own.
            Some(unsafe { (*self.value.get()).assume_init_read() })
        }
    }
}

impl<T> Drop for Inner<T> {
    fn drop(&mut self) {
        let s = *self.state.get_mut();
        if s & VALUE_SET != 0 && s & TAKEN == 0 {
            // Value was published but never taken (receiver vanished): drop it.
            // SAFETY: `VALUE_SET && !TAKEN` ⇒ the cell holds an initialized value
            // that no one else will read; exclusive access at drop.
            unsafe { (*self.value.get()).assume_init_drop() };
        }
    }
}

/// The sending half of a [`oneshot`] channel.
pub struct Sender<T> {
    inner: Arc<Inner<T>>,
}

/// The receiving half of a [`oneshot`] channel. Implements [`Future`]; `await`
/// it to get the value.
pub struct Receiver<T> {
    inner: Arc<Inner<T>>,
}

/// The sender was dropped without sending a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecvError;

/// Create a new oneshot channel.
pub fn oneshot<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Inner {
        value: UnsafeCell::new(MaybeUninit::uninit()),
        state: AtomicU8::new(0),
        recv: WaiterSlot::new(),
    });
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver { inner },
    )
}

impl<T> Sender<T> {
    /// Send `value`, waking the receiver if it is parked.
    ///
    /// Returns `Err(value)` (handing the value back) if the receiver was
    /// already dropped. If the receiver drops *concurrently* with a
    /// successful send, the value is delivered to the channel and then
    /// dropped unread — never leaked.
    pub fn send(self, value: T) -> Result<(), T> {
        let s = self.inner.state.load(Ordering::Acquire);
        if unlikely(s & RECEIVER_DROPPED != 0) {
            return Err(value);
        }
        // SAFETY: single sender, value not yet set ⇒ exclusive write to the cell.
        unsafe { (*self.inner.value.get()).write(value) };
        // Publish (SeqCst): pairs with the receiver's SeqCst state re-check and
        // the `recv.signal()` flag-load — the SB-free wakeup handshake. A SeqCst
        // *store* (AArch64 `stlr`) carries that pairing; the previous `fetch_or`
        // RMW bought nothing extra: the only bit a concurrent writer could add is
        // `RECEIVER_DROPPED`, and losing it merely re-creates the documented
        // "receiver drops concurrently with a successful send" outcome —
        // `Inner::drop` sees `VALUE_SET && !TAKEN` and drops the value unread.
        self.inner.state.store(s | VALUE_SET, Ordering::SeqCst);
        self.inner.recv.signal();
        Ok(())
    }

    /// `true` if the receiver has been dropped (a send would fail).
    pub fn is_closed(&self) -> bool {
        self.inner.state.load(Ordering::Acquire) & RECEIVER_DROPPED != 0
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // If we never published a value, signal closure so the receiver resolves
        // to `Err(RecvError)` instead of hanging.
        if self.inner.state.load(Ordering::Acquire) & VALUE_SET == 0 {
            self.inner.state.fetch_or(SENDER_DROPPED, Ordering::SeqCst);
            self.inner.recv.signal();
        }
    }
}

impl<T> Receiver<T> {
    /// Non-blocking receive: `Ok(Some(v))` if a value is ready, `Ok(None)` if
    /// not yet, `Err(RecvError)` if the sender dropped without sending.
    pub fn try_recv(&mut self) -> Result<Option<T>, RecvError> {
        let s = self.inner.state.load(Ordering::SeqCst);
        if s & VALUE_SET != 0 {
            Ok(self.inner.take_value(s))
        } else if s & SENDER_DROPPED != 0 {
            Err(RecvError)
        } else {
            Ok(None)
        }
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut(); // Receiver: Unpin (just an Arc)

        // Fast path.
        if let Some(out) = this.inner.resolve() {
            return Poll::Ready(out);
        }
        // Arm, then re-check to close the wakeup race (the WaiterSlot contract).
        this.inner.recv.arm(cx.waker());
        match this.inner.resolve() {
            Some(out) => {
                this.inner.recv.disarm();
                Poll::Ready(out)
            }
            None => Poll::Pending,
        }
    }
}

impl<T> Inner<T> {
    /// `Some(result)` if the channel has resolved (value ready or closed).
    #[inline]
    fn resolve(&self) -> Option<Result<T, RecvError>> {
        let s = self.state.load(Ordering::SeqCst);
        if s & VALUE_SET != 0 {
            Some(self.take_value(s).ok_or(RecvError))
        } else if s & SENDER_DROPPED != 0 {
            Some(Err(RecvError))
        } else {
            None
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.inner
            .state
            .fetch_or(RECEIVER_DROPPED, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::task::{RawWaker, RawWakerVTable, Waker};

    use super::*;

    fn noop_waker() -> Waker {
        fn no(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VT)
        }
        static VT: RawWakerVTable = RawWakerVTable::new(clone, no, no, no);
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VT)) }
    }

    #[test]
    fn send_then_recv() {
        let (tx, rx) = oneshot::<u32>();
        tx.send(7).unwrap();
        assert_eq!(crate::blocking::block_on(rx), Ok(7));
    }

    #[test]
    fn sender_dropped_is_err() {
        let (tx, rx) = oneshot::<u32>();
        drop(tx);
        assert_eq!(crate::blocking::block_on(rx), Err(RecvError));
    }

    #[test]
    fn receiver_dropped_send_fails() {
        let (tx, rx) = oneshot::<u32>();
        drop(rx);
        assert_eq!(tx.send(9), Err(9));
    }

    #[test]
    fn try_recv_states() {
        let (tx, mut rx) = oneshot::<u32>();
        assert_eq!(rx.try_recv(), Ok(None));
        tx.send(3).unwrap();
        assert_eq!(rx.try_recv(), Ok(Some(3)));
    }

    #[test]
    fn pending_then_ready_via_poll() {
        let (tx, mut rx) = oneshot::<u32>();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut rx).poll(&mut cx).is_pending());
        tx.send(5).unwrap();
        assert_eq!(Pin::new(&mut rx).poll(&mut cx), Poll::Ready(Ok(5)));
    }

    /// Race the sender's `store(s | VALUE_SET)` publish against a concurrent
    /// receiver drop — the exact interleaving the RMW→store change must keep
    /// safe. Whatever the outcome (Err(v) handed back, or
    /// delivered-then-dropped by `Inner::drop`), the value must be dropped
    /// exactly once: never leaked, never double-dropped.
    #[test]
    fn concurrent_send_vs_receiver_drop_never_leaks() {
        use std::sync::{
            Arc as StdArc,
            atomic::{AtomicUsize, Ordering as AO},
        };

        struct Counted(StdArc<AtomicUsize>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.fetch_add(1, AO::SeqCst);
            }
        }

        let iters = if cfg!(miri) { 40 } else { 2000 };
        for _ in 0..iters {
            let drops = StdArc::new(AtomicUsize::new(0));
            let (tx, rx) = oneshot::<Counted>();
            let t = std::thread::spawn(move || drop(rx));
            // Err(v) hands the value back and drops it here; Ok delivers it and
            // `Inner::drop` frees it when the last Arc goes.
            let _ = tx.send(Counted(drops.clone()));
            t.join().unwrap();
            assert_eq!(drops.load(AO::SeqCst), 1, "value dropped exactly once");
        }
    }

    /// Cross-thread send → recv: the SeqCst store publish must still carry the
    /// value and the wakeup (the SB-free handshake) with a real parked
    /// receiver.
    #[test]
    fn concurrent_send_recv_cross_thread() {
        let iters = if cfg!(miri) { 40 } else { 2000 };
        for i in 0..iters {
            let (tx, rx) = oneshot::<u64>();
            let t = std::thread::spawn(move || crate::blocking::block_on(rx));
            tx.send(i as u64).unwrap();
            assert_eq!(t.join().unwrap(), Ok(i as u64));
        }
    }

    #[test]
    fn value_dropped_if_receiver_vanishes() {
        use std::rc::Rc;
        let (tx, rx) = oneshot::<Rc<()>>();
        let marker = Rc::new(());
        tx.send(marker.clone()).unwrap();
        assert_eq!(Rc::strong_count(&marker), 2);
        drop(rx); // value set-but-not-taken ⇒ Inner::drop must drop it
        assert_eq!(Rc::strong_count(&marker), 1);
    }
}
