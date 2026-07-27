//! Minimal SVG builder — enough for the two chart forms, no dependencies.

use std::fmt::Write;

pub struct Svg {
    body: String,
    pub w: f64,
    pub h: f64,
}

/// Escape the five XML metacharacters.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Trim trailing zeros so coordinates stay short and diffs stay readable.
fn n(v: f64) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-0" {
        "0".into()
    } else {
        s.into()
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Anchor {
    Start,
    Middle,
    End,
}

impl Anchor {
    fn as_str(self) -> &'static str {
        match self {
            Anchor::Start => "start",
            Anchor::Middle => "middle",
            Anchor::End => "end",
        }
    }
}

impl Svg {
    pub fn new(w: f64, h: f64) -> Self {
        Self {
            body: String::new(),
            w,
            h,
        }
    }

    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, fill: &str, rx: f64) -> &mut Self {
        // Guard against degenerate geometry (a zero-width bar renders nothing).
        if w <= 0.0 || h <= 0.0 {
            return self;
        }
        let _ = write!(
            self.body,
            r#"<rect x="{}" y="{}" width="{}" height="{}" rx="{}" fill="{fill}"/>"#,
            n(x),
            n(y),
            n(w),
            n(h),
            n(rx.min(w / 2.0).min(h / 2.0))
        );
        self
    }

    pub fn line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, stroke: &str, w: f64) -> &mut Self {
        let _ = write!(
            self.body,
            r#"<line x1="{}" y1="{}" x2="{}" y2="{}" stroke="{stroke}" stroke-width="{}"/>"#,
            n(x1),
            n(y1),
            n(x2),
            n(y2),
            n(w)
        );
        self
    }

    /// Polyline through `pts`, 2px by default per the mark spec.
    pub fn path(&mut self, pts: &[(f64, f64)], stroke: &str, w: f64) -> &mut Self {
        if pts.len() < 2 {
            return self;
        }
        let mut d = String::new();
        for (i, (x, y)) in pts.iter().enumerate() {
            let _ = write!(d, "{}{} {}", if i == 0 { "M" } else { "L" }, n(*x), n(*y));
            if i + 1 < pts.len() {
                d.push(' ');
            }
        }
        let _ = write!(
            self.body,
            r#"<path d="{d}" fill="none" stroke="{stroke}" stroke-width="{}" stroke-linejoin="round" stroke-linecap="round"/>"#,
            n(w)
        );
        self
    }

    /// Marker with a 2px surface ring, so overlapping marks stay separable.
    pub fn marker(&mut self, x: f64, y: f64, r: f64, fill: &str, ring: &str) -> &mut Self {
        let _ = write!(
            self.body,
            r#"<circle cx="{}" cy="{}" r="{}" fill="{fill}" stroke="{ring}" stroke-width="2"/>"#,
            n(x),
            n(y),
            n(r)
        );
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text(
        &mut self,
        x: f64,
        y: f64,
        s: &str,
        fill: &str,
        size: f64,
        anchor: Anchor,
        weight: u32,
    ) -> &mut Self {
        // Berkeley Mono ships an oblique face at every weight and exposes its
        // upright weights as separate families, which several renderers get
        // wrong — a bold request lands on Bold-Oblique even with
        // `font-style="normal"`. So emphasis is carried by *size and colour* at
        // one upright weight instead, which is correct in every renderer.
        let _ = write!(
            self.body,
            r#"<text x="{}" y="{}" fill="{fill}" font-size="{}" font-weight="{weight}" font-style="normal" text-anchor="{}">{}</text>"#,
            n(x),
            n(y),
            n(size),
            anchor.as_str(),
            esc(s)
        );
        self
    }

    pub fn finish(self, title: &str, desc: &str) -> String {
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" role="img" aria-labelledby="t d">
<title id="t">{title}</title><desc id="d">{desc}</desc>
<style>text{{font-family:{font};{feat};font-style:normal;dominant-baseline:auto}}</style>
<rect width="{w}" height="{h}" fill="{bg}"/>
{body}
</svg>
"##,
            w = n(self.w),
            h = n(self.h),
            title = esc(title),
            desc = esc(desc),
            font = crate::theme::FONT,
            feat = crate::theme::FONT_FEATURES,
            bg = crate::theme::BG,
            body = self.body,
        )
    }
}
