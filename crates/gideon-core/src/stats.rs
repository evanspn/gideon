//! Reading statistics derived on-device from [`ProgressStore`].
//!
//! These numbers used to exist only on the web dashboard, where `computeStats`
//! in `web/app.js` folds the synced `reading_progress` rows into tiles, streaks
//! and a GitHub-style heatmap. The device has exactly the same information
//! locally — `progress.json` *is* those rows — so there is no reason for a Kobo
//! that has never signed in to show nothing. This module is a faithful port, so
//! a reader who looks at both surfaces sees the same figures.
//!
//! Definitions are deliberately copied from the web, not re-invented:
//!
//! - **tracked** — every chapter with a progress row. Opening a chapter creates
//!   one, so "tracked" is "started or finished".
//! - **finished** — [`ReadingProgress::is_finished`], i.e. the last page was
//!   reached (`current_page + 1 >= total_pages`, `total_pages > 0`).
//! - **pages read** — `min(current_page + 1, total_pages)` summed over tracked
//!   chapters (`pagesOf` in app.js). `current_page` is a zero-based index, so
//!   the `+ 1` counts the page the reader is *on*. The clamp matters because
//!   `total_pages` is whatever the file reported when progress was last saved:
//!   if a chapter is later replaced by a shorter re-release, an old
//!   `current_page` must not inflate the total. When `total_pages` is 0 (a row
//!   written before the page count was known) the clamp is skipped, matching
//!   the web.
//! - **series** — the chapter key's top-level directory, the same split
//!   `series_key_of` uses in the app UI (`"Series/vol3.cbz"` → `"Series"`, a
//!   loose root file is its own series).
//! - **a day's pages** — a chapter's whole page count is attributed to the day
//!   it was *last read*. Progress rows carry one timestamp, not a session log,
//!   so this is the only attribution available; it is what the web does and
//!   what the heatmap has always meant.
//!
//! # Why local calendar days
//!
//! Streaks and the heatmap are about *the reader's* days. Bucketing by UTC
//! would silently break both for anyone west of Greenwich: an evening reading
//! session at 21:00 in UTC-05:00 lands on the *next* UTC day, so a reader who
//! reads every single evening sees their streak reset and their heatmap
//! smeared across the wrong squares. So every timestamp is converted to a local
//! calendar day before it is counted — see [`LocalTimeZone`].
//!
//! `gideon-core` deliberately has no date/time dependency (see its
//! `Cargo.toml`: serde, zip, image, quick-xml and nothing else — chrono lives
//! only in `gideon-aidoku`, where the WASM source API forces it). Rather than
//! push a date library into the crate that runs on the device's tiny rootfs,
//! this module reads the system's own zoneinfo and does the civil-date
//! arithmetic itself; both are small, well-specified and testable.

use std::collections::{BTreeMap, BTreeSet};

use crate::library::{ProgressStore, ReadingProgress};

/// Seconds in a day. Local days are exactly this long *in local time*; DST
/// jumps are handled by changing the offset, never by stretching a day.
const SECS_PER_DAY: i64 = 86_400;

// ---------------------------------------------------------------------------
// Civil date arithmetic
// ---------------------------------------------------------------------------

/// Days since 1970-01-01 for a proleptic-Gregorian y/m/d.
///
/// Howard Hinnant's `days_from_civil` (public domain, the algorithm every
/// modern date library uses). Valid for any year in `i32`; the era arithmetic
/// avoids leap-year special cases entirely by shifting the year to start in
/// March, which makes the leap day the *last* day of the year.
pub fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = month as i64;
    let d = day as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]: a day index back to `(year, month, day)`.
pub fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    ((if m <= 2 { y + 1 } else { y }) as i32, m as u32, d as u32)
}

/// Day of the week for a day index, `0 = Sunday` … `6 = Saturday`.
///
/// Day 0 (1970-01-01) was a Thursday, hence the `+ 4`.
pub fn weekday(day_index: i64) -> u32 {
    (day_index + 4).rem_euclid(7) as u32
}

// ---------------------------------------------------------------------------
// Local time zone
// ---------------------------------------------------------------------------

/// The device's UTC offset over time, enough to turn a unix timestamp into a
/// local calendar day.
///
/// Built from the system's TZif (zoneinfo) database — `$TZ` if it names a zone,
/// otherwise `/etc/localtime`. The whole transition table is kept, not just
/// "the offset right now", so a timestamp from last winter is bucketed with
/// last winter's offset instead of today's; otherwise every DST change would
/// shift a slab of history by an hour and could invent or destroy a streak day
/// at the boundary.
///
/// If no zoneinfo can be read — some Kobo firmwares ship a rootfs without one,
/// and the WAL/desktop test hosts may too — this degrades to **UTC**, which
/// only distorts days for readers whose device clock is not on UTC. That
/// fallback is deliberate and observable via [`LocalTimeZone::is_utc_fallback`]
/// so the UI can say "times are UTC" rather than quietly lie; it is never
/// silent guesswork about which zone the reader is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTimeZone {
    /// `(transition_unix_time, offset_seconds_east)`, sorted ascending.
    transitions: Vec<(i64, i32)>,
    /// Offset for timestamps before the first transition (and the only offset
    /// when the table is empty).
    fallback: i32,
    /// True when no zoneinfo was found and this is plain UTC.
    utc_fallback: bool,
}

impl LocalTimeZone {
    /// A fixed-offset zone, `seconds_east` positive east of Greenwich.
    /// Used by tests and by callers that already know the offset.
    pub fn fixed(seconds_east: i32) -> Self {
        Self {
            transitions: Vec::new(),
            fallback: seconds_east,
            utc_fallback: false,
        }
    }

    /// UTC, flagged as the fallback zone.
    pub fn utc() -> Self {
        Self {
            transitions: Vec::new(),
            fallback: 0,
            utc_fallback: true,
        }
    }

    /// The system zone: `$TZ` if it names a readable zoneinfo file, else
    /// `/etc/localtime`, else [`LocalTimeZone::utc`].
    pub fn system() -> Self {
        for path in system_tzif_paths() {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Some(tz) = Self::from_tzif(&bytes) {
                    return tz;
                }
            }
        }
        Self::utc()
    }

    /// Whether this is the "no zoneinfo found" UTC fallback rather than a real
    /// zone that happens to be at offset zero.
    pub fn is_utc_fallback(&self) -> bool {
        self.utc_fallback
    }

    /// Offset east of UTC, in seconds, in effect at `unix`.
    pub fn offset_at(&self, unix: i64) -> i32 {
        match self.transitions.binary_search_by_key(&unix, |&(t, _)| t) {
            Ok(i) => self.transitions[i].1,
            Err(0) => self.fallback,
            Err(i) => self.transitions[i - 1].1,
        }
    }

    /// The local calendar day containing `unix`, as days since 1970-01-01.
    ///
    /// Floor division (not truncation) so pre-epoch instants — reachable for a
    /// reader east of UTC on 1970-01-01, or from a device with a dead RTC —
    /// land on the day before, not on day 0.
    pub fn day_index(&self, unix: i64) -> i64 {
        (unix + self.offset_at(unix) as i64).div_euclid(SECS_PER_DAY)
    }

    /// Parse a TZif (RFC 8536) file. Returns `None` if it is not a TZif or is
    /// truncated — a malformed zoneinfo must degrade to UTC, never panic on a
    /// user's device.
    ///
    /// Version 2+ files carry the authoritative 64-bit block *after* a
    /// (nowadays often stub) 32-bit block, so the 32-bit block is parsed only
    /// to find where the second header begins, then discarded.
    pub fn from_tzif(bytes: &[u8]) -> Option<Self> {
        let mut cur = Cursor::new(bytes);
        let head = TzifHeader::parse(&mut cur)?;
        let version = head.version;
        let block = parse_block(&mut cur, &head, 4)?;

        let block = if version >= b'2' {
            let head2 = TzifHeader::parse(&mut cur)?;
            parse_block(&mut cur, &head2, 8)?
        } else {
            block
        };

        // Timestamps before the first transition use the first non-DST type,
        // per RFC 8536 §3.2; falling back to the first type covers files whose
        // types are all DST (they exist, and an hour of skew beats no zone).
        let fallback = block
            .types
            .iter()
            .find(|t| !t.is_dst)
            .or_else(|| block.types.first())
            .map(|t| t.utoff)
            .unwrap_or(0);

        Some(Self {
            transitions: block.transitions,
            fallback,
            utc_fallback: false,
        })
    }
}

/// Candidate zoneinfo paths, most specific first.
///
/// `$TZ` may be a zone name (`America/New_York`), optionally `:`-prefixed, or
/// an absolute path. A POSIX rule string (`EST5EDT,M3.2.0,M11.1.0`) simply
/// won't resolve to a file and we move on to `/etc/localtime`, which on every
/// system that supports `$TZ` rules describes the same zone anyway.
fn system_tzif_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Ok(tz) = std::env::var("TZ") {
        let name = tz.strip_prefix(':').unwrap_or(&tz);
        // Reject traversal and empty names: this string is attacker-influenced
        // only in the loosest sense, but there is no reason to follow "..".
        let sane = !name.is_empty() && !name.split('/').any(|c| c == "..");
        if sane {
            if name.starts_with('/') {
                paths.push(std::path::PathBuf::from(name));
            } else {
                paths.push(std::path::Path::new("/usr/share/zoneinfo").join(name));
            }
        }
    }
    paths.push(std::path::PathBuf::from("/etc/localtime"));
    paths
}

/// Minimal big-endian reader over a byte slice; every read is bounds-checked
/// and returns `None` past the end, so a truncated file falls out as `None`
/// rather than panicking.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }

    /// A signed big-endian integer of `width` bytes (4 in a v1 block, 8 in a
    /// v2+ block).
    fn int(&mut self, width: usize) -> Option<i64> {
        let b = self.take(width)?;
        let mut v = i64::from(b[0] as i8);
        for &byte in &b[1..] {
            v = (v << 8) | i64::from(byte);
        }
        Some(v)
    }
}

struct TzifHeader {
    version: u8,
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

impl TzifHeader {
    fn parse(cur: &mut Cursor<'_>) -> Option<Self> {
        if cur.take(4)? != b"TZif" {
            return None;
        }
        let version = cur.u8()?;
        cur.take(15)?; // reserved
        let head = Self {
            version,
            isutcnt: cur.u32()? as usize,
            isstdcnt: cur.u32()? as usize,
            leapcnt: cur.u32()? as usize,
            timecnt: cur.u32()? as usize,
            typecnt: cur.u32()? as usize,
            charcnt: cur.u32()? as usize,
        };
        // Guard against a corrupt header asking us to allocate gigabytes: the
        // counts must at least be plausible for the bytes that remain.
        let remaining = head.timecnt.max(head.typecnt);
        if remaining > cur.bytes.len() {
            return None;
        }
        Some(head)
    }
}

struct LocalTimeType {
    utoff: i32,
    is_dst: bool,
}

struct TzifBlock {
    transitions: Vec<(i64, i32)>,
    types: Vec<LocalTimeType>,
}

/// Parse one TZif data block; `time_width` is 4 for a v1 block, 8 for v2+.
fn parse_block(cur: &mut Cursor<'_>, head: &TzifHeader, time_width: usize) -> Option<TzifBlock> {
    let mut times = Vec::with_capacity(head.timecnt);
    for _ in 0..head.timecnt {
        times.push(cur.int(time_width)?);
    }
    let mut indices = Vec::with_capacity(head.timecnt);
    for _ in 0..head.timecnt {
        indices.push(cur.u8()? as usize);
    }
    let mut types = Vec::with_capacity(head.typecnt);
    for _ in 0..head.typecnt {
        let utoff = cur.i32()?;
        let is_dst = cur.u8()? != 0;
        cur.u8()?; // designation index; abbreviations are not needed here
        types.push(LocalTimeType { utoff, is_dst });
    }
    cur.take(head.charcnt)?; // designation strings
    cur.take(head.leapcnt * (time_width + 4))?; // leap-second records
    cur.take(head.isstdcnt)?;
    cur.take(head.isutcnt)?;

    // Drop transitions whose type index is out of range rather than rejecting
    // the whole file; the remaining table is still usable.
    let transitions = times
        .into_iter()
        .zip(indices)
        .filter_map(|(t, i)| types.get(i).map(|ty| (t, ty.utoff)))
        .collect();

    Some(TzifBlock { transitions, types })
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/// The series key a chapter key belongs to: its top-level directory
/// (`"Series/vol3.cbz"` → `"Series"`), or the whole key for a loose file in the
/// library root. Mirrors `series_key_of` in the app UI so the series count here
/// matches the number of cards on the Home screen.
fn series_key_of(chapter_key: &str) -> &str {
    chapter_key.split('/').next().unwrap_or(chapter_key)
}

/// Pages attributed to one progress row — see the module docs.
fn pages_of(p: &ReadingProgress) -> usize {
    let read = p.current_page.saturating_add(1);
    if p.total_pages > 0 {
        read.min(p.total_pages)
    } else {
        read
    }
}

/// Everything the stats screen needs, computed in one pass over the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadingStats {
    /// Chapters with a progress row (started or finished).
    pub chapters_tracked: usize,
    /// Chapters whose last page was reached.
    pub chapters_finished: usize,
    /// Total pages read across all tracked chapters.
    pub pages_read: usize,
    /// Distinct series (top-level directories) touched.
    pub series_count: usize,
    /// Number of distinct local days with any reading.
    pub active_days: usize,
    /// Day index of the earliest active day, `None` for an empty store.
    ///
    /// A *day index* (days since 1970-01-01, local), not a timestamp — feed it
    /// to [`civil_from_days`] to render "since 3 Mar 2026". It is `u64` to
    /// match the store's other public time-ish fields; a negative index (only
    /// reachable from a device whose clock is set before the epoch) saturates
    /// to 0.
    pub first_day: Option<u64>,
    /// Consecutive local days with reading ending today, or ending yesterday
    /// if today has no reading yet — so the streak does not appear to break
    /// every morning before the reader has opened a chapter.
    pub current_streak: usize,
    /// The longest run of consecutive local days ever recorded.
    pub longest_streak: usize,
    /// Local day index → pages read that day. Sorted, so the first key is
    /// [`Self::first_day`] and iteration draws left-to-right.
    pub by_day: BTreeMap<i64, usize>,
    /// The local day the statistics were computed on. Anchors both the current
    /// streak and the right-hand edge of [`Self::heatmap`], and keeps the whole
    /// struct a pure function of its inputs (which is what makes it testable).
    pub today: i64,
}

impl ReadingStats {
    /// Compute statistics for `store` in the device's local zone, as of now.
    pub fn from_store(store: &ProgressStore) -> Self {
        let tz = LocalTimeZone::system();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Self::compute(store.entries(), &tz, now)
    }

    /// Compute statistics from raw `(chapter_key, progress)` pairs against an
    /// explicit zone and "now". Every input is a parameter, so tests can pin a
    /// clock and a zone; [`Self::from_store`] is the thin wrapper that supplies
    /// the real ones.
    pub fn compute<'a>(
        entries: impl Iterator<Item = (&'a str, ReadingProgress)>,
        tz: &LocalTimeZone,
        now_unix: i64,
    ) -> Self {
        let mut chapters_tracked = 0usize;
        let mut chapters_finished = 0usize;
        let mut pages_read = 0usize;
        let mut series = BTreeSet::new();
        let mut by_day: BTreeMap<i64, usize> = BTreeMap::new();

        for (key, progress) in entries {
            chapters_tracked += 1;
            series.insert(series_key_of(key).to_owned());
            let pages = pages_of(&progress);
            pages_read += pages;
            if progress.is_finished() {
                chapters_finished += 1;
            }
            let day = tz.day_index(progress.last_read_at as i64);
            *by_day.entry(day).or_insert(0) += pages;
        }

        let today = tz.day_index(now_unix);
        let (current_streak, longest_streak) = streaks(&by_day, today);
        let first_day = by_day.keys().next().map(|&d| d.max(0) as u64);

        Self {
            chapters_tracked,
            chapters_finished,
            pages_read,
            series_count: series.len(),
            active_days: by_day.len(),
            first_day,
            current_streak,
            longest_streak,
            by_day,
            today,
        }
    }

    /// The busiest day's page count, the denominator the heatmap shades
    /// against. 0 for an empty store.
    pub fn max_day(&self) -> usize {
        self.by_day.values().copied().max().unwrap_or(0)
    }

    /// Pages read on a given local day index.
    pub fn pages_on(&self, day_index: i64) -> usize {
        self.by_day.get(&day_index).copied().unwrap_or(0)
    }

    /// The last `weeks` weeks as a GitHub-style intensity grid: one entry per
    /// week column, each holding seven levels in `0..=4`, index 0 = Sunday.
    ///
    /// Laid out exactly like `heatmapHtml` in `web/app.js`: the window starts
    /// `weeks * 7 - 1` days before today and is then rounded *back* to the
    /// preceding Sunday, so columns are whole weeks and the last column holds
    /// today. Cells before the window's start and after today are level 0 —
    /// the caller draws the trailing ones as padding if it wants to, using
    /// [`Self::heatmap_start`] to work out which dates the cells stand for.
    ///
    /// Quantisation matches the web: 0 pages is level 0, anything else is
    /// `ceil(pages / max_day * 4)` capped at 4 — a relative scale, so the
    /// busiest day is always the darkest square no matter how much the reader
    /// reads. Integer `ceil` here, not floating point, so the levels are
    /// bit-identical on every device.
    pub fn heatmap(&self, weeks: usize) -> Vec<[u8; 7]> {
        let max = self.max_day();
        let start = self.heatmap_start(weeks);
        let mut grid = Vec::with_capacity(weeks + 1);
        let mut col_start = start;
        while col_start <= self.today {
            let mut col = [0u8; 7];
            for (d, cell) in col.iter_mut().enumerate() {
                let day = col_start + d as i64;
                *cell = if day > self.today {
                    0 // future padding
                } else {
                    quantise(self.pages_on(day), max)
                };
            }
            grid.push(col);
            col_start += 7;
        }
        grid
    }

    /// The local day index of the first cell (a Sunday) of a `weeks`-wide
    /// [`Self::heatmap`] grid, for labelling months and tooltips.
    pub fn heatmap_start(&self, weeks: usize) -> i64 {
        let span = (weeks as i64 * 7 - 1).max(0);
        let start = self.today - span;
        start - weekday(start) as i64
    }
}

/// Intensity level `0..=4` for `pages` against the busiest day's `max`.
fn quantise(pages: usize, max: usize) -> u8 {
    if pages == 0 || max == 0 {
        return 0;
    }
    // ceil(pages * 4 / max), capped — integer arithmetic, no float rounding.
    let level = pages.saturating_mul(4).div_ceil(max);
    level.min(4) as u8
}

/// Current and longest runs of consecutive active days.
///
/// The current streak walks back from `today`, or from yesterday when today
/// has no reading yet: a reader who read every day for a month should not see
/// "0" the moment midnight passes. Two days in a row means the day indices
/// differ by exactly 1 — which is why bucketing had to happen in local time
/// first; subtracting timestamps would get this wrong across a DST change.
fn streaks(by_day: &BTreeMap<i64, usize>, today: i64) -> (usize, usize) {
    if by_day.is_empty() {
        return (0, 0);
    }

    let mut longest = 1usize;
    let mut run = 1usize;
    let mut prev: Option<i64> = None;
    for &day in by_day.keys() {
        run = match prev {
            Some(p) if day == p + 1 => run + 1,
            _ => 1,
        };
        longest = longest.max(run);
        prev = Some(day);
    }

    let mut cursor = if by_day.contains_key(&today) {
        today
    } else {
        today - 1
    };
    let mut current = 0usize;
    while by_day.contains_key(&cursor) {
        current += 1;
        cursor -= 1;
    }

    (current, longest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UTC-05:00 — a zone where UTC bucketing visibly breaks, which is the
    /// whole point of the local-day work.
    fn tz_new_york_winter() -> LocalTimeZone {
        LocalTimeZone::fixed(-5 * 3600)
    }

    /// Unix seconds for a local wall-clock instant in a fixed-offset zone.
    fn local(tz: &LocalTimeZone, y: i32, m: u32, d: u32, hour: u32) -> u64 {
        let secs = days_from_civil(y, m, d) * SECS_PER_DAY + hour as i64 * 3600;
        (secs - tz.offset_at(secs) as i64) as u64
    }

    fn progress(page: usize, total: usize, at: u64) -> ReadingProgress {
        ReadingProgress {
            current_page: page,
            total_pages: total,
            last_read_at: at,
        }
    }

    fn stats(rows: &[(&str, ReadingProgress)], tz: &LocalTimeZone, now: u64) -> ReadingStats {
        ReadingStats::compute(rows.iter().map(|(k, p)| (*k, *p)), tz, now as i64)
    }

    #[test]
    fn civil_date_round_trips_across_leap_years_and_centuries() {
        for &(y, m, d) in &[
            (1970, 1, 1),
            (1969, 12, 31),
            (2000, 2, 29), // leap century
            (1900, 3, 1),  // non-leap century
            (2024, 2, 29),
            (2026, 8, 17),
            (2100, 12, 31),
        ] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d), "round trip {y}-{m}-{d}");
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        // 1970-01-01 was a Thursday (4), 1970-01-04 a Sunday (0).
        assert_eq!(weekday(0), 4);
        assert_eq!(weekday(3), 0);
        assert_eq!(weekday(-1), 3); // Wednesday
    }

    #[test]
    fn local_day_bucketing_differs_from_utc_for_an_evening_read() {
        let tz = tz_new_york_winter();
        // 21:00 local on 10 March is 02:00 UTC on 11 March. Bucketing in UTC
        // would put this on the wrong day and break the streak below.
        let ts = local(&tz, 2026, 3, 10, 21) as i64;
        assert_eq!(tz.day_index(ts), days_from_civil(2026, 3, 10));
        assert_eq!(
            LocalTimeZone::utc().day_index(ts),
            days_from_civil(2026, 3, 11),
            "sanity: UTC really does disagree here"
        );
    }

    #[test]
    fn empty_store_is_all_zeroes() {
        let tz = tz_new_york_winter();
        let now = local(&tz, 2026, 8, 17, 12);
        let s = stats(&[], &tz, now);

        assert_eq!(s.chapters_tracked, 0);
        assert_eq!(s.chapters_finished, 0);
        assert_eq!(s.pages_read, 0);
        assert_eq!(s.series_count, 0);
        assert_eq!(s.active_days, 0);
        assert_eq!(s.first_day, None);
        assert_eq!(s.current_streak, 0);
        assert_eq!(s.longest_streak, 0);
        assert!(s.by_day.is_empty());
        assert_eq!(s.max_day(), 0);
        // An empty heatmap is still a grid of the right shape, all level 0.
        let grid = s.heatmap(18);
        assert!(grid.iter().all(|c| c.iter().all(|&l| l == 0)));
    }

    #[test]
    fn from_store_matches_a_hand_built_store() {
        // Exercises the ProgressStore wiring itself; the numbers are trivial
        // because the clock and zone are the real ones here.
        let mut store = ProgressStore::default();
        store.update("Berserk/vol1.cbz", 9, 20);
        store.update("Berserk/vol2.cbz", 4, 20);
        let s = ReadingStats::from_store(&store);
        assert_eq!(s.chapters_tracked, 2);
        assert_eq!(s.pages_read, 15);
        assert_eq!(s.series_count, 1);
        assert_eq!(s.active_days, 1, "both rows were written just now");
        assert_eq!(s.current_streak, 1);
    }

    #[test]
    fn a_single_chapter_counts_one_of_everything() {
        let tz = tz_new_york_winter();
        let now = local(&tz, 2026, 8, 17, 9);
        let read_at = local(&tz, 2026, 8, 17, 8);
        let s = stats(
            &[("One Piece/vol2.cbz", progress(5, 200, read_at))],
            &tz,
            now,
        );

        assert_eq!(s.chapters_tracked, 1);
        assert_eq!(s.chapters_finished, 0);
        assert_eq!(s.pages_read, 6, "current_page is zero-based, so 5 -> 6");
        assert_eq!(s.series_count, 1);
        assert_eq!(s.active_days, 1);
        assert_eq!(s.first_day, Some(days_from_civil(2026, 8, 17) as u64));
        assert_eq!(s.current_streak, 1);
        assert_eq!(s.longest_streak, 1);
        assert_eq!(s.pages_on(days_from_civil(2026, 8, 17)), 6);
    }

    #[test]
    fn finished_pages_are_clamped_and_series_are_top_level_dirs() {
        let tz = LocalTimeZone::fixed(0);
        let day = local(&tz, 2026, 8, 17, 12);
        let s = stats(
            &[
                ("Berserk/vol1.cbz", progress(19, 20, day)), // finished, 20 pages
                ("Berserk/Deluxe/vol2.cbz", progress(3, 10, day)), // same series
                ("One Piece/vol1.cbz", progress(50, 20, day)), // stale total: clamped
                ("loose.cbz", progress(2, 0, day)),          // unknown total: no clamp
            ],
            &tz,
            day,
        );

        assert_eq!(s.chapters_tracked, 4);
        assert_eq!(s.chapters_finished, 2, "the 20/20 and the over-run 51/20");
        assert_eq!(s.pages_read, 20 + 4 + 20 + 3);
        assert_eq!(
            s.series_count, 3,
            "Berserk/Deluxe belongs to Berserk; a root file is its own series"
        );
    }

    #[test]
    fn a_gap_breaks_the_current_streak_but_not_the_longest() {
        let tz = tz_new_york_winter();
        // Read the 1st-4th (four days), skip the 5th and 6th, read the 7th-8th.
        let mut rows = Vec::new();
        for day in [1u32, 2, 3, 4, 7, 8] {
            rows.push((
                "Series/vol.cbz",
                progress(0, 10, local(&tz, 2026, 6, day, 20)),
            ));
        }
        // Distinct keys, otherwise they would be one chapter; the map is keyed
        // by chapter so reuse the day in the key.
        let keys: Vec<String> = (0..rows.len())
            .map(|i| format!("Series/v{i}.cbz"))
            .collect();
        let rows: Vec<(&str, ReadingProgress)> = keys
            .iter()
            .zip(&rows)
            .map(|(k, (_, p))| (k.as_str(), *p))
            .collect();

        let now = local(&tz, 2026, 6, 8, 23);
        let s = stats(&rows, &tz, now);

        assert_eq!(s.active_days, 6);
        assert_eq!(s.longest_streak, 4, "the 1st through the 4th");
        assert_eq!(s.current_streak, 2, "the 7th and 8th, ending today");

        // Two days later, with nothing read since, the current streak is gone
        // while the longest is remembered.
        let later = local(&tz, 2026, 6, 10, 12);
        let s = stats(&rows, &tz, later);
        assert_eq!(s.current_streak, 0);
        assert_eq!(s.longest_streak, 4);

        // The morning after the last read still counts: reading yesterday but
        // not yet today keeps the streak alive.
        let next_morning = local(&tz, 2026, 6, 9, 7);
        assert_eq!(stats(&rows, &tz, next_morning).current_streak, 2);
    }

    #[test]
    fn a_streak_spans_a_month_boundary() {
        let tz = tz_new_york_winter();
        // 28 Feb 2026 (a non-leap year) through 3 Mar: the calendar rolls over
        // both a short month and a month length, which day-index arithmetic
        // handles but naive "same date + 1" logic would not.
        let days = [
            (2026, 2, 27),
            (2026, 2, 28),
            (2026, 3, 1),
            (2026, 3, 2),
            (2026, 3, 3),
        ];
        let rows: Vec<(String, ReadingProgress)> = days
            .iter()
            .enumerate()
            .map(|(i, &(y, m, d))| {
                (
                    format!("Series/v{i}.cbz"),
                    progress(4, 10, local(&tz, y, m, d, 22)),
                )
            })
            .collect();
        let rows: Vec<(&str, ReadingProgress)> =
            rows.iter().map(|(k, p)| (k.as_str(), *p)).collect();

        let now = local(&tz, 2026, 3, 3, 23);
        let s = stats(&rows, &tz, now);
        assert_eq!(s.active_days, 5);
        assert_eq!(s.longest_streak, 5, "27 Feb → 3 Mar is unbroken");
        assert_eq!(s.current_streak, 5);
        assert_eq!(s.first_day, Some(days_from_civil(2026, 2, 27) as u64));

        // And across a leap day, where February has 29 days.
        let leap: Vec<(String, ReadingProgress)> = [(2024, 2, 28), (2024, 2, 29), (2024, 3, 1)]
            .iter()
            .enumerate()
            .map(|(i, &(y, m, d))| {
                (
                    format!("S/v{i}.cbz"),
                    progress(0, 10, local(&tz, y, m, d, 10)),
                )
            })
            .collect();
        let leap: Vec<(&str, ReadingProgress)> =
            leap.iter().map(|(k, p)| (k.as_str(), *p)).collect();
        let s = stats(&leap, &tz, local(&tz, 2024, 3, 1, 12));
        assert_eq!(s.longest_streak, 3);
    }

    #[test]
    fn heatmap_grid_shape_and_quantisation() {
        let tz = LocalTimeZone::fixed(0);
        let today = days_from_civil(2026, 8, 17); // a Monday
        let now = (today * SECS_PER_DAY + 12 * 3600) as u64;

        // A busiest day of 40 pages makes the level thresholds 1..=10, 11..=20,
        // 21..=30, 31..=40 — the same relative ramp the web draws.
        let rows: Vec<(String, ReadingProgress)> =
            [(0i64, 40usize), (1, 30), (2, 21), (3, 11), (4, 1)]
                .iter()
                .enumerate()
                .map(|(i, &(back, pages))| {
                    let at = ((today - back) * SECS_PER_DAY + 10 * 3600) as u64;
                    (format!("S/v{i}.cbz"), progress(pages - 1, pages, at))
                })
                .collect();
        let rows: Vec<(&str, ReadingProgress)> =
            rows.iter().map(|(k, p)| (k.as_str(), *p)).collect();
        let s = stats(&rows, &tz, now);
        assert_eq!(s.max_day(), 40);

        const WEEKS: usize = 18;
        let start = s.heatmap_start(WEEKS);
        assert_eq!(weekday(start), 0, "the grid always begins on a Sunday");
        assert!(start <= s.today - (WEEKS as i64 * 7 - 1));

        let grid = s.heatmap(WEEKS);
        assert_eq!(
            grid.len(),
            WEEKS + 1,
            "a partial leading week adds a column"
        );
        assert!(grid.iter().all(|c| c.len() == 7));

        // Look the five active days up by position in the grid.
        let level_at = |day: i64| -> u8 {
            let off = (day - start) as usize;
            grid[off / 7][off % 7]
        };
        assert_eq!(level_at(today), 4, "40/40 -> 4");
        assert_eq!(level_at(today - 1), 3, "30/40 -> ceil(3.0) = 3");
        assert_eq!(level_at(today - 2), 3, "21/40 -> ceil(2.1) = 3");
        assert_eq!(level_at(today - 3), 2, "11/40 -> ceil(1.1) = 2");
        assert_eq!(level_at(today - 4), 1, "1/40 -> ceil(0.1) = 1, never 0");
        assert_eq!(level_at(today - 5), 0, "a day with no reading");

        // Today is a Monday, so the rest of its column is future padding.
        let last = grid.last().unwrap();
        assert_eq!(last[weekday(today) as usize], 4);
        assert!(
            last[(weekday(today) + 1) as usize..]
                .iter()
                .all(|&l| l == 0),
            "days after today are level 0 padding"
        );

        // Every level stays in range for a pathological single-page maximum.
        let one = stats(&[("S/v.cbz", progress(0, 1, now))], &tz, now);
        assert!(one.heatmap(4).iter().all(|c| c.iter().all(|&l| l <= 4)));
        assert_eq!(quantise(0, 0), 0, "no data means no shading");
        assert_eq!(quantise(7, 7), 4);
    }

    #[test]
    fn tzif_parsing_survives_garbage_and_falls_back_to_utc() {
        assert!(LocalTimeZone::from_tzif(b"").is_none());
        assert!(LocalTimeZone::from_tzif(b"not a tzif file at all").is_none());
        // Right magic, truncated body: still no panic, still None.
        assert!(LocalTimeZone::from_tzif(b"TZif2\0\0\0").is_none());
        assert!(LocalTimeZone::utc().is_utc_fallback());
        assert!(!LocalTimeZone::fixed(0).is_utc_fallback());
    }

    #[test]
    fn tzif_transitions_pick_the_offset_in_force_at_each_timestamp() {
        // A hand-built v1 TZif with one transition: -05:00 before it, -04:00
        // after — i.e. a DST start. A timestamp from before the transition
        // must still use the winter offset.
        let switch = local(&LocalTimeZone::fixed(-5 * 3600), 2026, 3, 8, 2) as i64;
        let mut f = Vec::new();
        f.extend_from_slice(b"TZif");
        f.push(b'1');
        f.extend_from_slice(&[0u8; 15]);
        for count in [0u32, 0, 0, 1, 2, 0] {
            f.extend_from_slice(&count.to_be_bytes()); // isut, isstd, leap, time, type, char
        }
        f.extend_from_slice(&(switch as i32).to_be_bytes());
        f.push(1); // that transition uses type 1
        f.extend_from_slice(&(-5i32 * 3600).to_be_bytes());
        f.extend_from_slice(&[0, 0]); // type 0: standard
        f.extend_from_slice(&(-4i32 * 3600).to_be_bytes());
        f.extend_from_slice(&[1, 0]); // type 1: DST

        let tz = LocalTimeZone::from_tzif(&f).expect("parses");
        assert_eq!(tz.offset_at(switch - 1), -5 * 3600);
        assert_eq!(tz.offset_at(switch), -4 * 3600);
        assert_eq!(tz.offset_at(switch + 86_400), -4 * 3600);
        assert!(!tz.is_utc_fallback());

        // The spring-forward day is only 23 hours of UTC long, so day indices
        // must come from the *local* clock: 22 hours after the 02:00 switch it
        // is 01:00 the next local day, one day index later — while a UTC
        // bucketing would still call it the same day.
        let before = tz.day_index(switch - 3600); // 01:00 local, before the jump
        let after = tz.day_index(switch + 22 * 3600); // 01:00 local, next day
        assert_eq!(after - before, 1);
        assert_eq!(civil_from_days(before), (2026, 3, 8));
        assert_eq!(civil_from_days(after), (2026, 3, 9));
    }
}
