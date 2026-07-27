//! oneshot: create + send + receive a single value. All contenders allocate one
//! shared cell per channel, so this is largely allocator-bound.

use criterion::{Criterion, black_box, criterion_group, criterion_main};

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("oneshot create+send+recv");

    g.bench_function("cynosure", |b| {
        b.iter(|| {
            let (tx, mut rx) = cynosure::site_d::oneshot::oneshot::<u64>();
            tx.send(black_box(7)).unwrap();
            black_box(rx.try_recv().unwrap())
        })
    });
    g.bench_function("tokio", |b| {
        b.iter(|| {
            let (tx, mut rx) = tokio::sync::oneshot::channel::<u64>();
            tx.send(black_box(7)).unwrap();
            black_box(rx.try_recv().unwrap())
        })
    });
    g.bench_function("futures-channel", |b| {
        b.iter(|| {
            let (tx, mut rx) = futures::channel::oneshot::channel::<u64>();
            tx.send(black_box(7)).unwrap();
            black_box(rx.try_recv().unwrap().unwrap())
        })
    });
    g.bench_function("oneshot-crate", |b| {
        b.iter(|| {
            let (tx, rx) = oneshot::channel::<u64>();
            tx.send(black_box(7)).unwrap();
            black_box(rx.try_recv().unwrap())
        })
    });
    // The speed-focused async oneshot crates. Neither exposes a try_recv, so the
    // value-ready receive goes through a poll (same as cynosure's Future path).
    g.bench_function("async-oneshot", |b| {
        b.iter(|| {
            let (mut tx, rx) = async_oneshot::oneshot::<u64>();
            tx.send(black_box(7)).unwrap();
            black_box(futures::executor::block_on(rx).unwrap())
        })
    });
    g.bench_function("catty", |b| {
        b.iter(|| {
            let (tx, rx) = catty::oneshot::<u64>();
            tx.send(black_box(7)).unwrap();
            black_box(futures::executor::block_on(rx).unwrap())
        })
    });

    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
