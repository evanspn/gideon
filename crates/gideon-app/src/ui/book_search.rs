//! Discover › Books › Internet Archive: a catalogue that is search and
//! nothing else, drawn the way the design review approved it (gallery design
//! `internet-archive-search`):
//!
//! 1. the catalogue opens straight onto the keyboard, with the last few
//!    searches as one-tap chips under the field;
//! 2. results are book rows — a spine block, the title over at most two
//!    lines, author · year, and an EPUB / ON KOBO chip — paged from a footer;
//! 3. a tap raises a sheet with what archive.org knows about the book, and
//!    nothing downloads until Download is pressed; then "Read now".
//!
//! Refresh, per docs/REFRESH.md: opening a screen or flipping a results page
//! is Full (the whole content area changes); keystrokes are Partial with the
//! keyboard's own periodic Full; the sheet opens and changes state with
//! Partial (only its strip changes) and closes with Full (it reveals what it
//! covered).

use super::*;

/// How many recent searches the search screen offers.
pub(super) const RECENT_MAX: usize = 3;

/// The recent searches, newest first, beside the sync cache.
fn recent_path(library_dir: &Path) -> PathBuf {
    library_dir.join(".gideon").join("book_searches.json")
}

/// The recent Internet Archive searches, newest first. A missing or
/// unreadable file is simply no history.
pub(super) fn load_recent_searches(library_dir: &Path) -> Vec<String> {
    std::fs::read(recent_path(library_dir))
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|q| !q.trim().is_empty())
        .take(RECENT_MAX)
        .collect()
}

/// Put `query` at the front of the recent searches (once, whatever its
/// case). Best effort: failing to remember a search is not worth an error.
pub(super) fn remember_search(library_dir: &Path, query: &str) {
    let query = query.trim();
    if query.is_empty() {
        return;
    }
    let mut recent = load_recent_searches(library_dir);
    recent.retain(|q| !q.eq_ignore_ascii_case(query));
    recent.insert(0, query.to_string());
    recent.truncate(RECENT_MAX);
    let path = recent_path(library_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec(&recent) {
        let _ = std::fs::write(path, json);
    }
}

/// Where a book sheet is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DownloadState {
    /// Asking archive.org which EPUB the book has.
    Checking,
    /// Known, not downloaded: Cancel / Download.
    Ready,
    Downloading,
    /// On the Kobo at this path: Back to results / Read now.
    Saved(PathBuf),
    /// Something went wrong, in words: Close / Try again.
    Failed(String),
}

/// A rectangle: `(x, y, w, h)`.
type Rect = (u32, u32, u32, u32);

fn hit(r: Rect, x: u32, y: u32) -> bool {
    x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
}

/// A colour block the RGB pass fills before the ink goes on top.
type Fill = (Rect, [u8; 3]);

/// Ink plus the colour blocks under it. Grey panels get the ink alone;
/// colour panels get the blocks first — colour lives in blocks, text stays
/// black, which is what the Kaleido filter shows cleanly.
pub(super) struct Painted {
    pub ink: GrayPage,
    pub fills: Vec<Fill>,
}

impl Painted {
    pub fn into_rgb(self) -> RgbPage {
        let mut canvas = RgbPage::from_gray(&GrayPage::new_white(self.ink.width, self.ink.height));
        for ((x, y, w, h), c) in &self.fills {
            fill_rect_rgb(&mut canvas, *x, *y, *w, *h, *c);
        }
        widgets::overlay_ink(&mut canvas, &self.ink);
        canvas
    }
}

/// Fill a rectangle in the ink layer.
fn fill_gray(canvas: &mut GrayPage, r: Rect, value: u8) {
    for y in r.1..(r.1 + r.3).min(canvas.height) {
        for x in r.0..(r.0 + r.2).min(canvas.width) {
            plot(canvas, x as i32, y as i32, value);
        }
    }
}

/// A `t`-pixel outline.
fn outline(canvas: &mut GrayPage, r: Rect, t: u32, value: u8) {
    for i in 0..t {
        if r.2 > 2 * i && r.3 > 2 * i {
            rect_outline(canvas, r.0 + i, r.1 + i, r.2 - 2 * i, r.3 - 2 * i, value);
        }
    }
}

/// The letter on a book's spine block.
fn initial(title: &str) -> String {
    title
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default()
}

/// A spine/cover block: black outline, an accent stripe down its left, the
/// title's initial.
fn draw_spine(p: &mut Painted, r: Rect, px: f32, title: &str, accent: [u8; 3]) {
    outline(&mut p.ink, r, 2, 0x00);
    let stripe = (r.2 / 9).max(3);
    p.fills.push((
        (r.0 + r.2 / 6, r.1 + 2, stripe, r.3.saturating_sub(4)),
        accent,
    ));
    let letter = initial(title);
    let lw = measure_text(px, &letter, true);
    let left = r.0 + r.2 / 6 + stripe;
    let room = (r.0 + r.2).saturating_sub(left);
    draw_text(
        &mut p.ink,
        left + room.saturating_sub(lw) / 2,
        r.1 + r.3.saturating_sub(px as u32) / 2,
        px,
        &letter,
        room,
        true,
    );
}

/// A small filled chip with bold text; returns its rectangle.
fn chip_rect(px: f32, label: &str, right: u32, center_y: u32, pad: u32) -> Rect {
    let w = measure_text(px, label, true) + pad * 2;
    let h = (px * 1.6) as u32;
    (
        right.saturating_sub(w),
        center_y.saturating_sub(h / 2),
        w,
        h,
    )
}

fn draw_chip(p: &mut Painted, r: Rect, px: f32, label: &str, fill: [u8; 3]) {
    p.fills.push((r, fill));
    let w = measure_text(px, label, true);
    draw_text(
        &mut p.ink,
        r.0 + r.2.saturating_sub(w) / 2,
        r.1 + r.3.saturating_sub(px as u32) / 2,
        px,
        label,
        r.2,
        true,
    );
}

/// A button: the primary one is the accent's wash with a heavy accent
/// border; the other a plain black outline.
fn draw_button(p: &mut Painted, r: Rect, px: f32, label: &str, primary: bool, t: &widgets::Theme) {
    if primary {
        p.fills.push((r, t.accent_wash()));
        let b = 3;
        p.fills.push(((r.0, r.1, r.2, b), t.accent));
        p.fills
            .push(((r.0, (r.1 + r.3).saturating_sub(b), r.2, b), t.accent));
        p.fills.push(((r.0, r.1, b, r.3), t.accent));
        p.fills
            .push((((r.0 + r.2).saturating_sub(b), r.1, b, r.3), t.accent));
    } else {
        outline(&mut p.ink, r, 2, 0x00);
    }
    let w = measure_text(px, label, true).min(r.2);
    draw_text(
        &mut p.ink,
        r.0 + r.2.saturating_sub(w) / 2,
        r.1 + r.3.saturating_sub(px as u32 + 4) / 2,
        px,
        label,
        r.2,
        true,
    );
}

/// Where a book lands, so a row can say ON KOBO before anything is fetched.
pub(super) fn saved_path(books_dir: &Path, entry: &gateway::BookEntry) -> PathBuf {
    gideon_sources::opds::epub_path(books_dir, entry.detail.as_deref(), &entry.title)
}

/// "Mary Shelley · 1818", or whichever half there is.
fn byline(entry: &gateway::BookEntry) -> String {
    match (&entry.detail, &entry.year) {
        (Some(a), Some(y)) => format!("{a} · {y}"),
        (Some(a), None) => a.clone(),
        (None, Some(y)) => y.clone(),
        (None, None) => String::new(),
    }
}

/// "2.0 MB".
fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024).max(1))
    }
}

/// The search screen's chips and hint, laid out once for drawing and
/// hit-testing.
pub(super) struct SearchExtras {
    pub hint_y: u32,
    pub label_y: u32,
    pub chips: Vec<Rect>,
}

/// The Internet Archive's book sheet, laid out once for drawing and
/// hit-testing.
pub(super) struct DownloadSheetLayout {
    pub top: u32,
    pub height: u32,
    pub cover: Rect,
    pub text_x: u32,
    pub buttons: [Rect; 2],
}

/// The results footer's four zones, left to right: Back, Prev, the page
/// label, Next. One function for drawing and hit-testing.
pub(super) fn results_footer_zones(l: &UiLayout) -> [Rect; 4] {
    let y = l.nav_top();
    let unit = l.width / 4;
    [
        (0, y, unit, l.nav_h),
        (unit, y, unit, l.nav_h),
        (unit * 2, y, unit, l.nav_h),
        (unit * 3, y, l.width.saturating_sub(unit * 3), l.nav_h),
    ]
}

/// A result row is two list rows tall: two lines of title and a byline.
pub(super) fn result_row_h(l: &UiLayout) -> u32 {
    l.row_h * 2
}

pub(super) fn results_per_page(l: &UiLayout) -> usize {
    ((l.content_height() / result_row_h(l)).max(1)) as usize
}

/// How many result pages there are: the catalogue's total when it gave one
/// (more pages may still need fetching), else what has been loaded.
pub(super) fn results_page_count(l: &UiLayout, feed: &gateway::BookFeed) -> usize {
    let loaded = feed.entries.len();
    let total = match feed.total {
        Some(t) if feed.next.is_some() => (t as usize).max(loaded),
        _ => loaded,
    };
    total.div_ceil(results_per_page(l)).max(1)
}

/// The "Edit search" button of the empty and error states.
pub(super) fn edit_search_button(l: &UiLayout) -> Rect {
    let w = (measure_text(l.text_px, "Edit search", true) + l.pad * 8).min(l.width);
    let h = l.row_h;
    let y = l.content_top() + l.content_height() / 2 + l.row_h / 2;
    ((l.width - w) / 2, y, w, h)
}

/// What the empty / error state says: (heading, body).
fn empty_words(query: &str, error: Option<&str>) -> (String, String) {
    match error {
        Some(e) if e.contains("no network connection") => (
            "No Wi-Fi".to_string(),
            "Turn Wi-Fi on and search again.".to_string(),
        ),
        Some(e) => (
            "Couldn't search".to_string(),
            e.lines().next().unwrap_or_default().to_string(),
        ),
        None => (
            format!("No free books match \"{query}\""),
            "Check the spelling, or try just the author's last name.".to_string(),
        ),
    }
}

impl<D: Display, I: InputSource, G: SourceGateway> UiApp<D, I, G> {
    fn theme(&self) -> widgets::Theme {
        widgets::Theme::from_setting(&self.load_settings().color_profile)
    }

    // --- the search screen -------------------------------------------------

    /// Open the Internet Archive: straight onto the keyboard.
    pub(super) fn open_archive_search(&mut self, title: &str) -> Result<()> {
        self.keyboard_paints = 0;
        self.keyboard_shift = false;
        self.push(Screen::BookSearch {
            title: title.to_string(),
            search: gideon_sources::archive::search_link(),
            query: String::new(),
            recent: load_recent_searches(&self.library_dir),
        })
    }

    /// Where the hint, the "Recent" label and each chip sit, between the
    /// field and the keyboard. Chips that would not fit above the keyboard
    /// are left out rather than drawn under it.
    pub(super) fn search_extras(&self, recent: &[String]) -> SearchExtras {
        let l = &self.layout;
        let small = l.text_px * 0.75;
        let hint_y = l.row_top(1);
        let label_y = hint_y + l.row_h;
        let chip_px = l.text_px * 0.85;
        let chip_h = (chip_px * 1.9) as u32;
        let gap = l.pad;
        let mut x = l.pad;
        let mut y = label_y + (small * 1.8) as u32;
        let mut chips = Vec::new();
        for q in recent {
            let w = (measure_text(chip_px, q, false) + l.pad * 3).min(l.width - 2 * l.pad);
            if x + w > l.width - l.pad && x > l.pad {
                x = l.pad;
                y += chip_h + gap;
            }
            if y + chip_h > l.keyboard_top().saturating_sub(gap) {
                break;
            }
            chips.push((x, y, w, chip_h));
            x += w + gap;
        }
        SearchExtras {
            hint_y,
            label_y,
            chips,
        }
    }

    /// The search screen. Every catalogue's search is the keyboard; the
    /// Internet Archive's also says what it searches, offers the recent
    /// searches, and marks its Search key in the accent.
    pub(super) fn paint_book_search(
        &self,
        title: &str,
        search: &gateway::SearchLink,
        query: &str,
        recent: &[String],
    ) -> Painted {
        let l = &self.layout;
        let mut p = Painted {
            ink: compose_keyboard(l, title, query, "Search", self.keyboard_shift),
            fills: Vec::new(),
        };
        if !gideon_sources::archive::is_archive_search(search) {
            return p;
        }
        let t = self.theme();
        let small = l.text_px * 0.75;
        // The field is a box, so it reads as the thing being typed into.
        let inset = (l.row_h / 10).max(2);
        outline(
            &mut p.ink,
            (
                l.pad / 2,
                l.row_top(0) + inset,
                l.width.saturating_sub(l.pad),
                l.row_h.saturating_sub(2 * inset),
            ),
            2,
            0x00,
        );
        let extras = self.search_extras(recent);
        draw_text(
            &mut p.ink,
            l.pad,
            extras.hint_y + l.row_h.saturating_sub(small as u32) / 2,
            small,
            "Titles and authors · free EPUBs only, nothing to borrow",
            l.width - 2 * l.pad,
            false,
        );
        if !extras.chips.is_empty() {
            let mark = (l.pad / 2).max(3);
            p.fills
                .push(((l.pad, extras.label_y, mark, small as u32 + 4), t.accent));
            draw_text(
                &mut p.ink,
                l.pad + mark * 3,
                extras.label_y,
                small,
                "RECENT",
                l.width / 2,
                true,
            );
            let chip_px = l.text_px * 0.85;
            for (r, q) in extras.chips.iter().zip(recent) {
                outline(&mut p.ink, *r, 1, 0x00);
                draw_text(
                    &mut p.ink,
                    r.0 + l.pad * 3 / 2,
                    r.1 + r.3.saturating_sub(chip_px as u32 + 4) / 2,
                    chip_px,
                    q,
                    r.2.saturating_sub(l.pad * 3),
                    false,
                );
            }
        }
        // The Search key: the one key in the accent.
        if let Some((_, x, y, w, h)) = l.keyboard_keys().into_iter().find(|k| k.0 == Key::Search) {
            let inset = 2;
            let r = (
                x + inset,
                y + inset,
                w.saturating_sub(2 * inset),
                h.saturating_sub(2 * inset),
            );
            let b = 3;
            p.fills.push((r, t.accent_wash()));
            p.fills.push(((r.0, r.1, r.2, b), t.accent));
            p.fills
                .push(((r.0, (r.1 + r.3).saturating_sub(b), r.2, b), t.accent));
            p.fills.push(((r.0, r.1, b, r.3), t.accent));
            p.fills
                .push((((r.0 + r.2).saturating_sub(b), r.1, b, r.3), t.accent));
        }
        p
    }

    /// A tap on a recent-search chip, if it landed on one: run that search.
    pub(super) fn tap_recent_chip(
        &mut self,
        search: &gateway::SearchLink,
        recent: &[String],
        x: u32,
        y: u32,
    ) -> Result<bool> {
        if !gideon_sources::archive::is_archive_search(search) {
            return Ok(false);
        }
        let extras = self.search_extras(recent);
        let Some(i) = extras.chips.iter().position(|r| hit(*r, x, y)) else {
            return Ok(false);
        };
        let query = recent[i].clone();
        if let Some(Screen::BookSearch { query: q, .. }) = self.stack.last_mut() {
            *q = query.clone();
        }
        self.run_archive_search(search, &query)?;
        Ok(true)
    }

    // --- results -------------------------------------------------------

    /// Search the Internet Archive and show the results — or, when there are
    /// none or the search failed, the empty state with its way back to the
    /// keyboard. Never a dead-end message screen.
    pub(super) fn run_archive_search(
        &mut self,
        search: &gateway::SearchLink,
        query: &str,
    ) -> Result<()> {
        remember_search(&self.library_dir, query);
        if let Some(Screen::BookSearch { recent, .. }) = self.stack.last_mut() {
            *recent = load_recent_searches(&self.library_dir);
        }
        self.ensure_online()?;
        self.show_status(&[&format!("Searching for \"{query}\"…")])?;
        let (feed, error) = match self.gateway.book_search(search, query) {
            Ok(Some(feed)) => (feed, None),
            Ok(None) => (
                gateway::BookFeed::default(),
                Some("This catalogue doesn't offer search.".to_string()),
            ),
            Err(e) => {
                eprintln!("gideon: book search failed: {e:#}");
                (gateway::BookFeed::default(), Some(format!("{e:#}")))
            }
        };
        self.push(Screen::BookResults {
            query: query.to_string(),
            feed,
            page: 0,
            error,
        })
    }

    /// The results screen, ink and colour.
    pub(super) fn paint_book_results(
        &self,
        query: &str,
        feed: &gateway::BookFeed,
        page: usize,
        error: Option<&str>,
    ) -> Painted {
        let l = &self.layout;
        let t = self.theme();
        let mut ink = GrayPage::new_white(l.width, l.height);
        let mut p = Painted {
            ink: GrayPage::new_white(1, 1),
            fills: Vec::new(),
        };

        // Title bar: the query, and how many books it found.
        let title_y = l.title_h.saturating_sub(l.text_px as u32 + 4) / 2;
        let count = match feed.total {
            Some(n) if error.is_none() => {
                let n = n.max(feed.entries.len() as u64);
                if n == 1 {
                    "1 book".to_string()
                } else {
                    format!("{n} books")
                }
            }
            _ if error.is_none() => format!("{} books", feed.entries.len()),
            _ => String::new(),
        };
        let small = l.text_px * 0.8;
        let cw = measure_text(small, &count, false);
        draw_text(
            &mut ink,
            l.width.saturating_sub(l.pad + cw),
            l.title_h.saturating_sub(small as u32 + 4) / 2,
            small,
            &count,
            cw + 1,
            false,
        );
        draw_text(
            &mut ink,
            l.pad,
            title_y,
            l.text_px,
            &format!("\"{query}\""),
            l.width.saturating_sub(3 * l.pad + cw),
            true,
        );
        fill_gray(&mut ink, (0, l.title_h.saturating_sub(2), l.width, 2), 0x00);

        // Footer: Back · Prev · page · Next.
        let pages = results_page_count(l, feed);
        let zones = results_footer_zones(l);
        fill_gray(&mut ink, (0, l.nav_top(), l.width, 2), 0x00);
        let label_y = |z: Rect| z.1 + z.3.saturating_sub(l.text_px as u32 + 4) / 2;
        draw_text(
            &mut ink,
            l.pad,
            label_y(zones[0]),
            l.text_px,
            "< Back",
            zones[0].2,
            true,
        );
        if error.is_none() && !feed.entries.is_empty() {
            if page > 0 {
                let w = measure_text(l.text_px, "‹ Prev", false);
                draw_text(
                    &mut ink,
                    zones[1].0 + zones[1].2.saturating_sub(w) / 2,
                    label_y(zones[1]),
                    l.text_px,
                    "‹ Prev",
                    zones[1].2,
                    false,
                );
            }
            let label = format!("{} / {}", page + 1, pages);
            let w = measure_text(l.text_px, &label, false);
            draw_text(
                &mut ink,
                zones[2].0 + zones[2].2.saturating_sub(w) / 2,
                label_y(zones[2]),
                l.text_px,
                &label,
                zones[2].2,
                false,
            );
            if page + 1 < pages {
                let w = measure_text(l.text_px, "Next ›", true);
                let box_w = (w + l.pad * 2).min(zones[3].2);
                let bh = (l.text_px * 1.8) as u32;
                let bx = zones[3].0 + zones[3].2.saturating_sub(box_w) / 2;
                let by = zones[3].1 + zones[3].3.saturating_sub(bh) / 2;
                outline(&mut ink, (bx, by, box_w, bh), 2, 0x00);
                draw_text(
                    &mut ink,
                    bx + box_w.saturating_sub(w) / 2,
                    label_y(zones[3]),
                    l.text_px,
                    "Next ›",
                    box_w,
                    true,
                );
            }
        }

        if error.is_some() || feed.entries.is_empty() {
            let (heading, body) = empty_words(query, error);
            let max_w = l.width.saturating_sub(4 * l.pad);
            let mut y = l.content_top() + l.content_height() / 3;
            for line in wrap_text(l.text_px, &heading, max_w).iter().take(2) {
                let w = measure_text(l.text_px, line, true);
                draw_text(
                    &mut ink,
                    (l.width - w.min(l.width)) / 2,
                    y,
                    l.text_px,
                    line,
                    max_w,
                    true,
                );
                y += (l.text_px * 1.3) as u32;
            }
            y += l.pad;
            let px = l.text_px * 0.85;
            for line in wrap_text(px, &body, max_w).iter().take(3) {
                let w = measure_text(px, line, false);
                draw_text(
                    &mut ink,
                    (l.width - w.min(l.width)) / 2,
                    y,
                    px,
                    line,
                    max_w,
                    false,
                );
                y += (px * 1.3) as u32;
            }
            p.ink = ink;
            draw_button(
                &mut p,
                edit_search_button(l),
                l.text_px,
                "Edit search",
                true,
                &t,
            );
            return p;
        }

        // The rows.
        let rh = result_row_h(l);
        let per = results_per_page(l);
        let title_px = l.text_px;
        let by_px = l.text_px * 0.8;
        let chip_px = l.text_px * 0.6;
        let mut fills = Vec::new();
        for (i, entry) in feed.entries.iter().skip(page * per).take(per).enumerate() {
            let top = l.content_top() + i as u32 * rh;
            let sw = (rh as f32 * 0.36) as u32;
            let sh = (rh as f32 * 0.62) as u32;
            let spine = (l.pad, top + rh.saturating_sub(sh) / 2, sw, sh);
            let mut row = Painted { ink, fills };
            draw_spine(&mut row, spine, title_px * 0.9, &entry.title, t.accent);
            let on_kobo = saved_path(&self.books_dir, entry).exists();
            let (label, fill) = if on_kobo {
                ("ON KOBO", t.status_tint)
            } else {
                ("EPUB", t.genre_tint)
            };
            let chip = chip_rect(chip_px, label, l.width - l.pad, top + rh / 2, l.pad / 2 + 2);
            draw_chip(&mut row, chip, chip_px, label, fill);
            Painted { ink, fills } = row;

            let text_x = spine.0 + spine.2 + l.pad * 2;
            let text_w = chip.0.saturating_sub(l.pad + text_x);
            let lines = wrap_text(title_px, &entry.title, text_w);
            let shown: Vec<String> = match lines.len() {
                0 => vec![entry.title.clone()],
                1 | 2 => lines,
                // A third line would push the byline off the row: the second
                // line carries the rest, and draw_text ellipsizes it.
                _ => vec![lines[0].clone(), lines[1..].join(" ")],
            };
            let line_h = (title_px * 1.25) as u32;
            let by = byline(entry);
            let block = line_h * shown.len() as u32
                + if by.is_empty() {
                    0
                } else {
                    (by_px * 1.3) as u32
                };
            let mut y = top + rh.saturating_sub(block) / 2;
            for line in &shown {
                draw_text(&mut ink, text_x, y, title_px, line, text_w, true);
                y += line_h;
            }
            if !by.is_empty() {
                draw_text(&mut ink, text_x, y + 2, by_px, &by, text_w, false);
            }
            if top + rh < l.nav_top() {
                fill_gray(&mut ink, (0, top + rh - 1, l.width, 1), 0x00);
            }
        }
        p.ink = ink;
        p.fills = fills;
        p
    }

    /// Route a tap on the results screen.
    pub(super) fn tap_book_results(&mut self, x: u32, y: u32) -> Result<Flow> {
        let Some(Screen::BookResults {
            feed, page, error, ..
        }) = self.stack.last().cloned()
        else {
            return Ok(Flow::Continue);
        };
        let l = self.layout;
        if y >= l.nav_top() {
            let zones = results_footer_zones(&l);
            let pages = results_page_count(&l, &feed);
            if hit(zones[0], x, y) {
                return self.pop();
            }
            if hit(zones[1], x, y) && page > 0 {
                self.set_results_page(page - 1)?;
            } else if hit(zones[3], x, y) && page + 1 < pages {
                self.next_results_page()?;
            }
            return Ok(Flow::Continue);
        }
        if error.is_some() || feed.entries.is_empty() {
            // Edit search: back to the keyboard, query still in it.
            if hit(edit_search_button(&l), x, y) {
                return self.pop();
            }
            return Ok(Flow::Continue);
        }
        if y < l.content_top() {
            return Ok(Flow::Continue);
        }
        let row = ((y - l.content_top()) / result_row_h(&l)) as usize;
        if row >= results_per_page(&l) {
            return Ok(Flow::Continue);
        }
        if let Some(entry) = feed.entries.get(page * results_per_page(&l) + row).cloned() {
            self.open_book_sheet(entry)?;
        }
        Ok(Flow::Continue)
    }

    fn set_results_page(&mut self, to: usize) -> Result<()> {
        if let Some(Screen::BookResults { page, .. }) = self.stack.last_mut() {
            *page = to;
        }
        // The whole list is replaced: Full, like any list page flip.
        self.render_current(RefreshMode::Full)
    }

    /// Next page, fetching the catalogue's next page first when the rows for
    /// it have not been loaded yet.
    fn next_results_page(&mut self) -> Result<()> {
        let Some(Screen::BookResults { feed, page, .. }) = self.stack.last().cloned() else {
            return Ok(());
        };
        let per = results_per_page(&self.layout);
        let want = page + 1;
        // Fetch when the page about to show is not full yet — a half-loaded
        // page would end early and hide the rest of the results.
        if feed.entries.len() < (want + 1) * per && feed.next.is_some() {
            self.ensure_online()?;
            self.show_status(&["Loading more…"])?;
            match self.gateway.book_next_page(&feed) {
                Ok(Some(next)) => {
                    if let Some(Screen::BookResults { feed, .. }) = self.stack.last_mut() {
                        feed.entries.extend(next.entries);
                        feed.next = next.next;
                    }
                }
                Ok(None) => {
                    if let Some(Screen::BookResults { feed, .. }) = self.stack.last_mut() {
                        feed.next = None;
                    }
                }
                Err(e) => eprintln!("gideon: next results page failed: {e:#}"),
            }
        }
        // The catalogue's total counts items without an EPUB that never
        // reach the list, so the last page can turn out not to exist.
        let loaded = match self.stack.last() {
            Some(Screen::BookResults { feed, .. }) => feed.entries.len(),
            _ => 0,
        };
        let to = if loaded > want * per { want } else { page };
        self.set_results_page(to)
    }

    // --- the book sheet --------------------------------------------------

    /// Raise the sheet for one result. A book already on the Kobo opens
    /// straight to "Saved" — nothing to fetch; anything else asks archive.org
    /// for its EPUB first, with the sheet already up and saying so.
    fn open_book_sheet(&mut self, entry: gateway::BookEntry) -> Result<()> {
        let path = saved_path(&self.books_dir, &entry);
        if path.exists() {
            self.sheet = Some(Sheet::BookDownload {
                entry: Box::new(entry),
                epub: None,
                state: DownloadState::Saved(path),
            });
            return self.render_current(RefreshMode::Partial);
        }
        self.ensure_online()?;
        self.sheet = Some(Sheet::BookDownload {
            entry: Box::new(entry.clone()),
            epub: None,
            state: DownloadState::Checking,
        });
        self.render_current(RefreshMode::Partial)?;
        self.check_book_sheet(entry)
    }

    /// Ask for the book's EPUB and update the sheet in place.
    fn check_book_sheet(&mut self, entry: gateway::BookEntry) -> Result<()> {
        let (epub, state) = match self.gateway.book_open(&entry) {
            Ok(gateway::BookOpened::Book(epub)) => (Some(epub), DownloadState::Ready),
            Ok(gateway::BookOpened::Feed(_)) => (
                None,
                DownloadState::Failed("This entry isn't a single book.".to_string()),
            ),
            Err(e) => {
                eprintln!("gideon: opening book entry failed: {e:#}");
                (None, DownloadState::Failed(first_line(&e)))
            }
        };
        self.sheet = Some(Sheet::BookDownload {
            entry: Box::new(entry),
            epub,
            state,
        });
        // Same strip, new contents.
        self.render_current(RefreshMode::Partial)
    }

    fn download_from_sheet(
        &mut self,
        entry: gateway::BookEntry,
        epub: gateway::EpubLink,
    ) -> Result<()> {
        self.sheet = Some(Sheet::BookDownload {
            entry: Box::new(entry.clone()),
            epub: Some(epub.clone()),
            state: DownloadState::Downloading,
        });
        self.render_current(RefreshMode::Partial)?;
        let state = match self.gateway.book_download(
            &epub.url,
            &self.books_dir,
            entry.detail.as_deref(),
            &entry.title,
        ) {
            Ok((path, _)) => DownloadState::Saved(path),
            Err(e) => {
                eprintln!("gideon: book download failed: {e:#}");
                DownloadState::Failed(first_line(&e))
            }
        };
        self.sheet = Some(Sheet::BookDownload {
            entry: Box::new(entry),
            epub: Some(epub),
            state,
        });
        self.render_current(RefreshMode::Partial)
    }

    /// The sheet's buttons for its state: (secondary, primary), or none
    /// while it is busy.
    fn book_sheet_buttons(state: &DownloadState) -> Option<(&'static str, &'static str)> {
        match state {
            DownloadState::Checking | DownloadState::Downloading => None,
            DownloadState::Ready => Some(("Cancel", "Download")),
            DownloadState::Saved(_) => Some(("Back to results", "Read now")),
            DownloadState::Failed(_) => Some(("Close", "Try again")),
        }
    }

    pub(super) fn download_sheet_layout(&self) -> DownloadSheetLayout {
        let l = &self.layout;
        let pad = l.pad * 2;
        let cover_w = (l.row_h as f32 * 0.9) as u32;
        let cover_h = (l.row_h as f32 * 1.3) as u32;
        let text_body = (l.text_px * 1.25) as u32 * 2
            + (l.text_px * 0.85 * 1.3) as u32
            + (l.text_px * 0.7 * 2.2) as u32;
        let body_h = cover_h.max(text_body);
        let btn_h = l.row_h;
        let height = (pad + body_h + pad + btn_h + pad).min(l.height);
        let top = l.height - height;
        let cover = (pad, top + pad, cover_w, cover_h);
        let by = top + pad + body_h + pad;
        let bw = l.width.saturating_sub(pad * 3) / 2;
        DownloadSheetLayout {
            top,
            height,
            cover,
            text_x: pad + cover_w + pad,
            buttons: [(pad, by, bw, btn_h), (pad * 2 + bw, by, bw, btn_h)],
        }
    }

    /// Draw the book sheet over the composed screen.
    pub(super) fn overlay_book_download(&self, canvas: &mut RgbPage) {
        let Some(Sheet::BookDownload { entry, epub, state }) = &self.sheet else {
            return;
        };
        let l = &self.layout;
        let t = self.theme();
        let s = self.download_sheet_layout();
        // Opaque, with the heavy top edge every sheet carries.
        fill_rect_rgb(canvas, 0, s.top, l.width, s.height, [0xFF; 3]);
        fill_rect_rgb(canvas, 0, s.top, l.width, 3, [0x00; 3]);

        let mut p = Painted {
            ink: GrayPage::new_white(l.width, l.height),
            fills: Vec::new(),
        };
        draw_spine(&mut p, s.cover, l.text_px * 1.3, &entry.title, t.accent);

        let text_w = l.width.saturating_sub(s.text_x + l.pad * 2);
        let mut y = s.cover.1;
        let title_px = l.text_px * 1.05;
        let lines = wrap_text(title_px, &entry.title, text_w);
        let shown: Vec<String> = match lines.len() {
            0..=2 => lines,
            _ => vec![lines[0].clone(), lines[1..].join(" ")],
        };
        for line in &shown {
            draw_text(&mut p.ink, s.text_x, y, title_px, line, text_w, true);
            y += (title_px * 1.2) as u32;
        }
        if let Some(author) = &entry.detail {
            let px = l.text_px * 0.85;
            draw_text(&mut p.ink, s.text_x, y + 2, px, author, text_w, false);
            y += (px * 1.3) as u32;
        }
        y += l.pad;
        let px = l.text_px * 0.7;
        match state {
            DownloadState::Checking => {
                draw_text(
                    &mut p.ink,
                    s.text_x,
                    y,
                    px * 1.1,
                    "Checking the file…",
                    text_w,
                    false,
                );
            }
            DownloadState::Downloading => {
                draw_text(
                    &mut p.ink,
                    s.text_x,
                    y,
                    px * 1.2,
                    "Downloading…",
                    text_w,
                    true,
                );
            }
            DownloadState::Ready => {
                let mut facts: Vec<String> = Vec::new();
                facts.extend(entry.year.clone());
                facts.extend(entry.language.clone());
                facts.push(match epub.as_ref().and_then(|e| e.size) {
                    Some(n) => format!("EPUB · {}", human_size(n)),
                    None => "EPUB".to_string(),
                });
                let mut x = s.text_x;
                let h = (px * 1.7) as u32;
                for f in facts {
                    let w = measure_text(px, &f, false) + l.pad * 2;
                    if x + w > l.width - l.pad {
                        break;
                    }
                    outline(&mut p.ink, (x, y, w, h), 1, 0x00);
                    draw_text(
                        &mut p.ink,
                        x + l.pad,
                        y + h.saturating_sub(px as u32 + 2) / 2,
                        px,
                        &f,
                        w,
                        false,
                    );
                    x += w + l.pad / 2 + 2;
                }
            }
            DownloadState::Saved(_) => {
                let side = (l.text_px * 1.1) as u32;
                p.fills.push(((s.text_x, y, side, side), t.status_tint));
                let cw = measure_text(l.text_px * 0.8, "✓", true);
                draw_text(
                    &mut p.ink,
                    s.text_x + side.saturating_sub(cw) / 2,
                    y + side.saturating_sub((l.text_px * 0.8) as u32) / 2,
                    l.text_px * 0.8,
                    "✓",
                    side,
                    true,
                );
                draw_text(
                    &mut p.ink,
                    s.text_x + side + l.pad,
                    y + side.saturating_sub(l.text_px as u32) / 2,
                    l.text_px * 0.9,
                    "Saved to Books",
                    text_w.saturating_sub(side + l.pad),
                    true,
                );
            }
            DownloadState::Failed(msg) => {
                draw_text(&mut p.ink, s.text_x, y, px * 1.1, msg, text_w, false);
            }
        }
        if let Some((secondary, primary)) = Self::book_sheet_buttons(state) {
            draw_button(&mut p, s.buttons[0], l.text_px * 0.9, secondary, false, &t);
            draw_button(&mut p, s.buttons[1], l.text_px * 0.9, primary, true, &t);
        }
        for ((x, y, w, h), c) in &p.fills {
            fill_rect_rgb(canvas, *x, *y, *w, *h, *c);
        }
        // Only the sheet's own strip of the ink layer goes on: the layer is
        // white above it, and overlay_ink skips white.
        widgets::overlay_ink(canvas, &p.ink);
    }

    /// Route a tap while the book sheet is up.
    pub(super) fn tap_book_download(&mut self, x: u32, y: u32) -> Result<Flow> {
        let Some(Sheet::BookDownload { entry, epub, state }) = self.sheet.clone() else {
            return Ok(Flow::Continue);
        };
        let entry = *entry;
        let s = self.download_sheet_layout();
        let close = |app: &mut Self| -> Result<Flow> {
            app.sheet = None;
            // The sheet held a strip of the results stale: reveal it Full.
            app.render_current(RefreshMode::Full)?;
            Ok(Flow::Continue)
        };
        if y < s.top {
            return close(self);
        }
        let Some(_) = Self::book_sheet_buttons(&state) else {
            return Ok(Flow::Continue);
        };
        let primary = hit(s.buttons[1], x, y);
        if !primary {
            if hit(s.buttons[0], x, y) {
                return close(self);
            }
            return Ok(Flow::Continue);
        }
        match state {
            DownloadState::Ready => match epub {
                Some(epub) => self.download_from_sheet(entry, epub)?,
                None => self.check_book_sheet(entry)?,
            },
            DownloadState::Failed(_) => match epub {
                Some(epub) => self.download_from_sheet(entry, epub)?,
                None => {
                    self.ensure_online()?;
                    self.check_book_sheet(entry)?
                }
            },
            DownloadState::Saved(path) => {
                self.sheet = None;
                let book = gideon_core::books::scan(&self.books_dir)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|b| b.path == path);
                match book {
                    Some(book) => {
                        if let Err(e) = self.open_book_reader(book) {
                            self.push(Screen::Message {
                                title: entry.title.clone(),
                                body: format!("{e:#}"),
                            })?;
                        }
                    }
                    None => self.render_current(RefreshMode::Full)?,
                }
            }
            DownloadState::Checking | DownloadState::Downloading => {}
        }
        Ok(Flow::Continue)
    }
}

/// The first line of an error, for a sheet that has one line to give it.
fn first_line(e: &anyhow::Error) -> String {
    format!("{e:#}")
        .lines()
        .next()
        .unwrap_or("Something went wrong.")
        .to_string()
}
