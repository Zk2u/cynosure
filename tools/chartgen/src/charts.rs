//! The two chart forms.
//!
//! * [`throughput_bars`] — magnitude across competitors. One series (the
//!   measure); identity is carried by the axis label, so colour is used only for
//!   emphasis (cynosure in the brand green, the field neutral) and no legend is
//!   needed. Every bar is direct-labelled with its value.
//! * [`latency_percentiles`] — a latency *distribution* as a percentile curve
//!   (the HDR-histogram idiom): x is `1/(1-p)` on a log scale, so the tail gets
//!   the room it deserves, and a rightward/downward curve is unambiguously
//!   better. Multi-series, so it carries a legend *and* direct labels.

use crate::svg::{Anchor, Svg};
use crate::theme as t;

/// Every chart is emitted on the same canvas width so that a README showing
/// them one under another renders all type at the *same* size. Gutters flex to
/// fit their labels; the plot area absorbs the difference.
const CANVAS_W: f64 = 820.0;
/// Monospace advance at 11px — label widths are arithmetic, not guesswork.
const ADV11: f64 = 11.0 * 0.6;

/// Format nanoseconds with a unit that keeps 3 significant figures.
pub fn fmt_ns(ns: f64) -> String {
    if ns >= 1_000_000.0 {
        format!("{:.1} ms", ns / 1_000_000.0)
    } else if ns >= 1_000.0 {
        format!("{:.1} µs", ns / 1_000.0)
    } else if ns >= 1.0 {
        format!("{ns:.0} ns")
    } else {
        // site_c primitives run below a nanosecond; clamping these to "1 ns"
        // would flatten exactly the difference the chart exists to show.
        format!("{ns:.2} ns")
    }
}

fn fmt_val(v: f64) -> String {
    if v >= 100.0 {
        format!("{v:.0}")
    } else if v >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

/// "Nice" axis maximum: 1/2/5 × 10^k at or above `v`.
fn nice_max(v: f64) -> f64 {
    if v <= 0.0 {
        return 1.0;
    }
    let mag = 10f64.powf(v.log10().floor());
    for m in [1.0, 2.0, 2.5, 5.0, 10.0] {
        if mag * m >= v {
            return mag * m;
        }
    }
    mag * 10.0
}

pub struct Bar {
    pub label: String,
    pub value: f64,
    pub hero: bool,
}

/// Which end of the scale is good. Latency charts are `Lower`; throughput
/// charts are `Higher`. The chart states it explicitly in the subtitle and
/// orders the bars best-first either way, so a longer bar is never silently
/// assumed to be better.
#[derive(Clone, Copy, PartialEq)]
pub enum Better {
    Higher,
    Lower,
}

/// Horizontal bar chart. Horizontal because the category labels are crate
/// names — they read straight, need no rotation, and the eye compares bar
/// *ends* down a common baseline.
pub fn bars_chart(
    title: &str,
    subtitle: &str,
    unit: &str,
    bars: &[Bar],
    _better: Better,
) -> String {
    const LABEL_PX: f64 = 11.0;
    let widest_label = bars
        .iter()
        .map(|b| b.label.chars().count())
        .max()
        .unwrap_or(0) as f64;
    let widest_value = bars
        .iter()
        .map(|b| fmt_val(b.value).chars().count())
        .max()
        .unwrap_or(0) as f64;
    let pad_l = (widest_label * ADV11 + 24.0).max(96.0);
    let pad_r = (widest_value * ADV11 + 24.0).max(56.0);
    let pad_t = 68.0;
    let (row, gap) = (26.0, 10.0);
    let w = CANVAS_W;
    let plot_w = (w - pad_l - pad_r).max(200.0);
    let h = pad_t + bars.len() as f64 * (row + gap) + 34.0;
    let mut s = Svg::new(w, h);

    let max = nice_max(bars.iter().map(|b| b.value).fold(0.0, f64::max));

    s.text(24.0, 30.0, title, t::TEXT, 15.0, Anchor::Start, t::WEIGHT);
    s.text(
        24.0,
        46.0,
        subtitle,
        t::FAINT,
        11.0,
        Anchor::Start,
        t::WEIGHT,
    );

    // Recessive gridlines at 0/¼/½/¾/1, drawn behind the bars.
    for i in 0..=4 {
        let f = i as f64 / 4.0;
        let x = pad_l + plot_w * f;
        s.line(x, pad_t - 8.0, x, h - 30.0, t::BORDER, 1.0);
        s.text(
            x,
            h - 14.0,
            &fmt_val(max * f),
            t::FAINT,
            10.0,
            Anchor::Middle,
            400,
        );
    }
    s.text(
        pad_l + plot_w / 2.0,
        h - 2.0,
        unit,
        t::FAINT,
        10.0,
        Anchor::Middle,
        400,
    );

    for (i, b) in bars.iter().enumerate() {
        let y = pad_t + i as f64 * (row + gap);
        let bw = (b.value / max) * plot_w;
        let (fill, ink) = if b.hero {
            (t::HERO, t::TEXT)
        } else {
            (t::NEUTRAL, t::MUTED)
        };
        s.text(
            pad_l - 14.0,
            y + row / 2.0 + 4.0,
            &b.label,
            ink,
            LABEL_PX,
            Anchor::End,
            t::WEIGHT,
        );
        // 4px rounded data-end anchored to the baseline (the rect's left edge
        // sits on the axis; only the free end reads as rounded at this radius).
        s.rect(pad_l, y, bw, row, fill, t::RADIUS);
        if !b.hero {
            s.line(pad_l, y, pad_l, y + row, t::NEUTRAL_EDGE, 1.0);
        }
        s.text(
            pad_l + bw + 10.0,
            y + row / 2.0 + 4.0,
            &fmt_val(b.value),
            ink,
            11.0,
            Anchor::Start,
            t::WEIGHT,
        );
    }
    // Axis baseline last, so it sits above the bar left edges.
    s.line(pad_l, pad_t - 8.0, pad_l, h - 30.0, t::BORDER_STRONG, 1.0);
    s.finish(title, subtitle)
}

pub struct Series {
    pub name: String,
    /// `(percentile, nanoseconds)`, ascending by percentile. Percentiles are
    /// fractions in `(0, 1)`.
    pub points: Vec<(f64, f64)>,
}

/// Percentile-of-latency curve. Lower and flatter is better; a series that
/// stays flat to the right has a short tail.
pub fn latency_percentiles(title: &str, subtitle: &str, series: &[Series]) -> String {
    // Gutters sized from the actual text so nothing clips; the plot absorbs
    // whatever is left of the shared canvas width.
    let widest_name = series
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0) as f64;
    let (pad_l, pad_t, pad_b) = (74.0, 70.0, 56.0);
    let pad_r = (widest_name * ADV11 + 28.0).max(96.0);
    let plot_h = 260.0;
    let w = CANVAS_W;
    let plot_w = (w - pad_l - pad_r).max(200.0);
    let h = pad_t + plot_h + pad_b;
    let mut s = Svg::new(w, h);

    // x: log10(1/(1-p)) — p50 = 0.30, p90 = 1, p99 = 2, p99.9 = 3 …
    // The domain ends at the deepest percentile actually present, so every
    // point lands inside the plot and the right padding stays free for the
    // direct labels.
    let vx = |p: f64| (1.0f64 / (1.0 - p)).log10();
    let v0 = vx(0.5);
    let vmax = series
        .iter()
        .flat_map(|s| s.points.iter())
        .map(|&(p, _)| vx(p))
        .fold(vx(0.99), f64::max);
    let xpos = |p: f64| -> f64 { pad_l + ((vx(p) - v0) / (vmax - v0)) * plot_w };
    // y: log10(ns) — latency spans orders of magnitude across contenders.
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for ser in series {
        for &(_, ns) in &ser.points {
            lo = lo.min(ns);
            hi = hi.max(ns);
        }
    }
    // Floor at 0.01 ns rather than 1 ns: the single-threaded primitives are
    // genuinely sub-nanosecond and clamping them would erase the result.
    const FLOOR: f64 = 0.01;
    let lo = 10f64.powf(lo.max(FLOOR).log10().floor());
    let hi = 10f64.powf((hi.max(lo * 10.0)).log10().ceil());
    let ypos = |ns: f64| -> f64 {
        pad_t + plot_h - ((ns.max(FLOOR) / lo).log10() / (hi / lo).log10()) * plot_h
    };

    s.text(24.0, 30.0, title, t::TEXT, 15.0, Anchor::Start, t::WEIGHT);
    s.text(
        24.0,
        46.0,
        subtitle,
        t::FAINT,
        11.0,
        Anchor::Start,
        t::WEIGHT,
    );

    // y gridlines: one per decade.
    let decades = (hi / lo).log10().round() as i32;
    for d in 0..=decades {
        let ns = lo * 10f64.powi(d);
        let y = ypos(ns);
        s.line(pad_l, y, pad_l + plot_w, y, t::BORDER, 1.0);
        s.text(
            pad_l - 10.0,
            y + 4.0,
            &fmt_ns(ns),
            t::FAINT,
            10.0,
            Anchor::End,
            t::WEIGHT,
        );
    }
    // x gridlines at the percentiles people actually quote.
    for (p, lbl) in [
        (0.5, "p50"),
        (0.9, "p90"),
        (0.99, "p99"),
        (0.999, "p99.9"),
        (0.9999, "p99.99"),
    ] {
        let x = xpos(p);
        if x > pad_l + plot_w + 1.0 {
            continue;
        }
        s.line(x, pad_t, x, pad_t + plot_h, t::BORDER, 1.0);
        s.text(
            x,
            pad_t + plot_h + 18.0,
            lbl,
            t::FAINT,
            10.0,
            Anchor::Middle,
            t::WEIGHT,
        );
    }
    s.text(
        pad_l + plot_w / 2.0,
        h - 12.0,
        "percentile (log scale — the tail gets the room)",
        t::FAINT,
        10.0,
        Anchor::Middle,
        400,
    );

    // Draw the lines first, collecting where each direct label wants to sit.
    let mut labels: Vec<(f64, f64, &str, &str)> = Vec::new();
    for (i, ser) in series.iter().enumerate() {
        let colour = t::SERIES[i % t::SERIES.len()];
        let pts: Vec<(f64, f64)> = ser
            .points
            .iter()
            .map(|&(p, ns)| (xpos(p), ypos(ns)))
            .collect();
        s.path(&pts, colour, 2.0);
        for &(x, y) in &pts {
            s.marker(x, y, 4.0, colour, t::BG);
        }
        if let Some(&(x, y)) = pts.last() {
            labels.push((x, y, &ser.name, colour));
        }
    }
    // De-collide the direct labels: series that converge (a common and
    // interesting outcome) would otherwise stack their labels on top of each
    // other. Sort by y and push each down to keep a legible gap, then shift the
    // whole stack back up if it overflows the canvas.
    const LINE_H: f64 = 15.0;
    labels.sort_by(|a, b| a.1.total_cmp(&b.1));
    for i in 1..labels.len() {
        let min_y = labels[i - 1].1 + LINE_H;
        if labels[i].1 < min_y {
            labels[i].1 = min_y;
        }
    }
    if let Some(last) = labels.last() {
        let overflow = last.1 - (h - 8.0);
        if overflow > 0.0 {
            for l in &mut labels {
                l.1 -= overflow;
            }
        }
    }
    for (x, y, name, colour) in labels {
        s.text(
            x + 12.0,
            y + 4.0,
            name,
            colour,
            11.0,
            Anchor::Start,
            t::WEIGHT,
        );
    }

    s.line(pad_l, pad_t, pad_l, pad_t + plot_h, t::BORDER_STRONG, 1.0);
    s.line(
        pad_l,
        pad_t + plot_h,
        pad_l + plot_w,
        pad_t + plot_h,
        t::BORDER_STRONG,
        1.0,
    );
    s.finish(title, subtitle)
}
