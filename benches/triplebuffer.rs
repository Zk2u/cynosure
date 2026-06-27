//! Triple buffer, 2 threads: a writer fills whole buffers and publishes them, a
//! reader consumes them. Measures end-to-end streamed bandwidth (and thus the
//! coordination overhead, which dominates at small buffer sizes).
//!
//! Baseline: the realistic zero-alloc alternative — two `crossbeam` channels (a
//! "full" channel writer->reader and an "empty" return channel reader->writer)
//! recycling a fixed pool of 3 owned buffers, mirroring the triple buffer's
//! three buffers and depth-1 backpressure.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    thread,
};

use criterion::{
    BatchSize, BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main,
};
use cynosure::site_d::triplebuffer::{AlignedBuffer, triple_buffer};
use futures::executor::block_on;

/// Drive a future to completion by busy-spinning (no thread parking) — the
/// lock-free coordination ceiling, comparable to `try_*` spin loops.
fn spin_on<F: Future>(mut fut: F) -> F::Output {
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    // SAFETY: `fut` is not moved after pinning.
    let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::hint::spin_loop(),
        }
    }
}

// Total application-level bytes streamed per measured iteration (the number of
// handoffs is this divided by the buffer size, capped so tiny buffers don't
// explode the handoff count).
const TARGET_BYTES: u64 = 256 * 1024 * 1024;
const MAX_HANDOFFS: u64 = 50_000;

fn handoffs_for(size: usize) -> u64 {
    (TARGET_BYTES / size as u64).clamp(1, MAX_HANDOFFS)
}

const SIZES: [usize; 3] = [4 * 1024, 64 * 1024, 4 * 1024 * 1024];

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("TripleBuffer 2 threads");

    for &size in &SIZES {
        let n = handoffs_for(size);
        g.throughput(Throughput::Bytes(n * size as u64));

        // ---- cynosure triple buffer ----
        g.bench_with_input(BenchmarkId::new("cynosure", size), &size, |b, &size| {
            b.iter(|| {
                let (mut w, mut r, mut wbuf) = triple_buffer::<u8>(size);

                let prod = thread::spawn(move || {
                    for i in 0..n {
                        wbuf.capacity_mut().fill((i & 0xff) as u8);
                        wbuf.set_len(size);
                        wbuf = block_on(w.publish(wbuf));
                    }
                });
                let cons = thread::spawn(move || {
                    let mut prev = None;
                    let mut acc = 0u64;
                    for _ in 0..n {
                        let buf = block_on(r.next(prev.take()));
                        acc = acc.wrapping_add(buf.iter().map(|&x| x as u64).sum::<u64>());
                        prev = Some(buf);
                    }
                    black_box(acc);
                });
                prod.join().unwrap();
                cons.join().unwrap();
            })
        });

        // ---- crossbeam recycle baseline (3 buffers, depth-1 full channel) ----
        g.bench_with_input(
            BenchmarkId::new("crossbeam-recycle", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || {
                        let (full_tx, full_rx) = crossbeam_channel::bounded::<Vec<u8>>(1);
                        let (empty_tx, empty_rx) = crossbeam_channel::bounded::<Vec<u8>>(3);
                        for _ in 0..3 {
                            empty_tx.send(vec![0u8; size]).unwrap();
                        }
                        (full_tx, full_rx, empty_tx, empty_rx)
                    },
                    |(full_tx, full_rx, empty_tx, empty_rx)| {
                        let prod = thread::spawn(move || {
                            for i in 0..n {
                                let mut buf = empty_rx.recv().unwrap();
                                buf.fill((i & 0xff) as u8);
                                full_tx.send(buf).unwrap();
                            }
                        });
                        let cons = thread::spawn(move || {
                            let mut acc = 0u64;
                            for _ in 0..n {
                                let buf = full_rx.recv().unwrap();
                                acc = acc.wrapping_add(buf.iter().map(|&x| x as u64).sum::<u64>());
                                let _ = empty_tx.send(buf); // recycle
                            }
                            black_box(acc);
                        });
                        prod.join().unwrap();
                        cons.join().unwrap();
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }

    g.finish();
}

/// Same workload, but both sides busy-spin instead of parking — shows the
/// lock-free coordination ceiling (no syscalls).
fn bench_spin(c: &mut Criterion) {
    let mut g = c.benchmark_group("TripleBuffer 2 threads (spin)");

    for &size in &SIZES {
        let n = handoffs_for(size);
        g.throughput(Throughput::Bytes(n * size as u64));

        g.bench_with_input(BenchmarkId::new("cynosure", size), &size, |b, &size| {
            b.iter(|| {
                let (mut w, mut r, mut wbuf) = triple_buffer::<u8>(size);
                let prod = thread::spawn(move || {
                    for i in 0..n {
                        wbuf.capacity_mut().fill((i & 0xff) as u8);
                        wbuf.set_len(size);
                        wbuf = spin_on(w.publish(wbuf));
                    }
                });
                let cons = thread::spawn(move || {
                    let mut prev = None;
                    let mut acc = 0u64;
                    for _ in 0..n {
                        let buf = spin_on(r.next(prev.take()));
                        acc = acc.wrapping_add(buf.iter().map(|&x| x as u64).sum::<u64>());
                        prev = Some(buf);
                    }
                    black_box(acc);
                });
                prod.join().unwrap();
                cons.join().unwrap();
            })
        });

        g.bench_with_input(
            BenchmarkId::new("crossbeam-recycle", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || {
                        let (full_tx, full_rx) = crossbeam_channel::bounded::<Vec<u8>>(1);
                        let (empty_tx, empty_rx) = crossbeam_channel::bounded::<Vec<u8>>(3);
                        for _ in 0..3 {
                            empty_tx.send(vec![0u8; size]).unwrap();
                        }
                        (full_tx, full_rx, empty_tx, empty_rx)
                    },
                    |(full_tx, full_rx, empty_tx, empty_rx)| {
                        let prod = thread::spawn(move || {
                            for i in 0..n {
                                let mut buf = loop {
                                    if let Ok(b) = empty_rx.try_recv() {
                                        break b;
                                    }
                                    std::hint::spin_loop();
                                };
                                buf.fill((i & 0xff) as u8);
                                loop {
                                    match full_tx.try_send(buf) {
                                        Ok(()) => break,
                                        Err(crossbeam_channel::TrySendError::Full(b)) => {
                                            buf = b;
                                            std::hint::spin_loop();
                                        }
                                        Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                                            return;
                                        }
                                    }
                                }
                            }
                        });
                        let cons = thread::spawn(move || {
                            let mut acc = 0u64;
                            for _ in 0..n {
                                let buf = loop {
                                    if let Ok(b) = full_rx.try_recv() {
                                        break b;
                                    }
                                    std::hint::spin_loop();
                                };
                                acc = acc.wrapping_add(buf.iter().map(|&x| x as u64).sum::<u64>());
                                let _ = empty_tx.try_send(buf);
                            }
                            black_box(acc);
                        });
                        prod.join().unwrap();
                        cons.join().unwrap();
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }

    g.finish();
}

/// Latency of the core rotation, isolated. Single thread, fast path: the buffer
/// is never filled or read, so this measures the rotation cost (pointer swaps +
/// atomics + a no-op wake + the cost of polling a ready future) with no memory
/// bandwidth or thread parking. Buffer size is irrelevant to the rotation.
fn bench_rotation_latency(c: &mut Criterion) {
    let mut g = c.benchmark_group("TripleBuffer rotation latency");

    // publish (1 rotation) + next (1 rotation) per iteration; the two alternate
    // so each stays on its ready fast path (publish sees middle_free, next sees
    // has_unread).
    {
        let (mut w, mut r, wbuf0) = triple_buffer::<u8>(4096);
        let mut wbuf = Some(wbuf0);
        let mut prev: Option<AlignedBuffer<u8>> = None;
        // NB: this includes the cost of polling the async futures (the triple
        // buffer has no sync API); crossbeam below uses its direct sync calls.
        g.bench_function("cynosure publish+next cycle", |b| {
            b.iter(|| {
                let buf = wbuf.take().unwrap();
                wbuf = Some(spin_on(w.publish(buf)));
                prev = Some(spin_on(r.next(prev.take())));
            });
        });
    }

    // crossbeam-recycle equivalent: one owned buffer handed writer->reader and
    // recycled back, all on the fast path (channels never actually block here).
    // 3 buffers, full channel depth 1 — mirrors the triple buffer.
    {
        let (full_tx, full_rx) = crossbeam_channel::bounded::<Vec<u8>>(1);
        let (empty_tx, empty_rx) = crossbeam_channel::bounded::<Vec<u8>>(3);
        for _ in 0..3 {
            empty_tx.send(vec![0u8; 4096]).unwrap();
        }
        g.bench_function("crossbeam-recycle send+recv cycle", |b| {
            b.iter(|| {
                // "publish": take an empty buffer, send it full.
                let buf = empty_rx.recv().unwrap();
                full_tx.send(buf).unwrap();
                // "next": take the full buffer, recycle it empty.
                let buf = full_rx.recv().unwrap();
                empty_tx.send(buf).unwrap();
            });
        });
    }

    g.finish();
}

criterion_group!(benches, bench, bench_spin, bench_rotation_latency);
criterion_main!(benches);
