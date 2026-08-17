//! Reading-progress sync wiring for the device app.
//!
//! Bridges the app to [`gideon_sync`]: it holds the (build-default) Supabase
//! connection, builds a per-profile [`Account`], and kicks reconciles onto a
//! detached background thread so the network never touches the UI or reader
//! thread. Every sync is best-effort — offline-first means a failure is logged
//! and dropped, and the local `progress.json` stays authoritative.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gideon_sync::account::Account;
use gideon_sync::supabase::{SendItem, SupabaseConfig};

use crate::ui::SourceGateway;

/// Chapters whose page URLs are resolved per publish sweep. Resolving hits the
/// source over the network, so a big first-time library lights up over several
/// sweeps (each library-open / sign-in / chapter-close triggers one) rather
/// than in one long burst.
const PUBLISH_SWEEP_LIMIT: usize = 25;

/// True while a background reconcile is in flight, so overlapping triggers
/// (library-open then a quick chapter-close) don't run two syncs at once —
/// which would race each other's writes and double-spend the rotating refresh
/// token. The next trigger after this clears will catch up.
static SYNC_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Clears [`SYNC_IN_FLIGHT`] on drop, so the flag is released even if the sync
/// thread panics (unwinding runs `Drop`) — otherwise one panic would wedge
/// background sync off for the rest of the process.
struct InFlightGuard;
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        SYNC_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// How many times a reconcile is attempted before it's abandoned, and the base
/// wait between attempts (the wait grows with each retry).
const SYNC_ATTEMPTS: usize = 3;
const SYNC_RETRY_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long [`sync_before_sleep`] holds the device awake waiting for a
/// reconcile to land. Short on purpose: the push is one small request, and a
/// device asked to sleep must sleep — anything unsent is retried after wake.
const PRE_SLEEP_BUDGET: std::time::Duration = std::time::Duration::from_secs(4);

/// How long [`spawn_sync_when_online`] keeps waiting for the radio to come back
/// before giving up, and how often it re-checks. Bounded so a device that wakes
/// out of Wi-Fi range doesn't keep a thread (or the radio's attention) forever.
const ONLINE_WAIT: std::time::Duration = std::time::Duration::from_secs(120);
const ONLINE_POLL: std::time::Duration = std::time::Duration::from_secs(3);

/// True while a wait-for-network sync is already pending, so repeated wakes
/// don't stack up waiters all racing the same reconcile.
static WAITING_FOR_ONLINE: AtomicBool = AtomicBool::new(false);

/// Clears [`WAITING_FOR_ONLINE`] on any exit, panic included.
struct WaitingGuard;
impl Drop for WaitingGuard {
    fn drop(&mut self) {
        WAITING_FOR_ONLINE.store(false, Ordering::Release);
    }
}

/// The outcome of the most recent sync attempt this session, for the account
/// screen. Process-local by design: after a restart there's simply no status
/// until something syncs, which is honest — better than showing a stale
/// "synced" from a previous run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    Ok { at: u64 },
    Failed(String),
}

static STATUS: std::sync::Mutex<Option<SyncStatus>> = std::sync::Mutex::new(None);

fn set_status(status: SyncStatus) {
    if let Ok(mut slot) = STATUS.lock() {
        *slot = Some(status);
    }
}

/// The last sync attempt's outcome, if there's been one this session.
pub fn status() -> Option<SyncStatus> {
    STATUS.lock().ok().and_then(|s| s.clone())
}

/// Reset the session status (tests only — the status is process-global, so a
/// test that asserts "nothing yet" has to clear what another one set).
#[cfg(test)]
fn set_status_for_test(status: Option<SyncStatus>) {
    if let Ok(mut slot) = STATUS.lock() {
        *slot = status;
    }
}

/// A one-line summary of [`status`] for the account screen — `None` when
/// nothing has been attempted yet this run.
pub fn status_line() -> Option<String> {
    match status()? {
        SyncStatus::Ok { at } => Some(format!("Last synced {}", ago(now().saturating_sub(at)))),
        // The message itself is a transport/auth string; keep the row short and
        // say what it means for the user instead.
        SyncStatus::Failed(_) => Some("Last sync failed — will retry".to_string()),
    }
}

/// "just now" / "4 min ago" / "2 h ago" — coarse on purpose.
fn ago(secs: u64) -> String {
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86399 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86400),
    }
}

/// Whether the device is holding reading progress the server hasn't got yet.
/// Cheap (two small JSON reads) and used to decide whether a sync is worth
/// delaying sleep for at all.
pub fn has_unpushed(library_dir: &Path) -> bool {
    let Some(account) = account(library_dir) else {
        return false;
    };
    account.has_unpushed()
}

/// Flush reading progress before the device suspends.
///
/// This is the moment that matters most: you finish a chapter, put the device
/// down, and it naps — with nothing else to trigger a sync until you next open
/// the library. Deliberately conservative:
///
/// * does nothing when signed out, when there's nothing unpushed, or when the
///   device is offline — a nap must never be delayed to talk to a dead radio,
///   and Wi-Fi is never brought UP for this;
/// * waits at most [`PRE_SLEEP_BUDGET`] for the reconcile, then suspends
///   regardless. The half-finished request dies with the suspend and is retried
///   after wake — the local store is authoritative, so nothing is lost.
pub fn sync_before_sleep(library_dir: &Path, gateway: Option<Box<dyn SourceGateway + Send>>) {
    if !gideon_device::network::is_online() || !has_unpushed(library_dir) {
        return;
    }
    spawn_sync(library_dir, gateway);
    let deadline = std::time::Instant::now() + PRE_SLEEP_BUDGET;
    while SYNC_IN_FLIGHT.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Sync as soon as the network is actually back, for up to [`ONLINE_WAIT`].
///
/// The wake path kicks a Wi-Fi reconnect that completes asynchronously, so a
/// sync fired at that moment reliably fails against a radio that hasn't
/// associated yet. This waits for `is_online` instead of guessing — polling a
/// local sysfs read, never the network — and only then reconciles. It never
/// brings the radio up itself, gives up rather than waiting forever, and is
/// single-flight, so repeated wakes don't stack waiters.
pub fn spawn_sync_when_online(library_dir: &Path, gateway: Option<Box<dyn SourceGateway + Send>>) {
    let Some(account) = account(library_dir) else {
        return;
    };
    if !account.is_signed_in() {
        return;
    }
    if WAITING_FOR_ONLINE.swap(true, Ordering::AcqRel) {
        return; // a waiter is already pending
    }
    let library_dir = library_dir.to_path_buf();
    std::thread::spawn(move || {
        let _guard = WaitingGuard;
        gideon_device::power::lower_current_thread_to_idle();
        let deadline = std::time::Instant::now() + ONLINE_WAIT;
        while !gideon_device::network::is_online() {
            if std::time::Instant::now() >= deadline {
                return; // still no network — the next trigger will try again
            }
            std::thread::sleep(ONLINE_POLL);
        }
        spawn_sync(&library_dir, gateway);
    });
}

/// Default Supabase project the app syncs against. The anon key is public by
/// design — row-level security (keyed to `auth.uid()`), not this key, is what
/// protects a user's rows — so shipping it in the binary is expected. Both can
/// be overridden at runtime with `GIDEON_SUPABASE_URL` / `GIDEON_SUPABASE_ANON_KEY`
/// (e.g. to point a dev build at a throwaway project).
const DEFAULT_SUPABASE_URL: &str = "https://sqlkceqkdtmejhdoycsr.supabase.co";
const DEFAULT_SUPABASE_ANON_KEY: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InNxbGtjZXFrZHRtZWpoZG95Y3NyIiwicm9sZSI6ImFub24iLCJpYXQiOjE3ODMyOTE5MDAsImV4cCI6MjA5ODg2NzkwMH0.K8kXfcIihjw0Mz5qm1hW7nXHcymhN-yMLrV6CaLU1eo";

/// Current unix time in seconds (for token-expiry math).
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The active Supabase connection: env overrides, else the build defaults.
/// Returns `None` only if an override sets an empty value, so a deployment can
/// disable sync entirely by exporting an empty URL.
pub fn config() -> Option<SupabaseConfig> {
    let url = env_or("GIDEON_SUPABASE_URL", DEFAULT_SUPABASE_URL);
    let anon_key = env_or("GIDEON_SUPABASE_ANON_KEY", DEFAULT_SUPABASE_ANON_KEY);
    if url.is_empty() || anon_key.is_empty() {
        return None;
    }
    Some(SupabaseConfig { url, anon_key })
}

fn env_or(var: &str, default: &str) -> String {
    match std::env::var(var) {
        Ok(v) => v,
        Err(_) => default.to_string(),
    }
}

/// The `.gideon` directory for a profile's `library_dir` — where its
/// `progress.json` and sync bookkeeping live.
pub fn gideon_dir(library_dir: &Path) -> PathBuf {
    library_dir.join(".gideon")
}

/// Build an [`Account`] for the given profile library, if sync is configured.
pub fn account(library_dir: &Path) -> Option<Account> {
    config().map(|c| Account::new(c, gideon_dir(library_dir)))
}

/// Kick a reconcile for `library_dir` on a detached thread, if signed in.
/// Returns immediately; never blocks the caller. Errors (offline, expired
/// session that can't refresh) are logged and dropped — the next trigger
/// retries. Safe to call from the UI thread: nothing here does I/O inline.
///
/// `gateway` (a background-thread-safe clone of the source gateway, if
/// available) enables the page-URL publish sweep after the progress reconcile:
/// the device resolves each downloaded chapter's page image URLs and publishes
/// them to `chapter_pages`, which is what lets the web read them (only the
/// device's home IP can reach the sources; a datacenter can't). Running the
/// sweep on the same thread, right after the reconcile, reuses the just-
/// refreshed session and never races it.
pub fn spawn_sync(library_dir: &Path, gateway: Option<Box<dyn SourceGateway + Send>>) {
    let Some(account) = account(library_dir) else {
        return;
    };
    if !account.is_signed_in() {
        return; // no session — nothing to sync, and no auth to attempt
    }
    // Only one reconcile at a time; a trigger that arrives mid-sync is dropped
    // (the running sync, or the next trigger, covers it).
    if SYNC_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    let library_dir = library_dir.to_path_buf();
    std::thread::spawn(move || {
        // Releases the in-flight flag on any exit, including a panic unwind.
        let _guard = InFlightGuard;
        // Retry a failed reconcile a couple of times before giving up. The
        // usual cause is a radio that isn't back yet (a nap, a walk out of
        // range), which fixes itself in seconds — and a dropped sync used to
        // mean your place simply never reached the web until you happened to
        // open the library while online.
        let mut outcome = None;
        for attempt in 0..SYNC_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(SYNC_RETRY_WAIT * attempt as u32);
                if !gideon_device::network::is_online() {
                    continue; // still no network — don't burn an attempt on it
                }
            }
            match account.sync(now()) {
                Ok(o) => {
                    outcome = Some(o);
                    break;
                }
                Err(e) => set_status(SyncStatus::Failed(e.to_string())),
            }
        }
        match outcome {
            Some(outcome) => {
                set_status(SyncStatus::Ok { at: now() });
                if outcome.merged > 0 || outcome.pushed > 0 {
                    eprintln!(
                        "sync: merged {} pulled row(s), pushed {}",
                        outcome.merged, outcome.pushed
                    );
                }
            }
            None => {
                eprintln!("sync: gave up after {SYNC_ATTEMPTS} attempts");
                return; // offline: resolving/publishing pages would fail too
            }
        }
        // Pull the "send to Kobo" queue and cache it locally so the Home
        // notification badge works offline (best-effort, like everything here).
        match account.fetch_sends(now()) {
            Ok(sends) => write_sends_cache(&library_dir, &sends),
            Err(e) => eprintln!("sync: couldn't fetch sends ({e})"),
        }
        if let Some(gateway) = gateway {
            publish_pages_sweep(&library_dir, gateway.as_ref(), &account, now());
        }
    });
}

/// Local cache of the pending "send to Kobo" queue, beside the sync
/// bookkeeping. The sweep writes it; the Home screen reads it to show a
/// notification badge without hitting the network on every render.
const SENDS_CACHE_FILE: &str = "sends.json";

fn sends_cache_path(library_dir: &Path) -> PathBuf {
    gideon_dir(library_dir).join(SENDS_CACHE_FILE)
}

fn write_sends_cache(library_dir: &Path, sends: &[SendItem]) {
    let path = sends_cache_path(library_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(sends) {
        let _ = std::fs::write(path, bytes);
    }
}

/// The pending sends cached by the last sweep (empty if none / never synced).
pub fn cached_sends(library_dir: &Path) -> Vec<SendItem> {
    std::fs::read(sends_cache_path(library_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Drop a send from the local cache once it's been opened, so the badge and
/// list update immediately rather than after the next sweep.
pub fn forget_cached_send(library_dir: &Path, id: &str) {
    let mut sends = cached_sends(library_dir);
    let before = sends.len();
    sends.retain(|s| s.id != id);
    if sends.len() != before {
        write_sends_cache(library_dir, &sends);
    }
}

/// Mark a send `opened` on the server on a detached thread (best-effort), so it
/// clears from the queue and isn't offered again on the next device.
pub fn mark_send_opened_bg(library_dir: &Path, id: &str) {
    let Some(account) = account(library_dir) else {
        return;
    };
    let id = id.to_string();
    std::thread::spawn(move || {
        if let Err(e) = account.mark_send_opened(now(), &id) {
            eprintln!("sync: couldn't mark send opened ({e})");
        }
    });
}

/// Publish page URLs for downloaded chapters that haven't been published yet,
/// so the web reader can open them. Bounded to [`PUBLISH_SWEEP_LIMIT`] source
/// resolutions per call; the rest are picked up by later sweeps. Best-effort
/// throughout: a chapter that can't be resolved right now is retried next
/// sweep, and a publish (network) failure stops the sweep to retry later.
fn publish_pages_sweep(
    library_dir: &Path,
    gateway: &(dyn SourceGateway + Send),
    account: &Account,
    now: u64,
) {
    // Resolving and uploading is background work — never let it contend with the
    // reader for CPU/IO.
    gideon_device::power::lower_current_thread_to_idle();

    let index = gideon_core::SeriesIndex::load(library_dir);
    let already = account.published_pages();
    let mut newly: Vec<String> = Vec::new();
    let mut resolved = 0usize;

    'sweep: for (dir, series) in index.iter() {
        for (chapter_id, file_name) in &series.downloaded {
            let key = format!("{dir}/{file_name}");
            if already.contains(&key) {
                continue; // already readable on the web
            }
            // Skip chapters whose file was evicted/deleted; publishing URLs for
            // a chapter no longer on the shelf would be misleading.
            if !library_dir.join(dir).join(file_name).exists() {
                continue;
            }
            if resolved >= PUBLISH_SWEEP_LIMIT {
                break 'sweep;
            }
            resolved += 1;
            let urls =
                match gateway.resolve_page_urls(&series.source_id, &series.manga_id, chapter_id) {
                    Ok(urls) if !urls.is_empty() => urls,
                    _ => continue, // couldn't resolve now — try again next sweep
                };
            match account.publish_chapter_pages(now, &key, &urls) {
                Ok(()) => newly.push(key),
                Err(e) => {
                    // Network/auth failure: stop here and let the next trigger
                    // resume, rather than hammering a dead connection.
                    eprintln!("sync: page publish stopped ({e})");
                    break 'sweep;
                }
            }
        }
    }

    if let Err(e) = account.mark_pages_published(&newly) {
        eprintln!("sync: couldn't record published pages ({e})");
    } else if !newly.is_empty() {
        eprintln!("sync: published page URLs for {} chapter(s)", newly.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_default_config_is_present_and_well_formed() {
        // With no env override, the embedded project is used.
        std::env::remove_var("GIDEON_SUPABASE_URL");
        std::env::remove_var("GIDEON_SUPABASE_ANON_KEY");
        let c = config().expect("build default config exists");
        assert!(c.url.starts_with("https://") && c.url.ends_with(".supabase.co"));
        assert!(c.anon_key.starts_with("eyJ"), "anon key is a JWT");
    }

    #[test]
    fn ago_reads_as_a_glance_not_a_stopwatch() {
        assert_eq!(ago(0), "just now");
        assert_eq!(ago(59), "just now");
        assert_eq!(ago(60), "1 min ago");
        assert_eq!(ago(4 * 60 + 30), "4 min ago");
        assert_eq!(ago(3 * 3600), "3 h ago");
        assert_eq!(ago(50 * 3600), "2 d ago");
    }

    #[test]
    fn status_line_says_when_it_worked_and_when_it_didnt() {
        // Nothing attempted yet this run: no row at all, rather than a stale or
        // invented "synced".
        set_status_for_test(None);
        assert_eq!(status_line(), None);

        set_status(SyncStatus::Ok { at: now() });
        assert_eq!(status_line().as_deref(), Some("Last synced just now"));

        // A failure is stated plainly — this is the thing that used to be
        // completely invisible — without leaking the transport error text.
        set_status(SyncStatus::Failed("401 Unauthorized".into()));
        let line = status_line().expect("a failure shows a row");
        assert!(line.starts_with("Last sync failed"), "{line}");
        assert!(!line.contains("401"), "{line}");
        set_status_for_test(None);
    }

    #[test]
    fn sleep_is_never_delayed_when_there_is_nothing_to_send() {
        // No account dir / signed out: `sync_before_sleep` must fall straight
        // through. A device asked to sleep sleeps.
        let dir = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        sync_before_sleep(dir.path(), None);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "signed out, the pre-sleep flush returned in {:?}",
            start.elapsed()
        );
        assert!(!has_unpushed(dir.path()));
    }

    #[test]
    fn gideon_dir_sits_under_the_profile_library() {
        assert_eq!(
            gideon_dir(Path::new("/data/Manga/@alice")),
            Path::new("/data/Manga/@alice/.gideon")
        );
    }
}
