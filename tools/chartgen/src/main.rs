//! Renders cynosure's benchmark results as branded SVG charts.
//!
//! ```text
//! cargo bench                                    # produces docs/bench-data/*.csv
//! cargo run --manifest-path tools/chartgen/Cargo.toml
//! ```
//!
//! Input lives in `docs/bench-data/` and is written by the committed benches,
//! so a chart is never hand-edited — re-run the benches and re-run this.
//!
//! * `throughput.csv` — `chart,label,value,hero` plus `#title:`/`#subtitle:`/
//!   `#unit:` directives per chart id.
//! * `latency.csv` — `chart,series,percentile,ns` plus `#title:`/`#subtitle:`.

mod charts;
mod render;
mod svg;
mod theme;

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use charts::{Bar, Better, Series};

/// Per-chart metadata declared by `#key:chart:value` directive rows.
#[derive(Default)]
struct Meta {
    title: String,
    subtitle: String,
    unit: String,
    /// `#better:<chart>:lower` for latency charts; defaults to
    /// higher-is-better.
    lower_is_better: bool,
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <repo>/tools/chartgen.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("tools/chartgen lives two levels below the repo root")
        .to_path_buf()
}

/// Split a CSV line, trimming each field. Values never contain commas.
fn fields(line: &str) -> Vec<&str> {
    line.split(',').map(str::trim).collect()
}

/// Read directives (`#key:chart:value`) and data rows from a CSV.
fn load(path: &Path) -> (BTreeMap<String, Meta>, Vec<Vec<String>>) {
    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut meta: BTreeMap<String, Meta> = BTreeMap::new();
    let mut rows = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            let mut it = rest.splitn(3, ':');
            let (Some(key), Some(chart), Some(val)) = (it.next(), it.next(), it.next()) else {
                continue; // a plain comment
            };
            let m = meta.entry(chart.trim().to_string()).or_default();
            match key.trim() {
                "title" => m.title = val.trim().to_string(),
                "subtitle" => m.subtitle = val.trim().to_string(),
                "unit" => m.unit = val.trim().to_string(),
                "better" => m.lower_is_better = val.trim().eq_ignore_ascii_case("lower"),
                _ => {}
            }
            continue;
        }
        rows.push(fields(line).into_iter().map(str::to_string).collect());
    }
    (meta, rows)
}

fn main() {
    let root = repo_root();
    let data = root.join("docs/bench-data");
    let out = root.join("docs/charts");
    fs::create_dir_all(&out).expect("create docs/charts");
    let mut written = 0usize;

    // ---- throughput / latency bar charts ----
    // Every `throughput*.csv` is merged: `throughput.csv` is curated from the
    // criterion suites, `throughput-measured.csv` is emitted directly by
    // `cargo bench --bench primitives_throughput`.
    let mut sources: Vec<PathBuf> = fs::read_dir(&data)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("throughput") && n.ends_with(".csv"))
                })
                .collect()
        })
        .unwrap_or_default();
    sources.sort();
    if !sources.is_empty() {
        let mut meta: BTreeMap<String, Meta> = BTreeMap::new();
        let mut rows: Vec<Vec<String>> = Vec::new();
        for p in &sources {
            let (m, r) = load(p);
            meta.extend(m);
            rows.extend(r);
        }
        let mut per_chart: BTreeMap<String, Vec<Bar>> = BTreeMap::new();
        for r in rows {
            assert!(r.len() >= 4, "throughput csv: expected 4 fields, got {r:?}");
            per_chart.entry(r[0].clone()).or_default().push(Bar {
                label: r[1].clone(),
                value: r[2].parse().unwrap_or_else(|e| panic!("{:?}: {e}", r[2])),
                hero: r[3] == "1" || r[3].eq_ignore_ascii_case("true"),
            });
        }
        for (chart, mut bars) in per_chart {
            let m = meta.get(&chart).cloned_or_default(&chart);
            // Best first, whichever direction "best" runs in.
            if m.lower_is_better {
                bars.sort_by(|a, b| a.value.total_cmp(&b.value));
            } else {
                bars.sort_by(|a, b| b.value.total_cmp(&a.value));
            }
            let better = if m.lower_is_better {
                Better::Lower
            } else {
                Better::Higher
            };
            let svg = charts::bars_chart(&m.title, &m.subtitle, &m.unit, &bars, better);
            emit(
                &out,
                &chart,
                &svg,
                &format!("{} bars", bars.len()),
                &mut written,
            );
        }
    }

    // ---- latency distribution charts ----
    // Every `latency*.csv`: `latency.csv` from the control-plane bench,
    // `latency-primitives.csv` from `cargo bench --bench latency_dist`.
    let mut lat_sources: Vec<PathBuf> = fs::read_dir(&data)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("latency") && n.ends_with(".csv"))
                })
                .collect()
        })
        .unwrap_or_default();
    lat_sources.sort();
    if !lat_sources.is_empty() {
        let mut meta: BTreeMap<String, Meta> = BTreeMap::new();
        let mut rows: Vec<Vec<String>> = Vec::new();
        for p in &lat_sources {
            let (m, r) = load(p);
            meta.extend(m);
            rows.extend(r);
        }
        // chart -> series -> points, preserving first-seen series order.
        let mut per_chart: BTreeMap<String, Vec<Series>> = BTreeMap::new();
        for r in rows {
            assert!(r.len() >= 4, "latency csv: expected 4 fields, got {r:?}");
            let list = per_chart.entry(r[0].clone()).or_default();
            let idx = match list.iter().position(|s| s.name == r[1]) {
                Some(i) => i,
                None => {
                    list.push(Series {
                        name: r[1].clone(),
                        points: Vec::new(),
                    });
                    list.len() - 1
                }
            };
            let p: f64 = r[2].parse().unwrap_or_else(|e| panic!("{:?}: {e}", r[2]));
            let ns: f64 = r[3].parse().unwrap_or_else(|e| panic!("{:?}: {e}", r[3]));
            list[idx].points.push((p, ns));
        }
        for (chart, mut series) in per_chart {
            for s in &mut series {
                s.points.sort_by(|a, b| a.0.total_cmp(&b.0));
            }
            let m = meta.get(&chart).cloned_or_default(&chart);
            let svg = charts::latency_percentiles(&m.title, &m.subtitle, &series);
            emit(
                &out,
                &chart,
                &svg,
                &format!("{} series", series.len()),
                &mut written,
            );
        }
    }

    if written == 0 {
        eprintln!(
            "no input found in {} — run `cargo bench` first",
            data.display()
        );
        std::process::exit(1);
    }
    println!("{written} chart(s) written to {}", out.display());
}

/// Write `<chart>.svg` and its 4x `<chart>.png`.
fn emit(out: &Path, chart: &str, svg: &str, note: &str, written: &mut usize) {
    let svg_path = out.join(format!("{chart}.svg"));
    fs::write(&svg_path, svg).expect("write svg");
    let png_path = out.join(format!("{chart}.png"));
    match render::png(svg, &png_path) {
        Ok((w, h)) => println!("  {chart:<28} {note:<12} svg + png {w}x{h}"),
        Err(e) => eprintln!("  {chart:<28} {note:<12} svg only — png failed: {e}"),
    }
    *written += 1;
}

/// Small helper so a chart without directives still renders with its id.
trait MetaExt {
    fn cloned_or_default(&self, chart: &str) -> Meta;
}
impl MetaExt for Option<&Meta> {
    fn cloned_or_default(&self, chart: &str) -> Meta {
        match self {
            Some(m) => Meta {
                title: if m.title.is_empty() {
                    chart.to_string()
                } else {
                    m.title.clone()
                },
                subtitle: m.subtitle.clone(),
                unit: m.unit.clone(),
                lower_is_better: m.lower_is_better,
            },
            None => Meta {
                title: chart.to_string(),
                ..Default::default()
            },
        }
    }
}
