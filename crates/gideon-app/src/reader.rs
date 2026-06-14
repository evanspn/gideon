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
//! * **Render-ahead** — while page N is on screen, a [`Prefetcher`] decodes
//!   *and fully renders* page N+1 (scale + dither) on a background thread
//!   (with its own [`CbzDocument`] handle), so a page turn is just a blit
//!   and a refresh. The previous page stays cached, so going back is
//!   equally instant.
//! * **Rotation** — pages are rendered against the *reading* orientation
//!   (screen dimensions swapped for 90/270) and the visible window is
//!   rotated into the panel orientation just before blitting.

use anyhow::Result;
use gideon_core::{CbzDocument, ProgressStore};
use gideon_device::{Display, RefreshMode};
use gideon_render::{render_page, FitMode, GrayPage, PageBuf, RenderOptions};
use std::sync::mpsc;
use std::thread::JoinHandle;

/// Do a full (flashing) e-ink refresh every N page turns to clear ghosting;
/// partial refreshes in between keep page turns fast. The full refresh is the
/// slow, black-flashing one, so a larger interval = fewer flashes = smoother
/// reading (at the cost of more ghosting between flashes). Manga line art
/// ghosts little, so the default leans toward smoothness; the reader setting
/// lets it be tuned further.
const DEFAULT_FULL_REFRESH_INTERVAL: u32 = 8;

/// Keep rendered pages cached only while each stays under this many
/// screenfuls of pixels — webtoon-length FitWidth strips would otherwise
/// pin tens of MB per cache slot on a 512MB device.
const CACHE_BUDGET_SCREENS: usize = 4;

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
    /// so scrolling never re-decodes. Gray for B/W manga, RGB when the
    /// page has real color (the Kaleido panel shows it).
    rendered: Option<(usize, PageBuf)>,
    /// The previously shown page, kept rendered so `prev_page` is as fast
    /// as `next_page`.
    spare: Option<(usize, PageBuf)>,
    prefetcher: Prefetcher,
    turns_since_full_refresh: u32,
    /// Page turns between full (flashing) refreshes; see
    /// [`DEFAULT_FULL_REFRESH_INTERVAL`]. Settable from the reader settings.
    full_refresh_interval: u32,
    /// Whether the most recent paint used a full (flashing) refresh. A full
    /// refresh is slow by design (~0.5s GC16 flash), so the slow-turn input
    /// debounce skips it — only decode-induced slowness should drop presses.
    last_refresh_full: bool,
    /// Direction of the last turn: render-ahead follows it so sustained
    /// *backward* paging is prefetched too, not just forward. Without this
    /// the spare slot covers only one page back and every further back-turn
    /// fell to a synchronous decode (going back felt slower than forward).
    forward: bool,
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
            spare: None,
            prefetcher,
            turns_since_full_refresh: 0,
            full_refresh_interval: DEFAULT_FULL_REFRESH_INTERVAL,
            last_refresh_full: false,
            forward: true,
        }
    }

    /// Whether the most recent [`Self::show_current_page`] used a full
    /// (flashing) refresh — the slow-turn debounce uses this to ignore the
    /// expected periodic flash.
    pub fn last_refresh_was_full(&self) -> bool {
        self.last_refresh_full
    }

    /// The page currently being rendered ahead, if any — for tests asserting
    /// that render-ahead follows the direction of travel.
    #[cfg(test)]
    fn prefetch_target(&self) -> Option<usize> {
        self.prefetcher.pending.as_ref().map(|p| p.index)
    }

    /// Set how many page turns happen between full (flashing) refreshes. A
    /// larger interval flashes less often (smoother) but lets ghosting build
    /// up longer; clamped to at least 1 so a full refresh still happens.
    pub fn set_full_refresh_interval(&mut self, interval: u32) {
        self.full_refresh_interval = interval.max(1);
    }

    /// The current reading rotation in degrees.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn rotation(&self) -> u32 {
        self.rotation
    }

    /// Change the reading rotation (0/90/180/270). Invalidates every
    /// rendered page — the fit is computed against the rotated screen —
    /// and forces the next paint to be a full refresh.
    pub fn set_rotation(&mut self, degrees: u32) {
        let rotation = normalize_rotation(degrees);
        if rotation == self.rotation {
            return;
        }
        self.rotation = rotation;
        self.rendered = None;
        self.spare = None;
        self.scroll_y = 0;
        self.turns_since_full_refresh = 0;
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
                page.height().saturating_sub(reading_h)
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

    /// Start rendering the current page on the prefetch thread, so the
    /// session's first paint takes a ready render instead of decoding on
    /// the calling thread. Call between [`Self::resume_from`] and the
    /// first [`Self::show_current_page`].
    pub fn warm(&mut self) {
        let (reading_w, reading_h) = self.reading_dims();
        let opts = RenderOptions {
            screen_width: reading_w,
            screen_height: reading_h,
            fit: self.fit,
            dither: true,
        };
        self.prefetcher.start(self.current_page, &opts);
    }

    /// Render the current page and push it to the display. The first paint
    /// and every [`DEFAULT_FULL_REFRESH_INTERVAL`]th page turn use a full
    /// refresh.
    pub fn show_current_page(&mut self) -> Result<()> {
        let (reading_w, reading_h) = self.reading_dims();

        let opts = RenderOptions {
            screen_width: reading_w,
            screen_height: reading_h,
            fit: self.fit,
            dither: true,
        };
        let cached = matches!(&self.rendered, Some((index, _)) if *index == self.current_page);
        if !cached {
            // Cheapest first: the spare slot (the page we just came from),
            // then the render-ahead result, then a synchronous decode.
            let spare_hit = matches!(&self.spare, Some((i, _)) if *i == self.current_page);
            let page = if spare_hit {
                self.spare.take().expect("matched above").1
            } else if let Some(page) = self.prefetcher.take(self.current_page, &opts) {
                page
            } else {
                // A single broken page (corrupt/truncated image, unsupported
                // codec) must never drop the reader: render a placeholder in
                // its place so the rest of the chapter stays readable.
                match self.doc.decode_page(self.current_page) {
                    Ok(image) => render_page(&image, &opts),
                    Err(err) => {
                        eprintln!(
                            "reader: page {} failed to load: {err:#}",
                            self.current_page + 1
                        );
                        render_error_page(reading_w, reading_h, self.current_page)
                    }
                }
            };
            // The outgoing page becomes the spare: going back is instant.
            self.spare = self.rendered.replace((self.current_page, page));
        }

        let page = &self.rendered.as_ref().expect("rendered above").1;
        let max_scroll = page.height().saturating_sub(reading_h);
        self.scroll_y = self.scroll_y.min(max_scroll);

        let indicator = self.page_indicator_text();
        if self.rotation == 0 {
            // Steady-state fast path: the display's blit handles vertical
            // scrolling natively, so the cached page goes straight to the
            // backbuffer with zero copies, and the page indicator is a
            // tiny (~few KB) box stamped onto the backbuffer afterwards —
            // never a clone of the page or its visible window. Color pages
            // dispatch to blit_rgb (Kaleido shows them in color, and the
            // following flush picks the color waveforms).
            blit_page(&mut self.display, page, self.scroll_y)?;
            let visible_w = page.width().min(reading_w);
            let visible_h = (page.height() - self.scroll_y).min(reading_h);
            if let Some(overlay) = render_page_indicator(&indicator, visible_w, visible_h) {
                // Bottom-right corner of the blitted (centered) window.
                let x = (reading_w - visible_w) / 2 + visible_w - overlay.width;
                let y = visible_h - overlay.height;
                self.display.overlay(&overlay, x, y)?;
            }
        } else {
            // Rotated reading copies anyway (crop + rotate into the panel
            // orientation), so the indicator is drawn into the window
            // before rotating — it follows the reading direction.
            let mut window = page.crop_rows(self.scroll_y, reading_h);
            draw_page_indicator_buf(&mut window, &indicator);
            let rotated = window.rotate(self.rotation);
            blit_page(&mut self.display, &rotated, 0)?;
        }

        let mode = if self.turns_since_full_refresh == 0 {
            RefreshMode::Full
        } else {
            RefreshMode::Partial
        };
        // Remember it so callers can tell an expected full-flash turn (slow
        // by design) from a turn that was slow because it had to decode —
        // only the latter should trigger the frustration-mash debounce.
        self.last_refresh_full = mode == RefreshMode::Full;
        self.display.flush(mode)?;

        // Cache policy AFTER the paint (never stall a blit on a stale
        // in-flight render): tall FitWidth pages can be enormous —
        // 1680xN webtoon strips — so cap what stays resident. Past the
        // budget, drop the spare and skip the render-ahead; neighbors of
        // a huge page are almost certainly huge too. The comparison is in
        // PIXELS, not bytes: an RGB page is 3x the bytes at equal pixels,
        // and a 1.33-screen color page must not be treated as "huge".
        let budget = (reading_w as usize) * (reading_h as usize) * CACHE_BUDGET_SCREENS;
        let huge = self
            .rendered
            .as_ref()
            .is_some_and(|(_, p)| p.pixel_count() > budget);
        if huge {
            self.spare = None;
        } else {
            // Render the page ahead in the direction of travel, fully (scale
            // + dither): the next turn that way is then just a blit + refresh.
            // Following the direction makes sustained back-paging as fast as
            // forward (the spare slot alone only covers a single page back).
            let ahead = if self.forward {
                let next = self.current_page + 1;
                (next < self.doc.page_count()).then_some(next)
            } else {
                self.current_page.checked_sub(1)
            };
            if let Some(index) = ahead {
                self.prefetcher.start(index, &opts);
            }
        }
        Ok(())
    }

    /// Repaint the current page with a transient banner overlaid along the
    /// top edge (e.g. "Brightness 70%"), without dirtying the page cache —
    /// the next page repaint wipes the banner away.
    pub fn show_banner(&mut self, text: &str) -> Result<()> {
        let cached = matches!(&self.rendered, Some((index, _)) if *index == self.current_page);
        if !cached {
            self.show_current_page()?;
        }
        let (_, reading_h) = self.reading_dims();
        let page = &self.rendered.as_ref().expect("rendered above").1;
        let offset = self.scroll_y.min(page.height().saturating_sub(1));
        let mut window = page.crop_rows(offset, reading_h);
        draw_banner_buf(&mut window, text);
        if self.rotation == 0 {
            blit_page(&mut self.display, &window, 0)?;
        } else {
            let rotated = window.rotate(self.rotation);
            blit_page(&mut self.display, &rotated, 0)?;
        }
        self.display.flush(RefreshMode::Partial)?;
        Ok(())
    }

    /// Stamp UI chrome (e.g. the reader-controls sheet) on top of the
    /// current backbuffer at panel coordinates and flush partially. The
    /// page cache stays untouched — the next page repaint wipes the
    /// overlay away, exactly like the banner and the page indicator.
    pub fn overlay_chrome(&mut self, chrome: &GrayPage, x: u32, y: u32) -> Result<()> {
        self.display.overlay(chrome, x, y)?;
        self.display.flush(RefreshMode::Partial)?;
        Ok(())
    }

    /// Repaint the current page with a guaranteed full refresh — for waking
    /// from suspend, when the panel contents can't be trusted.
    pub fn repaint_full(&mut self) -> Result<()> {
        self.turns_since_full_refresh = 0;
        self.show_current_page()
    }

    /// Advance: scroll down within an oversized page first; turn to the
    /// next page only from the bottom. Returns `false` at the end of the
    /// document.
    pub fn next_page(&mut self) -> Result<bool> {
        self.forward = true;
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
        self.forward = false;
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
        self.turns_since_full_refresh =
            (self.turns_since_full_refresh + 1) % self.full_refresh_interval.max(1);
    }

    /// The page-indicator label: "13/187", with the scroll position within
    /// a FitWidth page appended ("13/187 ·40%") so long strips show where
    /// the reader is.
    fn page_indicator_text(&self) -> String {
        let mut text = format!("{}/{}", self.current_page + 1, self.doc.page_count());
        if self.fit == FitMode::FitWidth {
            let (scroll, max_scroll) = self.scroll_state();
            if let Some(percent) = (scroll * 100).checked_div(max_scroll) {
                text.push_str(&format!(" ·{percent}%"));
            }
        }
        text
    }
}

/// Normalize a rotation setting to 0/90/180/270 (anything else → 0).
fn normalize_rotation(degrees: u32) -> u32 {
    match degrees % 360 {
        d @ (90 | 180 | 270) => d,
        _ => 0,
    }
}

/// A full-screen placeholder shown in place of a page that failed to decode
/// (corrupt/truncated image, unsupported codec), so a single bad page can't
/// drop the reader. A centered message on white; the user can simply turn
/// past it. Always grayscale — an error page has no color content.
fn render_error_page(width: u32, height: u32, index: usize) -> PageBuf {
    use gideon_render::text::{draw_text, measure_text};

    let mut page = GrayPage::new_white(width, height);
    let title = format!("Page {} couldn't be loaded", index + 1);
    let hint = "It may be corrupt — turn the page to continue.";

    let (title_px, hint_px) = (34.0_f32, 22.0_f32);
    let title_w = measure_text(title_px, &title, true).min(width);
    let hint_w = measure_text(hint_px, hint, false).min(width);
    let title_y = height / 2;
    draw_text(
        &mut page,
        (width.saturating_sub(title_w)) / 2,
        title_y,
        title_px,
        &title,
        width,
        true,
    );
    draw_text(
        &mut page,
        (width.saturating_sub(hint_w)) / 2,
        title_y + title_px as u32 + 12,
        hint_px,
        hint,
        width,
        false,
    );
    PageBuf::Gray(page)
}

/// Blit a rendered page of either depth onto the display: gray pages take
/// [`Display::blit`] (the fast path B/W manga always stays on), color
/// pages [`Display::blit_rgb`] — Kaleido panels show the color, every
/// other backend collapses it to luma in the trait's default impl.
fn blit_page<D: Display>(display: &mut D, page: &PageBuf, offset_y: u32) -> Result<()> {
    match page {
        PageBuf::Gray(page) => display.blit(page, offset_y)?,
        PageBuf::Rgb(page) => display.blit_rgb(page, offset_y)?,
    }
    Ok(())
}

/// Draw a white banner strip with `text` along the top of `page`.
fn draw_banner(page: &mut GrayPage, text: &str) {
    use gideon_render::text::draw_text;

    let banner_h = 56.min(page.height);
    for y in 0..banner_h {
        let row = (y * page.width) as usize;
        let value = if y + 1 == banner_h { 0x00 } else { 0xFF };
        page.pixels[row..row + page.width as usize].fill(value);
    }
    draw_text(
        page,
        16,
        12,
        30.0,
        text,
        page.width.saturating_sub(32),
        true,
    );
}

/// [`draw_banner`] for a window of either depth. The text rasterizer is
/// grayscale-only, so for RGB windows the banner is drawn into a gray
/// strip and replicated channel-wise on top — convert the box, never the
/// page.
fn draw_banner_buf(page: &mut PageBuf, text: &str) {
    match page {
        PageBuf::Gray(page) => draw_banner(page, text),
        PageBuf::Rgb(page) => {
            let mut strip = GrayPage::new_white(page.width, 56.min(page.height));
            draw_banner(&mut strip, text);
            stamp_gray_onto_rgb(page, &strip, 0, 0);
        }
    }
}

/// Copy a small gray box on top of an RGB page at (`x0`, `y0`) by
/// replicating its channels. Callers guarantee the box fits.
fn stamp_gray_onto_rgb(page: &mut gideon_render::RgbPage, boxed: &GrayPage, x0: u32, y0: u32) {
    for y in 0..boxed.height {
        for x in 0..boxed.width {
            let g = boxed.pixel(x, y);
            let dst = (((y0 + y) * page.width + x0 + x) * 3) as usize;
            page.pixels[dst..dst + 3].copy_from_slice(&[g, g, g]);
        }
    }
}

/// Render the compact page-indicator box ("13/187") as its own tiny page
/// (~52x40 px, a few KB): a white box with a 1px dark edge on its top and
/// left sides and ~24px text. Returns `None` when the visible window is
/// too small for unobtrusive chrome (tiny test displays), matching
/// [`draw_page_indicator`]'s old skip rule.
fn render_page_indicator(text: &str, avail_w: u32, avail_h: u32) -> Option<GrayPage> {
    use gideon_render::text::{draw_text, measure_text};

    const TEXT_PX: f32 = 24.0;
    const PAD: u32 = 8;
    let box_w = measure_text(TEXT_PX, text, false) + 2 * PAD;
    let box_h = TEXT_PX as u32 + 2 * PAD;
    if avail_w < box_w * 3 || avail_h < box_h * 3 {
        return None;
    }
    let mut overlay = GrayPage::new_white(box_w, box_h);
    overlay.pixels[..box_w as usize].fill(0x00); // top edge
    for y in 1..box_h {
        overlay.pixels[(y * box_w) as usize] = 0x00; // left edge
    }
    draw_text(&mut overlay, PAD, PAD, TEXT_PX, text, box_w - PAD, false);
    Some(overlay)
}

/// Draw a compact page-indicator box ("13/187") in the bottom-right corner
/// of the visible window — used on the rotated paint path, where the
/// window is a copy anyway. Skipped when the window is too small for
/// unobtrusive chrome (tiny test displays).
fn draw_page_indicator(page: &mut GrayPage, text: &str) {
    let Some(overlay) = render_page_indicator(text, page.width, page.height) else {
        return;
    };
    let x0 = page.width - overlay.width;
    let y0 = page.height - overlay.height;
    for y in 0..overlay.height {
        let src = (y * overlay.width) as usize;
        let dst = ((y0 + y) * page.width + x0) as usize;
        page.pixels[dst..dst + overlay.width as usize]
            .copy_from_slice(&overlay.pixels[src..src + overlay.width as usize]);
    }
}

/// [`draw_page_indicator`] for a window of either depth: the same gray
/// box, replicated channel-wise when the window is RGB.
fn draw_page_indicator_buf(page: &mut PageBuf, text: &str) {
    match page {
        PageBuf::Gray(page) => draw_page_indicator(page, text),
        PageBuf::Rgb(page) => {
            let Some(overlay) = render_page_indicator(text, page.width, page.height) else {
                return;
            };
            let x0 = page.width - overlay.width;
            let y0 = page.height - overlay.height;
            stamp_gray_onto_rgb(page, &overlay, x0, y0);
        }
    }
}

/// Decodes *and renders* the upcoming page on a background thread so a
/// page turn is just a blit + refresh — the decode, scale and dither all
/// happened while the user was still reading the previous page.
///
/// The prefetcher owns its own [`CbzDocument`] (an independent handle to
/// the same file) and moves it into each render thread, taking it back
/// through the result channel. Results are keyed by the render options:
/// a rotation or fit change invalidates an in-flight prefetch. Without a
/// document (e.g. `try_clone` failed) every call degrades to a no-op and
/// the reader renders synchronously.
struct Prefetcher {
    /// The idle document handle, ready to move into the next render thread.
    doc: Option<CbzDocument>,
    pending: Option<Pending>,
}

struct Pending {
    index: usize,
    opts: RenderOptions,
    rx: mpsc::Receiver<(CbzDocument, Option<PageBuf>)>,
    handle: JoinHandle<()>,
}

impl Prefetcher {
    fn new(doc: Option<CbzDocument>) -> Self {
        Self { doc, pending: None }
    }

    /// Start rendering `index` with `opts` in the background. Any in-flight
    /// prefetch is drained first (its result is discarded unless it already
    /// matches). Out-of-range indices are ignored, so prefetching past the
    /// last page is a no-op.
    fn start(&mut self, index: usize, opts: &RenderOptions) {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.index == index && p.opts == *opts)
        {
            return; // already in flight for exactly this page
        }
        self.reclaim();
        let Some(mut doc) = self.doc.take() else {
            return;
        };
        if index >= doc.page_count() {
            self.doc = Some(doc);
            return;
        }
        let (tx, rx) = mpsc::channel();
        let thread_opts = *opts;
        let handle = std::thread::spawn(move || {
            // Errors aren't fatal here: the reader falls back to a
            // synchronous render and reports the error from there.
            let page = doc
                .decode_page(index)
                .ok()
                .map(|image| render_page(&image, &thread_opts));
            let _ = tx.send((doc, page));
        });
        self.pending = Some(Pending {
            index,
            opts: *opts,
            rx,
            handle,
        });
    }

    /// Take the prefetched page if it was rendered for exactly `index`
    /// with exactly `opts`. Returns `None` (caller renders synchronously)
    /// when nothing matching is in flight or rendering failed.
    fn take(&mut self, index: usize, opts: &RenderOptions) -> Option<PageBuf> {
        let wanted = self
            .pending
            .as_ref()
            .is_some_and(|p| p.index == index && p.opts == *opts);
        let page = self.reclaim();
        if wanted {
            page
        } else {
            None
        }
    }

    /// Wait for any in-flight render, take the document handle back and
    /// return the rendered page (if any).
    fn reclaim(&mut self) -> Option<PageBuf> {
        let pending = self.pending.take()?;
        let received = pending.rx.recv().ok();
        let _ = pending.handle.join();
        let (doc, page) = received?;
        self.doc = Some(doc);
        page
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
    fn render_ahead_follows_the_direction_of_travel() {
        // Forward reading prefetches the next page; backward reading must
        // prefetch the PREVIOUS page, so a sustained back-run is as fast as
        // forward instead of falling to a synchronous decode every turn.
        let dir = tempfile::tempdir().unwrap();
        let mut reader = new_reader(dir.path(), 6);

        reader.show_current_page().unwrap(); // page 0
        assert_eq!(
            reader.prefetch_target(),
            Some(1),
            "forward: render-ahead is the next page"
        );
        reader.next_page().unwrap(); // page 1
        reader.next_page().unwrap(); // page 2
        assert_eq!(reader.prefetch_target(), Some(3));

        reader.prev_page().unwrap(); // page 1, now travelling backward
        assert_eq!(
            reader.prefetch_target(),
            Some(0),
            "backward: render-ahead is the previous page"
        );
    }

    #[test]
    fn a_corrupt_page_renders_a_placeholder_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.cbz");
        // Page 0 and 2 are valid; page 1 is garbage (won't decode).
        let valid = {
            let img = image::RgbImage::from_pixel(8, 8, image::Rgb([10, 10, 10]));
            let mut buf = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut buf, image::ImageFormat::Png)
                .unwrap();
            buf.into_inner()
        };
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("001.png", opts).unwrap();
            zip.write_all(&valid).unwrap();
            zip.start_file("002.png", opts).unwrap();
            zip.write_all(b"this is not a valid image file").unwrap();
            zip.start_file("003.png", opts).unwrap();
            zip.write_all(&valid).unwrap();
            zip.finish().unwrap();
        }
        let doc = CbzDocument::open(&path).unwrap();
        let mut reader = Reader::new(doc, MemoryDisplay::new(16, 16), FitMode::Contain, 0);

        // The good first page paints fine.
        assert!(reader.show_current_page().is_ok());
        // Turning onto the corrupt page must NOT drop the reader — it shows
        // a placeholder and stays on that page.
        assert!(
            reader.next_page().is_ok(),
            "a corrupt page must render a placeholder, not error out"
        );
        assert_eq!(reader.current_page(), 1);
        // And the reader keeps working past it.
        assert!(reader.next_page().is_ok());
        assert_eq!(reader.current_page(), 2);
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
        // Pin a known interval so the test doesn't track the default.
        reader.set_full_refresh_interval(6);

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
    fn full_refresh_interval_is_configurable() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = new_reader(dir.path(), 20);
        // A larger interval flashes less often: with 10, the first full
        // refresh after the initial paint lands on turn 10, not before.
        reader.set_full_refresh_interval(10);
        reader.show_current_page().unwrap();
        for _ in 0..10 {
            reader.next_page().unwrap();
        }
        let flushes = &reader.display().flushes;
        assert_eq!(flushes[0], RefreshMode::Full);
        assert!(
            flushes[1..10].iter().all(|m| *m == RefreshMode::Partial),
            "turns 1..=9 stay partial at interval 10"
        );
        assert_eq!(flushes[10], RefreshMode::Full, "turn 10 wraps to full");
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
        let top_avg: f64 = reader
            .display()
            .buffer
            .iter()
            .map(|&p| p as f64)
            .sum::<f64>()
            / reader.display().buffer.len() as f64;
        reader.next_page().unwrap();
        let scrolled_avg: f64 = reader
            .display()
            .buffer
            .iter()
            .map(|&p| p as f64)
            .sum::<f64>()
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

    fn opts(w: u32, h: u32) -> RenderOptions {
        RenderOptions {
            screen_width: w,
            screen_height: h,
            fit: FitMode::Contain,
            dither: true,
        }
    }

    #[test]
    fn prefetcher_returns_the_rendered_page_for_the_right_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cbz");
        // Distinct dimensions per page so pages are distinguishable.
        make_cbz_sized(&path, &[(8, 8), (10, 12), (14, 6)]);
        let mut doc = CbzDocument::open(&path).unwrap();
        let mut prefetcher = Prefetcher::new(doc.try_clone().ok());
        let o = opts(16, 16);

        prefetcher.start(1, &o);
        let page = prefetcher.take(1, &o).expect("rendered page for index 1");
        // The background render matches a synchronous one exactly.
        let direct = render_page(&doc.decode_page(1).unwrap(), &o);
        assert_eq!(page, direct);

        // The prefetcher reclaimed its document and can go again.
        prefetcher.start(2, &o);
        assert!(prefetcher.take(2, &o).is_some());
    }

    #[test]
    fn wrong_index_prefetch_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cbz");
        make_cbz_sized(&path, &[(8, 8), (10, 12), (14, 6)]);
        let doc = CbzDocument::open(&path).unwrap();
        let mut prefetcher = Prefetcher::new(doc.try_clone().ok());
        let o = opts(16, 16);

        prefetcher.start(1, &o);
        // The user went backwards: the prefetched page 1 is useless.
        assert!(prefetcher.take(0, &o).is_none());
        // The document handle survived; the next prefetch still works.
        prefetcher.start(2, &o);
        assert!(prefetcher.take(2, &o).is_some());
    }

    #[test]
    fn changed_render_options_invalidate_the_prefetch() {
        // A rotation (or fit) change between prefetch and take means the
        // in-flight page was rendered for the wrong screen: discard it.
        let dir = tempfile::tempdir().unwrap();
        let doc = open_doc(dir.path(), 3);
        let mut prefetcher = Prefetcher::new(doc.try_clone().ok());

        prefetcher.start(1, &opts(16, 16));
        assert!(
            prefetcher.take(1, &opts(32, 16)).is_none(),
            "stale options must not be served"
        );
        // Still functional with the new options afterwards.
        prefetcher.start(1, &opts(32, 16));
        let page = prefetcher.take(1, &opts(32, 16)).expect("fresh render");
        assert_eq!((page.width(), page.height()), (32, 16));
    }

    #[test]
    fn take_without_prefetch_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let doc = open_doc(dir.path(), 2);
        let mut prefetcher = Prefetcher::new(doc.try_clone().ok());
        let o = opts(16, 16);
        assert!(prefetcher.take(0, &o).is_none());
        assert!(prefetcher.take(1, &o).is_none());
    }

    #[test]
    fn prefetch_past_the_last_page_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let doc = open_doc(dir.path(), 2);
        let mut prefetcher = Prefetcher::new(doc.try_clone().ok());
        let o = opts(16, 16);

        prefetcher.start(2, &o); // out of range
        assert!(prefetcher.take(2, &o).is_none());
        // Still usable afterwards.
        prefetcher.start(1, &o);
        assert!(prefetcher.take(1, &o).is_some());
    }

    #[test]
    fn prefetcher_without_document_degrades_to_sync() {
        let mut prefetcher = Prefetcher::new(None);
        let o = opts(16, 16);
        prefetcher.start(1, &o);
        assert!(prefetcher.take(1, &o).is_none());
    }

    #[test]
    fn going_back_uses_the_spare_page() {
        // After 0 -> 1, page 0 sits in the spare slot; 1 -> 0 must reuse it
        // (and render identically to a fresh paint).
        let dir = tempfile::tempdir().unwrap();
        let mut reader = new_reader(dir.path(), 3);
        reader.show_current_page().unwrap();
        let first_paint = reader.display().buffer.clone();

        reader.next_page().unwrap();
        assert!(
            matches!(&reader.spare, Some((0, _))),
            "page 0 kept rendered"
        );
        reader.prev_page().unwrap();
        assert_eq!(reader.display().buffer, first_paint);
        assert!(
            matches!(&reader.spare, Some((1, _))),
            "page 1 becomes the spare in turn"
        );
    }

    #[test]
    fn set_rotation_rerenders_against_the_rotated_screen() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = rotated_reader(dir.path(), 0);
        reader.show_current_page().unwrap();
        assert!(reader.display().pixel(10, 50) < 0x40, "black left at 0°");

        reader.set_rotation(90);
        assert_eq!(reader.rotation(), 90);
        reader.show_current_page().unwrap();
        // Clockwise: the reading-left (black) half lands at the panel top,
        // and the post-rotation paint is a full refresh.
        assert!(reader.display().pixel(50, 10) < 0x40);
        assert!(reader.display().pixel(50, 90) > 0xC0);
        assert_eq!(
            reader.display().flushes.last(),
            Some(&RefreshMode::Full),
            "rotation repaint must flash clean"
        );
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

    // --- page indicator (the banner's compact corner sibling) ---

    #[test]
    fn page_indicator_text_shows_page_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = new_reader(dir.path(), 3);
        reader.show_current_page().unwrap();
        assert_eq!(reader.page_indicator_text(), "1/3");
        reader.next_page().unwrap();
        assert_eq!(reader.page_indicator_text(), "2/3");
    }

    #[test]
    fn page_indicator_includes_scroll_percent_in_fit_width() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = fit_width_reader(dir.path(), 2);
        reader.show_current_page().unwrap();
        assert_eq!(reader.page_indicator_text(), "1/2 ·0%");
        // One tap scrolls 40 of 300: 13%.
        reader.next_page().unwrap();
        assert_eq!(reader.page_indicator_text(), "1/2 ·13%");
    }

    #[test]
    fn page_indicator_is_drawn_in_the_bottom_right_corner() {
        // A black page on a large display: the indicator box puts white
        // pixels in the corner where the page is otherwise black.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("black.cbz");
        make_cbz_sized(&path, &[(50, 50), (50, 50)]);
        let mut reader = Reader::new(
            CbzDocument::open(&path).unwrap(),
            MemoryDisplay::new(300, 300),
            FitMode::Contain,
            0,
        );
        reader.show_current_page().unwrap();
        assert!(
            reader.display().pixel(295, 295) > 0xC0,
            "corner box should be white over the black page"
        );
        assert!(
            reader.display().pixel(150, 150) < 0x40,
            "the page itself stays black"
        );
    }

    #[test]
    fn page_indicator_is_skipped_on_tiny_windows() {
        // 16x16 test displays have no room for chrome: the indicator must
        // not paint over the page (existing pixel assertions rely on it).
        let mut page = GrayPage {
            width: 16,
            height: 16,
            pixels: vec![0x00; 256],
        };
        draw_page_indicator(&mut page, "1/2");
        assert!(page.pixels.iter().all(|&p| p == 0x00));
    }

    #[test]
    fn page_indicator_does_not_dirty_the_cached_page() {
        // The rotation-0 fast path blits the cached page directly and
        // stamps the indicator onto the display backbuffer only: the
        // cached render must stay pristine (no baked-in stale "1/2").
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("black.cbz");
        make_cbz_sized(&path, &[(50, 50), (50, 50)]);
        let mut reader = Reader::new(
            CbzDocument::open(&path).unwrap(),
            MemoryDisplay::new(300, 300),
            FitMode::Contain,
            0,
        );
        reader.show_current_page().unwrap();
        assert!(reader.display().pixel(295, 295) > 0xC0, "indicator shown");
        let (_, page) = reader.rendered.as_ref().expect("page cached");
        let PageBuf::Gray(page) = page else {
            panic!("B/W page must stay on the gray path");
        };
        let corner = page.pixel(page.width - 5, page.height - 5);
        assert!(
            corner < 0x40,
            "cached page corner must stay black (got {corner:#x}): the \
             indicator may only live on the display backbuffer"
        );
    }

    // --- color pages (the Kaleido RGB path) ---

    /// Write a CBZ whose page `i` is a `width x height(i)` solid COLOR
    /// image — strong red, well past the color-detection thresholds.
    fn make_color_cbz_sized(path: &Path, dims: &[(u32, u32)]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (i, (w, h)) in dims.iter().enumerate() {
            let img = image::RgbImage::from_pixel(*w, *h, image::Rgb([200, 30, 30]));
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

    #[test]
    fn bw_cbz_never_takes_the_rgb_path() {
        // THE fast-path invariant: grayscale manga must never be promoted
        // to RGB blits (3x the bytes, color waveforms, slower refreshes).
        let dir = tempfile::tempdir().unwrap();
        let mut reader = new_reader(dir.path(), 3);
        reader.show_current_page().unwrap();
        reader.next_page().unwrap();
        reader.prev_page().unwrap();
        assert!(
            !reader.display().blits.is_empty()
                && reader.display().blits.iter().all(|&color| !color),
            "B/W pages must stay on the gray blit path: {:?}",
            reader.display().blits
        );
    }

    #[test]
    fn color_cbz_page_blits_color() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("color.cbz");
        make_color_cbz_sized(&path, &[(8, 8), (8, 8)]);
        let mut reader = Reader::new(
            CbzDocument::open(&path).unwrap(),
            MemoryDisplay::new(16, 16),
            FitMode::Contain,
            0,
        );
        reader.show_current_page().unwrap();
        assert_eq!(reader.display().blits, vec![true]);
        // And the shown pixels are the page's Rec.601 luma, not white.
        let center = reader.display().pixel(8, 8);
        let expected = gideon_render::luma_rec601(200, 30, 30);
        assert!(
            center.abs_diff(expected) <= 2,
            "expected ~{expected}, got {center}"
        );
    }

    #[test]
    fn color_pages_round_trip_rgb_through_resume_spare_and_prefetch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("color.cbz");
        make_color_cbz_sized(&path, &[(8, 8), (8, 8), (8, 8)]);

        // Resume into the middle of the chapter: the warmed first paint
        // (prefetcher render) must still be color.
        let mut store = ProgressStore::default();
        store.update("color.cbz", 1, 3);
        let mut reader = Reader::new(
            CbzDocument::open(&path).unwrap(),
            MemoryDisplay::new(16, 16),
            FitMode::Contain,
            0,
        );
        reader.resume_from(&store, "color.cbz");
        reader.warm();
        reader.show_current_page().unwrap(); // page 1: prefetched render
        reader.next_page().unwrap(); // page 2: render-ahead result
        assert!(
            matches!(&reader.spare, Some((1, page)) if page.is_color()),
            "the spare slot must keep the RGB render"
        );
        reader.prev_page().unwrap(); // page 1 again: spare-slot hit
        assert_eq!(
            reader.display().blits,
            vec![true, true, true],
            "resume, prefetch and spare paints must ALL stay color"
        );
    }

    #[test]
    fn rotation_90_color_page_keeps_orientation_and_color() {
        // The RGB twin of rotation_90_shows_black_on_the_top: left half
        // RED, right half white in reading orientation. Rotated 90° CW the
        // red half lands at the panel top — and the blit carries color.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half_red.cbz");
        let mut img = image::RgbImage::new(100, 100);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = if x < 50 {
                image::Rgb([200, 30, 30])
            } else {
                image::Rgb([255, 255, 255])
            };
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
            FitMode::Contain,
            90,
        );
        reader.show_current_page().unwrap();
        assert_eq!(reader.display().blits, vec![true], "rotated blit is RGB");
        // Red luma is ~76: the reading-left (red) half is at the panel
        // top, the white half at the bottom.
        let red_luma = gideon_render::luma_rec601(200, 30, 30);
        assert!(reader.display().pixel(50, 10).abs_diff(red_luma) <= 8);
        assert!(reader.display().pixel(50, 90) > 0xC0);
    }

    #[test]
    fn fit_width_scrolling_works_on_color_pages() {
        // A color page whose RED brightens with y: scrolling down must
        // show measurably brighter pixels, all through RGB blits.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gradient_color.cbz");
        let mut img = image::RgbImage::new(50, 200);
        for (_x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([y as u8, 0, 0]);
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
        assert_eq!(reader.scroll_state(), (0, 300));
        let top_avg: f64 = reader
            .display()
            .buffer
            .iter()
            .map(|&p| p as f64)
            .sum::<f64>()
            / reader.display().buffer.len() as f64;
        reader.next_page().unwrap();
        assert_eq!(reader.scroll_state(), (40, 300));
        let scrolled_avg: f64 = reader
            .display()
            .buffer
            .iter()
            .map(|&p| p as f64)
            .sum::<f64>()
            / reader.display().buffer.len() as f64;
        assert!(
            scrolled_avg > top_avg + 3.0,
            "scrolling down should show the brighter lower part \
             (top {top_avg:.1}, scrolled {scrolled_avg:.1})"
        );
        assert_eq!(reader.display().blits, vec![true, true]);
    }

    #[test]
    fn rgb_cache_budget_counts_pixels_not_bytes() {
        // A 50x200 color page FitWidth on 100x100 renders to 100x400 —
        // exactly CACHE_BUDGET_SCREENS (4) screenfuls of PIXELS, i.e. not
        // "huge". Counting its 120000 BYTES against the 40000 budget would
        // wrongly drop the spare and kill the render-ahead for every color
        // page above a third of the budget.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tall_color.cbz");
        make_color_cbz_sized(&path, &[(50, 200), (50, 200)]);
        let mut reader = Reader::new(
            CbzDocument::open(&path).unwrap(),
            MemoryDisplay::new(100, 100),
            FitMode::FitWidth,
            0,
        );
        reader.show_current_page().unwrap();
        while reader.scroll_state().0 < reader.scroll_state().1 {
            reader.next_page().unwrap();
        }
        reader.next_page().unwrap(); // onto page 1
        assert_eq!(reader.current_page(), 1);
        assert!(
            matches!(&reader.spare, Some((0, page)) if page.is_color()),
            "a 4-screen color page is within the pixel budget: spare kept"
        );
    }

    #[test]
    fn banner_on_a_color_page_keeps_the_color_blit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("color.cbz");
        make_color_cbz_sized(&path, &[(100, 100)]);
        let mut reader = Reader::new(
            CbzDocument::open(&path).unwrap(),
            MemoryDisplay::new(100, 100),
            FitMode::Contain,
            0,
        );
        reader.show_current_page().unwrap();
        reader.show_banner("Brightness 70%").unwrap();
        assert_eq!(reader.display().blits, vec![true, true]);
        // The banner strip itself is white chrome on top…
        assert!(reader.display().pixel(50, 5) > 0xC0);
        // …and the page below it is still the red page's luma.
        let red_luma = gideon_render::luma_rec601(200, 30, 30);
        assert!(reader.display().pixel(50, 80).abs_diff(red_luma) <= 8);
        // The cached page is untouched by the banner (still pristine RGB).
        let (_, page) = reader.rendered.as_ref().expect("page cached");
        assert!(page.is_color());
        let PageBuf::Rgb(rgb) = page else {
            unreachable!()
        };
        assert_eq!(rgb.pixel(50, 5), [200, 30, 30], "banner never bakes in");
    }

    #[test]
    fn page_indicator_overlay_box_matches_the_skip_rule() {
        // Too-small windows produce no box at all…
        assert!(render_page_indicator("1/2", 16, 16).is_none());
        // …large ones get a small white box with dark top/left edges.
        let overlay = render_page_indicator("1/2", 300, 300).expect("box");
        assert!(overlay.width < 100 && overlay.height < 50, "stays tiny");
        assert_eq!(overlay.pixel(overlay.width / 2, 0), 0x00, "top edge");
        assert_eq!(overlay.pixel(0, overlay.height / 2), 0x00, "left edge");
        assert!(
            overlay.pixel(overlay.width - 2, overlay.height - 2) > 0xC0,
            "white interior"
        );
    }
}
