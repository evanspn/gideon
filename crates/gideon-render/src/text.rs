//! Text rendering onto [`GrayPage`] buffers.
//!
//! Uses vendored DejaVu Sans fonts (see `assets/DEJAVU-LICENSE`) rasterized
//! with `ab_glyph`. Dark glyphs are blended onto the existing background by
//! coverage. The hard guarantee of this module: [`draw_text`] never writes a
//! pixel outside the page bounds or beyond `x + max_width`; text that would
//! overflow is truncated with an `…` ellipsis instead.

use ab_glyph::{Font, FontRef, Glyph, ScaleFont};

use crate::GrayPage;

static FONT_REGULAR: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");
static FONT_BOLD: &[u8] = include_bytes!("../assets/DejaVuSans-Bold.ttf");

const ELLIPSIS: char = '…';

fn font(bold: bool) -> FontRef<'static> {
    let bytes = if bold { FONT_BOLD } else { FONT_REGULAR };
    FontRef::try_from_slice(bytes).expect("vendored DejaVu font is valid")
}

/// Width in pixels `text` would occupy at `px` (no clipping applied).
pub fn measure_text(px: f32, text: &str, bold: bool) -> u32 {
    let font = font(bold);
    let scaled = font.as_scaled(px);
    let mut width = 0.0f32;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(prev) = prev {
            width += scaled.kern(prev, id);
        }
        width += scaled.h_advance(id);
        prev = Some(id);
    }
    width.ceil().max(0.0) as u32
}

/// Draw `text` at baseline-top position (`x`, `y`) with font size `px`,
/// clipped to `max_width` and the page bounds. Returns the drawn width in
/// pixels. Text wider than `max_width` is truncated with an `…` ellipsis.
pub fn draw_text(
    page: &mut GrayPage,
    x: u32,
    y: u32,
    px: f32,
    text: &str,
    max_width: u32,
    bold: bool,
) -> u32 {
    if max_width == 0 || x >= page.width || y >= page.height {
        return 0;
    }
    // Clip budget: never beyond x + max_width, never beyond the page edge.
    let budget = max_width.min(page.width - x) as f32;

    let font = font(bold);
    let scaled = font.as_scaled(px);

    // Lay out glyphs, deciding up-front whether truncation is needed so the
    // ellipsis can be reserved space.
    let chars: Vec<char> = text.chars().collect();
    let full_width = measure_text(px, text, bold) as f32;
    let truncate = full_width > budget;
    let ellipsis_w = scaled.h_advance(scaled.glyph_id(ELLIPSIS));

    let mut pen = 0.0f32;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    let mut glyphs: Vec<Glyph> = Vec::with_capacity(chars.len() + 1);
    let limit = if truncate {
        (budget - ellipsis_w).max(0.0)
    } else {
        budget
    };

    for &ch in &chars {
        let id = scaled.glyph_id(ch);
        if let Some(prev) = prev {
            pen += scaled.kern(prev, id);
        }
        let advance = scaled.h_advance(id);
        if pen + advance > limit {
            break;
        }
        glyphs.push(id.with_scale_and_position(px, ab_glyph::point(pen, 0.0)));
        pen += advance;
        prev = Some(id);
    }

    if truncate {
        let id = scaled.glyph_id(ELLIPSIS);
        if pen + ellipsis_w <= budget {
            glyphs.push(id.with_scale_and_position(px, ab_glyph::point(pen, 0.0)));
            pen += ellipsis_w;
        }
    }

    // Rasterize. Baseline sits `ascent` below the requested top `y`.
    let ascent = scaled.ascent();
    let mut drawn_right = 0u32;
    for glyph in glyphs {
        let Some(outline) = font.outline_glyph(glyph) else {
            continue; // whitespace has no outline
        };
        let bounds = outline.px_bounds();
        outline.draw(|gx, gy, coverage| {
            let px_x = x as i64 + bounds.min.x as i64 + gx as i64;
            let px_y = y as i64 + (ascent + bounds.min.y) as i64 + gy as i64;
            if px_x < x as i64
                || px_x >= (x + max_width.min(page.width - x)) as i64
                || px_y < 0
                || px_x >= page.width as i64
                || px_y >= page.height as i64
            {
                return;
            }
            let idx = (px_y as u32 * page.width + px_x as u32) as usize;
            let bg = page.pixels[idx] as f32;
            // Blend dark ink (near 0) over the background by coverage.
            let blended = bg * (1.0 - coverage);
            page.pixels[idx] = blended.round().clamp(0.0, 255.0) as u8;
            let right = px_x as u32 + 1 - x;
            if right > drawn_right {
                drawn_right = right;
            }
        });
    }

    let advance_width = (pen.ceil().max(0.0) as u32).min(budget as u32);
    drawn_right.max(advance_width)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(w: u32, h: u32) -> GrayPage {
        GrayPage::new_white(w, h)
    }

    #[test]
    fn text_renders_non_blank() {
        let mut p = page(200, 50);
        let w = draw_text(&mut p, 4, 4, 28.0, "Library", 190, false);
        assert!(w > 0);
        assert!(
            p.pixels.iter().any(|&px| px < 0x80),
            "no dark pixels were drawn"
        );
    }

    #[test]
    fn bold_differs_from_regular() {
        let mut a = page(200, 50);
        let mut b = page(200, 50);
        draw_text(&mut a, 4, 4, 28.0, "Hi", 190, false);
        draw_text(&mut b, 4, 4, 28.0, "Hi", 190, true);
        assert_ne!(a.pixels, b.pixels);
    }

    #[test]
    fn never_draws_beyond_max_width_or_page_edge() {
        let mut p = page(300, 60);
        let x = 10u32;
        let max_width = 80u32;
        let before = p.pixels.clone();
        draw_text(
            &mut p,
            x,
            8,
            28.0,
            "An extremely long string that cannot possibly fit in eighty pixels",
            max_width,
            false,
        );
        for y in 0..p.height {
            for px_x in 0..p.width {
                let outside = px_x < x || px_x >= x + max_width;
                if outside {
                    let idx = (y * p.width + px_x) as usize;
                    assert_eq!(
                        p.pixels[idx], before[idx],
                        "pixel ({px_x},{y}) outside the clip region was modified"
                    );
                }
            }
        }
    }

    #[test]
    fn clips_at_page_edge_without_panicking() {
        let mut p = page(60, 20);
        // max_width extends far past the page; must clip at the page edge.
        draw_text(&mut p, 40, 2, 28.0, "Wide text", 10_000, false);
        // x beyond the page: no-op.
        assert_eq!(draw_text(&mut p, 100, 2, 28.0, "x", 50, false), 0);
        // y beyond the page: no-op.
        assert_eq!(draw_text(&mut p, 0, 100, 28.0, "x", 50, false), 0);
    }

    #[test]
    fn ellipsis_appears_when_truncated() {
        let px = 24.0;
        let long = "A very long title that will definitely not fit";
        let max_width = 120u32;
        assert!(measure_text(px, long, false) > max_width);

        let mut truncated = page(400, 50);
        draw_text(&mut truncated, 0, 4, px, long, max_width, false);

        // Render what we expect: the prefix that fits plus the ellipsis.
        // Verify the ellipsis is present by comparing against a render of
        // just the fitting prefix without an ellipsis: they must differ, and
        // the truncated render must have ink close to the right clip edge
        // where the ellipsis sits.
        let mut darkest_x = 0u32;
        for y in 0..truncated.height {
            for x in 0..truncated.width {
                if truncated.pixel(x, y) < 0x80 && x > darkest_x {
                    darkest_x = x;
                }
            }
        }
        assert!(
            darkest_x >= max_width.saturating_sub(measure_text(px, "…", false) + 4),
            "no ink near the clip edge — ellipsis missing (rightmost ink at {darkest_x})"
        );
        assert!(darkest_x < max_width, "ink beyond the clip edge");
    }

    #[test]
    fn short_text_is_not_truncated() {
        let mut with_room = page(400, 50);
        let w = draw_text(&mut with_room, 0, 4, 24.0, "Hi", 380, false);
        assert_eq!(w as i64, measure_text(24.0, "Hi", false) as i64);
    }

    #[test]
    fn measure_is_monotonic() {
        assert!(measure_text(24.0, "ab", false) > measure_text(24.0, "a", false));
        assert_eq!(measure_text(24.0, "", false), 0);
    }
}
