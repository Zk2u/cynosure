//! A lock-free SPSC **bip buffer** (bipartite ring): an IO byte buffer that
//! always hands out a single **contiguous** region for each read and write.
//!
//! Where a [`RingBuf`](super::ringbuf) of bytes gives you *two* slices at the
//! wrap, a bip buffer guarantees one — so reservations go straight to io_uring
//! / `readv` / DMA with zero copy. It uses the same wakeup discipline as the
//! ring (`WaiterSlot` + the `SeqCst` publish/re-check
//! handshake) but wraps with a **watermark** instead of power-of-2 masking, so
//! the capacity is arbitrary.
//!
//! # Reserve / commit
//!
//! The producer [`reserve`](Producer::reserve)s a contiguous region, writes
//! into it (or hands it to the kernel), then [`commit`](WriteGrant::commit)s
//! the bytes actually produced. The consumer [`read`](Consumer::read)s the
//! contiguous readable region and [`release`](ReadGrant::release)s what it
//! consumed. Grants are `Arc`-backed (`'static`) and implement
//! `IoBuf`/`IoBufMut` under `monoio-0_2`. Dropping a grant without
//! commit/release is safe: a write reservation is abandoned, a read is left
//! unconsumed.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

#[cfg(feature = "monoio-0_2")]
use monoio::buf::{IoBuf, IoBufMut};

use super::{buffer::AlignedBuffer, notify::WaiterSlot, padding::CachePadded};
use crate::hints::{likely, unlikely};

struct Inner {
    buf: AlignedBuffer<u8>,
    cap: usize,

    write: CachePadded<AtomicUsize>,
    read: CachePadded<AtomicUsize>,
    last: AtomicUsize,
    reserve: AtomicUsize,
    wip: AtomicBool,
    rip: AtomicBool,

    data: CachePadded<WaiterSlot>, // consumer parks here; producer signals on commit
    space: CachePadded<WaiterSlot>, // producer parks here; consumer signals on release/wrap
}

// SAFETY: bytes only; the SPSC index protocol gives each side exclusive access
// to its region, with `SeqCst` publish/acquire across the boundary.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

/// Create a bip buffer with a `cap`-byte backing buffer, naturally aligned.
pub fn bip_buffer(cap: usize) -> (Producer, Consumer) {
    bip_buffer_aligned(cap, 1)
}

/// Create a bip buffer with a `cap`-byte backing buffer aligned to `align`
/// (e.g. `4096` for O_DIRECT).
///
/// # Panics
/// Panics if `cap == 0` or `align` is not a power of two.
pub fn bip_buffer_aligned(cap: usize, align: usize) -> (Producer, Consumer) {
    let buf = AlignedBuffer::<u8>::with_alignment(cap, align);
    let inner = Arc::new(Inner {
        buf,
        cap,
        write: CachePadded::new(AtomicUsize::new(0)),
        read: CachePadded::new(AtomicUsize::new(0)),
        last: AtomicUsize::new(cap),
        reserve: AtomicUsize::new(0),
        wip: AtomicBool::new(false),
        rip: AtomicBool::new(false),
        data: CachePadded::new(WaiterSlot::new()),
        space: CachePadded::new(WaiterSlot::new()),
    });
    (
        Producer {
            inner: inner.clone(),
        },
        Consumer { inner },
    )
}

impl Inner {
    /// Raw byte pointer to `offset` within the backing buffer.
    #[inline]
    fn at(&self, offset: usize) -> *mut u8 {
        // SAFETY: callers pass offsets within `[0, cap]`.
        unsafe { (self.buf.as_ptr() as *mut u8).add(offset) }
    }

    /// Try to reserve `n` contiguous bytes; returns the region `[start,
    /// start+n)`.
    fn try_reserve(&self, n: usize) -> Option<(usize, usize)> {
        if unlikely(self.wip.load(Ordering::Relaxed)) {
            return None; // a write grant is already outstanding
        }
        // A contiguous region can never exceed the capacity. Reject oversized
        // requests up front so the space arithmetic below cannot overflow: an
        // unchecked `write + n` wraps in release builds, and a wrapped sum can
        // slip past the capacity checks and hand out a grant whose `len` runs off
        // the end of the buffer (out-of-bounds write via `as_mut_slice`).
        if unlikely(n > self.cap) {
            return None;
        }
        let read = self.read.load(Ordering::SeqCst);
        let write = self.write.load(Ordering::Relaxed);

        let start = if write < read {
            // Inverted: free space is the gap (write, read). Keep one byte so
            // `write` can never become equal to `read` from behind (ambiguity).
            // `read - write` can't underflow (write < read); `n <= cap`, no overflow.
            if likely(n < read - write) {
                write
            } else {
                return None;
            }
        } else if n <= self.cap - write {
            write // fits in the tail (`cap - write` can't underflow: write <= cap)
        } else if n < read {
            0 // wrap to the head
        } else {
            return None;
        };

        self.reserve.store(start + n, Ordering::Relaxed);
        self.wip.store(true, Ordering::Relaxed);
        Some((start, n))
    }

    /// Try to get the contiguous readable region; returns `[start, start+len)`.
    fn try_read(&self) -> Option<(usize, usize)> {
        if unlikely(self.rip.load(Ordering::Relaxed)) {
            return None;
        }
        let write = self.write.load(Ordering::SeqCst);
        let mut read = self.read.load(Ordering::Relaxed);
        let last = self.last.load(Ordering::Acquire);

        // Consumed up to the watermark while inverted: wrap to the head. This
        // frees the upper region, so wake a producer waiting on space.
        if write < read && read == last {
            read = 0;
            self.read.store(0, Ordering::SeqCst);
            self.space.signal();
        }

        let avail_end = if write < read { last } else { write };
        let len = avail_end - read;
        if len == 0 {
            return None;
        }
        self.rip.store(true, Ordering::Relaxed);
        Some((read, len))
    }
}

/// The producer half of a bip buffer.
pub struct Producer {
    inner: Arc<Inner>,
}

impl Producer {
    /// Reserve `n` contiguous bytes without waiting; `None` if there isn't a
    /// contiguous run of `n` free bytes (or a grant is already outstanding).
    #[inline]
    pub fn try_reserve(&mut self, n: usize) -> Option<WriteGrant> {
        self.inner.try_reserve(n).map(|(start, len)| WriteGrant {
            inner: self.inner.clone(),
            start,
            len,
            handed_back: false,
        })
    }

    /// Reserve `n` contiguous bytes, waiting until space is available.
    #[inline]
    pub fn reserve(&mut self, n: usize) -> Reserve<'_> {
        Reserve { producer: self, n }
    }

    /// Total backing capacity in bytes.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner.cap
    }
}

/// The consumer half of a bip buffer.
pub struct Consumer {
    inner: Arc<Inner>,
}

impl Consumer {
    /// Get the contiguous readable region without waiting; `None` if empty (or
    /// a read grant is already outstanding).
    #[inline]
    pub fn try_read(&mut self) -> Option<ReadGrant> {
        self.inner.try_read().map(|(start, len)| ReadGrant {
            inner: self.inner.clone(),
            start,
            len,
            handed_back: false,
        })
    }

    /// Get the contiguous readable region, waiting until data is available.
    #[inline]
    pub fn read(&mut self) -> Read<'_> {
        Read { consumer: self }
    }

    /// Total backing capacity in bytes.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner.cap
    }
}

/// An exclusive, contiguous, writable region reserved from the producer.
pub struct WriteGrant {
    inner: Arc<Inner>,
    start: usize,
    len: usize,
    /// Set by [`commit`](Self::commit) once it has handed the in-progress flag
    /// back, so [`Drop`] does not clear it a *second* time. Without this, the
    /// window between `commit`'s clear and the grant's drop spans
    /// `data.signal()` — long enough for a woken waiter (or the producer on
    /// another thread; grants are `Send`) to take a fresh grant, which the old
    /// grant's drop would then silently release, handing out two overlapping
    /// `&mut [u8]` over the same bytes.
    handed_back: bool,
}

impl WriteGrant {
    /// The writable bytes.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the reservation owns `[start, start+len)` exclusively until
        // commit/drop, and the whole backing buffer is initialized.
        unsafe { std::slice::from_raw_parts_mut(self.inner.at(self.start), self.len) }
    }

    /// Reserved length in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` if the reservation is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Publish `used` bytes (`<= len`) to the consumer, waking it if parked.
    pub fn commit(mut self, used: usize) {
        let used = used.min(self.len);
        // Mark first: set before the clear so that even a panic unwinding out of
        // `signal()` leaves `Drop` a no-op rather than a second clear.
        self.handed_back = true;
        let inner = &self.inner;
        let write = inner.write.load(Ordering::Relaxed);
        // Wrapped (reserved at the head while data remained in the tail): record
        // the watermark — the end of the upper region — before publishing.
        if self.start == 0 && write != 0 {
            inner.last.store(write, Ordering::Relaxed);
        }
        // Publish (SeqCst): pairs with the consumer's SeqCst `write` re-check and
        // the `data.signal()` flag-load — the SB-free wakeup handshake.
        inner.write.store(self.start + used, Ordering::SeqCst);
        inner.wip.store(false, Ordering::Relaxed);
        inner.data.signal();
        // `self` drops here; `handed_back` makes that drop a no-op.
    }
}

impl Drop for WriteGrant {
    fn drop(&mut self) {
        // `commit` already handed the flag back (and published); clearing it
        // again could release a *different*, still-live grant.
        if self.handed_back {
            return;
        }
        // Abandon the reservation if it was never committed (nothing published).
        if self.inner.wip.swap(false, Ordering::Relaxed) {
            let w = self.inner.write.load(Ordering::Relaxed);
            self.inner.reserve.store(w, Ordering::Relaxed);
        }
    }
}

/// An exclusive, contiguous, readable region taken from the consumer.
pub struct ReadGrant {
    inner: Arc<Inner>,
    start: usize,
    len: usize,
    /// See [`WriteGrant::handed_back`] — same hazard on the read side.
    handed_back: bool,
}

impl ReadGrant {
    /// The readable bytes.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: the grant owns `[start, start+len)` until release/drop.
        unsafe { std::slice::from_raw_parts(self.inner.at(self.start), self.len) }
    }

    /// Readable length in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` if the readable region is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Mark `used` bytes (`<= len`) consumed, waking the producer if parked.
    pub fn release(mut self, used: usize) {
        let used = used.min(self.len);
        // See `WriteGrant::commit` — mark before the clear.
        self.handed_back = true;
        let inner = &self.inner;
        let read = inner.read.load(Ordering::Relaxed);
        // Publish (SeqCst): pairs with the producer's SeqCst `read` re-check and
        // the `space.signal()` flag-load.
        inner.read.store(read + used, Ordering::SeqCst);
        inner.rip.store(false, Ordering::Relaxed);
        inner.space.signal();
    }
}

impl Drop for ReadGrant {
    fn drop(&mut self) {
        // `release` already handed the flag back — see `WriteGrant::drop`.
        if self.handed_back {
            return;
        }
        // Not released: leave the data readable (read unchanged), just clear the
        // in-progress flag.
        self.inner.rip.store(false, Ordering::Relaxed);
    }
}

#[cfg(feature = "monoio-0_2")]
// SAFETY: the grant owns a contiguous, initialized region for its lifetime.
unsafe impl IoBufMut for WriteGrant {
    fn write_ptr(&mut self) -> *mut u8 {
        self.inner.at(self.start)
    }
    fn bytes_total(&mut self) -> usize {
        self.len
    }
    unsafe fn set_init(&mut self, _pos: usize) {}
}

#[cfg(feature = "monoio-0_2")]
// SAFETY: see above.
unsafe impl IoBuf for ReadGrant {
    fn read_ptr(&self) -> *const u8 {
        self.inner.at(self.start)
    }
    fn bytes_init(&self) -> usize {
        self.len
    }
}

/// Future returned by [`Producer::reserve`].
pub struct Reserve<'a> {
    producer: &'a mut Producer,
    n: usize,
}

impl<'a> std::future::Future for Reserve<'a> {
    type Output = WriteGrant;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(g) = this.producer.try_reserve(this.n) {
            return Poll::Ready(g);
        }
        // Arm `space`, then re-check (the WaiterSlot SB-free handshake).
        this.producer.inner.space.arm(cx.waker());
        match this.producer.try_reserve(this.n) {
            Some(g) => {
                this.producer.inner.space.disarm();
                Poll::Ready(g)
            }
            None => Poll::Pending,
        }
    }
}

/// Future returned by [`Consumer::read`].
pub struct Read<'a> {
    consumer: &'a mut Consumer,
}

impl<'a> std::future::Future for Read<'a> {
    type Output = ReadGrant;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(g) = this.consumer.try_read() {
            return Poll::Ready(g);
        }
        this.consumer.inner.data.arm(cx.waker());
        match this.consumer.try_read() {
            Some(g) => {
                this.consumer.inner.data.disarm();
                Poll::Ready(g)
            }
            None => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(c: &mut Consumer) -> Vec<u8> {
        match c.try_read() {
            Some(g) => {
                let v = g.as_slice().to_vec();
                let n = v.len();
                g.release(n);
                v
            }
            None => vec![],
        }
    }

    #[test]
    fn basic_reserve_commit_read_release() {
        let (mut p, mut c) = bip_buffer(16);
        let mut g = p.try_reserve(4).unwrap();
        g.as_mut_slice().copy_from_slice(&[1, 2, 3, 4]);
        g.commit(4);
        assert_eq!(drain(&mut c), vec![1, 2, 3, 4]);
        assert_eq!(drain(&mut c), Vec::<u8>::new()); // empty now
    }

    #[test]
    fn contiguous_wrap_uses_watermark() {
        // cap 8: fill 6, read 6, then a 4-byte reserve must wrap to the head and
        // stay contiguous (only 2 bytes free in the tail).
        let (mut p, mut c) = bip_buffer(8);
        let mut g = p.try_reserve(6).unwrap();
        g.as_mut_slice().copy_from_slice(&[10, 11, 12, 13, 14, 15]);
        g.commit(6);
        assert_eq!(drain(&mut c), vec![10, 11, 12, 13, 14, 15]);

        // tail has only 2 bytes (write=6, cap=8); a 4-byte reserve wraps to 0.
        let mut g = p.try_reserve(4).unwrap();
        assert_eq!(g.as_mut_slice().len(), 4, "wrapped grant is contiguous");
        g.as_mut_slice().copy_from_slice(&[20, 21, 22, 23]);
        g.commit(4);
        assert_eq!(drain(&mut c), vec![20, 21, 22, 23]);
    }

    #[test]
    fn reserve_fails_when_full() {
        let (mut p, _c) = bip_buffer(4);
        // The non-inverted tail can fill all the way to `cap` (no gap needed
        // when read == 0); after that the buffer is full.
        let g = p.try_reserve(4).unwrap();
        g.commit(4);
        assert!(p.try_reserve(1).is_none(), "buffer is full");
    }

    #[test]
    fn uncommitted_grant_is_rolled_back() {
        let (mut p, mut c) = bip_buffer(8);
        {
            let mut g = p.try_reserve(4).unwrap();
            g.as_mut_slice()[0] = 9;
            // dropped without commit
        }
        assert_eq!(drain(&mut c), Vec::<u8>::new(), "nothing published");
        // and we can reserve again (the reservation was abandoned)
        let g = p.try_reserve(8).unwrap();
        assert_eq!(g.len(), 8);
    }

    #[test]
    fn single_outstanding_write_grant() {
        let (mut p, _c) = bip_buffer(16);
        let _g = p.try_reserve(2).unwrap();
        assert!(p.try_reserve(2).is_none(), "only one write grant at a time");
    }

    /// A committed grant's `Drop` must not clear the in-progress flag a second
    /// time — otherwise a grant taken *during* `commit`'s `signal()` (grants
    /// are `Send`, and a waker runs arbitrary safe code) is silently
    /// released while still live, and the next reserve hands out an
    /// overlapping `&mut [u8]`.
    ///
    /// Reproduced deterministically by parking a consumer whose waker takes a
    /// fresh write grant from inside the wake.
    #[test]
    fn commit_drop_cannot_release_a_later_grant() {
        use std::{
            pin::Pin,
            sync::Mutex,
            task::{Wake, Waker},
        };

        struct Reenter {
            producer: Mutex<Option<Producer>>,
            grant: Mutex<Option<WriteGrant>>,
        }
        impl Wake for Reenter {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                // Runs inside `commit`'s `signal()`, i.e. after the first grant
                // cleared the flag but before it has dropped. Park the new grant
                // so it outlives that drop.
                if let Some(p) = self.producer.lock().unwrap().as_mut() {
                    *self.grant.lock().unwrap() = p.try_reserve(8);
                }
            }
        }

        let (mut p, mut c) = bip_buffer(64);
        let first = p.try_reserve(8).expect("first grant");

        let re = Arc::new(Reenter {
            producer: Mutex::new(Some(p)),
            grant: Mutex::new(None),
        });
        // Arm the consumer so `commit`'s `data.signal()` fires our waker.
        let waker: Waker = re.clone().into();
        let mut cx = Context::from_waker(&waker);
        let mut read_fut = c.read();
        assert!(Pin::new(&mut read_fut).poll(&mut cx).is_pending());

        first.commit(8); // clears the flag, signals (taking grant #2), then drops

        let second = re
            .grant
            .lock()
            .unwrap()
            .take()
            .expect("grant taken in wake");
        // Grant #2 is still alive, so the buffer must refuse a third.
        let mut prod = re.producer.lock().unwrap();
        let p = prod.as_mut().unwrap();
        assert!(
            p.try_reserve(8).is_none(),
            "a live write grant must block another: the committed grant's Drop \
             cleared the flag a second time and released grant #2"
        );
        drop(second);
        assert!(p.try_reserve(8).is_some(), "flag released once #2 drops");
    }

    #[test]
    fn oversized_reserve_rejected_no_overflow() {
        // Regression: `write + n` must not wrap past the capacity checks and hand
        // out a grant whose `len` runs off the end of the buffer. After a commit
        // advances `write`, a near-`usize::MAX` reserve previously wrapped and was
        // accepted (out-of-bounds); it must now be rejected.
        let (mut p, _c) = bip_buffer(16);
        let g = p.try_reserve(1).unwrap();
        g.commit(1); // write = 1
        assert!(
            p.try_reserve(usize::MAX).is_none(),
            "wrapping reserve must be rejected"
        );
        assert!(p.try_reserve(17).is_none(), "n > cap must be rejected");
        assert!(p.try_reserve(usize::MAX - 1).is_none());
        // A legitimate reserve still works.
        assert!(p.try_reserve(4).is_some());
    }

    #[test]
    fn partial_commit_and_release() {
        let (mut p, mut c) = bip_buffer(16);
        let mut g = p.try_reserve(8).unwrap();
        g.as_mut_slice()[..3].copy_from_slice(&[1, 2, 3]);
        g.commit(3); // only 3 of 8

        let rg = c.try_read().unwrap();
        assert_eq!(rg.as_slice(), &[1, 2, 3]);
        rg.release(2); // consume only 2
        let rg = c.try_read().unwrap();
        assert_eq!(rg.as_slice(), &[3], "the un-released byte remains");
        rg.release(1);
        assert!(c.try_read().is_none());
    }
}
