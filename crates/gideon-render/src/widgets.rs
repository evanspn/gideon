//! The reusable widgets for the data-rich library UI: stat tiles, library
//! rows, the navigation bar and a book's metadata block.
//!
//! These are drawn on the same panel — and under the same constraints — as
//! [`crate::heatmap`], so they follow the same rules.
//!
//! **Colour goes in blocks.** The Libra Colour's Kaleido filter resolves
//! colour at roughly half the resolution it resolves black, so a hue that
//! lands in thin strokes or small text reads soft and fringed while a filled
//! block stays crisp. Every colour this module spends is a block: a chip
//! fill, a progress bar, the nav indicator, the score marker. Body text is
//! always black, at every size.
//!
//! **Colour is never the only carrier.** [`Theme::MONO`] is the same widget
//! on a panel with no colour filter, and it is a target rather than a
//! fallback: anything colour distinguishes here also differs in value,
//! position, weight or label. The active nav item is bold *and* underscored;
//! a chip has a label inside it; a progress bar has a length. Throw the hue
//! away and the screen still reads.
//!
//! **Flat fills and 1px rules only** — no gradients, no rounded corners, no
//! anti-aliased hairlines, all of which the panel's dither would turn to mud.
//!
//! **Nothing draws outside the rect it was handed.** Every fill is clipped
//! twice, to the widget's own rect and to the canvas, and text is rasterized
//! into a scratch layer no bigger than the visible part of that rect before
//! being overlaid. See `docs/LESSONS.md` §1: bobo's Lua UI overflow bugs are
//! not allowed back in.
//!
//! Everything scales from the caller's rect and `text_px`. The panel is
//! 1264x1680 on a Libra Colour and 1072x1448 elsewhere; hardcoding either is
//! a bug.

use crate::text::{draw_text, measure_text};
use crate::{GrayPage, RgbPage};

/// Hairlines and progress-bar tracks. Deliberately a grey and not a theme
/// colour: a 1px colour rule is exactly what the Kaleido filter fringes.
const RULE: [u8; 3] = [0xCC, 0xCC, 0xCC];

/// Black. Text is drawn in it at every size, in every theme.
const INK: [u8; 3] = [0x00, 0x00, 0x00];

/// The colours one profile spends, by role rather than by hue, so a widget
/// asks for "the chip tint" and gets whatever this profile uses for it.
///
/// Roles that carry meaning appear twice — `status`/`status_tint`,
/// `genre`/`genre_tint` — because a chip is a light fill with black text
/// while the same category as a marker wants the saturated version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// The profile's lead colour: nav indicator, tile markers.
    pub accent: [u8; 3],
    /// The score marker.
    pub star: [u8; 3],
    /// Reading status, saturated.
    pub status: [u8; 3],
    /// Genre, saturated.
    pub genre: [u8; 3],
    /// Reading-status chip fill. Light enough to hold black text.
    pub status_tint: [u8; 3],
    /// Genre chip fill. Light enough to hold black text.
    pub genre_tint: [u8; 3],
    /// The filled part of a progress bar.
    pub bar: [u8; 3],
    /// Section-label rules.
    pub label: [u8; 3],
    /// Text. Black in every profile — see the module note.
    pub ink: [u8; 3],
}

impl Theme {
    /// Ink & Rust — the default profile. Warm earth.
    pub const INK_RUST: Theme = Theme {
        accent: [0xA8, 0x5F, 0x38],
        star: [0xB0, 0x8A, 0x2E],
        status: [0x4C, 0x7A, 0x55],
        genre: [0x3F, 0x6B, 0x8C],
        status_tint: [0xDB, 0xE7, 0xDD],
        genre_tint: [0xDA, 0xE4, 0xEB],
        bar: [0xA8, 0x5F, 0x38],
        label: [0xA8, 0x5F, 0x38],
        ink: INK,
    };

    /// Indigo Press — cool, blue-led.
    pub const INDIGO: Theme = Theme {
        accent: [0x3F, 0x54, 0x88],
        star: [0xA2, 0x60, 0x3F],
        status: [0x3A, 0x6E, 0x72],
        genre: [0x6B, 0x4A, 0x73],
        status_tint: [0xD7, 0xE4, 0xE4],
        genre_tint: [0xE4, 0xDC, 0xEA],
        bar: [0x3F, 0x54, 0x88],
        label: [0x3F, 0x54, 0x88],
        ink: INK,
    };

    /// Sumi & Vermilion — near-monochrome with a single hue.
    pub const SUMI: Theme = Theme {
        accent: [0xB1, 0x4A, 0x32],
        star: [0xB1, 0x4A, 0x32],
        status: [0x1A, 0x1A, 0x1A],
        genre: [0x6E, 0x6A, 0x63],
        status_tint: [0xEE, 0xDC, 0xD5],
        genre_tint: [0xEE, 0xEC, 0xE7],
        bar: [0x1A, 0x1A, 0x1A],
        label: [0x1A, 0x1A, 0x1A],
        ink: INK,
    };

    /// Botanical — moss-led, for the four-hue genre-coding profile.
    pub const BOTANICAL: Theme = Theme {
        accent: [0xA4, 0x63, 0x3E],
        star: [0x9A, 0x7A, 0x34],
        status: [0x5F, 0x6F, 0x3F],
        genre: [0x4E, 0x6E, 0x86],
        status_tint: [0xE2, 0xE6, 0xD6],
        genre_tint: [0xDF, 0xE6, 0xEC],
        bar: [0x5F, 0x6F, 0x3F],
        label: [0x5F, 0x6F, 0x3F],
        ink: INK,
    };

    /// The build for panels with no colour filter. Same widgets, same
    /// geometry, separation carried entirely by value, weight and label.
    pub const MONO: Theme = Theme {
        accent: [0x00, 0x00, 0x00],
        star: [0x55, 0x55, 0x55],
        status: [0x00, 0x00, 0x00],
        genre: [0x99, 0x99, 0x99],
        status_tint: [0xF0, 0xF0, 0xF0],
        genre_tint: [0xF6, 0xF6, 0xF6],
        bar: [0x22, 0x22, 0x22],
        label: [0x55, 0x55, 0x55],
        ink: INK,
    };

    /// Resolve a profile name from `settings.json`. Unknown names give the
    /// default rather than an error — settings are parsed leniently
    /// everywhere else in this codebase and a bad value must never be fatal.
    pub fn from_setting(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "indigo" => Self::INDIGO,
            "sumi" => Self::SUMI,
            "botanical" => Self::BOTANICAL,
            "mono" => Self::MONO,
            _ => Self::INK_RUST,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::INK_RUST
    }
}

// --- clipping -------------------------------------------------------------

/// The intersection of a widget's rect with the canvas, in canvas pixels.
/// Half-open: `x0..x1`, `y0..y1`.
///
/// Every drawing helper below takes one of these, which is what makes "a
/// widget cannot paint over its neighbours" structural rather than a thing
/// each widget has to remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Clip {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

impl Clip {
    fn new(canvas: &RgbPage, x: u32, y: u32, w: u32, h: u32) -> Self {
        Self {
            x0: x.min(canvas.width),
            y0: y.min(canvas.height),
            x1: x.saturating_add(w).min(canvas.width),
            y1: y.saturating_add(h).min(canvas.height),
        }
    }

    fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    fn width(&self) -> u32 {
        self.x1 - self.x0
    }

    fn height(&self) -> u32 {
        self.y1 - self.y0
    }

    /// A white scratch page covering exactly the visible part of the rect.
    /// Text drawn into it is clipped by [`draw_text`] to these bounds, so it
    /// cannot escape the widget however long the string is.
    fn text_layer(&self) -> GrayPage {
        GrayPage::new_white(self.width(), self.height())
    }
}

/// Fill an axis-aligned rectangle, clipped to `clip` and again to the canvas.
/// Anything outside is dropped rather than wrapping onto the next row — the
/// failure mode that makes overflow bugs hard to see.
fn fill_rect(canvas: &mut RgbPage, clip: &Clip, x: u32, y: u32, w: u32, h: u32, color: [u8; 3]) {
    let x0 = x.max(clip.x0);
    let y0 = y.max(clip.y0);
    let x1 = x.saturating_add(w).min(clip.x1).min(canvas.width);
    let y1 = y.saturating_add(h).min(clip.y1).min(canvas.height);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    for row in y0..y1 {
        let start = ((row * canvas.width + x0) * 3) as usize;
        for col in 0..(x1 - x0) {
            let idx = start + (col * 3) as usize;
            canvas.pixels[idx..idx + 3].copy_from_slice(&color);
        }
    }
}

/// Overlay the ink of a grayscale layer onto a colour canvas at the origin.
/// White is transparent, so a text layer lands on top of the colour blocks
/// beneath it without boxing them out.
///
/// This is the bridge between the text stack (which rasterizes to
/// [`GrayPage`]) and the colour canvas, and is public so callers composing
/// their own screens can use the same one.
pub fn overlay_ink(dst: &mut RgbPage, src: &GrayPage) {
    overlay_ink_at(dst, src, 0, 0);
}

/// [`overlay_ink`] with the layer placed at (`ox`, `oy`), clipped to the
/// canvas. Widgets rasterize into a rect-sized layer and land it in place.
fn overlay_ink_at(dst: &mut RgbPage, src: &GrayPage, ox: u32, oy: u32) {
    if ox >= dst.width || oy >= dst.height {
        return;
    }
    let w = src.width.min(dst.width - ox);
    let h = src.height.min(dst.height - oy);
    for sy in 0..h {
        for sx in 0..w {
            let g = src.pixel(sx, sy);
            if g == 0xFF {
                continue;
            }
            let idx = (((oy + sy) * dst.width + ox + sx) * 3) as usize;
            dst.pixels[idx..idx + 3].copy_from_slice(&[g, g, g]);
        }
    }
}

/// A filled diamond: the score marker, and the one glyph-sized thing here
/// that gets colour. It is a solid block at every size, so the Kaleido
/// filter resolves it cleanly where an outlined star would fringe.
fn fill_marker(canvas: &mut RgbPage, clip: &Clip, x: u32, y: u32, size: u32, color: [u8; 3]) {
    if size == 0 {
        return;
    }
    let half = size / 2;
    for row in 0..size {
        // Widen to the middle row, then narrow again.
        let spread = if row <= half { row } else { size - 1 - row };
        let run = spread * 2 + 1;
        let left = x + half.saturating_sub(spread);
        fill_rect(canvas, clip, left, y + row, run, 1, color);
    }
}

/// Vertically centre `px`-tall text in a band of height `h` starting at `y`.
fn center_y(y: u32, h: u32, px: f32) -> u32 {
    y + h.saturating_sub(px.max(0.0) as u32) / 2
}

/// The largest dimension any of this scales to. No panel is a million
/// pixels across, and capping here keeps a degenerate `text_px` from
/// overflowing the position arithmetic downstream instead of just clipping.
const MAX_SCALED: u32 = 1 << 20;

/// Round a scaled dimension up to at least `min`, so nothing a widget draws
/// vanishes at a small `text_px` — or runs away at an absurd one.
fn scaled(text_px: f32, factor: f32, min: u32) -> u32 {
    let v = text_px.max(0.0) * factor;
    if !v.is_finite() {
        return min;
    }
    (v as u32).clamp(min, MAX_SCALED.max(min))
}

/// Draw a tinted chip with a black label inside it, and report the width it
/// took. Returns 0 when `avail` cannot hold even a stub — the caller then
/// drops the chip rather than drawing a squashed one.
///
/// The fill goes on the canvas and the label into `layer` (whose origin is
/// `clip`'s), so the overlay puts black text over the colour block.
#[allow(clippy::too_many_arguments)]
fn draw_chip(
    canvas: &mut RgbPage,
    layer: &mut GrayPage,
    clip: &Clip,
    x: u32,
    y: u32,
    avail: u32,
    h: u32,
    text: &str,
    px: f32,
    tint: [u8; 3],
) -> u32 {
    let pad = scaled(px, 0.45, 2);
    let min_w = pad * 2 + scaled(px, 0.6, 3);
    if avail < min_w || h == 0 || text.is_empty() {
        return 0;
    }
    let label_w = measure_text(px, text, false);
    let chip_w = (label_w + pad * 2).min(avail);
    fill_rect(canvas, clip, x, y, chip_w, h, tint);
    draw_text(
        layer,
        (x + pad).saturating_sub(clip.x0),
        center_y(y, h, px).saturating_sub(clip.y0),
        px,
        text,
        chip_w - pad * 2,
        false,
    );
    chip_w
}

/// A progress bar: a grey track with a filled run, and a 1px ink underline
/// when the series is finished.
///
/// `pct` is clamped to 0.0..=1.0 (and a non-finite value reads as 0), so a
/// derived ratio can never run the fill past the track. The finished mark is
/// a separate rule rather than a colour change: on a mono panel a full bar
/// and a finished bar must still differ.
#[allow(clippy::too_many_arguments)]
fn draw_progress(
    canvas: &mut RgbPage,
    clip: &Clip,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    pct: f32,
    finished: bool,
    t: &Theme,
) {
    if w == 0 || h == 0 {
        return;
    }
    let pct = if pct.is_finite() {
        pct.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let pct = if finished { 1.0 } else { pct };
    fill_rect(canvas, clip, x, y, w, h, RULE);
    let filled = (w as f32 * pct) as u32;
    fill_rect(canvas, clip, x, y, filled.min(w), h, t.bar);
    if finished {
        fill_rect(canvas, clip, x, y + h + 1, w, 1, INK);
    }
}

// --- stat tiles -----------------------------------------------------------

/// One headline number with its label and a smaller qualifier beneath.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatTile<'a> {
    pub value: &'a str,
    pub label: &'a str,
    pub sub: &'a str,
}

/// A row of stat tiles laid out evenly across the rect.
///
/// Each tile carries a short accent block above its number — a block, not a
/// rule, so it survives the colour filter — and everything else is black
/// text. Tiles that would fall outside the rect are clipped away rather than
/// squeezed, and an empty slice draws nothing.
// A rect, its contents and its theme: the shape every widget here takes,
// fixed by the screens that call them.
#[allow(clippy::too_many_arguments)]
pub fn draw_stat_tiles(
    canvas: &mut RgbPage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    tiles: &[StatTile],
    text_px: f32,
    t: &Theme,
) {
    let clip = Clip::new(canvas, x, y, w, h);
    if clip.is_empty() || tiles.is_empty() || w == 0 || h == 0 {
        return;
    }
    let mut layer = clip.text_layer();

    let col_w = w / tiles.len() as u32;
    if col_w == 0 {
        return;
    }
    let pad = scaled(text_px, 0.35, 2);
    let mark_h = scaled(text_px, 0.16, 2);
    let inner = col_w.saturating_sub(pad);
    if inner == 0 {
        return;
    }

    for (i, tile) in tiles.iter().enumerate() {
        let cx = x + i as u32 * col_w;
        // The accent block: a third of the column, never hairline-thin.
        fill_rect(canvas, &clip, cx, y, (inner / 3).max(1), mark_h, t.accent);

        // The text stack, in layer-local coordinates (the layer's origin is
        // the rect's, so this is just the offset from the rect's top-left).
        let lx = cx.saturating_sub(clip.x0);
        let mut ty = mark_h.saturating_add(pad);
        ty = ty.saturating_add(draw_line(
            &mut layer,
            lx,
            ty,
            text_px * 1.45,
            tile.value,
            inner,
            true,
        ));
        ty = ty.saturating_add(draw_line(
            &mut layer,
            lx,
            ty,
            text_px * 0.66,
            tile.label,
            inner,
            false,
        ));
        draw_line(&mut layer, lx, ty, text_px * 0.58, tile.sub, inner, false);
    }

    overlay_ink_at(canvas, &layer, clip.x0, clip.y0);
}

/// Draw one line of black text into a widget's text layer, returning the
/// height to advance by. Coordinates are layer-local; a line that starts
/// past the layer still costs its height (so the stack below it stays put)
/// but draws nothing.
fn draw_line(
    layer: &mut GrayPage,
    lx: u32,
    ly: u32,
    px: f32,
    text: &str,
    max_w: u32,
    bold: bool,
) -> u32 {
    draw_text(layer, lx, ly, px, text, max_w, bold);
    scaled(px, 1.3, 0)
}

// --- library row ----------------------------------------------------------

/// One series in the library list, with everything the list shows about it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LibraryRow<'a> {
    pub title: &'a str,
    /// Community score, if the metadata has one.
    pub score: Option<f32>,
    /// Reading status ("Reading", "Completed", …), if known.
    pub status: Option<&'a str>,
    /// Genres, already joined by the caller.
    pub genres: &'a str,
    pub downloads: &'a str,
    pub when: &'a str,
    pub read: &'a str,
    /// Fraction read, clamped to 0.0..=1.0 on the way in.
    pub pct: f32,
    pub next: &'a str,
    pub finished: bool,
}

/// One row of the library list: title and recency on the first line,
/// score/status/genre/downloads on the second, progress on the third.
///
/// Colour appears only as blocks — the score marker, the status and genre
/// chip fills, the progress bar — and every one of them is redundant with
/// the text beside or inside it, so the row reads whole in [`Theme::MONO`].
/// Long titles and genre lists are ellipsised by the text layer, never
/// overflowed.
#[allow(clippy::too_many_arguments)] // the caller's contract; see draw_stat_tiles
pub fn draw_library_row(
    canvas: &mut RgbPage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    row: &LibraryRow,
    text_px: f32,
    t: &Theme,
) {
    let clip = Clip::new(canvas, x, y, w, h);
    if clip.is_empty() || w == 0 || h == 0 {
        return;
    }
    let mut layer = clip.text_layer();

    let pad = scaled(text_px, 0.35, 2);
    let gap = scaled(text_px, 0.4, 3);
    let small = text_px * 0.62;
    let band = (h / 3).max(1);
    let x0 = x.saturating_add(pad);
    let avail = w.saturating_sub(pad.saturating_mul(2));
    if avail == 0 {
        return;
    }
    // Exclusive right edge. A padding wider than the rect is possible at a
    // degenerate `text_px`, so this can never be allowed to cross the left.
    let right = x.saturating_add(w).saturating_sub(pad).max(x0);

    // --- band 0: title, and when it was last touched (right-aligned).
    let when_w = measure_text(small, row.when, false).min(avail / 3);
    let when_x = right.saturating_sub(when_w);
    draw_text(
        &mut layer,
        when_x.saturating_sub(clip.x0),
        center_y(y + pad, band, small).saturating_sub(clip.y0),
        small,
        row.when,
        when_w,
        false,
    );
    draw_text(
        &mut layer,
        x0.saturating_sub(clip.x0),
        (y + pad).saturating_sub(clip.y0),
        text_px,
        row.title,
        when_x.saturating_sub(x0).saturating_sub(gap),
        true,
    );

    // --- band 1: score, status chip, genre chip, downloads (right-aligned).
    let b1 = y + band;
    let chip_h = scaled(small, 1.5, 4);
    let chip_y = center_y(b1, band, small).saturating_sub(chip_h.saturating_sub(small as u32) / 2);
    let mut cx = x0;

    if let Some(score) = row.score {
        let size = scaled(small, 0.85, 3);
        fill_marker(
            canvas,
            &clip,
            cx,
            center_y(b1, band, size as f32),
            size,
            t.star,
        );
        cx += size + gap / 2;
        let label = format_score(score);
        let label_w = measure_text(small, &label, true);
        draw_text(
            &mut layer,
            cx.saturating_sub(clip.x0),
            center_y(b1, band, small).saturating_sub(clip.y0),
            small,
            &label,
            right.saturating_sub(cx),
            true,
        );
        cx += label_w + gap;
    }

    // Downloads sit at the right edge so the flexible chips in the middle
    // shrink into whatever is left instead of pushing them off the row.
    let dl_w = measure_text(small, row.downloads, false).min(avail / 3);
    let dl_x = right.saturating_sub(dl_w);
    draw_text(
        &mut layer,
        dl_x.saturating_sub(clip.x0),
        center_y(b1, band, small).saturating_sub(clip.y0),
        small,
        row.downloads,
        dl_w,
        false,
    );
    let chips_right = dl_x.saturating_sub(gap).max(cx);

    if let Some(status) = row.status {
        let used = draw_chip(
            canvas,
            &mut layer,
            &clip,
            cx,
            chip_y,
            chips_right.saturating_sub(cx),
            chip_h,
            status,
            small,
            t.status_tint,
        );
        if used > 0 {
            cx += used + gap / 2;
        }
    }
    if !row.genres.is_empty() {
        draw_chip(
            canvas,
            &mut layer,
            &clip,
            cx,
            chip_y,
            chips_right.saturating_sub(cx),
            chip_h,
            row.genres,
            small,
            t.genre_tint,
        );
    }

    // --- band 2: progress bar, then how much is read and what is next.
    let b2 = y + band * 2;
    let bar_h = scaled(text_px, 0.22, 3);
    let bar_w = (avail * 2 / 5).max(1);
    draw_progress(
        canvas,
        &clip,
        x0,
        center_y(b2, band, bar_h as f32),
        bar_w,
        bar_h,
        row.pct,
        row.finished,
        t,
    );

    let tx = x0 + bar_w + gap;
    let next_w = measure_text(small, row.next, false).min(avail / 3);
    let next_x = right.saturating_sub(next_w);
    draw_text(
        &mut layer,
        next_x.saturating_sub(clip.x0),
        center_y(b2, band, small).saturating_sub(clip.y0),
        small,
        row.next,
        next_w,
        false,
    );
    draw_text(
        &mut layer,
        tx.saturating_sub(clip.x0),
        center_y(b2, band, small).saturating_sub(clip.y0),
        small,
        row.read,
        next_x.saturating_sub(tx).saturating_sub(gap),
        false,
    );

    overlay_ink_at(canvas, &layer, clip.x0, clip.y0);
}

/// One decimal place, which is how every source reports a score.
fn format_score(score: f32) -> String {
    if score.is_finite() {
        format!("{:.1}", score.clamp(0.0, 99.9))
    } else {
        "-".to_string()
    }
}

// --- nav bar --------------------------------------------------------------

/// One destination in the navigation bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavItem<'a> {
    pub label: &'a str,
    pub active: bool,
}

/// The navigation bar: evenly divided items with a 1px rule along the top.
///
/// The active item is marked three ways — a filled accent block beneath it,
/// a bold label, and the position of that block — so it stays obvious with
/// the hue thrown away. Labels are centred and ellipsised within their own
/// slot; a slot too narrow for a label simply shows none rather than
/// bleeding into its neighbour.
#[allow(clippy::too_many_arguments)] // the caller's contract; see draw_stat_tiles
pub fn draw_nav_bar(
    canvas: &mut RgbPage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    items: &[NavItem],
    text_px: f32,
    t: &Theme,
) {
    let clip = Clip::new(canvas, x, y, w, h);
    if clip.is_empty() || items.is_empty() || w == 0 || h == 0 {
        return;
    }
    let mut layer = clip.text_layer();

    fill_rect(canvas, &clip, x, y, w, 1, RULE);

    let slot = w / items.len() as u32;
    if slot == 0 {
        overlay_ink_at(canvas, &layer, clip.x0, clip.y0);
        return;
    }
    let pad = scaled(text_px, 0.3, 2);
    let mark_h = scaled(text_px, 0.18, 2);
    let inner = slot.saturating_sub(pad * 2);

    for (i, item) in items.iter().enumerate() {
        let sx = x + i as u32 * slot;
        if inner == 0 {
            continue;
        }
        if item.active {
            // The indicator is a block spanning the slot's inner width and
            // sits at the bottom edge, where nothing else lives.
            let mark_y = y + h.saturating_sub(mark_h);
            fill_rect(canvas, &clip, sx + pad, mark_y, inner, mark_h, t.accent);
        }
        let label_w = measure_text(text_px, item.label, item.active).min(inner);
        let lx = sx + pad + (inner - label_w) / 2;
        draw_text(
            &mut layer,
            lx.saturating_sub(clip.x0),
            center_y(y, h.saturating_sub(mark_h), text_px).saturating_sub(clip.y0),
            text_px,
            item.label,
            label_w,
            item.active,
        );
    }

    overlay_ink_at(canvas, &layer, clip.x0, clip.y0);
}

// --- meta block -----------------------------------------------------------

/// A book's metadata block: score and rank, then a status chip, then genre
/// chips flowing across the available width.
///
/// Chips wrap to as many lines as the rect holds and the ones that do not
/// fit are dropped — never drawn past the bottom edge, never squeezed. As
/// everywhere else here, the chip fills are the colour and their labels are
/// black, so the block is legible in [`Theme::MONO`].
#[allow(clippy::too_many_arguments)] // the caller's contract; see draw_stat_tiles
pub fn draw_meta_block(
    canvas: &mut RgbPage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    score: Option<f32>,
    rank: Option<u32>,
    status: Option<&str>,
    genres: &[String],
    text_px: f32,
    t: &Theme,
) {
    let clip = Clip::new(canvas, x, y, w, h);
    if clip.is_empty() || w == 0 || h == 0 {
        return;
    }
    let mut layer = clip.text_layer();

    let pad = scaled(text_px, 0.35, 2);
    let gap = scaled(text_px, 0.35, 3);
    let small = text_px * 0.66;
    let x0 = x + pad;
    let avail = w.saturating_sub(pad * 2);
    if avail == 0 {
        return;
    }
    let bottom = y + h;
    let mut cy = y + pad;

    // --- score, with the rank beside it.
    if score.is_some() || rank.is_some() {
        let line_h = scaled(text_px, 1.35, 4);
        let mut cx = x0;
        if let Some(score) = score {
            let size = scaled(text_px, 0.8, 3);
            fill_marker(
                canvas,
                &clip,
                cx,
                center_y(cy, line_h, size as f32),
                size,
                t.star,
            );
            cx += size + gap / 2;
            let label = format_score(score);
            draw_text(
                &mut layer,
                cx.saturating_sub(clip.x0),
                center_y(cy, line_h, text_px).saturating_sub(clip.y0),
                text_px,
                &label,
                (x0 + avail).saturating_sub(cx),
                true,
            );
            cx += measure_text(text_px, &label, true) + gap;
        }
        if let Some(rank) = rank {
            let label = format!("#{rank}");
            draw_text(
                &mut layer,
                cx.saturating_sub(clip.x0),
                center_y(cy, line_h, small).saturating_sub(clip.y0),
                small,
                &label,
                (x0 + avail).saturating_sub(cx),
                false,
            );
        }
        cy += line_h;
    }

    let chip_h = scaled(small, 1.55, 4);

    // --- status chip on its own line, so it never competes with the genres.
    if let Some(status) = status {
        if cy + chip_h <= bottom {
            let used = draw_chip(
                canvas,
                &mut layer,
                &clip,
                x0,
                cy,
                avail,
                chip_h,
                status,
                small,
                t.status_tint,
            );
            if used > 0 {
                cy += chip_h + gap / 2;
            }
        }
    }

    // --- genre chips, flowing and wrapping while lines still fit.
    let mut cx = x0;
    for genre in genres {
        if cy + chip_h > bottom {
            break;
        }
        let wanted = measure_text(small, genre, false) + scaled(small, 0.45, 2) * 2;
        if cx > x0 && cx + wanted > x0 + avail {
            // Wrap. If the next line would not fit, stop rather than
            // clipping a half-drawn row of chips at the bottom edge.
            cx = x0;
            cy += chip_h + gap / 2;
            if cy + chip_h > bottom {
                break;
            }
        }
        let used = draw_chip(
            canvas,
            &mut layer,
            &clip,
            cx,
            cy,
            (x0 + avail).saturating_sub(cx),
            chip_h,
            genre,
            small,
            t.genre_tint,
        );
        if used == 0 {
            break;
        }
        cx += used + gap / 2;
    }

    overlay_ink_at(canvas, &layer, clip.x0, clip.y0);
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];

    const THEMES: [(&str, Theme); 5] = [
        ("ink-rust", Theme::INK_RUST),
        ("indigo", Theme::INDIGO),
        ("sumi", Theme::SUMI),
        ("botanical", Theme::BOTANICAL),
        ("mono", Theme::MONO),
    ];

    fn luma(c: [u8; 3]) -> u8 {
        crate::luma_rec601(c[0], c[1], c[2])
    }

    fn tiles() -> Vec<StatTile<'static>> {
        vec![
            StatTile {
                value: "14",
                label: "day streak",
                sub: "best 31",
            },
            StatTile {
                value: "1,204",
                label: "chapters",
                sub: "62 series",
            },
            StatTile {
                value: "88",
                label: "days read",
                sub: "this year",
            },
        ]
    }

    fn row() -> LibraryRow<'static> {
        LibraryRow {
            title: "Vinland Saga",
            score: Some(8.7),
            status: Some("Reading"),
            genres: "Action, Adventure, Drama",
            downloads: "12 dl",
            when: "2d ago",
            read: "104 / 210",
            pct: 0.49,
            next: "Ch. 105",
            finished: false,
        }
    }

    fn nav() -> Vec<NavItem<'static>> {
        vec![
            NavItem {
                label: "Library",
                active: true,
            },
            NavItem {
                label: "Browse",
                active: false,
            },
            NavItem {
                label: "Stats",
                active: false,
            },
        ]
    }

    fn genres() -> Vec<String> {
        ["Action", "Adventure", "Drama", "Historical"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// Every widget, drawn into the same rect of the same canvas, so the
    /// guarantees below can be asserted once for all of them.
    fn draw_each(x: u32, y: u32, w: u32, h: u32, cw: u32, ch: u32) -> Vec<(&'static str, RgbPage)> {
        let t = Theme::INK_RUST;
        let px = 20.0;
        let mut out = Vec::new();

        let mut c = RgbPage::new_white(cw, ch);
        draw_stat_tiles(&mut c, x, y, w, h, &tiles(), px, &t);
        out.push(("stat_tiles", c));

        let mut c = RgbPage::new_white(cw, ch);
        draw_library_row(&mut c, x, y, w, h, &row(), px, &t);
        out.push(("library_row", c));

        let mut c = RgbPage::new_white(cw, ch);
        draw_nav_bar(&mut c, x, y, w, h, &nav(), px, &t);
        out.push(("nav_bar", c));

        let mut c = RgbPage::new_white(cw, ch);
        draw_meta_block(
            &mut c,
            x,
            y,
            w,
            h,
            Some(8.7),
            Some(412),
            Some("Reading"),
            &genres(),
            px,
            &t,
        );
        out.push(("meta_block", c));

        out
    }

    fn ink_pixels(c: &RgbPage) -> usize {
        c.pixels.chunks_exact(3).filter(|p| *p != WHITE).count()
    }

    // --- themes ---

    #[test]
    fn unknown_profile_names_fall_back_to_the_default() {
        assert_eq!(Theme::from_setting("indigo"), Theme::INDIGO);
        assert_eq!(Theme::from_setting("  MONO "), Theme::MONO);
        assert_eq!(Theme::from_setting("botanical"), Theme::BOTANICAL);
        assert_eq!(Theme::from_setting("sumi"), Theme::SUMI);
        assert_eq!(Theme::from_setting("nope"), Theme::INK_RUST);
        assert_eq!(Theme::from_setting(""), Theme::INK_RUST);
        assert_eq!(Theme::default(), Theme::INK_RUST);
    }

    #[test]
    fn every_theme_holds_black_text_on_its_chips() {
        // Chip labels are always black, so a tint that went dark would eat
        // its own label on a panel that only sees value.
        for (name, t) in THEMES {
            assert_eq!(t.ink, INK, "{name} draws text in something but black");
            for (role, tint) in [("status", t.status_tint), ("genre", t.genre_tint)] {
                assert!(
                    luma(tint) >= 0xC0,
                    "{name}'s {role} tint is too dark to hold black text"
                );
            }
        }
    }

    #[test]
    fn every_theme_separates_its_blocks_from_the_page() {
        // Blocks sit on white paper. One that matched the paper's value
        // would vanish on a grayscale panel however saturated it is.
        for (name, t) in THEMES {
            for (role, color) in [
                ("accent", t.accent),
                ("bar", t.bar),
                ("star", t.star),
                ("label", t.label),
            ] {
                assert!(
                    luma(color) <= 0xB4,
                    "{name}'s {role} block does not separate from white paper"
                );
            }
            // A chip is not a bar: the two must not read as the same block.
            assert!(
                luma(t.status_tint).abs_diff(luma(t.bar)) >= 0x40,
                "{name}'s chip and bar are the same value"
            );
        }
    }

    #[test]
    fn mono_stays_distinguishable_with_no_hue_at_all() {
        // MONO is a target, not a fallback: it is already grey, so its
        // separations are exactly the ones every other theme falls back to.
        let m = Theme::MONO;
        for c in [m.accent, m.star, m.status, m.genre, m.bar, m.label] {
            assert_eq!(c[0], c[1], "mono is not grey");
            assert_eq!(c[1], c[2], "mono is not grey");
        }
        assert!(luma(m.status_tint) < 0xFF, "mono status chip is invisible");
        assert!(luma(m.genre_tint) < 0xFF, "mono genre chip is invisible");
        assert_ne!(
            luma(m.status_tint),
            luma(m.genre_tint),
            "mono's two chip kinds are indistinguishable"
        );
        assert!(
            luma(m.bar) < luma(m.status_tint),
            "mono's progress fill must read darker than a chip"
        );
    }

    #[test]
    fn a_mono_row_still_carries_every_signal() {
        // The colour-free build must not silently lose a widget: it draws
        // the same blocks, just in greys.
        let px = 20.0;
        let mut mono = RgbPage::new_white(400, 90);
        let mut color = RgbPage::new_white(400, 90);
        draw_library_row(&mut mono, 0, 0, 400, 90, &row(), px, &Theme::MONO);
        draw_library_row(&mut color, 0, 0, 400, 90, &row(), px, &Theme::INK_RUST);
        let (m, c) = (ink_pixels(&mono), ink_pixels(&color));
        assert!(m > 0, "mono row drew nothing");
        // Same geometry either way — only the fill values differ.
        assert_eq!(m, c, "mono row lost marks the colour row drew");
        assert_ne!(mono.pixels, color.pixels, "the themes are not different");
    }

    // --- the clipping guarantees, per widget ---

    #[test]
    fn nothing_is_drawn_outside_the_widgets_own_rect() {
        // The guarantee that lets a caller place these next to each other.
        let (x, y, w, h) = (12u32, 9u32, 260u32, 96u32);
        for (name, canvas) in draw_each(x, y, w, h, 320, 140) {
            for py in 0..canvas.height {
                for px in 0..canvas.width {
                    let inside = px >= x && px < x + w && py >= y && py < y + h;
                    if !inside {
                        assert_eq!(
                            canvas.pixel(px, py),
                            WHITE,
                            "{name} painted outside its rect at ({px}, {py})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn geometry_that_overruns_the_canvas_is_clipped_not_wrapped() {
        // Wrapping onto the next row is the failure mode that hides
        // overflow: a widget hanging off the right edge must lose those
        // pixels, not reappear on the left of the next scanline.
        let (x, y) = (30u32, 8u32);
        for (name, canvas) in draw_each(x, y, 400, 200, 40, 60) {
            for py in 0..canvas.height {
                for px in 0..x {
                    assert_eq!(
                        canvas.pixel(px, py),
                        WHITE,
                        "{name} wrapped to the next row at ({px}, {py})"
                    );
                }
            }
        }
    }

    #[test]
    fn a_rect_starting_past_the_canvas_draws_nothing_rather_than_panicking() {
        for (name, canvas) in draw_each(500, 500, 200, 60, 120, 80) {
            assert_eq!(ink_pixels(&canvas), 0, "{name} drew off-canvas");
        }
    }

    #[test]
    fn a_zero_size_rect_draws_nothing_rather_than_panicking() {
        for (w, h) in [(0u32, 40u32), (200, 0), (0, 0), (1, 1), (3, 2)] {
            for (name, canvas) in draw_each(5, 5, w, h, 240, 80) {
                if w == 0 || h == 0 {
                    assert_eq!(ink_pixels(&canvas), 0, "{name} drew into a {w}x{h} rect");
                }
                // The tiny non-empty rects only have to survive the call.
            }
        }
    }

    #[test]
    fn an_empty_item_list_draws_nothing() {
        let t = Theme::INK_RUST;
        let mut canvas = RgbPage::new_white(200, 80);
        draw_stat_tiles(&mut canvas, 4, 4, 190, 70, &[], 20.0, &t);
        draw_nav_bar(&mut canvas, 4, 4, 190, 70, &[], 20.0, &t);
        assert_eq!(ink_pixels(&canvas), 0);
    }

    #[test]
    fn absurd_text_sizes_do_not_panic_or_escape_the_rect() {
        // text_px comes from a layout computed at runtime; a degenerate one
        // must clip like everything else rather than take the reader down.
        for px in [0.0f32, 0.4, 1.0, 4000.0, f32::NAN, f32::INFINITY] {
            let t = Theme::INK_RUST;
            let (x, y, w, h) = (6u32, 6u32, 120u32, 60u32);
            let mut canvas = RgbPage::new_white(160, 90);
            draw_stat_tiles(&mut canvas, x, y, w, h, &tiles(), px, &t);
            draw_library_row(&mut canvas, x, y, w, h, &row(), px, &t);
            draw_nav_bar(&mut canvas, x, y, w, h, &nav(), px, &t);
            draw_meta_block(
                &mut canvas,
                x,
                y,
                w,
                h,
                Some(9.1),
                None,
                Some("Reading"),
                &genres(),
                px,
                &t,
            );
            for py in 0..canvas.height {
                for cx in 0..canvas.width {
                    let inside = cx >= x && cx < x + w && py >= y && py < y + h;
                    if !inside {
                        assert_eq!(canvas.pixel(cx, py), WHITE, "escaped at text_px {px}");
                    }
                }
            }
        }
    }

    // --- text that does not fit ---

    #[test]
    fn a_long_title_is_ellipsised_not_overflowed() {
        let t = Theme::INK_RUST;
        let long = "The Extraordinarily Long Series Title That Could Not Possibly Fit Here";
        let (x, y, w, h) = (10u32, 5u32, 220u32, 90u32);

        let mut short_row = row();
        short_row.title = "A";
        let mut long_row = row();
        long_row.title = long;

        let mut a = RgbPage::new_white(400, 120);
        let mut b = RgbPage::new_white(400, 120);
        draw_library_row(&mut a, x, y, w, h, &short_row, 20.0, &t);
        draw_library_row(&mut b, x, y, w, h, &long_row, 20.0, &t);

        // The long title draws more ink but not one pixel further right.
        assert!(
            ink_pixels(&b) > ink_pixels(&a),
            "long title drew no more ink"
        );
        for py in 0..b.height {
            for px in 0..b.width {
                if px < x || px >= x + w {
                    assert_eq!(b.pixel(px, py), WHITE, "title escaped at ({px}, {py})");
                }
            }
        }
    }

    #[test]
    fn a_long_genre_list_is_ellipsised_not_overflowed() {
        let t = Theme::INK_RUST;
        let mut long_row = row();
        long_row.genres =
            "Action, Adventure, Drama, Historical, Seinen, Tragedy, Psychological, Military";
        let (x, y, w, h) = (4u32, 4u32, 200u32, 84u32);
        let mut canvas = RgbPage::new_white(260, 100);
        draw_library_row(&mut canvas, x, y, w, h, &long_row, 18.0, &t);
        for py in 0..canvas.height {
            for px in 0..canvas.width {
                if px < x || px >= x + w || py < y || py >= y + h {
                    assert_eq!(
                        canvas.pixel(px, py),
                        WHITE,
                        "genres escaped at ({px}, {py})"
                    );
                }
            }
        }
    }

    #[test]
    fn genre_chips_that_do_not_fit_are_dropped_not_stacked_past_the_edge() {
        let t = Theme::INK_RUST;
        let many: Vec<String> = (0..40).map(|i| format!("Genre {i}")).collect();
        let (x, y, w, h) = (5u32, 5u32, 180u32, 70u32);
        let mut canvas = RgbPage::new_white(220, 100);
        draw_meta_block(
            &mut canvas,
            x,
            y,
            w,
            h,
            Some(7.7),
            Some(9),
            Some("Paused"),
            &many,
            18.0,
            &t,
        );
        for py in 0..canvas.height {
            for px in 0..canvas.width {
                if px < x || px >= x + w || py < y || py >= y + h {
                    assert_eq!(canvas.pixel(px, py), WHITE, "chips escaped at ({px}, {py})");
                }
            }
        }
        assert!(ink_pixels(&canvas) > 0, "the block drew nothing at all");
    }

    // --- progress ---

    #[test]
    fn pct_outside_the_range_clamps_instead_of_overrunning() {
        // pct is a derived ratio; a bad one must saturate, never paint a bar
        // longer than its track.
        let t = Theme::INK_RUST;
        let render = |pct: f32| {
            let mut r = row();
            r.pct = pct;
            let mut c = RgbPage::new_white(300, 100);
            draw_library_row(&mut c, 5, 5, 280, 90, &r, 20.0, &t);
            c
        };
        assert_eq!(
            render(5.0).pixels,
            render(1.0).pixels,
            "over 1.0 did not clamp"
        );
        assert_eq!(
            render(-3.0).pixels,
            render(0.0).pixels,
            "below 0.0 did not clamp"
        );
        assert_eq!(
            render(f32::NAN).pixels,
            render(0.0).pixels,
            "a non-finite pct did not read as empty"
        );
        assert_ne!(
            render(0.0).pixels,
            render(1.0).pixels,
            "pct changes nothing"
        );
    }

    #[test]
    fn a_fuller_bar_paints_more_of_its_track() {
        let t = Theme::INK_RUST;
        let bar_pixels = |pct: f32| {
            let mut c = RgbPage::new_white(300, 60);
            let clip = Clip::new(&c, 0, 0, 300, 60);
            draw_progress(&mut c, &clip, 10, 10, 200, 8, pct, false, &t);
            c.pixels.chunks_exact(3).filter(|p| *p == t.bar).count()
        };
        assert_eq!(bar_pixels(0.0), 0);
        assert!(bar_pixels(0.5) > 0);
        assert!(bar_pixels(1.0) > bar_pixels(0.5));
        assert_eq!(bar_pixels(1.0), 200 * 8, "a full bar must fill its track");
    }

    #[test]
    fn a_finished_row_differs_from_a_merely_full_one() {
        // Full and finished are different states and must not rely on
        // colour to tell them apart.
        let t = Theme::MONO;
        let render = |finished: bool| {
            let mut r = row();
            r.pct = 1.0;
            r.finished = finished;
            let mut c = RgbPage::new_white(300, 100);
            draw_library_row(&mut c, 5, 5, 280, 90, &r, 20.0, &t);
            c
        };
        assert_ne!(render(true).pixels, render(false).pixels);
    }

    // --- the marks each widget is supposed to make ---

    #[test]
    fn stat_tiles_mark_every_column_with_an_accent_block() {
        let t = Theme::INK_RUST;
        let mut canvas = RgbPage::new_white(360, 120);
        draw_stat_tiles(&mut canvas, 0, 0, 360, 120, &tiles(), 20.0, &t);
        let col_w = 360 / 3;
        for i in 0..3u32 {
            assert_eq!(
                canvas.pixel(i * col_w, 0),
                t.accent,
                "column {i} has no accent block"
            );
        }
        // The blocks are separated: the far end of each column is not one.
        assert_eq!(canvas.pixel(col_w - 1, 0), WHITE);
    }

    #[test]
    fn the_active_nav_item_is_marked_and_the_others_are_not() {
        let t = Theme::INK_RUST;
        let (w, h) = (300u32, 50u32);
        let mut canvas = RgbPage::new_white(w, h);
        draw_nav_bar(&mut canvas, 0, 0, w, h, &nav(), 20.0, &t);
        let slot = w / 3;
        let bottom = h - 1;
        assert_eq!(
            canvas.pixel(slot / 2, bottom),
            t.accent,
            "active item unmarked"
        );
        assert_eq!(
            canvas.pixel(slot + slot / 2, bottom),
            WHITE,
            "inactive item marked"
        );
        // And the bar's top rule spans it.
        assert_eq!(canvas.pixel(w / 2, 0), RULE);
    }

    #[test]
    fn an_active_label_reads_differently_from_an_inactive_one() {
        // The indicator block is not the only signal: the label is bold too,
        // which is what carries when the block's hue is gone.
        let t = Theme::MONO;
        let render = |active: bool| {
            let items = [NavItem {
                label: "Library",
                active,
            }];
            let mut c = RgbPage::new_white(160, 44);
            draw_nav_bar(&mut c, 0, 0, 160, 44, &items, 20.0, &t);
            // Ignore the indicator band, compare only the label rows.
            c.pixels[..(160 * 30 * 3) as usize].to_vec()
        };
        assert_ne!(
            render(true),
            render(false),
            "the active label is not bolder"
        );
    }

    #[test]
    fn optional_metadata_simply_draws_less() {
        let t = Theme::INK_RUST;
        let full = {
            let mut c = RgbPage::new_white(240, 140);
            draw_meta_block(
                &mut c,
                4,
                4,
                230,
                130,
                Some(8.7),
                Some(412),
                Some("Reading"),
                &genres(),
                20.0,
                &t,
            );
            c
        };
        let bare = {
            let mut c = RgbPage::new_white(240, 140);
            draw_meta_block(&mut c, 4, 4, 230, 130, None, None, None, &[], 20.0, &t);
            c
        };
        assert!(ink_pixels(&full) > ink_pixels(&bare));
        assert_eq!(ink_pixels(&bare), 0, "a block with no metadata drew marks");
    }

    #[test]
    fn a_row_without_a_score_or_status_draws_neither_mark() {
        let t = Theme::INK_RUST;
        let mut bare = row();
        bare.score = None;
        bare.status = None;
        bare.genres = "";
        let mut c = RgbPage::new_white(300, 100);
        draw_library_row(&mut c, 5, 5, 280, 90, &bare, 20.0, &t);
        let has = |color: [u8; 3]| c.pixels.chunks_exact(3).any(|p| p == color);
        assert!(!has(t.star), "a scoreless row drew a score marker");
        assert!(!has(t.status_tint), "a statusless row drew a status chip");
        assert!(!has(t.genre_tint), "a genreless row drew a genre chip");
        // The progress bar is unconditional, so the row is not blank.
        assert!(has(t.bar));
    }

    // --- the text overlay ---

    #[test]
    fn overlay_ink_treats_white_as_transparent() {
        let mut dst = RgbPage::new_white(4, 2);
        fill_rect(
            &mut dst,
            &Clip::new(&RgbPage::new_white(4, 2), 0, 0, 4, 2),
            0,
            0,
            4,
            2,
            [10, 20, 30],
        );
        let src = GrayPage {
            width: 4,
            height: 2,
            pixels: vec![0xFF, 0x00, 0x80, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
        };
        overlay_ink(&mut dst, &src);
        assert_eq!(dst.pixel(0, 0), [10, 20, 30], "white overwrote the block");
        assert_eq!(dst.pixel(1, 0), [0, 0, 0], "ink did not land");
        assert_eq!(
            dst.pixel(2, 0),
            [0x80, 0x80, 0x80],
            "grey ink lost its value"
        );
        assert_eq!(dst.pixel(3, 1), [10, 20, 30]);
    }

    #[test]
    fn overlay_ink_clips_a_layer_larger_than_the_canvas() {
        let mut dst = RgbPage::new_white(2, 2);
        let src = GrayPage {
            width: 8,
            height: 8,
            pixels: vec![0x00; 64],
        };
        overlay_ink(&mut dst, &src);
        assert_eq!(dst.pixels, vec![0x00; 12]);
        // And a layer placed entirely off-canvas is a no-op.
        let mut dst = RgbPage::new_white(2, 2);
        overlay_ink_at(&mut dst, &src, 9, 0);
        overlay_ink_at(&mut dst, &src, 0, 9);
        assert_eq!(dst.pixels, vec![0xFF; 12]);
    }

    #[test]
    fn a_marker_is_a_solid_block_inside_its_size() {
        let mut c = RgbPage::new_white(20, 20);
        let clip = Clip::new(&RgbPage::new_white(20, 20), 0, 0, 20, 20);
        fill_marker(&mut c, &clip, 4, 4, 5, [1, 2, 3]);
        // Centre row is full width, tips are single pixels.
        assert_eq!(c.pixel(6, 6), [1, 2, 3]);
        assert_eq!(c.pixel(4, 6), [1, 2, 3]);
        assert_eq!(c.pixel(6, 4), [1, 2, 3]);
        assert_eq!(c.pixel(4, 4), WHITE, "the diamond has square corners");
        // Nothing outside the size box.
        for y in 0..20 {
            for x in 0..20 {
                if !(4..9).contains(&x) || !(4..9).contains(&y) {
                    assert_eq!(c.pixel(x, y), WHITE, "marker escaped at ({x}, {y})");
                }
            }
        }
        // A zero-size marker is a no-op.
        let mut c = RgbPage::new_white(4, 4);
        fill_marker(&mut c, &clip, 0, 0, 0, [1, 2, 3]);
        assert_eq!(ink_pixels(&c), 0);
    }

    #[test]
    fn widgets_scale_with_the_rect_rather_than_a_fixed_panel_size() {
        // The same call on the two panel widths this codebase supports must
        // fill each one, not draw a Libra-sized widget on a Clara.
        let t = Theme::INK_RUST;
        let rightmost_ink = |w: u32| {
            let mut c = RgbPage::new_white(w, 120);
            draw_stat_tiles(&mut c, 0, 0, w, 120, &tiles(), 24.0, &t);
            (0..w)
                .rev()
                .find(|&x| (0..120).any(|y| c.pixel(x, y) != WHITE))
                .unwrap_or(0)
        };
        let clara = rightmost_ink(1072);
        let libra = rightmost_ink(1264);
        assert!(libra > clara, "the widget ignored the wider panel");
        assert!(clara > 1072 / 2, "the widget left a Clara half empty");
    }
}
