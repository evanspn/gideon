//! Reader session: ties a [`CbzDocument`] to a [`Display`] and tracks the
//! current page, scroll position and reading progress.
//!
//! This is deliberately generic over the display so the whole page-turn /
//! refresh policy is unit-testable with `MemoryDisplay`.
//!
//! Three reader features live here:
//!
//! * **FitWidth scrolling** — in [`FitMode::FitWidth`] the rendered page is
//!   taller than the screen; `next_page` scrolls down (with a small overlap)
//!   until the bottom is reached, and only then turns the page. `prev_page`
//!   scrolls up first, and enters the previous page at its bottom.
//! * **Pre-decoding** — while page N is on screen, a [`Prefetcher`] decodes
//!   page N+1 on a background thread (with its own [`CbzDocument`] handle)
//!   so the next page turn doesn't wait on the decoder.
//! * **Rotation** — pages are rendered against the *reading* orientation
//!   (screen dimensions swapped for 90/270) and the visible window is
//!   rotated into the panel orientation just before blitting.

use anyhow::Result;
use gideon_core::{CbzDocument, ProgressStore};
use gideon_device::{Display, RefreshMode};
use gideon_render::{render_page, rotate_page, FitMode, GrayPage, RenderOptions};
use image::DynamicImage;
use std::sync::mpsc;
use std::thread::JoinHandle;

/// Do a full (flashing) e-ink refresh every N page turns to clear ghosting;
/// partial refreshes in between keep page turns fast.
const FULL_REFRESH_INTERVAL: u32 = 6;

/// When scrolling within a FitWidth page, keep this many pixels of the
/// previous view visible so the reader doesn't lose their place.
const SCROLL_OVERLAP_PX: u32 = 60;

pub struct Reader<D: Display> {
    doc: CbzDocument,
    display: D,
    fit: FitMode,
    /// Reading rotation in degrees, normalized to 0/90/180/270.
    rotation: u32,
    current_page: usize,
    /// Vertical scroll within the current page, in reading orientation.
    scroll_y: u32,
    /// The current page rendered in reading orientation, keyed by index,
    /// so scrolling never re-decodes.
    rendered: Option<(usize, GrayPage)>,
    prefetcher: Prefetcher,
    turns_since_full_refresh: u32,
}

impl<D: Display> Reader<D> {
    pub fn new(doc: CbzDocument, display: D, fit: FitMode, rotation: u32) -> Self {
        // The prefetch thread needs its own archive handle; if re-opening
        // fails we degrade gracefully to synchronous decoding.
        let prefetcher = Prefetcher::new(doc.try_clone().ok());
        Self {
            doc,
            display,
            fit,
            rotation: normalize_rotation(rotation),
            current_page: 0,
            scroll_y: 0,
            rendered: None,
            prefetcher,
            turns_since_full_refresh: 0,
        }
    }

    /// Access the underlying display (used by tests and debug tooling).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn display(&self) -> &D {
        &self.display
    }

    /// Resume from saved progress, clamping to the document length.
    pub fn resume_from(&mut self, store: &ProgressStore, key: &str) {
        if let Some(progress) = store.get(key) {
            self.current_page = progress.current_page.min(self.doc.page_count() - 1);
            self.scroll_y = 0;
        }
    }

    pub fn current_page(&self) -> usize {
        self.current_page
    }

    pub fn page_count(&self) -> usize {
        self.doc.page_count()
    }

    pub fn title(&self) -> String {
        self.doc.title()
    }

    pub fn save_progress(&self, store: &mut ProgressStore, key: &str) {
        store.update(key, self.current_page, self.doc.page_count());
    }

    /// Current `(scroll_y, max_scroll)` within the page, in reading
    /// orientation. `max_scroll` is 0 until the page has been rendered.
    pub fn scroll_state(&self) -> (u32, u32) {
        let (_, reading_h) = self.reading_dims();
        let max_scroll = match &self.rendered {
            Some((index, page)) if *index == self.current_page => {
                page.height.saturating_sub(reading_h)
            }
            _ => 0,
        };
        (self.scroll_y.min(max_scroll), max_scroll)
    }

    /// Screen dimensions in reading orientation: swapped for 90/270 so the
    /// fit computation happens against the rotated screen.
    fn reading_dims(&self) -> (u32, u32) {
        let (w, h) = (self.display.width(), self.display.height());
        if self.rotation % 180 == 90 {
            (h, w)
        } else {
            (w, h)
        }
    }

    /// How far one tap scrolls within an oversized page.
    fn scroll_step(&self) -> u32 {
        let (_, reading_h) = self.reading_dims();
        reading_h.saturating_sub(SCROLL_OVERLAP_PX).max(1)
    }

    /// Render the current page and push it to the display. The first paint
    /// and every [`FULL_REFRESH_INTERVAL`]th page turn use a full refresh.
    pub fn show_current_page(&mut self) -> Result<()> {
        let (reading_w, reading_h) = self.reading_dims();

        let cached = matches!(&self.rendered, Some((index, _)) if *index == self.current_page);
        if !cached {
            // Use the prefetched image when it's for this page; otherwise
            // decode synchronously.
            let image = match self.prefetcher.take(self.current_page) {
                Some(image) => image,
                None => self.doc.decode_page(self.current_page)?,
            };
            let opts = RenderOptions {
                screen_width: reading_w,
                screen_height: reading_h,
                fit: self.fit,
                dither: true,
            };
            self.rendered = Some((self.current_page, render_page(&image, &opts)));
            // Kick off decoding of the next page in the background.
            self.prefetcher.start(self.current_page + 1);
        }

        let page = &self.rendered.as_ref().expect("rendered above").1;
        let max_scroll = page.height.saturating_sub(reading_h);
        self.scroll_y = self.scroll_y.min(max_scroll);

        if self.rotation == 0 {
            // The display's blit handles vertical scrolling natively.
            self.display.blit(page, self.scroll_y)?;
        } else {
            // Cut the visible window out of the reading-orientation page,
            // then rotate it into the panel orientation.
            let window = crop_rows(page, self.scroll_y, reading_h);
            let rotated = rotate_page(&window, self.rotation);
            self.display.blit(&rotated, 0)?;
        }

        let mode = if self.turns_since_full_refresh == 0 {
            RefreshMode::Full
        } else {
            RefreshMode::Partial
        };
        self.display.flush(mode)?;
        Ok(())
    }

    /// Advance: scroll down within an oversized page first; turn to the
    /// next page only from the bottom. Returns `false` at the end of the
    /// document.
    pub fn next_page(&mut self) -> Result<bool> {
        let (scroll, max_scroll) = self.scroll_state();
        if scroll < max_scroll {
            self.scroll_y = (scroll + self.scroll_step()).min(max_scroll);
            self.bump_refresh_counter();
            self.show_current_page()?;
            return Ok(true);
        }
        if self.current_page + 1 >= self.doc.page_count() {
            return Ok(false);
        }
        self.current_page += 1;
        self.scroll_y = 0;
        self.bump_refresh_counter();
        self.show_current_page()?;
        Ok(true)
    }

    /// Go back: scroll up within an oversized page first; from the top,
    /// enter the previous page at its bottom. Returns `false` at the start
    /// of the document.
    pub fn prev_page(&mut self) -> Result<bool> {
        if self.scroll_y > 0 {
            self.scroll_y = self.scroll_y.saturating_sub(self.scroll_step());
            self.bump_refresh_counter();
            self.show_current_page()?;
            return Ok(true);
        }
        if self.current_page == 0 {
            return Ok(false);
        }
        self.current_page -= 1;
        // Enter the previous page at its bottom; show_current_page clamps
        // this to the page's actual max scroll once it is rendered.
        self.scroll_y = u32::MAX;
        self.bump_refresh_counter();
        self.show_current_page()?;
        Ok(true)
    }

    fn bump_refresh_counter(&mut self) {
        self.turns_since_full_refresh = (self.turns_since_full_refresh + 1) % FULL_REFRESH_INTERVAL;
    }
}

/// Normalize a rotation setting to 0/90/180/270 (anything else → 0).
fn normalize_rotation(degrees: u32) -> u32 {
    match degrees % 360 {
        d @ (90 | 180 | 270) => d,
        _ => 0,
    }
}

/// Copy `height` rows of `page` starting at `offset_y` (clamped).
fn crop_rows(page: &GrayPage, offset_y: u32, height: u32) -> GrayPage {
    let offset_y = offset_y.min(page.height.saturating_sub(1));
    let height = height.min(page.height - offset_y);
    let start = (offset_y * page.width) as usize;
    let end = start + (height * page.width) as usize;
    GrayPage {
        width: page.width,
        height,
        pixels: page.pixels[start..end].to_vec(),
    }
}

/// Decodes the upcoming page on a background thread so page turns don't
/// wait on the image decoder.
///
/// The prefetcher owns its own [`CbzDocument`] (an independent handle to
/// the same file) and moves it into each decode thread, taking it back
/// through the result channel. Without a document (e.g. `try_clone`
/// failed) every call degrades to a no-op and the reader decodes
/// synchronously.
struct Prefetcher {
    /// The idle document handle, ready to move into the next decode thread.
    doc: Option<CbzDocument>,
    pending: Option<Pending>,
}

struct Pending {
    index: usize,
    rx: mpsc::Receiver<(CbzDocument, Option<DynamicImage>)>,
    handle: JoinHandle<()>,
}

impl Prefetcher {
    fn new(doc: Option<CbzDocument>) -> Self {
        Self { doc, pending: None }
    }

    /// Start decoding `index` in the background. Any in-flight prefetch is
    /// drained first (its result is discarded). Out-of-range indices are
    /// ignored, so prefetching past the last page is a no-op.
    fn start(&mut self, index: usize) {
        self.reclaim();
        let Some(mut doc) = self.doc.take() else {
            return;
        };
        if index >= doc.page_count() {
            self.doc = Some(doc);
            return;
        }
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            // Decode errors aren't fatal here: the reader falls back to a
            // synchronous decode and reports the error from there.
            let image = doc.decode_page(index).ok();
            let _ = tx.send((doc, image));
        });
        self.pending = Some(Pending { index, rx, handle });
    }

    /// Take the prefetched image if it was decoded for exactly `index`.
    /// Returns `None` (caller decodes synchronously) when nothing is in
    /// flight, the prefetch was for another page, or decoding failed.
    fn take(&mut self, index: usize) -> Option<DynamicImage> {
        let wanted = self.pending.as_ref().is_some_and(|p| p.index == index);
        let image = self.reclaim();
        if wanted {
            image
        } else {
            None
        }
    }

    /// Wait for any in-flight decode, take the document handle back and
    /// return the decoded image (if any).
    fn reclaim(&mut self) -> Option<DynamicImage> {
        let pending = self.pending.take()?;
        let received = pending.rx.recv().ok();
        let _ = pending.handle.join();
        let (doc, image) = received?;
        self.doc = Some(doc);
        image
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gideon_device::MemoryDisplay;
    use std::io::Write;
    use std::path::Path;

    /// Write a CBZ whose page `i` is a `width x height(i)` solid image with
    /// brightness `i * 40`, so tests can tell pages apart by pixel value
    /// and by decoded dimensions.
    fn make_cbz_sized(path: &Path, dims: &[(u32, u32)]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (i, (w, h)) in dims.iter().enumerate() {
            let gray = (i * 40) as u8;
            let img = image::RgbImage::from_pixel(*w, *h, image::Rgb([gray, gray, gray]));
            let mut buf = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut buf, image::ImageFormat::Png)
                .unwrap();
            zip.start_file(
                format!("{:03}.png", i + 1),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(&buf.into_inner()).unwrap();
        }
        zip.finish().unwrap();
    }

    fn make_cbz(path: &Path, pages: usize) {
        make_cbz_sized(path, &vec![(8, 8); pages]);
    }

    fn open_doc(dir: &Path, pages: usize) -> CbzDocument {
        let path = dir.join("test.cbz");
        make_cbz(&path, pages);
        CbzDocument::open(&path).unwrap()
    }

    fn new_reader(dir: &Path, pages: usize) -> Reader<MemoryDisplay> {
        Reader::new(
            open_doc(dir, pages),
            MemoryDisplay::new(16, 16),
            FitMode::Contain,
            0,
        )
    }

    #[test]
    fn first_paint_uses_full_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = new_reader(dir.path(), 3);

        reader.show_current_page().unwrap();
        assert_eq!(reader.display().flushes, vec![RefreshMode::Full]);
    }

    #[test]
    fn page_turns_are_partial_until_interval() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = new_reader(dir.path(), 10);

        reader.show_current_page().unwrap();
        for _ in 0..6 {
            reader.next_page().unwrap();
        }

        // Paint 0: full. Turns 1..=5: partial. Turn 6 wraps the counter: full.
        let flushes = &reader.display().flushes;
        assert_eq!(flushes[0], RefreshMode::Full);
        assert!(flushes[1..6].iter().all(|m| *m == RefreshMode::Partial));
        assert_eq!(flushes[6], RefreshMode::Full);
    }

    #[test]
    fn navigation_clamps_at_both_ends() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = new_reader(dir.path(), 2);

        assert!(!reader.prev_page().unwrap());
        assert_eq!(reader.current_page(), 0);

        assert!(reader.next_page().unwrap());
        assert_eq!(reader.current_page(), 1);

        assert!(!reader.next_page().unwrap());
        assert_eq!(reader.current_page(), 1);

        assert!(reader.prev_page().unwrap());
        assert_eq!(reader.current_page(), 0);
    }

    #[test]
    fn progress_round_trips_through_store() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = new_reader(dir.path(), 5);

        reader.next_page().unwrap();
        reader.next_page().unwrap();

        let mut store = ProgressStore::default();
        reader.save_progress(&mut store, "test.cbz");
        assert_eq!(store.get("test.cbz").unwrap().current_page, 2);

        // A fresh reader resumes where we left off.
        let doc2 = CbzDocument::open(dir.path().join("test.cbz")).unwrap();
        let mut reader2 = Reader::new(doc2, MemoryDisplay::new(16, 16), FitMode::Contain, 0);
        reader2.resume_from(&store, "test.cbz");
        assert_eq!(reader2.current_page(), 2);
    }

    #[test]
    fn resume_clamps_to_document_length() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = new_reader(dir.path(), 2);

        let mut store = ProgressStore::default();
        store.update("test.cbz", 99, 100);
        reader.resume_from(&store, "test.cbz");
        assert_eq!(reader.current_page(), 1);
    }

    #[test]
    fn blit_actually_changes_displayed_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = new_reader(dir.path(), 2);

        reader.show_current_page().unwrap();
        let first = reader.display().buffer.clone();
        reader.next_page().unwrap();
        assert_ne!(
            first,
            reader.display().buffer,
            "page turn should repaint the screen"
        );
    }

    // --- FitWidth scrolling ---

    /// A reader on a 100x100 display with one 50x200 page: FitWidth scales
    /// it to 100x400, so max_scroll = 300 and the scroll step is 40
    /// (100 - 60 overlap).
    fn fit_width_reader(dir: &Path, pages: usize) -> Reader<MemoryDisplay> {
        let path = dir.join("tall.cbz");
        make_cbz_sized(&path, &vec![(50, 200); pages]);
        Reader::new(
            CbzDocument::open(&path).unwrap(),
            MemoryDisplay::new(100, 100),
            FitMode::FitWidth,
            0,
        )
    }

    #[test]
    fn fit_width_next_scrolls_before_turning() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = fit_width_reader(dir.path(), 2);
        reader.show_current_page().unwrap();
        assert_eq!(reader.scroll_state(), (0, 300));

        // Each tap scrolls down by screen_height - overlap = 40px.
        assert!(reader.next_page().unwrap());
        assert_eq!(reader.current_page(), 0);
        assert_eq!(reader.scroll_state(), (40, 300));

        for _ in 0..6 {
            assert!(reader.next_page().unwrap());
        }
        assert_eq!(reader.current_page(), 0);
        assert_eq!(reader.scroll_state(), (280, 300));

        // The last step is clamped to the page bottom, not past it.
        assert!(reader.next_page().unwrap());
        assert_eq!(reader.scroll_state(), (300, 300));
        assert_eq!(reader.current_page(), 0);

        // Only from the bottom does the next tap turn the page.
        assert!(reader.next_page().unwrap());
        assert_eq!(reader.current_page(), 1);
        assert_eq!(reader.scroll_state(), (0, 300));
    }

    #[test]
    fn fit_width_prev_scrolls_up_then_enters_previous_page_at_bottom() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = fit_width_reader(dir.path(), 2);
        reader.show_current_page().unwrap();

        // Scroll to the bottom of page 0, turn to page 1, scroll down once.
        while reader.scroll_state().0 < reader.scroll_state().1 {
            reader.next_page().unwrap();
        }
        reader.next_page().unwrap(); // page 1, scroll 0
        reader.next_page().unwrap(); // page 1, scroll 40
        assert_eq!(reader.current_page(), 1);
        assert_eq!(reader.scroll_state(), (40, 300));

        // Prev scrolls up within page 1 first…
        assert!(reader.prev_page().unwrap());
        assert_eq!(reader.current_page(), 1);
        assert_eq!(reader.scroll_state(), (0, 300));

        // …and from the top it enters page 0 at its bottom.
        assert!(reader.prev_page().unwrap());
        assert_eq!(reader.current_page(), 0);
        assert_eq!(reader.scroll_state(), (300, 300));
    }

    #[test]
    fn fit_width_scroll_changes_displayed_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gradient.cbz");
        // A page whose brightness increases with y, so different scroll
        // offsets show measurably different pixels.
        let mut img = image::RgbImage::new(50, 200);
        for (_x, y, px) in img.enumerate_pixels_mut() {
            let g = y as u8;
            *px = image::Rgb([g, g, g]);
        }
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("001.png", zip::write::SimpleFileOptions::default())
            .unwrap();
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        zip.write_all(&buf.into_inner()).unwrap();
        zip.finish().unwrap();

        let mut reader = Reader::new(
            CbzDocument::open(&path).unwrap(),
            MemoryDisplay::new(100, 100),
            FitMode::FitWidth,
            0,
        );
        reader.show_current_page().unwrap();
        let top_avg: f64 = reader.display().buffer.iter().map(|&p| p as f64).sum::<f64>()
            / reader.display().buffer.len() as f64;
        reader.next_page().unwrap();
        let scrolled_avg: f64 = reader.display().buffer.iter().map(|&p| p as f64).sum::<f64>()
            / reader.display().buffer.len() as f64;
        assert!(
            scrolled_avg > top_avg + 10.0,
            "scrolling down should show the brighter lower part \
             (top {top_avg:.1}, scrolled {scrolled_avg:.1})"
        );
    }

    #[test]
    fn contain_mode_has_no_scroll() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = new_reader(dir.path(), 2);
        reader.show_current_page().unwrap();
        assert_eq!(reader.scroll_state(), (0, 0));
        reader.next_page().unwrap();
        assert_eq!(reader.current_page(), 1);
    }

    // --- prefetching ---

    #[test]
    fn prefetcher_returns_the_right_image_for_the_right_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cbz");
        // Distinct dimensions per page so images are distinguishable.
        make_cbz_sized(&path, &[(8, 8), (10, 12), (14, 6)]);
        let mut doc = CbzDocument::open(&path).unwrap();
        let mut prefetcher = Prefetcher::new(doc.try_clone().ok());

        prefetcher.start(1);
        let image = prefetcher.take(1).expect("prefetched image for index 1");
        let direct = doc.decode_page(1).unwrap();
        assert_eq!((image.width(), image.height()), (10, 12));
        assert_eq!(image.into_luma8(), direct.into_luma8());

        // The prefetcher reclaimed its document and can go again.
        prefetcher.start(2);
        let image = prefetcher.take(2).expect("prefetched image for index 2");
        assert_eq!((image.width(), image.height()), (14, 6));
    }

    #[test]
    fn wrong_index_prefetch_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cbz");
        make_cbz_sized(&path, &[(8, 8), (10, 12), (14, 6)]);
        let doc = CbzDocument::open(&path).unwrap();
        let mut prefetcher = Prefetcher::new(doc.try_clone().ok());

        prefetcher.start(1);
        // The user went backwards: the prefetched page 1 is useless.
        assert!(prefetcher.take(0).is_none());
        // The document handle survived; the next prefetch still works.
        prefetcher.start(2);
        let image = prefetcher.take(2).expect("prefetcher still functional");
        assert_eq!((image.width(), image.height()), (14, 6));
    }

    #[test]
    fn take_without_prefetch_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let doc = open_doc(dir.path(), 2);
        let mut prefetcher = Prefetcher::new(doc.try_clone().ok());
        assert!(prefetcher.take(0).is_none());
        assert!(prefetcher.take(1).is_none());
    }

    #[test]
    fn prefetch_past_the_last_page_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let doc = open_doc(dir.path(), 2);
        let mut prefetcher = Prefetcher::new(doc.try_clone().ok());

        prefetcher.start(2); // out of range
        assert!(prefetcher.take(2).is_none());
        // Still usable afterwards.
        prefetcher.start(1);
        assert!(prefetcher.take(1).is_some());
    }

    #[test]
    fn prefetcher_without_document_degrades_to_sync() {
        let mut prefetcher = Prefetcher::new(None);
        prefetcher.start(1);
        assert!(prefetcher.take(1).is_none());
    }

    #[test]
    fn reader_at_last_page_does_not_panic_and_stays_put() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = new_reader(dir.path(), 2);
        reader.show_current_page().unwrap();
        reader.next_page().unwrap(); // last page; prefetch of page 2 is a no-op
        assert!(!reader.next_page().unwrap());
        assert_eq!(reader.current_page(), 1);
    }

    #[test]
    fn double_next_page_in_quick_succession_shows_the_right_pages() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = new_reader(dir.path(), 5);
        reader.show_current_page().unwrap();

        // Two page turns back to back: the second arrives while/just after
        // the prefetch of page 2 ran; both must show the correct page.
        reader.next_page().unwrap();
        reader.next_page().unwrap();
        assert_eq!(reader.current_page(), 2);

        // Page 2 is solid brightness 80; the screen center must match
        // (16x16 display, 8x8 page contained → scaled to fill).
        let center = reader.display().pixel(8, 8);
        assert!(
            center.abs_diff(80) <= 17,
            "expected page-2 brightness near 80, got {center}"
        );
    }

    #[test]
    fn prefetched_and_sync_decoded_pages_render_identically() {
        let dir = tempfile::tempdir().unwrap();

        // Reader A turns pages normally (uses the prefetched image).
        let mut a = new_reader(dir.path(), 3);
        a.show_current_page().unwrap();
        a.next_page().unwrap();

        // Reader B has no prefetcher (decodes synchronously).
        let doc = CbzDocument::open(dir.path().join("test.cbz")).unwrap();
        let mut b = Reader {
            prefetcher: Prefetcher::new(None),
            ..Reader::new(doc, MemoryDisplay::new(16, 16), FitMode::Contain, 0)
        };
        b.show_current_page().unwrap();
        b.next_page().unwrap();

        assert_eq!(a.display().buffer, b.display().buffer);
    }

    // --- rotation ---

    /// A CBZ with one page: left half black, right half white (in reading
    /// orientation), so rotations are observable on the panel.
    fn make_half_black_cbz(path: &Path) {
        let mut img = image::RgbImage::new(100, 100);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            let g = if x < 50 { 0x00 } else { 0xFF };
            *px = image::Rgb([g, g, g]);
        }
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("001.png", zip::write::SimpleFileOptions::default())
            .unwrap();
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        zip.write_all(&buf.into_inner()).unwrap();
        zip.finish().unwrap();
    }

    fn rotated_reader(dir: &Path, rotation: u32) -> Reader<MemoryDisplay> {
        let path = dir.join("half.cbz");
        make_half_black_cbz(&path);
        Reader::new(
            CbzDocument::open(&path).unwrap(),
            MemoryDisplay::new(100, 100),
            FitMode::Contain,
            rotation,
        )
    }

    #[test]
    fn rotation_0_shows_black_on_the_left() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = rotated_reader(dir.path(), 0);
        reader.show_current_page().unwrap();
        assert!(reader.display().pixel(10, 50) < 0x40);
        assert!(reader.display().pixel(90, 50) > 0xC0);
    }

    #[test]
    fn rotation_90_shows_black_on_the_top() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = rotated_reader(dir.path(), 90);
        reader.show_current_page().unwrap();
        // Clockwise: the reading-left (black) half lands at the panel top.
        assert!(reader.display().pixel(50, 10) < 0x40);
        assert!(reader.display().pixel(50, 90) > 0xC0);
    }

    #[test]
    fn rotation_180_shows_black_on_the_right() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = rotated_reader(dir.path(), 180);
        reader.show_current_page().unwrap();
        assert!(reader.display().pixel(90, 50) < 0x40);
        assert!(reader.display().pixel(10, 50) > 0xC0);
    }

    #[test]
    fn rotation_270_shows_black_on_the_bottom() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = rotated_reader(dir.path(), 270);
        reader.show_current_page().unwrap();
        assert!(reader.display().pixel(50, 90) < 0x40);
        assert!(reader.display().pixel(50, 10) > 0xC0);
    }

    #[test]
    fn rotation_90_fits_against_swapped_screen_dims() {
        // A 200x100 (wide) page on a 100x200 (portrait) panel rotated 90:
        // the fit happens against the 200x100 reading screen, where the
        // page fills it exactly — no letterboxing in reading orientation.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.cbz");
        make_cbz_sized(&path, &[(200, 100)]);
        let mut reader = Reader::new(
            CbzDocument::open(&path).unwrap(),
            MemoryDisplay::new(100, 200),
            FitMode::Contain,
            90,
        );
        reader.show_current_page().unwrap();
        // Page 0 brightness is 0 (black): the whole panel must be covered.
        let buffer = &reader.display().buffer;
        let dark = buffer.iter().filter(|&&p| p < 0x40).count();
        assert!(
            dark > buffer.len() * 9 / 10,
            "rotated page should fill the panel, {dark}/{} dark",
            buffer.len()
        );
    }

    #[test]
    fn fit_width_scrolling_works_rotated() {
        // Landscape reading (rotation 90) with FitWidth: reading width is
        // the panel height. A 50x200 page on a 100x100 panel scales to
        // 100x400 in reading orientation → same scroll math as unrotated.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tall.cbz");
        make_cbz_sized(&path, &[(50, 200), (50, 200)]);
        let mut reader = Reader::new(
            CbzDocument::open(&path).unwrap(),
            MemoryDisplay::new(100, 100),
            FitMode::FitWidth,
            90,
        );
        reader.show_current_page().unwrap();
        assert_eq!(reader.scroll_state(), (0, 300));
        assert!(reader.next_page().unwrap());
        assert_eq!(reader.scroll_state(), (40, 300));
        assert_eq!(reader.current_page(), 0);
    }

    #[test]
    fn invalid_rotation_is_treated_as_zero() {
        assert_eq!(normalize_rotation(0), 0);
        assert_eq!(normalize_rotation(90), 90);
        assert_eq!(normalize_rotation(180), 180);
        assert_eq!(normalize_rotation(270), 270);
        assert_eq!(normalize_rotation(360), 0);
        assert_eq!(normalize_rotation(450), 90);
        assert_eq!(normalize_rotation(45), 0);
    }

    #[test]
    fn crop_rows_extracts_the_window() {
        let page = GrayPage {
            width: 2,
            height: 4,
            pixels: vec![0, 0, 1, 1, 2, 2, 3, 3],
        };
        let window = crop_rows(&page, 1, 2);
        assert_eq!((window.width, window.height), (2, 2));
        assert_eq!(window.pixels, vec![1, 1, 2, 2]);
        // Clamped at the bottom.
        let tail = crop_rows(&page, 3, 5);
        assert_eq!(tail.pixels, vec![3, 3]);
    }
}
