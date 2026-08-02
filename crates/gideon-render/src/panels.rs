//! Comic-panel detection for "panel zoom": given a rendered page, find the
//! individual frames so the reader can zoom to one at a time (a long-press
//! feature ported from KOReader's comic mode).
//!
//! The method is a **recursive X–Y cut** on the page's ink: manga panels are
//! separated by white *gutters*, so we look for full-width (or full-height)
//! runs of near-blank pixels and split there, recursing into each half until
//! no gutter is left. Each leaf is trimmed to the ink it contains and becomes
//! a panel. Detection runs on a coarse sub-sampled grid so a full-resolution
//! page stays cheap; the rectangles are scaled back to page pixels at the end.
//!
//! Real pages don't always cooperate — full-bleed art has no gutters — so when
//! the cut finds nothing the caller gets back a single page-sized rectangle
//! (or, from [`panel_at`], `None`) and can fall back to a tap-centred zoom.

use crate::GrayPage;

/// A rectangle in page pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn contains(&self, px: u32, py: u32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }
}

/// Pixels at or below this luma count as "ink" (content). Manga backgrounds
/// are near-white (>240) and gutters are paper-white, while panel interiors —
/// line art *and* grey screentone — sit well below this, so the threshold
/// cleanly separates a gutter from a panel's contents.
const INK_MAX_LUMA: u8 = 224;

/// Detect the comic panels on `page`, returned in manga reading order
/// (right-to-left, top-to-bottom). Returns a single page-sized rectangle when
/// the page has no usable gutters (full-bleed art, a single panel, or a page
/// too small to split) so callers always get at least one region.
pub fn detect_panels(page: &GrayPage) -> Vec<Rect> {
    let full = Rect {
        x: 0,
        y: 0,
        w: page.width,
        h: page.height,
    };
    if page.width < 8 || page.height < 8 {
        return vec![full];
    }

    // Sub-sample to a coarse grid (cap the long side ~512) so projection
    // profiles over a big page stay cheap; work entirely in grid cells and
    // scale back at the end.
    let step = (page.width.max(page.height) / 512).max(1);
    let grid = ContentGrid::sample(page, step);

    // The smallest gutter/panel we take seriously, as a fraction of the
    // shorter page side: gutters below this are ink gaps inside a panel, and
    // slivers below the panel floor aren't frames.
    let short = page.width.min(page.height);
    let min_gutter = ((short as f32 * 0.018) as u32 / step).max(1);
    let min_panel = ((short as f32 * 0.10) as u32 / step).max(2);

    let mut cells: Vec<Rect> = Vec::new();
    cut(&grid, grid.bounds(), min_gutter, min_panel, &mut cells);

    // Nothing split, or the whole page came back: no usable panels.
    if cells.len() <= 1 {
        return vec![full];
    }

    let mut panels: Vec<Rect> = cells
        .into_iter()
        .map(|c| grid.to_page(c, page.width, page.height))
        .filter(|r| r.w > 0 && r.h > 0)
        .collect();
    // Drop specks the cut may have left (a stray mark trimmed to a tiny box).
    let page_area = page.width as u64 * page.height as u64;
    panels.retain(|r| r.area() * 100 >= page_area); // >= 1% of the page
    if panels.len() <= 1 {
        return vec![full];
    }
    order_reading(&mut panels);
    panels
}

/// The panel containing `(px, py)`, or `None` when the tap misses every
/// detected panel or the page has none (so the caller can fall back to a
/// tap-centred zoom rather than jumping somewhere surprising).
pub fn panel_at(page: &GrayPage, px: u32, py: u32) -> Option<Rect> {
    let panels = detect_panels(page);
    // A lone page-sized result means "no panels found" — not a hit.
    if panels.len() == 1 && panels[0].w == page.width && panels[0].h == page.height {
        return None;
    }
    panels
        .into_iter()
        .filter(|r| r.contains(px, py))
        // If frames overlap the tap, prefer the tightest (smallest) one.
        .min_by_key(|r| r.area())
}

/// A coarse boolean "has ink" grid sampled from a page, plus the sub-sample
/// step so cells map back to page pixels.
struct ContentGrid {
    w: u32,
    h: u32,
    step: u32,
    /// Row-major, `w * h`; true where the sampled pixel was ink.
    ink: Vec<bool>,
}

impl ContentGrid {
    fn sample(page: &GrayPage, step: u32) -> Self {
        let w = page.width.div_ceil(step);
        let h = page.height.div_ceil(step);
        let mut ink = vec![false; (w * h) as usize];
        for gy in 0..h {
            for gx in 0..w {
                let px = (gx * step).min(page.width - 1);
                let py = (gy * step).min(page.height - 1);
                ink[(gy * w + gx) as usize] = page.pixel(px, py) <= INK_MAX_LUMA;
            }
        }
        Self { w, h, step, ink }
    }

    fn bounds(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: self.w,
            h: self.h,
        }
    }

    fn ink_at(&self, x: u32, y: u32) -> bool {
        self.ink[(y * self.w + x) as usize]
    }

    /// Ink count of column `x` within `[y0, y1)`.
    fn col_ink(&self, x: u32, y0: u32, y1: u32) -> u32 {
        (y0..y1).filter(|&y| self.ink_at(x, y)).count() as u32
    }

    /// Ink count of row `y` within `[x0, x1)`.
    fn row_ink(&self, y: u32, x0: u32, x1: u32) -> u32 {
        (x0..x1).filter(|&x| self.ink_at(x, y)).count() as u32
    }

    /// Map a grid rectangle back to page pixels, clamped to the page.
    fn to_page(&self, r: Rect, page_w: u32, page_h: u32) -> Rect {
        let x = (r.x * self.step).min(page_w);
        let y = (r.y * self.step).min(page_h);
        let x1 = ((r.x + r.w) * self.step).min(page_w);
        let y1 = ((r.y + r.h) * self.step).min(page_h);
        Rect {
            x,
            y,
            w: x1.saturating_sub(x),
            h: y1.saturating_sub(y),
        }
    }
}

/// Recursively X–Y cut `region` (in grid cells), pushing leaf panels into
/// `out`. At each step the region is trimmed to its ink, then split along the
/// widest interior gutter (whichever axis has one); with no gutter wide enough
/// it's a leaf.
fn cut(grid: &ContentGrid, region: Rect, min_gutter: u32, min_panel: u32, out: &mut Vec<Rect>) {
    let Some(region) = trim_to_ink(grid, region) else {
        return; // blank region — nothing here
    };

    // Try a horizontal cut (a blank band of rows) and a vertical cut (a blank
    // band of columns); take whichever gutter is wider, breaking ties toward
    // horizontal (page reads in rows).
    let h_gap = widest_gap_rows(grid, region, min_gutter);
    let v_gap = widest_gap_cols(grid, region, min_gutter);

    let split = match (h_gap, v_gap) {
        (Some(h), Some(v)) => {
            if v.len > h.len {
                Some(Axis::Col(v))
            } else {
                Some(Axis::Row(h))
            }
        }
        (Some(h), None) => Some(Axis::Row(h)),
        (None, Some(v)) => Some(Axis::Col(v)),
        (None, None) => None,
    };

    match split {
        Some(Axis::Row(gap)) => {
            let top = Rect {
                x: region.x,
                y: region.y,
                w: region.w,
                h: gap.start - region.y,
            };
            let bottom = Rect {
                x: region.x,
                y: gap.start + gap.len,
                w: region.w,
                h: region.y + region.h - (gap.start + gap.len),
            };
            cut(grid, top, min_gutter, min_panel, out);
            cut(grid, bottom, min_gutter, min_panel, out);
        }
        Some(Axis::Col(gap)) => {
            let left = Rect {
                x: region.x,
                y: region.y,
                w: gap.start - region.x,
                h: region.h,
            };
            let right = Rect {
                x: gap.start + gap.len,
                y: region.y,
                w: region.x + region.w - (gap.start + gap.len),
                h: region.h,
            };
            cut(grid, left, min_gutter, min_panel, out);
            cut(grid, right, min_gutter, min_panel, out);
        }
        None => {
            // Leaf: keep it only if it's panel-sized in at least one axis, so
            // stray marks don't become "panels".
            if region.w >= min_panel || region.h >= min_panel {
                out.push(region);
            }
        }
    }
}

enum Axis {
    Row(Gap),
    Col(Gap),
}

/// A blank band: `start` (first cell) and `len` (cells), along whichever axis.
#[derive(Clone, Copy)]
struct Gap {
    start: u32,
    len: u32,
}

/// The widest interior run of blank rows in `region` at least `min_gutter`
/// tall (never touching the region's own top/bottom edge, which are just its
/// margins). `None` if there's no such band.
fn widest_gap_rows(grid: &ContentGrid, region: Rect, min_gutter: u32) -> Option<Gap> {
    let (x0, x1) = (region.x, region.x + region.w);
    let blank = |y: u32| grid.row_ink(y, x0, x1) == 0;
    widest_blank_run(region.y, region.y + region.h, min_gutter, blank)
}

/// The widest interior run of blank columns in `region` at least `min_gutter`
/// wide. `None` if there's no such band.
fn widest_gap_cols(grid: &ContentGrid, region: Rect, min_gutter: u32) -> Option<Gap> {
    let (y0, y1) = (region.y, region.y + region.h);
    let blank = |x: u32| grid.col_ink(x, y0, y1) == 0;
    widest_blank_run(region.x, region.x + region.w, min_gutter, blank)
}

/// Scan `[lo, hi)` for the widest maximal run of `blank` lines that is at
/// least `min_len` long and does not touch either end (an interior gutter).
fn widest_blank_run(lo: u32, hi: u32, min_len: u32, blank: impl Fn(u32) -> bool) -> Option<Gap> {
    let mut best: Option<Gap> = None;
    let mut run_start = lo;
    let mut in_run = false;
    for i in lo..hi {
        if blank(i) {
            if !in_run {
                run_start = i;
                in_run = true;
            }
        } else if in_run {
            consider_run(run_start, i, lo, hi, min_len, &mut best);
            in_run = false;
        }
    }
    // A trailing run reaches `hi` (touches the end) and is a margin, not a
    // gutter, so it's intentionally not considered.
    best
}

fn consider_run(start: u32, end: u32, lo: u32, hi: u32, min_len: u32, best: &mut Option<Gap>) {
    // Skip runs touching either edge — those are the region's margins.
    if start == lo || end == hi {
        return;
    }
    let len = end - start;
    if len >= min_len && best.is_none_or(|b| len > b.len) {
        *best = Some(Gap { start, len });
    }
}

/// Shrink `region` to the bounding box of the ink inside it, or `None` if the
/// region is entirely blank.
fn trim_to_ink(grid: &ContentGrid, region: Rect) -> Option<Rect> {
    let (x0, x1) = (region.x, region.x + region.w);
    let (y0, y1) = (region.y, region.y + region.h);

    let top = (y0..y1).find(|&y| grid.row_ink(y, x0, x1) > 0)?;
    let bottom = (y0..y1).rev().find(|&y| grid.row_ink(y, x0, x1) > 0)?;
    let left = (x0..x1).find(|&x| grid.col_ink(x, y0, y1) > 0)?;
    let right = (x0..x1).rev().find(|&x| grid.col_ink(x, y0, y1) > 0)?;

    Some(Rect {
        x: left,
        y: top,
        w: right - left + 1,
        h: bottom - top + 1,
    })
}

/// Sort panels into manga reading order: grouped into horizontal bands
/// top-to-bottom, then right-to-left within each band. Panels whose vertical
/// spans overlap by more than half share a band (so a tall panel beside two
/// stacked ones doesn't scramble the order).
fn order_reading(panels: &mut [Rect]) {
    // First by top edge, so band-building sees rows in order.
    panels.sort_by_key(|r| r.y);
    let mut bands: Vec<Vec<Rect>> = Vec::new();
    for &r in panels.iter() {
        match bands.last_mut() {
            Some(band) if shares_band(band, &r) => band.push(r),
            _ => bands.push(vec![r]),
        }
    }
    let mut i = 0;
    for mut band in bands {
        // Right-to-left within the band (manga).
        band.sort_by_key(|r| std::cmp::Reverse(r.x));
        for r in band {
            panels[i] = r;
            i += 1;
        }
    }
}

/// Whether `r` belongs to the band currently being built: its vertical span
/// overlaps the band's first panel by more than half of the shorter height.
fn shares_band(band: &[Rect], r: &Rect) -> bool {
    let lead = band[0];
    let top = lead.y.max(r.y);
    let bottom = (lead.y + lead.h).min(r.y + r.h);
    let overlap = bottom.saturating_sub(top);
    let shorter = lead.h.min(r.h).max(1);
    overlap * 2 > shorter
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a white page and paint solid black panel rectangles onto it,
    /// leaving white gutters between them — a clean synthetic comic page.
    fn page_with_panels(w: u32, h: u32, rects: &[Rect]) -> GrayPage {
        let mut page = GrayPage::new_white(w, h);
        for r in rects {
            for y in r.y..(r.y + r.h).min(h) {
                for x in r.x..(r.x + r.w).min(w) {
                    page.pixels[(y * w + x) as usize] = 0x10;
                }
            }
        }
        page
    }

    /// Two rects are "close" if every edge is within `tol` px — detection
    /// trims to ink so it lands a hair inside the painted rectangle.
    fn near(a: Rect, b: Rect, tol: u32) -> bool {
        a.x.abs_diff(b.x) <= tol
            && a.y.abs_diff(b.y) <= tol
            && a.w.abs_diff(b.w) <= tol
            && a.h.abs_diff(b.h) <= tol
    }

    #[test]
    fn a_full_bleed_page_reports_one_region() {
        // Entirely inked: no gutters, so the whole page is the only region.
        let page = page_with_panels(
            400,
            600,
            &[Rect {
                x: 0,
                y: 0,
                w: 400,
                h: 600,
            }],
        );
        let panels = detect_panels(&page);
        assert_eq!(panels.len(), 1);
        assert_eq!(
            panels[0],
            Rect {
                x: 0,
                y: 0,
                w: 400,
                h: 600
            }
        );
        // And a tap anywhere is "no panel" → caller falls back.
        assert_eq!(panel_at(&page, 200, 300), None);
    }

    #[test]
    fn a_two_by_two_grid_is_split_into_four_panels_in_reading_order() {
        // Four panels with a white cross of gutters between them.
        let tl = Rect {
            x: 30,
            y: 30,
            w: 150,
            h: 240,
        };
        let tr = Rect {
            x: 220,
            y: 30,
            w: 150,
            h: 240,
        };
        let bl = Rect {
            x: 30,
            y: 320,
            w: 150,
            h: 240,
        };
        let br = Rect {
            x: 220,
            y: 320,
            w: 150,
            h: 240,
        };
        let page = page_with_panels(400, 600, &[tl, tr, bl, br]);

        let panels = detect_panels(&page);
        assert_eq!(panels.len(), 4, "2x2 grid → four panels");
        // Manga order: top band right→left, then bottom band right→left.
        assert!(near(panels[0], tr, 4), "first panel is top-right");
        assert!(near(panels[1], tl, 4), "second is top-left");
        assert!(near(panels[2], br, 4), "third is bottom-right");
        assert!(near(panels[3], bl, 4), "fourth is bottom-left");
    }

    #[test]
    fn panel_at_returns_the_frame_under_the_point() {
        let tl = Rect {
            x: 30,
            y: 30,
            w: 150,
            h: 240,
        };
        let tr = Rect {
            x: 220,
            y: 30,
            w: 150,
            h: 240,
        };
        let bl = Rect {
            x: 30,
            y: 320,
            w: 150,
            h: 240,
        };
        let br = Rect {
            x: 220,
            y: 320,
            w: 150,
            h: 240,
        };
        let page = page_with_panels(400, 600, &[tl, tr, bl, br]);

        let hit = panel_at(&page, 260, 400).expect("a panel under the point");
        assert!(near(hit, br, 4), "point lands in the bottom-right panel");
        // A point in a gutter hits nothing.
        assert_eq!(panel_at(&page, 200, 300), None);
    }

    #[test]
    fn stacked_rows_split_top_to_bottom() {
        let top = Rect {
            x: 20,
            y: 20,
            w: 360,
            h: 160,
        };
        let mid = Rect {
            x: 20,
            y: 220,
            w: 360,
            h: 160,
        };
        let bot = Rect {
            x: 20,
            y: 420,
            w: 360,
            h: 160,
        };
        let page = page_with_panels(400, 600, &[top, mid, bot]);
        let panels = detect_panels(&page);
        assert_eq!(panels.len(), 3);
        assert!(near(panels[0], top, 4));
        assert!(near(panels[1], mid, 4));
        assert!(near(panels[2], bot, 4));
    }
}
