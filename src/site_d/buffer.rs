//! Owning, custom-aligned, heap buffers shared by the `site_d` IO primitives
//! ([`triple_buffer`](super::triplebuffer), [`bipbuffer`](super::bipbuffer), and
//! the buffer [`pool`](crate::site_c::pool)).
//!
//! [`AlignedBuffer`] is the unit of zero-copy IO memory: its whole capacity is
//! zero-initialized once (lazily, via the OS zero pages), so it always derefs to
//! a safe slice and recycled buffers are never re-zeroed. With the `monoio-0_2`
//! feature, `AlignedBuffer<u8>` implements `IoBuf`/`IoBufMut`.

use std::{
    alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error},
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

#[cfg(feature = "monoio-0_2")]
use monoio::buf::{IoBuf, IoBufMut};

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

/// An owning, heap-allocated, custom-aligned buffer of `T`.
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
        unsafe {
            dealloc(
                self.ptr.as_ptr() as *mut u8,
                Self::layout_for(self.cap, self.align),
            )
        }
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
