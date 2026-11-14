//! Text rendering onto [`GrayPage`] buffers.
//!
//! Uses vendored fonts rasterized with `ab_glyph`: DejaVu Sans for every
//! screen, and twelve families the book reader offers ([`FontFamily`]). All
//! are freely redistributable — DejaVu under its own licence
//! (`assets/DEJAVU-LICENSE`), the rest under the SIL Open Font License
//! (`assets/*-OFL.txt`). Dark glyphs are blended onto the existing background by
//! coverage. The hard guarantee of this module: [`draw_text`] never writes a
//! pixel outside the page bounds or beyond `x + max_width`; text that would
//! overflow is truncated with an `…` ellipsis instead.

use ab_glyph::{Font, FontRef, Glyph, ScaleFont};

use crate::GrayPage;

static FONT_REGULAR: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");
static FONT_BOLD: &[u8] = include_bytes!("../assets/DejaVuSans-Bold.ttf");
// The reader's faces. Variable fonts are cut to static Regular and Bold
// instances (ab_glyph draws a variable font only at its default), and every
// reader face is subset to Latin text, which is what keeps twelve of them
// to ~1.6 MB. Licences sit beside each file in `assets/`.
static SERIF_REGULAR: &[u8] = include_bytes!("../assets/DejaVuSerif.ttf");
static SERIF_BOLD: &[u8] = include_bytes!("../assets/DejaVuSerif-Bold.ttf");
static LITERATA_REGULAR: &[u8] = include_bytes!("../assets/Literata-Regular.ttf");
static LITERATA_BOLD: &[u8] = include_bytes!("../assets/Literata-Bold.ttf");
static BITTER_REGULAR: &[u8] = include_bytes!("../assets/Bitter-Regular.ttf");
static BITTER_BOLD: &[u8] = include_bytes!("../assets/Bitter-Bold.ttf");
static BASKERVILLE_REGULAR: &[u8] = include_bytes!("../assets/LibreBaskerville-Regular.ttf");
static BASKERVILLE_BOLD: &[u8] = include_bytes!("../assets/LibreBaskerville-Bold.ttf");
static GARAMOND_REGULAR: &[u8] = include_bytes!("../assets/EBGaramond-Regular.ttf");
static GARAMOND_BOLD: &[u8] = include_bytes!("../assets/EBGaramond-Bold.ttf");
static LORA_REGULAR: &[u8] = include_bytes!("../assets/Lora-Regular.ttf");
static LORA_BOLD: &[u8] = include_bytes!("../assets/Lora-Bold.ttf");
static MERRIWEATHER_REGULAR: &[u8] = include_bytes!("../assets/Merriweather-Regular.ttf");
static MERRIWEATHER_BOLD: &[u8] = include_bytes!("../assets/Merriweather-Bold.ttf");
static OPEN_SANS_REGULAR: &[u8] = include_bytes!("../assets/OpenSans-Regular.ttf");
static OPEN_SANS_BOLD: &[u8] = include_bytes!("../assets/OpenSans-Bold.ttf");
static JOST_REGULAR: &[u8] = include_bytes!("../assets/Jost-Regular.ttf");
static JOST_BOLD: &[u8] = include_bytes!("../assets/Jost-Bold.ttf");
static HYPERLEGIBLE_REGULAR: &[u8] = include_bytes!("../assets/AtkinsonHyperlegible-Regular.ttf");
static HYPERLEGIBLE_BOLD: &[u8] = include_bytes!("../assets/AtkinsonHyperlegible-Bold.ttf");
static DYSLEXIC_REGULAR: &[u8] = include_bytes!("../assets/OpenDyslexic-Regular.otf");
static DYSLEXIC_BOLD: &[u8] = include_bytes!("../assets/OpenDyslexic-Bold.otf");

/// A typeface the book reader can set its text in. Every other screen is
/// DejaVu Sans, the default. The range follows what an e-reader owner
/// expects to find: book serifs, a slab, sans faces, and two faces for
/// readers who need them (low vision, dyslexia).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontFamily {
    #[default]
    Sans,
    Serif,
    Literata,
    Bitter,
    Baskerville,
    Garamond,
    Lora,
    Merriweather,
    OpenSans,
    Jost,
    Hyperlegible,
    OpenDyslexic,
}

impl FontFamily {
    /// In the order the font list shows them.
    pub const ALL: [FontFamily; 12] = [
        FontFamily::Literata,
        FontFamily::Serif,
        FontFamily::Baskerville,
        FontFamily::Garamond,
        FontFamily::Lora,
        FontFamily::Merriweather,
        FontFamily::Bitter,
        FontFamily::Sans,
        FontFamily::OpenSans,
        FontFamily::Jost,
        FontFamily::Hyperlegible,
        FontFamily::OpenDyslexic,
    ];

    /// The name stored in settings.json.
    pub fn name(self) -> &'static str {
        match self {
            FontFamily::Sans => "sans",
            FontFamily::Serif => "serif",
            FontFamily::Literata => "literata",
            FontFamily::Bitter => "bitter",
            FontFamily::Baskerville => "baskerville",
            FontFamily::Garamond => "garamond",
            FontFamily::Lora => "lora",
            FontFamily::Merriweather => "merriweather",
            FontFamily::OpenSans => "open-sans",
            FontFamily::Jost => "jost",
            FontFamily::Hyperlegible => "hyperlegible",
            FontFamily::OpenDyslexic => "opendyslexic",
        }
    }

    /// What the font list calls it.
    pub fn label(self) -> &'static str {
        match self {
            FontFamily::Sans => "DejaVu Sans",
            FontFamily::Serif => "DejaVu Serif",
            FontFamily::Literata => "Literata",
            FontFamily::Bitter => "Bitter",
            FontFamily::Baskerville => "Baskerville",
            FontFamily::Garamond => "Garamond",
            FontFamily::Lora => "Lora",
            FontFamily::Merriweather => "Merriweather",
            FontFamily::OpenSans => "Open Sans",
            FontFamily::Jost => "Jost",
            FontFamily::Hyperlegible => "Hyperlegible",
            FontFamily::OpenDyslexic => "OpenDyslexic",
        }
    }

    /// The family a stored name means; anything unknown is Sans.
    pub fn from_name(name: &str) -> FontFamily {
        FontFamily::ALL
            .into_iter()
            .find(|f| f.name().eq_ignore_ascii_case(name.trim()))
            .unwrap_or_default()
    }

    fn bytes(self, bold: bool) -> &'static [u8] {
        let (regular, heavy) = match self {
            FontFamily::Sans => (FONT_REGULAR, FONT_BOLD),
            FontFamily::Serif => (SERIF_REGULAR, SERIF_BOLD),
            FontFamily::Literata => (LITERATA_REGULAR, LITERATA_BOLD),
            FontFamily::Bitter => (BITTER_REGULAR, BITTER_BOLD),
            FontFamily::Baskerville => (BASKERVILLE_REGULAR, BASKERVILLE_BOLD),
            FontFamily::Garamond => (GARAMOND_REGULAR, GARAMOND_BOLD),
            FontFamily::Lora => (LORA_REGULAR, LORA_BOLD),
            FontFamily::Merriweather => (MERRIWEATHER_REGULAR, MERRIWEATHER_BOLD),
            FontFamily::OpenSans => (OPEN_SANS_REGULAR, OPEN_SANS_BOLD),
            FontFamily::Jost => (JOST_REGULAR, JOST_BOLD),
            FontFamily::Hyperlegible => (HYPERLEGIBLE_REGULAR, HYPERLEGIBLE_BOLD),
            FontFamily::OpenDyslexic => (DYSLEXIC_REGULAR, DYSLEXIC_BOLD),
        };
        if bold {
            heavy
        } else {
            regular
        }
    }
}

const ELLIPSIS: char = '…';

/// How far an italic glyph leans: horizontal shift per pixel above the
/// baseline. No oblique faces are vendored (they would double the reader's
/// fonts); a shear of the upright outline is what most e-readers do for
/// synthetic italics and reads as italic at 300 dpi.
const SLANT: f32 = 0.21;

/// Weight and slant for a run of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextStyle {
    pub bold: bool,
    pub italic: bool,
    pub family: FontFamily,
}

impl TextStyle {
    pub const REGULAR: TextStyle = TextStyle {
        bold: false,
        italic: false,
        family: FontFamily::Sans,
    };
    pub const BOLD: TextStyle = TextStyle {
        bold: true,
        italic: false,
        family: FontFamily::Sans,
    };
}

fn font_in(family: FontFamily, bold: bool) -> FontRef<'static> {
    FontRef::try_from_slice(family.bytes(bold)).expect("vendored font is valid")
}

/// Width in pixels `text` would occupy at `px` (no clipping applied).
pub fn measure_text(px: f32, text: &str, bold: bool) -> u32 {
    measure_text_in(px, text, bold, FontFamily::Sans)
}

/// [`measure_text`] in a given family.
pub fn measure_text_in(px: f32, text: &str, bold: bool, family: FontFamily) -> u32 {
    let font = font_in(family, bold);
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

/// Width `text` would occupy at `px` in `style`, including the lean of an
/// italic run's last glyph.
pub fn measure_text_styled(px: f32, text: &str, style: TextStyle) -> u32 {
    let base = measure_text_in(px, text, style.bold, style.family);
    if style.italic && base > 0 {
        base + italic_overhang(px)
    } else {
        base
    }
}

/// How far past its advance an italic glyph's top can reach.
fn italic_overhang(px: f32) -> u32 {
    (px * 0.75 * SLANT).ceil() as u32
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
    draw_text_styled(
        page,
        x,
        y,
        px,
        text,
        max_width,
        TextStyle {
            bold,
            italic: false,
            family: FontFamily::Sans,
        },
    )
}

/// [`draw_text`] with a [`TextStyle`]: italic is a shear of the upright
/// outline (see [`SLANT`]), so the same clipping promise holds — nothing
/// lands past `x + max_width` or the page edge.
pub fn draw_text_styled(
    page: &mut GrayPage,
    x: u32,
    y: u32,
    px: f32,
    text: &str,
    max_width: u32,
    style: TextStyle,
) -> u32 {
    if max_width == 0 || x >= page.width || y >= page.height {
        return 0;
    }
    let bold = style.bold;
    // Clip budget: never beyond x + max_width, never beyond the page edge.
    let budget = max_width.min(page.width - x) as f32;
    let slant = if style.italic { SLANT } else { 0.0 };

    let font = font_in(style.family, bold);
    let scaled = font.as_scaled(px);

    // Lay out glyphs, deciding up-front whether truncation is needed so the
    // ellipsis can be reserved space.
    let chars: Vec<char> = text.chars().collect();
    let full_width = measure_text_in(px, text, bold, style.family) as f32;
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
            // Lean: a shear about the baseline. `bounds.min.y + gy` is the
            // row relative to the baseline (negative above it), so rows
            // above shift right and descenders shift left; the baseline
            // itself stays put, which keeps runs flush with their
            // neighbours.
            let above = -(bounds.min.y + gy as f32);
            let lean = (above * slant).round() as i64;
            let px_x = x as i64 + bounds.min.x as i64 + gx as i64 + lean;
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
    fn italic_differs_from_regular_and_stays_inside_its_box() {
        let mut regular = page(200, 60);
        let mut italic = page(200, 60);
        draw_text(&mut regular, 10, 5, 24.0, "Billy", 150, false);
        draw_text_styled(
            &mut italic,
            10,
            5,
            24.0,
            "Billy",
            150,
            TextStyle {
                bold: false,
                italic: true,
                ..Default::default()
            },
        );
        assert_ne!(regular.pixels, italic.pixels, "the lean changes pixels");
        // Ink leans right: the top rows of the italic reach further right
        // than the regular's, and nothing lands left of x.
        let right_edge = |p: &GrayPage, row: u32| {
            (0..p.width)
                .rev()
                .find(|&col| p.pixels[(row * p.width + col) as usize] < 0x80)
        };
        let top_row = (5..30).find(|&r| right_edge(&italic, r).is_some()).unwrap();
        assert!(right_edge(&italic, top_row) >= right_edge(&regular, top_row));
        for row in 0..italic.height {
            for col in 0..10 {
                assert_eq!(italic.pixels[(row * italic.width + col) as usize], 0xff);
            }
        }
        assert!(
            measure_text_styled(
                24.0,
                "Billy",
                TextStyle {
                    bold: false,
                    italic: true,
                    ..Default::default()
                }
            ) > measure_text(24.0, "Billy", false)
        );
    }

    #[test]
    fn every_reader_family_draws_its_own_glyphs_regular_and_bold() {
        let mut seen: Vec<Vec<u8>> = Vec::new();
        for family in FontFamily::ALL {
            for bold in [false, true] {
                let mut p = page(240, 50);
                let style = TextStyle {
                    bold,
                    italic: false,
                    family,
                };
                let w = draw_text_styled(&mut p, 4, 4, 28.0, "Harbour", 230, style);
                assert!(w > 0, "{family:?} bold={bold} drew nothing");
                assert!(p.pixels.iter().any(|&v| v < 0x80), "{family:?} has ink");
                assert!(
                    !seen.contains(&p.pixels),
                    "{family:?} bold={bold} looks like another face"
                );
                seen.push(p.pixels);
            }
            assert_eq!(FontFamily::from_name(family.name()), family);
        }
        assert_eq!(FontFamily::from_name("comic sans"), FontFamily::Sans);
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
