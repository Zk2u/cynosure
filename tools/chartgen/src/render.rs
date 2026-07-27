//! SVG → PNG rasterisation at 4×, so the charts stay crisp on HiDPI displays
//! and in any renderer that won't load the SVG's fonts.
//!
//! `resvg` is pure Rust and only ever a dependency of this tool — the library
//! itself stays dependency-free.

use std::{path::Path, sync::OnceLock};

use resvg::{tiny_skia, usvg};

/// Chart PNGs are emitted at this multiple of their SVG dimensions.
pub const SCALE: f32 = 4.0;

/// System fonts, loaded once — Berkeley Mono lives there.
fn options() -> &'static usvg::Options<'static> {
    static OPTS: OnceLock<usvg::Options> = OnceLock::new();
    OPTS.get_or_init(|| {
        let mut opt = usvg::Options::default();
        opt.fontdb_mut().load_system_fonts();
        // Fallback for machines without Berkeley Mono, so text never vanishes.
        opt.font_family = "monospace".to_string();
        opt
    })
}

/// Rasterise `svg` to `png_path` at [`SCALE`]×.
pub fn png(svg: &str, png_path: &Path) -> Result<(u32, u32), String> {
    let tree = usvg::Tree::from_str(svg, options()).map_err(|e| e.to_string())?;
    let size = tree.size();
    let (w, h) = (
        (size.width() * SCALE).round() as u32,
        (size.height() * SCALE).round() as u32,
    );
    let mut pixmap =
        tiny_skia::Pixmap::new(w, h).ok_or_else(|| format!("bad pixmap size {w}x{h}"))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(SCALE, SCALE),
        &mut pixmap.as_mut(),
    );
    pixmap.save_png(png_path).map_err(|e| e.to_string())?;
    Ok((w, h))
}
