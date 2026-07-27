//! Shared SPSC wakeup signaling.
//!
//! A single, audited flag-gated waker slot used across the `site_d` primitives,
//! so they share *one* wakeup model instead of hand-rolling it each time. Built
//! on a vendored [`AtomicWaker`] (no external dependency).

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::Waker;

use crate::hints::unlikely;

// ================== AtomicWaker ==================
//
// A `Waker` cell safe against a single registering task racing concurrent
// `wake()`s — the exact guarantee `WaiterSlot` needs (one waiter parks, the peer
// signals). This is the well-known three-state machine (as in `futures-util`),
// vendored so the crate stays dependency-free.

const WAITING: usize = 0; // no register/wake in progress
const REGISTERING: usize = 0b01; // a task is storing its waker
const WAKING: usize = 0b10; // a wake is taking the waker

/// A `Waker` slot supporting one registering task vs. concurrent wakers.
pub(crate) struct AtomicWaker {
    state: AtomicUsize,
    waker: UnsafeCell<Option<Waker>>,
}

// SAFETY: all access to `waker` is gated by the `state` lock bits, so it is
// never aliased concurrently; the contained `Waker` is itself `Send + Sync`.
unsafe impl Send for AtomicWaker {}
unsafe impl Sync for AtomicWaker {}

impl AtomicWaker {
    #[inline]
    pub(crate) const fn new() -> Self {
        Self {
            state: AtomicUsize::new(WAITING),
            waker: UnsafeCell::new(None),
        }
    }

    /// Register `waker` to be woken by a later [`wake`](Self::wake). At most one
    /// task may register at a time (the single waiter).
    pub(crate) fn register(&self, waker: &Waker) {
        match self
            .state
            .compare_exchange(WAITING, REGISTERING, Ordering::Acquire, Ordering::Acquire)
            .unwrap_or_else(|x| x)
        {
            WAITING => {
                // SAFETY: we hold the REGISTERING lock; exclusive access to the cell.
                unsafe {
                    // Skip the clone if the same waker is already stored.
                    let same = (*self.waker.get())
                        .as_ref()
                        .is_some_and(|w| w.will_wake(waker));
                    if !same {
                        *self.waker.get() = Some(waker.clone());
                    }
                    // Release the lock, observing any wake that raced us.
                    match self.state.compare_exchange(
                        REGISTERING,
                        WAITING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {}
                        Err(actual) => {
                            // A wake arrived during registration: take and fire it.
                            debug_assert_eq!(actual, REGISTERING | WAKING);
                            let waker = (*self.waker.get()).take().unwrap();
                            self.state.swap(WAITING, Ordering::AcqRel);
                            waker.wake();
                        }
                    }
                }
            }
            // A wake (or, defensively, another register) is in progress: wake the
            // new waker directly so the task re-polls.
            _ => waker.wake_by_ref(),
        }
    }

    /// Wake the registered task, if any.
    #[inline]
    pub(crate) fn wake(&self) {
        if let Some(waker) = self.take() {
            waker.wake();
        }
    }

    /// Take the registered waker, if it is free to take right now.
    fn take(&self) -> Option<Waker> {
        match self.state.fetch_or(WAKING, Ordering::AcqRel) {
            WAITING => {
                // SAFETY: we hold the WAKING lock; exclusive access to the cell.
                let waker = unsafe { (*self.waker.get()).take() };
                self.state.fetch_and(!WAKING, Ordering::Release);
                waker
            }
            // REGISTERING (the registerer will observe our WAKING bit and fire) or
            // a concurrent WAKING already taking the waker.
            _ => None,
        }
    }
}

// ================== WaiterSlot ==================

/// One direction of an SPSC wakeup: an [`AtomicWaker`] for the parked task plus
/// a rarely-written "is a waiter armed?" flag, so the signaling side can skip
/// the wake read-modify-write entirely when nobody is parked (the flag stays
/// clean in its cache and the check is a single load).
///
/// # Lost-wakeup safety — the contract callers MUST uphold
///
/// The flag check alone does **not** order against a peer that is concurrently
/// parking. Correctness relies on a store-buffer-free handshake that spans this
/// slot *and the caller's own published state*:
///
/// * **Signaling side** (just made the resource available): publish your state
///   with a `SeqCst` store, *then* call [`signal`](Self::signal). The `signal`
///   flag-load is `SeqCst`.
/// * **Waiting side** (about to park): call [`arm`](Self::arm), *then* re-check
///   your condition with a `SeqCst` load. If now satisfied, call
///   [`disarm`](Self::disarm) and proceed; otherwise return `Pending`.
///
/// A `SeqCst` publish-store + `SeqCst` flag-load on one side, paired with a
/// `SeqCst` arm-store + `SeqCst` re-check-load on the other, forbid the
/// store-buffer outcome where the waiter sleeps *and* the signaller misses the
/// flag — so the wakeup can never be lost, with no `fence` required (stlr/ldar
/// on AArch64; the SC store carries the barrier on x86).
pub(crate) struct WaiterSlot {
    waker: AtomicWaker,
    armed: AtomicBool,
}

impl WaiterSlot {
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            waker: AtomicWaker::new(),
            armed: AtomicBool::new(false),
        }
    }

    /// Waiting side: register `w` and arm the slot. The caller **must** re-check
    /// its condition (with a `SeqCst` load) after this returns, before parking.
    #[inline(always)]
    pub(crate) fn arm(&self, w: &Waker) {
        self.waker.register(w);
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Waiting side: disarm after a successful re-check (the resource became
    /// available, so we are no longer parking).
    #[inline(always)]
    pub(crate) fn disarm(&self) {
        self.armed.store(false, Ordering::Relaxed);
    }

    /// Signaling side: wake the parked waiter if one is armed. **Must** be
    /// called *after* the `SeqCst` publish store of the caller's state.
    ///
    /// Hot path is a single `SeqCst` load + predicted-not-taken branch; the
    /// claim-and-wake read-modify-write is cold-outlined.
    #[inline(always)]
    pub(crate) fn signal(&self) {
        // Cheap SeqCst gate: clean in our cache when nobody is parked. This is
        // the store-buffer-pairing load (see the type-level contract).
        if unlikely(self.armed.load(Ordering::SeqCst)) {
            self.signal_slow();
        }
    }

    #[cold]
    #[inline(never)]
    fn signal_slow(&self) {
        // Claim the flag so exactly one wake fires and the slot is reset.
        if self.armed.swap(false, Ordering::AcqRel) {
            self.waker.wake();
        }
    }
}
