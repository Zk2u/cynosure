//! Sustained-throughput benchmark for the primitives whose committed benches
//! only measured single-operation latency.
//!
//! This is a *separate measurement*, not latency inverted: each case runs a
//! long steady-state loop, so it captures allocator reuse, free-list locality
//! and branch-predictor warmth that a one-shot timing does not.
//!
//! Emits `docs/bench-data/throughput-measured.csv`, which `tools/chartgen`
//! reads alongside the hand-recorded `throughput.csv`.

use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

use cynosure::site_c::pool::LocalBufferPool;
use cynosure::site_c::queue::Queue;
use cynosure::site_c::semaphore::LocalSemaphore;
use cynosure::site_d::bipbuffer::bip_buffer;
use cynosure::site_d::oneshot::oneshot;

/// Run `f` for `iters` operations and return millions of ops/second.
fn mops(iters: u64, mut f: impl FnMut()) -> f64 {
    // Warm caches, allocator and predictors before the timed region.
    for _ in 0..(iters / 10).max(1_000) {
        f();
    }
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    iters as f64 / t.elapsed().as_secs_f64() / 1e6
}

/// Best of three — the cleanest run is the one least perturbed by the OS.
fn best(iters: u64, mut f: impl FnMut()) -> f64 {
    let mut top = 0.0f64;
    for _ in 0..3 {
        top = top.max(mops(iters, &mut f));
    }
    top
}

fn main() {
    let mut rows: Vec<String> = Vec::new();
    let mut row = |chart: &str, label: &str, v: f64, hero: bool| {
        println!("  {chart:<22} {label:<28} {v:8.1} Mops/s");
        rows.push(format!("{chart},{label},{v:.2},{}", u8::from(hero)));
    };

    println!("sustained throughput (best of 3):");

    // ---- oneshot: create + send + receive, repeatedly ----
    row(
        "oneshot-throughput",
        "cynosure oneshot",
        best(2_000_000, || {
            let (tx, mut rx) = oneshot::<u64>();
            tx.send(black_box(7)).unwrap();
            black_box(rx.try_recv().unwrap());
        }),
        true,
    );
    row(
        "oneshot-throughput",
        "tokio",
        best(2_000_000, || {
            let (tx, mut rx) = tokio::sync::oneshot::channel::<u64>();
            tx.send(black_box(7)).unwrap();
            black_box(rx.try_recv().unwrap());
        }),
        false,
    );
    row(
        "oneshot-throughput",
        "futures-channel",
        best(2_000_000, || {
            let (tx, mut rx) = futures::channel::oneshot::channel::<u64>();
            tx.send(black_box(7)).unwrap();
            black_box(rx.try_recv().unwrap().unwrap());
        }),
        false,
    );

    // ---- bip_buffer: reserve + commit + read + release, 256 B grants ----
    {
        let (mut p, mut c) = bip_buffer(64 * 1024);
        row(
            "bipbuffer-throughput",
            "cynosure bip_buffer",
            best(2_000_000, || {
                let mut g = p.try_reserve(256).unwrap();
                g.as_mut_slice()[0] = 1;
                g.commit(256);
                let r = c.try_read().unwrap();
                let n = r.len();
                black_box(r.as_slice()[0]);
                r.release(n);
            }),
            true,
        );
    }
    {
        use bbqueue::BBBuffer;
        static BB: BBBuffer<65536> = BBBuffer::new();
        let (mut p, mut c) = BB.try_split().unwrap();
        row(
            "bipbuffer-throughput",
            "bbqueue",
            best(2_000_000, || {
                let mut g = p.grant_exact(256).unwrap();
                g.buf()[0] = 1;
                g.commit(256);
                let r = c.read().unwrap();
                let n = r.len();
                black_box(r.buf()[0]);
                r.release(n);
            }),
            false,
        );
    }

    // ---- LocalSemaphore: acquire + release ----
    {
        let sem = LocalSemaphore::new(64);
        row(
            "semaphore-throughput",
            "cynosure LocalSemaphore",
            best(5_000_000, || {
                let p = sem.try_acquire().unwrap();
                black_box(&p);
            }),
            true,
        );
    }
    {
        let sem = tokio::sync::Semaphore::new(64);
        row(
            "semaphore-throughput",
            "tokio",
            best(5_000_000, || {
                let p = sem.try_acquire().unwrap();
                black_box(&p);
            }),
            false,
        );
    }
    {
        let sem = async_lock::Semaphore::new(64);
        row(
            "semaphore-throughput",
            "async-lock",
            best(5_000_000, || {
                let p = sem.try_acquire().unwrap();
                black_box(&p);
            }),
            false,
        );
    }

    // ---- LocalBufferPool: acquire + return, 4 KiB ----
    {
        let pool = LocalBufferPool::<u8>::new(16, 4096);
        row(
            "pool-throughput",
            "cynosure LocalBufferPool",
            best(3_000_000, || {
                let b = pool.try_acquire().unwrap();
                black_box(&b);
            }),
            true,
        );
    }
    {
        let pool = object_pool::Pool::new(16, || vec![0u8; 4096]);
        row(
            "pool-throughput",
            "object-pool",
            best(3_000_000, || {
                let b = pool.try_pull().unwrap();
                black_box(&b);
            }),
            false,
        );
    }
    {
        let pool = lockfree_object_pool::LinearObjectPool::new(|| vec![0u8; 4096], |_| {});
        row(
            "pool-throughput",
            "lockfree-object-pool",
            best(3_000_000, || {
                let b = pool.pull();
                black_box(&b);
            }),
            false,
        );
    }

    // ---- Queue: sustained push+pop while the data stays inline ----
    {
        let mut q: Queue<u64, 8> = Queue::new();
        row(
            "queue-throughput",
            "cynosure Queue",
            best(10_000_000, || {
                q.push_back(black_box(1));
                black_box(q.pop_front());
            }),
            true,
        );
    }
    {
        let mut q: std::collections::VecDeque<u64> = std::collections::VecDeque::with_capacity(8);
        row(
            "queue-throughput",
            "VecDeque",
            best(10_000_000, || {
                q.push_back(black_box(1));
                black_box(q.pop_front());
            }),
            false,
        );
    }

    // ---- LocalMutex / LocalRwLock: sustained lock + unlock ----
    {
        use cynosure::site_c::mutex::LocalMutex;
        let m = LocalMutex::new(0u64);
        row(
            "mutex-throughput",
            "cynosure LocalMutex",
            best(10_000_000, || {
                let mut g = m.try_lock().unwrap();
                *g += black_box(1);
                black_box(*g);
            }),
            true,
        );
    }
    {
        let m = parking_lot::Mutex::new(0u64);
        row(
            "mutex-throughput",
            "parking_lot",
            best(10_000_000, || {
                let mut g = m.lock();
                *g += black_box(1);
                black_box(*g);
            }),
            false,
        );
    }
    {
        let m = std::sync::Mutex::new(0u64);
        row(
            "mutex-throughput",
            "std::sync::Mutex",
            best(10_000_000, || {
                let mut g = m.lock().unwrap();
                *g += black_box(1);
                black_box(*g);
            }),
            false,
        );
    }
    {
        use cynosure::site_c::rwlock::LocalRwLock;
        let l = LocalRwLock::new(0u64);
        row(
            "rwlock-throughput",
            "cynosure LocalRwLock",
            best(10_000_000, || {
                let g = l.try_read().unwrap();
                black_box(*g);
            }),
            true,
        );
    }
    {
        let l = parking_lot::RwLock::new(0u64);
        row(
            "rwlock-throughput",
            "parking_lot",
            best(10_000_000, || {
                let g = l.read();
                black_box(*g);
            }),
            false,
        );
    }
    {
        let l = std::sync::RwLock::new(0u64);
        row(
            "rwlock-throughput",
            "std::sync::RwLock",
            best(10_000_000, || {
                let g = l.read().unwrap();
                black_box(*g);
            }),
            false,
        );
    }

    // ---- emit ----
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/bench-data");
    fs::create_dir_all(&dir).expect("create docs/bench-data");
    let mut out = String::from(
        "# Sustained throughput, written by `cargo bench --bench primitives_throughput`.\n\
         # A separate measurement from the latency charts, not latency inverted.\n\
         # Columns: chart,label,value,hero\n",
    );
    for (chart, title, sub) in [
        (
            "oneshot-throughput",
            "oneshot — sustained create + send + receive",
            "steady-state channel churn · allocator-bound · higher is better",
        ),
        (
            "bipbuffer-throughput",
            "bip_buffer — sustained grant cycles",
            "reserve + commit + read + release · 256 B contiguous grants · higher is better",
        ),
        (
            "semaphore-throughput",
            "LocalSemaphore — sustained acquire + release",
            "uncontended · non-atomic single-core · higher is better",
        ),
        (
            "pool-throughput",
            "LocalBufferPool — sustained acquire + return",
            "4 KiB buffers recycled through the free list · higher is better",
        ),
        (
            "queue-throughput",
            "Queue<T, N> — sustained push + pop, warm queue",
            "steady state on a pre-sized queue — no allocation on either side · higher is better",
        ),
        (
            "mutex-throughput",
            "LocalMutex — sustained lock + unlock",
            "single-threaded, non-atomic · usable across .await · higher is better",
        ),
        (
            "rwlock-throughput",
            "LocalRwLock — sustained read lock + unlock",
            "single-threaded, non-atomic · write-preferring · higher is better",
        ),
    ] {
        out.push_str(&format!(
            "#title:{chart}:{title}\n#subtitle:{chart}:{sub}\n#unit:{chart}:Mops/s\n"
        ));
    }
    for r in &rows {
        out.push_str(r);
        out.push('\n');
    }
    fs::write(dir.join("throughput-measured.csv"), out).expect("write csv");
    println!(
        "\nwrote docs/bench-data/throughput-measured.csv ({} rows)",
        rows.len()
    );
}
