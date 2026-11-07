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
}

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
    let mut chapters = Vec::new();
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
        if !chapter.blocks.is_empty() {
            chapters.push(chapter);
        }
    }
    Ok(EpubText {
        title: meta.title,
        author: meta.author,
        chapters,
    })
}

/// The manifest and spine of an OPF: which documents, in what order.
#[derive(Debug, Default)]
struct Package {
    /// id → (href, media-type)
    manifest: Vec<(String, String, String)>,
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
            .filter_map(|id| self.manifest.iter().find(|(mid, _, _)| mid == id))
            .filter(|(_, href, media)| is_doc(media, href))
            .map(|(_, href, _)| href.clone())
            .collect();
        if out.is_empty() {
            out = self
                .manifest
                .iter()
                .filter(|(_, href, media)| is_doc(media, href))
                .map(|(_, href, _)| href.clone())
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
                            let media = attr(b"media-type").unwrap_or_default();
                            package.manifest.push((id, href, media));
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
