//! A lock-free single-producer single-consumer **triple buffer** with async
//! backpressure: a zero-copy, zero-allocation handoff of whole reusable buffers
//! between one writer and one reader.
//!
//! # When to use this vs a ring buffer
//!
//! A [ring buffer](super::ringbuf) moves a *stream of many small elements*,
//! copying each one into and out of shared slots (FIFO, lossless). A triple
//! buffer moves *one whole buffer at a time by handing off ownership* — nothing
//! is copied element-by-element; the producer fills a private buffer and a
//! single pointer swap makes that buffer the consumer's. Reach for the triple
//! buffer when:
//!
//! - the payload is large enough that copying it would dominate (I/O buffers,
//!   frames, snapshots),
//! - you want a fixed pool of buffers recycled with **no per-message
//!   allocation**, and
//! - the producer and consumer should never block each other.
//!
//! Three buffers are the minimum that decouples them: at all times one is being
//! written, one is being read, and one sits "in the middle" published-and-ready,
//! so neither side ever waits on the other for a buffer.
//!
//! This implementation is the **lossless, back-pressured** flavor: the writer's
//! [`publish`](TripleBufWriter::publish) waits until the reader has taken the
//! previous buffer, so published data is never overwritten unread.
//!
//! # The ownership contract (read this)
//!
//! There are **exactly three buffers**, and ownership circulates between the
//! structure, the writer, and the reader. Nothing is allocated after
//! construction. You **must give buffers back** to keep the pool balanced:
//!
//! - The **writer** hands its filled buffer into [`publish`] and receives a
//!   fresh (`len == 0`) buffer to write next. Publishing always costs you one
//!   buffer and returns one.
//! - The **reader** passes `None` to the *first* [`next`] call (it holds no
//!   previous buffer yet) and `Some(prev)` to *every* call afterwards. **If you
//!   keep buffers without returning them, the pool drains and a later operation
//!   panics** with an empty-middle invariant violation.
//! - Only return a buffer to the *same* triple buffer it came from — geometry
//!   (capacity/alignment) is checked on the way in.
//!
//! [`publish`]: TripleBufWriter::publish
//! [`next`]: TripleBufReader::next
//!
//! # Element types
//!
//! Buffers are generic over `T: Zeroable + Copy`. [`Zeroable`] asserts that an
//! all-zero bit pattern is a valid value, which lets construction use the OS's
//! lazy zero pages (`alloc_zeroed`) — even a multi-MiB buffer costs ~nothing to
//! initialize and recycled buffers are never re-zeroed. `Copy` means there are
//! no destructors to run during the recycling.
//!
//! # Example
//!
//! ```no_run
//! use cynosure::site_d::triplebuffer::triple_buffer;
//!
//! # async fn ex() {
//! // A triple buffer of 1024 `f32`s.
//! let (mut writer, mut reader, mut wbuf) = triple_buffer::<f32>(1024);
//!
//! // Writer fills its buffer and publishes it, getting a fresh one back.
//! wbuf.capacity_mut()[0] = 1.0;
//! wbuf.set_len(1);
//! let mut next_wbuf = writer.publish(wbuf).await;
//!
//! // Reader takes the latest; passes `None` the first time, `Some(prev)` after.
//! let rbuf = reader.next(None).await;
//! assert_eq!(&rbuf[..], &[1.0]);
//! let _next = reader.next(Some(rbuf)).await; // return the buffer to the pool
//! # let _ = (&mut next_wbuf,);
//! # }
//! ```

use std::{
    alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error},
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

use futures::task::AtomicWaker;
#[cfg(feature = "monoio-0_2")]
use monoio::buf::{IoBuf, IoBufMut};

use crate::{
    hints::{likely, unlikely},
    site_d::padding::CachePadded,
};

// ================== Zeroable ==================

/// Marker for types whose all-zero bit pattern is a valid, safe value.
///
/// # Safety
///
/// Implementors guarantee that a region of all-zero bytes is a valid and safe
/// value of `Self`. This holds for integers, floats, `bool`, `char`, and
/// fixed-size arrays of such types, but **not** for references, `NonNull`,
/// `NonZero*`, or enums without a zero-valued variant.
pub unsafe trait Zeroable {}

macro_rules! impl_zeroable {
    ($($t:ty),* $(,)?) => {
        $(
            // SAFETY: all-zero bytes are a valid value of each of these types
            // (`0` for integers/floats, `false` for `bool`, `'\0'` for `char`).
            unsafe impl Zeroable for $t {}
        )*
    };
}

impl_zeroable!(
    u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64, bool, char
);

// SAFETY: an array is all-zero iff every element is all-zero, which is valid
// when `T: Zeroable`.
unsafe impl<T: Zeroable, const N: usize> Zeroable for [T; N] {}

// ================== AlignedBuffer ==================

/// An owning, heap-allocated, custom-aligned buffer of `T` used as the unit of
/// transfer in a [`triple_buffer`].
///
/// The whole capacity is zero-initialized at construction (lazily, via the OS
/// zero pages), so every element is always a valid `T` and the buffer derefs to
/// a safe `&[T]` / `&mut [T]` of its logical [`len`](Self::len). `len` is purely
/// a "how many elements are meaningful this round" cursor — separate from the
/// always-valid physical capacity.
pub struct AlignedBuffer<T: Zeroable + Copy> {
    // `NonNull<T>` already makes this covariant in `T` and `!Send`/`!Sync`
    // (re-granted under bounds by the manual impls below); since `Drop` never
    // touches a `T` (`Copy` ⇒ no destructors), no `PhantomData<T>` is needed.
    ptr: NonNull<T>,
    /// Logical, meaningful length in elements (`<= cap`).
    len: usize,
    /// Capacity in elements.
    cap: usize,
    /// Alignment the allocation was made with (needed to reconstruct the
    /// `Layout` for deallocation).
    align: usize,
}

impl<T: Zeroable + Copy> AlignedBuffer<T> {
    #[inline]
    fn layout_for(cap: usize, align: usize) -> Layout {
        Layout::array::<T>(cap)
            .and_then(|l| l.align_to(align))
            .expect("invalid buffer layout (capacity overflow)")
    }

    /// Allocate a new zeroed buffer with capacity `capacity` elements, aligned
    /// to `align_of::<T>()`.
    ///
    /// # Panics
    /// Panics if `capacity == 0`.
    pub fn new(capacity: usize) -> Self {
        Self::with_alignment(capacity, std::mem::align_of::<T>())
    }

    /// Allocate a new zeroed buffer with capacity `capacity` elements, aligned
    /// to `align` bytes (e.g. `4096` for O_DIRECT).
    ///
    /// # Panics
    /// Panics if `capacity == 0`, if `align` is not a power of two, or if
    /// `align < align_of::<T>()`.
    pub fn with_alignment(capacity: usize, align: usize) -> Self {
        assert!(capacity > 0, "capacity must be greater than 0");
        // A ZST would make `layout` zero-sized, and `alloc_zeroed` with a
        // zero-size layout is undefined behavior. `[u8; 0]` is `Zeroable + Copy`,
        // so this is reachable through entirely safe code without the guard.
        assert!(
            std::mem::size_of::<T>() != 0,
            "zero-sized element types are not supported"
        );
        assert!(align.is_power_of_two(), "alignment must be a power of two");
        assert!(
            align >= std::mem::align_of::<T>(),
            "alignment must be >= align_of::<T>()"
        );

        let layout = Self::layout_for(capacity, align);
        // SAFETY: `capacity > 0` and `T` is not a ZST (both asserted above), so
        // the layout has non-zero size.
        let raw = unsafe { alloc_zeroed(layout) } as *mut T;
        let ptr = NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout));
        debug_assert_eq!((ptr.as_ptr() as usize) % align, 0);

        Self {
            ptr,
            len: 0,
            cap: capacity,
            align,
        }
    }

    /// Capacity of the buffer in elements.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Alignment the buffer was allocated with.
    #[inline]
    pub fn alignment(&self) -> usize {
        self.align
    }

    /// Logical (meaningful) length in elements.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the logical length is 0.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Set the logical length.
    ///
    /// This is safe (unlike `Vec::set_len`): the entire capacity is always a
    /// valid initialized region of `T`, so any `new_len <= capacity` exposes
    /// only valid elements. Elements beyond what you actually wrote read back as
    /// their zero value.
    ///
    /// # Panics
    /// Panics if `new_len > capacity`.
    #[inline]
    pub fn set_len(&mut self, new_len: usize) {
        assert!(
            new_len <= self.cap,
            "len {} exceeds capacity {}",
            new_len,
            self.cap
        );
        self.len = new_len;
    }

    /// The full capacity as a mutable slice, for filling the buffer before
    /// setting its logical [`len`](Self::set_len).
    #[inline]
    pub fn capacity_mut(&mut self) -> &mut [T] {
        // SAFETY: the whole `cap` region is zero-initialized and `T: Zeroable`,
        // so every element is a valid `T`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.cap) }
    }

    /// Raw const pointer to the buffer.
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        self.ptr.as_ptr()
    }

    /// Raw mutable pointer to the buffer.
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.ptr.as_ptr()
    }

    /// Convert into a raw data pointer and the logical length, forgetting
    /// `self`. Reconstruct with [`from_raw_with_len`](Self::from_raw_with_len)
    /// using the same capacity and alignment.
    pub fn into_raw(self) -> (*mut T, usize) {
        let p = self.ptr.as_ptr();
        let len = self.len;
        std::mem::forget(self);
        (p, len)
    }

    /// Reconstruct a buffer from a raw pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must have been produced by [`into_raw`](Self::into_raw) on an
    /// `AlignedBuffer<T>` allocated with capacity `cap` and alignment `align`,
    /// and not already reconstructed. `len` must be `<= cap`.
    pub unsafe fn from_raw_with_len(ptr: *mut T, len: usize, cap: usize, align: usize) -> Self {
        debug_assert!(!ptr.is_null());
        debug_assert!(len <= cap);
        debug_assert_eq!((ptr as usize) % align, 0);
        Self {
            ptr: unsafe { NonNull::new_unchecked(ptr) },
            len,
            cap,
            align,
        }
    }
}

impl<T: Zeroable + Copy> Deref for AlignedBuffer<T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        // SAFETY: `len <= cap`, and the whole capacity is valid initialized `T`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl<T: Zeroable + Copy> DerefMut for AlignedBuffer<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: see `deref`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl<T: Zeroable + Copy> Drop for AlignedBuffer<T> {
    fn drop(&mut self) {
        // `T: Copy` => no element destructors to run; just free the allocation.
        unsafe { dealloc(self.ptr.as_ptr() as *mut u8, Self::layout_for(self.cap, self.align)) }
    }
}

impl<T: Zeroable + Copy + std::fmt::Debug> std::fmt::Debug for AlignedBuffer<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlignedBuffer")
            .field("len", &self.len)
            .field("capacity", &self.cap)
            .field("alignment", &self.align)
            .finish()
    }
}

// SAFETY: `AlignedBuffer<T>` owns a heap region of `T`, like `Box<[T]>`; it is
// `Send`/`Sync` exactly when `T` is.
unsafe impl<T: Zeroable + Copy + Send> Send for AlignedBuffer<T> {}
unsafe impl<T: Zeroable + Copy + Sync> Sync for AlignedBuffer<T> {}

#[cfg(feature = "monoio-0_2")]
// SAFETY: `read_ptr` points to `bytes_init` valid, initialized bytes for the
// lifetime of the buffer.
unsafe impl IoBuf for AlignedBuffer<u8> {
    fn read_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.len
    }
}

#[cfg(feature = "monoio-0_2")]
// SAFETY: `write_ptr` points to `bytes_total` writable bytes; `set_init` only
// records how many were initialized.
unsafe impl IoBufMut for AlignedBuffer<u8> {
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
struct TripleBuffer<T: Zeroable + Copy> {
    // Three buffer data pointers; at most one slot can be null (held out-of-structure by writer or
    // reader).
    buffers: [AtomicPtr<T>; 3],
    // Lengths of the buffers in each slot.
    lens: [AtomicUsize; 3],

    // Shared geometry for every buffer in this triple buffer (used to
    // reconstruct `AlignedBuffer`s from the circulating raw pointers).
    cap: usize,
    align: usize,

    // === Writer's cache line ===
    writer_idx: CachePadded<AtomicUsize>,

    // === Reader's cache line ===
    reader_idx: CachePadded<AtomicUsize>,

    // === Shared cache line ===
    shared_state: CachePadded<SharedState>,

    // === Wakers (not on hot path, but kept padded anyway) ===
    waiters: CachePadded<Waiters>,

    // Tie `Send`/`Sync` of the structure to `T` (the `AtomicPtr<T>`s would
    // otherwise make it unconditionally `Send`/`Sync`, which is unsound for
    // `T: !Send`).
    _marker: PhantomData<T>,
}

impl<T: Zeroable + Copy> TripleBuffer<T> {
    /// Create a new triple buffer and return it along with the initial
    /// writer-owned buffer.
    fn new(capacity: usize, align: usize) -> (Self, AlignedBuffer<T>) {
        let b0 = AlignedBuffer::<T>::with_alignment(capacity, align);
        let b1 = AlignedBuffer::<T>::with_alignment(capacity, align);
        let b2 = AlignedBuffer::<T>::with_alignment(capacity, align);

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
            cap: capacity,
            align,
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
            _marker: PhantomData,
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
    fn writer_publish_now(&self, completed: AlignedBuffer<T>) -> AlignedBuffer<T> {
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

        let next = unsafe { AlignedBuffer::from_raw_with_len(ptr, 0, self.cap, self.align) };

        // Notify reader that new data is ready
        self.wake_reader();

        next
    }

    /// Reader consumes latest buffer if present, returning it.
    /// Precondition: `has_unread()` must be true (checked by async wrapper).
    #[inline(always)]
    fn reader_take_now(&self, previous: Option<AlignedBuffer<T>>) -> AlignedBuffer<T> {
        // Observe current generation; we will commit it after removing the middle
        // buffer
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

        let buf =
            unsafe { AlignedBuffer::from_raw_with_len(ptr, published_len, self.cap, self.align) };

        // Commit: mark this generation as read only after we've removed the buffer from
        // the middle
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

impl<T: Zeroable + Copy> Drop for TripleBuffer<T> {
    fn drop(&mut self) {
        unsafe {
            // Free any remaining buffers in slots
            for i in 0..3 {
                let ptr = self.buffers[i].load(Ordering::Relaxed);
                if !ptr.is_null() {
                    // Reconstruct and drop; safe because these were produced by
                    // AlignedBuffer::into_raw with this geometry.
                    let _ = AlignedBuffer::from_raw_with_len(ptr, 0, self.cap, self.align);
                }
            }
        }
    }
}

// ================== Public SPSC handles ==================

/// Writer handle for the SPSC triple buffer.
///
/// Non-cloneable. `Send` (move across threads) but not `Sync` (don't share).
pub struct TripleBufWriter<T: Zeroable + Copy> {
    inner: Arc<TripleBuffer<T>>,
    _nosync: PhantomData<Cell<()>>,
}

/// Reader handle for the SPSC triple buffer.
///
/// Non-cloneable. `Send` (move across threads) but not `Sync` (don't share).
pub struct TripleBufReader<T: Zeroable + Copy> {
    inner: Arc<TripleBuffer<T>>,
    _nosync: PhantomData<Cell<()>>,
}

/// Construct a new SPSC triple buffer whose buffers hold `capacity` elements of
/// `T`, aligned to `align_of::<T>()`. Returns the writer handle, reader handle,
/// and the initial writer-owned buffer.
pub fn triple_buffer<T: Zeroable + Copy>(
    capacity: usize,
) -> (TripleBufWriter<T>, TripleBufReader<T>, AlignedBuffer<T>) {
    triple_buffer_aligned(capacity, std::mem::align_of::<T>())
}

/// Like [`triple_buffer`], but with a custom buffer alignment (e.g. `4096` for
/// O_DIRECT I/O).
///
/// # Panics
/// Panics if `capacity == 0`, `align` is not a power of two, or
/// `align < align_of::<T>()`.
pub fn triple_buffer_aligned<T: Zeroable + Copy>(
    capacity: usize,
    align: usize,
) -> (TripleBufWriter<T>, TripleBufReader<T>, AlignedBuffer<T>) {
    let (tb, wbuf) = TripleBuffer::<T>::new(capacity, align);
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

impl<T: Zeroable + Copy> TripleBufWriter<T> {
    /// Publish `buf`, waiting until the reader has consumed the previously
    /// published buffer (never overwrites unread data). Resolves to a fresh
    /// (`len == 0`) buffer to write into next.
    ///
    /// `buf` must have come from this triple buffer (same capacity and
    /// alignment).
    ///
    /// Requires `&mut self`, so only one publish future can exist at a time.
    ///
    /// # Panics
    /// Panics if `buf`'s capacity or alignment does not match this triple
    /// buffer.
    pub fn publish(&mut self, buf: AlignedBuffer<T>) -> WriterPublish<'_, T> {
        assert!(
            buf.capacity() == self.inner.cap && buf.alignment() == self.inner.align,
            "buffer geometry does not match this triple buffer"
        );
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

impl<T: Zeroable + Copy> TripleBufReader<T> {
    /// Yield the next published buffer. Pass `None` on the first call and
    /// `Some(previous)` on every call afterwards to return your last buffer to
    /// the pool.
    ///
    /// Failing to return previous buffers drains the fixed pool of three and
    /// will eventually panic.
    ///
    /// Requires `&mut self`, so only one read future can exist at a time.
    ///
    /// # Panics
    /// Panics if `previous`'s capacity or alignment does not match this triple
    /// buffer.
    pub fn next(&mut self, previous: Option<AlignedBuffer<T>>) -> ReaderNext<'_, T> {
        if let Some(ref b) = previous {
            assert!(
                b.capacity() == self.inner.cap && b.alignment() == self.inner.align,
                "buffer geometry does not match this triple buffer"
            );
        }
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

// ================== Futures (public types) ==================

/// Future returned by [`TripleBufWriter::publish`].
pub struct WriterPublish<'a, T: Zeroable + Copy> {
    tb: &'a TripleBuffer<T>,
    buf: Option<AlignedBuffer<T>>,
}

impl<'a, T: Zeroable + Copy> Future for WriterPublish<'a, T> {
    type Output = AlignedBuffer<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: this future holds no address-sensitive state; the only field
        // we move is the owned (movable) `AlignedBuffer` out of the `Option`.
        let this = unsafe { self.get_unchecked_mut() };

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

/// Future returned by [`TripleBufReader::next`].
pub struct ReaderNext<'a, T: Zeroable + Copy> {
    tb: &'a TripleBuffer<T>,
    prev: Option<AlignedBuffer<T>>,
}

impl<'a, T: Zeroable + Copy> Future for ReaderNext<'a, T> {
    type Output = AlignedBuffer<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: see `WriterPublish::poll`.
        let this = unsafe { self.get_unchecked_mut() };

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
    use std::{
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering as AO},
        },
        task::{Context, Waker},
        thread,
        time::Duration,
    };

    use super::*;

    // Default test geometry: a small `u8` buffer, 4096-aligned (so the existing
    // alignment assertions still mean something).
    const CAP: usize = 8192;
    const ALIGN: usize = 4096;

    fn u8_triple() -> (
        TripleBufWriter<u8>,
        TripleBufReader<u8>,
        AlignedBuffer<u8>,
    ) {
        triple_buffer_aligned::<u8>(CAP, ALIGN)
    }

    // ----------------------------
    // Test utilities
    // ----------------------------

    fn thread_unpark_waker() -> (Waker, Arc<AtomicBool>) {
        use std::task::{RawWaker, RawWakerVTable};

        unsafe fn clone(data: *const ()) -> RawWaker {
            let arc = unsafe { Arc::<AtomicBool>::from_raw(data as *const AtomicBool) };
            let cloned = arc.clone();
            let _ = Arc::into_raw(arc);
            RawWaker::new(Arc::into_raw(cloned) as *const (), &VTABLE)
        }
        unsafe fn wake(data: *const ()) {
            let arc = unsafe { Arc::<AtomicBool>::from_raw(data as *const AtomicBool) };
            arc.store(true, AO::SeqCst);
            std::thread::current().unpark();
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

    fn block_on<F: std::future::Future>(mut fut: F) -> F::Output {
        let (waker, flag) = thread_unpark_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(val) => return val,
                Poll::Pending => {
                    flag.store(false, AO::SeqCst);
                    thread::park_timeout(Duration::from_millis(10));
                }
            }
        }
    }

    fn poll_ready_once<F: std::future::Future>(mut fut: F) -> F::Output {
        let (waker, _) = thread_unpark_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(val) => val,
            Poll::Pending => panic!("Future unexpectedly Pending"),
        }
    }

    fn poll_pending_once<F: std::future::Future>(mut fut: Pin<&mut F>) -> Arc<AtomicBool> {
        let (waker, flag) = thread_unpark_waker();
        let mut cx = Context::from_waker(&waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(_) => panic!("Future unexpectedly Ready"),
            Poll::Pending => {}
        }
        flag
    }

    fn write_seq(buf: &mut AlignedBuffer<u8>, v: u64) {
        let bytes = v.to_le_bytes();
        buf.capacity_mut()[..8].copy_from_slice(&bytes);
        buf.set_len(8);
    }
    fn read_seq(buf: &AlignedBuffer<u8>) -> u64 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&buf[..8]);
        u64::from_le_bytes(bytes)
    }

    // ----------------------------
    // Tests
    // ----------------------------

    #[test]
    fn aligned_buffer_alignment_and_len() {
        let mut b = AlignedBuffer::<u8>::with_alignment(CAP, ALIGN);
        assert_eq!(b.capacity(), CAP);
        assert_eq!(b.len(), 0);
        assert_eq!(b.as_mut_ptr() as usize % ALIGN, 0, "buffer not aligned");

        b.capacity_mut()[0] = 0xAA;
        b.capacity_mut()[CAP - 1] = 0xBB;
        assert_eq!(b.capacity_mut()[0], 0xAA);
        assert_eq!(b.capacity_mut()[CAP - 1], 0xBB);
    }

    #[test]
    fn default_alignment_is_align_of_t() {
        let b = AlignedBuffer::<u32>::new(16);
        assert_eq!(b.alignment(), std::mem::align_of::<u32>());
        assert_eq!(b.as_ptr() as usize % std::mem::align_of::<u32>(), 0);
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn rejects_non_pow2_alignment() {
        let _ = AlignedBuffer::<u8>::with_alignment(64, 3);
    }

    #[test]
    #[should_panic(expected = "capacity must be greater than 0")]
    fn rejects_zero_capacity() {
        let _ = AlignedBuffer::<u8>::new(0);
    }

    #[test]
    #[should_panic(expected = "zero-sized element types")]
    fn rejects_zst() {
        // `[u8; 0]` is `Zeroable + Copy`; a zero-size `alloc_zeroed` would be UB.
        let _ = AlignedBuffer::<[u8; 0]>::new(8);
    }

    #[test]
    fn zero_initialized() {
        let b = AlignedBuffer::<u32>::new(32);
        // SAFETY-free: whole capacity is valid, zeroed.
        let mut b = b;
        assert!(b.capacity_mut().iter().all(|&x| x == 0));
    }

    #[test]
    fn triple_buffer_initial_state() {
        let (writer, _reader, mut writer_buf) = u8_triple();
        assert_eq!(writer_buf.capacity(), CAP);
        assert_eq!(writer_buf.as_mut_ptr() as usize % ALIGN, 0);

        let st = writer.stats();
        assert_eq!(st.writer_idx, 0);
        assert_eq!(st.reader_idx, 1);
        assert_eq!(st.middle_idx, 2);
        assert_eq!(st.generation, 0);
    }

    #[test]
    fn async_publish_and_read_basic() {
        let (mut writer, mut reader, mut wbuf) = u8_triple();

        write_seq(&mut wbuf, 7);
        let next_buf = poll_ready_once(writer.publish(wbuf));

        let rbuf = poll_ready_once(reader.next(None));
        assert_eq!(read_seq(&rbuf), 7);

        let mut wbuf2 = next_buf;
        write_seq(&mut wbuf2, 9);
        let next2 = poll_ready_once(writer.publish(wbuf2));

        let rbuf2 = poll_ready_once(reader.next(Some(rbuf)));
        assert_eq!(read_seq(&rbuf2), 9);

        assert_eq!(next2.capacity(), CAP);
    }

    #[test]
    fn backpressure_pending_and_wakeup() {
        let (mut writer, mut reader, mut wbuf) = u8_triple();

        write_seq(&mut wbuf, 1);
        let mut next = poll_ready_once(writer.publish(wbuf));

        write_seq(&mut next, 2);
        let mut publish_fut = writer.publish(next);
        let mut publish_fut = unsafe { Pin::new_unchecked(&mut publish_fut) };

        let writer_wake_flag = poll_pending_once(publish_fut.as_mut());

        let _rbuf = poll_ready_once(reader.next(None));

        for _ in 0..1000 {
            if writer_wake_flag.load(AO::SeqCst) {
                break;
            }
            std::hint::spin_loop();
        }
        assert!(writer_wake_flag.load(AO::SeqCst), "writer should wake");

        let (waker, _) = thread_unpark_waker();
        let mut cx = Context::from_waker(&waker);
        let buf_back = match publish_fut.as_mut().poll(&mut cx) {
            Poll::Ready(b) => b,
            Poll::Pending => panic!("publish should be ready after wake"),
        };
        assert_eq!(buf_back.capacity(), CAP);
    }

    #[test]
    fn reader_pending_then_wakeup() {
        let (mut writer, mut reader, mut wbuf) = u8_triple();

        let mut read_fut = reader.next(None);
        let mut read_fut = unsafe { Pin::new_unchecked(&mut read_fut) };
        let reader_wake_flag = poll_pending_once(read_fut.as_mut());

        write_seq(&mut wbuf, 123);
        let _writer_next_buf = poll_ready_once(writer.publish(wbuf));

        for _ in 0..1000 {
            if reader_wake_flag.load(AO::SeqCst) {
                break;
            }
            std::hint::spin_loop();
        }
        assert!(reader_wake_flag.load(AO::SeqCst), "reader should wake");

        let (waker, _) = thread_unpark_waker();
        let mut cx = Context::from_waker(&waker);
        let rbuf = match read_fut.as_mut().poll(&mut cx) {
            Poll::Ready(b) => b,
            Poll::Pending => panic!("read should be ready after publish"),
        };
        assert_eq!(read_seq(&rbuf), 123);
    }

    #[test]
    fn single_thread_event_loop_stress() {
        let (mut writer, mut reader, mut wbuf) = u8_triple();
        let mut prev_read: Option<AlignedBuffer<u8>> = None;

        const N: u64 = 10_000;
        for i in 0..N {
            write_seq(&mut wbuf, i);
            wbuf = block_on(writer.publish(wbuf));

            let rbuf = block_on(reader.next(prev_read.take()));
            assert_eq!(read_seq(&rbuf), i, "mismatch at {}", i);
            prev_read = Some(rbuf);
        }
    }

    #[test]
    fn spsc_concurrent_stress() {
        let (mut writer, mut reader, mut wbuf) = u8_triple();
        const N: u64 = 5000;

        let reader_thread = thread::spawn(move || {
            let mut prev: Option<AlignedBuffer<u8>> = None;
            for expected in 0..N {
                let buf = block_on(reader.next(prev.take()));
                assert_eq!(read_seq(&buf), expected, "out-of-order");
                prev = Some(buf);
            }
        });

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
        let buf_back = {
            let (mut writer, _reader, mut wbuf) = u8_triple();
            write_seq(&mut wbuf, 42);
            poll_ready_once(writer.publish(wbuf))
            // writer/reader dropped here; middle still holds unread data
        };
        assert_eq!(buf_back.capacity(), CAP);
        assert_eq!(buf_back.as_ptr() as usize % ALIGN, 0);
        drop(buf_back);
    }

    #[test]
    #[should_panic(expected = "geometry does not match")]
    fn rejects_foreign_buffer() {
        let (mut writer, _r, _w) = u8_triple();
        let foreign = AlignedBuffer::<u8>::new(CAP); // different alignment (1)
        drop(writer.publish(foreign)); // panics in `publish` before the future exists
    }

    // ---- generic element types ----

    #[test]
    fn typed_f32_roundtrip() {
        let (mut writer, mut reader, mut wbuf) = triple_buffer::<f32>(1024);
        let data = [1.5f32, 2.5, 3.5, 4.5];
        wbuf.capacity_mut()[..4].copy_from_slice(&data);
        wbuf.set_len(4);
        let _next = poll_ready_once(writer.publish(wbuf));

        let rbuf = poll_ready_once(reader.next(None));
        assert_eq!(&rbuf[..], &data);
    }

    #[test]
    fn typed_pod_struct() {
        #[derive(Clone, Copy, Debug, PartialEq)]
        #[repr(C)]
        struct Pixel {
            r: u8,
            g: u8,
            b: u8,
            a: u8,
        }
        // SAFETY: all-zero is a valid `Pixel`.
        unsafe impl Zeroable for Pixel {}

        let (mut writer, mut reader, mut wbuf) = triple_buffer::<Pixel>(256);
        let px = Pixel {
            r: 1,
            g: 2,
            b: 3,
            a: 4,
        };
        wbuf.capacity_mut()[0] = px;
        wbuf.set_len(1);
        let _next = poll_ready_once(writer.publish(wbuf));

        let rbuf = poll_ready_once(reader.next(None));
        assert_eq!(rbuf[0], px);
    }

    #[test]
    fn typed_custom_alignment() {
        // 64-byte (cache-line) aligned u64 buffer.
        let (mut writer, mut reader, mut wbuf) = triple_buffer_aligned::<u64>(128, 64);
        assert_eq!(wbuf.as_mut_ptr() as usize % 64, 0);
        wbuf.capacity_mut()[0] = 0xDEAD_BEEF;
        wbuf.set_len(1);
        let _next = poll_ready_once(writer.publish(wbuf));
        let rbuf = poll_ready_once(reader.next(None));
        assert_eq!(rbuf[0], 0xDEAD_BEEF);
    }
}
