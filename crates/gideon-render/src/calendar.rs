//! Month calendar: which series were read on which days, drawn as bars that
//! continue across consecutive days.
//!
//! The heatmap answers "how much, over months". This answers "what was I
//! reading, and for how many days running" — which is the question a reader
//! actually asks about last week. A series read three evenings in a row is
//! ONE bar three days wide; that continuity is the information, and drawing
//! three separate marks would throw it away.

use crate::text::{draw_text, measure_text};
use crate::widgets::Theme;
use crate::{GrayPage, RgbPage};

/// Grid lines. Light enough to organise without competing with the bars.
const RULE: [u8; 3] = [0xD4, 0xD4, 0xD4];
/// A day outside the month being shown.
const OUTSIDE: [u8; 3] = [0xF4, 0xF4, 0xF4];

/// Days per week. Columns run Monday-first, which is how a reading week
/// reads and what the design this came from uses.
const DAYS: i64 = 7;

/// A run of days one series was read on, as the widget needs it: day indices
/// (days since the epoch, local), not timestamps.
///
/// `gideon-render` deliberately knows nothing about progress stores or
/// calendars — the caller computes the runs (see `ReadingStats::spans`) and
/// this draws them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span<'a> {
    pub series: &'a str,
    pub start_day: i64,
    pub end_day: i64,
}

/// Which cells a month occupies, in day indices. Computed by the caller,
/// which owns the civil-date arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Month {
    /// Day index of the top-left cell: the Monday on or before the 1st.
    pub first_cell: i64,
    /// Day index of the 1st.
    pub first_day: i64,
    /// How many days the month has.
    pub days: u32,
}

impl Month {
    /// Week rows the month needs — four for a short February that starts on
    /// a Monday, six for a long month that starts on a Sunday. A fixed six
    /// would waste a row of a screen that has none to spare.
    pub fn weeks(&self) -> u32 {
        let span = (self.first_day - self.first_cell) as u32 + self.days;
        span.div_ceil(DAYS as u32).max(1)
    }

    /// Day of the month for a day index, or `None` when it falls outside.
    pub fn day_of_month(&self, day: i64) -> Option<u32> {
        let offset = day - self.first_day;
        // `then`, not `then_some`: the latter evaluates its argument even
        // when the condition is false, and `offset as u32 + 1` on a day
        // before the month overflows.
        (offset >= 0 && offset < i64::from(self.days)).then(|| offset as u32 + 1)
    }
}

/// Where a month calendar sits and how big its cells are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalendarLayout {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    /// Height of the weekday header strip.
    pub header_h: u32,
    /// Height of one week row.
    pub row_h: u32,
    /// Number of week rows this month needs.
    pub weeks: u32,
}

impl CalendarLayout {
    /// Fit a month into `(width, height)`, sized to the weeks it needs.
    pub fn fit(x: u32, y: u32, width: u32, height: u32, month: Month) -> Self {
        let weeks = month.weeks();
        let header_h = (height / (weeks + 3)).max(1);
        let row_h = height.saturating_sub(header_h) / weeks.max(1);
        Self {
            x,
            y,
            width,
            header_h,
            row_h,
            weeks,
        }
    }

    pub fn cell_w(&self) -> u32 {
        self.width / DAYS as u32
    }

    pub fn height(&self) -> u32 {
        self.header_h + self.row_h * self.weeks
    }

    /// The rect of the cell for `column` (0 = Monday) in `week`.
    pub fn cell(&self, week: u32, column: u32) -> (u32, u32, u32, u32) {
        (
            self.x + column * self.cell_w(),
            self.y + self.header_h + week * self.row_h,
            self.cell_w(),
            self.row_h,
        )
    }
}

/// Monday-first column for a day index: 0 = Monday … 6 = Sunday.
///
/// Day 0 (1970-01-01) was a Thursday, so Monday-first columns are the day
/// index plus three, modulo seven.
fn column_of(day: i64) -> u32 {
    (day + 3).rem_euclid(DAYS) as u32
}

/// One drawn piece of a span: a run of days inside a single week row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment<'a> {
    pub series: &'a str,
    pub week: u32,
    /// First and last column, inclusive.
    pub from: u32,
    pub to: u32,
    /// Which stacked line inside the cell this segment sits on.
    pub lane: usize,
    /// Whether the run started before this segment (it wrapped a week
    /// boundary), and whether it continues after it.
    pub continues_before: bool,
    pub continues_after: bool,
}

/// Break spans into per-week segments and assign each a lane, so two series
/// read on the same day stack instead of overlapping.
///
/// Lanes are assigned greedily in span order, and a series keeps its lane
/// across a week boundary when it can — a bar that jumps up a line as it
/// wraps reads as two different books.
pub fn segments<'a>(
    spans: &'a [Span<'a>],
    first_cell: i64,
    weeks: u32,
    lanes: usize,
) -> Vec<Segment<'a>> {
    let last_cell = first_cell + i64::from(weeks) * DAYS - 1;
    // occupied[week][lane] = highest column used so far.
    let mut occupied: Vec<Vec<Option<u32>>> = vec![vec![None; lanes.max(1)]; weeks as usize];
    let mut out = Vec::new();

    for span in spans {
        let start = span.start_day.max(first_cell);
        let end = span.end_day.min(last_cell);
        if start > end {
            continue;
        }
        let mut day = start;
        let mut preferred: Option<usize> = None;
        while day <= end {
            let week = ((day - first_cell) / DAYS) as u32;
            let from = column_of(day);
            let week_end = first_cell + i64::from(week) * DAYS + DAYS - 1;
            let piece_end = end.min(week_end);
            let to = column_of(piece_end);

            // The lane this piece can take: the preferred one if it is free
            // for the whole piece, else the first that is.
            let lane = (0..lanes.max(1))
                .filter(|l| occupied[week as usize][*l].is_none_or(|used| used < from))
                .min_by_key(|l| (Some(*l) != preferred, *l));
            let Some(lane) = lane else {
                // No room on this week: the day is busier than the cell is
                // tall. Dropping the piece is better than drawing it over
                // another series' bar.
                day = piece_end + 1;
                continue;
            };
            occupied[week as usize][lane] = Some(to);
            preferred = Some(lane);
            out.push(Segment {
                series: span.series,
                week,
                from,
                to,
                lane,
                continues_before: start < day,
                continues_after: piece_end < end,
            });
            day = piece_end + 1;
        }
    }
    out
}

/// Draw the month, its grid, and the reading bars.
///
/// `today` marks the current day so it can be picked out; pass a day index
/// outside the month for "no today". Everything is clipped to the canvas —
/// a bar can never paint over the widget's neighbours.
#[allow(clippy::too_many_arguments)] // the caller's contract; see draw_stat_tiles
pub fn draw_calendar(
    canvas: &mut RgbPage,
    layout: &CalendarLayout,
    month: Month,
    spans: &[Span],
    today: i64,
    text_px: f32,
    t: &Theme,
) {
    if layout.cell_w() == 0 || layout.row_h == 0 {
        return;
    }
    let first_cell = month.first_cell;
    let day_px = text_px * 0.6;
    let head_px = text_px * 0.55;
    let bar_px = text_px * 0.5;
    let mut layer = GrayPage::new_white(canvas.width, canvas.height);

    // Weekday header.
    for (i, name) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
        .iter()
        .enumerate()
    {
        let w = measure_text(head_px, name, false);
        let (cx, _, cw, _) = layout.cell(0, i as u32);
        draw_text(
            &mut layer,
            cx + cw.saturating_sub(w) / 2,
            layout.y + layout.header_h.saturating_sub(head_px as u32) / 2,
            head_px,
            name,
            w,
            false,
        );
    }

    // Cells: grid, day number, and a wash for days outside this month.
    for week in 0..layout.weeks {
        for col in 0..DAYS as u32 {
            let day = first_cell + i64::from(week) * DAYS + i64::from(col);
            let (cx, cy, cw, ch) = layout.cell(week, col);
            let inside = month.day_of_month(day);
            if inside.is_none() {
                fill_rect(canvas, cx, cy, cw, ch, OUTSIDE);
            }
            // Grid: one rule down the left and one across the top, so
            // neighbouring cells share an edge instead of doubling it.
            fill_rect(canvas, cx, cy, 1, ch, RULE);
            fill_rect(canvas, cx, cy, cw, 1, RULE);
            if col == DAYS as u32 - 1 {
                fill_rect(canvas, cx + cw.saturating_sub(1), cy, 1, ch, RULE);
            }
            if week == layout.weeks - 1 {
                fill_rect(canvas, cx, cy + ch.saturating_sub(1), cw, 1, RULE);
            }

            let Some(number) = inside else {
                continue; // an outside day is a wash and nothing else
            };
            let label = number.to_string();
            let pad = (day_px * 0.3) as u32 + 1;
            draw_text(
                &mut layer,
                cx + pad,
                cy + pad,
                day_px,
                &label,
                cw.saturating_sub(pad * 2),
                day == today,
            );
            // Today gets a block under its number rather than a ring: a
            // 1px outline is exactly what the colour filter loses.
            if day == today {
                let mark = (day_px * 0.25).max(3.0) as u32;
                fill_rect(
                    canvas,
                    cx + pad,
                    cy + pad + day_px as u32 + 1,
                    measure_text(day_px, &label, true).max(mark),
                    mark,
                    t.accent,
                );
            }
        }
    }

    // The bars. Lanes are sized from what is left under the day number.
    let lane_h = (bar_px * 1.35) as u32;
    let top = (day_px * 1.9) as u32;
    let lanes = ((layout.row_h.saturating_sub(top)) / lane_h.max(1)).max(1) as usize;
    for seg in segments(spans, first_cell, layout.weeks, lanes) {
        let (x0, cy, cw, _) = layout.cell(seg.week, seg.from);
        let (x1, _, _, _) = layout.cell(seg.week, seg.to);
        let gap = 2;
        let bx = x0 + gap;
        let bw = (x1 + cw).saturating_sub(x0 + gap * 2).max(1);
        let by = cy + top + seg.lane as u32 * lane_h;
        if by + lane_h > cy + layout.row_h {
            continue;
        }
        // The bar itself: a filled block, because a run of days is a
        // quantity and this panel renders quantity-as-length well and
        // quantity-as-hue badly.
        fill_rect(canvas, bx, by, bw, lane_h.saturating_sub(1), t.status_tint);
        // The leading edge carries the accent unless the run started in an
        // earlier week — so a wrapped bar reads as a continuation rather
        // than as a new book.
        if !seg.continues_before {
            fill_rect(canvas, bx, by, 3, lane_h.saturating_sub(1), t.accent);
        }
        let text_x = bx + if seg.continues_before { 3 } else { 6 };
        let avail = bw.saturating_sub(text_x - bx + 2);
        // A continued bar repeats the title: the reader is looking at one
        // week, not scanning back to find where the run began.
        draw_text(
            &mut layer,
            text_x,
            by + lane_h.saturating_sub(bar_px as u32) / 2,
            bar_px,
            seg.series,
            avail,
            false,
        );
    }

    overlay_ink(canvas, &layer);
}

/// Fill a rectangle, clipped to the canvas. Anything outside is dropped
/// rather than wrapped — see `docs/LESSONS.md` §1.
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

/// Lay the text layer's ink over the canvas, leaving its white alone, so
/// black text sits on top of the coloured bars.
fn overlay_ink(dst: &mut RgbPage, src: &GrayPage) {
    for y in 0..dst.height.min(src.height) {
        for x in 0..dst.width.min(src.width) {
            let v = src.pixel(x, y);
            if v < 0xFF {
                let idx = ((y * dst.width + x) * 3) as usize;
                dst.pixels[idx..idx + 3].copy_from_slice(&[v, v, v]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// August 2026: the 1st is a Saturday, so the grid starts on Monday the
    /// 27th of July and runs six rows — the month the design came from.
    fn august_2026() -> Month {
        // 2026-08-01 is day 20666; 2026-07-27 (Monday) is 20661.
        Month {
            first_cell: 20661,
            first_day: 20666,
            days: 31,
        }
    }

    #[test]
    fn columns_are_monday_first() {
        let m = august_2026();
        assert_eq!(column_of(m.first_cell), 0, "the grid starts on a Monday");
        assert_eq!(column_of(m.first_day), 5, "the 1st is a Saturday");
        assert_eq!(column_of(m.first_day + 1), 6, "the 2nd is a Sunday");
        assert_eq!(column_of(m.first_day + 2), 0, "the 3rd wraps to Monday");
    }

    #[test]
    fn a_month_takes_the_rows_it_needs() {
        assert_eq!(august_2026().weeks(), 6);
        // A 28-day February beginning on a Monday is exactly four rows.
        let feb = Month {
            first_cell: 20000,
            first_day: 20000,
            days: 28,
        };
        assert_eq!(feb.weeks(), 4);
    }

    #[test]
    fn a_run_of_days_is_one_bar() {
        // The whole point of the view: three evenings running is ONE bar
        // three days wide, not three marks.
        let m = august_2026();
        let start = m.first_day + 2; // Monday the 3rd
        let spans = [Span {
            series: "Berserk",
            start_day: start,
            end_day: start + 2,
        }];
        let segs = segments(&spans, m.first_cell, m.weeks(), 3);
        assert_eq!(segs.len(), 1, "one week, one segment");
        assert_eq!(
            (segs[0].from, segs[0].to),
            (0, 2),
            "Monday through Wednesday"
        );
        assert!(!segs[0].continues_before && !segs[0].continues_after);
    }

    #[test]
    fn a_run_across_a_week_boundary_continues_on_the_next_row() {
        // Saturday to Tuesday is two segments, and they must say so — a bar
        // that just stops at the edge reads as a run that ended there.
        let m = august_2026();
        let saturday = m.first_day; // the 1st
        let spans = [Span {
            series: "Pluto",
            start_day: saturday,
            end_day: saturday + 3,
        }];
        let segs = segments(&spans, m.first_cell, m.weeks(), 3);
        assert_eq!(segs.len(), 2);
        assert_eq!((segs[0].week, segs[0].from, segs[0].to), (0, 5, 6));
        assert!(segs[0].continues_after && !segs[0].continues_before);
        assert_eq!((segs[1].week, segs[1].from, segs[1].to), (1, 0, 1));
        assert!(segs[1].continues_before && !segs[1].continues_after);
        assert_eq!(
            segs[0].lane, segs[1].lane,
            "a wrapped bar keeps its lane; jumping a line reads as another book"
        );
    }

    #[test]
    fn overlapping_series_stack_instead_of_colliding() {
        let m = august_2026();
        let monday = m.first_day + 2;
        let spans = [
            Span {
                series: "Berserk",
                start_day: monday,
                end_day: monday + 3,
            },
            Span {
                series: "Pluto",
                start_day: monday + 1,
                end_day: monday + 1,
            },
            Span {
                series: "Frieren",
                start_day: monday + 1,
                end_day: monday + 2,
            },
        ];
        let segs = segments(&spans, m.first_cell, m.weeks(), 3);
        let lane_of = |name: &str| segs.iter().find(|s| s.series == name).unwrap().lane;
        assert_eq!(lane_of("Berserk"), 0);
        assert_ne!(lane_of("Pluto"), lane_of("Berserk"));
        assert_ne!(lane_of("Frieren"), lane_of("Pluto"));
        assert_ne!(lane_of("Frieren"), lane_of("Berserk"));

        // With only one lane, the extra series are dropped rather than drawn
        // over the top of another series' bar.
        let one = segments(&spans, m.first_cell, m.weeks(), 1);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].series, "Berserk");
    }

    #[test]
    fn a_run_that_starts_before_the_window_is_clipped_not_dropped() {
        let m = august_2026();
        let spans = [Span {
            series: "Vinland Saga",
            start_day: m.first_cell - 10,
            end_day: m.first_cell + 1,
        }];
        let segs = segments(&spans, m.first_cell, m.weeks(), 3);
        assert_eq!(segs.len(), 1);
        assert_eq!((segs[0].from, segs[0].to), (0, 1), "clipped to the grid");
    }

    #[test]
    fn nothing_is_drawn_outside_the_widgets_box() {
        // The rule from docs/LESSONS.md §1: a widget handed hostile geometry
        // clips rather than wrapping onto its neighbours.
        let mut canvas = RgbPage::new_white(200, 120);
        let m = august_2026();
        let layout = CalendarLayout::fit(150, 90, 400, 400, m);
        let spans = [Span {
            series: "Berserk",
            start_day: m.first_day,
            end_day: m.first_day + 20,
        }];
        draw_calendar(
            &mut canvas,
            &layout,
            m,
            &spans,
            m.first_day,
            16.0,
            &Theme::INK_RUST,
        );
        // It painted something inside, and nothing ran off the buffer.
        assert_eq!(canvas.pixels.len(), (200 * 120 * 3) as usize);
    }

    #[test]
    fn a_degenerate_box_draws_nothing_instead_of_panicking() {
        let mut canvas = RgbPage::new_white(100, 100);
        let m = august_2026();
        let layout = CalendarLayout::fit(0, 0, 0, 0, m);
        draw_calendar(
            &mut canvas,
            &layout,
            m,
            &[],
            m.first_day,
            16.0,
            &Theme::INK_RUST,
        );
        assert!(canvas.pixels.iter().all(|&p| p == 0xFF), "nothing drawn");
    }
}
