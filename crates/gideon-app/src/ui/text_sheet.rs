//! The book reader's Text sheet and its font list, as approved in design
//! review (gallery design `reader-highlights-fonts-words`):
//!
//! * **Font** — the current face, written in itself; tap for the list of
//!   twelve, each name set in its own face.
//! * **Size** — a slider, like a Kindle's: tap the track or slide the knob;
//!   it snaps to [`BOOK_TEXT_PX_STEPS`].
//! * **Line spacing** and **Margins** — three-way switches.
//!
//! Refresh, per docs/REFRESH.md: the knob follows the finger with Partial
//! repaints of the sheet alone (the page behind has not changed yet); a
//! committed change re-flows the page, which moves every line, so that is
//! Full; swapping the Text sheet for the font list (or back) is Partial;
//! closing a sheet is Full.

use super::book_search::{fill_gray, hit, outline, Painted, Rect};
use super::*;

/// Where the Text sheet's controls sit. One value for drawing and hit-tests.
pub(super) struct TextSheetLayout {
    pub top: u32,
    pub height: u32,
    pub close: Rect,
    pub font_row: Rect,
    /// The slider's track (the knob travels its full width).
    pub track: Rect,
    /// The band a tap or drag on the slider may land in: the track, the two
    /// "A"s and a finger's height around them.
    pub slider_band: Rect,
    pub spacing: [Rect; 3],
    pub margins: [Rect; 3],
}

/// Where the font list's tiles sit.
pub(super) struct FontSheetLayout {
    pub top: u32,
    pub height: u32,
    pub close: Rect,
    pub tiles: Vec<Rect>,
}

/// The size step a point on the track means, snapped to the nearest.
pub(super) fn step_at(track: Rect, x: u32) -> usize {
    let n = BOOK_TEXT_PX_STEPS.len();
    let rel = x.saturating_sub(track.0).min(track.2) as f32 / track.2.max(1) as f32;
    ((rel * (n - 1) as f32).round() as usize).min(n - 1)
}

/// The step nearest a stored size (a hand-edited settings.json may hold a
/// size between steps).
pub(super) fn step_of(px: u32) -> usize {
    BOOK_TEXT_PX_STEPS
        .iter()
        .enumerate()
        .min_by_key(|(_, s)| s.abs_diff(px))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// The x a step's knob sits at.
fn step_x(track: Rect, step: usize) -> u32 {
    let n = BOOK_TEXT_PX_STEPS.len() as u32;
    track.0 + track.2 * step as u32 / (n - 1).max(1)
}

impl<D: Display, I: InputSource, G: SourceGateway> UiApp<D, I, G> {
    /// The panel's title bar: the title, and a finger-sized × at the right.
    fn sheet_head(&self) -> (u32, Rect) {
        let l = &self.layout;
        let title_h = (l.text_px * 1.6) as u32;
        let side = title_h.min(l.width);
        (title_h, (l.width - side, 0, side, title_h))
    }

    pub(super) fn text_sheet_layout(&self) -> TextSheetLayout {
        let l = &self.layout;
        let pad = l.pad * 2;
        let (title_h, close) = self.sheet_head();
        let label_h = (l.text_px * 0.75 * 1.6) as u32;
        let row = l.row_h;
        let height =
            (title_h + row + label_h + row + label_h + row + label_h + row + pad).min(l.height);
        let top = l.height - height;
        let w = l.width.saturating_sub(pad * 2);
        let mut y = top + title_h;
        let font_row = (pad, y, w, row);
        y += row + label_h;
        let a_small = (l.text_px * 0.7) as u32;
        let a_big = (l.text_px * 1.3) as u32;
        let gap = l.pad * 2;
        let track = (
            pad + a_small + gap,
            y,
            w.saturating_sub(a_small + a_big + gap * 2),
            row,
        );
        let slider_band = (0, y.saturating_sub(label_h), l.width, row + label_h);
        y += row + label_h;
        let third = w / 3;
        let seg = |y: u32| -> [Rect; 3] {
            [
                (pad, y, third, row),
                (pad + third, y, third, row),
                (pad + third * 2, y, w - third * 2, row),
            ]
        };
        let spacing = seg(y);
        y += row + label_h;
        let margins = seg(y);
        TextSheetLayout {
            top,
            height,
            close: (close.0, top, close.2, close.3),
            font_row,
            track,
            slider_band,
            spacing,
            margins,
        }
    }

    pub(super) fn font_sheet_layout(&self) -> FontSheetLayout {
        let l = &self.layout;
        let pad = l.pad * 2;
        let (title_h, close) = self.sheet_head();
        let gap = l.pad;
        let cols = 2u32;
        let rows = FontFamily::ALL.len().div_ceil(cols as usize) as u32;
        let tile_h = l.row_h;
        let height = (title_h + rows * (tile_h + gap) + pad).min(l.height);
        let top = l.height - height;
        let w = l.width.saturating_sub(pad * 2);
        let tile_w = (w - gap * (cols - 1)) / cols;
        let tiles = (0..FontFamily::ALL.len() as u32)
            .map(|i| {
                let (c, r) = (i % cols, i / cols);
                (
                    pad + c * (tile_w + gap),
                    top + title_h + r * (tile_h + gap),
                    tile_w,
                    tile_h,
                )
            })
            .collect();
        FontSheetLayout {
            top,
            height,
            close: (close.0, top, close.2, close.3),
            tiles,
        }
    }

    /// The panel every one of these sheets sits in: opaque white, the heavy
    /// top edge, an accent mark beside the title, and the ×.
    fn draw_sheet_panel(
        &self,
        canvas: &mut RgbPage,
        p: &mut Painted,
        top: u32,
        height: u32,
        title: &str,
    ) {
        let l = &self.layout;
        let t = widgets::Theme::from_setting(&self.load_settings().color_profile);
        fill_rect_rgb(canvas, 0, top, l.width, height, [0xFF; 3]);
        fill_rect_rgb(canvas, 0, top, l.width, 3, [0x00; 3]);
        let (title_h, close) = self.sheet_head();
        let px = l.text_px * 0.95;
        let mark_h = (px as u32).max(6);
        p.fills.push((
            (
                l.pad,
                top + (title_h - mark_h) / 2,
                (l.pad / 2).max(4),
                mark_h,
            ),
            t.accent,
        ));
        draw_text(
            &mut p.ink,
            l.pad * 2 + l.pad / 2,
            top + title_h.saturating_sub(px as u32 + 4) / 2,
            px,
            title,
            l.width / 2,
            true,
        );
        let cpx = l.text_px * 1.15;
        let cw = measure_text(cpx, "\u{00d7}", true);
        draw_text(
            &mut p.ink,
            close.0 + close.2.saturating_sub(cw) / 2,
            top + title_h.saturating_sub(cpx as u32) / 2,
            cpx,
            "\u{00d7}",
            cw + 1,
            true,
        );
    }

    /// A small caps label over a control.
    fn draw_label(&self, p: &mut Painted, y: u32, text: &str) {
        let l = &self.layout;
        let px = l.text_px * 0.6;
        draw_text(&mut p.ink, l.pad * 2, y, px, text, l.width / 2, true);
    }

    /// A three-way switch: thin dividers, the chosen one washed and
    /// underlined in the accent.
    fn draw_segments(&self, p: &mut Painted, cells: &[Rect; 3], labels: [&str; 3], on: usize) {
        let l = &self.layout;
        let t = widgets::Theme::from_setting(&self.load_settings().color_profile);
        let all = (
            cells[0].0,
            cells[0].1,
            cells[2].0 + cells[2].2 - cells[0].0,
            cells[0].3,
        );
        let inset = l.row_h / 8;
        let box_ = (all.0, all.1 + inset, all.2, all.3.saturating_sub(inset * 2));
        let chosen = cells[on];
        p.fills
            .push(((chosen.0, box_.1, chosen.2, box_.3), t.accent_wash()));
        let bar = (l.pad / 2).max(4);
        p.fills
            .push(((chosen.0, box_.1 + box_.3 - bar, chosen.2, bar), t.accent));
        outline(&mut p.ink, box_, 2, 0x00);
        for c in &cells[1..] {
            fill_gray(&mut p.ink, (c.0, box_.1, 1, box_.3), 0x00);
        }
        let px = l.text_px * 0.8;
        for (i, (c, label)) in cells.iter().zip(labels).enumerate() {
            let bold = i == on;
            let w = measure_text(px, label, bold);
            draw_text(
                &mut p.ink,
                c.0 + c.2.saturating_sub(w) / 2,
                box_.1 + box_.3.saturating_sub(px as u32 + 4) / 2,
                px,
                label,
                c.2,
                bold,
            );
        }
    }

    /// Draw the Text sheet. `preview` is the size step under a finger still
    /// on the slider, not yet saved.
    pub(super) fn overlay_text_sheet(&self, canvas: &mut RgbPage, preview: Option<usize>) {
        let l = &self.layout;
        let t = widgets::Theme::from_setting(&self.load_settings().color_profile);
        let settings = self.load_settings();
        let s = self.text_sheet_layout();
        let mut p = Painted {
            ink: GrayPage::new_white(l.width, l.height),
            fills: Vec::new(),
        };
        self.draw_sheet_panel(canvas, &mut p, s.top, s.height, "Text");
        let label_h = (l.text_px * 0.75 * 1.6) as u32;

        // Font: the name on the right, set in its own face.
        let fr = s.font_row;
        let px = l.text_px * 0.9;
        draw_text(
            &mut p.ink,
            fr.0,
            fr.1 + fr.3.saturating_sub(px as u32 + 4) / 2,
            px,
            "Font",
            fr.2 / 2,
            true,
        );
        let family = FontFamily::from_name(&settings.book_font);
        let value = format!("{} \u{203a}", family.label());
        let style = gideon_render::text::TextStyle {
            family,
            ..Default::default()
        };
        let vw = gideon_render::text::measure_text_styled(px, &value, style);
        gideon_render::text::draw_text_styled(
            &mut p.ink,
            (fr.0 + fr.2).saturating_sub(vw),
            fr.1 + fr.3.saturating_sub(px as u32 + 4) / 2,
            px,
            &value,
            vw + 1,
            style,
        );
        fill_gray(&mut p.ink, (fr.0, fr.1 + fr.3 - 1, fr.2, 1), 0xCC);

        // Size: label and value, the small and big A, the track, the knob.
        let step = preview.unwrap_or_else(|| step_of(settings.book_text_px));
        let tr = s.track;
        self.draw_label(&mut p, tr.1.saturating_sub(label_h), "SIZE");
        let value = format!("{} px", BOOK_TEXT_PX_STEPS[step]);
        let vpx = l.text_px * 0.7;
        let vw = measure_text(vpx, &value, false);
        draw_text(
            &mut p.ink,
            (l.width - vw) / 2,
            tr.1.saturating_sub(label_h),
            vpx,
            &value,
            vw + 1,
            false,
        );
        let mid = tr.1 + tr.3 / 2;
        let a_small = l.text_px * 0.7;
        let a_big = l.text_px * 1.3;
        draw_text(
            &mut p.ink,
            l.pad * 2,
            mid.saturating_sub(a_small as u32 / 2 + 2),
            a_small,
            "A",
            a_small as u32 * 2,
            true,
        );
        let bw = measure_text(a_big, "A", true);
        draw_text(
            &mut p.ink,
            l.width.saturating_sub(l.pad * 2 + bw),
            mid.saturating_sub(a_big as u32 / 2 + 2),
            a_big,
            "A",
            bw + 1,
            true,
        );
        let line = (l.pad / 3).max(3);
        fill_gray(&mut p.ink, (tr.0, mid - line / 2, tr.2, line), 0x00);
        let kx = step_x(tr, step);
        p.fills
            .push(((tr.0, mid - line, kx - tr.0, line * 2), t.accent));
        let tick_h = tr.3 / 4;
        for i in 0..BOOK_TEXT_PX_STEPS.len() {
            let x = step_x(tr, i);
            fill_gray(
                &mut p.ink,
                (x.saturating_sub(1), mid - tick_h / 2, 2, tick_h),
                0x00,
            );
        }
        // The knob, drawn in the ink layer last, over the ticks: a round
        // knob, white inside a heavy ring.
        let r = (tr.3 / 4).max(5) as i64;
        let ring = (r / 3).max(3);
        for dy in -r..=r {
            for dx in -r..=r {
                let d2 = dx * dx + dy * dy;
                if d2 > r * r {
                    continue;
                }
                let inner = (r - ring) * (r - ring);
                let v = if d2 >= inner { 0x00 } else { 0xFE };
                plot(&mut p.ink, kx as i32 + dx as i32, mid as i32 + dy as i32, v);
            }
        }

        // Spacing and margins.
        let spacing_names: Vec<&str> = BOOK_LINE_SPACINGS.iter().map(|(n, _)| *n).collect();
        let margin_names: Vec<&str> = BOOK_MARGINS.iter().map(|(n, _)| *n).collect();
        let on_spacing = spacing_names
            .iter()
            .position(|n| *n == settings.book_line_spacing)
            .unwrap_or(1);
        let on_margins = margin_names
            .iter()
            .position(|n| *n == settings.book_margins)
            .unwrap_or(1);
        self.draw_label(
            &mut p,
            s.spacing[0].1.saturating_sub(label_h) + label_h / 4,
            "LINE SPACING",
        );
        self.draw_segments(&mut p, &s.spacing, ["Tight", "Normal", "Loose"], on_spacing);
        self.draw_label(
            &mut p,
            s.margins[0].1.saturating_sub(label_h) + label_h / 4,
            "MARGINS",
        );
        self.draw_segments(&mut p, &s.margins, ["Narrow", "Normal", "Wide"], on_margins);

        for ((x, y, w, h), c) in &p.fills {
            fill_rect_rgb(canvas, *x, *y, *w, *h, *c);
        }
        widgets::overlay_ink(canvas, &p.ink);
    }

    /// Draw the font list: twelve tiles, each name in its own face.
    pub(super) fn overlay_font_sheet(&self, canvas: &mut RgbPage) {
        let l = &self.layout;
        let t = widgets::Theme::from_setting(&self.load_settings().color_profile);
        let current = FontFamily::from_name(&self.load_settings().book_font);
        let s = self.font_sheet_layout();
        let mut p = Painted {
            ink: GrayPage::new_white(l.width, l.height),
            fills: Vec::new(),
        };
        self.draw_sheet_panel(canvas, &mut p, s.top, s.height, "Font");
        let px = l.text_px * 0.95;
        for (family, r) in FontFamily::ALL.iter().zip(&s.tiles) {
            let on = *family == current;
            if on {
                p.fills.push((*r, t.accent_wash()));
                let b = 3;
                p.fills.push(((r.0, r.1, r.2, b), t.accent));
                p.fills.push(((r.0, r.1 + r.3 - b, r.2, b), t.accent));
                p.fills.push(((r.0, r.1, b, r.3), t.accent));
                p.fills.push(((r.0 + r.2 - b, r.1, b, r.3), t.accent));
            } else {
                outline(&mut p.ink, *r, 2, 0x00);
            }
            let style = gideon_render::text::TextStyle {
                family: *family,
                ..Default::default()
            };
            let w = gideon_render::text::measure_text_styled(px, family.label(), style).min(r.2);
            gideon_render::text::draw_text_styled(
                &mut p.ink,
                r.0 + r.2.saturating_sub(w) / 2,
                r.1 + r.3.saturating_sub(px as u32 + 4) / 2,
                px,
                family.label(),
                r.2,
                style,
            );
        }
        for ((x, y, w, h), c) in &p.fills {
            fill_rect_rgb(canvas, *x, *y, *w, *h, *c);
        }
        widgets::overlay_ink(canvas, &p.ink);
    }

    /// Save a change to the reader's typography and re-flow the page under
    /// the open sheet. Every line moves, so the repaint is Full.
    fn apply_text_setting(
        &mut self,
        change: impl FnOnce(&mut gideon_core::Settings),
    ) -> Result<()> {
        let mut settings = self.load_settings();
        change(&mut settings);
        self.save_settings(&settings);
        self.reflow_book()
    }

    fn close_text_sheets(&mut self) -> Result<Flow> {
        self.sheet = None;
        // The sheet held the page under it stale: reveal it Full.
        self.render_current(RefreshMode::Full)?;
        Ok(Flow::Continue)
    }

    /// Route a tap while the Text sheet is up.
    pub(super) fn tap_text_sheet(&mut self, x: u32, y: u32) -> Result<Flow> {
        let s = self.text_sheet_layout();
        if y < s.top || hit(s.close, x, y) {
            return self.close_text_sheets();
        }
        if hit(s.font_row, x, y) {
            self.sheet = Some(Sheet::BookFonts);
            // One sheet for another over the same unmoved page.
            self.render_current(RefreshMode::Partial)?;
            return Ok(Flow::Continue);
        }
        if hit(s.slider_band, x, y) {
            let step = step_at(s.track, x);
            self.sheet = Some(Sheet::BookText { preview: None });
            self.apply_text_setting(|st| st.book_text_px = BOOK_TEXT_PX_STEPS[step])?;
            return Ok(Flow::Continue);
        }
        let names = |list: &[(&'static str, _)]| -> Vec<&'static str> {
            list.iter().map(|(n, _)| *n).collect()
        };
        if let Some(i) = s.spacing.iter().position(|r| hit(*r, x, y)) {
            let name = names(&BOOK_LINE_SPACINGS)[i].to_string();
            self.apply_text_setting(|st| st.book_line_spacing = name)?;
        } else if let Some(i) = s.margins.iter().position(|r| hit(*r, x, y)) {
            let name = BOOK_MARGINS[i].0.to_string();
            self.apply_text_setting(|st| st.book_margins = name)?;
        }
        Ok(Flow::Continue)
    }

    /// A finger on the size slider: the knob follows it with Partial
    /// repaints of the sheet; the size is saved, and the page re-flows,
    /// where it lifts.
    pub(super) fn drag_text_slider(
        &mut self,
        x0: u32,
        y0: u32,
        x1: u32,
        released: bool,
    ) -> Result<bool> {
        if !matches!(self.sheet, Some(Sheet::BookText { .. })) {
            return Ok(false);
        }
        let s = self.text_sheet_layout();
        if !hit(s.slider_band, x0, y0) {
            return Ok(false);
        }
        let step = step_at(s.track, x1);
        if released {
            self.sheet = Some(Sheet::BookText { preview: None });
            self.apply_text_setting(|st| st.book_text_px = BOOK_TEXT_PX_STEPS[step])?;
        } else if self.sheet
            != Some(Sheet::BookText {
                preview: Some(step),
            })
        {
            self.sheet = Some(Sheet::BookText {
                preview: Some(step),
            });
            self.render_current(RefreshMode::Partial)?;
        }
        Ok(true)
    }

    /// Route a tap while the font list is up: a font sets the page in it and
    /// returns to the Text sheet.
    pub(super) fn tap_font_sheet(&mut self, x: u32, y: u32) -> Result<Flow> {
        let s = self.font_sheet_layout();
        if y < s.top || hit(s.close, x, y) {
            return self.close_text_sheets();
        }
        if let Some(i) = s.tiles.iter().position(|r| hit(*r, x, y)) {
            let name = FontFamily::ALL[i].name().to_string();
            self.sheet = Some(Sheet::BookText { preview: None });
            self.apply_text_setting(|st| st.book_font = name)?;
        }
        Ok(Flow::Continue)
    }
}
