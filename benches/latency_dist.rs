//! Latency **distributions** for every primitive — a single mean hides the tail
//! that actually decides whether a primitive is usable.
//!
//! Two sampling modes, because the operations span six orders of magnitude:
//!
//! * **Direct** — cross-thread handoffs (µs scale) are timed per message, so
//!   these are true per-operation latencies including every scheduler and
//!   cache-coherency hiccup.
//! * **Batched** — a `LocalMutex` lock is ~0.3 ns, far under `Instant::now()`'s
//!   own overhead (tens of ns), so timing one op is impossible. Instead each
//!   sample times a batch of `BATCH` operations and divides. The result is a
//!   distribution of *per-op cost averaged over a batch*: it still exposes
//!   allocator hiccups, page faults and preemption (the things that make a
//!   tail), but it cannot show a single unlucky operation. Charts say which
//!   mode was used.
//!
//! Emits `docs/bench-data/latency-primitives.csv`.

use std::{fs, hint::black_box, path::PathBuf, thread, time::Instant};

use cynosure::{
    site_c::{
        mutex::LocalMutex, pool::LocalBufferPool, queue::Queue, rwlock::LocalRwLock,
        semaphore::LocalSemaphore,
    },
    site_d::{
        bipbuffer::bip_buffer,
        oneshot::oneshot,
        ringbuf::RingBuf,
        triplebuffer::{AlignedBuffer, triple_buffer},
    },
};

/// Samples per series — enough to resolve p99.9.
const SAMPLES: usize = 4000;
/// Operations per batched sample.
const BATCH: usize = 512;
/// Percentiles plotted for *directly* timed series (true per-op latencies).
const SWEEP_DIRECT: [f64; 7] = [0.50, 0.75, 0.90, 0.99, 0.999, 0.9999, 0.99999];
/// Percentiles plotted for *batched* series. Capped at p99 deliberately:
/// beyond it the batch means are dominated by OS preemption, not the primitive.
/// Measured — the per-batch excess at p99.9 lands at ~6-7 µs (one context
/// switch) for *every* implementation, and which series catches one is a coin
/// flip (cynosure's mutex showed 5.8 µs while parking_lot showed 0.17 µs; on
/// the rwlock the two swapped places). Plotting that would chart the scheduler.
const SWEEP_BATCHED: [f64; 4] = [0.50, 0.75, 0.90, 0.99];

/// Batched sampling: `SAMPLES` batches of `BATCH` ops, ns-per-op each.
fn batched(mut f: impl FnMut()) -> Vec<f64> {
    for _ in 0..BATCH * 20 {
        f(); // warm caches, allocator, predictors
    }
    let mut v = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let t = Instant::now();
        for _ in 0..BATCH {
            f();
        }
        v.push(t.elapsed().as_nanos() as f64 / BATCH as f64);
    }
    v
}

struct Out(Vec<String>);

impl Out {
    fn add(&mut self, chart: &str, series: &str, v: Vec<f64>) {
        self.push(chart, series, v, &SWEEP_BATCHED);
    }

    /// Directly-timed series: the tail is real, so plot all of it.
    fn add_direct(&mut self, chart: &str, series: &str, v: Vec<f64>) {
        self.push(chart, series, v, &SWEEP_DIRECT);
    }

    fn push(&mut self, chart: &str, series: &str, mut v: Vec<f64>, sweep: &[f64]) {
        v.sort_by(f64::total_cmp);
        let n = v.len() as f64;
        for &q in sweep {
            // Skip percentiles this sample count cannot support.
            if n * (1.0 - q) < 1.0 {
                continue;
            }
            let ns = v[((n - 1.0) * q) as usize];
            self.0.push(format!("{chart},{series},{q},{ns:.3}"));
        }
        let p = |q: f64| v[((n - 1.0) * q) as usize];
        println!(
            "  {chart:<24} {series:<26} p50 {:9.2}  p99 {:9.2}  p99.9 {:9.2} ns",
            p(0.50),
            p(0.99),
            p(0.999)
        );
    }
}

fn main() {
    let mut o = Out(Vec::new());
    println!("latency distributions ({SAMPLES} samples):");

    // ── RingBuf: true per-message cross-thread round trip ───────────────────
    {
        let rb = RingBuf::<u64>::new(1024);
        let (mut p, mut c) = rb.split();
        let rb2 = RingBuf::<u64>::new(1024);
        let (mut p2, mut c2) = rb2.split();
        let peer = thread::spawn(move || {
            for _ in 0..SAMPLES + 1000 {
                loop {
                    if let Some(v) = c.try_pop() {
                        while p2.try_push(v).is_err() {
                            std::hint::spin_loop();
                        }
                        break;
                    }
                    std::hint::spin_loop();
                }
            }
        });
        let mut v = Vec::with_capacity(SAMPLES);
        for i in 0..SAMPLES + 1000 {
            let t = Instant::now();
            while p.try_push(i as u64).is_err() {
                std::hint::spin_loop();
            }
            loop {
                if c2.try_pop().is_some() {
                    break;
                }
                std::hint::spin_loop();
            }
            // Discard the warmup prefix.
            if i >= 1000 {
                v.push(t.elapsed().as_nanos() as f64 / 2.0); // one-way
            }
        }
        peer.join().unwrap();
        o.add_direct("ringbuf-latency-dist", "cynosure RingBuf", v);
    }
    {
        let (tx, rx) = crossbeam_channel::bounded::<u64>(1024);
        let (tx2, rx2) = crossbeam_channel::bounded::<u64>(1024);
        let peer = thread::spawn(move || {
            for _ in 0..SAMPLES + 1000 {
                if let Ok(v) = rx.recv() {
                    let _ = tx2.send(v);
                }
            }
        });
        let mut v = Vec::with_capacity(SAMPLES);
        for i in 0..SAMPLES + 1000 {
            let t = Instant::now();
            tx.send(i as u64).unwrap();
            rx2.recv().unwrap();
            if i >= 1000 {
                v.push(t.elapsed().as_nanos() as f64 / 2.0);
            }
        }
        peer.join().unwrap();
        o.add_direct("ringbuf-latency-dist", "crossbeam", v);
    }

    // ── oneshot ─────────────────────────────────────────────────────────────
    o.add(
        "oneshot-latency-dist",
        "cynosure oneshot",
        batched(|| {
            let (tx, mut rx) = oneshot::<u64>();
            tx.send(black_box(7)).unwrap();
            black_box(rx.try_recv().unwrap());
        }),
    );
    o.add(
        "oneshot-latency-dist",
        "tokio",
        batched(|| {
            let (tx, mut rx) = tokio::sync::oneshot::channel::<u64>();
            tx.send(black_box(7)).unwrap();
            black_box(rx.try_recv().unwrap());
        }),
    );
    o.add(
        "oneshot-latency-dist",
        "futures-channel",
        batched(|| {
            let (tx, mut rx) = futures::channel::oneshot::channel::<u64>();
            tx.send(black_box(7)).unwrap();
            black_box(rx.try_recv().unwrap().unwrap());
        }),
    );

    // ── bip_buffer ──────────────────────────────────────────────────────────
    {
        let (mut p, mut c) = bip_buffer(64 * 1024);
        o.add(
            "bipbuffer-latency-dist",
            "cynosure bip_buffer",
            batched(|| {
                let mut g = p.try_reserve(256).unwrap();
                g.as_mut_slice()[0] = 1;
                g.commit(256);
                let r = c.try_read().unwrap();
                let n = r.len();
                black_box(r.as_slice()[0]);
                r.release(n);
            }),
        );
    }
    {
        use bbqueue::BBBuffer;
        static BB: BBBuffer<65536> = BBBuffer::new();
        let (mut p, mut c) = BB.try_split().unwrap();
        o.add(
            "bipbuffer-latency-dist",
            "bbqueue",
            batched(|| {
                let mut g = p.grant_exact(256).unwrap();
                g.buf()[0] = 1;
                g.commit(256);
                let r = c.read().unwrap();
                let n = r.len();
                black_box(r.buf()[0]);
                r.release(n);
            }),
        );
    }

    // ── triple_buffer: publish + next rotation ──────────────────────────────
    {
        let (mut w, mut r, wbuf0) = triple_buffer::<u8>(4096);
        let mut wbuf = Some(wbuf0);
        let mut prev: Option<AlignedBuffer<u8>> = None;
        o.add(
            "triplebuffer-latency-dist",
            "cynosure triple_buffer",
            batched(|| {
                let buf = wbuf.take().unwrap();
                wbuf = Some(w.try_publish(buf).expect("middle free"));
                prev = Some(r.try_next(prev.take()).expect("unread available"));
            }),
        );
    }
    {
        let (full_tx, full_rx) = crossbeam_channel::bounded::<Vec<u8>>(1);
        let (empty_tx, empty_rx) = crossbeam_channel::bounded::<Vec<u8>>(3);
        for _ in 0..3 {
            empty_tx.send(vec![0u8; 4096]).unwrap();
        }
        o.add(
            "triplebuffer-latency-dist",
            "crossbeam-recycle",
            batched(|| {
                let buf = empty_rx.recv().unwrap();
                full_tx.send(buf).unwrap();
                let buf = full_rx.recv().unwrap();
                empty_tx.send(buf).unwrap();
            }),
        );
    }

    // ── LocalMutex ──────────────────────────────────────────────────────────
    {
        let m = LocalMutex::new(0u64);
        o.add(
            "mutex-latency-dist",
            "cynosure LocalMutex",
            batched(|| {
                let mut g = m.try_lock().unwrap();
                *g += black_box(1);
                black_box(*g);
            }),
        );
    }
    {
        let m = parking_lot::Mutex::new(0u64);
        o.add(
            "mutex-latency-dist",
            "parking_lot",
            batched(|| {
                let mut g = m.lock();
                *g += black_box(1);
                black_box(*g);
            }),
        );
    }
    {
        let m = std::sync::Mutex::new(0u64);
        o.add(
            "mutex-latency-dist",
            "std::sync::Mutex",
            batched(|| {
                let mut g = m.lock().unwrap();
                *g += black_box(1);
                black_box(*g);
            }),
        );
    }

    // ── LocalRwLock ─────────────────────────────────────────────────────────
    {
        let l = LocalRwLock::new(0u64);
        o.add(
            "rwlock-latency-dist",
            "cynosure LocalRwLock",
            batched(|| {
                let g = l.try_read().unwrap();
                black_box(*g);
            }),
        );
    }
    {
        let l = parking_lot::RwLock::new(0u64);
        o.add(
            "rwlock-latency-dist",
            "parking_lot",
            batched(|| {
                let g = l.read();
                black_box(*g);
            }),
        );
    }
    {
        let l = std::sync::RwLock::new(0u64);
        o.add(
            "rwlock-latency-dist",
            "std::sync::RwLock",
            batched(|| {
                let g = l.read().unwrap();
                black_box(*g);
            }),
        );
    }

    // ── LocalSemaphore ──────────────────────────────────────────────────────
    {
        let s = LocalSemaphore::new(64);
        o.add(
            "semaphore-latency-dist",
            "cynosure LocalSemaphore",
            batched(|| {
                let p = s.try_acquire().unwrap();
                black_box(&p);
            }),
        );
    }
    {
        let s = tokio::sync::Semaphore::new(64);
        o.add(
            "semaphore-latency-dist",
            "tokio",
            batched(|| {
                let p = s.try_acquire().unwrap();
                black_box(&p);
            }),
        );
    }
    {
        let s = async_lock::Semaphore::new(64);
        o.add(
            "semaphore-latency-dist",
            "async-lock",
            batched(|| {
                let p = s.try_acquire().unwrap();
                black_box(&p);
            }),
        );
    }

    // ── LocalBufferPool ─────────────────────────────────────────────────────
    {
        let pool = LocalBufferPool::<u8>::new(16, 4096);
        o.add(
            "pool-latency-dist",
            "cynosure LocalBufferPool",
            batched(|| {
                let b = pool.try_acquire().unwrap();
                black_box(&b);
            }),
        );
    }
    {
        let pool = object_pool::Pool::new(16, || vec![0u8; 4096]);
        o.add(
            "pool-latency-dist",
            "object-pool",
            batched(|| {
                let b = pool.try_pull().unwrap();
                black_box(&b);
            }),
        );
    }
    {
        let pool = lockfree_object_pool::LinearObjectPool::new(|| vec![0u8; 4096], |_| {});
        o.add(
            "pool-latency-dist",
            "lockfree-object-pool",
            batched(|| {
                let b = pool.pull();
                black_box(&b);
            }),
        );
    }

    // ── Queue ───────────────────────────────────────────────────────────────
    {
        let mut q: Queue<u64, 8> = Queue::new();
        o.add(
            "queue-latency-dist",
            "cynosure Queue",
            batched(|| {
                q.push_back(black_box(1));
                black_box(q.pop_front());
            }),
        );
    }
    {
        let mut q: std::collections::VecDeque<u64> = std::collections::VecDeque::with_capacity(8);
        o.add(
            "queue-latency-dist",
            "VecDeque",
            batched(|| {
                q.push_back(black_box(1));
                black_box(q.pop_front());
            }),
        );
    }

    // ── emit ────────────────────────────────────────────────────────────────
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/bench-data");
    fs::create_dir_all(&dir).expect("create docs/bench-data");
    let mut out = String::from(
        "# Latency distributions, written by `cargo bench --bench latency_dist`.\n\
         # Columns: chart,series,percentile,ns\n",
    );
    // Why p99 and not p99.9: see SWEEP_BATCHED.
    let batched_note = "per-op cost over batches of 512 · to p99 — past that the samples measure OS preemption, not the primitive";
    for (chart, title, sub) in [
        (
            "ringbuf-latency-dist",
            "RingBuf — cross-thread handoff latency",
            "true per-message one-way latency, 2 threads busy-spinning · lower and flatter is better",
        ),
        (
            "oneshot-latency-dist",
            "oneshot — create + send + receive",
            batched_note,
        ),
        (
            "triplebuffer-latency-dist",
            "triple_buffer — publish + next rotation",
            batched_note,
        ),
        (
            "bipbuffer-latency-dist",
            "bip_buffer — reserve + commit + read + release",
            batched_note,
        ),
        (
            "mutex-latency-dist",
            "LocalMutex — lock + unlock",
            batched_note,
        ),
        (
            "rwlock-latency-dist",
            "LocalRwLock — read lock + unlock",
            batched_note,
        ),
        (
            "semaphore-latency-dist",
            "LocalSemaphore — acquire + release",
            batched_note,
        ),
        (
            "pool-latency-dist",
            "LocalBufferPool — acquire + return",
            batched_note,
        ),
        (
            "queue-latency-dist",
            "Queue<T, N> — push + pop, warm queue",
            batched_note,
        ),
    ] {
        out.push_str(&format!(
            "#title:{chart}:{title}\n#subtitle:{chart}:{sub}\n"
        ));
    }
    for r in &o.0 {
        out.push_str(r);
        out.push('\n');
    }
    fs::write(dir.join("latency-primitives.csv"), out).expect("write csv");
    println!(
        "\nwrote docs/bench-data/latency-primitives.csv ({} rows)",
        o.0.len()
    );
}
