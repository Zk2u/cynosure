//! Real-world control-plane benchmark: paced traffic, parked consumers, and
//! co-located work — the regime a control channel actually lives in, as opposed
//! to saturated spin-loop throughput suites.
//!
//! Scenarios (vs kanal-async and tokio mpsc):
//!   A. Command dispatch: a message every ~20 µs to an idle, parked consumer
//!      (actor waiting for orders). Reports wakeup latency p50/p99/p999.
//!   B. Paced fan-in: 8 producers each sending at ~1 µs intervals (service
//!      rates, queue stays shallow). Reports end-to-end latency percentiles.
//!   C. Citizenship: the consumer shares a single-threaded executor with
//!      compute tasks; an external producer sends paced commands. Reports
//!      message latency AND how much co-located work still gets done — the
//!      cost the channel imposes on its neighbors.

use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use cynosure::site_d::mpsc_light;

// ---------------------------------------------------------------------------
// Minimal thread-park executor (Arc-refcount waker, like a real runtime's).
// ---------------------------------------------------------------------------
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

/// Busy-wait pacing (sleep() jitter at µs scale would swamp the measurement).
#[inline]
fn pace_until(deadline: Instant) {
    while Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

fn percentiles(mut ns: Vec<u64>) -> (u64, u64, u64) {
    ns.sort_unstable();
    let p = |q: f64| ns[((ns.len() as f64 - 1.0) * q) as usize];
    (p(0.50), p(0.99), p(0.999))
}

/// Percentiles the distribution chart plots (`docs/bench-data/latency.csv`).
const SWEEP: [f64; 8] = [0.50, 0.75, 0.90, 0.99, 0.999, 0.9999, 0.99999, 0.999999];

/// Rows accumulated across scenarios, flushed once at exit.
static ROWS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Record a full percentile sweep for `chart`/`series`, for chartgen to plot.
fn record(chart: &str, series: &str, lat: &[u64]) {
    let mut v = lat.to_vec();
    v.sort_unstable();
    let mut rows = ROWS.lock().unwrap();
    for &q in &SWEEP {
        // Skip percentiles the sample size can't support (a p99.999 from 3k
        // samples is just the max, not an estimate).
        if (v.len() as f64) * (1.0 - q) < 1.0 {
            continue;
        }
        let ns = v[((v.len() as f64 - 1.0) * q) as usize];
        rows.push(format!("{chart},{series},{q},{ns}"));
    }
}

/// Write `docs/bench-data/latency.csv` with directives chartgen understands.
fn flush_csv() {
    let rows = ROWS.lock().unwrap();
    if rows.is_empty() {
        return;
    }
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/bench-data");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut out = String::from(
        "# Latency distributions, written by `cargo bench --bench control_plane`.\n\
         # Regenerate charts: cargo run --manifest-path tools/chartgen/Cargo.toml\n\
         # Columns: chart,series,percentile,ns\n",
    );
    out.push_str(
        "#title:mpsc-light-latency-dist:mpsc_light — latency under paced fan-in\n\
         #subtitle:mpsc-light-latency-dist:8 producers @1 µs into 1 consumer · capacity 256 · lower and flatter is better\n\
         #title:mpsc-light-reactor-dist:mpsc_light — delivery latency inside a shared reactor\n\
         #subtitle:mpsc-light-reactor-dist:consumer sharing a single-threaded executor with 4 compute tasks · lower and flatter is better\n",
    );
    for r in rows.iter() {
        out.push_str(r);
        out.push('\n');
    }
    let _ = fs::write(dir.join("latency.csv"), out);
}

// ---------------------------------------------------------------------------
// Contender adapters: unify send/recv over an Instant payload.
// ---------------------------------------------------------------------------
trait Chan: Sized {
    type Tx: ChanTx;
    type Rx: ChanRx;
    fn make(cap: usize) -> (Self::Tx, Self::Rx);
    const NAME: &'static str;
}
trait ChanTx: Clone + Send + 'static {
    fn try_send_t(&self, v: Instant) -> bool;
}
trait ChanRx: Send + 'static {
    fn recv_t(&mut self) -> impl Future<Output = Option<Instant>> + '_;
}

struct Cyn;
impl Chan for Cyn {
    type Tx = mpsc_light::Sender<Instant>;
    type Rx = mpsc_light::Receiver<Instant>;
    fn make(cap: usize) -> (Self::Tx, Self::Rx) {
        mpsc_light::bounded(cap)
    }
    const NAME: &'static str = "cynosure-light";
}
impl ChanTx for mpsc_light::Sender<Instant> {
    fn try_send_t(&self, v: Instant) -> bool {
        self.try_send(v).is_ok()
    }
}
impl ChanRx for mpsc_light::Receiver<Instant> {
    async fn recv_t(&mut self) -> Option<Instant> {
        self.recv().await
    }
}

struct Kanal;
impl Chan for Kanal {
    type Tx = kanal::AsyncSender<Instant>;
    type Rx = kanal::AsyncReceiver<Instant>;
    fn make(cap: usize) -> (Self::Tx, Self::Rx) {
        kanal::bounded_async(cap)
    }
    const NAME: &'static str = "kanal-async";
}
impl ChanTx for kanal::AsyncSender<Instant> {
    fn try_send_t(&self, v: Instant) -> bool {
        self.try_send(v).unwrap_or(false)
    }
}
impl ChanRx for kanal::AsyncReceiver<Instant> {
    async fn recv_t(&mut self) -> Option<Instant> {
        self.recv().await.ok()
    }
}

struct Tokio;
impl Chan for Tokio {
    type Tx = tokio::sync::mpsc::Sender<Instant>;
    type Rx = tokio::sync::mpsc::Receiver<Instant>;
    fn make(cap: usize) -> (Self::Tx, Self::Rx) {
        tokio::sync::mpsc::channel(cap)
    }
    const NAME: &'static str = "tokio-mpsc";
}
impl ChanTx for tokio::sync::mpsc::Sender<Instant> {
    fn try_send_t(&self, v: Instant) -> bool {
        self.try_send(v).is_ok()
    }
}
impl ChanRx for tokio::sync::mpsc::Receiver<Instant> {
    async fn recv_t(&mut self) -> Option<Instant> {
        self.recv().await
    }
}

// ---------------------------------------------------------------------------
// A: command dispatch to a parked consumer (one message every ~20 µs).
// ---------------------------------------------------------------------------
fn scenario_a<C: Chan>() {
    const MSGS: usize = 4000;
    const GAP: Duration = Duration::from_micros(20);

    let (tx, mut rx) = C::make(64);
    let consumer = thread::spawn(move || {
        park_on(async move {
            let mut lat = Vec::with_capacity(MSGS);
            for _ in 0..MSGS {
                let sent = rx.recv_t().await.unwrap();
                lat.push(sent.elapsed().as_nanos() as u64);
            }
            lat
        })
    });

    let start = Instant::now();
    for i in 0..MSGS {
        pace_until(start + GAP * (i as u32 + 1));
        // Paced and shallow: the channel is never full.
        assert!(tx.try_send_t(Instant::now()));
    }
    let lat = consumer.join().unwrap();
    let (p50, p99, p999) = percentiles(lat);
    println!(
        "A dispatch-latency  {:16}  p50 {:6} ns  p99 {:7} ns  p99.9 {:7} ns",
        C::NAME,
        p50,
        p99,
        p999
    );
}

// ---------------------------------------------------------------------------
// B: paced fan-in — 8 producers at ~1 µs each (aggregate ~8 M/s, shallow queue).
// ---------------------------------------------------------------------------
fn scenario_b<C: Chan>() {
    const PRODUCERS: usize = 8;
    const PER: usize = 30_000;
    const GAP: Duration = Duration::from_micros(1);

    let (tx, mut rx) = C::make(256);
    let producers: Vec<_> = (0..PRODUCERS)
        .map(|_| {
            let tx = tx.clone();
            thread::spawn(move || {
                let start = Instant::now();
                for i in 0..PER {
                    pace_until(start + GAP * (i as u32 + 1));
                    while !tx.try_send_t(Instant::now()) {
                        std::hint::spin_loop(); // rare: queue is shallow at this rate
                    }
                }
            })
        })
        .collect();
    drop(tx);

    let lat = park_on(async move {
        let mut lat = Vec::with_capacity(PRODUCERS * PER);
        for _ in 0..PRODUCERS * PER {
            let sent = rx.recv_t().await.unwrap();
            lat.push(sent.elapsed().as_nanos() as u64);
        }
        lat
    });
    for p in producers {
        p.join().unwrap();
    }
    record("mpsc-light-latency-dist", C::NAME, &lat);
    let (p50, p99, p999) = percentiles(lat);
    println!(
        "B paced-fanin(8x1µs) {:15}  p50 {:6} ns  p99 {:7} ns  p99.9 {:7} ns",
        C::NAME,
        p50,
        p99,
        p999
    );
}

// ---------------------------------------------------------------------------
// C: citizenship — consumer shares a single-threaded executor with compute
// tasks; paced external producer. Latency + co-located work throughput.
// ---------------------------------------------------------------------------
fn scenario_c<C: Chan>() {
    use futures::executor::LocalPool;
    use futures::task::LocalSpawnExt;
    use std::cell::Cell;
    use std::rc::Rc;

    const MSGS: usize = 3000;
    const GAP: Duration = Duration::from_micros(5);
    const NEIGHBOR_TASKS: usize = 4;

    /// Cooperative yield: wake immediately, return Pending once.
    struct YieldOnce(bool);
    impl Future for YieldOnce {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    let (tx, mut rx) = C::make(64);
    let producer = thread::spawn(move || {
        let start = Instant::now();
        for i in 0..MSGS {
            pace_until(start + GAP * (i as u32 + 1));
            while !tx.try_send_t(Instant::now()) {
                std::hint::spin_loop();
            }
        }
    });

    let mut pool = LocalPool::new();
    let spawner = pool.spawner();
    let done = Rc::new(Cell::new(false));
    let work = Rc::new(Cell::new(0u64));
    for _ in 0..NEIGHBOR_TASKS {
        let done = done.clone();
        let work = work.clone();
        spawner
            .spawn_local(async move {
                while !done.get() {
                    work.set(work.get() + 1); // one unit of co-located progress
                    YieldOnce(false).await;
                }
            })
            .unwrap();
    }

    let done2 = done.clone();
    let lat = pool.run_until(async move {
        let mut lat = Vec::with_capacity(MSGS);
        for _ in 0..MSGS {
            let sent = rx.recv_t().await.unwrap();
            lat.push(sent.elapsed().as_nanos() as u64);
        }
        done2.set(true);
        lat
    });
    producer.join().unwrap();

    record("mpsc-light-reactor-dist", C::NAME, &lat);
    let (p50, p99, _) = percentiles(lat);
    let window_ms = (MSGS as u64 * GAP.as_nanos() as u64) as f64 / 1e6;
    println!(
        "C shared-reactor    {:16}  p50 {:6} ns  p99 {:7} ns  neighbor-work {:7.0} iters/ms",
        C::NAME,
        p50,
        p99,
        work.get() as f64 / window_ms
    );
}

fn main() {
    println!("control-plane benchmark: paced traffic, parked consumers, shared reactors");
    println!("(lower latency better; higher neighbor-work better)\n");
    scenario_a::<Cyn>();
    scenario_a::<Kanal>();
    scenario_a::<Tokio>();
    println!();
    scenario_b::<Cyn>();
    scenario_b::<Kanal>();
    scenario_b::<Tokio>();
    println!();
    scenario_c::<Cyn>();
    scenario_c::<Kanal>();
    scenario_c::<Tokio>();

    flush_csv();
    println!("\nwrote docs/bench-data/latency.csv");
}
