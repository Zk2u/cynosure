//! `mpsc_light` (single-queue control-plane MPSC): async fan-in throughput and
//! parked ping-pong wakeup latency, vs kanal (the fastest general channel).
//!
//! Both sides run on the same minimal thread-park executor so the *channel* is
//! measured, not the runtime (`futures::block_on`'s allocating waker skews
//! low-N numbers; a real runtime's waker clone is a refcount bump, like this).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use cynosure::site_d::mpsc_light;

const PER: u64 = 100_000; // items per producer
const CAP: usize = 256;
const FANIN: &[usize] = &[1, 4, 16];

/// Minimal thread-park executor with a non-allocating (Arc-refcount) waker —
/// the wake cost profile of a real runtime.
struct Unpark(Thread);
impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}
fn park_on<F: Future>(mut f: F) -> F::Output {
    // SAFETY: `f` is not moved after pinning.
    let mut f = unsafe { Pin::new_unchecked(&mut f) };
    let waker: Waker = Arc::new(Unpark(thread::current())).into();
    let mut cx = Context::from_waker(&waker);
    loop {
        match f.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => thread::park(),
        }
    }
}

// ============================================================================
// Async fan-in: N producer threads -> 1 consumer thread
// ============================================================================

fn bench_async_fanin(c: &mut Criterion) {
    let mut g = c.benchmark_group("mpsc_light fan-in (async)");
    for &n in FANIN {
        let total = PER * n as u64;
        g.throughput(Throughput::Elements(total));

        g.bench_with_input(BenchmarkId::new("cynosure light", n), &n, |b, &n| {
            b.iter(|| {
                let (tx, mut rx) = mpsc_light::bounded::<u64>(CAP);
                let ps: Vec<_> = (0..n)
                    .map(|_| {
                        let tx = tx.clone();
                        thread::spawn(move || {
                            park_on(async move {
                                for i in 0..PER {
                                    tx.send(i).await.unwrap();
                                }
                            })
                        })
                    })
                    .collect();
                drop(tx);
                let cons = thread::spawn(move || {
                    park_on(async move {
                        let mut got = 0u64;
                        while got < total {
                            // dedicated consumer thread: recv_hot is the right mode
                            if let Some(v) = rx.recv_hot().await {
                                black_box(v);
                                got += 1;
                            }
                        }
                    })
                });
                for p in ps {
                    p.join().unwrap();
                }
                cons.join().unwrap();
            })
        });

        g.bench_with_input(BenchmarkId::new("kanal", n), &n, |b, &n| {
            b.iter(|| {
                let (tx, rx) = kanal::bounded_async::<u64>(CAP);
                let ps: Vec<_> = (0..n)
                    .map(|_| {
                        let tx = tx.clone();
                        thread::spawn(move || {
                            park_on(async move {
                                for i in 0..PER {
                                    tx.send(i).await.unwrap();
                                }
                            })
                        })
                    })
                    .collect();
                drop(tx);
                let cons = thread::spawn(move || {
                    park_on(async move {
                        let mut got = 0u64;
                        while got < total {
                            if rx.recv().await.is_ok() {
                                got += 1;
                            }
                        }
                    })
                });
                for p in ps {
                    p.join().unwrap();
                }
                cons.join().unwrap();
            })
        });
    }
    g.finish();
}

// ============================================================================
// Wakeup latency: strict ping-pong over two channels (each hop parks the
// other side; measures the park -> wake round trip, the control-plane metric)
// ============================================================================

fn bench_latency(c: &mut Criterion) {
    const ROUND: u64 = 5_000;
    let mut g = c.benchmark_group("mpsc_light ping-pong latency");
    g.throughput(Throughput::Elements(ROUND * 2)); // one-way hops per iter

    g.bench_function("cynosure light (recv_hot)", |b| {
        b.iter(|| {
            let (t1, mut r1) = mpsc_light::bounded::<u64>(1);
            let (t2, mut r2) = mpsc_light::bounded::<u64>(1);
            let peer = thread::spawn(move || {
                park_on(async move {
                    for _ in 0..ROUND {
                        r1.recv_hot().await;
                        t2.send(1).await.unwrap();
                    }
                })
            });
            park_on(async move {
                for _ in 0..ROUND {
                    t1.send(0).await.unwrap();
                    black_box(r2.recv_hot().await);
                }
            });
            peer.join().unwrap();
        })
    });

    g.bench_function("cynosure light (recv default)", |b| {
        b.iter(|| {
            let (t1, mut r1) = mpsc_light::bounded::<u64>(1);
            let (t2, mut r2) = mpsc_light::bounded::<u64>(1);
            let peer = thread::spawn(move || {
                park_on(async move {
                    for _ in 0..ROUND {
                        r1.recv().await;
                        t2.send(1).await.unwrap();
                    }
                })
            });
            park_on(async move {
                for _ in 0..ROUND {
                    t1.send(0).await.unwrap();
                    black_box(r2.recv().await);
                }
            });
            peer.join().unwrap();
        })
    });

    g.bench_function("kanal", |b| {
        b.iter(|| {
            let (t1, r1) = kanal::bounded_async::<u64>(1);
            let (t2, r2) = kanal::bounded_async::<u64>(1);
            let peer = thread::spawn(move || {
                park_on(async move {
                    for _ in 0..ROUND {
                        r1.recv().await.unwrap();
                        t2.send(1).await.unwrap();
                    }
                })
            });
            park_on(async move {
                for _ in 0..ROUND {
                    t1.send(0).await.unwrap();
                    black_box(r2.recv().await.unwrap());
                }
            });
            peer.join().unwrap();
        })
    });

    g.finish();
}

criterion_group!(benches, bench_async_fanin, bench_latency);
criterion_main!(benches);
