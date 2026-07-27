//! High-performance concurrency primitives for **thread-per-core** async
//! executors (e.g. [monoio], [glommio]) — though several work anywhere.
//! Zero dependencies by default.
//!
//! The crate is split by where a primitive is allowed to be used, and each
//! primitive sits behind its own feature flag (all enabled by default):
//!
//! * [`site_c`] — **single-threaded**, `!Send` primitives. Because they never
//!   cross a core they use `Cell`/`UnsafeCell` instead of atomics, landing near
//!   `RefCell` speed while remaining usable across `.await` (which a plain
//!   `RefCell` borrow is not). This mirrors the per-shard, lock-free model of
//!   C++'s [Seastar]. Contains [`mutex`](site_c::mutex),
//!   [`rwlock`](site_c::rwlock), [`semaphore`](site_c::semaphore),
//!   [`pool`](site_c::pool) and [`queue`](site_c::queue).
//! * [`site_d`] — **cross-thread** primitives: [`ringbuf`](site_d::ringbuf)
//!   (SPSC ring), [`triplebuffer`](site_d::triplebuffer) (zero-copy
//!   whole-buffer handoff), [`bipbuffer`](site_d::bipbuffer) (always-contiguous
//!   IO byte buffer), [`oneshot`](site_d::oneshot) (single-value reply) and
//!   [`mpsc_light`](site_d::mpsc_light) (control-plane channel).
//!
//! Every `site_d` primitive shares one audited wakeup core (a flag-gated
//! `SeqCst` handshake) that skips the wake entirely when nobody is parked.
//!
//! # Choosing a channel
//!
//! | shape | reach for |
//! |---|---|
//! | fixed 1:1 stream | [`ringbuf`](site_d::ringbuf) |
//! | one reply to one request | [`oneshot`](site_d::oneshot) |
//! | many senders → one consumer | [`mpsc_light`](site_d::mpsc_light) |
//! | whole buffers, zero-copy | [`triplebuffer`](site_d::triplebuffer) |
//! | contiguous bytes for io_uring | [`bipbuffer`](site_d::bipbuffer) |
//!
//! # Example
//!
//! ```
//! # use cynosure::site_d::mpsc_light;
//! # async fn ex() {
//! let (tx, mut rx) = mpsc_light::bounded::<u32>(256);
//! tx.send(1).await.unwrap();
//! assert_eq!(rx.recv().await, Some(1));
//! # }
//! ```
//!
//! Benchmarks, methodology and the honest caveats (including where cynosure
//! *loses* and why) live in `BENCHMARKS.md` in the repository.
//!
//! [monoio]: https://github.com/bytedance/monoio
//! [glommio]: https://github.com/DataDog/glommio
//! [Seastar]: https://seastar.io

mod blocking;
#[cfg(feature = "hints")]
pub mod hints;
pub mod site_c;
pub mod site_d;
