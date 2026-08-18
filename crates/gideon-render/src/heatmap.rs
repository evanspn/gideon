//! The reading-activity heatmap: a GitHub-style grid of one cell per day,
//! columns running week by week, shaded by how much was read that day.
//!
//! Colour here is deliberate and constrained. The Libra Colour's Kaleido
//! filter resolves colour at roughly half the resolution it resolves black,
//! so colour that lands in thin strokes or small text reads soft and fringed
//! — but a filled block stays crisp. A grid of flat squares is therefore one
//! of the few places on this device where colour is unambiguously worth
//! spending, which is why the heatmap carries a real ramp instead of greys.
//!
//! The ramp is five steps and MONOTONIC IN VALUE, lightest to darkest. That
//! is not a stylistic choice: a panel without a colour filter (a Clara, an
//! older Libra) collapses the ramp to its luma, and a ramp that only varied
//! in hue would collapse into a single flat grey. [`Palette::MONO`] is the
//! same widget on those devices, and it is a target rather than a fallback.
//!
//! Every draw is clipped twice — to the widget's own box and to the canvas —
//! so a heatmap can never paint over its neighbours no matter what geometry
//! or day count it is handed. See `docs/LESSONS.md` §1: bobo's Lua UI
//! overflow bugs are not allowed back in.

use crate::RgbPage;

/// Days in a heatmap column. One week, Sunday-first, matching the web
/// dashboard's calendar so the two surfaces show the same shape.
pub const DAYS_PER_WEEK: u32 = 7;

/// The five intensity steps, lightest (no reading) to darkest (a heavy day).
pub const LEVELS: usize = 5;

/// A heatmap colour ramp: index 0 is an untouched day, index 4 the heaviest.
///
/// Every ramp must descend in luma from 0 to 4 so it survives a grayscale
/// panel; [`Palette::is_monotonic`] asserts it and the tests hold every
/// built-in ramp to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub steps: [[u8; 3]; LEVELS],
}

impl Palette {
    /// Ink & Rust — the default profile. Warm earth ramp.
    pub const INK_RUST: Palette = Palette {
        steps: [
            [0xEE, 0xEE, 0xEE],
            [0xDC, 0xC9, 0xA6],
            [0xC9, 0xA8, 0x6B],
            [0xA8, 0x5F, 0x38],
            [0x6C, 0x3F, 0x24],
        ],
    };

    /// Indigo Press — cool, blue-led.
    pub const INDIGO: Palette = Palette {
        steps: [
            [0xEE, 0xEE, 0xEE],
            [0xC8, 0xCF, 0xE2],
            [0x8E, 0x9C, 0xC4],
            [0x3F, 0x54, 0x88],
            [0x28, 0x34, 0x5A],
        ],
    };

    /// Sumi & Vermilion — near-monochrome with a single hue.
    pub const SUMI: Palette = Palette {
        steps: [
            [0xEF, 0xEC, 0xE6],
            [0xE0, 0xC4, 0xBA],
            [0xCD, 0x88, 0x74],
            [0xB1, 0x4A, 0x32],
            [0x6F, 0x2D, 0x1E],
        ],
    };

    /// Botanical — moss-led, for the four-hue genre-coding profile.
    pub const BOTANICAL: Palette = Palette {
        steps: [
            [0xEE, 0xEE, 0xEE],
            [0xD5, 0xDC, 0xC2],
            [0xA8, 0xB5, 0x85],
            [0x5F, 0x6F, 0x3F],
            [0x3B, 0x45, 0x27],
        ],
    };

    /// The build for panels with no colour filter. Same widget, same
    /// geometry, separation carried entirely by value.
    pub const MONO: Palette = Palette {
        steps: [
            [0xEE, 0xEE, 0xEE],
            [0xCC, 0xCC, 0xCC],
            [0x99, 0x99, 0x99],
            [0x55, 0x55, 0x55],
            [0x00, 0x00, 0x00],
        ],
    };

    /// Resolve a profile name from `settings.json`. Unknown names give the
    /// default rather than an error — settings are parsed leniently
    /// everywhere else in this codebase and a bad value must never be fatal.
    pub fn from_setting(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "indigo" => Self::INDIGO,
            "sumi" => Self::SUMI,
            "botanical" => Self::BOTANICAL,
            "mono" => Self::MONO,
            _ => Self::INK_RUST,
        }
    }

    /// Whether the ramp darkens all the way down, which is what keeps it
    /// legible once a grayscale panel throws the hue away.
    pub fn is_monotonic(&self) -> bool {
        self.steps.windows(2).all(|w| {
            crate::luma_rec601(w[0][0], w[0][1], w[0][2])
                >= crate::luma_rec601(w[1][0], w[1][1], w[1][2])
        })
    }

    fn step(&self, level: u8) -> [u8; 3] {
        self.steps[(level as usize).min(LEVELS - 1)]
    }
}

/// Where the heatmap sits and how big its cells are. All values are pixels;
/// the caller owns placement, this type only answers what it will occupy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeatmapLayout {
    pub x: u32,
    pub y: u32,
    /// How many week-columns to draw. The widget never draws more than this
    /// many even when handed a longer history.
    pub weeks: u32,
    pub cell: u32,
    pub gap: u32,
}

impl HeatmapLayout {
    /// A layout that fits `weeks` columns into `max_width`, picking the
    /// largest cell size that fits and never going below 1px.
    ///
    /// Sizing from the available width rather than hardcoding a cell keeps
    /// the widget correct across the panel sizes this codebase supports
    /// (1264x1680 on a Libra Colour, 1072x1448 elsewhere) instead of only
    /// the one it was designed on.
    pub fn fit(x: u32, y: u32, weeks: u32, max_width: u32, gap: u32) -> Self {
        let weeks = weeks.max(1);
        let gaps = gap.saturating_mul(weeks.saturating_sub(1));
        let cell = max_width.saturating_sub(gaps) / weeks;
        Self {
            x,
            y,
            weeks,
            cell: cell.max(1),
            gap,
        }
    }

    pub fn width(&self) -> u32 {
        self.weeks * self.cell + self.gap * self.weeks.saturating_sub(1)
    }

    pub fn height(&self) -> u32 {
        DAYS_PER_WEEK * self.cell + self.gap * (DAYS_PER_WEEK - 1)
    }
}

/// Draw the grid onto `canvas`.
///
/// `grid` is one entry per week-column, each holding seven intensity levels
/// (0..=4, clamped). Extra columns beyond `layout.weeks` are ignored and a
/// short history simply draws fewer columns — the caller never has to pad or
/// truncate to match the layout.
///
/// Nothing is drawn outside the layout's own box or outside the canvas, so
/// this is safe to call with any geometry.
pub fn draw_heatmap(
    canvas: &mut RgbPage,
    layout: &HeatmapLayout,
    grid: &[[u8; DAYS_PER_WEEK as usize]],
    palette: &Palette,
) {
    if layout.cell == 0 {
        return;
    }
    for (col, week) in grid.iter().take(layout.weeks as usize).enumerate() {
        let cell_x = layout.x + col as u32 * (layout.cell + layout.gap);
        for (row, &level) in week.iter().enumerate() {
            let cell_y = layout.y + row as u32 * (layout.cell + layout.gap);
            fill_rect(
                canvas,
                cell_x,
                cell_y,
                layout.cell,
                layout.cell,
                palette.step(level),
            );
        }
    }
}

/// Fill an axis-aligned rectangle, clipped to the canvas. Any part of the
/// rectangle that falls outside is dropped rather than wrapping onto the
/// next row — the failure mode that makes overflow bugs hard to see.
fn fill_rect(canvas: &mut RgbPage, x: u32, y: u32, w: u32, h: u32, color: [u8; 3]) {
    if x >= canvas.width || y >= canvas.height {
        return;
    }
    let w = w.min(canvas.width - x);
    let h = h.min(canvas.height - y);
    for row in 0..h {
        let start = (((y + row) * canvas.width + x) * 3) as usize;
        for col in 0..w {
            let idx = start + (col * 3) as usize;
            canvas.pixels[idx..idx + 3].copy_from_slice(&color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(weeks: usize, level: u8) -> Vec<[u8; 7]> {
        vec![[level; 7]; weeks]
    }

    #[test]
    fn every_built_in_ramp_survives_a_grayscale_panel() {
        // A ramp that only varied in hue would collapse to one flat grey on a
        // panel with no colour filter. Each must darken all the way down.
        for (name, p) in [
            ("ink-rust", Palette::INK_RUST),
            ("indigo", Palette::INDIGO),
            ("sumi", Palette::SUMI),
            ("botanical", Palette::BOTANICAL),
            ("mono", Palette::MONO),
        ] {
            assert!(p.is_monotonic(), "{name} ramp is not monotonic in luma");
        }
    }

    #[test]
    fn unknown_profile_names_fall_back_to_the_default() {
        assert_eq!(Palette::from_setting("indigo"), Palette::INDIGO);
        assert_eq!(Palette::from_setting("  MONO "), Palette::MONO);
        assert_eq!(Palette::from_setting("nope"), Palette::INK_RUST);
        assert_eq!(Palette::from_setting(""), Palette::INK_RUST);
    }

    #[test]
    fn cells_land_where_the_layout_says() {
        let layout = HeatmapLayout {
            x: 10,
            y: 20,
            weeks: 3,
            cell: 4,
            gap: 2,
        };
        let mut canvas = RgbPage::new_white(64, 64);
        draw_heatmap(&mut canvas, &layout, &grid(3, 4), &Palette::MONO);

        // First cell's top-left corner, and the last cell's bottom-right.
        assert_eq!(canvas.pixel(10, 20), [0x00, 0x00, 0x00]);
        let last_x = 10 + 2 * (4 + 2) + 3;
        let last_y = 20 + 6 * (4 + 2) + 3;
        assert_eq!(canvas.pixel(last_x, last_y), [0x00, 0x00, 0x00]);
        // The gap between columns stays background.
        assert_eq!(canvas.pixel(14, 20), [0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn levels_map_to_their_ramp_step() {
        let layout = HeatmapLayout {
            x: 0,
            y: 0,
            weeks: 1,
            cell: 1,
            gap: 0,
        };
        let mut canvas = RgbPage::new_white(4, 8);
        draw_heatmap(
            &mut canvas,
            &layout,
            &[[0, 1, 2, 3, 4, 4, 4]],
            &Palette::INK_RUST,
        );
        for (row, expected) in Palette::INK_RUST.steps.iter().enumerate() {
            assert_eq!(canvas.pixel(0, row as u32), *expected, "row {row}");
        }
    }

    #[test]
    fn a_level_above_the_ramp_clamps_instead_of_panicking() {
        // Intensity comes from derived stats; an out-of-range value must
        // saturate, never index past the ramp and take the reader down.
        let layout = HeatmapLayout {
            x: 0,
            y: 0,
            weeks: 1,
            cell: 1,
            gap: 0,
        };
        let mut canvas = RgbPage::new_white(2, 8);
        draw_heatmap(&mut canvas, &layout, &[[9; 7]], &Palette::MONO);
        assert_eq!(canvas.pixel(0, 0), [0x00, 0x00, 0x00]);
    }

    #[test]
    fn nothing_is_drawn_outside_the_widgets_own_box() {
        // The guarantee that lets a caller place this next to other widgets.
        let layout = HeatmapLayout {
            x: 5,
            y: 6,
            weeks: 4,
            cell: 3,
            gap: 1,
        };
        let mut canvas = RgbPage::new_white(80, 80);
        draw_heatmap(&mut canvas, &layout, &grid(4, 4), &Palette::MONO);

        let (x0, y0) = (layout.x, layout.y);
        let (x1, y1) = (x0 + layout.width(), y0 + layout.height());
        for y in 0..canvas.height {
            for x in 0..canvas.width {
                let inside = x >= x0 && x < x1 && y >= y0 && y < y1;
                if !inside {
                    assert_eq!(
                        canvas.pixel(x, y),
                        [0xFF, 0xFF, 0xFF],
                        "painted outside the box at ({x}, {y})"
                    );
                }
            }
        }
    }

    #[test]
    fn geometry_that_overruns_the_canvas_is_clipped_not_wrapped() {
        // Wrapping onto the next row is the failure mode that hides overflow.
        let layout = HeatmapLayout {
            x: 30,
            y: 30,
            weeks: 10,
            cell: 8,
            gap: 2,
        };
        let mut canvas = RgbPage::new_white(40, 40);
        draw_heatmap(&mut canvas, &layout, &grid(10, 4), &Palette::MONO);
        // Left of the widget on every row must be untouched — if a row had
        // wrapped, these would be black.
        for y in 0..canvas.height {
            for x in 0..layout.x {
                assert_eq!(
                    canvas.pixel(x, y),
                    [0xFF, 0xFF, 0xFF],
                    "wrapped at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn more_history_than_the_layout_holds_draws_only_what_fits() {
        let layout = HeatmapLayout {
            x: 0,
            y: 0,
            weeks: 2,
            cell: 2,
            gap: 0,
        };
        let mut canvas = RgbPage::new_white(20, 20);
        draw_heatmap(&mut canvas, &layout, &grid(50, 4), &Palette::MONO);
        assert_eq!(
            canvas.pixel(3, 0),
            [0x00, 0x00, 0x00],
            "second column drawn"
        );
        assert_eq!(
            canvas.pixel(4, 0),
            [0xFF, 0xFF, 0xFF],
            "third column not drawn"
        );
    }

    #[test]
    fn a_short_history_draws_fewer_columns_without_padding() {
        let layout = HeatmapLayout {
            x: 0,
            y: 0,
            weeks: 5,
            cell: 2,
            gap: 0,
        };
        let mut canvas = RgbPage::new_white(20, 20);
        draw_heatmap(&mut canvas, &layout, &grid(2, 4), &Palette::MONO);
        assert_eq!(canvas.pixel(3, 0), [0x00, 0x00, 0x00]);
        assert_eq!(canvas.pixel(4, 0), [0xFF, 0xFF, 0xFF], "no phantom column");
    }

    #[test]
    fn fit_sizes_cells_to_the_width_it_is_given() {
        let l = HeatmapLayout::fit(0, 0, 18, 1226, 8);
        assert!(l.width() <= 1226, "overran the width budget");
        // A cell never collapses to zero, even in an absurdly tight box.
        let tight = HeatmapLayout::fit(0, 0, 100, 10, 4);
        assert!(tight.cell >= 1);
    }

    #[test]
    fn a_zero_cell_layout_draws_nothing_rather_than_panicking() {
        let layout = HeatmapLayout {
            x: 0,
            y: 0,
            weeks: 3,
            cell: 0,
            gap: 1,
        };
        let mut canvas = RgbPage::new_white(8, 8);
        draw_heatmap(&mut canvas, &layout, &grid(3, 4), &Palette::MONO);
        assert_eq!(canvas.pixel(0, 0), [0xFF, 0xFF, 0xFF]);
    }
}
