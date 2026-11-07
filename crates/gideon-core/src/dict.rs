//! Offline dictionaries in the StarDict format, for the book reader's
//! long-press lookup.
//!
//! A dictionary is three files sharing a stem: `name.ifo` (a header),
//! `name.idx` (sorted headwords, each followed by a big-endian offset and
//! size into the data file) and `name.dict` or `name.dict.dz` (the
//! definitions; `.dz` is dictzip, a gzip whose `RA` extra field lists
//! independently-inflatable chunks, so one definition costs one chunk
//! rather than the whole file). Only the plain-text and HTML-ish entry
//! kinds are shown; markup is stripped to text.
//!
//! The index is read once into memory (a few MB for an English
//! dictionary) and searched by binary search on the lowercased headword,
//! the order StarDict writes. Lookups try the word as pressed, then a few
//! common English inflections, so "ran", "running" and "runs" all find
//! "run".

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::Error;

/// One installed dictionary.
#[derive(Debug)]
pub struct Dictionary {
    /// Its display name, from the `.ifo`'s `bookname`.
    pub name: String,
    /// Headwords with their (offset, size) in the data file, sorted the
    /// way StarDict sorts: case-insensitively, then by bytes.
    entries: Vec<(String, u32, u32)>,
    data: DataFile,
    /// Entry kinds, from `sametypesequence`; empty when each entry says.
    types: String,
}

#[derive(Debug)]
enum DataFile {
    Plain(PathBuf),
    /// A dictzip: the chunk table and where the chunks start.
    Dz {
        path: PathBuf,
        chunk_len: u64,
        /// Compressed size of each chunk.
        chunk_sizes: Vec<u16>,
        data_start: u64,
    },
}

/// A hit: the headword as the dictionary spells it, and its definition as
/// plain text, one sense per line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub word: String,
    pub text: String,
}

impl Dictionary {
    /// Open the dictionary whose `.ifo` is at `ifo`.
    pub fn open(ifo: &Path) -> Result<Self, Error> {
        let header = std::fs::read_to_string(ifo)?;
        let mut fields = BTreeMap::new();
        for line in header.lines().skip(1) {
            if let Some((k, v)) = line.split_once('=') {
                fields.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        let stem = ifo.with_extension("");
        let name = fields.get("bookname").cloned().unwrap_or_else(|| {
            stem.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
        let types = fields.get("sametypesequence").cloned().unwrap_or_default();
        if fields.get("idxoffsetbits").is_some_and(|b| b == "64") {
            return Err(Error::Dictionary(format!(
                "{}: 64-bit StarDict indexes are not supported",
                ifo.display()
            )));
        }
        let idx_path = stem.with_extension("idx");
        let idx = match std::fs::read(&idx_path) {
            Ok(bytes) => bytes,
            Err(_) => {
                let gz = std::fs::read(stem.with_extension("idx.gz"))?;
                let mut out = Vec::new();
                flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut out)?;
                out
            }
        };
        let entries = parse_idx(&idx)?;
        let plain = stem.with_extension("dict");
        let data = if plain.exists() {
            DataFile::Plain(plain)
        } else {
            let path = stem.with_extension("dict.dz");
            let mut file = File::open(&path)?;
            let (chunk_len, chunk_sizes, data_start) = read_dictzip_header(&mut file)?;
            DataFile::Dz {
                path,
                chunk_len,
                chunk_sizes,
                data_start,
            }
        };
        Ok(Self {
            name,
            entries,
            data,
            types,
        })
    }

    /// Every dictionary under `dir`: each `*.ifo` in it or one folder down,
    /// which is how a downloaded dictionary unpacks.
    pub fn installed(dir: &Path) -> Vec<Dictionary> {
        let mut ifos = Vec::new();
        let Ok(top) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        for entry in top.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "ifo") {
                ifos.push(path);
            } else if path.is_dir() {
                if let Ok(inner) = std::fs::read_dir(&path) {
                    ifos.extend(
                        inner
                            .flatten()
                            .map(|e| e.path())
                            .filter(|p| p.extension().is_some_and(|e| e == "ifo")),
                    );
                }
            }
        }
        ifos.sort();
        ifos.iter()
            .filter_map(|ifo| match Dictionary::open(ifo) {
                Ok(d) => Some(d),
                Err(e) => {
                    eprintln!("gideon: skipping dictionary {}: {e}", ifo.display());
                    None
                }
            })
            .collect()
    }

    /// How many headwords the dictionary has.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look a word up as pressed on the page: punctuation around it is
    /// dropped, case is ignored, and common inflections are tried when the
    /// exact form is not a headword.
    pub fn lookup(&mut self, word: &str) -> Option<Definition> {
        let clean = trim_word(word);
        if clean.is_empty() {
            return None;
        }
        for candidate in candidates(&clean) {
            if let Some(hit) = self.exact(&candidate) {
                return Some(hit);
            }
        }
        None
    }

    /// The entry whose headword equals `word` ignoring case.
    fn exact(&mut self, word: &str) -> Option<Definition> {
        let key = word.to_lowercase();
        let i = self
            .entries
            .partition_point(|(w, _, _)| w.to_lowercase() < key);
        let (head, off, size) = self
            .entries
            .get(i)
            .filter(|(w, _, _)| w.eq_ignore_ascii_case(word) || w.to_lowercase() == key)?
            .clone();
        let raw = self.read(off as u64, size as usize).ok()?;
        let text = render_entry(&raw, &self.types);
        (!text.trim().is_empty()).then_some(Definition { word: head, text })
    }

    fn read(&self, off: u64, size: usize) -> Result<Vec<u8>, Error> {
        match &self.data {
            DataFile::Plain(path) => {
                let mut f = File::open(path)?;
                f.seek(SeekFrom::Start(off))?;
                let mut buf = vec![0; size];
                f.read_exact(&mut buf)?;
                Ok(buf)
            }
            DataFile::Dz {
                path,
                chunk_len,
                chunk_sizes,
                data_start,
            } => {
                let first = (off / chunk_len) as usize;
                let last = ((off + size as u64).saturating_sub(1) / chunk_len) as usize;
                let mut f = File::open(path)?;
                let mut out = Vec::with_capacity(size + *chunk_len as usize);
                for c in first..=last {
                    let Some(&csize) = chunk_sizes.get(c) else {
                        return Err(Error::Dictionary(
                            "dictzip: entry past the last chunk".into(),
                        ));
                    };
                    let start: u64 =
                        *data_start + chunk_sizes[..c].iter().map(|&s| s as u64).sum::<u64>();
                    f.seek(SeekFrom::Start(start))?;
                    let mut comp = vec![0; csize as usize];
                    f.read_exact(&mut comp)?;
                    let mut dec = flate2::read::DeflateDecoder::new(&comp[..]);
                    let mut plain = Vec::with_capacity(*chunk_len as usize);
                    // A chunk ends in a full flush, not a final block, so the
                    // decoder reports an unexpected end: everything before
                    // it is the chunk.
                    let _ = dec.read_to_end(&mut plain);
                    out.extend_from_slice(&plain);
                }
                let skip = (off - first as u64 * chunk_len) as usize;
                if out.len() < skip + size {
                    return Err(Error::Dictionary(
                        "dictzip: chunk shorter than its entry".into(),
                    ));
                }
                Ok(out[skip..skip + size].to_vec())
            }
        }
    }
}

/// Headwords from a `.idx`: NUL-terminated UTF-8, then big-endian u32
/// offset and size.
fn parse_idx(idx: &[u8]) -> Result<Vec<(String, u32, u32)>, Error> {
    let mut entries = Vec::new();
    let mut at = 0;
    while at < idx.len() {
        let end = idx[at..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::Dictionary("StarDict index: unterminated headword".into()))?;
        let word = String::from_utf8_lossy(&idx[at..at + end]).into_owned();
        let rest = &idx[at + end + 1..];
        if rest.len() < 8 {
            return Err(Error::Dictionary("StarDict index: truncated entry".into()));
        }
        let off = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
        let size = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]);
        entries.push((word, off, size));
        at += end + 1 + 8;
    }
    Ok(entries)
}

/// Parse a dictzip header: `(chunk length, compressed chunk sizes, offset
/// of the first chunk)`.
fn read_dictzip_header<R: Read + Seek>(r: &mut R) -> Result<(u64, Vec<u16>, u64), Error> {
    let mut head = [0u8; 10];
    r.read_exact(&mut head)?;
    if head[0] != 0x1f || head[1] != 0x8b || head[2] != 8 {
        return Err(Error::Dictionary("dictzip: not a gzip file".into()));
    }
    let flags = head[3];
    if flags & 0x04 == 0 {
        return Err(Error::Dictionary(
            "dictzip: no random-access table (a plain gzip)".into(),
        ));
    }
    let mut xlen = [0u8; 2];
    r.read_exact(&mut xlen)?;
    let xlen = u16::from_le_bytes(xlen) as usize;
    let mut extra = vec![0; xlen];
    r.read_exact(&mut extra)?;
    let mut pos = 12 + xlen as u64;
    // Optional name / comment / header CRC follow the extra field.
    if flags & 0x08 != 0 {
        pos += skip_cstring(r)?;
    }
    if flags & 0x10 != 0 {
        pos += skip_cstring(r)?;
    }
    if flags & 0x02 != 0 {
        let mut crc = [0u8; 2];
        r.read_exact(&mut crc)?;
        pos += 2;
    }
    // Subfields: id(2) len(2) data.
    let mut at = 0;
    while at + 4 <= extra.len() {
        let len = u16::from_le_bytes([extra[at + 2], extra[at + 3]]) as usize;
        let body = &extra[at + 4..(at + 4 + len).min(extra.len())];
        if &extra[at..at + 2] == b"RA" && body.len() >= 6 {
            let chunk_len = u16::from_le_bytes([body[2], body[3]]) as u64;
            let count = u16::from_le_bytes([body[4], body[5]]) as usize;
            let sizes: Vec<u16> = (0..count)
                .map_while(|i| body.get(6 + 2 * i..8 + 2 * i))
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            if sizes.len() != count || chunk_len == 0 {
                return Err(Error::Dictionary("dictzip: short chunk table".into()));
            }
            return Ok((chunk_len, sizes, pos));
        }
        at += 4 + len;
    }
    Err(Error::Dictionary("dictzip: no RA subfield".into()))
}

fn skip_cstring<R: Read>(r: &mut R) -> Result<u64, Error> {
    let mut n = 0;
    let mut b = [0u8; 1];
    loop {
        r.read_exact(&mut b)?;
        n += 1;
        if b[0] == 0 {
            return Ok(n);
        }
    }
}

/// An entry's bytes as plain text. With a `sametypesequence` every entry
/// is that sequence of kinds; otherwise each piece starts with its kind
/// letter. Text kinds (`m`, `l`, `g`, `t`, `x`, `y`, `k`, `w`, `h`, `n`)
/// are kept, markup stripped; binary kinds are skipped.
fn render_entry(raw: &[u8], types: &str) -> String {
    let mut out = Vec::new();
    if types.is_empty() {
        let mut at = 0;
        while at < raw.len() {
            let kind = raw[at] as char;
            at += 1;
            if kind.is_ascii_lowercase() {
                let end = raw[at..]
                    .iter()
                    .position(|&b| b == 0)
                    .map_or(raw.len(), |p| at + p);
                out.push(piece_text(kind, &raw[at..end]));
                at = end + 1;
            } else {
                if at + 4 > raw.len() {
                    break;
                }
                let n =
                    u32::from_be_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]) as usize;
                at += 4 + n;
            }
        }
    } else {
        let kinds: Vec<char> = types.chars().collect();
        let mut at = 0;
        for (i, &kind) in kinds.iter().enumerate() {
            let last = i + 1 == kinds.len();
            if kind.is_ascii_lowercase() {
                let end = if last {
                    raw.len()
                } else {
                    raw[at..]
                        .iter()
                        .position(|&b| b == 0)
                        .map_or(raw.len(), |p| at + p)
                };
                out.push(piece_text(kind, &raw[at..end.min(raw.len())]));
                at = (end + 1).min(raw.len());
            } else if last {
                break;
            } else {
                if at + 4 > raw.len() {
                    break;
                }
                let n =
                    u32::from_be_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]) as usize;
                at += 4 + n;
            }
        }
    }
    out.into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn piece_text(kind: char, bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    match kind {
        'm' | 'l' | 't' | 'y' | 'k' | 'w' => s.into_owned(),
        // Pango/HTML/XDXF markup: tags out, entities decoded, breaks kept.
        'g' | 'h' | 'x' | 'n' => strip_markup(&s),
        _ => String::new(),
    }
}

/// Tags removed; `<br>`, `<p>`, `<div>`, `<li>` and XDXF `<def>` become
/// line breaks; entities decoded; whitespace collapsed per line.
pub fn strip_markup(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let Some(gt) = rest[lt..].find('>') else {
            break;
        };
        let tag = rest[lt + 1..lt + gt].trim_start_matches('/');
        let name: String = tag
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "br" | "p" | "div" | "li" | "def" | "tr" | "blockquote" | "ex"
        ) {
            out.push('\n');
        }
        rest = &rest[lt + gt + 1..];
    }
    out.push_str(rest);
    let decoded = crate::epub::decode_entities(&out);
    decoded
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The word without the punctuation a press on a page picks up with it:
/// quotes, brackets, sentence punctuation, and a possessive 's.
pub fn trim_word(word: &str) -> String {
    let trimmed =
        word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '\'' && c != '’');
    let trimmed = trimmed.trim_matches(|c| c == '\'' || c == '’' || c == '-');
    let lower = trimmed.to_lowercase();
    for suffix in ["’s", "'s"] {
        if let Some(base) = lower.strip_suffix(suffix) {
            return trimmed[..base.len()].to_string();
        }
    }
    trimmed.to_string()
}

/// The word, then the base forms common English inflections point at,
/// most likely first.
fn candidates(word: &str) -> Vec<String> {
    let mut out = vec![word.to_string()];
    let lower = word.to_lowercase();
    if lower != word {
        out.push(lower.clone());
    }
    let mut push = |s: String| {
        if s.len() >= 2 && !out.contains(&s) {
            out.push(s);
        }
    };
    if let Some(b) = lower.strip_suffix("ies") {
        push(format!("{b}y"));
    }
    if let Some(b) = lower.strip_suffix("es") {
        push(b.to_string());
    }
    if let Some(b) = lower.strip_suffix('s') {
        push(b.to_string());
    }
    if let Some(b) = lower.strip_suffix("ied") {
        push(format!("{b}y"));
    }
    if let Some(b) = lower.strip_suffix("ed") {
        push(b.to_string());
        push(format!("{b}e"));
        if b.len() >= 2 && b.as_bytes()[b.len() - 1] == b.as_bytes()[b.len() - 2] {
            push(b[..b.len() - 1].to_string());
        }
    }
    if let Some(b) = lower.strip_suffix("ing") {
        push(b.to_string());
        push(format!("{b}e"));
        if b.len() >= 2 && b.as_bytes()[b.len() - 1] == b.as_bytes()[b.len() - 2] {
            push(b[..b.len() - 1].to_string());
        }
    }
    if let Some(b) = lower.strip_suffix("iest") {
        push(format!("{b}y"));
    }
    if let Some(b) = lower.strip_suffix("est") {
        push(b.to_string());
        push(format!("{b}e"));
    }
    if let Some(b) = lower.strip_suffix("ier") {
        push(format!("{b}y"));
    }
    if let Some(b) = lower.strip_suffix("er") {
        push(b.to_string());
        push(format!("{b}e"));
    }
    if let Some(b) = lower.strip_suffix("ily") {
        push(format!("{b}y"));
    }
    if let Some(b) = lower.strip_suffix("ly") {
        push(b.to_string());
    }
    out
}

/// Unpack a downloaded dictionary zip into `dicts_dir`. Only the StarDict
/// files and a licence are written, each under its own folder name with
/// path parts stripped, so a hostile archive cannot write elsewhere.
/// Returns the names of the dictionaries unpacked.
pub fn install_zip(dicts_dir: &Path, bytes: &[u8]) -> Result<Vec<String>, Error> {
    let mut archive = crate::zipread::ZipSource::open(std::io::Cursor::new(bytes.to_vec()))
        .map_err(|e| Error::Dictionary(format!("not a zip archive: {e}")))?;
    let names = archive.names();
    let mut installed = Vec::new();
    for name in names {
        let file = name.rsplit('/').next().unwrap_or(&name);
        let ok = file.ends_with(".ifo")
            || file.ends_with(".idx")
            || file.ends_with(".idx.gz")
            || file.ends_with(".dict")
            || file.ends_with(".dict.dz")
            || file.to_ascii_uppercase().contains("LICENSE");
        if !ok || file.is_empty() {
            continue;
        }
        let folder = name
            .split('/')
            .next()
            .filter(|f| !f.is_empty() && name.contains('/'))
            .unwrap_or("dictionary");
        let folder = crate::upload::sanitize_filename(folder);
        let dir = dicts_dir.join(&folder);
        std::fs::create_dir_all(&dir)?;
        let data = archive
            .read(&name)
            .ok_or_else(|| Error::Dictionary(format!("{name}: unreadable entry")))?;
        std::fs::write(dir.join(crate::upload::sanitize_filename(file)), data)?;
        if file.ends_with(".ifo") {
            installed.push(folder.clone());
        }
    }
    if installed.is_empty() {
        return Err(Error::Dictionary(
            "the archive holds no StarDict dictionary".into(),
        ));
    }
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write a small StarDict dictionary; `dz` picks dictzip for the data.
    pub(crate) fn write_dictionary(dir: &Path, stem: &str, entries: &[(&str, &str)], dz: bool) {
        std::fs::create_dir_all(dir).unwrap();
        let mut sorted: Vec<_> = entries.to_vec();
        sorted.sort_by(|a, b| {
            a.0.to_lowercase()
                .cmp(&b.0.to_lowercase())
                .then(a.0.cmp(b.0))
        });
        let mut idx = Vec::new();
        let mut data = Vec::new();
        for (w, d) in &sorted {
            idx.extend_from_slice(w.as_bytes());
            idx.push(0);
            idx.extend_from_slice(&(data.len() as u32).to_be_bytes());
            idx.extend_from_slice(&(d.len() as u32).to_be_bytes());
            data.extend_from_slice(d.as_bytes());
        }
        std::fs::write(dir.join(format!("{stem}.idx")), &idx).unwrap();
        std::fs::write(
            dir.join(format!("{stem}.ifo")),
            format!(
                "StarDict's dict ifo file\nversion=2.4.2\nwordcount={}\nidxfilesize={}\nbookname=Test Words\nsametypesequence=m\n",
                sorted.len(),
                idx.len()
            ),
        )
        .unwrap();
        if dz {
            std::fs::write(dir.join(format!("{stem}.dict.dz")), dictzip(&data, 16)).unwrap();
        } else {
            std::fs::write(dir.join(format!("{stem}.dict")), &data).unwrap();
        }
    }

    /// A dictzip with tiny chunks, so entries straddle chunk boundaries.
    fn dictzip(plain: &[u8], chunk_len: usize) -> Vec<u8> {
        use flate2::{Compress, Compression, FlushCompress};
        let mut comp = Compress::new(Compression::default(), false);
        let mut sizes = Vec::new();
        let mut body = Vec::new();
        let chunks: Vec<&[u8]> = plain.chunks(chunk_len).collect();
        for (i, c) in chunks.iter().enumerate() {
            let before = comp.total_out();
            let mut out = Vec::with_capacity(c.len() + 64);
            let flush = if i + 1 == chunks.len() {
                FlushCompress::Finish
            } else {
                FlushCompress::Full
            };
            comp.compress_vec(c, &mut out, flush).unwrap();
            sizes.push((comp.total_out() - before) as u16);
            body.extend_from_slice(&out);
        }
        let mut ra = Vec::new();
        ra.extend_from_slice(&1u16.to_le_bytes());
        ra.extend_from_slice(&(chunk_len as u16).to_le_bytes());
        ra.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
        for s in &sizes {
            ra.extend_from_slice(&s.to_le_bytes());
        }
        let mut extra = b"RA".to_vec();
        extra.extend_from_slice(&(ra.len() as u16).to_le_bytes());
        extra.extend_from_slice(&ra);
        let mut gz = vec![0x1f, 0x8b, 8, 0x04, 0, 0, 0, 0, 2, 3];
        gz.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        gz.extend_from_slice(&extra);
        gz.extend_from_slice(&body);
        let mut hasher = flate2::Crc::new();
        hasher.update(plain);
        gz.extend_from_slice(&hasher.sum().to_le_bytes());
        gz.extend_from_slice(&(plain.len() as u32).to_le_bytes());
        gz
    }

    const WORDS: &[(&str, &str)] = &[
        (
            "apothecary",
            "n. a health professional trained in the art of preparing and dispensing drugs",
        ),
        (
            "run",
            "v. move fast by using one's feet\nn. a score in baseball",
        ),
        (
            "Zebra",
            "n. any of several fleet black-and-white striped African equines",
        ),
        ("berry", "n. any of numerous small and pulpy edible fruits"),
        ("stop", "v. come to a halt"),
        (
            "happy",
            "adj. enjoying or showing or marked by joy or pleasure",
        ),
    ];

    #[test]
    fn plain_and_dictzip_dictionaries_answer_the_same() {
        for dz in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            write_dictionary(dir.path(), "words", WORDS, dz);
            let mut d = Dictionary::open(&dir.path().join("words.ifo")).unwrap();
            assert_eq!(d.name, "Test Words");
            assert_eq!(d.len(), 6);
            let hit = d.lookup("apothecary").unwrap();
            assert_eq!(hit.word, "apothecary");
            assert!(
                hit.text.starts_with("n. a health professional"),
                "{}",
                hit.text
            );
            // Every entry, so chunk boundaries are crossed both ways.
            for (w, text) in WORDS {
                assert_eq!(d.lookup(w).unwrap().text, *text, "dz={dz} {w}");
            }
        }
    }

    #[test]
    fn a_pressed_word_is_cleaned_cased_and_uninflected() {
        let dir = tempfile::tempdir().unwrap();
        write_dictionary(dir.path(), "words", WORDS, true);
        let mut d = Dictionary::open(&dir.path().join("words.ifo")).unwrap();
        for pressed in [
            "“Apothecary’s",
            "apothecary.",
            "(apothecary)",
            "APOTHECARY,",
        ] {
            assert_eq!(d.lookup(pressed).unwrap().word, "apothecary", "{pressed}");
        }
        for (pressed, head) in [
            ("runs", "run"),
            ("running", "run"),
            ("zebras", "Zebra"),
            ("berries", "berry"),
            ("stopped", "stop"),
            ("happily", "happy"),
            ("happier", "happy"),
        ] {
            assert_eq!(d.lookup(pressed).unwrap().word, head, "{pressed}");
        }
        assert!(d.lookup("xylophone").is_none());
        assert!(d.lookup("…").is_none());
    }

    #[test]
    fn markup_entries_come_out_as_text() {
        assert_eq!(
            strip_markup("<k>run</k><def>move <i>fast</i>&amp; free<br/>second</def>"),
            "run\nmove fast& free\nsecond"
        );
        // Mixed-type entries without sametypesequence: text kinds kept,
        // a binary kind skipped by its length.
        let mut raw = b"m".to_vec();
        raw.extend_from_slice(b"plain\0");
        raw.push(b'P');
        raw.extend_from_slice(&3u32.to_be_bytes());
        raw.extend_from_slice(&[1, 2, 3]);
        raw.push(b'h');
        raw.extend_from_slice(b"<b>html</b>\0");
        assert_eq!(render_entry(&raw, ""), "plain\nhtml");
    }

    #[test]
    fn installed_finds_dictionaries_one_folder_down_and_skips_broken_ones() {
        let dir = tempfile::tempdir().unwrap();
        write_dictionary(&dir.path().join("wordnet"), "wordnet", WORDS, true);
        write_dictionary(dir.path(), "top", &WORDS[..1], false);
        std::fs::write(dir.path().join("broken.ifo"), "StarDict's dict ifo file\n").unwrap();
        let found = Dictionary::installed(dir.path());
        assert_eq!(found.len(), 2);
        assert!(Dictionary::installed(&dir.path().join("nowhere")).is_empty());
    }

    #[test]
    fn install_zip_unpacks_only_dictionary_files_into_a_safe_folder() {
        let dir = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        write_dictionary(src.path(), "wordnet", WORDS, true);
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            for f in ["wordnet.ifo", "wordnet.idx", "wordnet.dict.dz"] {
                w.start_file(format!("wordnet/{f}"), opts).unwrap();
                w.write_all(&std::fs::read(src.path().join(f)).unwrap())
                    .unwrap();
            }
            w.start_file("wordnet/LICENSE", opts).unwrap();
            w.write_all(b"free").unwrap();
            w.start_file("../../etc/passwd", opts).unwrap();
            w.write_all(b"nope").unwrap();
            w.start_file("wordnet/evil.sh", opts).unwrap();
            w.write_all(b"nope").unwrap();
            w.finish().unwrap();
        }
        let dicts = dir.path().join("dicts");
        let names = install_zip(&dicts, &buf.into_inner()).unwrap();
        assert_eq!(names, ["wordnet"]);
        assert!(dicts.join("wordnet/wordnet.dict.dz").exists());
        assert!(dicts.join("wordnet/LICENSE").exists());
        assert!(!dicts.join("wordnet/evil.sh").exists());
        assert!(!dir.path().join("etc").exists());
        let mut found = Dictionary::installed(&dicts);
        assert_eq!(found.len(), 1);
        assert!(found[0].lookup("Zebra").is_some());

        let err = install_zip(&dicts, b"PK\x03\x04 not really").unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
