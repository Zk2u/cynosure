//! Bip buffer (bipartite SPSC byte buffer): reserve + commit + read + release a
//! contiguous chunk. Compared against `bbqueue`, the reference bip buffer.
//!
//! cynosure's grants are `Arc`-backed (`'static`, usable with io_uring across a
//! completion); bbqueue's borrow the producer/consumer — that ~refcount is the
//! gap, the price of the `'static` grant.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

const CAP: usize = 4096;

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("BipBuffer reserve+commit+read+release");
    for &chunk in &[16usize, 64, 256] {
        g.bench_with_input(BenchmarkId::new("cynosure", chunk), &chunk, |b, &chunk| {
            let (mut p, mut c) = cynosure::site_d::bipbuffer::bip_buffer(CAP);
            b.iter(|| {
                let mut gr = p.try_reserve(chunk).unwrap();
                gr.as_mut_slice()[0] = 1;
                gr.commit(chunk);
                let r = c.try_read().unwrap();
                black_box(r.as_slice()[0]);
                let n = r.len().min(chunk);
                r.release(n);
            })
        });
        g.bench_with_input(BenchmarkId::new("bbqueue", chunk), &chunk, |b, &chunk| {
            let bb = bbqueue::BBBuffer::<CAP>::new();
            let (mut prod, mut cons) = bb.try_split().unwrap();
            b.iter(|| {
                let mut gr = prod.grant_exact(chunk).unwrap();
                gr.buf()[0] = 1;
                gr.commit(chunk);
                let r = cons.read().unwrap();
                black_box(r.buf()[0]);
                let n = r.buf().len().min(chunk);
                r.release(n);
            })
        });
    }
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
