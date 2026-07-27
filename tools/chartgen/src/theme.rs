//! zk2u brand tokens, mirrored from `zk2u.com/sass/_tokens.scss` and the Zed
//! theme `zk2u.json` (CarbonFox neutral near-black, green as the one accent).
//!
//! The series hues are the brand colours **snapped into the dark-mode OKLCH
//! lightness band (L 0.48–0.67)** so they stay legible on `#101010` and remain
//! separable under colour-vision deficiency. The set was verified with the
//! dataviz palette validator: lightness band, chroma floor, CVD separation,
//! normal-vision floor, and contrast-vs-surface all pass. The green↔orange pair
//! sits in the 6–8 ΔE deutan floor band, which is legal *only* with secondary
//! encoding — hence every line is direct-labelled.

/// Page / chart surface.
pub const BG: &str = "#101010";
/// Primary ink.
pub const TEXT: &str = "#f2f4f8";
/// Secondary ink (axis labels, legend).
pub const MUTED: &str = "#b6b8bb";
/// Tertiary ink — still ≥4.5:1 on both surfaces.
pub const FAINT: &str = "#838383";
/// Hairline.
pub const BORDER: &str = "#2a2a2a";
/// Emphasised hairline (axis baseline).
pub const BORDER_STRONG: &str = "#3a3a3a";

/// cynosure — the brand green, stepped into the dark band.
pub const HERO: &str = "#1fae61";
/// Muted fill for non-hero bars: identity comes from the axis label, so the
/// bars carry emphasis only, not identity (single-series chart).
pub const NEUTRAL: &str = "#3a3a3a";
/// Hairline on neutral bars so they read as marks, not gaps.
pub const NEUTRAL_EDGE: &str = "#4a4a4a";

/// Fixed categorical order for multi-series charts. Never cycled: a chart that
/// would need a 6th series gets faceted instead.
pub const SERIES: [&str; 5] = [
    HERO,      // cynosure
    "#e8630f", // orange
    "#4589ff", // blue
    "#d02670", // magenta
    "#a56eff", // purple
];

/// Berkeley Mono with ligatures on, falling back gracefully. The font is
/// commercial, so it is referenced by family name and never embedded — viewers
/// who own it get the real thing, everyone else gets a sane mono fallback.
pub const FONT: &str = "'Berkeley Mono', ui-monospace, 'SF Mono', Menlo, monospace";
/// Emphasis is size + colour, never weight: Berkeley Mono's oblique faces get
/// mis-selected for bold requests by several renderers, so every glyph here is
/// drawn at one upright weight.
pub const WEIGHT: u32 = 400;
/// Enables `liga`/`calt` so Berkeley Mono's ligatures render.
pub const FONT_FEATURES: &str = "font-feature-settings:'liga' 1,'calt' 1";

/// Sharp, not bubbly — matches the 2–4px radius scale of the brand.
pub const RADIUS: f64 = 3.0;
