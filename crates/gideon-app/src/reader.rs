//! Reader session: ties a [`CbzDocument`] to a [`Display`] and tracks the
//! current page, scroll position and reading progress.
//!
//! This is deliberately generic over the display so the whole page-turn /
//! refresh policy is unit-testable with `MemoryDisplay`.

use anyhow::Result;
use gideon_core::{CbzDocument, ProgressStore};
use gideon_device::{Display, RefreshMode};
use gideon_render::{render_page, FitMode, RenderOptions};

/// Do a full (flashing) e-ink refresh every N page turns to clear ghosting;
/// partial refreshes in between keep page turns fast.
const FULL_REFRESH_INTERVAL: u32 = 6;

pub struct Reader<D: Display> {
    doc: CbzDocument,
    display: D,
    fit: FitMode,
    current_page: usize,
    scroll_y: u32,
    turns_since_full_refresh: u32,
}

impl<D: Display> Reader<D> {
    pub fn new(doc: CbzDocument, display: D, fit: FitMode) -> Self {
        Self {
            doc,
            display,
            fit,
            current_page: 0,
            scroll_y: 0,
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

    /// Render the current page and push it to the display. The first paint
    /// and every [`FULL_REFRESH_INTERVAL`]th page turn use a full refresh.
    pub fn show_current_page(&mut self) -> Result<()> {
        let opts = RenderOptions {
            screen_width: self.display.width(),
            screen_height: self.display.height(),
            fit: self.fit,
            dither: true,
        };
        let image = self.doc.decode_page(self.current_page)?;
        let page = render_page(&image, &opts);
        self.display.blit(&page, self.scroll_y)?;

        let mode = if self.turns_since_full_refresh == 0 {
            RefreshMode::Full
        } else {
            RefreshMode::Partial
        };
        self.display.flush(mode)?;
        Ok(())
    }

    /// Advance to the next page. Returns `false` at the end of the document.
    pub fn next_page(&mut self) -> Result<bool> {
        if self.current_page + 1 >= self.doc.page_count() {
            return Ok(false);
        }
        self.current_page += 1;
        self.scroll_y = 0;
        self.bump_refresh_counter();
        self.show_current_page()?;
        Ok(true)
    }

    /// Go back one page. Returns `false` at the start of the document.
    pub fn prev_page(&mut self) -> Result<bool> {
        if self.current_page == 0 {
            return Ok(false);
        }
        self.current_page -= 1;
        self.scroll_y = 0;
        self.bump_refresh_counter();
        self.show_current_page()?;
        Ok(true)
    }

    fn bump_refresh_counter(&mut self) {
        self.turns_since_full_refresh = (self.turns_since_full_refresh + 1) % FULL_REFRESH_INTERVAL;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gideon_device::MemoryDisplay;
    use std::io::Write;
    use std::path::Path;

    fn make_cbz(path: &Path, pages: usize) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for i in 0..pages {
            // Page brightness varies per page so tests can tell them apart.
            let gray = (i * 40) as u8;
            let img = image::RgbImage::from_pixel(8, 8, image::Rgb([gray, gray, gray]));
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
        let mut reader2 = Reader::new(doc2, MemoryDisplay::new(16, 16), FitMode::Contain);
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
}
