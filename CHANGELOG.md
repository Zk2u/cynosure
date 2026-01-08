# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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