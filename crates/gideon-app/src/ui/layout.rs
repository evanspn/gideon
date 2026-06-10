//! Screen geometry and tap-zone math for the browse UI.
//!
//! Sized for a 1072x1448 Kobo panel (~64px rows with 28px text) but computed
//! from the actual display dimensions so smaller test displays work too.

/// Where a tap landed, in UI terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapTarget {
    /// The title bar at the top.
    Title,
    /// Content row `n` of the current page (0-based, top to bottom). May be
    /// past the end of the actual row list; callers must bounds-check.
    Row(usize),
    /// Bottom bar: \[Back\].
    Back,
    /// Bottom bar: \[Prev\].
    Prev,
    /// Bottom bar: \[Next\].
    Next,
}

/// Reader tap zones: thirds of the screen width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderZone {
    PrevPage,
    Back,
    NextPage,
}

#[derive(Debug, Clone, Copy)]
pub struct UiLayout {
    pub width: u32,
    pub height: u32,
    /// Title bar height (top).
    pub title_h: u32,
    /// Navigation bar height (bottom).
    pub nav_h: u32,
    /// Height of one content row.
    pub row_h: u32,
    /// Font size for row text.
    pub text_px: f32,
    /// Horizontal padding for text.
    pub pad: u32,
}

impl UiLayout {
    pub fn new(width: u32, height: u32) -> Self {
        // 1448px tall → 64px rows; scale proportionally, clamped to stay
        // readable on small test displays.
        let row_h = (height * 64 / 1448).clamp(24, 96);
        let text_px = (row_h as f32 * 28.0 / 64.0).clamp(11.0, 40.0);
        Self {
            width,
            height,
            title_h: row_h,
            nav_h: row_h,
            row_h,
            text_px,
            pad: (width / 64).max(4),
        }
    }

    /// First content pixel row.
    pub fn content_top(&self) -> u32 {
        self.title_h
    }

    /// Height of the content area between the bars.
    pub fn content_height(&self) -> u32 {
        self.height.saturating_sub(self.title_h + self.nav_h)
    }

    /// First pixel row of the bottom navigation bar.
    pub fn nav_top(&self) -> u32 {
        self.height - self.nav_h
    }

    /// How many list rows fit on one page.
    pub fn rows_per_page(&self) -> usize {
        ((self.content_height() / self.row_h).max(1)) as usize
    }

    /// Y position of content row `i` on the current page.
    pub fn row_top(&self, i: usize) -> u32 {
        self.content_top() + i as u32 * self.row_h
    }

    /// Resolve a tap into a [`TapTarget`].
    pub fn tap_target(&self, x: u32, y: u32) -> TapTarget {
        if y >= self.nav_top() {
            let third = (self.width / 3).max(1);
            if x < third {
                TapTarget::Back
            } else if x < 2 * third {
                TapTarget::Prev
            } else {
                TapTarget::Next
            }
        } else if y >= self.content_top() {
            TapTarget::Row(((y - self.content_top()) / self.row_h) as usize)
        } else {
            TapTarget::Title
        }
    }

    /// Resolve a tap inside the reader into a zone (thirds of the width:
    /// left = previous page, center = back, right = next page).
    pub fn reader_zone(&self, x: u32) -> ReaderZone {
        let third = (self.width / 3).max(1);
        if x < third {
            ReaderZone::PrevPage
        } else if x < 2 * third {
            ReaderZone::Back
        } else {
            ReaderZone::NextPage
        }
    }

    /// Number of pages needed to show `n` rows.
    pub fn page_count(&self, n: usize) -> usize {
        n.div_ceil(self.rows_per_page()).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kobo_sized_layout_has_64px_rows() {
        let l = UiLayout::new(1072, 1448);
        assert_eq!(l.row_h, 64);
        assert_eq!(l.title_h, 64);
        assert_eq!(l.nav_h, 64);
        assert!((l.text_px - 28.0).abs() < 0.01);
        assert_eq!(l.content_top(), 64);
        assert_eq!(l.nav_top(), 1448 - 64);
        assert_eq!(l.rows_per_page(), ((1448 - 128) / 64) as usize);
    }

    #[test]
    fn tap_zones_resolve_exactly() {
        let l = UiLayout::new(1072, 1448);
        // Title bar.
        assert_eq!(l.tap_target(500, 0), TapTarget::Title);
        assert_eq!(l.tap_target(500, 63), TapTarget::Title);
        // First row starts at 64.
        assert_eq!(l.tap_target(500, 64), TapTarget::Row(0));
        assert_eq!(l.tap_target(500, 127), TapTarget::Row(0));
        assert_eq!(l.tap_target(500, 128), TapTarget::Row(1));
        // Nav bar thirds: width 1072 → third = 357.
        let nav_y = l.nav_top();
        assert_eq!(l.tap_target(0, nav_y), TapTarget::Back);
        assert_eq!(l.tap_target(356, nav_y), TapTarget::Back);
        assert_eq!(l.tap_target(357, nav_y), TapTarget::Prev);
        assert_eq!(l.tap_target(713, nav_y), TapTarget::Prev);
        assert_eq!(l.tap_target(714, nav_y), TapTarget::Next);
        assert_eq!(l.tap_target(1071, 1447), TapTarget::Next);
        // Last pixel above the nav bar is still a row.
        assert!(matches!(l.tap_target(500, nav_y - 1), TapTarget::Row(_)));
    }

    #[test]
    fn reader_zones_are_thirds() {
        let l = UiLayout::new(900, 1200);
        assert_eq!(l.reader_zone(0), ReaderZone::PrevPage);
        assert_eq!(l.reader_zone(299), ReaderZone::PrevPage);
        assert_eq!(l.reader_zone(300), ReaderZone::Back);
        assert_eq!(l.reader_zone(599), ReaderZone::Back);
        assert_eq!(l.reader_zone(600), ReaderZone::NextPage);
        assert_eq!(l.reader_zone(899), ReaderZone::NextPage);
    }

    #[test]
    fn page_count_rounds_up() {
        let l = UiLayout::new(1072, 1448);
        let per = l.rows_per_page();
        assert_eq!(l.page_count(0), 1);
        assert_eq!(l.page_count(per), 1);
        assert_eq!(l.page_count(per + 1), 2);
    }

    #[test]
    fn small_display_stays_usable() {
        let l = UiLayout::new(300, 400);
        assert!(l.row_h >= 24);
        assert!(l.rows_per_page() >= 1);
        assert!(l.content_height() > 0);
    }
}
