//! gideon-device: hardware abstraction for displays and (eventually) input.
//!
//! The [`Display`] trait is what the reader UI draws against. Backends:
//!
//! * [`MemoryDisplay`] — in-memory, used by tests and headless rendering.
//! * `KoboDisplay` (feature `kobo`) — Linux framebuffer with mxcfb e-ink
//!   refresh ioctls, for actual Kobo hardware.

use gideon_render::{GrayPage, RgbPage};

pub mod input;
#[cfg(feature = "kobo")]
pub mod kobo;
#[cfg(feature = "kobo")]
pub mod kobo_input;
pub mod light;
pub mod power;

pub use input::{FakeInput, InputSource, TouchTransform, UiEvent};
pub use light::{KoboFrontlight, LightControl};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("display error: {0}")]
    Display(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The Kaleido color-filter-array post-process applied to color refreshes.
///
/// The panel can run a hardware saturation boost over the color waveform.
/// The strongest setting makes color pop but, stacked on the panel's Y8→Y4
/// quantization, bands smooth gradients ("rainbow banding"). This mirrors
/// KOReader's `noCFAPostProcess` escape hatch so the boost can be dialed
/// down or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorPostProcess {
    /// Strongest saturation boost (KOReader's default, `CFA_EINK_G2`):
    /// vivid color, but can band gradients on the Kaleido panel.
    #[default]
    Vivid,
    /// Standard CFA gain (`CFA_EINK_G1`): no extra boost, no banding.
    Standard,
    /// No CFA post-process at all: flattest color, never bands.
    Off,
}

impl ColorPostProcess {
    /// Parse the settings.json value leniently (defaults to `Vivid`).
    pub fn from_setting(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "standard" => Self::Standard,
            "off" | "none" => Self::Off,
            _ => Self::Vivid,
        }
    }

    /// The settings.json token for this mode.
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Vivid => "vivid",
            Self::Standard => "standard",
            Self::Off => "off",
        }
    }
}

/// How the e-ink panel should refresh after a draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    /// Full refresh: slow, flashes black, removes ghosting. Use on page turns
    /// every N pages and for first paint.
    Full,
    /// Partial refresh: fast, may leave ghosting. Use for UI updates.
    Partial,
}

/// An abstract grayscale display.
pub trait Display {
    fn width(&self) -> u32;
    fn height(&self) -> u32;

    /// Copy a rendered page onto the display's backbuffer. `page` may be
    /// larger than the screen (e.g. FitWidth); `offset_y` selects the
    /// vertical scroll position within it.
    fn blit(&mut self, page: &GrayPage, offset_y: u32) -> Result<()>;

    /// Copy a rendered COLOR page onto the backbuffer. The default
    /// collapses to grayscale (Rec.601 luma) and blits that, so
    /// monochrome backends and tests behave exactly like [`Self::blit`];
    /// color-capable backends (Kaleido) override it.
    fn blit_rgb(&mut self, page: &RgbPage, offset_y: u32) -> Result<()> {
        self.blit(&page.to_gray(), offset_y)
    }

    /// Draw a small grayscale overlay (e.g. the reader's page indicator)
    /// on top of whatever the backbuffer currently holds, at panel
    /// coordinates (`x`, `y`), clipped to the screen. Unlike [`Self::blit`]
    /// this never clears, centers or scrolls — the rest of the buffer is
    /// untouched, so callers can blit a cached page zero-copy and stamp
    /// chrome on top.
    fn overlay(&mut self, page: &GrayPage, x: u32, y: u32) -> Result<()>;

    /// Push the backbuffer to the physical screen.
    fn flush(&mut self, mode: RefreshMode) -> Result<()>;

    /// Set the Kaleido color post-process applied to color refreshes.
    /// Default: no-op (only the Kobo color backend honors it).
    fn set_color_post_process(&mut self, _mode: ColorPostProcess) {}
}

/// Allow driving a display through a mutable reference, so a UI can lend
/// its display to a nested session (e.g. the reader) without giving it up.
impl<D: Display + ?Sized> Display for &mut D {
    fn width(&self) -> u32 {
        (**self).width()
    }

    fn height(&self) -> u32 {
        (**self).height()
    }

    fn blit(&mut self, page: &GrayPage, offset_y: u32) -> Result<()> {
        (**self).blit(page, offset_y)
    }

    fn blit_rgb(&mut self, page: &RgbPage, offset_y: u32) -> Result<()> {
        (**self).blit_rgb(page, offset_y)
    }

    fn overlay(&mut self, page: &GrayPage, x: u32, y: u32) -> Result<()> {
        (**self).overlay(page, x, y)
    }

    fn flush(&mut self, mode: RefreshMode) -> Result<()> {
        (**self).flush(mode)
    }

    fn set_color_post_process(&mut self, mode: ColorPostProcess) {
        (**self).set_color_post_process(mode)
    }
}

/// In-memory display backend for tests and headless use.
pub struct MemoryDisplay {
    width: u32,
    height: u32,
    /// The current backbuffer, row-major 8-bit grayscale.
    pub buffer: Vec<u8>,
    /// Refresh modes recorded by `flush`, for assertions.
    pub flushes: Vec<RefreshMode>,
    /// One entry per blit, recording whether it carried color (`blit_rgb`)
    /// — so tests can pin that B/W pages never take the RGB path and that
    /// color pages never silently fall back to gray.
    pub blits: Vec<bool>,
}

impl MemoryDisplay {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            buffer: vec![0xFF; (width * height) as usize],
            flushes: Vec::new(),
            blits: Vec::new(),
        }
    }

    pub fn pixel(&self, x: u32, y: u32) -> u8 {
        self.buffer[(y * self.width + x) as usize]
    }
}

impl Display for MemoryDisplay {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn blit(&mut self, page: &GrayPage, offset_y: u32) -> Result<()> {
        self.blits.push(false);
        blit_into(&mut self.buffer, self.width, self.height, page, offset_y);
        Ok(())
    }

    fn blit_rgb(&mut self, page: &RgbPage, offset_y: u32) -> Result<()> {
        // Identical pixels to the trait default (Rec.601 luma into the
        // gray backbuffer) — overridden only to record `true` here.
        self.blits.push(true);
        blit_into(
            &mut self.buffer,
            self.width,
            self.height,
            &page.to_gray(),
            offset_y,
        );
        Ok(())
    }

    fn overlay(&mut self, page: &GrayPage, x: u32, y: u32) -> Result<()> {
        overlay_into(&mut self.buffer, self.width, self.height, page, x, y);
        Ok(())
    }

    fn flush(&mut self, mode: RefreshMode) -> Result<()> {
        self.flushes.push(mode);
        Ok(())
    }
}

/// Shared blit implementation: copy the visible window of `page` (starting
/// at `offset_y`) into a `screen_w x screen_h` backbuffer, centering
/// horizontally and padding with white.
pub(crate) fn blit_into(
    buffer: &mut [u8],
    screen_w: u32,
    screen_h: u32,
    page: &GrayPage,
    offset_y: u32,
) {
    buffer.fill(0xFF);

    let copy_w = page.width.min(screen_w);
    let dst_x = (screen_w - copy_w) / 2;
    let src_x = (page.width - copy_w) / 2;

    let max_offset = page.height.saturating_sub(1);
    let offset_y = offset_y.min(max_offset);
    let copy_h = (page.height - offset_y).min(screen_h);

    for row in 0..copy_h {
        let src_start = ((offset_y + row) * page.width + src_x) as usize;
        let dst_start = (row * screen_w + dst_x) as usize;
        buffer[dst_start..dst_start + copy_w as usize]
            .copy_from_slice(&page.pixels[src_start..src_start + copy_w as usize]);
    }
}

/// Shared overlay implementation: copy `page` on top of the backbuffer at
/// (`x`, `y`), clipped to the screen, leaving everything else untouched.
pub(crate) fn overlay_into(
    buffer: &mut [u8],
    screen_w: u32,
    screen_h: u32,
    page: &GrayPage,
    x: u32,
    y: u32,
) {
    let copy_w = page.width.min(screen_w.saturating_sub(x)) as usize;
    let copy_h = page.height.min(screen_h.saturating_sub(y));
    if copy_w == 0 {
        return;
    }
    for row in 0..copy_h {
        let src_start = (row * page.width) as usize;
        let dst_start = ((y + row) * screen_w + x) as usize;
        buffer[dst_start..dst_start + copy_w]
            .copy_from_slice(&page.pixels[src_start..src_start + copy_w]);
    }
}

/// Like [`blit_into`], but for packed RGB (3 bytes per pixel) buffers.
/// Same window/centering/padding semantics as the grayscale version.
#[cfg(feature = "kobo")]
pub(crate) fn blit_rgb_into(
    buffer: &mut [u8],
    screen_w: u32,
    screen_h: u32,
    page: &RgbPage,
    offset_y: u32,
) {
    buffer.fill(0xFF);

    let copy_w = page.width.min(screen_w);
    let dst_x = (screen_w - copy_w) / 2;
    let src_x = (page.width - copy_w) / 2;

    let max_offset = page.height.saturating_sub(1);
    let offset_y = offset_y.min(max_offset);
    let copy_h = (page.height - offset_y).min(screen_h);

    for row in 0..copy_h {
        let src_start = (((offset_y + row) * page.width + src_x) * 3) as usize;
        let dst_start = ((row * screen_w + dst_x) * 3) as usize;
        buffer[dst_start..dst_start + copy_w as usize * 3]
            .copy_from_slice(&page.pixels[src_start..src_start + copy_w as usize * 3]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_filled(w: u32, h: u32, value: u8) -> GrayPage {
        GrayPage {
            width: w,
            height: h,
            pixels: vec![value; (w * h) as usize],
        }
    }

    #[test]
    fn blit_exact_fit_copies_everything() {
        let mut d = MemoryDisplay::new(4, 4);
        d.blit(&page_filled(4, 4, 0x00), 0).unwrap();
        assert!(d.buffer.iter().all(|&p| p == 0x00));
    }

    #[test]
    fn blit_smaller_page_is_centered_horizontally() {
        let mut d = MemoryDisplay::new(6, 2);
        d.blit(&page_filled(2, 2, 0x00), 0).unwrap();
        // Columns 2 and 3 are black, the rest white.
        assert_eq!(d.pixel(0, 0), 0xFF);
        assert_eq!(d.pixel(1, 0), 0xFF);
        assert_eq!(d.pixel(2, 0), 0x00);
        assert_eq!(d.pixel(3, 0), 0x00);
        assert_eq!(d.pixel(4, 0), 0xFF);
        assert_eq!(d.pixel(5, 0), 0xFF);
    }

    #[test]
    fn blit_tall_page_scrolls_with_offset() {
        // Page rows: row i has value i (height 10).
        let mut page = page_filled(2, 10, 0);
        for y in 0..10u32 {
            for x in 0..2u32 {
                page.pixels[(y * 2 + x) as usize] = y as u8;
            }
        }

        let mut d = MemoryDisplay::new(2, 3);
        d.blit(&page, 4).unwrap();
        assert_eq!(d.pixel(0, 0), 4);
        assert_eq!(d.pixel(0, 1), 5);
        assert_eq!(d.pixel(0, 2), 6);
    }

    #[test]
    fn blit_offset_past_end_is_clamped() {
        let mut d = MemoryDisplay::new(2, 2);
        d.blit(&page_filled(2, 4, 0x00), 100).unwrap();
        // Clamped to last row: one row of page visible, rest white.
        assert_eq!(d.pixel(0, 0), 0x00);
        assert_eq!(d.pixel(0, 1), 0xFF);
    }

    #[test]
    fn default_blit_rgb_collapses_with_rec601_luma() {
        // A backend that does NOT override blit_rgb: the trait default
        // must convert with Rec.601 (0.299 R + 0.587 G + 0.114 B) and
        // route through the plain gray blit.
        struct GrayOnly(Vec<u8>);
        impl Display for GrayOnly {
            fn width(&self) -> u32 {
                3
            }
            fn height(&self) -> u32 {
                1
            }
            fn blit(&mut self, page: &GrayPage, offset_y: u32) -> Result<()> {
                blit_into(&mut self.0, 3, 1, page, offset_y);
                Ok(())
            }
            fn overlay(&mut self, _page: &GrayPage, _x: u32, _y: u32) -> Result<()> {
                Ok(())
            }
            fn flush(&mut self, _mode: RefreshMode) -> Result<()> {
                Ok(())
            }
        }

        let page = RgbPage {
            width: 3,
            height: 1,
            pixels: vec![255, 0, 0, 0, 255, 0, 0, 0, 255],
        };
        let mut plain = GrayOnly(vec![0xFF; 3]);
        plain.blit_rgb(&page, 0).unwrap();
        assert_eq!(plain.0, vec![76, 150, 29]);

        // MemoryDisplay's override shows the same pixels…
        let mut d = MemoryDisplay::new(3, 1);
        d.blit_rgb(&page, 0).unwrap();
        assert_eq!(d.buffer, vec![76, 150, 29]);
        // …and records that the blit carried color.
        assert_eq!(d.blits, vec![true]);
    }

    #[test]
    fn memory_display_records_color_per_blit() {
        let mut d = MemoryDisplay::new(2, 2);
        d.blit(&page_filled(2, 2, 0x00), 0).unwrap();
        let rgb = RgbPage::from_gray(&page_filled(2, 2, 0x80));
        d.blit_rgb(&rgb, 0).unwrap();
        d.blit(&page_filled(2, 2, 0xFF), 0).unwrap();
        assert_eq!(d.blits, vec![false, true, false]);
    }

    #[test]
    fn overlay_draws_on_top_without_clearing_the_rest() {
        let mut d = MemoryDisplay::new(4, 4);
        d.blit(&page_filled(4, 4, 0x80), 0).unwrap();
        d.overlay(&page_filled(2, 2, 0x00), 1, 1).unwrap();
        // The 2x2 box landed at (1,1); everything else kept the blit.
        assert_eq!(d.pixel(1, 1), 0x00);
        assert_eq!(d.pixel(2, 2), 0x00);
        assert_eq!(d.pixel(0, 0), 0x80);
        assert_eq!(d.pixel(3, 1), 0x80);
        assert_eq!(d.pixel(1, 3), 0x80);
    }

    #[test]
    fn overlay_is_clipped_at_the_screen_edges() {
        let mut d = MemoryDisplay::new(4, 4);
        d.blit(&page_filled(4, 4, 0x80), 0).unwrap();
        d.overlay(&page_filled(3, 3, 0x00), 2, 3).unwrap();
        // Only the on-screen sliver is drawn; nothing panics or wraps.
        assert_eq!(d.pixel(2, 3), 0x00);
        assert_eq!(d.pixel(3, 3), 0x00);
        assert_eq!(d.pixel(1, 3), 0x80);
        assert_eq!(d.pixel(2, 2), 0x80);
        // Fully off-screen overlays are a no-op.
        d.overlay(&page_filled(2, 2, 0x00), 9, 9).unwrap();
        assert_eq!(d.pixel(0, 0), 0x80);
    }

    #[test]
    fn color_post_process_parses_and_round_trips() {
        assert_eq!(
            ColorPostProcess::from_setting("vivid"),
            ColorPostProcess::Vivid
        );
        assert_eq!(
            ColorPostProcess::from_setting("standard"),
            ColorPostProcess::Standard
        );
        assert_eq!(ColorPostProcess::from_setting("OFF"), ColorPostProcess::Off);
        assert_eq!(
            ColorPostProcess::from_setting(" none "),
            ColorPostProcess::Off
        );
        // Unknown / empty fall back to the vivid default.
        assert_eq!(
            ColorPostProcess::from_setting("zzz"),
            ColorPostProcess::Vivid
        );
        assert_eq!(ColorPostProcess::default(), ColorPostProcess::Vivid);
        for mode in [
            ColorPostProcess::Vivid,
            ColorPostProcess::Standard,
            ColorPostProcess::Off,
        ] {
            assert_eq!(ColorPostProcess::from_setting(mode.as_setting()), mode);
        }
        // The default Display impl ignores it (no panic, no state).
        let mut d = MemoryDisplay::new(1, 1);
        d.set_color_post_process(ColorPostProcess::Off);
    }

    #[test]
    fn flush_records_modes() {
        let mut d = MemoryDisplay::new(1, 1);
        d.flush(RefreshMode::Full).unwrap();
        d.flush(RefreshMode::Partial).unwrap();
        assert_eq!(d.flushes, vec![RefreshMode::Full, RefreshMode::Partial]);
    }
}
