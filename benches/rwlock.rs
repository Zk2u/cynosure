//! Benchmark comparing LocalRwLock vs parking_lot vs tokio vs std rwlocks.
//!
//! LocalRwLock is designed for single-threaded async executors, so we test
//! both sync and async patterns on a single thread.

use std::{
    cell::{Cell, RefCell, UnsafeCell},
    rc::Rc,
    sync::{Arc, RwLock as StdRwLock},
};

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use cynosure::site_c::rwlock::LocalRwLock;
use parking_lot::RwLock as ParkingLotRwLock;
use tokio::sync::RwLock as TokioRwLock;

// ============================================================================
// Minimal rwlock implementation without Rc - to measure raw overhead
// ============================================================================

struct RawRwLock<T> {
    readers: Cell<usize>,
    writer: Cell<bool>,
    value: UnsafeCell<T>,
}

struct RawReadGuard<'a, T> {
    rwlock: &'a RawRwLock<T>,
}

struct RawWriteGuard<'a, T> {
    rwlock: &'a RawRwLock<T>,
}

impl<T> RawRwLock<T> {
    #[inline]
    fn new(value: T) -> Self {
        Self {
            readers: Cell::new(0),
            writer: Cell::new(false),
            value: UnsafeCell::new(value),
        }
    }

    #[inline]
    fn try_read(&self) -> Option<RawReadGuard<'_, T>> {
        if !self.writer.get() {
            self.readers.set(self.readers.get() + 1);
            Some(RawReadGuard { rwlock: self })
        } else {
            None
        }
    }

    #[inline]
    fn try_write(&self) -> Option<RawWriteGuard<'_, T>> {
        if self.readers.get() == 0 && !self.writer.get() {
            self.writer.set(true);
            Some(RawWriteGuard { rwlock: self })
        } else {
            None
        }
    }
}

impl<T> std::ops::Deref for RawReadGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.rwlock.value.get() }
    }
}

impl<T> Drop for RawReadGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.rwlock.readers.set(self.rwlock.readers.get() - 1);
    }
}

impl<T> std::ops::Deref for RawWriteGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.rwlock.value.get() }
    }
}

impl<T> std::ops::DerefMut for RawWriteGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.rwlock.value.get() }
    }
}

impl<T> Drop for RawWriteGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.rwlock.writer.set(false);
    }
}

// ============================================================================
// Read Latency: single read lock/unlock cycle
// ============================================================================

fn bench_read_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("Read Lock Latency");

    // LocalRwLock - try_read (sync)
    group.bench_function("LocalRwLock::try_read", |b| {
        let rwlock = LocalRwLock::new(0u64);
        b.iter(|| {
            let guard = rwlock.try_read().unwrap();
            black_box(&*guard);
            drop(guard);
        })
    });

    // LocalRwLock - read().await (async, but polls once since uncontended)
    group.bench_function("LocalRwLock::read (async)", |b| {
        let rwlock = LocalRwLock::new(0u64);
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

            let mut fut = rwlock.read();
            match unsafe { Pin::new_unchecked(&mut fut).poll(&mut cx) } {
                Poll::Ready(guard) => {
                    black_box(&*guard);
                }
                Poll::Pending => unreachable!("uncontended lock should be ready"),
            }
        })
    });

    // parking_lot
    group.bench_function("parking_lot::RwLock::read", |b| {
        let rwlock = ParkingLotRwLock::new(0u64);
        b.iter(|| {
            let guard = rwlock.read();
            black_box(&*guard);
            drop(guard);
        })
    });

    // parking_lot try_read
    group.bench_function("parking_lot::RwLock::try_read", |b| {
        let rwlock = ParkingLotRwLock::new(0u64);
        b.iter(|| {
            let guard = rwlock.try_read().unwrap();
            black_box(&*guard);
            drop(guard);
        })
    });

    // std::sync::RwLock
    group.bench_function("std::sync::RwLock::read", |b| {
        let rwlock = StdRwLock::new(0u64);
        b.iter(|| {
            let guard = rwlock.read().unwrap();
            black_box(&*guard);
            drop(guard);
        })
    });

    // tokio (async)
    group.bench_function("tokio::sync::RwLock::read (async)", |b| {
        let rwlock = TokioRwLock::new(0u64);
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

            let mut fut = rwlock.read();
            match unsafe { Pin::new_unchecked(&mut fut).poll(&mut cx) } {
                Poll::Ready(guard) => {
                    black_box(&*guard);
                }
                Poll::Pending => unreachable!("uncontended lock should be ready"),
            }
        })
    });

    // RefCell baseline (no locking, just borrow checking)
    group.bench_function("RefCell::borrow (baseline)", |b| {
        let cell = RefCell::new(0u64);
        b.iter(|| {
            let guard = cell.borrow();
            black_box(&*guard);
            drop(guard);
        })
    });

    // RawRwLock - Cell + UnsafeCell without waiter queues
    group.bench_function("RawRwLock (no waiters, baseline)", |b| {
        let rwlock = RawRwLock::new(0u64);
        b.iter(|| {
            let guard = rwlock.try_read().unwrap();
            black_box(&*guard);
            drop(guard);
        })
    });

    group.finish();
}

// ============================================================================
// Write Latency: single write lock/unlock cycle
// ============================================================================

fn bench_write_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("Write Lock Latency");

    // LocalRwLock - try_write (sync)
    group.bench_function("LocalRwLock::try_write", |b| {
        let rwlock = LocalRwLock::new(0u64);
        b.iter(|| {
            let mut guard = rwlock.try_write().unwrap();
            *guard += 1;
            black_box(&*guard);
            drop(guard);
        })
    });

    // LocalRwLock - write().await (async)
    group.bench_function("LocalRwLock::write (async)", |b| {
        let rwlock = LocalRwLock::new(0u64);
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

            let mut fut = rwlock.write();
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
    group.bench_function("parking_lot::RwLock::write", |b| {
        let rwlock = ParkingLotRwLock::new(0u64);
        b.iter(|| {
            let mut guard = rwlock.write();
            *guard += 1;
            black_box(&*guard);
            drop(guard);
        })
    });

    // parking_lot try_write
    group.bench_function("parking_lot::RwLock::try_write", |b| {
        let rwlock = ParkingLotRwLock::new(0u64);
        b.iter(|| {
            let mut guard = rwlock.try_write().unwrap();
            *guard += 1;
            black_box(&*guard);
            drop(guard);
        })
    });

    // std::sync::RwLock
    group.bench_function("std::sync::RwLock::write", |b| {
        let rwlock = StdRwLock::new(0u64);
        b.iter(|| {
            let mut guard = rwlock.write().unwrap();
            *guard += 1;
            black_box(&*guard);
            drop(guard);
        })
    });

    // tokio (async)
    group.bench_function("tokio::sync::RwLock::write (async)", |b| {
        let rwlock = TokioRwLock::new(0u64);
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

            let mut fut = rwlock.write();
            match unsafe { Pin::new_unchecked(&mut fut).poll(&mut cx) } {
                Poll::Ready(mut guard) => {
                    *guard += 1;
                    black_box(&*guard);
                }
                Poll::Pending => unreachable!("uncontended lock should be ready"),
            }
        })
    });

    // RefCell baseline
    group.bench_function("RefCell::borrow_mut (baseline)", |b| {
        let cell = RefCell::new(0u64);
        b.iter(|| {
            let mut guard = cell.borrow_mut();
            *guard += 1;
            black_box(&*guard);
            drop(guard);
        })
    });

    // RawRwLock baseline
    group.bench_function("RawRwLock (no waiters, baseline)", |b| {
        let rwlock = RawRwLock::new(0u64);
        b.iter(|| {
            let mut guard = rwlock.try_write().unwrap();
            *guard += 1;
            black_box(&*guard);
            drop(guard);
        })
    });

    group.finish();
}

// ============================================================================
// Read Throughput: many read lock/unlock cycles
// ============================================================================

fn bench_read_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("Read Lock Throughput");

    const ITERATIONS: u64 = 10_000;
    group.throughput(criterion::Throughput::Elements(ITERATIONS));

    group.bench_function("LocalRwLock", |b| {
        b.iter_batched(
            || LocalRwLock::new(0u64),
            |rwlock| {
                for _ in 0..ITERATIONS {
                    let guard = rwlock.try_read().unwrap();
                    black_box(&*guard);
                }
                rwlock
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("parking_lot", |b| {
        b.iter_batched(
            || ParkingLotRwLock::new(0u64),
            |rwlock| {
                for _ in 0..ITERATIONS {
                    let guard = rwlock.read();
                    black_box(&*guard);
                }
                rwlock
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("std::sync::RwLock", |b| {
        b.iter_batched(
            || StdRwLock::new(0u64),
            |rwlock| {
                for _ in 0..ITERATIONS {
                    let guard = rwlock.read().unwrap();
                    black_box(&*guard);
                }
                rwlock
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("RefCell (baseline)", |b| {
        b.iter_batched(
            || RefCell::new(0u64),
            |cell| {
                for _ in 0..ITERATIONS {
                    let guard = cell.borrow();
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
// Write Throughput: many write lock/unlock cycles
// ============================================================================

fn bench_write_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("Write Lock Throughput");

    const ITERATIONS: u64 = 10_000;
    group.throughput(criterion::Throughput::Elements(ITERATIONS));

    group.bench_function("LocalRwLock", |b| {
        b.iter_batched(
            || LocalRwLock::new(0u64),
            |rwlock| {
                for _ in 0..ITERATIONS {
                    let mut guard = rwlock.try_write().unwrap();
                    *guard += 1;
                    black_box(&*guard);
                }
                rwlock
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("parking_lot", |b| {
        b.iter_batched(
            || ParkingLotRwLock::new(0u64),
            |rwlock| {
                for _ in 0..ITERATIONS {
                    let mut guard = rwlock.write();
                    *guard += 1;
                    black_box(&*guard);
                }
                rwlock
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("std::sync::RwLock", |b| {
        b.iter_batched(
            || StdRwLock::new(0u64),
            |rwlock| {
                for _ in 0..ITERATIONS {
                    let mut guard = rwlock.write().unwrap();
                    *guard += 1;
                    black_box(&*guard);
                }
                rwlock
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
    let mut group = c.benchmark_group("RwLock Clone + Lock Pattern");

    group.bench_function("LocalRwLock (Rc::clone + read)", |b| {
        let rwlock = Rc::new(LocalRwLock::new(0u64));
        b.iter(|| {
            let r = rwlock.clone();
            let guard = r.try_read().unwrap();
            black_box(&*guard);
        })
    });

    group.bench_function("LocalRwLock (Rc::clone + write)", |b| {
        let rwlock = Rc::new(LocalRwLock::new(0u64));
        b.iter(|| {
            let r = rwlock.clone();
            let mut guard = r.try_write().unwrap();
            *guard += 1;
            black_box(&*guard);
        })
    });

    group.bench_function("parking_lot (Arc clone + read)", |b| {
        let rwlock = Arc::new(ParkingLotRwLock::new(0u64));
        b.iter(|| {
            let r = rwlock.clone();
            let guard = r.read();
            black_box(&*guard);
        })
    });

    group.bench_function("parking_lot (Arc clone + write)", |b| {
        let rwlock = Arc::new(ParkingLotRwLock::new(0u64));
        b.iter(|| {
            let r = rwlock.clone();
            let mut guard = r.write();
            *guard += 1;
            black_box(&*guard);
        })
    });

    group.bench_function("tokio (Arc clone + read)", |b| {
        let rwlock = Arc::new(TokioRwLock::new(0u64));
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

            let r = rwlock.clone();
            let mut fut = r.read();
            match unsafe { Pin::new_unchecked(&mut fut).poll(&mut cx) } {
                Poll::Ready(guard) => {
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
            let guard = c.borrow();
            black_box(&*guard);
        })
    });

    group.finish();
}

// ============================================================================
// Mixed read/write pattern
// ============================================================================

fn bench_mixed_read_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("Mixed Read/Write (90% read, 10% write)");

    const ITERATIONS: u64 = 10_000;
    group.throughput(criterion::Throughput::Elements(ITERATIONS));

    group.bench_function("LocalRwLock", |b| {
        b.iter_batched(
            || LocalRwLock::new(0u64),
            |rwlock| {
                for i in 0..ITERATIONS {
                    if i % 10 == 0 {
                        let mut guard = rwlock.try_write().unwrap();
                        *guard += 1;
                        black_box(&*guard);
                    } else {
                        let guard = rwlock.try_read().unwrap();
                        black_box(&*guard);
                    }
                }
                rwlock
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("parking_lot", |b| {
        b.iter_batched(
            || ParkingLotRwLock::new(0u64),
            |rwlock| {
                for i in 0..ITERATIONS {
                    if i % 10 == 0 {
                        let mut guard = rwlock.write();
                        *guard += 1;
                        black_box(&*guard);
                    } else {
                        let guard = rwlock.read();
                        black_box(&*guard);
                    }
                }
                rwlock
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("std::sync::RwLock", |b| {
        b.iter_batched(
            || StdRwLock::new(0u64),
            |rwlock| {
                for i in 0..ITERATIONS {
                    if i % 10 == 0 {
                        let mut guard = rwlock.write().unwrap();
                        *guard += 1;
                        black_box(&*guard);
                    } else {
                        let guard = rwlock.read().unwrap();
                        black_box(&*guard);
                    }
                }
                rwlock
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("RefCell (baseline)", |b| {
        b.iter_batched(
            || RefCell::new(0u64),
            |cell| {
                for i in 0..ITERATIONS {
                    if i % 10 == 0 {
                        let mut guard = cell.borrow_mut();
                        *guard += 1;
                        black_box(&*guard);
                    } else {
                        let guard = cell.borrow();
                        black_box(&*guard);
                    }
                }
                cell
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

// ============================================================================
// Creation overhead
// ============================================================================

fn bench_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("RwLock Creation");

    group.bench_function("LocalRwLock::new", |b| {
        b.iter(|| black_box(LocalRwLock::new(0u64)))
    });

    group.bench_function("parking_lot::RwLock::new", |b| {
        b.iter(|| black_box(ParkingLotRwLock::new(0u64)))
    });

    group.bench_function("std::sync::RwLock::new", |b| {
        b.iter(|| black_box(StdRwLock::new(0u64)))
    });

    group.bench_function("tokio::sync::RwLock::new", |b| {
        b.iter(|| black_box(TokioRwLock::new(0u64)))
    });

    group.bench_function("RefCell::new", |b| b.iter(|| black_box(RefCell::new(0u64))));

    group.finish();
}

criterion_group!(
    benches,
    bench_read_latency,
    bench_write_latency,
    bench_read_throughput,
    bench_write_throughput,
    bench_clone_and_lock,
    bench_mixed_read_write,
    bench_creation,
);
criterion_main!(benches);
