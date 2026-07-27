# Cynosure

High-performance, lightweight concurrency primitives and data structures, primarily optimized for
thread-per-core async executors (e.g. [monoio](https://github.com/bytedance/monoio),
[glommio](https://github.com/DataDog/glommio)) — though several work anywhere. Zero dependencies by
default.

The crate is split into `site_c` (single-threaded) and `site_d` (cross-thread). Each primitive is
behind its own feature flag.

## `site_c` — single-threaded async primitives

Non-atomic, `!Send` primitives for single-threaded async executors. Because they never cross a
core, they use `Cell`/`UnsafeCell` instead of atomics — roughly `RefCell` speed, while remaining
usable across `.await` points (which a plain `RefCell` borrow is not). This mirrors the
synchronization model of C++'s [Seastar](https://seastar.io) (per-shard, lock-free, no atomics).

- **`LocalMutex<T>`** — single-threaded async mutex. Hold it across `.await`. `try_lock` is ~0.6 ns
  (near `RefCell`, ~3× faster than `parking_lot`).
- **`LocalRwLock<T>`** — single-threaded async reader-writer lock; write-preferring.
- **`LocalSemaphore`** — counting semaphore for concurrency limiting / backpressure / resource
  pools. FIFO-fair `acquire`, opportunistic `try_acquire`.
- **`LocalBufferPool`** — per-core pool of recyclable, aligned IO buffers; a `PooledBuffer` is
  `'static` and returns itself on drop (composes `LocalSemaphore` + `AlignedBuffer`; `IoBuf` under
  `monoio`).
- **`Queue<T, N>`** — double-ended queue that stores up to `N` items inline before spilling to the
  heap.

## `site_d` — cross-thread SPSC primitives

- **`RingBuf<T>`** — lock-free single-producer single-consumer ring buffer with both sync (`try_*`)
  and async (`push().await` / `pop().await`) APIs. It uses the cached-index, cache-line-padded
  design of the fastest SPSC rings (cf. [`rtrb`](https://github.com/mgeier/rtrb),
  [`rigtorp::SPSCQueue`](https://github.com/rigtorp/SPSCQueue)). It is the fastest *async-native*
  SPSC queue we've measured — beating every async channel, and even `rigtorp::SPSCQueue` on latency
  — while a bare pure-sync C ring is still faster on the raw sync path (it has no waiter wakeup to
  do). See [BENCHMARKS.md](BENCHMARKS.md).
- **`triple_buffer`** — lock-free SPSC triple buffer for zero-copy, zero-allocation handoff of whole
  buffers, generic over the element type (`T: Zeroable + Copy`) with custom capacity and alignment
  (e.g. `4096` for O_DIRECT). Async backpressure; optional `monoio` `IoBuf` integration for `u8`
  buffers.
- **`bip_buffer`** — lock-free SPSC bip buffer (bipartite ring): an IO byte buffer that always hands
  out a single **contiguous** region per reserve/read, so grants go straight to io_uring/DMA
  zero-copy. Arbitrary capacity (watermark wrap), `Arc`-backed `'static` grants (`IoBuf`/`IoBufMut`
  under `monoio`).
- **`oneshot`** — single-value cross-core channel; the lightweight reply path for request/response.
- **`mpsc_light`** — single-queue control-plane MPSC: cloneable `Send` senders, **global FIFO**
  across senders, O(cap) memory per channel. `recv()` is polite by default (parks promptly — a
  shared reactor's co-located tasks pay nothing); `recv_hot()` opts into a ~70 ns spin-catch for
  consumers that own their thread. Batch `try_send_many`/`recv_many` amortize the lock under
  producer contention (2–3.6×). For a fixed 1:1 stream reach for `RingBuf`; for a single reply,
  `oneshot`.

All `site_d` primitives share one audited wakeup core (`notify::WaiterSlot`): a flag-gated `SeqCst`
handshake that skips the wake when nobody is parked.

## Performance

Measured on Apple M4 Max (`cargo bench`, release + LTO), against the fastest crate in each niche.
Every chart is generated from the committed benchmarks — never hand-drawn:

```bash
cargo bench                                            # writes docs/bench-data/*.csv
cargo run --manifest-path tools/chartgen/Cargo.toml    # -> docs/charts/*.svg + *.png (4x)
```

Each primitive gets a **throughput** chart and a **latency distribution**. Latency is plotted as a
percentile curve rather than a single number, because one mean hides the tail that actually decides
whether a primitive is usable — and throughput and latency are measured separately, never one
derived from the other. Full tables and caveats in [BENCHMARKS.md](BENCHMARKS.md).

### `RingBuf` — SPSC ring buffer

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/ringbuf-spsc-throughput.png" alt="RingBuf SPSC throughput 2 threads: cynosure 450 Melem/s, rtrb 400, ringbuf 220, thingbuf 158, std::mpsc 140, crossbeam 137, kanal 38, flume 19" width="100%">

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/ringbuf-async-throughput.png" alt="RingBuf async cross-core throughput: cynosure 90 Melem/s, kanal 68, tokio 15, flume 13" width="100%">

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/ringbuf-latency-dist.png" alt="RingBuf cross-thread handoff latency distribution: cynosure flat at 62 ns to p99 rising to 104 ns at p99.9, crossbeam 104 ns at p50 rising to 208 ns" width="100%">

### `mpsc_light` — control-plane channel

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/mpsc-light-fanin.png" alt="mpsc_light async fan-in at 16 producers: cynosure 73 Melem/s, kanal 28, async-channel 4.2, flume 3.4, std::mpsc 3.2, tokio 0.6" width="100%">

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/mpsc-light-latency-dist.png" alt="mpsc_light latency under paced fan-in: cynosure under 100 microseconds through p99.99, kanal past 1 ms beyond p99, tokio flat near 130 microseconds" width="100%">

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/mpsc-light-reactor-dist.png" alt="mpsc_light delivery latency inside a shared reactor: cynosure lowest at p50 through p99, tail rising past kanal at p99.9" width="100%">

### `triple_buffer` — zero-copy whole-buffer handoff

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/triplebuffer-bandwidth.png" alt="triple_buffer streamed bandwidth at 64 KB buffers: cynosure 19.7 GiB/s, crossbeam-recycle 19.2 GiB/s" width="100%">

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/triplebuffer-latency-dist.png" alt="triple_buffer rotation latency distribution to p99: cynosure 8.4 ns at p50, crossbeam-recycle 18.1" width="100%">

### `bip_buffer` — contiguous IO byte buffer

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/bipbuffer-throughput.png" alt="bip_buffer sustained grant cycles: bbqueue 213 Mops/s, cynosure 137 Mops/s" width="100%">

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/bipbuffer-latency-dist.png" alt="bip_buffer grant cycle latency distribution to p99: bbqueue 5.3 ns at p50, cynosure 7.6 ns" width="100%">

### `oneshot` — single-value reply channel

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/oneshot-throughput.png" alt="oneshot sustained create send receive: cynosure 81 Mops/s, tokio 79.6, futures-channel 63.4" width="100%">

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/oneshot-latency-dist.png" alt="oneshot latency distribution to p99: cynosure 12.5 ns at p50, tokio 12.6, futures-channel 22.9" width="100%">

### `site_c` — single-threaded primitives

**`LocalMutex`**

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/mutex-throughput.png" alt="LocalMutex sustained lock and unlock: cynosure 3349 Mops/s, parking_lot 475, std Mutex 236" width="100%">

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/mutex-latency-dist.png" alt="LocalMutex latency distribution to p99: cynosure 0.33 ns at p50 and 0.41 at p99, parking_lot near 2.4, std Mutex 4.3" width="100%">

**`LocalRwLock`**

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/rwlock-throughput.png" alt="LocalRwLock sustained read lock: cynosure 480 Mops/s, parking_lot 318, std RwLock 234" width="100%">

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/rwlock-latency-dist.png" alt="LocalRwLock read latency distribution to p99: cynosure 2.4 ns at p50, parking_lot 3.5, std 4.9" width="100%">

**`LocalSemaphore`**

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/semaphore-throughput.png" alt="LocalSemaphore sustained acquire and release: cynosure 562 Mops/s, async-lock 169, tokio 159" width="100%">

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/semaphore-latency-dist.png" alt="LocalSemaphore latency distribution to p99: cynosure 2.0 ns at p50, async-lock 5.8, tokio 6.5" width="100%">

**`LocalBufferPool`**

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/pool-throughput.png" alt="LocalBufferPool sustained acquire and return: lockfree-object-pool 233 Mops/s, object-pool 200, cynosure 143" width="100%">

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/pool-latency-dist.png" alt="LocalBufferPool latency distribution to p99: lockfree-object-pool 4.4 ns at p50, object-pool 5.5, cynosure 5.6" width="100%">

**`Queue<T, N>`**

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/queue-throughput.png" alt="Queue sustained push and pop on a warm queue: VecDeque 979 Mops/s, cynosure Queue 392" width="100%">

<img src="https://raw.githubusercontent.com/Zk2u/cynosure/main/docs/charts/queue-latency-dist.png" alt="Queue push and pop latency distribution to p99 on a warm queue: VecDeque 0.98 ns at p50, cynosure Queue 2.52" width="100%">

Several charts above are **not** wins, and they stay in on purpose:

* `bip_buffer` and `LocalBufferPool` trade a few ns for `'static`, io_uring-ready grants plus async
  backpressure that the borrow-based competitors (`bbqueue`, `object-pool`) cannot offer.
* `Queue`'s advantage is **avoiding an allocation** on a fresh queue, not a faster per-op path — on a
  warm, pre-sized queue `VecDeque`'s tight ring wins. Reach for `Queue` for the short-lived, small
  queues it exists for.
* `mpsc_light`'s polite `recv()` gives up the far tail inside a shared reactor to stop starving
  co-located tasks; `recv_hot()` trades back the other way.

## Correctness

The lock-free and `unsafe`-heavy paths are validated with [Miri](https://github.com/rust-lang/miri)
and, for the wakeup / memory-ordering races, [loom](https://github.com/tokio-rs/loom). See
[MIRI.md](MIRI.md).

## License

MIT
