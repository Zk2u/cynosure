//! A per-core pool of recyclable, aligned IO buffers.
//!
//! Keeps `alloc`/`free` off the hot path: take a buffer, hand it to io_uring,
//! and it returns itself to the pool on drop. Single-core, `!Send`, `Rc`-shared
//! — so a [`PooledBuffer`] is `'static` and goes straight to a completion-based
//! runtime (it implements `IoBuf`/`IoBufMut` under the `monoio-0_2` feature).
//!
//! A pool *is* a [`LocalSemaphore`] whose permits are buffers: `acquire`
//! reuses the semaphore's FIFO-fair waiting and cancellation safety, and
//! returning a buffer releases a permit, waking the next waiter.

use std::{
    cell::UnsafeCell,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    rc::Rc,
};

use super::semaphore::LocalSemaphore;
use crate::site_d::buffer::{AlignedBuffer, Zeroable};

#[cfg(feature = "monoio-0_2")]
use monoio::buf::{IoBuf, IoBufMut};

struct PoolInner<T: Zeroable + Copy> {
    // Invariant: `free.len() == sem.available()` at every suspension point.
    // `UnsafeCell` (not `RefCell`) — single-core, and each access is a scoped
    // pop/push that never holds the borrow across a callback (matching the
    // semaphore's `waiters`), so no runtime borrow check is needed.
    free: UnsafeCell<Vec<AlignedBuffer<T>>>,
    sem: LocalSemaphore,
}

/// A cloneable handle to a per-core pool of recyclable aligned `T` buffers.
///
/// `LocalBufferPool<u8>` is the IO case (its `PooledBuffer<u8>` is an `IoBuf`);
/// other element types give aligned typed-buffer pools (e.g. `f32` audio/DSP
/// frames, `u32` pixel rows).
pub struct LocalBufferPool<T: Zeroable + Copy> {
    inner: Rc<PoolInner<T>>,
}

// Hand-written so the handle is `Clone` regardless of whether `T: Clone`.
impl<T: Zeroable + Copy> Clone for LocalBufferPool<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Zeroable + Copy> LocalBufferPool<T> {
    /// Pre-allocate `count` buffers of `buf_size` elements, aligned to
    /// `align_of::<T>()`.
    pub fn new(count: usize, buf_size: usize) -> Self {
        Self::with_alignment(count, buf_size, std::mem::align_of::<T>())
    }

    /// Pre-allocate `count` buffers of `buf_size` elements, aligned to `align`
    /// bytes (e.g. `4096` for O_DIRECT).
    pub fn with_alignment(count: usize, buf_size: usize, align: usize) -> Self {
        let mut free = Vec::with_capacity(count);
        for _ in 0..count {
            free.push(AlignedBuffer::<T>::with_alignment(buf_size, align));
        }
        Self {
            inner: Rc::new(PoolInner {
                free: UnsafeCell::new(free),
                sem: LocalSemaphore::new(count),
            }),
        }
    }

    /// Buffers currently available.
    #[inline]
    pub fn available(&self) -> usize {
        self.inner.sem.available()
    }

    /// Take a buffer without waiting; `None` if the pool is exhausted.
    #[inline]
    pub fn try_acquire(&self) -> Option<PooledBuffer<T>> {
        let permit = self.inner.sem.try_acquire()?;
        permit.forget(); // ownership of the permit moves into the PooledBuffer
        Some(self.check_out())
    }

    /// Take a buffer, waiting (FIFO-fair) until one is returned if exhausted.
    pub async fn acquire(&self) -> PooledBuffer<T> {
        let permit = self.inner.sem.acquire().await;
        permit.forget();
        self.check_out()
    }

    /// Pop a buffer for an already-acquired permit. The semaphore guarantees one
    /// is free (`free.len() == available()`), and single-threadedness means no
    /// task runs between the permit grant and this pop.
    #[inline]
    fn check_out(&self) -> PooledBuffer<T> {
        // SAFETY: single-threaded; the borrow is dropped before any callback.
        let mut buf =
            unsafe { (*self.inner.free.get()).pop() }.expect("permit implies a free buffer");
        buf.set_len(0); // hand out a clean logical length (contents are not re-zeroed)
        PooledBuffer {
            buf: ManuallyDrop::new(buf),
            pool: self.inner.clone(),
        }
    }
}

impl<T: Zeroable + Copy> std::fmt::Debug for LocalBufferPool<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalBufferPool")
            .field("available", &self.available())
            .finish()
    }
}

/// A buffer checked out from a [`LocalBufferPool`]; returns itself on drop.
///
/// Derefs to its [`AlignedBuffer<T>`] (use `capacity_mut`/`set_len` to fill).
pub struct PooledBuffer<T: Zeroable + Copy> {
    buf: ManuallyDrop<AlignedBuffer<T>>,
    pool: Rc<PoolInner<T>>,
}

impl<T: Zeroable + Copy> Deref for PooledBuffer<T> {
    type Target = AlignedBuffer<T>;
    #[inline]
    fn deref(&self) -> &AlignedBuffer<T> {
        &self.buf
    }
}

impl<T: Zeroable + Copy> DerefMut for PooledBuffer<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut AlignedBuffer<T> {
        &mut self.buf
    }
}

impl<T: Zeroable + Copy> Drop for PooledBuffer<T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `buf` is taken exactly once, here.
        let buf = unsafe { ManuallyDrop::take(&mut self.buf) };
        // Return the buffer FIRST, then release the permit, so any waiter woken
        // by the release (even a re-entrant one) finds the buffer present. The
        // push borrow is dropped before `add_permits` can re-enter `check_out`.
        // SAFETY: single-threaded; no borrow held across the wake.
        unsafe { (*self.pool.free.get()).push(buf) };
        self.pool.sem.add_permits(1);
    }
}

impl<T: Zeroable + Copy> std::fmt::Debug for PooledBuffer<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledBuffer")
            .field("len", &self.buf.len())
            .field("capacity", &self.buf.capacity())
            .finish()
    }
}

#[cfg(feature = "monoio-0_2")]
// SAFETY: delegates to the inner AlignedBuffer<u8>, which is a valid IoBuf for
// the lifetime of the PooledBuffer (it owns the buffer until drop). Only the
// `u8` instantiation is an IoBuf — io_uring is byte-oriented.
unsafe impl IoBuf for PooledBuffer<u8> {
    fn read_ptr(&self) -> *const u8 {
        self.buf.read_ptr()
    }
    fn bytes_init(&self) -> usize {
        self.buf.bytes_init()
    }
}

#[cfg(feature = "monoio-0_2")]
// SAFETY: see `IoBuf`.
unsafe impl IoBufMut for PooledBuffer<u8> {
    fn write_ptr(&mut self) -> *mut u8 {
        self.buf.write_ptr()
    }
    fn bytes_total(&mut self) -> usize {
        self.buf.bytes_total()
    }
    unsafe fn set_init(&mut self, pos: usize) {
        unsafe { self.buf.set_init(pos) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::pin;
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicBool, Ordering as AO};
    use std::task::{Context, Wake, Waker};

    struct FlagWaker(StdArc<AtomicBool>);
    impl Wake for FlagWaker {
        fn wake(self: StdArc<Self>) {
            self.0.store(true, AO::SeqCst);
        }
    }
    fn flag_waker() -> (Waker, StdArc<AtomicBool>) {
        let f = StdArc::new(AtomicBool::new(false));
        (StdArc::new(FlagWaker(f.clone())).into(), f)
    }

    #[test]
    fn acquire_return_cycle() {
        let pool = LocalBufferPool::<u8>::new(2, 64);
        assert_eq!(pool.available(), 2);
        let a = pool.try_acquire().unwrap();
        let b = pool.try_acquire().unwrap();
        assert_eq!(a.capacity(), 64);
        assert!(pool.try_acquire().is_none());
        drop(a);
        assert_eq!(pool.available(), 1);
        drop(b);
        assert_eq!(pool.available(), 2);
    }

    #[test]
    fn writes_visible_through_pooled_buffer() {
        let pool = LocalBufferPool::<u8>::new(1, 16);
        let mut buf = pool.try_acquire().unwrap();
        buf.capacity_mut()[0] = 0xAB;
        buf.set_len(1);
        assert_eq!(&buf[..], &[0xAB]);
    }

    #[test]
    fn generic_over_element_type() {
        // An aligned `f32` buffer pool (e.g. DSP/audio frames), 16-byte aligned.
        let pool = LocalBufferPool::<f32>::with_alignment(2, 256, 16);
        let mut a = pool.try_acquire().unwrap();
        assert_eq!(a.capacity(), 256);
        assert_eq!(a.as_ptr() as usize % 16, 0);
        a.capacity_mut()[0] = 1.5;
        a.set_len(1);
        assert_eq!(&a[..], &[1.5]);
        drop(a);
        assert_eq!(pool.available(), 2);
    }

    #[test]
    fn exhaustion_waits_then_return_wakes() {
        use std::future::Future;
        let pool = LocalBufferPool::<u8>::new(1, 8);
        let held = pool.try_acquire().unwrap();

        let (w, flag) = flag_waker();
        let mut cx = Context::from_waker(&w);
        let mut fut = pin!(pool.acquire());
        assert!(fut.as_mut().poll(&mut cx).is_pending());

        drop(held); // returns the buffer -> wakes the waiter
        assert!(flag.load(AO::SeqCst));
        match fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(b) => assert_eq!(b.capacity(), 8),
            std::task::Poll::Pending => panic!("should acquire after return"),
        }
    }

    #[test]
    fn buffers_are_recycled_not_reallocated() {
        let pool = LocalBufferPool::<u8>::new(1, 32);
        let a = pool.try_acquire().unwrap();
        let ptr_a = a.as_ptr();
        drop(a);
        let b = pool.try_acquire().unwrap();
        assert_eq!(
            b.as_ptr(),
            ptr_a,
            "the same buffer should be handed back out"
        );
    }
}
