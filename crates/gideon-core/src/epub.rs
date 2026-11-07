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

/// A unit of text that starts on its own line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading(String),
    Paragraph(String),
    /// A picture lived here. The reader says so rather than silently
    /// dropping it: a light novel's illustrations are part of the book.
    Image,
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
        let chapter = html_to_chapter(&html);
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

/// Flatten one XHTML document to blocks: the text of block-level elements,
/// whitespace collapsed, head/style/script skipped, images marked.
pub fn html_to_chapter(html: &str) -> Chapter {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(html);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    reader.config_mut().expand_empty_elements = false;

    let mut blocks: Vec<Block> = Vec::new();
    let mut current = String::new();
    let mut heading_depth = 0usize;
    let mut skip_depth = 0usize;
    let mut title: Option<String> = None;

    fn flush(
        current: &mut String,
        heading: bool,
        blocks: &mut Vec<Block>,
        title: &mut Option<String>,
    ) {
        let text: String = current.split_whitespace().collect::<Vec<_>>().join(" ");
        current.clear();
        if text.is_empty() {
            return;
        }
        if heading {
            if title.is_none() {
                *title = Some(text.clone());
            }
            blocks.push(Block::Heading(text));
        } else {
            blocks.push(Block::Paragraph(text));
        }
    }

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.local_name();
                let name = name.as_ref();
                if matches!(name, b"head" | b"style" | b"script" | b"svg" | b"title") {
                    skip_depth += 1;
                    if name == b"svg" {
                        flush(&mut current, heading_depth > 0, &mut blocks, &mut title);
                        blocks.push(Block::Image);
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
                if name == b"img" || name == b"image" {
                    flush(&mut current, heading_depth > 0, &mut blocks, &mut title);
                    blocks.push(Block::Image);
                }
            }
            Ok(Event::Empty(e)) => {
                let name = e.local_name();
                let name = name.as_ref();
                if skip_depth > 0 {
                    continue;
                }
                if name == b"br" {
                    flush(&mut current, heading_depth > 0, &mut blocks, &mut title);
                } else if name == b"img" || name == b"image" {
                    flush(&mut current, heading_depth > 0, &mut blocks, &mut title);
                    blocks.push(Block::Image);
                } else if is_block(name) {
                    flush(&mut current, heading_depth > 0, &mut blocks, &mut title);
                }
            }
            Ok(Event::End(e)) => {
                let name = e.local_name();
                let name = name.as_ref();
                if matches!(name, b"head" | b"style" | b"script" | b"svg" | b"title") {
                    skip_depth = skip_depth.saturating_sub(1);
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
            }
            Ok(Event::Text(t)) => {
                if skip_depth > 0 {
                    continue;
                }
                let raw = String::from_utf8_lossy(&t);
                current.push_str(&decode_entities(&raw));
            }
            Ok(Event::CData(t)) => {
                if skip_depth == 0 {
                    current.push_str(&String::from_utf8_lossy(&t));
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
                // A no-break space is just a space to a flowed page.
                Block::Paragraph(
                    "All this happened, more or less. The war parts, anyway, are pretty much true."
                        .into()
                ),
                Block::Paragraph("Billy Pilgrim has come unstuck in time.".into()),
                Block::Image,
                Block::Paragraph("So it goes.".into()),
                Block::Paragraph("Second line — dash — too — & done.".into()),
            ]
        );
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
