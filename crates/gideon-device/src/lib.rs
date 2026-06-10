//! gideon-device: hardware abstraction for displays and (eventually) input.
//!
//! The [`Display`] trait is what the reader UI draws against. Backends:
//!
//! * [`MemoryDisplay`] — in-memory, used by tests and headless rendering.
//! * `KoboDisplay` (feature `kobo`) — Linux framebuffer with mxcfb e-ink
//!   refresh ioctls, for actual Kobo hardware.

use gideon_render::GrayPage;

pub mod input;
#[cfg(feature = "kobo")]
pub mod kobo;
#[cfg(feature = "kobo")]
pub mod kobo_input;

pub use input::{FakeInput, InputSource, TouchTransform, UiEvent};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("display error: {0}")]
    Display(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

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

    /// Push the backbuffer to the physical screen.
    fn flush(&mut self, mode: RefreshMode) -> Result<()>;
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

    fn flush(&mut self, mode: RefreshMode) -> Result<()> {
        (**self).flush(mode)
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
}

impl MemoryDisplay {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            buffer: vec![0xFF; (width * height) as usize],
            flushes: Vec::new(),
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
        blit_into(&mut self.buffer, self.width, self.height, page, offset_y);
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
    fn flush_records_modes() {
        let mut d = MemoryDisplay::new(1, 1);
        d.flush(RefreshMode::Full).unwrap();
        d.flush(RefreshMode::Partial).unwrap();
        assert_eq!(d.flushes, vec![RefreshMode::Full, RefreshMode::Partial]);
    }
}
