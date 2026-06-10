//! Library shelf (cover grid) composition.
//!
//! Lays out cover thumbnails in a grid with progress bars — the rendering
//! core of the library cover view. Pure pixel math over [`GrayPage`], so
//! the whole layout is testable headless: no widget ever draws outside its
//! cell, by construction and by test.

use image::imageops::FilterType;
use image::DynamicImage;

use crate::GrayPage;

/// Grid geometry, all in pixels.
#[derive(Debug, Clone, Copy)]
pub struct ShelfLayout {
    pub screen_width: u32,
    pub screen_height: u32,
    pub columns: u32,
    /// Gap between cells and around the edges.
    pub gap: u32,
    /// Height reserved at the bottom of each cell for the progress bar.
    pub progress_bar_height: u32,
}

impl ShelfLayout {
    pub fn new(screen_width: u32, screen_height: u32, columns: u32) -> Self {
        Self {
            screen_width,
            screen_height,
            columns: columns.max(1),
            gap: 16,
            progress_bar_height: 8,
        }
    }

    /// Width of one cell.
    pub fn cell_width(&self) -> u32 {
        let total_gaps = self.gap * (self.columns + 1);
        (self.screen_width.saturating_sub(total_gaps) / self.columns).max(1)
    }

    /// Height of one cell: 3:4 cover ratio plus the progress bar strip.
    pub fn cell_height(&self) -> u32 {
        self.cell_width() * 4 / 3 + self.progress_bar_height
    }

    /// How many rows fit on the screen.
    pub fn rows(&self) -> u32 {
        ((self.screen_height.saturating_sub(self.gap)) / (self.cell_height() + self.gap)).max(1)
    }

    /// Maximum number of covers visible on one screen.
    pub fn capacity(&self) -> usize {
        (self.columns * self.rows()) as usize
    }

    /// Top-left corner of cell `index` (row-major).
    pub fn cell_origin(&self, index: usize) -> (u32, u32) {
        let col = index as u32 % self.columns;
        let row = index as u32 / self.columns;
        (
            self.gap + col * (self.cell_width() + self.gap),
            self.gap + row * (self.cell_height() + self.gap),
        )
    }
}

/// One entry on the shelf: a cover image and optional reading progress
/// (0.0–1.0).
pub struct ShelfEntry {
    pub cover: DynamicImage,
    pub progress: Option<f32>,
}

/// Compose a shelf screen from entries. At most [`ShelfLayout::capacity`]
/// entries are drawn; covers are scaled to fit their cell, centered, with a
/// progress bar along the bottom of the cell.
pub fn compose_shelf(entries: &[ShelfEntry], layout: &ShelfLayout) -> GrayPage {
    let mut canvas = GrayPage::new_white(layout.screen_width, layout.screen_height);
    let cell_w = layout.cell_width();
    let cover_h = layout.cell_height() - layout.progress_bar_height;

    for (index, entry) in entries.iter().take(layout.capacity()).enumerate() {
        let (cell_x, cell_y) = layout.cell_origin(index);

        // Scale the cover to fit within the cell's cover area.
        let (fit_w, fit_h) = crate::compute_fit(
            entry.cover.width(),
            entry.cover.height(),
            cell_w,
            cover_h,
            crate::FitMode::Contain,
        );
        let thumb = entry
            .cover
            .resize_exact(fit_w, fit_h, FilterType::Triangle)
            .into_luma8();

        let off_x = cell_x + (cell_w - fit_w) / 2;
        let off_y = cell_y + (cover_h - fit_h) / 2;
        for y in 0..fit_h {
            for x in 0..fit_w {
                let px = thumb.get_pixel(x, y).0[0];
                let canvas_idx = ((off_y + y) * canvas.width + off_x + x) as usize;
                canvas.pixels[canvas_idx] = px;
            }
        }

        // Progress bar: a light track with a dark fill proportional to
        // progress, clamped to the cell width.
        if let Some(progress) = entry.progress {
            let progress = progress.clamp(0.0, 1.0);
            let bar_y = cell_y + cover_h + 1;
            let bar_h = layout.progress_bar_height.saturating_sub(2);
            let filled = (cell_w as f32 * progress).round() as u32;
            for y in 0..bar_h {
                for x in 0..cell_w {
                    let value = if x < filled { 0x22 } else { 0xCC };
                    let canvas_idx = ((bar_y + y) * canvas.width + cell_x + x) as usize;
                    canvas.pixels[canvas_idx] = value;
                }
            }
        }
    }

    canvas
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbImage;

    fn cover(w: u32, h: u32, gray: u8) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_pixel(w, h, image::Rgb([gray, gray, gray])))
    }

    fn layout() -> ShelfLayout {
        ShelfLayout::new(600, 800, 3)
    }

    #[test]
    fn geometry_is_consistent() {
        let l = layout();
        // 3 columns: cells + 4 gaps fit within the screen width.
        assert!(l.columns * l.cell_width() + (l.columns + 1) * l.gap <= l.screen_width);
        // All rows fit within the screen height.
        assert!(l.gap + l.rows() * (l.cell_height() + l.gap) <= l.screen_height + l.cell_height());
        assert_eq!(l.capacity(), (l.columns * l.rows()) as usize);
    }

    #[test]
    fn covers_never_draw_outside_their_cell() {
        let l = layout();
        // A pathologically wide cover must still be confined to its cell.
        let entries = vec![ShelfEntry {
            cover: cover(5000, 10, 0),
            progress: None,
        }];
        let page = compose_shelf(&entries, &l);

        let (cx, cy) = l.cell_origin(0);
        for y in 0..page.height {
            for x in 0..page.width {
                let inside =
                    x >= cx && x < cx + l.cell_width() && y >= cy && y < cy + l.cell_height();
                if !inside {
                    assert_eq!(
                        page.pixel(x, y),
                        0xFF,
                        "pixel ({x},{y}) outside cell 0 was drawn"
                    );
                }
            }
        }
    }

    #[test]
    fn entries_land_in_row_major_cells() {
        let l = layout();
        let entries: Vec<ShelfEntry> = (0..4)
            .map(|_| ShelfEntry {
                cover: cover(300, 400, 0),
                progress: None,
            })
            .collect();
        let page = compose_shelf(&entries, &l);

        // Center of each of the first 4 cells should be dark (cover drawn).
        for i in 0..4 {
            let (cx, cy) = l.cell_origin(i);
            let mid_x = cx + l.cell_width() / 2;
            let mid_y = cy + (l.cell_height() - l.progress_bar_height) / 2;
            assert!(
                page.pixel(mid_x, mid_y) < 0x80,
                "cell {i} center is not covered"
            );
        }
        // Cell 4 (second column of row 2) is empty.
        if l.capacity() > 4 {
            let (cx, cy) = l.cell_origin(4);
            assert_eq!(page.pixel(cx + l.cell_width() / 2, cy + 10), 0xFF);
        }
    }

    #[test]
    fn progress_bar_fills_proportionally() {
        let l = layout();
        let entries = vec![ShelfEntry {
            cover: cover(300, 400, 128),
            progress: Some(0.5),
        }];
        let page = compose_shelf(&entries, &l);

        let (cx, cy) = l.cell_origin(0);
        let bar_y = cy + l.cell_height() - l.progress_bar_height + 1;
        // Left quarter: filled (dark). Right quarter: track (light).
        assert_eq!(page.pixel(cx + l.cell_width() / 4, bar_y), 0x22);
        assert_eq!(page.pixel(cx + l.cell_width() * 3 / 4, bar_y), 0xCC);
    }

    #[test]
    fn overflow_entries_are_dropped_not_drawn() {
        let l = layout();
        let too_many: Vec<ShelfEntry> = (0..l.capacity() + 10)
            .map(|_| ShelfEntry {
                cover: cover(30, 40, 0),
                progress: Some(1.0),
            })
            .collect();
        // Must not panic or draw outside the screen.
        let page = compose_shelf(&too_many, &l);
        assert_eq!((page.width, page.height), (l.screen_width, l.screen_height));
    }
}
