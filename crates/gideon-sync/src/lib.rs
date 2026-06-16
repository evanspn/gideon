//! Cross-platform reading-progress sync.
//!
//! Reconciles gideon's local [`gideon_core::ProgressStore`] with a remote
//! backend (Supabase — see `docs/SYNC.md` and
//! `supabase/migrations/0001_reading_progress.sql`) using **furthest-page-wins**:
//! a chapter's page only ever advances, never rewinds, no matter which device
//! synced last. The reconcile rules are pure and the network lives behind the
//! [`ProgressTransport`] trait, so the conflict behaviour is fully unit-tested
//! without a live backend; the real HTTP transport drops in once the project
//! is provisioned.
//!
//! Offline-first: the local store stays authoritative. A sync pulls remote
//! rows and merges them in (never lowering a page), then pushes the chapters
//! the device is ahead on. A failure leaves the local store untouched.

use std::collections::BTreeMap;

use gideon_core::ProgressStore;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sync transport error: {0}")]
    Transport(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A remote progress row (one per user + chapter), as returned by a pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteProgress {
    pub chapter_key: String,
    pub current_page: usize,
    pub total_pages: usize,
    /// Server timestamp (RFC3339). Used **only** to advance the pull cursor,
    /// never to decide conflicts — clock skew between devices isn't trusted.
    pub updated_at: String,
}

/// A change to push: the arguments to the `upsert_progress` RPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressUpdate {
    pub chapter_key: String,
    pub current_page: usize,
    pub total_pages: usize,
}

/// The network boundary. The real implementation talks to Supabase
/// (PostgREST `select` for pull, the `upsert_progress` RPC for push) with the
/// user's JWT; tests use a fake.
pub trait ProgressTransport {
    /// Rows updated since `cursor` (the server timestamp of the newest row
    /// seen so far), or all of the user's rows when `None`.
    fn pull(&self, cursor: Option<&str>) -> Result<Vec<RemoteProgress>>;
    /// Upsert a batch through the furthest-page-wins RPC.
    fn push(&self, updates: &[ProgressUpdate]) -> Result<()>;
}

/// Persisted sync bookkeeping (stored next to the progress store): the pull
/// cursor and, per chapter, the page the server is known to already have — so
/// freshly-pulled rows aren't pushed straight back and the device only pushes
/// where it's genuinely ahead.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub server_pages: BTreeMap<String, usize>,
}

/// What a [`Syncer::sync`] did, for logging/UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncOutcome {
    pub pulled: usize,
    pub merged: usize,
    pub pushed: usize,
}

/// Merge remote rows into `local` with furthest-page-wins, advancing the pull
/// cursor and recording what the server already has. Returns how many local
/// chapters actually changed. A remote row never lowers a local page; a row's
/// `total_pages` (a re-scan may change it) is taken as-is. Only writes the
/// local store when something actually changes, so a no-op merge doesn't churn
/// `last_read_at` / "continue reading" order.
pub fn merge_remote(
    local: &mut ProgressStore,
    remote: &[RemoteProgress],
    state: &mut SyncState,
) -> usize {
    let mut changed = 0;
    for r in remote {
        let existing = local.get(&r.chapter_key);
        let merged_page = existing.map_or(r.current_page, |l| l.current_page.max(r.current_page));
        let needs_write = existing
            .is_none_or(|l| l.current_page != merged_page || l.total_pages != r.total_pages);
        if needs_write {
            local.update(&r.chapter_key, merged_page, r.total_pages);
            changed += 1;
        }
        // The server is known to have at least the remote page for this key.
        let known = state.server_pages.entry(r.chapter_key.clone()).or_insert(0);
        *known = (*known).max(r.current_page);
        // Advance the cursor to the newest server timestamp seen.
        if state
            .cursor
            .as_deref()
            .is_none_or(|c| r.updated_at.as_str() > c)
        {
            state.cursor = Some(r.updated_at.clone());
        }
    }
    changed
}

/// The local chapters the device is ahead on (page beyond what the server is
/// known to have) — the batch to push.
pub fn pending_pushes(local: &ProgressStore, state: &SyncState) -> Vec<ProgressUpdate> {
    let mut out: Vec<ProgressUpdate> = local
        .entries()
        .filter(|(k, p)| {
            state
                .server_pages
                .get(*k)
                .is_none_or(|&s| p.current_page > s)
        })
        .map(|(k, p)| ProgressUpdate {
            chapter_key: k.to_string(),
            current_page: p.current_page,
            total_pages: p.total_pages,
        })
        .collect();
    // Deterministic order (HashMap iteration isn't) — nicer for batching/tests.
    out.sort_by(|a, b| a.chapter_key.cmp(&b.chapter_key));
    out
}

/// Orchestrates one reconcile against a [`ProgressTransport`].
pub struct Syncer<T> {
    transport: T,
}

impl<T: ProgressTransport> Syncer<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Pull → merge (furthest-wins) → push the chapters we're ahead on. The
    /// local store and `state` are only mutated on success of each step; a
    /// transport error leaves the local store consistent (offline-first: the
    /// caller keeps reading from the unchanged local store).
    pub fn sync(&self, local: &mut ProgressStore, state: &mut SyncState) -> Result<SyncOutcome> {
        let remote = self.transport.pull(state.cursor.as_deref())?;
        let merged = merge_remote(local, &remote, state);

        let pending = pending_pushes(local, state);
        if !pending.is_empty() {
            self.transport.push(&pending)?;
            for u in &pending {
                // The server now has this page; don't push it again next time.
                state
                    .server_pages
                    .insert(u.chapter_key.clone(), u.current_page);
            }
        }
        Ok(SyncOutcome {
            pulled: remote.len(),
            merged,
            pushed: pending.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Scripted transport: `pull` returns the queued rows once; `push` records
    /// every batch it's handed.
    struct FakeTransport {
        to_pull: RefCell<Vec<RemoteProgress>>,
        pushed: RefCell<Vec<ProgressUpdate>>,
    }
    impl FakeTransport {
        fn new(pull: Vec<RemoteProgress>) -> Self {
            Self {
                to_pull: RefCell::new(pull),
                pushed: RefCell::new(Vec::new()),
            }
        }
    }
    impl ProgressTransport for FakeTransport {
        fn pull(&self, _cursor: Option<&str>) -> Result<Vec<RemoteProgress>> {
            Ok(std::mem::take(&mut *self.to_pull.borrow_mut()))
        }
        fn push(&self, updates: &[ProgressUpdate]) -> Result<()> {
            self.pushed.borrow_mut().extend_from_slice(updates);
            Ok(())
        }
    }

    fn remote(key: &str, page: usize, total: usize, at: &str) -> RemoteProgress {
        RemoteProgress {
            chapter_key: key.into(),
            current_page: page,
            total_pages: total,
            updated_at: at.into(),
        }
    }

    #[test]
    fn remote_ahead_advances_local() {
        let mut local = ProgressStore::default();
        local.update("a", 2, 20);
        let mut state = SyncState::default();
        merge_remote(&mut local, &[remote("a", 5, 20, "t1")], &mut state);
        assert_eq!(
            local.get("a").unwrap().current_page,
            5,
            "advances to the remote page"
        );
    }

    #[test]
    fn remote_behind_never_rewinds_and_is_pushed_back() {
        let mut local = ProgressStore::default();
        local.update("a", 9, 20);
        let mut state = SyncState::default();
        merge_remote(&mut local, &[remote("a", 3, 20, "t1")], &mut state);
        assert_eq!(
            local.get("a").unwrap().current_page,
            9,
            "a behind remote never rewinds us"
        );
        // We're ahead of the server (it had 3), so 'a' is pending to push.
        let pending = pending_pushes(&local, &state);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].current_page, 9);
    }

    #[test]
    fn freshly_pulled_rows_are_not_pushed_back() {
        let mut local = ProgressStore::default();
        let mut state = SyncState::default();
        // A brand-new remote chapter we didn't have locally.
        merge_remote(&mut local, &[remote("b", 4, 30, "t2")], &mut state);
        assert_eq!(local.get("b").unwrap().current_page, 4);
        assert!(
            pending_pushes(&local, &state).is_empty(),
            "a row we just pulled must not be pushed straight back"
        );
    }

    #[test]
    fn cursor_advances_to_newest_timestamp() {
        let mut local = ProgressStore::default();
        let mut state = SyncState::default();
        merge_remote(
            &mut local,
            &[
                remote("a", 1, 10, "2026-01-01"),
                remote("b", 1, 10, "2026-03-09"),
            ],
            &mut state,
        );
        assert_eq!(state.cursor.as_deref(), Some("2026-03-09"));
    }

    #[test]
    fn full_sync_pushes_local_then_is_idempotent() {
        let mut local = ProgressStore::default();
        local.update("a", 7, 20); // device is ahead, nothing remote yet
        let mut state = SyncState::default();

        let syncer = Syncer::new(FakeTransport::new(vec![]));
        let out = syncer.sync(&mut local, &mut state).unwrap();
        assert_eq!(out.pushed, 1, "the ahead chapter is pushed once");
        assert_eq!(syncer.transport.pushed.borrow().len(), 1);

        // Nothing changed locally: a second sync pushes nothing.
        let out2 = syncer.sync(&mut local, &mut state).unwrap();
        assert_eq!(out2.pushed, 0, "no re-push when nothing advanced");

        // Read further, then sync: only the delta goes up.
        local.update("a", 12, 20);
        let out3 = syncer.sync(&mut local, &mut state).unwrap();
        assert_eq!(out3.pushed, 1);
        assert_eq!(
            syncer
                .transport
                .pushed
                .borrow()
                .last()
                .unwrap()
                .current_page,
            12
        );
    }

    #[test]
    fn sync_merges_pull_and_push_together() {
        let mut local = ProgressStore::default();
        local.update("mine", 8, 40); // ahead locally
        local.update("shared", 2, 20); // behind remote
        let mut state = SyncState::default();

        let syncer = Syncer::new(FakeTransport::new(vec![
            remote("shared", 15, 20, "t1"), // remote ahead → should win
            remote("theirs", 3, 10, "t2"),  // new from another device
        ]));
        let out = syncer.sync(&mut local, &mut state).unwrap();

        assert_eq!(
            local.get("shared").unwrap().current_page,
            15,
            "remote-ahead wins"
        );
        assert_eq!(
            local.get("theirs").unwrap().current_page,
            3,
            "new remote chapter pulled in"
        );
        assert_eq!(
            local.get("mine").unwrap().current_page,
            8,
            "local untouched"
        );
        // Only 'mine' (ahead of the server) is pushed.
        let pushed = syncer.transport.pushed.borrow();
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0].chapter_key, "mine");
        assert_eq!(out.pulled, 2);
    }
}
