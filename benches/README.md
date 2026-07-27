# Cynosure Benchmarks

[Criterion.rs](https://github.com/bheisler/criterion.rs) benchmarks for the cynosure primitives,
with comparisons against the relevant standard-library and ecosystem alternatives. See
[`../BENCHMARKS.md`](../BENCHMARKS.md) for results and analysis.

## Benches

| bench | covers | compares against |
|---|---|---|
| `spsc_compare` | `RingBuf` SPSC — latency, throughput (1/2-thread sync, blocking, 1-thread async, cross-thread async + round-trip latency) | rtrb, ringbuf, thingbuf, kanal, flume, crossbeam, std::mpsc, tokio::mpsc |
| `ringbuf_spsc` | `RingBuf` sync + async + bulk-slice + buffer sizes | std::mpsc, crossbeam |
| `triplebuffer` | triple buffer throughput + rotation latency | crossbeam recycling channels |
| `bipbuffer` | bip buffer reserve/commit/read/release | bbqueue |
| `mpsc_light` | fan-in throughput (1–16 producers) + ping-pong wakeup latency | kanal |
| `oneshot` | create + send + recv | tokio, futures-channel, oneshot, async-oneshot, catty |
| `semaphore` | `LocalSemaphore` try_acquire | tokio, async-lock |
| `pool` | `LocalBufferPool` acquire/return | object-pool, lockfree-object-pool, Mutex<Vec> |
| `mutex` | `LocalMutex` | parking_lot, std, tokio, RefCell |
| `rwlock` | `LocalRwLock` | parking_lot, std, tokio, RefCell |
| `queue` | `Queue<T, N>` (incl. `retain`) | VecDeque |

Three further benches are plain binaries rather than criterion suites: they measure the data the
README charts are drawn from and write it to `docs/bench-data/`.

| bench | produces |
|---|---|
| `latency_dist` | `latency-primitives.csv` — latency **distributions** for every primitive |
| `primitives_throughput` | `throughput-measured.csv` — sustained throughput, measured separately from latency |
| `control_plane` | `latency.csv` — `mpsc_light` under paced/real-world control-plane load |

## Running

```bash
cargo bench                       # everything
cargo bench --bench spsc_compare  # one suite
cargo bench --bench mutex -- "Latency"   # filter by group/name

cargo run --manifest-path ../tools/chartgen/Cargo.toml   # redraw docs/charts/* from the data
```

HTML reports are written to `target/criterion/report/index.html`.

## Notes

- Results are relative; focus on ratios, not absolute numbers, and re-run on your own hardware.
- All benches use `black_box` to prevent the optimizer from eliding the measured work.
- The bench profile builds with **LTO** (`[profile.bench]`) so cynosure's lean inlined ops inline
  across the crate boundary — i.e. what a `release` + LTO user gets. Without it, sub-5 ns ops read
  artificially slow (the wins live in the inlining).
- Numbers differ across architectures (notably x86 vs AArch64 for the atomic-ordering hot paths).
- Sub-nanosecond operations cannot be timed individually (`Instant::now()` costs more than they do),
  so `latency_dist` samples them in batches of 512 and reports **to p99 only** — past that the
  batch means measure OS preemption rather than the primitive. See BENCHMARKS.md for the control
  experiment that establishes this.
