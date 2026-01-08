//! SPSC benchmark: cynosure RingBuf vs std::sync::mpsc vs crossbeam-channel
//!
//! Measures latency (ns/op) and throughput (items/sec) for various type sizes.

use std::sync::mpsc;

use criterion::{
    BatchSize, BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main,
};
use crossbeam_channel as crossbeam;
use cynosure::site_d::ringbuf::RingBuf;

#[derive(Clone, Copy, Debug)]
#[repr(C, align(64))]
struct Bytes64([u8; 64]);

impl Default for Bytes64 {
    fn default() -> Self {
        Self([0u8; 64])
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C, align(64))]
struct Bytes512([u8; 512]);

impl Default for Bytes512 {
    fn default() -> Self {
        Self([0u8; 512])
    }
}

const BUFFER_SIZE: usize = 1024;
const ITEMS: u64 = 100_000;

// ============================================================================
// Latency: single push+pop
// ============================================================================

fn bench_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("Latency");

    macro_rules! bench_type {
        ($name:literal, $ty:ty, $val:expr) => {
            group.bench_function(concat!("RingBuf<", $name, ">"), |b| {
                let rb = RingBuf::<$ty>::new(BUFFER_SIZE);
                let (mut p, mut c) = rb.split();
                b.iter(|| {
                    p.try_push(black_box($val)).unwrap();
                    black_box(c.try_pop().unwrap())
                })
            });

            group.bench_function(concat!("mpsc<", $name, ">"), |b| {
                let (tx, rx) = mpsc::sync_channel::<$ty>(BUFFER_SIZE);
                b.iter(|| {
                    tx.try_send(black_box($val)).unwrap();
                    black_box(rx.try_recv().unwrap())
                })
            });

            group.bench_function(concat!("crossbeam<", $name, ">"), |b| {
                let (tx, rx) = crossbeam::bounded::<$ty>(BUFFER_SIZE);
                b.iter(|| {
                    tx.try_send(black_box($val)).unwrap();
                    black_box(rx.try_recv().unwrap())
                })
            });
        };
    }

    bench_type!("u8", u8, 42u8);
    bench_type!("u32", u32, 42u32);
    bench_type!("u128", u128, 42u128);
    bench_type!("64B", Bytes64, Bytes64::default());
    bench_type!("512B", Bytes512, Bytes512::default());

    group.finish();
}

// ============================================================================
// Throughput: single-thread interleaved push/pop
// ============================================================================

fn bench_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("Throughput");
    group.throughput(Throughput::Elements(ITEMS));

    macro_rules! bench_type {
        ($name:literal, $ty:ty, $val_fn:expr) => {
            group.bench_function(concat!("RingBuf<", $name, ">"), |b| {
                b.iter_batched(
                    || {
                        let rb = RingBuf::<$ty>::new(BUFFER_SIZE);
                        rb.split()
                    },
                    |(mut p, mut c)| {
                        for i in 0..ITEMS {
                            while p.try_push($val_fn(i)).is_err() {
                                black_box(c.try_pop());
                            }
                        }
                        while c.try_pop().is_some() {}
                    },
                    BatchSize::SmallInput,
                )
            });

            group.bench_function(concat!("mpsc<", $name, ">"), |b| {
                b.iter_batched(
                    || mpsc::sync_channel::<$ty>(BUFFER_SIZE),
                    |(tx, rx)| {
                        for i in 0..ITEMS {
                            while tx.try_send($val_fn(i)).is_err() {
                                black_box(rx.try_recv().ok());
                            }
                        }
                        while rx.try_recv().is_ok() {}
                    },
                    BatchSize::SmallInput,
                )
            });

            group.bench_function(concat!("crossbeam<", $name, ">"), |b| {
                b.iter_batched(
                    || crossbeam::bounded::<$ty>(BUFFER_SIZE),
                    |(tx, rx)| {
                        for i in 0..ITEMS {
                            while tx.try_send($val_fn(i)).is_err() {
                                black_box(rx.try_recv().ok());
                            }
                        }
                        while rx.try_recv().is_ok() {}
                    },
                    BatchSize::SmallInput,
                )
            });
        };
    }

    bench_type!("u8", u8, |i: u64| (i & 0xFF) as u8);
    bench_type!("u32", u32, |i: u64| i as u32);
    bench_type!("u128", u128, |i: u64| i as u128);
    bench_type!("64B", Bytes64, |_| Bytes64::default());
    bench_type!("512B", Bytes512, |_| Bytes512::default());

    group.finish();
}

// ============================================================================
// Async throughput: monoio, sequential fill/drain (no task spawn in hot path)
// ============================================================================

fn bench_async_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("Throughput Async");
    group.throughput(Throughput::Elements(ITEMS));

    macro_rules! bench_type {
        ($name:literal, $ty:ty, $val_fn:expr) => {
            group.bench_function(concat!("RingBuf<", $name, ">"), |b| {
                let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
                    .build()
                    .unwrap();

                b.iter_custom(|iters| {
                    let start = std::time::Instant::now();
                    for _ in 0..iters {
                        rt.block_on(async {
                            let rb = RingBuf::<$ty>::new(BUFFER_SIZE);
                            let (mut p, mut c) = rb.split();

                            // Sequential: push batch, pop batch, repeat
                            // This measures async fast-path (no actual waiting)
                            let mut pushed = 0u64;
                            let mut popped = 0u64;

                            while popped < ITEMS {
                                // Push until full
                                while pushed < ITEMS {
                                    if p.try_push($val_fn(pushed)).is_ok() {
                                        pushed += 1;
                                    } else {
                                        break;
                                    }
                                }

                                // Pop until empty
                                while let Some(v) = c.try_pop() {
                                    black_box(v);
                                    popped += 1;
                                }
                            }
                        });
                    }
                    start.elapsed()
                })
            });
        };
    }

    bench_type!("u8", u8, |i: u64| (i & 0xFF) as u8);
    bench_type!("u32", u32, |i: u64| i as u32);
    bench_type!("u128", u128, |i: u64| i as u128);
    bench_type!("64B", Bytes64, |_| Bytes64::default());
    bench_type!("512B", Bytes512, |_| Bytes512::default());

    group.finish();
}

// ============================================================================
// Async with actual awaiting (producer/consumer interleave via yield)
// ============================================================================

fn bench_async_with_await(c: &mut Criterion) {
    let mut group = c.benchmark_group("Throughput Async Await");
    group.throughput(Throughput::Elements(ITEMS));

    macro_rules! bench_type {
        ($name:literal, $ty:ty, $val_fn:expr) => {
            group.bench_function(concat!("RingBuf<", $name, ">"), |b| {
                let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
                    .build()
                    .unwrap();

                b.iter_custom(|iters| {
                    let start = std::time::Instant::now();
                    for _ in 0..iters {
                        rt.block_on(async {
                            let rb = RingBuf::<$ty>::new(BUFFER_SIZE);
                            let (mut p, mut c) = rb.split();

                            // Spawn producer task once, outside timing would be better
                            // but we need both ends. This measures realistic async usage.
                            let producer = monoio::spawn(async move {
                                for i in 0..ITEMS {
                                    p.push($val_fn(i)).await;
                                }
                            });

                            // Consumer runs on main task
                            for _ in 0..ITEMS {
                                black_box(c.pop().await);
                            }

                            producer.await;
                        });
                    }
                    start.elapsed()
                })
            });
        };
    }

    bench_type!("u8", u8, |i: u64| (i & 0xFF) as u8);
    bench_type!("u32", u32, |i: u64| i as u32);
    bench_type!("u128", u128, |i: u64| i as u128);
    bench_type!("64B", Bytes64, |_| Bytes64::default());
    bench_type!("512B", Bytes512, |_| Bytes512::default());

    group.finish();
}

// ============================================================================
// Bulk slice operations
// ============================================================================

fn bench_bulk_slice(c: &mut Criterion) {
    let mut group = c.benchmark_group("Bulk Slice");
    group.throughput(Throughput::Elements(ITEMS));

    let data: Vec<u8> = (0..ITEMS as usize).map(|i| (i & 0xFF) as u8).collect();

    for chunk_size in [64, 256, 512] {
        group.bench_with_input(
            BenchmarkId::new("RingBuf slice", chunk_size),
            &chunk_size,
            |b, &chunk_size| {
                b.iter_batched(
                    || {
                        let rb = RingBuf::<u8>::new(BUFFER_SIZE);
                        rb.split()
                    },
                    |(mut p, mut c)| {
                        let mut buf = vec![0u8; chunk_size];
                        let mut sent = 0usize;
                        let mut recvd = 0usize;
                        let total = ITEMS as usize;

                        while recvd < total {
                            if sent < total {
                                let n =
                                    p.try_push_slice(&data[sent..(sent + chunk_size).min(total)]);
                                sent += n;
                            }
                            let n = c.try_pop_slice(&mut buf[..chunk_size.min(total - recvd)]);
                            if n > 0 {
                                black_box(&buf[..n]);
                                recvd += n;
                            }
                        }
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }

    // Baseline: individual ops
    group.bench_function("RingBuf individual", |b| {
        b.iter_batched(
            || {
                let rb = RingBuf::<u8>::new(BUFFER_SIZE);
                rb.split()
            },
            |(mut p, mut c)| {
                for i in 0..ITEMS {
                    while p.try_push((i & 0xFF) as u8).is_err() {
                        black_box(c.try_pop());
                    }
                }
                while c.try_pop().is_some() {}
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

// ============================================================================
// Buffer size impact
// ============================================================================

fn bench_buffer_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("Buffer Size");
    group.throughput(Throughput::Elements(ITEMS));

    for buf_size in [64, 256, 1024, 4096] {
        group.bench_with_input(
            BenchmarkId::new("RingBuf<u32>", buf_size),
            &buf_size,
            |b, &buf_size| {
                b.iter_batched(
                    || {
                        let rb = RingBuf::<u32>::new(buf_size);
                        rb.split()
                    },
                    |(mut p, mut c)| {
                        for i in 0..ITEMS {
                            while p.try_push(i as u32).is_err() {
                                black_box(c.try_pop());
                            }
                        }
                        while c.try_pop().is_some() {}
                    },
                    BatchSize::SmallInput,
                )
            },
        );

        group.bench_with_input(
            BenchmarkId::new("mpsc<u32>", buf_size),
            &buf_size,
            |b, &buf_size| {
                b.iter_batched(
                    || mpsc::sync_channel::<u32>(buf_size),
                    |(tx, rx)| {
                        for i in 0..ITEMS {
                            while tx.try_send(i as u32).is_err() {
                                black_box(rx.try_recv().ok());
                            }
                        }
                        while rx.try_recv().is_ok() {}
                    },
                    BatchSize::SmallInput,
                )
            },
        );

        group.bench_with_input(
            BenchmarkId::new("crossbeam<u32>", buf_size),
            &buf_size,
            |b, &buf_size| {
                b.iter_batched(
                    || crossbeam::bounded::<u32>(buf_size),
                    |(tx, rx)| {
                        for i in 0..ITEMS {
                            while tx.try_send(i as u32).is_err() {
                                black_box(rx.try_recv().ok());
                            }
                        }
                        while rx.try_recv().is_ok() {}
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }

    group.finish();
}

// ============================================================================
// Threaded mpsc baseline (true parallel producer/consumer)
// ============================================================================

fn bench_mpsc_threaded(c: &mut Criterion) {
    let mut group = c.benchmark_group("mpsc Threaded");
    group.throughput(Throughput::Elements(ITEMS));

    macro_rules! bench_type {
        ($name:literal, $ty:ty, $val_fn:expr) => {
            group.bench_function(concat!("mpsc<", $name, ">"), |b| {
                b.iter(|| {
                    let (tx, rx) = mpsc::sync_channel::<$ty>(BUFFER_SIZE);

                    let producer = std::thread::spawn(move || {
                        for i in 0..ITEMS {
                            tx.send($val_fn(i)).unwrap();
                        }
                    });

                    let consumer = std::thread::spawn(move || {
                        for _ in 0..ITEMS {
                            black_box(rx.recv().unwrap());
                        }
                    });

                    producer.join().unwrap();
                    consumer.join().unwrap();
                })
            });

            group.bench_function(concat!("crossbeam<", $name, ">"), |b| {
                b.iter(|| {
                    let (tx, rx) = crossbeam::bounded::<$ty>(BUFFER_SIZE);

                    let producer = std::thread::spawn(move || {
                        for i in 0..ITEMS {
                            tx.send($val_fn(i)).unwrap();
                        }
                    });

                    let consumer = std::thread::spawn(move || {
                        for _ in 0..ITEMS {
                            black_box(rx.recv().unwrap());
                        }
                    });

                    producer.join().unwrap();
                    consumer.join().unwrap();
                })
            });
        };
    }

    bench_type!("u8", u8, |i: u64| (i & 0xFF) as u8);
    bench_type!("u32", u32, |i: u64| i as u32);
    bench_type!("u128", u128, |i: u64| i as u128);
    bench_type!("64B", Bytes64, |_| Bytes64::default());
    bench_type!("512B", Bytes512, |_| Bytes512::default());

    group.finish();
}

criterion_group!(
    benches,
    bench_latency,
    bench_throughput,
    bench_async_throughput,
    bench_async_with_await,
    bench_bulk_slice,
    bench_buffer_sizes,
    bench_mpsc_threaded,
);
criterion_main!(benches);
