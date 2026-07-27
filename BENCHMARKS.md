# Cynosure Benchmarks

Measured on **Apple M4 Max** (aarch64, macOS), Rust stable, `--release`. Numbers are a single
representative run; criterion confidence intervals are tight but absolute values vary run-to-run
(and are very different on x86 — see the memory-ordering notes). Treat these as ratios, not
gospel — and run `cargo bench` on your own hardware for decisions.

**Methodology note:** numbers were measured per-bench on a cool machine. A full back-to-back
`cargo bench` run thermally throttles the later benches by ~15% on this hardware — reproduce
individual benches in isolation when checking a specific row.

All benches use `black_box`/`DoNotOptimize` to prevent the optimizer from eliding the work.

---

## `RingBuf<T>` — SPSC ring buffer

The headline structure. Cross-crate comparison via `cargo bench --bench spsc_compare`, against the
recognized fast SPSC/MPSC options (`u32`, capacity 1024).

### Latency — 1 push + 1 pop, single thread (lower = better)

| crate | ns/op |
|---|---|
| **cynosure** | **2.2** |
| rtrb | 2.5 |
| kanal | 3.6 |
| ringbuf | 4.4 |
| thingbuf | 5.4 |
| std::mpsc | 6.3 |
| crossbeam | 8.5 |
| flume | 11.1 |
| tokio mpsc | 12.1 |

### Throughput — single thread, interleaved fill/drain (Melem/s, higher = better)

| crate | Melem/s |
|---|---|
| **cynosure** | **~420** |
| rtrb | ~340 |
| kanal | ~230 |
| ringbuf | ~185 |
| thingbuf | ~168 |
| std::mpsc | ~130 |
| crossbeam | ~100 |
| tokio mpsc | ~73 |
| flume | ~58 |

### Throughput — 2 threads, busy-spin lock-free ceiling (Melem/s)

| crate | Melem/s |
|---|---|
| **cynosure** | **~450** |
| rtrb | ~400 |
| ringbuf | ~220 |
| thingbuf | ~158 |
| std::mpsc | ~140 |
| crossbeam | ~50–180¹ |
| kanal | ~38 |
| flume | ~10 |

¹ crossbeam's `try_recv` spin is unstable under genuine 2-thread contention (the MPMC channels
serialize on internal state; the lock-free ring buffers don't).

### Throughput — 2 threads, blocking / park-unpark (Melem/s)

| crate | Melem/s |
|---|---|
| std::mpsc | ~250 |
| **cynosure** | **~240** |
| crossbeam | ~140 |
| kanal | ~45 |
| tokio | ~14 |
| flume | ~20 |

std's native park/unpark path edges cynosure here (its numbers also swing the most run-to-run);
cynosure leads the rest of the field by 5×+. For blocking hand-off at rate, prefer the spin path
above — this mode is for CPU-friendly waiting.

### Throughput — async, 1 thread / 2 tasks on one executor (Melem/s)

Producer and consumer as cooperating tasks on a single-threaded runtime, every contender driven by
the same `LocalPool`.

| crate | Melem/s |
|---|---|
| **cynosure** | **275** |
| kanal | 198 |
| thingbuf | 86 |
| flume | 63 |
| tokio mpsc | 45 |

### Throughput — async, 2 threads / cross-core (Melem/s)

The headline thread-per-core case: producer and consumer on **separate threads**, each on its own
`block_on` executor, communicating only via `push().await` / `pop().await` with real cross-thread
wakeups (the path the F1/F2 fixes protect).

| crate | Melem/s |
|---|---|
| **cynosure** | **~90** |
| kanal | ~68 |
| tokio mpsc | ~15 |
| flume | ~13 |

Unlike the single-thread async case, the cross-core path parks and wakes for real, which caps
throughput well below the same-thread number (~230) — for every contender. cynosure remains the
fastest async-native option (~1.3× kanal, ~6× tokio), and this row is the most run-sensitive in
the file: expect wide variance from core placement and load.

### Round-trip latency — async, 2 threads / cross-core (lower = better)

A strict ping-pong over two ring buffers: every message parks one side and is woken by the other
across cores.

| crate | round-trip |
|---|---|
| **cynosure** | 3.82 µs |
| tokio mpsc | 3.87 µs |

Tied — strict ping-pong pays a full OS park/unpark per message, so this is **syscall-bound, not
queue-bound** (~3.8 µs is the kernel, not the queue). The queue implementation only separates the
field once the buffer provides slack to amortize parking (the throughput row above). For
latency-critical cross-core handoff, busy-spin rather than park.

### How it compares to the absolute fastest (C / C++), and the honest caveat

Against a maximally-tuned tight loop (LTO, `DoNotOptimize`), cynosure vs the C++ reference and a
bare hand-rolled C11 cached-index ring:

| (1 thread) | latency | notes |
|---|---|---|
| hand-rolled C11 ring (pure sync) | **0.79 ns** | no async, no waiter wakeup |
| **cynosure** (Rust) | 1.29 ns | async-native, memory-safe |
| `rigtorp::SPSCQueue` (C++) | 2.46 ns | pure sync |
| `ringbuffer-spsc` (Rust) | 2.03 ns | pure sync |

cynosure is the fastest **async-native** SPSC queue measured — it beats every async channel, ties
the fastest pure-sync Rust crate (`ringbuffer-spsc`) on throughput, and beats the canonical C++
library (`rigtorp`) on latency. A **bare pure-sync C ring is still faster** on the raw sync path
(~1.6× on latency): it does zero waiter bookkeeping, whereas cynosure pays ~one atomic load + branch
per op to check whether an async waiter needs waking (free on AArch64 `ldarb`, the price of having
`push().await`/`pop().await` built in). In any workload that ever waits, that tax is dwarfed by *not
busy-spinning a core*, which is the only way a pure-sync ring can wait.

---

## `triple_buffer` — zero-copy whole-buffer handoff

`cargo bench --bench triplebuffer`. A writer fills whole buffers and publishes them by pointer swap;
a reader consumes them. Baseline: `crossbeam-recycle` (two channels recycling a 3-buffer pool — the
realistic zero-alloc alternative).

### Throughput, 2 threads, busy-spin (sync `try_publish`/`try_next` vs `try_send`/`try_recv`)

| buffer size | cynosure | crossbeam-recycle | cynosure handoffs/s |
|---|---|---|---|
| 4 KB | **14.2 GiB/s** | 13.6 GiB/s | ~3.7M |
| 64 KB | **19.7 GiB/s** | 19.2 GiB/s | ~320K |
| 4 MiB (O_DIRECT) | **19.7 GiB/s** | 19.6 GiB/s | ~5K |

**Ahead at every size.** The lead is clearest at 4 KB (+4–5%), which is **coordination-bound** — the
fixed per-handoff cost (rotation + spin ping-pong) caps the *handoff rate*, so 4 KB shows the most
handoffs/s but the least bandwidth. At 64 KB / 4 MiB both are **memory-bandwidth-bound** (~20 GiB/s;
the zero-copy pointer swap means fill+read saturate memory BW), so the lead narrows to a hair.
(Before the sync `try_*` API the spin bench drove the async future and *trailed* crossbeam at 4 KB;
the lean sync path is what put it ahead.)

### Rotation latency, single thread (lower = better)

| operation | cynosure | crossbeam-recycle | `triple_buffer` crate |
|---|---|---|---|
| publish + next cycle (2 rotations) | 10.6–10.8 ns | 21.3 ns | **2.4 ns** |

The `triple_buffer` crate rotates ~4.5× faster — and that gap is a *capability price*, not a
missed optimization. Its rotation is a bare index swap on a **latest-value, lossy** channel: the
writer overwrites in place and the reader may skip frames. cynosure's is a **lossless depth-1
handoff** of *owned, `'static`, aligned* buffers (`try_publish` backpressures until the reader
takes the middle buffer, and the grant goes straight to io_uring/DMA). Different contracts: pick
the crate for shared-memory latest-value telemetry, cynosure when every frame must survive and go
to the kernel zero-copy. vs the realistic same-contract baseline (crossbeam-recycle) cynosure is
~2× faster.

~5 ns to hand off an entire buffer of any size, zero-copy — **~2× faster** than the channel
equivalent (via the sync `try_*` API; the async futures add ~5 ns of poll overhead on top).

---

## `bip_buffer` — contiguous SPSC byte buffer

`cargo bench --bench bipbuffer`. reserve + commit + read + release one contiguous chunk, vs
[`bbqueue`](https://github.com/jamesmunns/bbqueue) (the reference bip buffer).

| reserve+commit+read+release | cynosure | bbqueue |
|---|---|---|
| 16 B | 7.3 ns | 5.5 ns |
| 64 B | 7.3 ns | 5.5 ns |
| 256 B | 7.3 ns | 5.6 ns |

Both are zero-copy and flat (size-independent). bbqueue is ~1.3× faster on the raw cycle because its
grants *borrow* the producer/consumer; cynosure's grants are `Arc`-backed so they're `'static` and go
straight to io_uring across a completion (`IoBuf`/`IoBufMut`) — bbqueue's can't. That is **four**
atomic refcount operations per full cycle (a clone into each of the write and read grants, and a
drop as `commit`/`release` consume them) — the price of the `'static` grant. See "Where cynosure
loses, and why" below.

---

## `mpsc_light` — single-queue control channel

One small shared queue (a `VecDeque` under a tuned TTAS spinlock), O(cap) memory per channel,
**global FIFO** across senders, cloneable `Send` senders with no executor constraint, and a
single-consumer **buffer-steal** drain.

**Its competitive envelope starts at 2 producers.** For a fixed 1:1 pairing, the right cynosure
tool is `RingBuf` (which leads the field in every mode) or `oneshot` for a single reply — a
single-producer row for this channel benchmarks the wrong tool from this crate's own shelf. The
N=1 numbers below exist for the *dynamic-senders* case (one sender today, cloned to three
tomorrow): a flexibility price, not this primitive's performance posture.

Async fan-in, N producers → 1 consumer, same executor for both crates (Melem/s):

| producers / cap | **cynosure light** | kanal |
|---|---|---|
| 1 / 256 | **~86** | ~53 |
| 4 / 256 | **~68** | ~37 |
| 16 / 256 | **~67** | ~28 |
| 16 / 1024 | **~50** | ~27 |

**Two receive modes.** Default `recv()` is *polite*: it parks promptly, so a consumer
embedded in a shared reactor costs co-located tasks nothing while idle. `recv_hot()`
opts into a short pre-park spin for consumers that own their thread. Ping-pong wakeup
latency (one-way, dedicated threads): **`recv_hot` ~70–90 ns vs kanal ~1961 ns (~25×)**;
default `recv` pays the plain park path there (~1.9 µs) — but *in a shared reactor*
(its real deployment, `cargo bench --bench control_plane` scenario C) the default is
best-in-class: **p50 166 ns / p99 291 ns** vs kanal 792 ns / 18.8 µs, with co-located
task throughput on par with kanal/tokio (59k vs 67k/61k iters/ms — vs **830** when a
fixed spin ran inside `poll()`, the measured cost of impoliteness).

Real-world control-plane suite (`benches/control_plane.rs`): paced fan-in (8×1 µs)
**p50 6.3 µs vs kanal 59 µs / tokio 113 µs**, with kanal's p99 tail at **2.7 ms**;
parked dispatch (20 µs pacing) roughly ties (~1 µs behind p50, futex-bound for all).

Contention-*stable* by design: throughput stays flat as producers are added, where single-queue
MPMC channels (kanal/crossbeam/flume) degrade 2–100×+ under the same fan-in.

**Regime scope (measured, both directions):** these wins hold on dedicated-thread /
thread-per-core executors — the crate's target (monoio-style runtimes) — where the design's
assumptions (cheap precise wakeups, spin windows on a dedicated core) pay off. kanal is
executor-agnostic by construction (it spins where we rely on wakeups), so on a shared
work-stealing pool (tokio's multi-thread runtime; verified against kanal's own
`rust-channel-benchmarks` suite) the ranking flips and kanal leads (~1.4× at control caps,
more at its cap-1/cap-1M operating points). Generalist-by-robustness vs
specialist-by-assumption: pick by runtime.

**Batch API** (`try_send_many` / `recv_many`): one lock acquisition + one consumer signal per
*batch* instead of per item. Uncontended it's a wash (the consumer already buffer-steals, and an
uncontended lock is ~free), but under producer contention — where each contended lock is the
whole cost — batching in chunks of 64 is a **2–3.6× throughput win** (N=8 producers: +262%; N=16:
+159%). This is the bursty-producer path (e.g. a per-core broadcast fan-out draining a queue of
events). Honest caveats:
kanal is MPMC (multi-consumer) — cynosure wins the MPSC job partly by *exploiting*
single-consumer (the O(1) whole-queue steal is unsound for MPMC); and the 16/1024 row trades
some big-buffer throughput for the latency/small-cap wins — a deliberate control-plane call.

---

## `oneshot` — single-value reply channel

`cargo bench --bench oneshot`. create + send + receive one value (allocator-bound — one shared cell
per channel).

| | cycle |
|---|---|
| async-oneshot | ~11.9 ns |
| **cynosure** | **~12.0 ns** |
| tokio | ~12.0 ns |
| oneshot crate | 13.4 ns |
| futures-channel | 16.3 ns |
| catty | ~18 ns |

Three contenders — cynosure, tokio, `async-oneshot` — sit in a **~0.2 ns dead heat at the
allocator floor** (the cycle is dominated by the one `Arc`/`Box` alloc+free per channel; the
order flips run-to-run). cynosure got there by removing two hot-path RMWs a single-receiver
analysis proved unnecessary. To meaningfully beat this you'd have to *stop allocating* — a
pool-backed `oneshot_in(&pool)` (a ~3–4 ns candidate) is the real lever, deferred.

---

## `LocalSemaphore` — single-threaded async semaphore

`cargo bench --bench semaphore`. uncontended `try_acquire` + release. Single-core, non-atomic
(`Cell`); the others are thread-safe (atomics).

| | latency |
|---|---|
| **cynosure** | **2.4 ns** |
| async-lock | 6.8 ns |
| tokio | 7.4 ns |

**~3× faster** — the non-atomic single-core path, with the wake cold-outlined so the uncontended
release is just `permits += n` and a branch.

---

## `LocalBufferPool` — recyclable IO buffer pool

`cargo bench --bench pool`. acquire + return one 4 KiB buffer.

| | cycle |
|---|---|
| lockfree-object-pool | 4.6 ns |
| object-pool | 6.5 ns |
| **cynosure** | 8.7 ns |
| Mutex<Vec> | 8.5 ns |

cynosure beats a `Mutex<Vec>` pool but trails the borrow-based object pools — same trade as the bip
buffer: its `PooledBuffer` is `Rc`-backed so it's `'static` + `IoBuf` (hands to io_uring, returns on
drop) and supports **async `acquire` with bounded backpressure**, which the others have no path for.
The ~`Rc` refcount is the price of that capability.

---

## `LocalMutex<T>` — single-threaded async mutex

`cargo bench --bench mutex`. Non-atomic (`Cell`), so it sits near the `RefCell` floor while working
across `.await`.

| | latency |
|---|---|
| **`LocalMutex::try_lock`** | **0.57 ns** |
| **`LocalMutex::lock().await`** | **2.24 ns** |
| parking_lot::Mutex | 1.84 ns |
| std::Mutex | 4.26 ns |
| tokio::Mutex (async) | 11.7 ns |
| `RefCell::borrow_mut` (floor) | 0.29 ns |

`try_lock` is ~3× faster than `parking_lot`; the async `lock` is ~5× faster than tokio's async
mutex.

---

## `LocalRwLock<T>` — single-threaded async reader-writer lock

`cargo bench --bench rwlock`. Write-preferring.

| | read | write |
|---|---|---|
| `RefCell` (floor) | 0.27 ns | 0.29 ns |
| **`LocalRwLock` (try)** | **1.85 ns** | 2.00 ns |
| **`LocalRwLock` (async)** | **3.11 ns** | 2.78 ns |
| parking_lot | 3.31 ns | 1.83 ns |
| std::RwLock | 4.43 ns | 2.14 ns |
| tokio::RwLock (async) | 11.7 ns | 11.1 ns |

Reads are ~1.8× faster than `parking_lot` (a non-atomic `Cell` increment vs an atomic). Writes are
roughly tied with `parking_lot` (cynosure carries a bit more per-release bookkeeping for its
cancellation-safe wakeup). Throughput: reads 20.4 µs vs parking_lot 32 µs vs std 45 µs; writes
19.7 µs vs parking_lot 18.3 µs vs std 21.2 µs.

---

## `Queue<T, N>` — inline-spill deque

`cargo bench --bench queue`. Stores up to `N` items inline before spilling to a `VecDeque`.

| operation (`Queue<i32, 8>` vs `VecDeque`) | Queue | VecDeque |
|---|---|---|
| push_back | **4.7 ns** | 13.6 ns |
| pop_front | **3.4 ns** | 7.3 ns |
| push 20 | **71 ns** | 97 ns |
| iterate | 15.7 ns | 15.6 ns |

The win is **allocation avoidance, not a faster per-op path**. These rows start from a fresh
`VecDeque::new()` (capacity 0), so `VecDeque::push_back` is paying a heap allocation that `Queue`'s
inline storage skips — which is exactly the point for the short-lived, small queues it exists for
(a waiter list, a drain buffer). On a *warm, pre-sized* queue where neither side allocates,
`VecDeque`'s tight ring is the faster one: **979 vs 392 Mops/s** sustained push+pop
(`cargo bench --bench primitives_throughput`). Reach for `Queue` to avoid the allocation, not to
beat `VecDeque` at steady-state churn.

---

## Where cynosure loses, and why

Three primitives trail a competitor at p50. In each case the cause is a *capability the competitor
does not provide*, and it is visible in the source rather than inferred:

**`bip_buffer` — 7.7 ns vs bbqueue 5.6 ns (+2.1).** `try_reserve` and `try_read` each clone the
channel's `Arc` into the grant they hand out (`bipbuffer.rs`, two `self.inner.clone()` sites), and
`commit`/`release` consume the grant, dropping it. That is **four atomic refcount operations per
full cycle**. bbqueue's grants *borrow* the producer and consumer, so it pays none — and cannot
hand a grant to io_uring across a completion, which is exactly what the `Arc` buys.

**`LocalBufferPool` — 5.7 ns vs lockfree-object-pool 4.5 ns (+1.2).** Same shape, one tier cheaper:
`PooledBuffer` carries an `Rc<Inner>` (clone on checkout, drop on return — non-atomic, so cheaper
than the bip buffer's `Arc`), plus a `ManuallyDrop::take` and the semaphore permit accounting that
gives the pool async backpressure. The borrow-based pools have no handle to refcount.

**`Queue<T, N>` — 2.6 ns vs `VecDeque` 1.06 ns (+1.5).** `Queue` is an *enum* — `Inline { buf, head,
tail, len }` or `Heap(VecDeque)` — so every `push_back`/`pop_front` matches on the discriminant
before doing any work, and the inline arm then maintains three fields with explicit wrap branches
where `VecDeque` masks two. The gap is the **cost of the inline/heap duality**, which is the same
mechanism that makes the allocation disappear. It is a fixed per-op tax, not a scaling problem.

## The p99+ "spikes" are the scheduler, not the primitives

An earlier draft of the latency charts showed every batched series spiking 3–7× at p99.9. That was
a measurement artifact, and the check that proves it is simple: convert each series' p99.9 excess
into a *per-batch* figure (batch = 512 ops).

| chart | cynosure | competitor |
|---|---|---|
| `LocalMutex` | 5833 ns | parking_lot **167 ns** |
| `LocalRwLock` | **375 ns** | parking_lot 7000 ns |
| `bip_buffer` | 7249 ns | bbqueue **875 ns** |

The excess clusters at **~6–7.7 µs — one OS context switch — for whichever series got unlucky**, and
which series that is flips between charts for the *same* implementations. A control run with no
primitive at all (an empty `wrapping_add` loop) reproduces the same shape once the batch is long
enough to catch a preemption: 5792 ns of excess at batch = 4096.

So the batched charts stop at **p99**, where the samples still describe the primitive. Series that
are timed *directly* — `RingBuf`'s cross-thread handoff and the `mpsc_light` control-plane
scenarios, both µs-scale per-operation measurements — keep their full curve, because there the tail
is genuinely the primitive's.

## Running the benchmarks

```bash
cargo bench                              # everything

# cross-crate comparisons (criterion)
cargo bench --bench spsc_compare         # RingBuf vs the field (sync, async, threaded)
cargo bench --bench ringbuf_spsc         # RingBuf vs std::mpsc / crossbeam, by element type
cargo bench --bench triplebuffer         # triple buffer vs crossbeam-recycle + triple_buffer crate
cargo bench --bench bipbuffer            # bip buffer vs bbqueue
cargo bench --bench oneshot              # vs tokio / futures / oneshot / async-oneshot / catty
cargo bench --bench mpsc_light           # fan-in + ping-pong latency vs kanal
cargo bench --bench semaphore            # vs tokio / async-lock
cargo bench --bench pool                 # vs object-pool / lockfree-object-pool / Mutex<Vec>
cargo bench --bench mutex                # LocalMutex
cargo bench --bench rwlock               # LocalRwLock
cargo bench --bench queue                # Queue vs VecDeque

# chart data producers (plain binaries, not criterion)
cargo bench --bench latency_dist         # -> docs/bench-data/latency-primitives.csv
cargo bench --bench primitives_throughput # -> docs/bench-data/throughput-measured.csv
cargo bench --bench control_plane        # -> docs/bench-data/latency.csv (+ prints the table)

# regenerate every chart from the data above
cargo run --manifest-path tools/chartgen/Cargo.toml
```

The bench profile builds with LTO so cynosure's lean inlined ops inline across the crate boundary —
i.e. what a `release` + LTO user gets. HTML reports land in `target/criterion/report/index.html`.

`docs/bench-data/throughput.csv` is the one file that is *curated* rather than emitted: it carries
the cross-crate numbers read off the criterion suites above. Everything else regenerates.

---

*Numbers on Apple M4 Max. They will differ on x86 (where the SeqCst publish store is a barrier
rather than the free `stlr` it is on AArch64) and on other hardware. Run them yourself.*
