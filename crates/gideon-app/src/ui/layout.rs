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

/// A key on the on-screen search keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Backspace,
    Space,
    /// Run the search with the current query.
    Search,
}

/// Character rows of the keyboard, top to bottom. The bottom row
/// (backspace / space / search) is built separately in [`keyboard_keys`].
pub const KEYBOARD_ROWS: [&str; 4] = ["1234567890", "qwertyuiop", "asdfghjkl", "zxcvbnm"];

/// A key and its screen rectangle: `(key, x, y, w, h)`.
pub type KeyRect = (Key, u32, u32, u32, u32);

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

    /// Resolve a *rotated* reader tap: the panel coordinates are first
    /// mapped into reading orientation (see [`map_reader_tap`]), then the
    /// zone is computed against the reading-orientation width — so
    /// next/prev/back follow the direction the user is reading in, not the
    /// panel.
    pub fn reader_zone_rotated(&self, x: u32, y: u32, rotation: u32) -> ReaderZone {
        let (rx, _ry) = map_reader_tap(x, y, self.width, self.height, rotation);
        let reading_width = if rotation % 180 == 90 {
            self.height
        } else {
            self.width
        };
        reader_zone_in_width(reading_width, rx)
    }

    /// Number of pages needed to show `n` rows.
    pub fn page_count(&self, n: usize) -> usize {
        n.div_ceil(self.rows_per_page()).max(1)
    }

    /// Height of one keyboard key: taller than a list row so the targets
    /// are finger-sized on e-ink.
    pub fn key_h(&self) -> u32 {
        self.row_h * 3 / 2
    }

    /// First pixel row of the on-screen keyboard. The keyboard sits at the
    /// bottom of the content area, directly above the navigation bar.
    pub fn keyboard_top(&self) -> u32 {
        let kb_h = self.key_h() * (KEYBOARD_ROWS.len() as u32 + 1);
        self.nav_top().saturating_sub(kb_h).max(self.content_top())
    }

    /// Every keyboard key with its screen rectangle, for both rendering
    /// and hit-testing (one source of truth).
    pub fn keyboard_keys(&self) -> Vec<KeyRect> {
        let key_h = self.key_h();
        let mut keys = Vec::new();
        for (i, row) in KEYBOARD_ROWS.iter().enumerate() {
            let y = self.keyboard_top() + i as u32 * key_h;
            let n = row.chars().count() as u32;
            let w = (self.width / n).max(1);
            // Center the row when keys don't fill the full width.
            let x0 = (self.width - w * n) / 2;
            for (j, c) in row.chars().enumerate() {
                keys.push((Key::Char(c), x0 + j as u32 * w, y, w, key_h));
            }
        }
        // Bottom row in tenths: [backspace 2][space 5][search 3].
        let y = self.keyboard_top() + KEYBOARD_ROWS.len() as u32 * key_h;
        let unit = (self.width / 10).max(1);
        keys.push((Key::Backspace, 0, y, 2 * unit, key_h));
        keys.push((Key::Space, 2 * unit, y, 5 * unit, key_h));
        keys.push((Key::Search, 7 * unit, y, self.width - 7 * unit, key_h));
        keys
    }

    /// The key under a tap, if any.
    pub fn key_at(&self, x: u32, y: u32) -> Option<Key> {
        self.keyboard_keys()
            .into_iter()
            .find(|&(_, kx, ky, kw, kh)| x >= kx && x < kx + kw && y >= ky && y < ky + kh)
            .map(|(key, ..)| key)
    }
}

/// Reader zone for a tap at `x` within a screen of `width` (thirds: left =
/// previous, center = back, right = next).
pub fn reader_zone_in_width(width: u32, x: u32) -> ReaderZone {
    let third = (width / 3).max(1);
    if x < third {
        ReaderZone::PrevPage
    } else if x < 2 * third {
        ReaderZone::Back
    } else {
        ReaderZone::NextPage
    }
}

/// Map a tap at panel coordinates `(x, y)` into reading-orientation
/// coordinates for a reader rotated clockwise by `rotation` degrees.
///
/// The displayed image is `rotate_page(reading_image, rotation)`, so this
/// applies the inverse rotation. For 90/270 the reading-orientation screen
/// is `panel_h x panel_w`.
pub fn map_reader_tap(x: u32, y: u32, panel_w: u32, panel_h: u32, rotation: u32) -> (u32, u32) {
    match rotation % 360 {
        90 => (y, panel_w.saturating_sub(1).saturating_sub(x)),
        180 => (
            panel_w.saturating_sub(1).saturating_sub(x),
            panel_h.saturating_sub(1).saturating_sub(y),
        ),
        270 => (panel_h.saturating_sub(1).saturating_sub(y), x),
        _ => (x, y),
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
        assert_eq!(reader_zone_in_width(900, 0), ReaderZone::PrevPage);
        assert_eq!(reader_zone_in_width(900, 299), ReaderZone::PrevPage);
        assert_eq!(reader_zone_in_width(900, 300), ReaderZone::Back);
        assert_eq!(reader_zone_in_width(900, 599), ReaderZone::Back);
        assert_eq!(reader_zone_in_width(900, 600), ReaderZone::NextPage);
        assert_eq!(reader_zone_in_width(900, 899), ReaderZone::NextPage);
    }

    #[test]
    fn map_reader_tap_identity_at_rotation_0() {
        assert_eq!(map_reader_tap(5, 7, 100, 200, 0), (5, 7));
        assert_eq!(map_reader_tap(99, 199, 100, 200, 0), (99, 199));
    }

    #[test]
    fn map_reader_tap_rotation_90() {
        // Display = rotate_cw(reading, 90): reading screen is 200x100.
        // Panel top-left came from the reading bottom-left corner area.
        assert_eq!(map_reader_tap(0, 0, 100, 200, 90), (0, 99));
        assert_eq!(map_reader_tap(99, 0, 100, 200, 90), (0, 0));
        assert_eq!(map_reader_tap(99, 199, 100, 200, 90), (199, 0));
        assert_eq!(map_reader_tap(0, 199, 100, 200, 90), (199, 99));
    }

    #[test]
    fn map_reader_tap_rotation_180() {
        assert_eq!(map_reader_tap(0, 0, 100, 200, 180), (99, 199));
        assert_eq!(map_reader_tap(99, 199, 100, 200, 180), (0, 0));
        assert_eq!(map_reader_tap(5, 7, 100, 200, 180), (94, 192));
    }

    #[test]
    fn map_reader_tap_rotation_270() {
        // Display = rotate_cw(reading, 270): reading screen is 200x100.
        assert_eq!(map_reader_tap(0, 0, 100, 200, 270), (199, 0));
        assert_eq!(map_reader_tap(99, 0, 100, 200, 270), (199, 99));
        assert_eq!(map_reader_tap(0, 199, 100, 200, 270), (0, 0));
        assert_eq!(map_reader_tap(99, 199, 100, 200, 270), (0, 99));
    }

    #[test]
    fn map_reader_tap_round_trips_through_the_pixel_rotation() {
        // For every rotation, a tap on a panel pixel must map to the
        // reading-orientation pixel that rotate_page moved there.
        use gideon_render::{rotate_page, GrayPage};
        for rotation in [0u32, 90, 180, 270] {
            let (rw, rh) = if rotation % 180 == 90 { (5, 4) } else { (4, 5) };
            // Reading-orientation page with unique pixel values.
            let reading = GrayPage {
                width: rw,
                height: rh,
                pixels: (0..(rw * rh) as usize).map(|i| i as u8).collect(),
            };
            let panel = rotate_page(&reading, rotation);
            for py in 0..panel.height {
                for px in 0..panel.width {
                    let (x, y) = map_reader_tap(px, py, panel.width, panel.height, rotation);
                    assert_eq!(
                        reading.pixel(x, y),
                        panel.pixel(px, py),
                        "rotation {rotation}, panel ({px},{py}) -> reading ({x},{y})"
                    );
                }
            }
        }
    }

    #[test]
    fn rotated_reader_zones_follow_reading_orientation() {
        let l = UiLayout::new(900, 1200);

        // Rotation 0: thirds of the panel width.
        assert_eq!(l.reader_zone_rotated(0, 600, 0), ReaderZone::PrevPage);
        assert_eq!(l.reader_zone_rotated(450, 600, 0), ReaderZone::Back);
        assert_eq!(l.reader_zone_rotated(899, 600, 0), ReaderZone::NextPage);

        // Rotation 90 (clockwise): reading width is the panel height
        // (1200); reading-x = panel-y, so "next" is the bottom of the panel.
        assert_eq!(l.reader_zone_rotated(450, 0, 90), ReaderZone::PrevPage);
        assert_eq!(l.reader_zone_rotated(450, 600, 90), ReaderZone::Back);
        assert_eq!(l.reader_zone_rotated(450, 1199, 90), ReaderZone::NextPage);

        // Rotation 180: reading is upside down — "next" is the panel left.
        assert_eq!(l.reader_zone_rotated(899, 600, 180), ReaderZone::PrevPage);
        assert_eq!(l.reader_zone_rotated(450, 600, 180), ReaderZone::Back);
        assert_eq!(l.reader_zone_rotated(0, 600, 180), ReaderZone::NextPage);

        // Rotation 270: "next" is the top of the panel.
        assert_eq!(l.reader_zone_rotated(450, 1199, 270), ReaderZone::PrevPage);
        assert_eq!(l.reader_zone_rotated(450, 600, 270), ReaderZone::Back);
        assert_eq!(l.reader_zone_rotated(450, 0, 270), ReaderZone::NextPage);
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

    #[test]
    fn keyboard_keys_cover_all_characters_once() {
        let l = UiLayout::new(1072, 1448);
        let keys = l.keyboard_keys();
        let chars: Vec<char> = keys
            .iter()
            .filter_map(|(k, ..)| match k {
                Key::Char(c) => Some(*c),
                _ => None,
            })
            .collect();
        let expected: Vec<char> = KEYBOARD_ROWS.iter().flat_map(|r| r.chars()).collect();
        assert_eq!(chars, expected);
        assert_eq!(keys.len(), expected.len() + 3, "backspace, space, search");
    }

    #[test]
    fn keyboard_fits_between_content_top_and_nav_bar() {
        for (w, h) in [(1072, 1448), (1264, 1680), (600, 800), (300, 400)] {
            let l = UiLayout::new(w, h);
            assert!(l.keyboard_top() >= l.content_top(), "{w}x{h}");
            for (key, _, y, _, kh) in l.keyboard_keys() {
                assert!(y + kh <= l.nav_top(), "{key:?} overlaps nav bar at {w}x{h}");
            }
        }
    }

    #[test]
    fn key_at_hits_centers_and_misses_outside() {
        let l = UiLayout::new(1072, 1448);
        for (key, x, y, w, h) in l.keyboard_keys() {
            assert_eq!(l.key_at(x + w / 2, y + h / 2), Some(key));
        }
        // Above the keyboard (query area) is not a key.
        assert_eq!(l.key_at(l.width / 2, l.keyboard_top() - 1), None);
        // The nav bar is not a key.
        assert_eq!(l.key_at(l.width / 2, l.nav_top()), None);
    }

    #[test]
    fn key_at_resolves_known_keys() {
        let l = UiLayout::new(1000, 1448);
        let top = l.keyboard_top();
        let key_h = l.key_h();
        // First key of the first row is '1' (10 keys of 100px each).
        assert_eq!(l.key_at(50, top + 1), Some(Key::Char('1')));
        // Second row starts with 'q'.
        assert_eq!(l.key_at(50, top + key_h), Some(Key::Char('q')));
        // Bottom row tenths: backspace 0..200, space 200..700, search 700..
        let bottom = top + 4 * key_h;
        assert_eq!(l.key_at(100, bottom), Some(Key::Backspace));
        assert_eq!(l.key_at(450, bottom), Some(Key::Space));
        assert_eq!(l.key_at(999, bottom), Some(Key::Search));
    }
}
