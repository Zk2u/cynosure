# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.0] - 2026-07-28

### Added — new primitives

- **`site_d::oneshot`** — single-value, single-use cross-core channel (the request/response reply
  path); reuses `WaiterSlot`.
- **`site_c::semaphore::LocalSemaphore`** — single-threaded counting semaphore (concurrency limiting
  / backpressure / pools), FIFO-fair, built on the lock waiter machinery.
- **`site_d::bipbuffer`** — lock-free SPSC bip buffer (bipartite ring): always-contiguous reserve/
  read regions for io_uring/DMA zero-copy; arbitrary capacity via a watermark; `Arc`-backed
  `'static` grants (`IoBuf`/`IoBufMut` under `monoio-0_2`).
- **`site_c::pool::LocalBufferPool<T>`** — per-core recyclable pool of aligned `AlignedBuffer<T>`
  buffers (generic over the element type; `u8` is the IO case, with `PooledBuffer<u8>: IoBuf`),
  composing `LocalSemaphore` + `AlignedBuffer`; `'static` `PooledBuffer` returns itself on drop.
- **`site_d::mpsc_light`** — single-queue control-plane MPSC: cloneable `Send` senders (no executor
  constraint), **global FIFO** across senders, O(cap) memory per channel. A `VecDeque` under a tuned
  TTAS spinlock (jittered backoff), an under-lock sender wait-list (a backpressured sender parks and
  is woken in *one* lock acquisition), and a single-consumer O(1) **buffer-steal** drain. The
  default channel for the many small control channels; use `RingBuf` for a fixed 1:1 stream.
  **`recv()` is polite by default** (parks promptly; a shared reactor's co-located tasks pay nothing — measured 71×
  neighbor-throughput vs an in-`poll` spin); **`recv_hot()`** opts into the pre-park spin
  window (~70–90 ns ping-pong catch) for consumers that own their thread. Adaptive spin
  heuristics were evaluated and rejected: only the caller knows its deployment.
  **Batch API**: `Sender::try_send_many(&mut Vec<T>)` (one lock + one signal for a whole batch)
  and `Receiver::recv_many(&mut Vec<T>, max)` (drain a burst for ~one lock via the buffer-steal).
  Under producer contention this is a 2–3.6× throughput win (N=8 producers, chunks of 64: +262%);
  a wash uncontended.
- **`benches/control_plane.rs`** — real-world control-plane benchmark (paced dispatch, paced
  fan-in, shared-reactor citizenship with co-located tasks) vs kanal/tokio, complementing the
  saturation-style suites.
- New feature flags `oneshot`, `semaphore`, `mpsc_light`, `bipbuffer`, `pool`, `buffer`
  (all pulled in by `site_c`/`site_d`).

### Removed (pre-release)

- **`site_d::mpsc`** (the sharded fan-in channel) and its cloneable `SharedSender` are **not part
  of 0.6**. `SharedSender` routed each thread to a lane via a *process-global* dense thread-id, so
  the id space was shared across unrelated channels: a correctly-sized channel could panic
  (`more concurrent sender threads than lanes`) because another channel's threads consumed the
  space — and `max_lanes` could not be reasoned about locally. The sound fixes (a per-channel id
  map, or returning an error) either reintroduce the map the design existed to avoid or change the
  API contract, so the whole primitive is deferred rather than shipped with a landmine.
  `mpsc_light` covers clonable-sender fan-in; `RingBuf` covers 1:1 streams.

### Fixed — security / soundness (pre-release audit; never shipped)

- **`bipbuffer`**: a committed/released grant cleared its in-progress flag **twice** — once in
  `commit`/`release`, then again in `Drop`. The window between them spans `signal()`, so a waiter
  woken there (or the producer on another thread — grants are `Send`) could take a fresh grant that
  the old grant's drop then silently released, handing out two overlapping `&mut [u8]` over the same
  bytes from safe code. Grants now record that they handed the flag back, so `Drop` is a no-op on
  that path; regression-tested via waker re-entrancy (the test fails without the fix) and
  Miri-verified. Found in the pre-release security review.
- **`bipbuffer`**: `try_reserve(n)` computed `write + n` unchecked; a huge `n` could wrap past the
  capacity checks in release builds and hand out a write grant running off the end of the buffer
  (an out-of-bounds `&mut [u8]` reachable from safe code). Oversized requests are now rejected up
  front and the space arithmetic is overflow-free; regression-tested and Miri-verified.

### Changed — shared wakeup core

- Extracted one audited cross-core wakeup primitive, `site_d::notify::WaiterSlot` (flag-gated
  `SeqCst` handshake), now used by `RingBuf`, the triple buffer, oneshot, `mpsc_light`, and the bip buffer —
  replacing per-type hand-rolled wakeups. `RingBuf`'s refactor onto it is behavior-preserving.
- **Removed the `futures` runtime dependency.** Vendored its `AtomicWaker` (the three-state
  register/wake machine) into `notify`; the library is now genuinely dependency-free (`monoio-0_2`
  remains the only optional dep). The internal `futures` feature is gone.

### Performance

- **triple buffer**: rotation latency ~16 → **~10.6 ns** (`generation` is a single-writer `store`, not
  a `fetch_add` RMW; flag-gated wakeup; waiter slots on separate cache lines; **sync
  `try_publish`/`try_next`** API that skips the future-poll overhead) — ~2× faster than the
  recycling-channel baseline, and 2-thread spin throughput now beats it at every buffer size (it
  *trailed* at 4 KB when the bench drove the async future).
- **mpsc_light**: instruction-level pass (AArch64): the under-lock `len` mirror's `fetch_add` RMW
  replaced by a **transition-only non-empty hint** (stored only on the empty→non-empty push, cleared
  by the steal — plain release stores, and steady-state pushes skip it entirely); explicit `repr(C)`
  + `CachePadded` grouping (lock cluster / consumer waiter slot / lifecycle words on separate
  lines). Measured (interleaved A/B): **+36% at 1 producer, +25–29% at 4–16, wakeup latency
  131 → ~70 ns** (the consumer's pre-park spin is pure `spin_loop` — no `yield_now`, which
  stalls co-located tasks on shared executors; removing it was free on dedicated threads and
  recovered up to +40% on a tokio worker pool); a deliberate big-buffer trade at 16 producers/cap 1024 (~85 → ~50, still ~1.8× kanal).
- **oneshot**: removed both hot-path RMWs — `send`'s `fetch_or(VALUE_SET)` is a SeqCst *store* (the
  only losable concurrent bit re-creates the documented deliver-then-drop outcome), and
  `take_value`'s `fetch_or(TAKEN)` is a plain store (after `VALUE_SET` is observed there are no
  concurrent state writers — single receiver thread, sender's post-send drop takes the no-write
  branch). Flips the create+send+recv cycle from ~1.5% behind tokio to at-or-ahead (~11.9 vs
  ~12.0 ns); race-stress-tested and Miri-verified.
- **branch hints**: `mpsc_light`'s hot branches gained `likely`/`unlikely` (full queue, the
  empty→non-empty transition, has-room, batch-accepted, empty-steal) — won 12/12 interleaved A/B
  comparisons, **+6–9% at one producer** and ~+1% under contention, where the lock dominates.
- **semaphore**: ~3× faster uncontended `try_acquire`/release (2.4 ns) — the wake is cold-outlined so
  the common no-waiter release inlines to `permits += n` + a branch.
- **pool**: free-list switched from `RefCell` to `UnsafeCell` (single-core; no runtime borrow check),
  ~15% faster acquire/return.

### Changed — internal

- `AlignedBuffer`/`Zeroable` moved from `triplebuffer` to a shared `site_d::buffer` module (used by
  the triple buffer, bip buffer, and pool). Re-exported from `triplebuffer` — **not breaking**.

### Documentation

- **Crate-level documentation** — the crate root previously had none, so docs.rs showed an empty
  landing page. Adds an overview of the `site_c`/`site_d` split, the shared wakeup core, a
  "choosing a channel" table, and a runnable example. Module docs added for `mutex`, `queue`,
  `rwlock` and `padding`; field/variant docs for `Iter`/`IterMut`, `CachePadded` and `BufferStats`.
  `-W missing_docs` is now clean.
- **Corrected the `Queue` claim.** It was documented as "~2–3× faster than `VecDeque`"; the bench
  behind that starts from a fresh `VecDeque::new()`, so it was timing an *allocation*. The real
  win is allocation avoidance — on a warm, pre-sized queue `VecDeque` is the faster one
  (979 vs 392 Mops/s). BENCHMARKS.md now says so.
- **Added "Where cynosure loses, and why"** to BENCHMARKS.md: the `bip_buffer`, `LocalBufferPool`
  and `Queue` deficits, each traced to the capability that causes it.

### Benchmarks & charts

- New competitor benches: `mpsc_light` (vs kanal), `control_plane` (real-world control-plane load
  vs kanal/tokio), `oneshot` (now also vs `async-oneshot` and `catty`), `triplebuffer` (now also vs
  the `triple_buffer` crate). Bench profile builds with LTO.
- **`latency_dist`** and **`primitives_throughput`** — latency *distributions* and sustained
  throughput for every primitive, measured independently (never one derived from the other) and
  written to `docs/bench-data/*.csv`.
- **`tools/chartgen`** — a dependency-free Rust tool that renders those CSVs to branded SVG + 4×
  PNG charts in `docs/charts/`, embedded in the README. Charts are never hand-drawn: re-run the
  benches and re-run the tool.
- Latency charts for sub-nanosecond operations stop at **p99**: past that the batch means measure OS
  preemption rather than the primitive (~6–7 µs of per-batch excess — one context switch — appearing
  on whichever series got unlucky, reproduced with a no-primitive control). Directly-timed
  cross-thread series keep their full curve.
- Methodology note: benches are measured per-suite on a cool machine; a full back-to-back
  `cargo bench` thermally throttles later suites by ~15%.

### Changed — packaging

- Maintainer email is now `hey@zk2u.com`. `exclude` keeps `tools/`, `docs/` and the local-only
  `attic/`/`rust-channel-benchmarks/` trees out of the published crate; dropped the now-unused
  `async-channel` dev-dependency.

## [0.5.0] - 2026-06-27

### Breaking Changes

- **Generic triple buffer**: the triple buffer is now generic over `T: Zeroable + Copy` with runtime
  capacity and alignment. `triple_buffer()` now takes a capacity; `BUFFER_SIZE`, `BUFFER_ALIGN`,
  `AlignedBuffer::new()` (no-arg) and `AlignedBuffer::default()` are removed. The 4 MiB O_DIRECT
  setup becomes `triple_buffer_aligned::<u8>(4 * 1024 * 1024, 4096)`. `IoBuf`/`IoBufMut` are now
  implemented only for `AlignedBuffer<u8>`.

### Fixed (soundness / liveness)

- **`RingBuf`**: fixed a cross-thread lost-wakeup deadlock — the async register/re-check handshake
  used `Release`/`Acquire`, a store-buffer race that could leave a producer or consumer parked while
  data/space was available (reachable cross-thread, including on x86-TSO). Now uses `SeqCst` on the
  publish store and waker-flag load on both sides (no fence; `stlr`/`ldar` on AArch64). Also fixed a
  data race on the waker cell by switching to `futures::task::AtomicWaker`.
- **`LocalRwLock`**: fixed a writer-cancellation deadlock and genuine write-preferring fairness
  (`read().await` now defers to a queued writer). **`LocalMutex`**: replaced wake-all (thundering
  herd) with FIFO wake-one. Both: lock futures now deregister their waker on drop and pass the turn
  on (waiters tracked by a per-future token), so cancellation can no longer strand a live waiter.
- **`Queue`**: fixed an out-of-bounds panic in `make_contiguous` on a full buffer.
- Zero-sized element types are now rejected by `RingBuf` and the triple buffer (a zero-size
  `alloc_zeroed` is UB).

### Added

- `Zeroable` marker trait (no external dependency) for the triple buffer.
- `Queue::retain`.

### Performance

- `block_on` now caches its thread waker in a thread-local instead of allocating per call
  (~5× sync blocking throughput).
- Branch hints across `RingBuf` slice and IO paths.

## [0.4.0] - 2026-01-08

### Breaking Changes

- **Removed `ScopedCell`**: The `cell` module and `ScopedCell` type have been removed. Use `RefCell` from the standard library or `LocalMutex` for async-compatible interior mutability.
- The `cell` feature flag has been removed from `site_c`.

### Performance Improvements

#### LocalMutex

Fixed a critical performance bug in `LocalMutex::unlock()` where `mem::take` on the waiter queue caused ~150 bytes of memory operations on every unlock, even when no tasks were waiting.

| Implementation | Latency | vs parking_lot |
|----------------|---------|----------------|
| **LocalMutex::try_lock** | **0.59 ns** | **3.3x faster** |
| **LocalMutex::lock (async)** | **0.67 ns** | **2.9x faster** |
| parking_lot::Mutex | 1.92 ns | baseline |
| std::sync::Mutex | 4.27 ns | 0.45x |
| tokio::sync::Mutex | 11.79 ns | 0.16x |

#### LocalRwLock

Applied the same optimization to `LocalRwLock` and changed from read-preferring to **write-preferring** fairness policy, matching the behavior of `parking_lot` and `tokio`.

| Implementation | Read Latency | Write Latency |
|----------------|--------------|---------------|
| **LocalRwLock** | **0.53-0.60 ns** | **1.79-1.80 ns** |
| parking_lot::RwLock | 3.62 ns | 1.92 ns |
| std::sync::RwLock | 4.89 ns | 2.19 ns |
| tokio::sync::RwLock | 11.36 ns | 11.11 ns |

**Read locks are 6-7x faster than parking_lot. Write locks are ~7% faster.**

#### RingBuf SPSC

The single-producer/single-consumer ring buffer continues to outperform both `std::sync::mpsc` and `crossbeam-channel`:

**Latency (single push+pop):**

| Type | RingBuf | std::mpsc | crossbeam | RingBuf speedup |
|------|---------|-----------|-----------|-----------------|
| u8 | 2.2 ns | 7.1 ns | 8.3 ns | **3.2-3.8x** |
| u32 | 2.2 ns | 6.4 ns | 8.6 ns | **2.9-3.9x** |
| u128 | 2.2 ns | 8.4 ns | 8.7 ns | **3.8-4.0x** |
| 64B | 40.4 ns | 50.3 ns | 50.7 ns | **1.25x** |
| 512B | 337 ns | 377 ns | 378 ns | **1.12x** |

**Throughput (100k items, interleaved push/pop):**

| Type | RingBuf | std::mpsc | crossbeam |
|------|---------|-----------|-----------|
| u32 | **421 Melem/s** | 147 Melem/s | 143 Melem/s |
| u128 | **365 Melem/s** | 121 Melem/s | 115 Melem/s |
| 64B | **68 Melem/s** | 48 Melem/s | 48 Melem/s |
| 512B | **13 Melem/s** | 10 Melem/s | 10 Melem/s |

### Bug Fixes

- Fixed inefficient waiter queue draining in `LocalMutex::unlock()` and `LocalRwLock::release_write()` that caused unnecessary memory operations.
- Changed `LocalRwLock` from read-preferring to write-preferring to prevent writer starvation.

### Internal Changes

- Added `unlikely` branch hints to mutex/rwlock unlock fast paths.
- Waiter queues now drain in-place using `pop_front()` instead of `mem::take()`.

## [0.3.0] - Previous Release

Initial public release with `LocalMutex`, `LocalRwLock`, `Queue`, `RingBuf`, and `TripleBuffer`.