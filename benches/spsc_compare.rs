//! SPSC comparison: cynosure RingBuf vs the field.
//!
//! Ring buffers (lock-free SPSC): cynosure, rtrb, ringbuf, thingbuf.
//! Channels (general MPMC/MPSC):  crossbeam, kanal, flume.
//!
//! - Latency: single-thread 1 push + 1 pop.
//! - Throughput (1 thread): interleaved fill/drain.
//! - Throughput (2 threads): real producer + consumer threads, busy-spin on the
//!   non-blocking fast path (lock-free hand-off ceiling).
//! - Throughput (2 threads, blocking): park/unpark mode (CPU-friendly).

use std::sync::Arc;

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use cynosure::site_d::ringbuf::RingBuf;
use ringbuf::traits::{Consumer as _, Producer as _, Split as _};

const CAP: usize = 1024;
const ITEMS: u64 = 1_000_000;

// ============================================================================
// Latency: single push + pop (same thread)
// ============================================================================

fn bench_latency(c: &mut Criterion) {
    let mut g = c.benchmark_group("SPSC Latency");

    g.bench_function("cynosure", |b| {
        let (mut p, mut c) = RingBuf::<u32>::new(CAP).split();
        b.iter(|| {
            p.try_push(black_box(42u32)).unwrap();
            black_box(c.try_pop().unwrap())
        })
    });
    g.bench_function("rtrb", |b| {
        let (mut p, mut c) = rtrb::RingBuffer::<u32>::new(CAP);
        b.iter(|| {
            p.push(black_box(42u32)).unwrap();
            black_box(c.pop().unwrap())
        })
    });
    g.bench_function("ringbuf", |b| {
        let (mut p, mut c) = ringbuf::HeapRb::<u32>::new(CAP).split();
        b.iter(|| {
            p.try_push(black_box(42u32)).unwrap();
            black_box(c.try_pop().unwrap())
        })
    });
    g.bench_function("thingbuf", |b| {
        let tb = thingbuf::ThingBuf::<u32>::new(CAP);
        b.iter(|| {
            tb.push(black_box(42u32)).unwrap();
            black_box(tb.pop().unwrap())
        })
    });
    g.bench_function("crossbeam", |b| {
        let (tx, rx) = crossbeam_channel::bounded::<u32>(CAP);
        b.iter(|| {
            tx.try_send(black_box(42u32)).unwrap();
            black_box(rx.try_recv().unwrap())
        })
    });
    g.bench_function("kanal", |b| {
        let (tx, rx) = kanal::bounded::<u32>(CAP);
        b.iter(|| {
            tx.try_send(black_box(42u32)).unwrap();
            black_box(rx.try_recv().unwrap().unwrap())
        })
    });
    g.bench_function("flume", |b| {
        let (tx, rx) = flume::bounded::<u32>(CAP);
        b.iter(|| {
            tx.try_send(black_box(42u32)).unwrap();
            black_box(rx.try_recv().unwrap())
        })
    });
    g.bench_function("std", |b| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<u32>(CAP);
        b.iter(|| {
            tx.try_send(black_box(42u32)).unwrap();
            black_box(rx.try_recv().unwrap())
        })
    });
    g.bench_function("tokio", |b| {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(CAP);
        b.iter(|| {
            tx.try_send(black_box(42u32)).unwrap();
            black_box(rx.try_recv().unwrap())
        })
    });

    g.finish();
}

// ============================================================================
// Throughput: single-thread interleaved fill/drain
// ============================================================================

fn bench_throughput_st(c: &mut Criterion) {
    let mut g = c.benchmark_group("SPSC Throughput (1 thread)");
    g.throughput(Throughput::Elements(ITEMS));

    g.bench_function("cynosure", |b| {
        let (mut p, mut c) = RingBuf::<u32>::new(CAP).split();
        b.iter(|| {
            for i in 0..ITEMS {
                while p.try_push(i as u32).is_err() {
                    black_box(c.try_pop());
                }
            }
            while c.try_pop().is_some() {}
        })
    });
    g.bench_function("rtrb", |b| {
        let (mut p, mut c) = rtrb::RingBuffer::<u32>::new(CAP);
        b.iter(|| {
            for i in 0..ITEMS {
                while p.push(i as u32).is_err() {
                    black_box(c.pop().ok());
                }
            }
            while c.pop().is_ok() {}
        })
    });
    g.bench_function("ringbuf", |b| {
        let (mut p, mut c) = ringbuf::HeapRb::<u32>::new(CAP).split();
        b.iter(|| {
            for i in 0..ITEMS {
                while p.try_push(i as u32).is_err() {
                    black_box(c.try_pop());
                }
            }
            while c.try_pop().is_some() {}
        })
    });
    g.bench_function("thingbuf", |b| {
        let tb = thingbuf::ThingBuf::<u32>::new(CAP);
        b.iter(|| {
            for i in 0..ITEMS {
                while tb.push(i as u32).is_err() {
                    black_box(tb.pop());
                }
            }
            while tb.pop().is_some() {}
        })
    });
    g.bench_function("crossbeam", |b| {
        let (tx, rx) = crossbeam_channel::bounded::<u32>(CAP);
        b.iter(|| {
            for i in 0..ITEMS {
                while tx.try_send(i as u32).is_err() {
                    black_box(rx.try_recv().ok());
                }
            }
            while rx.try_recv().is_ok() {}
        })
    });
    g.bench_function("kanal", |b| {
        let (tx, rx) = kanal::bounded::<u32>(CAP);
        b.iter(|| {
            for i in 0..ITEMS {
                while !tx.try_send(i as u32).unwrap() {
                    black_box(rx.try_recv().ok());
                }
            }
            while rx.try_recv().unwrap().is_some() {}
        })
    });
    g.bench_function("flume", |b| {
        let (tx, rx) = flume::bounded::<u32>(CAP);
        b.iter(|| {
            for i in 0..ITEMS {
                while tx.try_send(i as u32).is_err() {
                    black_box(rx.try_recv().ok());
                }
            }
            while rx.try_recv().is_ok() {}
        })
    });
    g.bench_function("std", |b| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<u32>(CAP);
        b.iter(|| {
            for i in 0..ITEMS {
                while tx.try_send(i as u32).is_err() {
                    black_box(rx.try_recv().ok());
                }
            }
            while rx.try_recv().is_ok() {}
        })
    });
    g.bench_function("tokio", |b| {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(CAP);
        b.iter(|| {
            for i in 0..ITEMS {
                while tx.try_send(i as u32).is_err() {
                    black_box(rx.try_recv().ok());
                }
            }
            while rx.try_recv().is_ok() {}
        })
    });

    g.finish();
}

// ============================================================================
// Throughput: real producer + consumer threads, busy-spin fast path
// ============================================================================

fn bench_throughput_threaded(c: &mut Criterion) {
    let mut g = c.benchmark_group("SPSC Throughput (2 threads)");
    g.throughput(Throughput::Elements(ITEMS));

    g.bench_function("cynosure", |b| {
        b.iter(|| {
            let (mut p, mut c) = RingBuf::<u32>::new(CAP).split();
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    while p.try_push(i as u32).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let cons = std::thread::spawn(move || {
                let mut n = 0u64;
                while n < ITEMS {
                    if let Some(v) = c.try_pop() {
                        black_box(v);
                        n += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });
    g.bench_function("rtrb", |b| {
        b.iter(|| {
            let (mut p, mut c) = rtrb::RingBuffer::<u32>::new(CAP);
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    while p.push(i as u32).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let cons = std::thread::spawn(move || {
                let mut n = 0u64;
                while n < ITEMS {
                    if let Ok(v) = c.pop() {
                        black_box(v);
                        n += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });
    g.bench_function("ringbuf", |b| {
        b.iter(|| {
            let (mut p, mut c) = ringbuf::HeapRb::<u32>::new(CAP).split();
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    while p.try_push(i as u32).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let cons = std::thread::spawn(move || {
                let mut n = 0u64;
                while n < ITEMS {
                    if let Some(v) = c.try_pop() {
                        black_box(v);
                        n += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });
    g.bench_function("thingbuf", |b| {
        b.iter(|| {
            let tb = Arc::new(thingbuf::ThingBuf::<u32>::new(CAP));
            let tbp = tb.clone();
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    while tbp.push(i as u32).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let cons = std::thread::spawn(move || {
                let mut n = 0u64;
                while n < ITEMS {
                    if let Some(v) = tb.pop() {
                        black_box(v);
                        n += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });
    g.bench_function("crossbeam", |b| {
        b.iter(|| {
            let (tx, rx) = crossbeam_channel::bounded::<u32>(CAP);
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    while tx.try_send(i as u32).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let cons = std::thread::spawn(move || {
                let mut n = 0u64;
                while n < ITEMS {
                    if let Ok(v) = rx.try_recv() {
                        black_box(v);
                        n += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });
    g.bench_function("kanal", |b| {
        b.iter(|| {
            let (tx, rx) = kanal::bounded::<u32>(CAP);
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    while !tx.try_send(i as u32).unwrap() {
                        std::hint::spin_loop();
                    }
                }
            });
            let cons = std::thread::spawn(move || {
                let mut n = 0u64;
                while n < ITEMS {
                    if let Some(v) = rx.try_recv().unwrap() {
                        black_box(v);
                        n += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });
    g.bench_function("flume", |b| {
        b.iter(|| {
            let (tx, rx) = flume::bounded::<u32>(CAP);
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    while tx.try_send(i as u32).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let cons = std::thread::spawn(move || {
                let mut n = 0u64;
                while n < ITEMS {
                    if let Ok(v) = rx.try_recv() {
                        black_box(v);
                        n += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });
    g.bench_function("std", |b| {
        b.iter(|| {
            let (tx, rx) = std::sync::mpsc::sync_channel::<u32>(CAP);
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    while tx.try_send(i as u32).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let cons = std::thread::spawn(move || {
                let mut n = 0u64;
                while n < ITEMS {
                    if let Ok(v) = rx.try_recv() {
                        black_box(v);
                        n += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });
    g.bench_function("tokio", |b| {
        b.iter(|| {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(CAP);
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    while tx.try_send(i as u32).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let cons = std::thread::spawn(move || {
                let mut n = 0u64;
                while n < ITEMS {
                    if let Ok(v) = rx.try_recv() {
                        black_box(v);
                        n += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });

    g.finish();
}

// ============================================================================
// Throughput (threaded, BLOCKING): park/unpark mode (CPU-friendly).
// cynosure block_on vs the channels' native blocking send/recv.
// ============================================================================

fn bench_throughput_threaded_blocking(c: &mut Criterion) {
    let mut g = c.benchmark_group("SPSC Throughput (2 threads, blocking)");
    g.throughput(Throughput::Elements(ITEMS));

    g.bench_function("cynosure", |b| {
        b.iter(|| {
            let (mut p, mut c) = RingBuf::<u32>::new(CAP).split();
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    p.push_blocking(i as u32);
                }
            });
            let cons = std::thread::spawn(move || {
                for _ in 0..ITEMS {
                    black_box(c.pop_blocking());
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });
    g.bench_function("crossbeam", |b| {
        b.iter(|| {
            let (tx, rx) = crossbeam_channel::bounded::<u32>(CAP);
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    tx.send(i as u32).unwrap();
                }
            });
            let cons = std::thread::spawn(move || {
                for _ in 0..ITEMS {
                    black_box(rx.recv().unwrap());
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });
    g.bench_function("kanal", |b| {
        b.iter(|| {
            let (tx, rx) = kanal::bounded::<u32>(CAP);
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    tx.send(i as u32).unwrap();
                }
            });
            let cons = std::thread::spawn(move || {
                for _ in 0..ITEMS {
                    black_box(rx.recv().unwrap());
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });
    g.bench_function("flume", |b| {
        b.iter(|| {
            let (tx, rx) = flume::bounded::<u32>(CAP);
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    tx.send(i as u32).unwrap();
                }
            });
            let cons = std::thread::spawn(move || {
                for _ in 0..ITEMS {
                    black_box(rx.recv().unwrap());
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });
    g.bench_function("std", |b| {
        b.iter(|| {
            let (tx, rx) = std::sync::mpsc::sync_channel::<u32>(CAP);
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    tx.send(i as u32).unwrap();
                }
            });
            let cons = std::thread::spawn(move || {
                for _ in 0..ITEMS {
                    black_box(rx.recv().unwrap());
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });
    g.bench_function("tokio", |b| {
        b.iter(|| {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(CAP);
            let prod = std::thread::spawn(move || {
                for i in 0..ITEMS {
                    tx.blocking_send(i as u32).unwrap();
                }
            });
            let cons = std::thread::spawn(move || {
                for _ in 0..ITEMS {
                    black_box(rx.blocking_recv().unwrap());
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });

    g.finish();
}

// ============================================================================
// Threaded large payload (512B) — exercises the cross-core cache-line handoff,
// where memory-latency hiding (e.g. prefetch) could matter.
// ============================================================================

#[derive(Clone, Copy)]
#[allow(dead_code)] // payload bytes are moved through the queue, never read
struct Big([u8; 512]);
impl Default for Big {
    fn default() -> Self {
        Self([0u8; 512])
    }
}

const ITEMS_LARGE: u64 = 200_000;

fn bench_threaded_large(c: &mut Criterion) {
    let mut g = c.benchmark_group("SPSC Throughput (2 threads, 512B)");
    g.throughput(Throughput::Elements(ITEMS_LARGE));

    g.bench_function("cynosure", |b| {
        b.iter(|| {
            let (mut p, mut c) = RingBuf::<Big>::new(CAP).split();
            let prod = std::thread::spawn(move || {
                for _ in 0..ITEMS_LARGE {
                    while p.try_push(black_box(Big::default())).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let cons = std::thread::spawn(move || {
                let mut n = 0u64;
                while n < ITEMS_LARGE {
                    if let Some(v) = c.try_pop() {
                        black_box(v);
                        n += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });
    g.bench_function("rtrb", |b| {
        b.iter(|| {
            let (mut p, mut c) = rtrb::RingBuffer::<Big>::new(CAP);
            let prod = std::thread::spawn(move || {
                for _ in 0..ITEMS_LARGE {
                    while p.push(black_box(Big::default())).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let cons = std::thread::spawn(move || {
                let mut n = 0u64;
                while n < ITEMS_LARGE {
                    if let Ok(v) = c.pop() {
                        black_box(v);
                        n += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            prod.join().unwrap();
            cons.join().unwrap();
        })
    });

    g.finish();
}

// ============================================================================
// Async throughput: producer + consumer as two tasks on ONE single-threaded
// executor (the thread-per-core / monoio model). Every contender is driven by
// the same runtime-agnostic `LocalPool`, so it's a fair async comparison —
// this is cynosure's actual design target (its API is async-first), unlike the
// sync/blocking groups above where crossbeam et al. are in their element.
// ============================================================================

fn bench_async(c: &mut Criterion) {
    use futures::{executor::LocalPool, task::LocalSpawnExt};

    let mut g = c.benchmark_group("SPSC Async Throughput (1 thread, 2 tasks)");
    g.throughput(Throughput::Elements(ITEMS));

    g.bench_function("cynosure", |b| {
        b.iter(|| {
            let mut pool = LocalPool::new();
            let (mut p, mut c) = RingBuf::<u32>::new(CAP).split();
            pool.spawner()
                .spawn_local(async move {
                    for i in 0..ITEMS {
                        p.push(i as u32).await;
                    }
                })
                .unwrap();
            pool.run_until(async move {
                let mut acc = 0u64;
                for _ in 0..ITEMS {
                    acc = acc.wrapping_add(c.pop().await as u64);
                }
                black_box(acc);
            });
        })
    });
    g.bench_function("tokio-mpsc", |b| {
        b.iter(|| {
            let mut pool = LocalPool::new();
            let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(CAP);
            pool.spawner()
                .spawn_local(async move {
                    for i in 0..ITEMS {
                        tx.send(i as u32).await.unwrap();
                    }
                })
                .unwrap();
            pool.run_until(async move {
                let mut acc = 0u64;
                for _ in 0..ITEMS {
                    acc = acc.wrapping_add(rx.recv().await.unwrap() as u64);
                }
                black_box(acc);
            });
        })
    });
    g.bench_function("flume", |b| {
        b.iter(|| {
            let mut pool = LocalPool::new();
            let (tx, rx) = flume::bounded::<u32>(CAP);
            pool.spawner()
                .spawn_local(async move {
                    for i in 0..ITEMS {
                        tx.send_async(i as u32).await.unwrap();
                    }
                })
                .unwrap();
            pool.run_until(async move {
                let mut acc = 0u64;
                for _ in 0..ITEMS {
                    acc = acc.wrapping_add(rx.recv_async().await.unwrap() as u64);
                }
                black_box(acc);
            });
        })
    });
    g.bench_function("kanal", |b| {
        b.iter(|| {
            let mut pool = LocalPool::new();
            let (tx, rx) = kanal::bounded_async::<u32>(CAP);
            pool.spawner()
                .spawn_local(async move {
                    for i in 0..ITEMS {
                        tx.send(i as u32).await.unwrap();
                    }
                })
                .unwrap();
            pool.run_until(async move {
                let mut acc = 0u64;
                for _ in 0..ITEMS {
                    acc = acc.wrapping_add(rx.recv().await.unwrap() as u64);
                }
                black_box(acc);
            });
        })
    });
    g.bench_function("thingbuf", |b| {
        b.iter(|| {
            let mut pool = LocalPool::new();
            let (tx, rx) = thingbuf::mpsc::channel::<u32>(CAP);
            pool.spawner()
                .spawn_local(async move {
                    for i in 0..ITEMS {
                        tx.send(i as u32).await.unwrap();
                    }
                })
                .unwrap();
            pool.run_until(async move {
                let mut acc = 0u64;
                for _ in 0..ITEMS {
                    acc = acc.wrapping_add(rx.recv().await.unwrap() as u64);
                }
                black_box(acc);
            });
        })
    });

    g.finish();
}

/// Cross-thread async: producer and consumer each run on their OWN thread and
/// executor, communicating purely through the async API (`push().await` /
/// `pop().await`) with real cross-thread wakeups. This is the headline
/// thread-per-core use case and the path the F1/F2 wakeup fixes protect. Every
/// contender is driven by `futures::executor::block_on` (parks on Pending).
fn bench_async_2thread(c: &mut Criterion) {
    let mut g = c.benchmark_group("SPSC Async Throughput (2 threads)");
    g.throughput(Throughput::Elements(ITEMS));

    g.bench_function("cynosure", |b| {
        b.iter(|| {
            let (mut p, mut cons) = RingBuf::<u32>::new(CAP).split();
            let pt = std::thread::spawn(move || {
                futures::executor::block_on(async move {
                    for i in 0..ITEMS {
                        p.push(i as u32).await;
                    }
                })
            });
            let ct = std::thread::spawn(move || {
                futures::executor::block_on(async move {
                    let mut acc = 0u64;
                    for _ in 0..ITEMS {
                        acc = acc.wrapping_add(cons.pop().await as u64);
                    }
                    black_box(acc);
                })
            });
            pt.join().unwrap();
            ct.join().unwrap();
        })
    });

    g.bench_function("tokio-mpsc", |b| {
        b.iter(|| {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(CAP);
            let pt = std::thread::spawn(move || {
                futures::executor::block_on(async move {
                    for i in 0..ITEMS {
                        tx.send(i as u32).await.unwrap();
                    }
                })
            });
            let ct = std::thread::spawn(move || {
                futures::executor::block_on(async move {
                    let mut acc = 0u64;
                    for _ in 0..ITEMS {
                        acc = acc.wrapping_add(rx.recv().await.unwrap() as u64);
                    }
                    black_box(acc);
                })
            });
            pt.join().unwrap();
            ct.join().unwrap();
        })
    });

    g.bench_function("flume", |b| {
        b.iter(|| {
            let (tx, rx) = flume::bounded::<u32>(CAP);
            let pt = std::thread::spawn(move || {
                futures::executor::block_on(async move {
                    for i in 0..ITEMS {
                        tx.send_async(i as u32).await.unwrap();
                    }
                })
            });
            let ct = std::thread::spawn(move || {
                futures::executor::block_on(async move {
                    let mut acc = 0u64;
                    for _ in 0..ITEMS {
                        acc = acc.wrapping_add(rx.recv_async().await.unwrap() as u64);
                    }
                    black_box(acc);
                })
            });
            pt.join().unwrap();
            ct.join().unwrap();
        })
    });

    g.bench_function("kanal", |b| {
        b.iter(|| {
            let (tx, rx) = kanal::bounded_async::<u32>(CAP);
            let pt = std::thread::spawn(move || {
                futures::executor::block_on(async move {
                    for i in 0..ITEMS {
                        tx.send(i as u32).await.unwrap();
                    }
                })
            });
            let ct = std::thread::spawn(move || {
                futures::executor::block_on(async move {
                    let mut acc = 0u64;
                    for _ in 0..ITEMS {
                        acc = acc.wrapping_add(rx.recv().await.unwrap() as u64);
                    }
                    black_box(acc);
                })
            });
            pt.join().unwrap();
            ct.join().unwrap();
        })
    });

    g.finish();
}

/// Cross-thread async round-trip latency: a strict ping-pong over two ring
/// buffers. Every message parks one side and is woken by the other across
/// cores, so this measures the cross-thread async wakeup round-trip directly
/// (and stress-tests the F1/F2 path — millions of cross-thread wakeups).
fn bench_async_2thread_latency(c: &mut Criterion) {
    use std::time::Instant;
    let mut g = c.benchmark_group("SPSC Async Round-trip Latency (2 threads)");

    g.bench_function("cynosure", |b| {
        b.iter_custom(|iters| {
            let (mut req_tx, mut req_rx) = RingBuf::<u32>::new(CAP).split();
            let (mut resp_tx, mut resp_rx) = RingBuf::<u32>::new(CAP).split();
            let server = std::thread::spawn(move || {
                futures::executor::block_on(async move {
                    for _ in 0..iters {
                        let v = req_rx.pop().await;
                        resp_tx.push(v).await;
                    }
                })
            });
            let start = Instant::now();
            futures::executor::block_on(async move {
                for i in 0..iters {
                    req_tx.push(i as u32).await;
                    black_box(resp_rx.pop().await);
                }
            });
            let elapsed = start.elapsed();
            server.join().unwrap();
            elapsed
        })
    });

    g.bench_function("tokio-mpsc", |b| {
        b.iter_custom(|iters| {
            let (req_tx, mut req_rx) = tokio::sync::mpsc::channel::<u32>(CAP);
            let (resp_tx, mut resp_rx) = tokio::sync::mpsc::channel::<u32>(CAP);
            let server = std::thread::spawn(move || {
                futures::executor::block_on(async move {
                    for _ in 0..iters {
                        let v = req_rx.recv().await.unwrap();
                        resp_tx.send(v).await.unwrap();
                    }
                })
            });
            let start = Instant::now();
            futures::executor::block_on(async move {
                for i in 0..iters {
                    req_tx.send(i as u32).await.unwrap();
                    black_box(resp_rx.recv().await.unwrap());
                }
            });
            let elapsed = start.elapsed();
            server.join().unwrap();
            elapsed
        })
    });

    g.finish();
}

criterion_group!(
    benches,
    bench_latency,
    bench_throughput_st,
    bench_throughput_threaded,
    bench_throughput_threaded_blocking,
    bench_threaded_large,
    bench_async_2thread,
    bench_async_2thread_latency,
    bench_async
);
criterion_main!(benches);
