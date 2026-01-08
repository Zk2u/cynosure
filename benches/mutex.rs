//! Benchmark comparing LocalMutex vs parking_lot vs tokio vs std mutexes.
//!
//! LocalMutex is designed for single-threaded async executors, so we test
//! both sync and async patterns on a single thread.

use std::{
    cell::{Cell, RefCell, UnsafeCell},
    rc::Rc,
    sync::{Arc, Mutex as StdMutex},
};

use criterion::{BatchSize, BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use cynosure::site_c::mutex::LocalMutex;
use parking_lot::Mutex as ParkingLotMutex;
use tokio::sync::Mutex as TokioMutex;

// ============================================================================
// Minimal lock implementation without Rc - to measure raw overhead
// ============================================================================

struct RawMutex<T> {
    locked: Cell<bool>,
    value: UnsafeCell<T>,
}

struct RawMutexGuard<'a, T> {
    mutex: &'a RawMutex<T>,
}

impl<T> RawMutex<T> {
    #[inline]
    fn new(value: T) -> Self {
        Self {
            locked: Cell::new(false),
            value: UnsafeCell::new(value),
        }
    }

    #[inline]
    fn try_lock(&self) -> Option<RawMutexGuard<'_, T>> {
        if !self.locked.replace(true) {
            Some(RawMutexGuard { mutex: self })
        } else {
            None
        }
    }
}

impl<T> std::ops::Deref for RawMutexGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> std::ops::DerefMut for RawMutexGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for RawMutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.locked.set(false);
    }
}

// ============================================================================
// Latency: single lock/unlock cycle
// ============================================================================

fn bench_lock_unlock_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("Lock-Unlock Latency");

    // LocalMutex - try_lock (sync)
    group.bench_function("LocalMutex::try_lock", |b| {
        let mutex = LocalMutex::new(0u64);
        b.iter(|| {
            let mut guard = mutex.try_lock().unwrap();
            *guard += 1;
            black_box(&*guard);
            drop(guard);
        })
    });

    // LocalMutex - lock().await (async, but polls once since uncontended)
    group.bench_function("LocalMutex::lock (async)", |b| {
        let mutex = LocalMutex::new(0u64);
        b.iter(|| {
            // Simulate what an executor does - poll the future once
            use std::{
                future::Future,
                pin::Pin,
                task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
            };

            static VTABLE: RawWakerVTable =
                RawWakerVTable::new(|p| RawWaker::new(p, &VTABLE), |_| {}, |_| {}, |_| {});
            let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
            let mut cx = Context::from_waker(&waker);

            let mut fut = mutex.lock();
            match unsafe { Pin::new_unchecked(&mut fut).poll(&mut cx) } {
                Poll::Ready(mut guard) => {
                    *guard += 1;
                    black_box(&*guard);
                }
                Poll::Pending => unreachable!("uncontended lock should be ready"),
            }
        })
    });

    // parking_lot
    group.bench_function("parking_lot::Mutex::lock", |b| {
        let mutex = ParkingLotMutex::new(0u64);
        b.iter(|| {
            let mut guard = mutex.lock();
            *guard += 1;
            black_box(&*guard);
            drop(guard);
        })
    });

    // parking_lot try_lock
    group.bench_function("parking_lot::Mutex::try_lock", |b| {
        let mutex = ParkingLotMutex::new(0u64);
        b.iter(|| {
            let mut guard = mutex.try_lock().unwrap();
            *guard += 1;
            black_box(&*guard);
            drop(guard);
        })
    });

    // std::sync::Mutex
    group.bench_function("std::sync::Mutex::lock", |b| {
        let mutex = StdMutex::new(0u64);
        b.iter(|| {
            let mut guard = mutex.lock().unwrap();
            *guard += 1;
            black_box(&*guard);
            drop(guard);
        })
    });

    // tokio (async)
    group.bench_function("tokio::sync::Mutex::lock (async)", |b| {
        let mutex = TokioMutex::new(0u64);
        b.iter(|| {
            use std::{
                future::Future,
                pin::Pin,
                task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
            };

            static VTABLE: RawWakerVTable =
                RawWakerVTable::new(|p| RawWaker::new(p, &VTABLE), |_| {}, |_| {}, |_| {});
            let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
            let mut cx = Context::from_waker(&waker);

            let mut fut = mutex.lock();
            match unsafe { Pin::new_unchecked(&mut fut).poll(&mut cx) } {
                Poll::Ready(mut guard) => {
                    *guard += 1;
                    black_box(&*guard);
                }
                Poll::Pending => unreachable!("uncontended lock should be ready"),
            }
        })
    });

    // RefCell baseline (no locking, just borrow checking)
    group.bench_function("RefCell::borrow_mut (baseline)", |b| {
        let cell = RefCell::new(0u64);
        b.iter(|| {
            let mut guard = cell.borrow_mut();
            *guard += 1;
            black_box(&*guard);
            drop(guard);
        })
    });

    // RawMutex - Cell<bool> + UnsafeCell without Rc indirection
    group.bench_function("RawMutex (no Rc, baseline)", |b| {
        let mutex = RawMutex::new(0u64);
        b.iter(|| {
            let mut guard = mutex.try_lock().unwrap();
            *guard += 1;
            black_box(&*guard);
            drop(guard);
        })
    });

    group.finish();
}

// ============================================================================
// Throughput: many lock/unlock cycles
// ============================================================================

fn bench_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("Lock Throughput");

    const ITERATIONS: u64 = 10_000;
    group.throughput(criterion::Throughput::Elements(ITERATIONS));

    group.bench_function("LocalMutex", |b| {
        b.iter_batched(
            || LocalMutex::new(0u64),
            |mutex| {
                for _ in 0..ITERATIONS {
                    let mut guard = mutex.try_lock().unwrap();
                    *guard += 1;
                    black_box(&*guard);
                }
                mutex
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("parking_lot", |b| {
        b.iter_batched(
            || ParkingLotMutex::new(0u64),
            |mutex| {
                for _ in 0..ITERATIONS {
                    let mut guard = mutex.lock();
                    *guard += 1;
                    black_box(&*guard);
                }
                mutex
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("std::sync::Mutex", |b| {
        b.iter_batched(
            || StdMutex::new(0u64),
            |mutex| {
                for _ in 0..ITERATIONS {
                    let mut guard = mutex.lock().unwrap();
                    *guard += 1;
                    black_box(&*guard);
                }
                mutex
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("RefCell (baseline)", |b| {
        b.iter_batched(
            || RefCell::new(0u64),
            |cell| {
                for _ in 0..ITERATIONS {
                    let mut guard = cell.borrow_mut();
                    *guard += 1;
                    black_box(&*guard);
                }
                cell
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

// ============================================================================
// Clone + lock pattern (common in async code)
// ============================================================================

fn bench_clone_and_lock(c: &mut Criterion) {
    let mut group = c.benchmark_group("Clone + Lock Pattern");

    group.bench_function("LocalMutex (Rc::clone + lock)", |b| {
        let mutex = Rc::new(LocalMutex::new(0u64));
        b.iter(|| {
            let m = mutex.clone();
            let mut guard = m.try_lock().unwrap();
            *guard += 1;
            black_box(&*guard);
        })
    });

    group.bench_function("parking_lot (Arc clone + lock)", |b| {
        let mutex = Arc::new(ParkingLotMutex::new(0u64));
        b.iter(|| {
            let m = mutex.clone();
            let mut guard = m.lock();
            *guard += 1;
            black_box(&*guard);
        })
    });

    group.bench_function("tokio (Arc clone + lock)", |b| {
        let mutex = Arc::new(TokioMutex::new(0u64));
        b.iter(|| {
            use std::{
                future::Future,
                pin::Pin,
                task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
            };

            static VTABLE: RawWakerVTable =
                RawWakerVTable::new(|p| RawWaker::new(p, &VTABLE), |_| {}, |_| {}, |_| {});
            let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
            let mut cx = Context::from_waker(&waker);

            let m = mutex.clone();
            let mut fut = m.lock();
            match unsafe { Pin::new_unchecked(&mut fut).poll(&mut cx) } {
                Poll::Ready(mut guard) => {
                    *guard += 1;
                    black_box(&*guard);
                }
                Poll::Pending => unreachable!(),
            }
        })
    });

    group.bench_function("RefCell (Rc clone + borrow)", |b| {
        let cell = Rc::new(RefCell::new(0u64));
        b.iter(|| {
            let c = cell.clone();
            let mut guard = c.borrow_mut();
            *guard += 1;
            black_box(&*guard);
        })
    });

    group.finish();
}

// ============================================================================
// Different data sizes
// ============================================================================

fn bench_data_sizes(c: &mut Criterion) {
    let sizes: &[usize] = &[8, 64, 256, 1024];

    for &size in sizes {
        let mut group = c.benchmark_group(format!("Data Size {size}B"));

        group.bench_with_input(BenchmarkId::new("LocalMutex", size), &size, |b, &size| {
            let mutex = LocalMutex::new(vec![0u8; size]);
            b.iter(|| {
                let mut guard = mutex.try_lock().unwrap();
                guard[0] = guard[0].wrapping_add(1);
                black_box(&guard[..]);
            })
        });

        group.bench_with_input(BenchmarkId::new("parking_lot", size), &size, |b, &size| {
            let mutex = ParkingLotMutex::new(vec![0u8; size]);
            b.iter(|| {
                let mut guard = mutex.lock();
                guard[0] = guard[0].wrapping_add(1);
                black_box(&guard[..]);
            })
        });

        group.bench_with_input(
            BenchmarkId::new("std::sync::Mutex", size),
            &size,
            |b, &size| {
                let mutex = StdMutex::new(vec![0u8; size]);
                b.iter(|| {
                    let mut guard = mutex.lock().unwrap();
                    guard[0] = guard[0].wrapping_add(1);
                    black_box(&guard[..]);
                })
            },
        );

        group.bench_with_input(BenchmarkId::new("RefCell", size), &size, |b, &size| {
            let cell = RefCell::new(vec![0u8; size]);
            b.iter(|| {
                let mut guard = cell.borrow_mut();
                guard[0] = guard[0].wrapping_add(1);
                black_box(&guard[..]);
            })
        });

        group.finish();
    }
}

// ============================================================================
// Async simulation: multiple "tasks" acquiring lock in sequence
// ============================================================================

fn bench_async_sequential(c: &mut Criterion) {
    let mut group = c.benchmark_group("Async Sequential (single-threaded)");

    const TASKS: usize = 100;
    group.throughput(criterion::Throughput::Elements(TASKS as u64));

    // Helper to poll a future to completion
    fn block_on<F: std::future::Future>(mut f: F) -> F::Output {
        use std::{
            pin::Pin,
            task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
        };

        static VTABLE: RawWakerVTable =
            RawWakerVTable::new(|p| RawWaker::new(p, &VTABLE), |_| {}, |_| {}, |_| {});
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut f = unsafe { Pin::new_unchecked(&mut f) };

        loop {
            match f.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => {} // In real async, would yield; here just spin
            }
        }
    }

    group.bench_function("LocalMutex", |b| {
        b.iter_batched(
            || LocalMutex::new(0u64),
            |mutex| {
                for _ in 0..TASKS {
                    block_on(async {
                        let mut guard = mutex.lock().await;
                        *guard += 1;
                        black_box(&*guard);
                    });
                }
                mutex
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("tokio::sync::Mutex", |b| {
        b.iter_batched(
            || TokioMutex::new(0u64),
            |mutex| {
                for _ in 0..TASKS {
                    block_on(async {
                        let mut guard = mutex.lock().await;
                        *guard += 1;
                        black_box(&*guard);
                    });
                }
                mutex
            },
            BatchSize::SmallInput,
        )
    });

    // parking_lot doesn't have async, so use sync for comparison
    group.bench_function("parking_lot (sync baseline)", |b| {
        b.iter_batched(
            || ParkingLotMutex::new(0u64),
            |mutex| {
                for _ in 0..TASKS {
                    let mut guard = mutex.lock();
                    *guard += 1;
                    black_box(&*guard);
                }
                mutex
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

// ============================================================================
// Memory overhead comparison
// ============================================================================

fn bench_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("Mutex Creation");

    group.bench_function("LocalMutex::new", |b| {
        b.iter(|| black_box(LocalMutex::new(0u64)))
    });

    group.bench_function("parking_lot::Mutex::new", |b| {
        b.iter(|| black_box(ParkingLotMutex::new(0u64)))
    });

    group.bench_function("std::sync::Mutex::new", |b| {
        b.iter(|| black_box(StdMutex::new(0u64)))
    });

    group.bench_function("tokio::sync::Mutex::new", |b| {
        b.iter(|| black_box(TokioMutex::new(0u64)))
    });

    group.bench_function("RefCell::new", |b| b.iter(|| black_box(RefCell::new(0u64))));

    group.finish();
}

criterion_group!(
    benches,
    bench_lock_unlock_latency,
    bench_throughput,
    bench_clone_and_lock,
    bench_data_sizes,
    bench_async_sequential,
    bench_creation,
);
criterion_main!(benches);
