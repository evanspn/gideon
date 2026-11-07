//! Reading ZIPs the way Python does: leniently.
//!
//! The `zip` crate is strict, and real ebooks from the wild are not. A
//! J-Novel Club EPUB (via a .NET zip library) puts a ZIP64 extra field on
//! every entry although every size fits in 32 bits, and ends with a full
//! ZIP64 end record; `zip` 2 cannot find that end record and `zip` 6
//! refuses every entry's local header. Python's `zipfile` reads the book
//! without comment, and so does the Kobo's own reader. So: the strict
//! reader first, and when it balks, this one — the central directory,
//! the sizes it names (ZIP64 extras honoured where the 32-bit fields
//! overflow, ignored otherwise), and plain inflate.

use std::io::{self, Read, Seek, SeekFrom};

/// One entry of the central directory.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    method: u16,
    compressed: u64,
    pub size: u64,
    local_offset: u64,
}

/// A ZIP opened leniently.
pub struct LenientZip<R: Read + Seek> {
    reader: R,
    entries: Vec<Entry>,
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
fn u64_at(b: &[u8], i: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[i..i + 8]);
    u64::from_le_bytes(a)
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

impl<R: Read + Seek> LenientZip<R> {
    pub fn open(mut reader: R) -> io::Result<Self> {
        let len = reader.seek(SeekFrom::End(0))?;
        // The end record is within the last 64 KiB + 22 bytes (a comment
        // can be that long).
        let tail_len = len.min(65_557 + 22);
        reader.seek(SeekFrom::Start(len - tail_len))?;
        let mut tail = vec![0u8; tail_len as usize];
        reader.read_exact(&mut tail)?;
        let eocd = (0..tail.len().saturating_sub(21))
            .rev()
            .find(|&i| tail[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
            .ok_or_else(|| bad("no end-of-central-directory record"))?;
        let rec = &tail[eocd..];
        let mut count = u16_at(rec, 10) as u64;
        let mut cd_size = u32_at(rec, 12) as u64;
        let mut cd_off = u32_at(rec, 16) as u64;
        // ZIP64: any field at its 32-bit ceiling sends us to the ZIP64 end
        // record, found through its locator just before the end record.
        if count == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_off == 0xFFFF_FFFF {
            if eocd >= 20 && tail[eocd - 20..eocd - 16] == [0x50, 0x4b, 0x06, 0x07] {
                let z64_off = u64_at(&tail, eocd - 20 + 8);
                reader.seek(SeekFrom::Start(z64_off))?;
                let mut z = [0u8; 56];
                reader.read_exact(&mut z)?;
                if z[..4] != [0x50, 0x4b, 0x06, 0x06] {
                    return Err(bad("ZIP64 end record is not where its locator says"));
                }
                count = u64_at(&z, 32);
                cd_size = u64_at(&z, 40);
                cd_off = u64_at(&z, 48);
            } else {
                return Err(bad("ZIP64 end record without a locator"));
            }
        }
        if cd_off.checked_add(cd_size).is_none_or(|end| end > len) {
            return Err(bad("central directory runs past the end of the file"));
        }
        reader.seek(SeekFrom::Start(cd_off))?;
        let mut cd = vec![0u8; cd_size as usize];
        reader.read_exact(&mut cd)?;
        let mut entries = Vec::with_capacity(count.min(100_000) as usize);
        let mut i = 0usize;
        while i + 46 <= cd.len() && entries.len() < count as usize {
            if cd[i..i + 4] != [0x50, 0x4b, 0x01, 0x02] {
                break;
            }
            let method = u16_at(&cd, i + 10);
            let mut compressed = u32_at(&cd, i + 20) as u64;
            let mut size = u32_at(&cd, i + 24) as u64;
            let name_len = u16_at(&cd, i + 28) as usize;
            let extra_len = u16_at(&cd, i + 30) as usize;
            let comment_len = u16_at(&cd, i + 32) as usize;
            let mut local_offset = u32_at(&cd, i + 42) as u64;
            let name_start = i + 46;
            let extra_start = name_start + name_len;
            let next = extra_start + extra_len + comment_len;
            if next > cd.len() {
                break;
            }
            let name = String::from_utf8_lossy(&cd[name_start..extra_start]).into_owned();
            // A ZIP64 extra field overrides only the fields that overflowed,
            // in the order the spec lists them. One present on an entry whose
            // sizes fit is simply not consulted — the mistake that trips the
            // strict reader.
            let extra = &cd[extra_start..extra_start + extra_len];
            let mut e = 0usize;
            while e + 4 <= extra.len() {
                let id = u16_at(extra, e);
                let sz = u16_at(extra, e + 2) as usize;
                let body = &extra[(e + 4).min(extra.len())..(e + 4 + sz).min(extra.len())];
                if id == 0x0001 {
                    let mut p = 0usize;
                    if size == 0xFFFF_FFFF && p + 8 <= body.len() {
                        size = u64_at(body, p);
                        p += 8;
                    }
                    if compressed == 0xFFFF_FFFF && p + 8 <= body.len() {
                        compressed = u64_at(body, p);
                        p += 8;
                    }
                    if local_offset == 0xFFFF_FFFF && p + 8 <= body.len() {
                        local_offset = u64_at(body, p);
                    }
                }
                e += 4 + sz;
            }
            entries.push(Entry {
                name,
                method,
                compressed,
                size,
                local_offset,
            });
            i = next;
        }
        Ok(Self { reader, entries })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    /// The bytes of `name`, inflated. Stored and deflated entries only;
    /// anything else (bzip2, zstd, encrypted) is not a book's business.
    pub fn read(&mut self, name: &str) -> io::Result<Vec<u8>> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.name == name)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, name.to_string()))?;
        self.reader.seek(SeekFrom::Start(entry.local_offset))?;
        let mut local = [0u8; 30];
        self.reader.read_exact(&mut local)?;
        if local[..4] != [0x50, 0x4b, 0x03, 0x04] {
            return Err(bad("entry's local header is not where the directory says"));
        }
        let name_len = u16_at(&local, 26) as u64;
        let extra_len = u16_at(&local, 28) as u64;
        self.reader.seek(SeekFrom::Start(
            entry.local_offset + 30 + name_len + extra_len,
        ))?;
        let mut raw = vec![0u8; entry.compressed as usize];
        self.reader.read_exact(&mut raw)?;
        match entry.method {
            0 => Ok(raw),
            8 => {
                let mut out = Vec::with_capacity(entry.size as usize);
                flate2::read::DeflateDecoder::new(&raw[..]).read_to_end(&mut out)?;
                Ok(out)
            }
            m => Err(bad(&format!("unsupported compression method {m}"))),
        }
    }
}

/// A ZIP read the strict way when it can be, the lenient way when it must.
pub enum ZipSource<R: Read + Seek> {
    Strict(zip::ZipArchive<R>),
    Lenient(LenientZip<R>),
}

impl<R: Read + Seek + Clone> ZipSource<R> {
    /// Open with the strict reader; if it cannot open the archive, or cannot
    /// read its first entry, open leniently instead. `reader` is cloned for
    /// the second attempt (a `Cursor` or a `File`-backed reader both can).
    pub fn open(reader: R) -> io::Result<Self> {
        if let Ok(mut strict) = zip::ZipArchive::new(reader.clone()) {
            // Every entry's local header must pass the strict reader, not
            // only the first: the sloppy ZIP64 extra sits on every entry
            // but the stored `mimetype` that comes first. by_index reads a
            // header, not the data, so this is cheap even for a big book.
            let all_ok = (0..strict.len()).all(|i| strict.by_index(i).is_ok());
            if all_ok {
                return Ok(ZipSource::Strict(strict));
            }
        }
        LenientZip::open(reader).map(ZipSource::Lenient)
    }
}

impl<R: Read + Seek> ZipSource<R> {
    pub fn names(&self) -> Vec<String> {
        match self {
            ZipSource::Strict(a) => a.file_names().map(str::to_string).collect(),
            ZipSource::Lenient(a) => a.names().map(str::to_string).collect(),
        }
    }

    /// The bytes of `name`, or `None` when it is not there or will not read.
    pub fn read(&mut self, name: &str) -> Option<Vec<u8>> {
        match self {
            ZipSource::Strict(a) => {
                let mut entry = a.by_name(name).ok()?;
                let mut out = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut out).ok()?;
                Some(out)
            }
            ZipSource::Lenient(a) => a.read(name).ok(),
        }
    }

    pub fn read_string(&mut self, name: &str) -> Option<String> {
        self.read(name)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }
}

/// A file opened for seeking, cloneable so [`ZipSource::open`] can try
/// twice. Each clone is its own handle on the same path.
pub struct SeekFile {
    path: std::path::PathBuf,
    inner: std::io::BufReader<std::fs::File>,
}

impl SeekFile {
    pub fn open(path: &std::path::Path) -> io::Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            inner: std::io::BufReader::new(std::fs::File::open(path)?),
        })
    }
}

impl Clone for SeekFile {
    fn clone(&self) -> Self {
        // A path that opened a moment ago; if it has gone, the strict
        // attempt simply fails and the lenient one reports the real error.
        let file = std::fs::File::open(&self.path)
            .unwrap_or_else(|_| std::fs::File::open("/dev/null").expect("/dev/null"));
        Self {
            path: self.path.clone(),
            inner: std::io::BufReader::new(file),
        }
    }
}

impl Read for SeekFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for SeekFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a ZIP by hand: stored entries, a pointless ZIP64 extra on each
    /// (sizes fit in 32 bits), and a full ZIP64 end record — the shape the
    /// strict reader refuses.
    fn sloppy_zip64(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, body) in entries {
            let offset = out.len() as u32;
            let mut extra = Vec::new();
            // 32 bytes, the way the .NET writer does it: sizes, offset,
            // and a disk number padded out.
            extra.extend_from_slice(&1u16.to_le_bytes());
            extra.extend_from_slice(&32u16.to_le_bytes());
            extra.extend_from_slice(&(body.len() as u64).to_le_bytes());
            extra.extend_from_slice(&(body.len() as u64).to_le_bytes());
            extra.extend_from_slice(&(offset as u64).to_le_bytes());
            extra.extend_from_slice(&0u64.to_le_bytes());
            let crc = crc32(body);
            // Local header.
            out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // stored
            out.extend_from_slice(&[0, 0, 0, 0]);
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&(extra.len() as u16).to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&extra);
            out.extend_from_slice(body);
            // Central header.
            central.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
            central.extend_from_slice(&45u16.to_le_bytes());
            central.extend_from_slice(&45u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&[0, 0, 0, 0]);
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&(extra.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
            central.extend_from_slice(&extra);
        }
        let cd_off = out.len() as u64;
        out.extend_from_slice(&central);
        let z64_off = out.len() as u64;
        // ZIP64 end record.
        out.extend_from_slice(&[0x50, 0x4b, 0x06, 0x06]);
        out.extend_from_slice(&44u64.to_le_bytes());
        out.extend_from_slice(&45u16.to_le_bytes());
        out.extend_from_slice(&45u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        out.extend_from_slice(&(central.len() as u64).to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        // Locator.
        out.extend_from_slice(&[0x50, 0x4b, 0x06, 0x07]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&z64_off.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        // Classic end record, all at the ceiling.
        out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&0xFFFFu16.to_le_bytes());
        out.extend_from_slice(&0xFFFFu16.to_le_bytes());
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn a_zip64_archive_with_needless_extras_reads_leniently() {
        let bytes = sloppy_zip64(&[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", b"<container/>"),
        ]);
        let mut lenient = LenientZip::open(Cursor::new(bytes.clone())).unwrap();
        assert_eq!(lenient.read("mimetype").unwrap(), b"application/epub+zip");
        // Whichever reader takes it, the source reads it.
        let mut src = ZipSource::open(Cursor::new(bytes)).unwrap();
        assert_eq!(src.names(), ["mimetype", "META-INF/container.xml"]);
        assert_eq!(src.read("mimetype").unwrap(), b"application/epub+zip");
        assert_eq!(
            src.read_string("META-INF/container.xml").as_deref(),
            Some("<container/>")
        );
        assert!(src.read("nope").is_none());
    }

    #[test]
    fn a_well_formed_zip_takes_the_strict_path_and_deflate_inflates() {
        let mut buf = Cursor::new(Vec::new());
        {
            use std::io::Write;
            let mut w = zip::ZipWriter::new(&mut buf);
            w.start_file("a.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(b"hello hello hello hello").unwrap();
            w.finish().unwrap();
        }
        let bytes = buf.into_inner();
        let mut src = ZipSource::open(Cursor::new(bytes.clone())).unwrap();
        assert!(matches!(src, ZipSource::Strict(_)));
        assert_eq!(src.read("a.txt").unwrap(), b"hello hello hello hello");
        // The lenient reader handles the same deflated archive too.
        let mut lenient = LenientZip::open(Cursor::new(bytes)).unwrap();
        assert_eq!(lenient.read("a.txt").unwrap(), b"hello hello hello hello");
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(LenientZip::open(Cursor::new(b"not a zip at all".to_vec())).is_err());
        assert!(ZipSource::open(Cursor::new(Vec::new())).is_err());
        let mut truncated = sloppy_zip64(&[("a", b"x")]);
        truncated.truncate(truncated.len() - 30);
        assert!(LenientZip::open(Cursor::new(truncated)).is_err());
    }
}
