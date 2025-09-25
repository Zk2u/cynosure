use futures::task::AtomicWaker;
#[cfg(feature = "monoio-0_2")]
use monoio::buf::{IoBuf, IoBufMut};
use std::{
    alloc::{Layout, alloc_zeroed, dealloc},
    cell::Cell,
    future::Future,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    pin::Pin,
    ptr::{self, NonNull},
    sync::{
        Arc,
        atomic::{AtomicPtr, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use crate::hints::{likely, unlikely};
use crate::site_d::padding::CachePadded;

/// 4 MiB buffer size optimized for NVMe and O_DIRECT
pub const BUFFER_SIZE: usize = 4 * 1024 * 1024;
pub const BUFFER_ALIGN: usize = 4096;

/// Owning, properly-aligned buffer for O_DIRECT I/O.
///
/// - Allocates with alignment `BUFFER_ALIGN`.
/// - Deallocates with the same alignment (no UB).
/// - Can be converted to/from a raw data pointer to circulate through atomics without extra heap allocations.
pub struct AlignedBuffer {
    ptr: NonNull<u8>,
    len: usize,
    cap: usize,
}

impl AlignedBuffer {
    fn layout() -> Layout {
        Layout::from_size_align(BUFFER_SIZE, BUFFER_ALIGN).expect("invalid layout")
    }

    /// Allocate a new zeroed, aligned buffer.
    pub fn new() -> Self {
        let layout = Self::layout();
        let ptr = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));

        // Debug-check alignment
        debug_assert_eq!((ptr.as_ptr() as usize) % BUFFER_ALIGN, 0);

        Self {
            ptr,
            len: 0,
            cap: BUFFER_SIZE,
        }
    }

    /// Convert into a raw data pointer and the initialized length.
    /// Caller becomes responsible to reconstruct with `from_raw_with_len`.
    pub fn into_raw(self) -> (*mut u8, usize) {
        let p = self.ptr.as_ptr();
        let len = self.len;
        std::mem::forget(self);
        (p, len)
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Reconstruct with explicit initialized length.
    ///
    /// # Safety
    ///
    /// `ptr` must be produced by `AlignedBuffer::into_raw` and refer to a buffer
    /// with `cap == BUFFER_SIZE`. `len` must be <= `BUFFER_SIZE`.
    pub unsafe fn from_raw_with_len(ptr: *mut u8, len: usize) -> Self {
        debug_assert!(!ptr.is_null());
        debug_assert_eq!((ptr as usize) % BUFFER_ALIGN, 0);
        debug_assert!(len <= BUFFER_SIZE);
        Self {
            ptr: unsafe { NonNull::new_unchecked(ptr) },
            len,
            cap: BUFFER_SIZE,
        }
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }
}

impl Default for AlignedBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for AlignedBuffer {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for AlignedBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr(), Self::layout()) }
    }
}

unsafe impl Send for AlignedBuffer {}
unsafe impl Sync for AlignedBuffer {} // optional; safe as explained

#[cfg(feature = "monoio-0_2")]
unsafe impl IoBuf for AlignedBuffer {
    fn read_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.len
    }
}

#[cfg(feature = "monoio-0_2")]
unsafe impl IoBufMut for AlignedBuffer {
    fn write_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    fn bytes_total(&mut self) -> usize {
        self.cap
    }

    unsafe fn set_init(&mut self, pos: usize) {
        self.len = pos;
    }
}

// ================== Internal shared state (hidden) ==================

struct SharedState {
    // Index of the buffer currently in the middle slot.
    middle_idx: AtomicUsize,
    // Monotonic generation of published buffers (wraps on usize overflow).
    generation: AtomicUsize,
    // Last generation observed and committed by the reader.
    last_read_gen: AtomicUsize,
}

struct Waiters {
    reader: AtomicWaker, // one reader task
    writer: AtomicWaker, // one writer task
}

/// Internal lock-free triple buffer with async backpressure (SPSC).
///
/// Hidden from public API; accessed only via TripleBufWriter/TripleBufReader.
struct TripleBuffer {
    // Three buffer data pointers; at most one slot can be null (held out-of-structure by writer or reader).
    buffers: [AtomicPtr<u8>; 3],
    // Lengths of the buffers in each slot.
    lens: [AtomicUsize; 3],

    // === Writer's cache line ===
    writer_idx: CachePadded<AtomicUsize>,

    // === Reader's cache line ===
    reader_idx: CachePadded<AtomicUsize>,

    // === Shared cache line ===
    shared_state: CachePadded<SharedState>,

    // === Wakers (not on hot path, but kept padded anyway) ===
    waiters: CachePadded<Waiters>,
}

impl TripleBuffer {
    /// Create a new triple buffer and return it along with the initial writer-owned buffer.
    fn new() -> (Self, AlignedBuffer) {
        let b0 = AlignedBuffer::new();
        let b1 = AlignedBuffer::new();
        let b2 = AlignedBuffer::new();

        let buffer = Self {
            buffers: [
                AtomicPtr::new(ptr::null_mut()), // Writer holds b0
                AtomicPtr::new(b1.into_raw().0), // Stored in slot 1
                AtomicPtr::new(b2.into_raw().0), // Stored in slot 2
            ],
            lens: [
                AtomicUsize::new(0), // Length of b0
                AtomicUsize::new(0), // Length of b1
                AtomicUsize::new(0), // Length of b2
            ],
            writer_idx: CachePadded::new(AtomicUsize::new(0)), // Writer starts with index 0
            reader_idx: CachePadded::new(AtomicUsize::new(1)), // Reader starts with index 1
            shared_state: CachePadded::new(SharedState {
                middle_idx: AtomicUsize::new(2), // Middle starts at index 2
                generation: AtomicUsize::new(0),
                last_read_gen: AtomicUsize::new(0),
            }),
            waiters: CachePadded::new(Waiters {
                reader: AtomicWaker::new(),
                writer: AtomicWaker::new(),
            }),
        };

        (buffer, b0) // writer starts with b0 in hand
    }

    // ------------- Wake/registration -------------

    #[inline(always)]
    fn wake_reader(&self) {
        self.waiters.as_ref().reader.wake();
    }

    #[inline(always)]
    fn wake_writer(&self) {
        self.waiters.as_ref().writer.wake();
    }

    #[inline(always)]
    fn register_reader_waker(&self, w: &std::task::Waker) {
        self.waiters.as_ref().reader.register(w);
    }

    #[inline(always)]
    fn register_writer_waker(&self, w: &std::task::Waker) {
        self.waiters.as_ref().writer.register(w);
    }

    // ------------- State checks -------------

    /// Returns true if there is a published-but-unread buffer.
    #[inline(always)]
    fn has_unread(&self) -> bool {
        let generation = self
            .shared_state
            .as_ref()
            .generation
            .load(Ordering::Acquire);
        let last_read = self
            .shared_state
            .as_ref()
            .last_read_gen
            .load(Ordering::Acquire);
        generation != last_read
    }

    /// Returns true if writer can publish (no unread data in middle).
    #[inline(always)]
    fn middle_free(&self) -> bool {
        let generation = self
            .shared_state
            .as_ref()
            .generation
            .load(Ordering::Acquire);
        let last_read = self
            .shared_state
            .as_ref()
            .last_read_gen
            .load(Ordering::Acquire);
        generation == last_read
    }

    // ------------- Core operations (sync, internal) -------------

    /// Writer publishes its completed buffer, rotating it with the middle.
    /// Precondition: `middle_free()` should be true (checked by async wrapper).
    #[inline(always)]
    fn writer_publish_now(&self, completed: AlignedBuffer) -> AlignedBuffer {
        // Convert to raw pointer for atomic slot
        let (completed_ptr, completed_len) = completed.into_raw();

        // Load current indices (current view)
        let writer_idx = self.writer_idx.as_ref().load(Ordering::Relaxed);
        let middle_idx = self
            .shared_state
            .as_ref()
            .middle_idx
            .load(Ordering::Acquire);

        // Return completed buffer to writer slot; it must be empty
        let old = self.buffers[writer_idx].swap(completed_ptr, Ordering::Release);
        debug_assert!(old.is_null(), "writer slot not empty");
        self.lens[writer_idx].store(completed_len, Ordering::Relaxed);

        // Rotate indices: writer takes middle; middle becomes old writer slot
        self.writer_idx
            .as_ref()
            .store(middle_idx, Ordering::Relaxed);
        self.shared_state
            .as_ref()
            .middle_idx
            .store(writer_idx, Ordering::Relaxed);

        // Increment generation to publish; pairs with reader's Acquire observations
        self.shared_state
            .as_ref()
            .generation
            .fetch_add(1, Ordering::Release);

        // Take buffer from what was middle (now writer's)
        let ptr = self.buffers[middle_idx].swap(ptr::null_mut(), Ordering::Acquire);
        if unlikely(ptr.is_null()) {
            panic!("Invariant violated: middle buffer pointer was null");
        }

        let next = unsafe { AlignedBuffer::from_raw_with_len(ptr, 0) };

        // Notify reader that new data is ready
        self.wake_reader();

        next
    }

    /// Reader consumes latest buffer if present, returning it.
    /// Precondition: `has_unread()` must be true (checked by async wrapper).
    #[inline(always)]
    fn reader_take_now(&self, previous: Option<AlignedBuffer>) -> AlignedBuffer {
        // Observe current generation; we will commit it after removing the middle buffer
        let generation = self
            .shared_state
            .as_ref()
            .generation
            .load(Ordering::Acquire);

        // Get current indices
        let reader_idx = self.reader_idx.as_ref().load(Ordering::Relaxed);
        let middle_idx = self
            .shared_state
            .as_ref()
            .middle_idx
            .load(Ordering::Relaxed);

        // Return previous buffer (if any) into the reader slot; it must be empty
        if let Some(prev) = previous {
            let (prev_ptr, _) = prev.into_raw();
            let old = self.buffers[reader_idx].swap(prev_ptr, Ordering::Release);
            debug_assert!(old.is_null(), "reader slot not empty");
            self.lens[reader_idx].store(0, Ordering::Relaxed);
        }

        // Rotate indices: reader takes middle; middle becomes old reader slot
        self.reader_idx
            .as_ref()
            .store(middle_idx, Ordering::Relaxed);
        self.shared_state
            .as_ref()
            .middle_idx
            .store(reader_idx, Ordering::Relaxed);

        // Take buffer from what was middle (now reader's)
        let ptr = self.buffers[middle_idx].swap(ptr::null_mut(), Ordering::Acquire);
        if unlikely(ptr.is_null()) {
            panic!("Invariant violated: middle buffer pointer was null");
        }
        let published_len = self.lens[middle_idx].load(Ordering::Relaxed);

        let buf = unsafe { AlignedBuffer::from_raw_with_len(ptr, published_len) };

        // Commit: mark this generation as read only after we've removed the buffer from the middle
        self.shared_state
            .as_ref()
            .last_read_gen
            .store(generation, Ordering::Release);

        // Notify writer that middle is now free
        self.wake_writer();

        buf
    }

    // ------------- Synchronous helpers -------------

    #[inline(always)]
    fn stats(&self) -> BufferStats {
        BufferStats {
            writer_idx: self.writer_idx.as_ref().load(Ordering::Relaxed),
            reader_idx: self.reader_idx.as_ref().load(Ordering::Relaxed),
            middle_idx: self
                .shared_state
                .as_ref()
                .middle_idx
                .load(Ordering::Relaxed),
            generation: self
                .shared_state
                .as_ref()
                .generation
                .load(Ordering::Relaxed),
        }
    }
}

impl Drop for TripleBuffer {
    fn drop(&mut self) {
        unsafe {
            // Free any remaining buffers in slots
            for i in 0..3 {
                let ptr = self.buffers[i].load(Ordering::Relaxed);
                if !ptr.is_null() {
                    // Reconstruct and drop; safe because these were produced by AlignedBuffer::into_raw
                    let _ = AlignedBuffer::from_raw_with_len(ptr, 0);
                }
            }
        }
    }
}

// ================== Public SPSC handles ==================

/// Writer handle for the SPSC triple buffer.
///
/// - Non-cloneable. Send, but not Sync (move across threads, but don't share).
pub struct TripleBufWriter {
    inner: Arc<TripleBuffer>,
    _nosync: PhantomData<Cell<()>>,
}

/// Reader handle for the SPSC triple buffer.
///
/// - Non-cloneable. Send, but not Sync (move across threads, but don't share).
pub struct TripleBufReader {
    inner: Arc<TripleBuffer>,
    _nosync: PhantomData<Cell<()>>,
}

/// Construct a new SPSC triple buffer, returning the writer handle,
/// reader handle, and the initial writer-owned buffer.
pub fn triple_buffer() -> (TripleBufWriter, TripleBufReader, AlignedBuffer) {
    let (tb, wbuf) = TripleBuffer::new();
    let inner = Arc::new(tb);
    let writer = TripleBufWriter {
        inner: inner.clone(),
        _nosync: PhantomData,
    };
    let reader = TripleBufReader {
        inner,
        _nosync: PhantomData,
    };
    (writer, reader, wbuf)
}

impl TripleBufWriter {
    /// Publish `buf` when the reader has consumed the previous data.
    /// Never drops/overwrites unread data.
    ///
    /// Requires `&mut self` so only one publish future can exist at a time per writer.
    pub fn publish<'a>(&'a mut self, buf: AlignedBuffer) -> WriterPublish<'a> {
        WriterPublish {
            tb: &self.inner,
            buf: Some(buf),
        }
    }

    /// Snapshot statistics (debugging).
    pub fn stats(&self) -> BufferStats {
        self.inner.stats()
    }
}

impl TripleBufReader {
    /// Yield the next available buffer. If `previous` is Some, it is returned to the pool.
    ///
    /// Requires `&mut self` so only one read future can exist at a time per reader.
    pub fn next<'a>(&'a mut self, previous: Option<AlignedBuffer>) -> ReaderNext<'a> {
        ReaderNext {
            tb: &self.inner,
            prev: previous,
        }
    }

    /// Snapshot statistics (debugging).
    pub fn stats(&self) -> BufferStats {
        self.inner.stats()
    }
}

// Make handles Send but not Sync.
unsafe impl Send for TripleBufWriter {}
unsafe impl Send for TripleBufReader {}

// ================== Futures (public types) ==================

/// Future returned by `TripleBufWriter::publish`
pub struct WriterPublish<'a> {
    tb: &'a TripleBuffer,
    buf: Option<AlignedBuffer>,
}

impl<'a> Future for WriterPublish<'a> {
    type Output = AlignedBuffer;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Fast path: can publish now?
        if likely(this.tb.middle_free()) {
            let buf = this.buf.take().expect("polled after completion");
            let next = this.tb.writer_publish_now(buf);
            return Poll::Ready(next);
        }

        // Register waker, then re-check to avoid missed wake
        this.tb.register_writer_waker(cx.waker());

        if this.tb.middle_free() {
            let buf = this.buf.take().expect("polled after completion");
            let next = this.tb.writer_publish_now(buf);
            Poll::Ready(next)
        } else {
            Poll::Pending
        }
    }
}

/// Future returned by `TripleBufReader::next`
pub struct ReaderNext<'a> {
    tb: &'a TripleBuffer,
    prev: Option<AlignedBuffer>,
}

impl<'a> Future for ReaderNext<'a> {
    type Output = AlignedBuffer;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Fast path: unread data available?
        if likely(this.tb.has_unread()) {
            let buf = this.tb.reader_take_now(this.prev.take());
            return Poll::Ready(buf);
        }

        // Register waker, then re-check to avoid missed wake
        this.tb.register_reader_waker(cx.waker());

        if this.tb.has_unread() {
            let buf = this.tb.reader_take_now(this.prev.take());
            Poll::Ready(buf)
        } else {
            Poll::Pending
        }
    }
}

/// Statistics about the triple buffer state (for debugging/visibility).
#[derive(Debug, Clone, Copy)]
pub struct BufferStats {
    pub writer_idx: usize,
    pub reader_idx: usize,
    pub middle_idx: usize,
    pub generation: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering as AO};
    use std::task::{Context, Waker};
    use std::thread;
    use std::time::Duration;

    // ----------------------------
    // Test utilities
    // ----------------------------

    // A waker that unparks the current thread and flips a flag on wake.
    fn thread_unpark_waker() -> (Waker, Arc<std::sync::atomic::AtomicBool>) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering as AO};
        use std::task::{RawWaker, RawWakerVTable, Waker};

        unsafe fn clone(data: *const ()) -> RawWaker {
            let arc = unsafe { Arc::<AtomicBool>::from_raw(data as *const AtomicBool) };
            let cloned = arc.clone();
            let _ = Arc::into_raw(arc); // restore original ownership
            RawWaker::new(Arc::into_raw(cloned) as *const (), &VTABLE)
        }

        unsafe fn wake(data: *const ()) {
            let arc = unsafe { Arc::<AtomicBool>::from_raw(data as *const AtomicBool) };
            arc.store(true, AO::SeqCst);
            std::thread::current().unpark();
            // arc drops here
        }

        unsafe fn wake_by_ref(data: *const ()) {
            let arc = unsafe { &*(data as *const AtomicBool) };
            arc.store(true, AO::SeqCst);
            std::thread::current().unpark();
        }

        unsafe fn drop(data: *const ()) {
            let _ = unsafe { Arc::<AtomicBool>::from_raw(data as *const AtomicBool) };
        }

        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

        let flag = Arc::new(AtomicBool::new(false));
        let raw = RawWaker::new(Arc::into_raw(flag.clone()) as *const (), &VTABLE);
        let waker = unsafe { Waker::from_raw(raw) };
        (waker, flag)
    }

    // Simple blocking poll that parks the thread until the future is ready.
    fn block_on<F: std::future::Future>(mut fut: F) -> F::Output {
        let (waker, flag) = thread_unpark_waker();
        let mut cx = Context::from_waker(&waker);
        // Safety: We never move `fut` after pinning it.
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };

        loop {
            match fut.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(val) => return val,
                std::task::Poll::Pending => {
                    // Clear the flag and park with a timeout to avoid deadlocks on missed wakes.
                    flag.store(false, AO::SeqCst);
                    thread::park_timeout(Duration::from_millis(10));
                }
            }
        }
    }

    // Poll once and expect Ready, returning the value.
    fn poll_ready_once<F: std::future::Future>(mut fut: F) -> F::Output {
        let (waker, _) = thread_unpark_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        match fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(val) => val,
            std::task::Poll::Pending => panic!("Future unexpectedly Pending"),
        }
    }

    // Poll once and expect Pending. Returns the wake flag so the caller can observe wakeups.
    fn poll_pending_once<F: std::future::Future>(mut fut: Pin<&mut F>) -> Arc<AtomicBool> {
        let (waker, flag) = thread_unpark_waker();
        let mut cx = Context::from_waker(&waker);
        match fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(_) => panic!("Future unexpectedly Ready"),
            std::task::Poll::Pending => {}
        }
        flag
    }

    // Read/write a u64 at the start of the buffer for sequencing tests.
    // Note: we write/read via raw pointers (ignoring `len`) to keep tests simple.
    fn write_seq(buf: &mut AlignedBuffer, v: u64) {
        let bytes = v.to_le_bytes();
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.as_mut_ptr(), 8);
        }
    }
    fn read_seq(buf: &mut AlignedBuffer) -> u64 {
        let mut bytes = [0u8; 8];
        unsafe {
            std::ptr::copy_nonoverlapping(buf.as_mut_ptr() as *const u8, bytes.as_mut_ptr(), 8);
        }
        u64::from_le_bytes(bytes)
    }

    // ----------------------------
    // Tests
    // ----------------------------

    #[test]
    fn aligned_buffer_alignment_and_len() {
        let mut b = AlignedBuffer::new();
        assert_eq!(b.capacity(), BUFFER_SIZE);
        assert_eq!(b.len(), 0);

        let ptr = b.as_mut_ptr() as usize;
        assert_eq!(
            ptr % BUFFER_ALIGN,
            0,
            "buffer not aligned to {}",
            BUFFER_ALIGN
        );

        // Basic read/write sanity using raw pointer (ignore logical len).
        unsafe {
            let s = std::slice::from_raw_parts_mut(b.as_mut_ptr(), b.capacity());
            s[0] = 0xAA;
            s[BUFFER_SIZE - 1] = 0xBB;
            assert_eq!(s[0], 0xAA);
            assert_eq!(s[BUFFER_SIZE - 1], 0xBB);
        }
    }

    #[test]
    fn triple_buffer_initial_state_and_alignment() {
        let (writer, _reader, mut writer_buf) = triple_buffer();

        // Writer got a buffer
        assert_eq!(writer_buf.capacity(), BUFFER_SIZE);
        assert_eq!(
            (writer_buf.as_mut_ptr() as usize) % BUFFER_ALIGN,
            0,
            "writer buffer not aligned"
        );

        // Stats sanity
        let st = writer.stats();
        assert_eq!(st.writer_idx, 0);
        assert_eq!(st.reader_idx, 1);
        assert_eq!(st.middle_idx, 2);
        assert_eq!(st.generation, 0);
    }

    #[test]
    fn async_publish_and_read_basic() {
        let (mut writer, mut reader, mut wbuf) = triple_buffer();

        // Fill and publish; should be Ready immediately because middle is initially free.
        write_seq(&mut wbuf, 7);
        let next_buf = poll_ready_once(writer.publish(wbuf));

        // Reader should get it immediately.
        let mut rbuf = poll_ready_once(reader.next(None));
        assert_eq!(read_seq(&mut rbuf), 7);

        // Publish again and read again with returning previous buffer
        let mut wbuf2 = next_buf;
        write_seq(&mut wbuf2, 9);
        let next2 = poll_ready_once(writer.publish(wbuf2));

        let mut rbuf2 = poll_ready_once(reader.next(Some(rbuf)));
        assert_eq!(read_seq(&mut rbuf2), 9);

        assert_eq!(next2.capacity(), BUFFER_SIZE);
    }

    #[test]
    fn backpressure_pending_and_wakeup() {
        let (mut writer, mut reader, mut wbuf) = triple_buffer();

        // First publish: immediate
        write_seq(&mut wbuf, 1);
        let mut next = poll_ready_once(writer.publish(wbuf));

        // Second publish should pend until we read.
        write_seq(&mut next, 2);
        let mut publish_fut = writer.publish(next);
        let mut publish_fut = unsafe { Pin::new_unchecked(&mut publish_fut) };

        // First poll should pend; capture the wake flag.
        let writer_wake_flag = poll_pending_once(publish_fut.as_mut());

        // Now perform the read; this should wake the writer.
        let _rbuf = poll_ready_once(reader.next(None));

        // Wait a bit for wake propagation.
        for _ in 0..1000 {
            if writer_wake_flag.load(AO::SeqCst) {
                break;
            }
            std::hint::spin_loop();
        }
        assert!(
            writer_wake_flag.load(AO::SeqCst),
            "writer should have been woken after reader consumed"
        );

        // Poll again using a fresh context; should be Ready now.
        let (waker, _) = thread_unpark_waker();
        let mut cx = Context::from_waker(&waker);
        let buf_back = match publish_fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(b) => b,
            std::task::Poll::Pending => panic!("publish should be ready after wake"),
        };

        assert_eq!(buf_back.capacity(), BUFFER_SIZE);
    }

    #[test]
    fn reader_pending_then_wakeup() {
        // Keep the writer-owned buffer so we can publish later.
        let (mut writer, mut reader, mut wbuf) = triple_buffer();

        // Start reader before anything is published: should pend.
        let mut read_fut = reader.next(None);
        let mut read_fut = unsafe { Pin::new_unchecked(&mut read_fut) };
        let reader_wake_flag = poll_pending_once(read_fut.as_mut());

        // Now publish using the initial writer buffer we got from `triple_buffer_spsc()`.
        write_seq(&mut wbuf, 123);
        let _writer_next_buf = poll_ready_once(writer.publish(wbuf));

        // Wait for wake.
        for _ in 0..1000 {
            if reader_wake_flag.load(AO::SeqCst) {
                break;
            }
            std::hint::spin_loop();
        }
        assert!(
            reader_wake_flag.load(AO::SeqCst),
            "reader should have been woken after publish"
        );

        // Complete the read.
        let (waker, _) = thread_unpark_waker();
        let mut cx = Context::from_waker(&waker);
        let mut rbuf = match read_fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(b) => b,
            std::task::Poll::Pending => panic!("read should be ready after publish"),
        };
        assert_eq!(read_seq(&mut rbuf), 123);
    }

    #[test]
    fn single_thread_event_loop_stress() {
        let (mut writer, mut reader, mut wbuf) = triple_buffer();
        let mut prev_read: Option<AlignedBuffer> = None;

        const N: u64 = 10_000;

        for i in 0..N {
            write_seq(&mut wbuf, i);
            wbuf = block_on(writer.publish(wbuf));

            let mut rbuf = block_on(reader.next(prev_read.take()));
            let v = read_seq(&mut rbuf);
            assert_eq!(v, i, "monotonic sequence mismatch at {}", i);

            prev_read = Some(rbuf);
        }
    }

    #[test]
    fn spsc_concurrent_stress() {
        let (mut writer, mut reader, mut wbuf) = triple_buffer();

        const N: u64 = 5000;

        // Reader thread
        let reader_thread = thread::spawn(move || {
            let mut prev: Option<AlignedBuffer> = None;
            let mut expected = 0u64;

            for _ in 0..N {
                let mut buf = block_on(reader.next(prev.take()));
                let v = read_seq(&mut buf);
                assert_eq!(v, expected, "reader observed out-of-order value");
                expected += 1;
                prev = Some(buf);
            }
        });

        // Writer thread
        let writer_thread = thread::spawn(move || {
            for i in 0..N {
                write_seq(&mut wbuf, i);
                wbuf = block_on(writer.publish(wbuf));
            }
        });

        writer_thread.join().unwrap();
        reader_thread.join().unwrap();
    }

    #[test]
    fn drop_with_in_flight_buffers_no_panic() {
        // Publish one buffer; leave it unread; drop the handles (and internal buffer).
        let mut buf_back = {
            let (mut writer, _reader, mut wbuf) = triple_buffer();
            write_seq(&mut wbuf, 42);
            let buf_back = poll_ready_once(writer.publish(wbuf));
            // writer and reader dropped here; middle still holds unread data; drop must not panic.
            buf_back
        };

        // The writer-owned buffer (buf_back) should be valid and aligned, and dropping it must be safe.
        assert_eq!(buf_back.capacity(), BUFFER_SIZE);
        assert_eq!(
            (buf_back.as_mut_ptr() as usize) % BUFFER_ALIGN,
            0,
            "buffer not aligned"
        );
        drop(buf_back);
    }

    #[test]
    fn stats_progress() {
        let (mut writer, mut reader, mut wbuf) = triple_buffer();

        let s0 = writer.stats();
        assert_eq!(s0.generation, 0);

        // Publish
        write_seq(&mut wbuf, 1);
        let _next = poll_ready_once(writer.publish(wbuf));
        let s1 = writer.stats();
        assert_eq!(s1.generation, 1);

        // Read
        let _r = poll_ready_once(reader.next(None));
        let s2 = writer.stats();
        // generation stays at 1; last_read_gen is internal but middle is free now.
        assert_eq!(s2.generation, 1);
    }
}
