//! Highlighting in the book reader, as approved in design review (gallery
//! design `reader-highlights-fonts-words`), done the Kindle way:
//!
//! * **Hold a word, then drag.** The hold ([`UiEvent::Hold`], while the
//!   finger is still down) selects the word under it; the drag that follows
//!   extends the selection word by word; the lift opens the highlight
//!   sheet. A hold and lift without moving is the dictionary, as before,
//!   with a Highlight button on the definition.
//! * **Three colours**, drawn as blocks behind the words (black text stays
//!   black; on a grey panel the band is a light grey).
//! * **Tap a highlight** to change its colour, add a note, or remove it.
//! * **Reading menu › Highlights** lists them; a tap jumps there.
//!
//! A highlight is stored as words — (paragraph, word) to (paragraph, word)
//! — not pixels, so it survives any font or size change.
//!
//! Refresh, per docs/REFRESH.md: the selection as it grows is Partial (one
//! line or two change), with a Full every [`SELECTION_FULL_EVERY`] repaints
//! so a long drag leaves no ghosting; a sheet opening is Partial; a sheet
//! closing is Full.

use super::book_search::{draw_button, hit, Painted, Rect};
use super::*;

/// The light grey a highlight band is drawn in on the grey page. The colour
/// pass recolours exactly these pixels, so text over the band keeps its ink.
pub(super) const BAND_GREY: u8 = 0xD9;

/// Force a Full refresh this often while a selection is being dragged.
pub(super) const SELECTION_FULL_EVERY: u32 = 8;

/// A word's place in a chapter: (paragraph, word in it).
pub(super) type WordPos = (usize, usize);

/// The fill a highlight colour draws in.
pub(super) fn highlight_rgb(color: &str) -> [u8; 3] {
    match color {
        "blue" => [0xCF, 0xE0, 0xEE],
        "pink" => [0xF1, 0xD3, 0xD3],
        _ => [0xF3, 0xE3, 0xA6],
    }
}

/// The x-extent of each word on a laid-out line, in order, measured as the
/// page draws it. A word is a run of non-space characters, which may cross
/// a change of style ("<i>so</i>," is one word).
pub(super) fn line_words(px: f32, family: FontFamily, spans: &[BookSpan]) -> Vec<(u32, u32)> {
    let mut words = Vec::new();
    let mut open: Option<u32> = None;
    let mut cursor = 0u32;
    for span in spans {
        let mut last_end = cursor;
        for (i, c) in span.text.char_indices() {
            let x = cursor + measure_text_in(px, &span.text[..i], span.bold, family);
            if c.is_whitespace() {
                if let Some(start) = open.take() {
                    words.push((start, x));
                }
            } else if open.is_none() {
                open = Some(x);
            }
            last_end = x;
        }
        let _ = last_end;
        cursor += measure_text_in(px, &span.text, span.bold, family);
    }
    if let Some(start) = open {
        words.push((start, cursor));
    }
    words
}

/// The words of a paragraph, split the way the page wraps it.
pub(super) fn block_words(
    text: &gideon_core::EpubText,
    chapter: usize,
    block: usize,
) -> Vec<String> {
    use gideon_core::BookBlock;
    let Some(b) = text.chapters.get(chapter).and_then(|c| c.blocks.get(block)) else {
        return Vec::new();
    };
    let joined: String = match b {
        BookBlock::Heading(r) | BookBlock::Paragraph(r) => {
            r.spans.iter().map(|s| s.text.as_str()).collect()
        }
        BookBlock::Image(_) => return Vec::new(),
    };
    joined
        .split([' ', '\u{a0}'])
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// The words from `a` to `b` (inclusive) of a chapter, joined with spaces.
pub(super) fn words_between(
    text: &gideon_core::EpubText,
    chapter: usize,
    a: WordPos,
    b: WordPos,
) -> String {
    let mut out: Vec<String> = Vec::new();
    for block in a.0..=b.0 {
        let words = block_words(text, chapter, block);
        let from = if block == a.0 { a.1 } else { 0 };
        let to = if block == b.0 {
            b.1.min(words.len().saturating_sub(1))
        } else {
            words.len().saturating_sub(1)
        };
        if words.is_empty() || from > to {
            continue;
        }
        out.extend(words[from..=to].iter().cloned());
    }
    out.join(" ")
}

/// What a band on the page is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Mark {
    /// A saved highlight, by colour name.
    Highlight(String),
    /// The selection being made.
    Selected,
}

/// "She had promised herself…" — the start of a passage, for a title.
fn snippet(s: &str, words: usize) -> String {
    let w: Vec<&str> = s.split_whitespace().collect();
    if w.len() <= words {
        format!("“{}”", w.join(" "))
    } else {
        format!("“{}…”", w[..words].join(" "))
    }
}

/// Where a word on the page is, from the same geometry the page draws with.
pub(super) struct PageWord {
    pub pos: WordPos,
    pub rect: Rect,
}

impl<D: Display, I: InputSource, G: SourceGateway> UiApp<D, I, G> {
    /// Every word on the page on screen, with its box.
    pub(super) fn page_words(&self) -> Vec<PageWord> {
        let Some(Screen::BookReader { pages, page, .. }) = self.stack.last() else {
            return Vec::new();
        };
        let (px, margin, _, top, line_h, _) = self.book_page_metrics();
        let family = self.book_family();
        let mut out = Vec::new();
        let mut y = top;
        for line in pages.get(*page).map(|p| p.as_slice()).unwrap_or(&[]) {
            let rows = line.image.as_ref().map_or(1, |img| img.rows as u32);
            if line.image.is_none() && !line.text.is_empty() {
                for (i, (x0, x1)) in line_words(px, family, &line.spans).into_iter().enumerate() {
                    out.push(PageWord {
                        pos: (line.block, line.word0 + i),
                        rect: (margin + x0, y, x1.saturating_sub(x0).max(1), line_h),
                    });
                }
            }
            y += rows * line_h;
        }
        out
    }

    /// The word under a point. With `snap`, a point beside the text on a
    /// line (or between two words) takes the nearest word on that line, so
    /// a drag that runs past the end of a line still selects to its end.
    pub(super) fn word_pos_at(&self, x: u32, y: u32, snap: bool) -> Option<WordPos> {
        let words = self.page_words();
        if let Some(w) = words.iter().find(|w| hit(w.rect, x, y)) {
            return Some(w.pos);
        }
        if !snap {
            return None;
        }
        words
            .iter()
            .filter(|w| y >= w.rect.1 && y < w.rect.1 + w.rect.3)
            .min_by_key(|w| {
                let (x0, x1) = (w.rect.0, w.rect.0 + w.rect.2);
                if x < x0 {
                    x0 - x
                } else {
                    x.saturating_sub(x1)
                }
            })
            .map(|w| w.pos)
    }

    /// The open book's path key and chapter, when the reader is up.
    fn reader_book(&self) -> Option<(String, usize)> {
        match self.stack.last() {
            Some(Screen::BookReader { book, chapter, .. }) => {
                Some((book.relative_path.clone(), *chapter))
            }
            _ => None,
        }
    }

    /// The bands this page carries: saved highlights and the selection,
    /// one band per run of marked words on a line (spaces between them
    /// included).
    pub(super) fn page_marks(&self) -> Vec<(Rect, Mark)> {
        let Some((key, chapter)) = self.reader_book() else {
            return Vec::new();
        };
        let highlights = gideon_core::BookPlaces::load(&self.books_dir)
            .highlights(&key)
            .to_vec();
        let selection = self
            .selection
            .map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
        if highlights.is_empty() && selection.is_none() {
            return Vec::new();
        }
        let mark_of = |pos: WordPos| -> Option<Mark> {
            if let Some((a, b)) = selection {
                if a <= pos && pos <= b {
                    return Some(Mark::Selected);
                }
            }
            highlights
                .iter()
                .find(|h| h.covers(chapter, pos.0, pos.1))
                .map(|h| Mark::Highlight(h.color.clone()))
        };
        let mut out: Vec<(Rect, Mark)> = Vec::new();
        let pad = (self.layout.pad / 3).max(2);
        for w in self.page_words() {
            let Some(mark) = mark_of(w.pos) else {
                continue;
            };
            // The band covers the text, a little above and below it.
            let px = self.book_page_metrics().0;
            let band = (
                w.rect.0,
                w.rect.1.saturating_sub(pad),
                w.rect.2,
                (px * 1.25) as u32 + pad * 2,
            );
            match out.last_mut() {
                // The next word on the same line with the same mark: one
                // band through the space between them.
                Some((r, m)) if *m == mark && r.1 == band.1 => {
                    r.2 = (band.0 + band.2).saturating_sub(r.0);
                }
                _ => out.push((band, mark)),
            }
        }
        out
    }

    /// Paint the page's highlight bands into the grey page, before its text.
    pub(super) fn paint_highlight_bands(&self, canvas: &mut GrayPage, marks: &[(Rect, Mark)]) {
        for (r, m) in marks {
            if matches!(m, Mark::Highlight(_)) {
                super::book_search::fill_gray(canvas, *r, BAND_GREY);
            }
        }
    }

    /// Invert the selection over the drawn text: white words on black, the
    /// way an e-reader shows what is selected.
    pub(super) fn paint_selection(canvas: &mut GrayPage, marks: &[(Rect, Mark)]) {
        for (r, m) in marks {
            if *m != Mark::Selected {
                continue;
            }
            for y in r.1..(r.1 + r.3).min(canvas.height) {
                for x in r.0..(r.0 + r.2).min(canvas.width) {
                    let i = (y * canvas.width + x) as usize;
                    canvas.pixels[i] = 255 - canvas.pixels[i];
                }
            }
        }
    }

    /// The reader page in colour, when it carries highlights a colour panel
    /// can show: the grey page, its bands recoloured. Night mode keeps the
    /// grey page (colour bands on black read badly).
    pub(super) fn compose_highlighted_page(&self) -> Result<Option<RgbPage>> {
        if self.load_settings().book_night {
            return Ok(None);
        }
        let marks = self.page_marks();
        if !marks.iter().any(|(_, m)| matches!(m, Mark::Highlight(_))) {
            return Ok(None);
        }
        let gray = self.compose_current()?;
        let mut rgb = RgbPage::from_gray(&gray);
        for (r, m) in &marks {
            let Mark::Highlight(color) = m else {
                continue;
            };
            let c = highlight_rgb(color);
            for y in r.1..(r.1 + r.3).min(gray.height) {
                for x in r.0..(r.0 + r.2).min(gray.width) {
                    if gray.pixels[(y * gray.width + x) as usize] == BAND_GREY {
                        let i = ((y * gray.width + x) * 3) as usize;
                        rgb.pixels[i..i + 3].copy_from_slice(&c);
                    }
                }
            }
        }
        Ok(Some(rgb))
    }

    // --- the gesture -----------------------------------------------------

    /// A finger held on a word: select it. Elsewhere on the page the hold
    /// does nothing, and the long press at lift marks the page as before.
    pub(super) fn hold_in_reader(&mut self, x: u32, y: u32) -> Result<Flow> {
        if !matches!(self.stack.last(), Some(Screen::BookReader { .. })) || self.sheet.is_some() {
            return Ok(Flow::Continue);
        }
        if let Some(pos) = self.word_pos_at(x, y, false) {
            self.selection = Some((pos, pos));
            self.selection_paints = 0;
            // One word changes: Partial.
            self.render_current(RefreshMode::Partial)?;
        }
        Ok(Flow::Continue)
    }

    /// A drag while a selection is being made: extend it to the word under
    /// the finger; the lift finishes it. Returns whether it was handled.
    pub(super) fn drag_selection(&mut self, x1: u32, y1: u32, released: bool) -> Result<bool> {
        let Some((anchor, focus)) = self.selection else {
            return Ok(false);
        };
        if self.sheet.is_some() {
            return Ok(false);
        }
        let to = self.word_pos_at(x1, y1, true).unwrap_or(focus);
        if to != focus {
            self.selection = Some((anchor, to));
            if !released {
                self.selection_paints += 1;
                let mode = if self.selection_paints.is_multiple_of(SELECTION_FULL_EVERY) {
                    RefreshMode::Full
                } else {
                    RefreshMode::Partial
                };
                self.render_current(mode)?;
            }
        }
        if released {
            self.finish_selection()?;
        }
        Ok(true)
    }

    /// The finger lifted: one word is the dictionary (with Highlight
    /// offered); more is the highlight sheet. The selection stays drawn
    /// under either sheet until it closes.
    pub(super) fn finish_selection(&mut self) -> Result<Flow> {
        let Some((a, b)) = self.selection else {
            return Ok(Flow::Continue);
        };
        if a == b {
            let word = match self.stack.last() {
                Some(Screen::BookReader { text, chapter, .. }) => {
                    words_between(text, *chapter, a, a)
                }
                _ => String::new(),
            };
            if word.is_empty() {
                self.selection = None;
                return self
                    .render_current(RefreshMode::Full)
                    .map(|_| Flow::Continue);
            }
            return self.look_up(&word);
        }
        self.sheet = Some(Sheet::Highlight { existing: None });
        self.render_current(RefreshMode::Partial)?;
        Ok(Flow::Continue)
    }

    /// A tap on the page that lands on a highlight opens its sheet.
    pub(super) fn tap_on_highlight(&mut self, x: u32, y: u32) -> Result<bool> {
        let Some((key, chapter)) = self.reader_book() else {
            return Ok(false);
        };
        let Some(pos) = self.word_pos_at(x, y, false) else {
            return Ok(false);
        };
        let places = gideon_core::BookPlaces::load(&self.books_dir);
        let Some(i) = places
            .highlights(&key)
            .iter()
            .position(|h| h.covers(chapter, pos.0, pos.1))
        else {
            return Ok(false);
        };
        self.sheet = Some(Sheet::Highlight { existing: Some(i) });
        self.render_current(RefreshMode::Partial)?;
        Ok(true)
    }

    /// Save the selection as a highlight in `color`; returns its index.
    pub(super) fn save_selection(&mut self, color: &str) -> Option<usize> {
        let (a, b) = self
            .selection
            .map(|(a, b)| if a <= b { (a, b) } else { (b, a) })?;
        let (key, chapter) = self.reader_book()?;
        let text = match self.stack.last() {
            Some(Screen::BookReader { text, .. }) => words_between(text, chapter, a, b),
            _ => String::new(),
        };
        let mut places = gideon_core::BookPlaces::load(&self.books_dir);
        let i = places.add_highlight(
            &key,
            gideon_core::Highlight {
                chapter,
                start: a,
                end: b,
                color: color.to_string(),
                note: String::new(),
                text,
                created_at: now_secs(),
            },
        );
        let _ = places.save(&self.books_dir);
        Some(i)
    }

    // --- the sheet -------------------------------------------------------

    pub(super) fn highlight_sheet_layout(&self) -> (u32, u32, [Rect; 3], [Rect; 2]) {
        let l = &self.layout;
        let pad = l.pad * 2;
        let title_h = (l.text_px * 1.6) as u32;
        let btn = l.row_h;
        let gap = l.pad;
        let height = (title_h + btn + gap + btn + pad).min(l.height);
        let top = l.height - height;
        let w = l.width.saturating_sub(pad * 2);
        let third = (w - gap * 2) / 3;
        let y1 = top + title_h;
        let colors = [
            (pad, y1, third, btn),
            (pad + third + gap, y1, third, btn),
            (pad + (third + gap) * 2, y1, w - (third + gap) * 2, btn),
        ];
        let half = (w - gap) / 2;
        let y2 = y1 + btn + gap;
        let actions = [
            (pad, y2, half, btn),
            (pad + half + gap, y2, w - half - gap, btn),
        ];
        (top, height, colors, actions)
    }

    /// The highlight in question: the one tapped, or the selection's words.
    fn highlight_subject(
        &self,
        existing: Option<usize>,
    ) -> (String, Option<gideon_core::Highlight>) {
        let Some((key, chapter)) = self.reader_book() else {
            return (String::new(), None);
        };
        if let Some(i) = existing {
            let h = gideon_core::BookPlaces::load(&self.books_dir)
                .highlights(&key)
                .get(i)
                .cloned();
            return (h.as_ref().map(|h| h.text.clone()).unwrap_or_default(), h);
        }
        let text = match (self.selection, self.stack.last()) {
            (Some((a, b)), Some(Screen::BookReader { text, .. })) => {
                let (a, b) = if a <= b { (a, b) } else { (b, a) };
                words_between(text, chapter, a, b)
            }
            _ => String::new(),
        };
        (text, None)
    }

    pub(super) fn overlay_highlight_sheet(&self, canvas: &mut RgbPage, existing: Option<usize>) {
        let l = &self.layout;
        let t = widgets::Theme::from_setting(&self.load_settings().color_profile);
        let (top, height, colors, actions) = self.highlight_sheet_layout();
        let (text, current) = self.highlight_subject(existing);
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
        let title = match &current {
            Some(h) if !h.note.is_empty() => format!("{} · {}", snippet(&text, 4), h.note),
            _ => snippet(&text, 4),
        };
        draw_text(
            &mut p.ink,
            l.pad * 2 + l.pad / 2,
            top + title_h.saturating_sub(px as u32 + 4) / 2,
            px,
            &title,
            l.width - l.pad * 5,
            true,
        );
        let bpx = l.text_px * 0.85;
        for (r, name) in colors.iter().zip(gideon_core::HIGHLIGHT_COLORS) {
            let on = current.as_ref().is_some_and(|h| h.color == name);
            let label = title_case(name);
            draw_button(&mut p, *r, bpx, "", on, &t);
            // A swatch of the colour, then its name.
            let sw = (bpx * 0.9) as u32;
            let lw = measure_text(bpx, &label, true);
            let x0 = r.0 + r.2.saturating_sub(sw + l.pad + lw) / 2;
            let sy = r.1 + r.3.saturating_sub(sw) / 2;
            p.fills.push(((x0, sy, sw, sw), highlight_rgb(name)));
            super::book_search::outline(&mut p.ink, (x0, sy, sw, sw), 1, 0x00);
            draw_text(
                &mut p.ink,
                x0 + sw + l.pad,
                r.1 + r.3.saturating_sub(bpx as u32 + 4) / 2,
                bpx,
                &label,
                lw + 1,
                true,
            );
        }
        let (left, right) = match &current {
            Some(h) if !h.note.is_empty() => ("Edit note", "Remove"),
            Some(_) => ("Add a note", "Remove"),
            None => ("Define", "Add a note"),
        };
        draw_button(&mut p, actions[0], bpx, left, false, &t);
        draw_button(&mut p, actions[1], bpx, right, false, &t);
        for ((x, y, w, h), c) in &p.fills {
            fill_rect_rgb(canvas, *x, *y, *w, *h, *c);
        }
        widgets::overlay_ink(canvas, &p.ink);
    }

    fn close_highlight_sheet(&mut self) -> Result<Flow> {
        self.sheet = None;
        self.selection = None;
        // The sheet held part of the page stale, and the page's bands may
        // have changed: Full.
        self.render_current(RefreshMode::Full)?;
        Ok(Flow::Continue)
    }

    pub(super) fn tap_highlight_sheet(
        &mut self,
        x: u32,
        y: u32,
        existing: Option<usize>,
    ) -> Result<Flow> {
        let (top, _, colors, actions) = self.highlight_sheet_layout();
        if y < top {
            return self.close_highlight_sheet();
        }
        let Some((key, _)) = self.reader_book() else {
            return self.close_highlight_sheet();
        };
        if let Some(c) = colors.iter().position(|r| hit(*r, x, y)) {
            let color = gideon_core::HIGHLIGHT_COLORS[c];
            match existing {
                Some(i) => {
                    let mut places = gideon_core::BookPlaces::load(&self.books_dir);
                    places.edit_highlight(&key, i, |h| h.color = color.to_string());
                    let _ = places.save(&self.books_dir);
                }
                None => {
                    self.save_selection(color);
                }
            }
            return self.close_highlight_sheet();
        }
        if hit(actions[0], x, y) {
            return match existing {
                // Define: the first word of the selection.
                None => {
                    let word = self
                        .highlight_subject(None)
                        .0
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    self.sheet = None;
                    self.look_up(&word)
                }
                Some(i) => self.open_highlight_note(i),
            };
        }
        if hit(actions[1], x, y) {
            return match existing {
                // Add a note: keep it (yellow, the first colour), then type.
                None => match self.save_selection(gideon_core::HIGHLIGHT_COLORS[0]) {
                    Some(i) => self.open_highlight_note(i),
                    None => self.close_highlight_sheet(),
                },
                Some(i) => {
                    let mut places = gideon_core::BookPlaces::load(&self.books_dir);
                    places.remove_highlight(&key, i);
                    let _ = places.save(&self.books_dir);
                    self.close_highlight_sheet()
                }
            };
        }
        Ok(Flow::Continue)
    }

    /// The keyboard, for a highlight's note.
    fn open_highlight_note(&mut self, index: usize) -> Result<Flow> {
        let Some((key, _)) = self.reader_book() else {
            return Ok(Flow::Continue);
        };
        let note = gideon_core::BookPlaces::load(&self.books_dir)
            .highlights(&key)
            .get(index)
            .map(|h| h.note.clone())
            .unwrap_or_default();
        self.sheet = None;
        self.selection = None;
        self.keyboard_paints = 0;
        self.keyboard_shift = false;
        self.push(Screen::HighlightNote {
            book: key,
            index,
            note,
        })?;
        Ok(Flow::Continue)
    }

    /// A key on the note keyboard: edit, or (the action key) save and go
    /// back to the page.
    pub(super) fn tap_highlight_note(
        &mut self,
        book: &str,
        index: usize,
        note: &str,
        x: u32,
        y: u32,
    ) -> Result<Flow> {
        let key = self.layout.key_at(x, y);
        if key == Some(Key::Search) {
            let mut places = gideon_core::BookPlaces::load(&self.books_dir);
            let text = note.trim().to_string();
            places.edit_highlight(book, index, |h| h.note = text);
            let _ = places.save(&self.books_dir);
            return self.pop();
        }
        if self.toggle_shift_if_pressed(key)? {
            return Ok(Flow::Continue);
        }
        if let Some(q) = key.and_then(|k| apply_key_edit(note, k, self.keyboard_shift)) {
            if let Some(Screen::HighlightNote { note, .. }) = self.stack.last_mut() {
                *note = q;
            }
            self.keyboard_repaint()?;
        }
        Ok(Flow::Continue)
    }

    // --- the list --------------------------------------------------------

    /// The open book's highlights, over the reader; none says how to make
    /// one.
    pub(super) fn open_book_highlights(&mut self) -> Result<()> {
        let Some((key, _)) = self.reader_book() else {
            return Ok(());
        };
        let entries = gideon_core::BookPlaces::load(&self.books_dir)
            .highlights(&key)
            .to_vec();
        if entries.is_empty() {
            return self.push(Screen::Message {
                title: "Highlights".to_string(),
                body: "No highlights yet.\nHold a word, then drag across the text.".to_string(),
            });
        }
        self.push(Screen::BookHighlights { entries, page: 0 })
    }
}

/// Seconds since the epoch, for a highlight's date.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
