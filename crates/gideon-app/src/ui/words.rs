//! Saved words, as approved in design review (gallery design
//! `reader-highlights-fonts-words`, revised: a plain list, no
//! Learning / Known):
//!
//! * every dictionary lookup in the book reader is kept, with the sentence
//!   the word came from — nothing to tap;
//! * the definition sheet says so, and offers "All my words";
//! * **Words** lists them newest first, all books or just this one; a tap
//!   shows the definition and the sentence, with Remove and Go to page.
//!
//! Refresh, per docs/REFRESH.md: the list is a new screen (Full), as are
//! its page flips and tab switches; the word's sheet opens Partial and
//! closes Full.

use super::book_search::{draw_button, hit, results_footer_zones, Painted, Rect};
use super::*;

/// The longest sentence kept with a word.
const CONTEXT_CHARS: usize = 220;

/// A word's row is two list rows tall: the word, its gloss, where from.
fn word_row_h(l: &UiLayout) -> u32 {
    l.row_h * 2
}

fn words_per_page(l: &UiLayout) -> usize {
    ((l.content_height() / word_row_h(l)).max(1)) as usize
}

/// The title bar's two tabs, as tap zones: "All books" then "This book".
fn word_tabs(l: &UiLayout) -> [Rect; 2] {
    let gap = l.pad * 2;
    let all = measure_text(l.text_px, "All books", true) + gap;
    let this = measure_text(l.text_px, "This book", true) + gap;
    [
        (0, 0, l.pad + all, l.title_h),
        (l.pad + all, 0, this, l.title_h),
    ]
}

/// "today", "yesterday", or "Nov 9".
fn when(saved_at: u64, now: u64) -> String {
    let days = now.saturating_sub(saved_at) / 86_400;
    match days {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        _ => chrono_like(saved_at),
    }
}

/// "Nov 9" for a unix time, without pulling in a date crate.
fn chrono_like(secs: u64) -> String {
    // Civil-from-days (Howard Hinnant), enough for a short date.
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!("{} {}", MONTHS[(m - 1) as usize], d)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl<D: Display, I: InputSource, G: SourceGateway> UiApp<D, I, G> {
    /// Keep a word just looked up in the reader, with where it was met.
    pub(super) fn save_looked_up_word(&mut self, word: &str, definition: &str) {
        let Some(Screen::BookReader {
            book,
            text,
            chapter,
            pages,
            page,
        }) = self.stack.last()
        else {
            return;
        };
        // The paragraph it came from: the held selection's, else the first
        // on this page that has the word.
        let block = match self.selection {
            Some((a, _)) => Some(a.0),
            None => {
                let want = gideon_core::dict::trim_word(word).to_lowercase();
                pages.get(*page).and_then(|lines| {
                    lines
                        .iter()
                        .find(|l| l.text.to_lowercase().contains(&want))
                        .map(|l| l.block)
                })
            }
        };
        let paragraph = block
            .map(|b| super::highlights::block_words(text, *chapter, b).join(" "))
            .unwrap_or_default();
        let entry = gideon_core::SavedWord {
            word: word.to_string(),
            gloss: definition
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string(),
            context: gideon_core::words::sentence_with(&paragraph, word, CONTEXT_CHARS),
            book: book.relative_path.clone(),
            title: book.title.clone(),
            chapter: *chapter,
            block: block.unwrap_or(0),
            saved_at: now_secs(),
        };
        let mut words = gideon_core::SavedWords::load(&self.books_dir);
        words.record(entry);
        let _ = words.save(&self.books_dir);
    }

    /// Open the Words list. From the reader it knows the open book, so the
    /// "This book" tab has something to show.
    pub(super) fn open_words(&mut self, this_book: bool) -> Result<()> {
        let book = match self.stack.last() {
            Some(Screen::BookReader { book, .. }) => Some(book.relative_path.clone()),
            _ => None,
        };
        self.sheet = None;
        self.selection = None;
        let entries = gideon_core::SavedWords::load(&self.books_dir).words;
        if entries.is_empty() {
            return self.push(Screen::Message {
                title: "Words".to_string(),
                body: "No words yet.\nHold a word in a book to look it up;\nevery word you look up is kept here."
                    .to_string(),
            });
        }
        self.push(Screen::Words {
            entries,
            book: book.clone(),
            this_book: this_book && book.is_some(),
            page: 0,
        })
    }

    /// The rows the list shows under its current tab.
    fn shown_words<'a>(
        entries: &'a [gideon_core::SavedWord],
        book: &Option<String>,
        this_book: bool,
    ) -> Vec<&'a gideon_core::SavedWord> {
        entries
            .iter()
            .filter(|w| !this_book || book.as_deref() == Some(w.book.as_str()))
            .collect()
    }

    pub(super) fn paint_words(
        &self,
        entries: &[gideon_core::SavedWord],
        book: &Option<String>,
        this_book: bool,
        page: usize,
    ) -> GrayPage {
        let l = &self.layout;
        let mut ink = GrayPage::new_white(l.width, l.height);
        let shown = Self::shown_words(entries, book, this_book);

        // Title bar: the tabs, and the count on the right.
        let tabs = word_tabs(l);
        let ty = l.title_h.saturating_sub(l.text_px as u32 + 4) / 2;
        let bar_h = (l.pad / 2).max(3);
        for (i, (label, on)) in [("All books", !this_book), ("This book", this_book)]
            .iter()
            .enumerate()
        {
            if i == 1 && book.is_none() {
                continue;
            }
            let x = tabs[i].0 + l.pad;
            let w = measure_text(l.text_px, label, true);
            draw_text(&mut ink, x, ty, l.text_px, label, w + 1, *on);
            if *on {
                super::book_search::fill_gray(&mut ink, (x, l.title_h - bar_h - 2, w, bar_h), 0x00);
            }
        }
        let count = format!("{} saved", shown.len());
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
        super::book_search::fill_gray(&mut ink, (0, l.title_h - 1, l.width, 1), 0x00);

        // Rows.
        let rh = word_row_h(l);
        let per = words_per_page(l);
        let now = now_secs();
        for (i, w) in shown.iter().skip(page * per).take(per).enumerate() {
            let top = l.content_top() + i as u32 * rh;
            let x = l.pad * 2;
            let tw = l.width.saturating_sub(x + l.pad * 2);
            let wpx = l.text_px * 1.05;
            let gpx = l.text_px * 0.8;
            let spx = l.text_px * 0.65;
            let block = (wpx * 1.3) as u32 + (gpx * 1.3) as u32 + (spx * 1.3) as u32;
            let mut y = top + rh.saturating_sub(block) / 2;
            draw_text(&mut ink, x, y, wpx, &w.word, tw, true);
            y += (wpx * 1.3) as u32;
            draw_text(&mut ink, x, y, gpx, &w.gloss, tw, false);
            y += (gpx * 1.3) as u32;
            let source = format!("{} · {}", w.title, when(w.saved_at, now));
            draw_text(&mut ink, x, y, spx, &source, tw, false);
            if top + rh < l.nav_top() {
                super::book_search::fill_gray(&mut ink, (0, top + rh - 1, l.width, 1), 0x00);
            }
        }

        // Footer: Back · Prev · page · Next, as the search results.
        let pages = shown.len().div_ceil(per).max(1);
        let zones = results_footer_zones(l);
        super::book_search::fill_gray(&mut ink, (0, l.nav_top(), l.width, 2), 0x00);
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
        if pages > 1 {
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
        }
        if page + 1 < pages {
            let w = measure_text(l.text_px, "Next ›", true);
            draw_text(
                &mut ink,
                zones[3].0 + zones[3].2.saturating_sub(w) / 2,
                label_y(zones[3]),
                l.text_px,
                "Next ›",
                zones[3].2,
                true,
            );
        }
        if shown.is_empty() {
            let msg = "No words from this book yet.";
            let w = measure_text(l.text_px, msg, false);
            draw_text(
                &mut ink,
                (l.width - w.min(l.width)) / 2,
                l.content_top() + l.content_height() / 3,
                l.text_px,
                msg,
                l.width,
                false,
            );
        }
        ink
    }

    pub(super) fn tap_words(&mut self, x: u32, y: u32) -> Result<Flow> {
        let Some(Screen::Words {
            entries,
            book,
            this_book,
            page,
        }) = self.stack.last().cloned()
        else {
            return Ok(Flow::Continue);
        };
        let l = self.layout;
        let shown: Vec<gideon_core::SavedWord> = Self::shown_words(&entries, &book, this_book)
            .into_iter()
            .cloned()
            .collect();
        let per = words_per_page(&l);
        let pages = shown.len().div_ceil(per).max(1);
        let set = |app: &mut Self, tab: bool, to: usize| -> Result<()> {
            if let Some(Screen::Words {
                this_book, page, ..
            }) = app.stack.last_mut()
            {
                *this_book = tab;
                *page = to;
            }
            // The whole list is replaced: Full.
            app.render_current(RefreshMode::Full)
        };
        if y < l.title_h {
            let tabs = word_tabs(&l);
            if hit(tabs[0], x, y) && this_book {
                set(self, false, 0)?;
            } else if hit(tabs[1], x, y) && !this_book && book.is_some() {
                set(self, true, 0)?;
            }
            return Ok(Flow::Continue);
        }
        if y >= l.nav_top() {
            let zones = results_footer_zones(&l);
            if hit(zones[0], x, y) {
                return self.pop();
            }
            if hit(zones[1], x, y) && page > 0 {
                set(self, this_book, page - 1)?;
            } else if hit(zones[3], x, y) && page + 1 < pages {
                set(self, this_book, page + 1)?;
            }
            return Ok(Flow::Continue);
        }
        let row = ((y - l.content_top()) / word_row_h(&l)) as usize;
        if row < per {
            if let Some(w) = shown.get(page * per + row).cloned() {
                self.sheet = Some(Sheet::SavedWord { word: Box::new(w) });
                // The list behind does not move.
                self.render_current(RefreshMode::Partial)?;
            }
        }
        Ok(Flow::Continue)
    }

    // --- the word's sheet --------------------------------------------------

    pub(super) fn word_sheet_layout(&self, lines: usize, context: usize) -> (u32, u32, [Rect; 2]) {
        let l = &self.layout;
        let pad = l.pad * 2;
        let title_h = (l.text_px * 1.6) as u32;
        let dpx = l.text_px * 0.85;
        let body =
            (dpx * 1.3) as u32 * lines as u32 + (dpx * 1.3) as u32 * (context as u32 + 1) + pad;
        let btn = l.row_h;
        let height = (title_h + body + btn + pad).min(l.height);
        let top = l.height - height;
        let w = l.width.saturating_sub(pad * 2);
        let half = (w - l.pad) / 2;
        let by = top + height - pad - btn;
        (
            top,
            height,
            [
                (pad, by, half, btn),
                (pad + half + l.pad, by, w - half - l.pad, btn),
            ],
        )
    }

    /// The sheet's text: definition lines and the sentence's lines.
    fn word_sheet_text(&self, w: &gideon_core::SavedWord) -> (Vec<String>, Vec<String>) {
        let l = &self.layout;
        let px = l.text_px * 0.85;
        let max_w = l.width.saturating_sub(l.pad * 6);
        // Wrapped here without the dictionary, so drawing stays &self; the
        // gloss is what was kept at lookup time.
        let mut def = wrap_text(px, &w.gloss, max_w);
        def.truncate(3);
        let mut ctx = wrap_text(px, &format!("“{}”", w.context), max_w);
        ctx.truncate(3);
        ctx.push(format!("{}, chapter {}", w.title, w.chapter + 1));
        (def, ctx)
    }

    pub(super) fn overlay_word_sheet(&self, canvas: &mut RgbPage) {
        let Some(Sheet::SavedWord { word }) = &self.sheet else {
            return;
        };
        let l = &self.layout;
        let t = widgets::Theme::from_setting(&self.load_settings().color_profile);
        let (def, ctx) = self.word_sheet_text(word);
        let (top, height, buttons) = self.word_sheet_layout(def.len(), ctx.len());
        fill_rect_rgb(canvas, 0, top, l.width, height, [0xFF; 3]);
        fill_rect_rgb(canvas, 0, top, l.width, 3, [0x00; 3]);
        let mut p = Painted {
            ink: GrayPage::new_white(l.width, l.height),
            fills: Vec::new(),
        };
        let title_h = (l.text_px * 1.6) as u32;
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
            &word.word,
            l.width / 2,
            true,
        );
        let dpx = l.text_px * 0.85;
        let lh = (dpx * 1.3) as u32;
        let x = l.pad * 2;
        let mut y = top + title_h;
        for line in &def {
            draw_text(&mut p.ink, x, y, dpx, line, l.width - x * 2, false);
            y += lh;
        }
        y += l.pad;
        // The sentence, marked with the accent bar quotes carry.
        let bar = (l.pad / 2).max(3);
        p.fills.push(((x, y, bar, lh * ctx.len() as u32), t.accent));
        for (i, line) in ctx.iter().enumerate() {
            let style = gideon_render::text::TextStyle {
                italic: i + 1 < ctx.len(),
                ..Default::default()
            };
            gideon_render::text::draw_text_styled(
                &mut p.ink,
                x + bar * 3,
                y,
                dpx * if i + 1 < ctx.len() { 1.0 } else { 0.85 },
                line,
                l.width - x * 2 - bar * 3,
                style,
            );
            y += lh;
        }
        draw_button(&mut p, buttons[0], l.text_px * 0.9, "Remove", false, &t);
        draw_button(&mut p, buttons[1], l.text_px * 0.9, "Go to page", true, &t);
        for ((x, y, w, h), c) in &p.fills {
            fill_rect_rgb(canvas, *x, *y, *w, *h, *c);
        }
        widgets::overlay_ink(canvas, &p.ink);
    }

    pub(super) fn word_sheet_bounds(&self) -> Option<(u32, u32)> {
        let Some(Sheet::SavedWord { word }) = &self.sheet else {
            return None;
        };
        let (def, ctx) = self.word_sheet_text(word);
        let (top, height, _) = self.word_sheet_layout(def.len(), ctx.len());
        Some((top, height))
    }

    pub(super) fn tap_word_sheet(&mut self, x: u32, y: u32) -> Result<Flow> {
        let Some(Sheet::SavedWord { word }) = self.sheet.clone() else {
            return Ok(Flow::Continue);
        };
        let (def, ctx) = self.word_sheet_text(&word);
        let (top, _, buttons) = self.word_sheet_layout(def.len(), ctx.len());
        let close = |app: &mut Self| -> Result<Flow> {
            app.sheet = None;
            app.render_current(RefreshMode::Full)?;
            Ok(Flow::Continue)
        };
        if y < top {
            return close(self);
        }
        if hit(buttons[0], x, y) {
            // Remove: from the file and from the list on screen.
            let mut all = gideon_core::SavedWords::load(&self.books_dir);
            if let Some(i) = all.words.iter().position(|w| w.word == word.word) {
                all.remove(i);
                let _ = all.save(&self.books_dir);
            }
            if let Some(Screen::Words { entries, page, .. }) = self.stack.last_mut() {
                entries.retain(|w| w.word != word.word);
                let per = words_per_page(&self.layout);
                *page = (*page).min(entries.len().saturating_sub(1) / per.max(1));
            }
            return close(self);
        }
        if hit(buttons[1], x, y) {
            self.sheet = None;
            return self.go_to_word(&word).map(|_| Flow::Continue);
        }
        Ok(Flow::Continue)
    }

    /// Back into the book, at the paragraph the word was met in: the open
    /// reader when it is that book, else the book opened afresh.
    fn go_to_word(&mut self, w: &gideon_core::SavedWord) -> Result<()> {
        while matches!(self.stack.last(), Some(Screen::Words { .. })) && self.stack.len() > 1 {
            self.stack.pop();
        }
        let open_here = matches!(
            self.stack.last(),
            Some(Screen::BookReader { book, .. }) if book.relative_path == w.book
        );
        if !open_here {
            let book = gideon_core::books::scan(&self.books_dir)
                .unwrap_or_default()
                .into_iter()
                .find(|b| b.relative_path == w.book);
            match book {
                Some(book) => self.open_book_reader(book)?,
                None => {
                    return self.push(Screen::Message {
                        title: w.word.clone(),
                        body: format!("\"{}\" is no longer on this Kobo.", w.title),
                    })
                }
            }
        }
        self.jump_to_place(w.chapter, (w.block, 0))
    }
}

#[cfg(test)]
pub(super) fn row_h_for_tests(l: &UiLayout) -> u32 {
    word_row_h(l)
}

#[cfg(test)]
pub(super) fn tabs_for_tests(l: &UiLayout) -> [Rect; 2] {
    word_tabs(l)
}

#[cfg(test)]
impl<D: Display, I: InputSource, G: SourceGateway> UiApp<D, I, G> {
    /// The open word sheet's layout, as drawn.
    pub(super) fn word_sheet_layout_for_tests(&self) -> (u32, u32, [Rect; 2]) {
        let Some(Sheet::SavedWord { word }) = &self.sheet else {
            panic!("no word sheet open");
        };
        let (def, ctx) = self.word_sheet_text(word);
        self.word_sheet_layout(def.len(), ctx.len())
    }
}
