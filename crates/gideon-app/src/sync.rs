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
use gideon_sync::supabase::SupabaseConfig;

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
        match account.sync(now()) {
            Ok(outcome) => {
                if outcome.merged > 0 || outcome.pushed > 0 {
                    eprintln!(
                        "sync: merged {} pulled row(s), pushed {}",
                        outcome.merged, outcome.pushed
                    );
                }
            }
            Err(e) => {
                eprintln!("sync: skipped ({e})");
                return; // offline: resolving/publishing pages would fail too
            }
        }
        if let Some(gateway) = gateway {
            publish_pages_sweep(&library_dir, gateway.as_ref(), &account, now());
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
    fn gideon_dir_sits_under_the_profile_library() {
        assert_eq!(
            gideon_dir(Path::new("/data/Manga/@alice")),
            Path::new("/data/Manga/@alice/.gideon")
        );
    }
}
