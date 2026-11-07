//! An EPUB as text: what gideon's own book reader shows.
//!
//! Novels and light novels are paragraphs; the layout an EPUB carries in
//! its CSS is for a browser engine this device does not have. So the
//! reader takes the spine in order, reads each XHTML document, and keeps
//! only what a page of a novel needs — headings, paragraphs, and a marker
//! where an illustration was — then flows that text itself. No fonts
//! loaded, no CSS, no scripts: a 26 MB light novel full of pictures is a
//! few hundred kilobytes of text to the reader, which is why it opens in
//! under a second on the Kobo.

use std::io::{Read, Seek};
use std::path::Path;

use crate::{Error, Result};

/// A book flattened to its reading order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EpubText {
    pub title: Option<String>,
    pub author: Option<String>,
    pub chapters: Vec<Chapter>,
    /// The book's own table of contents (EPUB 3 nav, else EPUB 2 NCX),
    /// each entry pointing at the chapter it opens. Empty when the book
    /// has none; see [`EpubText::contents`] for what the reader shows.
    pub toc: Vec<TocEntry>,
}

/// One line of the table of contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocEntry {
    pub title: String,
    /// Index into [`EpubText::chapters`].
    pub chapter: usize,
}

/// Spine items this short (in blocks) that carry no heading are folded
/// into the chapter before them while it is still under this size. The
/// Internet Archive's generated EPUBs put every scanned page in its own
/// file — 163 "chapters" of a paragraph each — and a book reads as one
/// text, not as a page per chapter. A real chapter starts with a heading
/// and stays its own.
const FOLD_UNDER_BLOCKS: usize = 40;

/// One spine item with something to read in it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Chapter {
    /// The first heading in the document, if any.
    pub title: Option<String>,
    pub blocks: Vec<Block>,
}

/// A run of text in one style.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Span {
    pub text: String,
    pub italic: bool,
    pub bold: bool,
}

/// Styled text: runs of plain, italic and bold. Most paragraphs are one
/// plain run; emphasis is the exception that carries meaning (a thought, a
/// title, a stressed word), which is why it is kept.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rich {
    pub spans: Vec<Span>,
}

impl Rich {
    /// The text with its styling dropped.
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|s| s.text.is_empty())
    }

    /// Build from `(text, italic, bold)` runs.
    pub fn styled(runs: &[(&str, bool, bool)]) -> Rich {
        Rich {
            spans: runs
                .iter()
                .map(|(t, i, b)| Span {
                    text: t.to_string(),
                    italic: *i,
                    bold: *b,
                })
                .collect(),
        }
    }
}

impl From<&str> for Rich {
    fn from(text: &str) -> Self {
        Rich {
            spans: vec![Span {
                text: text.to_string(),
                ..Default::default()
            }],
        }
    }
}

impl From<String> for Rich {
    fn from(text: String) -> Self {
        Rich::from(text.as_str())
    }
}

/// A unit of text that starts on its own line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading(Rich),
    Paragraph(Rich),
    /// A picture: the zip entry it lives at, resolved from the chapter's
    /// own folder. A light novel's illustrations are part of the book; the
    /// reader draws them in place ([`read_entry`] fetches the bytes).
    Image(String),
}

impl Block {
    /// The block's text without styling; empty for a picture.
    pub fn text(&self) -> String {
        match self {
            Block::Heading(r) | Block::Paragraph(r) => r.text(),
            Block::Image(_) => String::new(),
        }
    }
}

impl EpubText {
    /// What the contents list shows: the book's own table of contents, or,
    /// when it has none, the chapters that have a heading, or failing that
    /// numbered chapters — there is always a way to jump.
    pub fn contents(&self) -> Vec<TocEntry> {
        if !self.toc.is_empty() {
            return self.toc.clone();
        }
        let titled: Vec<TocEntry> = self
            .chapters
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                c.title.as_ref().map(|t| TocEntry {
                    title: t.clone(),
                    chapter: i,
                })
            })
            .collect();
        if !titled.is_empty() {
            return titled;
        }
        (0..self.chapters.len())
            .map(|i| TocEntry {
                title: format!("Chapter {}", i + 1),
                chapter: i,
            })
            .collect()
    }

    /// Total paragraphs across the book, for a rough "n% read".
    pub fn block_count(&self) -> usize {
        self.chapters.iter().map(|c| c.blocks.len()).sum()
    }
}

/// Read the EPUB at `path` into text. Fails only when the file is not an
/// EPUB at all (no container, no OPF); a chapter that will not parse
/// yields what was read before the error rather than failing the book.
pub fn load(path: &Path) -> Result<EpubText> {
    let file = std::fs::File::open(path)?;
    let archive = zip::ZipArchive::new(std::io::BufReader::new(file))
        .map_err(|e| Error::InvalidUpload(format!("not a readable ZIP: {e}")))?;
    load_from(archive)
}

/// [`load`] for an archive already open, so tests can feed bytes.
pub fn load_from<R: Read + Seek>(mut archive: zip::ZipArchive<R>) -> Result<EpubText> {
    let read = |archive: &mut zip::ZipArchive<R>, name: &str| -> Option<String> {
        let candidates = [name.to_string(), percent_decode(name)];
        for candidate in candidates {
            if let Ok(mut entry) = archive.by_name(&candidate) {
                let mut s = String::new();
                if entry.read_to_string(&mut s).is_ok() {
                    return Some(s);
                }
            }
        }
        None
    };
    let container = read(&mut archive, "META-INF/container.xml")
        .ok_or_else(|| Error::InvalidUpload("no META-INF/container.xml".into()))?;
    let opf_path = crate::upload::opf_path_from_container(&container)
        .ok_or_else(|| Error::InvalidUpload("container.xml names no package".into()))?;
    let opf = read(&mut archive, &opf_path)
        .ok_or_else(|| Error::InvalidUpload(format!("package {opf_path} missing")))?;
    let meta = crate::upload::opf_meta(&opf);
    let package = parse_package(&opf);
    let opf_dir = match opf_path.rfind('/') {
        Some(i) => &opf_path[..=i],
        None => "",
    };
    let mut chapters: Vec<Chapter> = Vec::new();
    // Which chapter each spine document ended up in (folded ones share).
    let mut chapter_of: Vec<(String, usize)> = Vec::new();
    for href in package.spine_hrefs() {
        let full = normalize_path(&format!("{opf_dir}{href}"));
        let Some(html) = read(&mut archive, &full) else {
            continue;
        };
        let doc_dir = match full.rfind('/') {
            Some(i) => &full[..=i],
            None => "",
        };
        let chapter = html_to_chapter_in(&html, doc_dir);
        if chapter.blocks.is_empty() {
            continue;
        }
        let starts_with_heading = matches!(chapter.blocks.first(), Some(Block::Heading(_)));
        match chapters.last_mut() {
            Some(prev) if !starts_with_heading && prev.blocks.len() < FOLD_UNDER_BLOCKS => {
                prev.blocks.extend(chapter.blocks);
                if prev.title.is_none() {
                    prev.title = chapter.title;
                }
            }
            _ => chapters.push(chapter),
        }
        chapter_of.push((full, chapters.len() - 1));
    }
    let toc = load_toc(&mut archive, &read, &package, opf_dir, &chapter_of);
    Ok(EpubText {
        title: meta.title,
        author: meta.author,
        chapters,
        toc,
    })
}

/// The table of contents: the EPUB 3 nav document when the manifest marks
/// one, else the EPUB 2 NCX; entries resolved to chapters, those pointing
/// nowhere dropped, runs of one chapter collapsed to the first.
fn load_toc<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    read: &dyn Fn(&mut zip::ZipArchive<R>, &str) -> Option<String>,
    package: &Package,
    opf_dir: &str,
    chapter_of: &[(String, usize)],
) -> Vec<TocEntry> {
    let nav = package
        .manifest
        .iter()
        .find(|m| m.properties.split_whitespace().any(|p| p == "nav"))
        .map(|m| (m.href.clone(), true))
        .or_else(|| {
            package
                .manifest
                .iter()
                .find(|m| {
                    m.media.contains("dtbncx") || m.href.to_ascii_lowercase().ends_with(".ncx")
                })
                .map(|m| (m.href.clone(), false))
        });
    let Some((href, is_nav)) = nav else {
        return Vec::new();
    };
    let full = normalize_path(&format!("{opf_dir}{href}"));
    let Some(xml) = read(archive, &full) else {
        return Vec::new();
    };
    let doc_dir = match full.rfind('/') {
        Some(i) => &full[..=i],
        None => "",
    };
    let raw = if is_nav {
        parse_nav(&xml)
    } else {
        parse_ncx(&xml)
    };
    let mut out: Vec<TocEntry> = Vec::new();
    for (title, target) in raw {
        let target = normalize_path(&format!("{doc_dir}{target}"));
        let Some(&(_, chapter)) = chapter_of
            .iter()
            .find(|(h, _)| *h == target || percent_decode(h) == percent_decode(&target))
        else {
            continue;
        };
        if out.last().is_some_and(|e| e.chapter == chapter) {
            continue;
        }
        out.push(TocEntry { title, chapter });
    }
    out
}

/// `<nav epub:type="toc">` anchors, in document order: (label, href).
/// Nesting is flattened; a book's parts and chapters read as one list.
fn parse_nav(xml: &str) -> Vec<(String, String)> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut out = Vec::new();
    let mut in_toc = false;
    let mut nav_depth = 0usize;
    let mut href: Option<String> = None;
    let mut label = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"nav" => {
                    nav_depth += 1;
                    let kind = e.attributes().flatten().find_map(|a| {
                        (a.key.local_name().as_ref() == b"type")
                            .then(|| a.unescape_value().ok().map(|v| v.into_owned()))
                            .flatten()
                    });
                    // The first nav is the toc when none says so.
                    in_toc = kind
                        .as_deref()
                        .map(|k| k == "toc")
                        .unwrap_or(out.is_empty());
                }
                b"a" if in_toc => {
                    href = e.attributes().flatten().find_map(|a| {
                        (a.key.local_name().as_ref() == b"href")
                            .then(|| a.unescape_value().ok().map(|v| v.into_owned()))
                            .flatten()
                    });
                    label.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if href.is_some() {
                    label.push_str(&decode_entities(&String::from_utf8_lossy(&t)));
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"a" => {
                    if let Some(h) = href.take() {
                        let text: String = label.split_whitespace().collect::<Vec<_>>().join(" ");
                        if !text.is_empty() {
                            out.push((text, h));
                        }
                    }
                }
                b"nav" => {
                    nav_depth = nav_depth.saturating_sub(1);
                    if nav_depth == 0 {
                        in_toc = false;
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// NCX `<navPoint>`s in document order: (`<navLabel><text>`, `<content src>`).
fn parse_ncx(xml: &str) -> Vec<(String, String)> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut out = Vec::new();
    let mut in_text = false;
    let mut label = String::new();
    let content_src = |e: &quick_xml::events::BytesStart| -> Option<String> {
        e.attributes().flatten().find_map(|a| {
            (a.key.local_name().as_ref() == b"src")
                .then(|| a.unescape_value().ok().map(|v| v.into_owned()))
                .flatten()
        })
    };
    let mut take_content = |src: Option<String>, label: &mut String| {
        if let Some(src) = src {
            let text: String = label.split_whitespace().collect::<Vec<_>>().join(" ");
            if !text.is_empty() {
                out.push((text, src));
            }
        }
    };
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"navPoint" => label.clear(),
                b"text" => in_text = true,
                b"content" => take_content(content_src(&e), &mut label),
                _ => {}
            },
            Ok(Event::Empty(e)) => {
                if e.local_name().as_ref() == b"content" {
                    take_content(content_src(&e), &mut label);
                }
            }
            Ok(Event::Text(t)) => {
                if in_text {
                    label.push_str(&decode_entities(&String::from_utf8_lossy(&t)));
                }
            }
            Ok(Event::End(e)) => {
                if e.local_name().as_ref() == b"text" {
                    in_text = false;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// One `<item>` of the manifest.
#[derive(Debug, Default, Clone)]
struct ManifestItem {
    id: String,
    href: String,
    media: String,
    /// EPUB 3 `properties` ("nav", "cover-image", …).
    properties: String,
}

/// The manifest and spine of an OPF: which documents, in what order.
#[derive(Debug, Default)]
struct Package {
    manifest: Vec<ManifestItem>,
    spine: Vec<String>,
}

impl Package {
    /// The hrefs of the spine's XHTML documents, in reading order. A spine
    /// that names nothing readable (or no spine at all) falls back to every
    /// XHTML document in manifest order, so a sloppy EPUB still reads.
    fn spine_hrefs(&self) -> Vec<String> {
        let is_doc = |media: &str, href: &str| {
            media.contains("xhtml") || media.contains("html") || {
                let h = href.to_ascii_lowercase();
                h.ends_with(".xhtml") || h.ends_with(".html") || h.ends_with(".htm")
            }
        };
        let mut out: Vec<String> = self
            .spine
            .iter()
            .filter_map(|id| self.manifest.iter().find(|m| m.id == *id))
            .filter(|m| is_doc(&m.media, &m.href))
            .map(|m| m.href.clone())
            .collect();
        if out.is_empty() {
            out = self
                .manifest
                .iter()
                .filter(|m| is_doc(&m.media, &m.href))
                .map(|m| m.href.clone())
                .collect();
        }
        out
    }
}

fn parse_package(xml: &str) -> Package {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    reader.config_mut().check_end_names = false;
    let mut package = Package::default();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let attr = |name: &[u8]| -> Option<String> {
                    e.attributes().flatten().find_map(|a| {
                        (a.key.local_name().as_ref() == name)
                            .then(|| a.unescape_value().ok().map(|v| v.into_owned()))
                            .flatten()
                    })
                };
                match e.local_name().as_ref() {
                    b"item" => {
                        if let (Some(id), Some(href)) = (attr(b"id"), attr(b"href")) {
                            package.manifest.push(ManifestItem {
                                id,
                                href,
                                media: attr(b"media-type").unwrap_or_default(),
                                properties: attr(b"properties").unwrap_or_default(),
                            });
                        }
                    }
                    b"itemref" => {
                        if let Some(idref) = attr(b"idref") {
                            package.spine.push(idref);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    package
}

/// One file out of the EPUB, by zip entry name (as a [`Block::Image`]
/// carries it). Tries the name as given and percent-decoded.
pub fn read_entry(path: &Path, name: &str) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file))
        .map_err(|e| Error::InvalidUpload(format!("not a readable ZIP: {e}")))?;
    for candidate in [name.to_string(), percent_decode(name)] {
        if let Ok(mut entry) = archive.by_name(&candidate) {
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
    }
    Err(Error::InvalidUpload(format!("no entry {name} in the EPUB")))
}

/// Width and height of an encoded image, from its header alone.
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Flatten one XHTML document to blocks: the text of block-level elements,
/// whitespace collapsed, head/style/script skipped, images kept by path.
pub fn html_to_chapter(html: &str) -> Chapter {
    html_to_chapter_in(html, "")
}

/// [`html_to_chapter`] with the document's folder inside the zip, so an
/// image's relative `src` resolves to its entry.
pub fn html_to_chapter_in(html: &str, doc_dir: &str) -> Chapter {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(html);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    reader.config_mut().expand_empty_elements = false;

    let mut blocks: Vec<Block> = Vec::new();
    let mut current: Vec<Span> = Vec::new();
    let mut heading_depth = 0usize;
    let mut italic_depth = 0usize;
    let mut bold_depth = 0usize;
    let mut skip_depth = 0usize;
    let mut title: Option<String> = None;

    fn flush(
        current: &mut Vec<Span>,
        heading: bool,
        blocks: &mut Vec<Block>,
        title: &mut Option<String>,
    ) {
        let rich = collapse(std::mem::take(current));
        if rich.is_empty() {
            return;
        }
        if heading {
            if title.is_none() {
                *title = Some(rich.text());
            }
            blocks.push(Block::Heading(rich));
        } else {
            blocks.push(Block::Paragraph(rich));
        }
    }

    let image_of = |e: &quick_xml::events::BytesStart| -> Option<String> {
        let src = e.attributes().flatten().find_map(|a| {
            let key = a.key.local_name();
            (key.as_ref() == b"src" || key.as_ref() == b"href")
                .then(|| a.unescape_value().ok().map(|v| v.into_owned()))
                .flatten()
        })?;
        let src = src.trim();
        if src.is_empty() || src.starts_with("data:") || src.contains("://") {
            return None;
        }
        Some(normalize_path(&format!("{doc_dir}{src}")))
    };
    let mut svg_image: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.local_name();
                let name = name.as_ref();
                if skip_depth > 0 && name == b"image" {
                    // An <svg><image href=…/> wrapper around a picture: the
                    // picture is what we keep.
                    if svg_image.is_none() {
                        svg_image = image_of(&e);
                    }
                    continue;
                }
                if matches!(name, b"head" | b"style" | b"script" | b"svg" | b"title") {
                    skip_depth += 1;
                    if name == b"svg" {
                        flush(&mut current, heading_depth > 0, &mut blocks, &mut title);
                        svg_image = None;
                    }
                    continue;
                }
                if skip_depth > 0 {
                    continue;
                }
                if is_block(name) {
                    flush(&mut current, heading_depth > 0, &mut blocks, &mut title);
                }
                if is_heading(name) {
                    heading_depth += 1;
                }
                if is_italic(name) {
                    italic_depth += 1;
                }
                if is_bold(name) {
                    bold_depth += 1;
                }
                if name == b"img" || name == b"image" {
                    flush(&mut current, heading_depth > 0, &mut blocks, &mut title);
                    if let Some(src) = image_of(&e) {
                        blocks.push(Block::Image(src));
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                let name = e.local_name();
                let name = name.as_ref();
                if skip_depth > 0 {
                    if name == b"image" && svg_image.is_none() {
                        svg_image = image_of(&e);
                    }
                    continue;
                }
                if name == b"br" {
                    flush(&mut current, heading_depth > 0, &mut blocks, &mut title);
                } else if name == b"img" || name == b"image" {
                    flush(&mut current, heading_depth > 0, &mut blocks, &mut title);
                    if let Some(src) = image_of(&e) {
                        blocks.push(Block::Image(src));
                    }
                } else if is_block(name) {
                    flush(&mut current, heading_depth > 0, &mut blocks, &mut title);
                }
            }
            Ok(Event::End(e)) => {
                let name = e.local_name();
                let name = name.as_ref();
                if matches!(name, b"head" | b"style" | b"script" | b"svg" | b"title") {
                    skip_depth = skip_depth.saturating_sub(1);
                    if name == b"svg" {
                        if let Some(src) = svg_image.take() {
                            blocks.push(Block::Image(src));
                        }
                    }
                    continue;
                }
                if skip_depth > 0 {
                    continue;
                }
                if is_block(name) {
                    flush(&mut current, heading_depth > 0, &mut blocks, &mut title);
                }
                if is_heading(name) {
                    heading_depth = heading_depth.saturating_sub(1);
                }
                if is_italic(name) {
                    italic_depth = italic_depth.saturating_sub(1);
                }
                if is_bold(name) {
                    bold_depth = bold_depth.saturating_sub(1);
                }
            }
            Ok(Event::Text(t)) => {
                if skip_depth > 0 {
                    continue;
                }
                let raw = String::from_utf8_lossy(&t);
                current.push(Span {
                    text: decode_entities(&raw),
                    italic: italic_depth > 0,
                    bold: bold_depth > 0,
                });
            }
            Ok(Event::CData(t)) => {
                if skip_depth == 0 {
                    current.push(Span {
                        text: String::from_utf8_lossy(&t).into_owned(),
                        italic: italic_depth > 0,
                        bold: bold_depth > 0,
                    });
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    flush(&mut current, heading_depth > 0, &mut blocks, &mut title);
    Chapter { title, blocks }
}

fn is_heading(name: &[u8]) -> bool {
    matches!(name, b"h1" | b"h2" | b"h3" | b"h4" | b"h5" | b"h6")
}

fn is_italic(name: &[u8]) -> bool {
    matches!(name, b"i" | b"em" | b"cite" | b"dfn" | b"var")
}

fn is_bold(name: &[u8]) -> bool {
    matches!(name, b"b" | b"strong")
}

/// Collapse whitespace the way a browser would — runs become one space,
/// across span boundaries too — merge neighbouring runs of one style, and
/// trim the ends of the paragraph.
fn collapse(spans: Vec<Span>) -> Rich {
    let mut out: Vec<Span> = Vec::new();
    let mut pending_space = false;
    let mut at_start = true;
    for span in spans {
        for ch in span.text.chars() {
            if ch.is_whitespace() {
                pending_space = !at_start;
                continue;
            }
            let same_style = out
                .last()
                .is_some_and(|l| l.italic == span.italic && l.bold == span.bold);
            if !same_style {
                // A space between styles goes with the run before it.
                if pending_space {
                    if let Some(last) = out.last_mut() {
                        last.text.push(' ');
                    }
                    pending_space = false;
                }
                out.push(Span {
                    text: String::new(),
                    italic: span.italic,
                    bold: span.bold,
                });
            }
            let last = out.last_mut().expect("pushed above");
            if pending_space {
                last.text.push(' ');
                pending_space = false;
            }
            last.text.push(ch);
            at_start = false;
        }
    }
    Rich { spans: out }
}

fn is_block(name: &[u8]) -> bool {
    is_heading(name)
        || matches!(
            name,
            b"p" | b"div"
                | b"li"
                | b"blockquote"
                | b"tr"
                | b"td"
                | b"th"
                | b"section"
                | b"article"
                | b"aside"
                | b"header"
                | b"footer"
                | b"figure"
                | b"figcaption"
                | b"pre"
                | b"hr"
                | b"dt"
                | b"dd"
                | b"ul"
                | b"ol"
                | b"table"
                | b"body"
        )
}

/// XML's five entities, the HTML names a novel actually uses, and numeric
/// references. Anything unknown is kept as typed: a stray `&` in a book is
/// better than losing the sentence around it.
pub fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(end) = after.find(';').filter(|&e| e <= 12) else {
            out.push('&');
            rest = &rest[start + 1..];
            continue;
        };
        let name = &after[1..end];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "hellip" => Some('…'),
            "ldquo" => Some('“'),
            "rdquo" => Some('”'),
            "lsquo" => Some('‘'),
            "rsquo" => Some('’'),
            "copy" => Some('©'),
            "shy" => Some('\u{ad}'),
            "ensp" | "emsp" | "thinsp" => Some(' '),
            _ if name.starts_with("#x") || name.starts_with("#X") => {
                u32::from_str_radix(&name[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ if name.starts_with('#') => name[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    // Soft hyphens are for a justifier; a flowed plain-text page has none.
    out.replace('\u{ad}', "")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve `..` and `.` segments and strip a `#fragment`, so a spine href
/// of `../Text/ch1.xhtml#top` finds its zip entry.
fn normalize_path(p: &str) -> String {
    let p = p.split('#').next().unwrap_or(p);
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn epub(files: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in files {
                zip.start_file(*name, opts).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    const CONTAINER: &str = r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;

    const OPF: &str = r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0">
<metadata><dc:title>Slaughterhouse-Five</dc:title><dc:creator>Kurt Vonnegut</dc:creator></metadata>
<manifest>
<item id="css" href="style.css" media-type="text/css"/>
<item id="c1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/>
<item id="c2" href="Text/ch%202.xhtml" media-type="application/xhtml+xml"/>
<item id="pic" href="Images/p.jpg" media-type="image/jpeg"/>
</manifest>
<spine><itemref idref="c2"/><itemref idref="c1"/></spine>
</package>"#;

    const CH1: &str = r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>ignored</title><style>p { x: y }</style></head>
<body><h1>One</h1>
<p>All this happened, <i>more</i> or less.&nbsp;The war parts, anyway, are pretty much true.</p>
<p>
   Billy Pilgrim has come unstuck
   in time.
</p>
<div><img src="../Images/p.jpg" alt=""/></div>
<div><svg xmlns="http://www.w3.org/2000/svg"><image xlink:href="../Images/cover.jpg"/></svg></div>
<p>So it goes.<br/>Second line &mdash; dash &#8212; too &#x2014; &amp; done.</p>
</body></html>"#;

    const CH2: &str = r#"<html><body><h2>Two</h2><p>Listen:</p></body></html>"#;

    #[test]
    fn reads_the_spine_in_order_as_paragraphs() {
        let bytes = epub(&[
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", OPF),
            ("OEBPS/Text/ch1.xhtml", CH1),
            ("OEBPS/Text/ch 2.xhtml", CH2),
        ]);
        let book = load_from(zip::ZipArchive::new(Cursor::new(bytes)).unwrap()).unwrap();
        assert_eq!(book.title.as_deref(), Some("Slaughterhouse-Five"));
        assert_eq!(book.author.as_deref(), Some("Kurt Vonnegut"));
        assert_eq!(
            book.chapters.len(),
            2,
            "spine order, CSS and images skipped"
        );
        assert_eq!(book.chapters[0].title.as_deref(), Some("Two"));
        assert_eq!(
            book.chapters[0].blocks,
            [
                Block::Heading("Two".into()),
                Block::Paragraph("Listen:".into())
            ]
        );
        let one = &book.chapters[1];
        assert_eq!(one.title.as_deref(), Some("One"));
        assert_eq!(
            one.blocks,
            [
                Block::Heading("One".into()),
                // A no-break space is just a space to a flowed page; the
                // <i>more</i> is its own run.
                Block::Paragraph(Rich::styled(&[
                    ("All this happened, ", false, false),
                    ("more ", true, false),
                    (
                        "or less. The war parts, anyway, are pretty much true.",
                        false,
                        false
                    ),
                ])),
                Block::Paragraph("Billy Pilgrim has come unstuck in time.".into()),
                Block::Image("OEBPS/Images/p.jpg".into()),
                Block::Image("OEBPS/Images/cover.jpg".into()),
                Block::Paragraph("So it goes.".into()),
                Block::Paragraph("Second line — dash — too — & done.".into()),
            ]
        );
    }

    #[test]
    fn emphasis_becomes_styled_runs_with_browser_whitespace() {
        let ch = html_to_chapter(
            "<p>  So <i>it</i> <b>goes</b>.  <em>Thought.</em>\n <strong><i>Both</i></strong></p>",
        );
        assert_eq!(
            ch.blocks,
            [Block::Paragraph(Rich::styled(&[
                ("So ", false, false),
                ("it ", true, false),
                ("goes", false, true),
                (". ", false, false),
                ("Thought. ", true, false),
                ("Both", true, true),
            ]))]
        );
        assert_eq!(ch.blocks[0].text(), "So it goes. Thought. Both");
    }

    const NAV: &str = r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="Text/ch%202.xhtml">Two</a></li><li><a href="Text/ch1.xhtml#start">One &amp; only</a><ol><li><a href="Text/ch1.xhtml#later">deeper, same chapter</a></li></ol></li><li><a href="Text/missing.xhtml">Nowhere</a></li></ol></nav><nav epub:type="landmarks"><ol><li><a href="Text/ch1.xhtml">ignored</a></li></ol></nav></body></html>"#;

    #[test]
    fn the_nav_document_becomes_the_contents() {
        let opf = OPF.replace(
            "</manifest>",
            r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/></manifest>"#,
        );
        let bytes = epub(&[
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", &opf),
            ("OEBPS/nav.xhtml", NAV),
            ("OEBPS/Text/ch1.xhtml", CH1),
            ("OEBPS/Text/ch 2.xhtml", CH2),
        ]);
        let book = load_from(zip::ZipArchive::new(Cursor::new(bytes)).unwrap()).unwrap();
        let toc: Vec<(&str, usize)> = book
            .toc
            .iter()
            .map(|e| (e.title.as_str(), e.chapter))
            .collect();
        assert_eq!(
            toc,
            [("Two", 0), ("One & only", 1)],
            "resolved to chapters, same-chapter run collapsed, dead link dropped, landmarks ignored"
        );
        assert_eq!(book.contents(), book.toc);
    }

    #[test]
    fn an_ncx_is_read_when_there_is_no_nav() {
        let opf = OPF.replace(
            "</manifest>",
            r#"<item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/></manifest>"#,
        );
        let ncx = r#"<ncx><navMap><navPoint id="a"><navLabel><text>Chapter Two</text></navLabel><content src="Text/ch%202.xhtml"/></navPoint><navPoint id="b"><navLabel><text>Chapter One</text></navLabel><content src="Text/ch1.xhtml"/></navPoint></navMap></ncx>"#;
        let bytes = epub(&[
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", &opf),
            ("OEBPS/toc.ncx", ncx),
            ("OEBPS/Text/ch1.xhtml", CH1),
            ("OEBPS/Text/ch 2.xhtml", CH2),
        ]);
        let book = load_from(zip::ZipArchive::new(Cursor::new(bytes)).unwrap()).unwrap();
        let toc: Vec<(&str, usize)> = book
            .toc
            .iter()
            .map(|e| (e.title.as_str(), e.chapter))
            .collect();
        assert_eq!(toc, [("Chapter Two", 0), ("Chapter One", 1)]);
    }

    #[test]
    fn without_a_toc_the_contents_are_the_headings_or_numbers() {
        let bytes = epub(&[
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", OPF),
            ("OEBPS/Text/ch1.xhtml", CH1),
            ("OEBPS/Text/ch 2.xhtml", CH2),
        ]);
        let book = load_from(zip::ZipArchive::new(Cursor::new(bytes)).unwrap()).unwrap();
        assert!(book.toc.is_empty());
        let contents = book.contents();
        let titles: Vec<&str> = contents.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["Two", "One"]);
        let plain = EpubText {
            chapters: vec![Chapter::default(), Chapter::default()],
            ..Default::default()
        };
        let titles: Vec<String> = plain.contents().into_iter().map(|e| e.title).collect();
        assert_eq!(titles, ["Chapter 1", "Chapter 2"]);
    }

    #[test]
    fn page_per_file_scans_fold_into_chapters_but_headed_chapters_stay() {
        // 12 one-paragraph files with no heading, then a headed chapter,
        // then 2 more loose pages: 1 + 1 + (the 2 fold into the headed one).
        let mut files: Vec<(String, String)> = vec![(
            "META-INF/container.xml".into(),
            CONTAINER.replace("OEBPS/content.opf", "content.opf"),
        )];
        let mut manifest = String::new();
        let mut spine = String::new();
        let mut add = |name: &str, body: &str| {
            let id = name.replace('.', "_");
            manifest.push_str(&format!(
                r#"<item id="{id}" href="{name}" media-type="application/xhtml+xml"/>"#
            ));
            spine.push_str(&format!(r#"<itemref idref="{id}"/>"#));
            files.push((
                name.to_string(),
                format!("<html><body>{body}</body></html>"),
            ));
        };
        for i in 0..12 {
            add(&format!("p{i}.xhtml"), &format!("<p>Scanned page {i}.</p>"));
        }
        add("c.xhtml", "<h1>Real chapter</h1><p>Starts here.</p>");
        add("q1.xhtml", "<p>continues</p>");
        add("q2.xhtml", "<p>and ends.</p>");
        files.push((
            "content.opf".into(),
            format!("<package><manifest>{manifest}</manifest><spine>{spine}</spine></package>"),
        ));
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let bytes = epub(&refs);
        let book = load_from(zip::ZipArchive::new(Cursor::new(bytes)).unwrap()).unwrap();
        assert_eq!(book.chapters.len(), 2);
        assert_eq!(
            book.chapters[0].blocks.len(),
            12,
            "the loose pages are one chapter"
        );
        assert_eq!(book.chapters[0].title, None);
        assert_eq!(book.chapters[1].title.as_deref(), Some("Real chapter"));
        assert_eq!(
            book.chapters[1].blocks.len(),
            4,
            "the pages after it fold in"
        );
        let titles: Vec<String> = book.contents().into_iter().map(|e| e.title).collect();
        assert_eq!(titles, ["Real chapter"]);
    }

    #[test]
    fn a_book_without_a_spine_still_reads_its_documents() {
        let opf = r#"<package><manifest><item id="a" href="a.html" media-type="text/html"/></manifest></package>"#;
        let bytes = epub(&[
            (
                "META-INF/container.xml",
                CONTAINER
                    .replace("OEBPS/content.opf", "content.opf")
                    .as_str(),
            ),
            ("content.opf", opf),
            ("a.html", "<p>hello</p>"),
        ]);
        let book = load_from(zip::ZipArchive::new(Cursor::new(bytes)).unwrap()).unwrap();
        assert_eq!(book.chapters[0].blocks, [Block::Paragraph("hello".into())]);
    }

    #[test]
    fn not_an_epub_is_an_error_not_a_panic() {
        let bytes = epub(&[("hello.txt", "x")]);
        assert!(load_from(zip::ZipArchive::new(Cursor::new(bytes)).unwrap()).is_err());
    }

    #[test]
    fn an_image_entry_reads_back_and_measures() {
        let dir = tempfile::tempdir().unwrap();
        let png = {
            let img = image::GrayImage::from_pixel(7, 5, image::Luma([0u8]));
            let mut buf = Cursor::new(Vec::new());
            image::DynamicImage::ImageLuma8(img)
                .write_to(&mut buf, image::ImageFormat::Png)
                .unwrap();
            buf.into_inner()
        };
        let path = dir.path().join("b.epub");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("OEBPS/Images/p 1.png", opts).unwrap();
            zip.write_all(&png).unwrap();
            zip.finish().unwrap();
        }
        let bytes = read_entry(&path, "OEBPS/Images/p%201.png").unwrap();
        assert_eq!(bytes, png);
        assert_eq!(image_dimensions(&bytes), Some((7, 5)));
        assert!(read_entry(&path, "nope.png").is_err());
        assert_eq!(image_dimensions(b"not an image"), None);
    }

    #[test]
    fn entities_decode_and_unknown_ones_survive() {
        assert_eq!(
            decode_entities("a &amp; b &foo; &#65;&#x42; &rsquo;"),
            "a & b &foo; AB ’"
        );
        assert_eq!(decode_entities("no entities"), "no entities");
        assert_eq!(decode_entities("dangling & sign"), "dangling & sign");
    }

    #[test]
    fn paths_normalise() {
        assert_eq!(
            normalize_path("OEBPS/Text/../Text/ch1.xhtml#top"),
            "OEBPS/Text/ch1.xhtml"
        );
        assert_eq!(percent_decode("ch%202.xhtml"), "ch 2.xhtml");
        assert_eq!(percent_decode("100%"), "100%");
    }
}
