use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
};

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use cynosure::site_c::mutex::LocalMutex;
use tokio::sync::Mutex as TokioMutex;

// Simple executor for benchmarking async code
struct DummyWaker;

impl Wake for DummyWaker {
    fn wake(self: Arc<Self>) {}
}

fn dummy_waker() -> Waker {
    Arc::new(DummyWaker).into()
}

fn block_on<F: Future>(mut future: F) -> F::Output {
    let waker = dummy_waker();
    let mut cx = Context::from_waker(&waker);

    // Safety: we're pinning the future on the stack, but we won't move it
    let mut future = unsafe { Pin::new_unchecked(&mut future) };

    loop {
        match Future::poll(future.as_mut(), &mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => {
                // In a real benchmark, we might want to yield here
                // For simplicity, we'll just continue polling
            }
        }
    }
}

// Basic operations benchmarks
fn bench_mutex_basic_ops(c: &mut Criterion) {
    let mut group = c.benchmark_group("Basic Mutex Operations");

    // Simple lock/unlock cycle
    group.bench_function("LocalMutex::lock_unlock", |b| {
        b.iter(|| {
            let mutex = LocalMutex::new(42);
            let guard = block_on(mutex.lock());
            black_box(*guard);
            drop(guard);
        })
    });

    group.bench_function("TokioMutex::lock_unlock", |b| {
        b.iter(|| {
            let mutex = TokioMutex::new(42);
            let guard = block_on(mutex.lock());
            black_box(*guard);
            drop(guard);
        })
    });

    // try_lock when mutex is available
    group.bench_function("LocalMutex::try_lock_available", |b| {
        b.iter(|| {
            let mutex = LocalMutex::new(42);
            let guard = mutex.try_lock();
            black_box(guard);
        })
    });

    group.bench_function("TokioMutex::try_lock_available", |b| {
        b.iter(|| {
            let mutex = TokioMutex::new(42);
            let guard = mutex.try_lock();
            let _ = black_box(guard);
        })
    });

    // try_lock when mutex is unavailable
    group.bench_function("LocalMutex::try_lock_unavailable", |b| {
        b.iter(|| {
            let mutex = LocalMutex::new(42);
            let _guard = mutex.try_lock().unwrap();
            let result = mutex.try_lock();
            black_box(result);
        })
    });

    group.bench_function("TokioMutex::try_lock_unavailable", |b| {
        b.iter(|| {
            let mutex = TokioMutex::new(42);
            let _guard = mutex.try_lock().unwrap();
            let result = mutex.try_lock();
            let _ = black_box(result);
        })
    });

    // Lock and modify value
    group.bench_function("LocalMutex::lock_modify", |b| {
        b.iter(|| {
            let mutex = LocalMutex::new(42);
            let mut guard = block_on(mutex.lock());
            *guard += 1;
            black_box(*guard);
        })
    });

    group.bench_function("TokioMutex::lock_modify", |b| {
        b.iter(|| {
            let mutex = TokioMutex::new(42);
            let mut guard = block_on(mutex.lock());
            *guard += 1;
            black_box(*guard);
        })
    });

    group.finish();
}

// Test holding locks across "await points" (simulated)
fn bench_mutex_across_awaits(c: &mut Criterion) {
    let mut group = c.benchmark_group("Lock Held Across Await Points");

    // Simulate holding a lock while doing async work
    group.bench_function("LocalMutex::lock_across_async_work", |b| {
        b.iter(|| {
            let mutex = LocalMutex::new(vec![1, 2, 3]);

            block_on(async {
                let mut guard = mutex.lock().await;

                // Simulate some async work while holding the lock
                guard.push(4);

                // In a real scenario, this might be an actual await
                // For benchmarking, we'll simulate with a ready future
                futures::future::ready(()).await;

                guard.push(5);
                black_box(&*guard);
            })
        })
    });

    group.bench_function("TokioMutex::lock_across_async_work", |b| {
        b.iter(|| {
            let mutex = TokioMutex::new(vec![1, 2, 3]);

            block_on(async {
                let mut guard = mutex.lock().await;

                // Simulate some async work while holding the lock
                guard.push(4);

                // In a real scenario, this might be an actual await
                futures::future::ready(()).await;

                guard.push(5);
                black_box(&*guard);
            })
        })
    });

    group.finish();
}

// Sequential operations (no contention)
fn bench_mutex_sequential(c: &mut Criterion) {
    let mut group = c.benchmark_group("Sequential Operations");

    // Multiple sequential lock/unlock cycles
    group.bench_function("LocalMutex::sequential_locks", |b| {
        b.iter(|| {
            let mutex = LocalMutex::new(0);

            block_on(async {
                for _ in 0..10 {
                    let mut guard = mutex.lock().await;
                    *guard += 1;
                    drop(guard);
                }

                let guard = mutex.lock().await;
                black_box(*guard);
            })
        })
    });

    group.bench_function("TokioMutex::sequential_locks", |b| {
        b.iter(|| {
            let mutex = TokioMutex::new(0);

            block_on(async {
                for _ in 0..10 {
                    let mut guard = mutex.lock().await;
                    *guard += 1;
                    drop(guard);
                }

                let guard = mutex.lock().await;
                black_box(*guard);
            })
        })
    });

    group.finish();
}

// Different data sizes
fn bench_mutex_data_sizes(c: &mut Criterion) {
    let data_sizes = [8, 64, 512, 4096];

    for &size in &data_sizes {
        let mut group = c.benchmark_group(format!("Data Size: {} bytes", size));

        group.bench_with_input(BenchmarkId::new("LocalMutex", size), &size, |b, &size| {
            b.iter(|| {
                let mutex = LocalMutex::new(vec![0u8; size]);

                block_on(async {
                    let mut guard = mutex.lock().await;
                    guard.push(1);
                    black_box(&*guard);
                })
            })
        });

        group.bench_with_input(BenchmarkId::new("TokioMutex", size), &size, |b, &size| {
            b.iter(|| {
                let mutex = TokioMutex::new(vec![0u8; size]);

                block_on(async {
                    let mut guard = mutex.lock().await;
                    guard.push(1);
                    black_box(&*guard);
                })
            })
        });

        group.finish();
    }
}

// Clone overhead comparison
fn bench_mutex_cloning(c: &mut Criterion) {
    let mut group = c.benchmark_group("Mutex Cloning");

    group.bench_function("LocalMutex::clone", |b| {
        b.iter(|| {
            let mutex = LocalMutex::new(42);
            let cloned = mutex.clone();
            black_box(cloned);
        })
    });

    group.bench_function("TokioMutex::clone", |b| {
        b.iter(|| {
            let mutex = Arc::new(TokioMutex::new(42));
            let cloned = Arc::clone(&mutex);
            black_box(cloned);
        })
    });

    // Test using the cloned mutexes
    group.bench_function("LocalMutex::use_cloned", |b| {
        b.iter(|| {
            let mutex = LocalMutex::new(0);
            let mutex2 = mutex.clone();

            block_on(async {
                {
                    let mut guard = mutex.lock().await;
                    *guard += 1;
                }

                {
                    let mut guard = mutex2.lock().await;
                    *guard += 1;
                }

                let guard = mutex.lock().await;
                black_box(*guard);
            })
        })
    });

    group.bench_function("TokioMutex::use_cloned", |b| {
        b.iter(|| {
            let mutex = Arc::new(TokioMutex::new(0));
            let mutex2 = Arc::clone(&mutex);

            block_on(async {
                {
                    let mut guard = mutex.lock().await;
                    *guard += 1;
                }

                {
                    let mut guard = mutex2.lock().await;
                    *guard += 1;
                }

                let guard = mutex.lock().await;
                black_box(*guard);
            })
        })
    });

    group.finish();
}

// Memory overhead comparison
fn bench_mutex_memory_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("Memory Overhead");

    // Test creation overhead
    group.bench_function("LocalMutex::creation_overhead", |b| {
        b.iter(|| {
            let mutexes: Vec<_> = (0..100).map(|i| LocalMutex::new(i)).collect();
            black_box(mutexes);
        })
    });

    group.bench_function("TokioMutex::creation_overhead", |b| {
        b.iter(|| {
            let mutexes: Vec<_> = (0..100).map(|i| Arc::new(TokioMutex::new(i))).collect();
            black_box(mutexes);
        })
    });

    group.finish();
}

// Realistic usage patterns
fn bench_realistic_patterns(c: &mut Criterion) {
    let mut group = c.benchmark_group("Realistic Usage Patterns");

    // Shared counter pattern
    group.bench_function("LocalMutex::shared_counter", |b| {
        b.iter(|| {
            let counter = LocalMutex::new(0);
            let counter2 = counter.clone();
            let counter3 = counter.clone();

            block_on(async {
                // Multiple tasks incrementing a shared counter
                {
                    let mut guard = counter.lock().await;
                    *guard += 1;
                }

                {
                    let mut guard = counter2.lock().await;
                    *guard += 1;
                }

                {
                    let mut guard = counter3.lock().await;
                    *guard += 1;
                }

                let final_count = counter.lock().await;
                black_box(*final_count);
            })
        })
    });

    group.bench_function("TokioMutex::shared_counter", |b| {
        b.iter(|| {
            let counter = Arc::new(TokioMutex::new(0));
            let counter2 = Arc::clone(&counter);
            let counter3 = Arc::clone(&counter);

            block_on(async {
                // Multiple tasks incrementing a shared counter
                {
                    let mut guard = counter.lock().await;
                    *guard += 1;
                }

                {
                    let mut guard = counter2.lock().await;
                    *guard += 1;
                }

                {
                    let mut guard = counter3.lock().await;
                    *guard += 1;
                }

                let final_count = counter.lock().await;
                black_box(*final_count);
            })
        })
    });

    // Read-modify-write pattern
    group.bench_function("LocalMutex::read_modify_write", |b| {
        b.iter(|| {
            let data = LocalMutex::new(vec![1, 2, 3, 4, 5]);

            block_on(async {
                let mut guard = data.lock().await;
                let sum: i32 = guard.iter().sum();
                guard.push(sum);
                black_box(&*guard);
            })
        })
    });

    group.bench_function("TokioMutex::read_modify_write", |b| {
        b.iter(|| {
            let data = TokioMutex::new(vec![1, 2, 3, 4, 5]);

            block_on(async {
                let mut guard = data.lock().await;
                let sum: i32 = guard.iter().sum();
                guard.push(sum);
                black_box(&*guard);
            })
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_mutex_basic_ops,
    bench_mutex_across_awaits,
    bench_mutex_sequential,
    bench_mutex_data_sizes,
    bench_mutex_cloning,
    bench_mutex_memory_overhead,
    bench_realistic_patterns,
);
criterion_main!(benches);
