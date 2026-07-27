//! Semaphore: uncontended `try_acquire` + release. cynosure's is
//! single-threaded (non-atomic `Cell`); the others are thread-safe (atomics),
//! which is the gap.

use criterion::{Criterion, black_box, criterion_group, criterion_main};

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("Semaphore try_acquire+drop");

    let cs = cynosure::site_c::semaphore::LocalSemaphore::new(1);
    g.bench_function("cynosure LocalSemaphore", |b| {
        b.iter(|| {
            let _p = black_box(cs.try_acquire().unwrap());
        })
    });

    let ts = tokio::sync::Semaphore::new(1);
    g.bench_function("tokio", |b| {
        b.iter(|| {
            let _p = black_box(ts.try_acquire().unwrap());
        })
    });

    let als = async_lock::Semaphore::new(1);
    g.bench_function("async-lock", |b| {
        b.iter(|| {
            let _p = black_box(als.try_acquire().unwrap());
        })
    });

    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
