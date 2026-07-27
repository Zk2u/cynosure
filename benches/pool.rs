//! Buffer pool: acquire + return one buffer. cynosure recycles aligned IO
//! buffers (single-core, returns `'static` `IoBuf` handles); compared against
//! popular object pools and a naive `Mutex<Vec>`.

use std::sync::Mutex;

use criterion::{Criterion, black_box, criterion_group, criterion_main};

const COUNT: usize = 8;
const SIZE: usize = 4096;

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("Pool acquire+return");

    let cp = cynosure::site_c::pool::LocalBufferPool::<u8>::new(COUNT, SIZE);
    g.bench_function("cynosure LocalBufferPool", |b| {
        b.iter(|| black_box(cp.try_acquire().unwrap()))
    });

    let op: object_pool::Pool<Vec<u8>> = object_pool::Pool::new(COUNT, || vec![0u8; SIZE]);
    g.bench_function("object-pool", |b| {
        b.iter(|| black_box(op.try_pull().unwrap()))
    });

    let lf = lockfree_object_pool::LinearObjectPool::<Vec<u8>>::new(|| vec![0u8; SIZE], |_v| {});
    g.bench_function("lockfree-object-pool", |b| b.iter(|| black_box(lf.pull())));

    let mp: Mutex<Vec<Vec<u8>>> = Mutex::new((0..COUNT).map(|_| vec![0u8; SIZE]).collect());
    g.bench_function("Mutex<Vec>", |b| {
        b.iter(|| {
            let v = mp.lock().unwrap().pop().unwrap();
            mp.lock().unwrap().push(v);
        })
    });

    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
