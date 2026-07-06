//! Per-profile account + sync orchestration — the one type the app drives.
//!
//! Everything an account needs lives inside a single profile's `.gideon`
//! directory, right beside its `progress.json`:
//!
//! - `sync_session.json` — the signed-in [`Session`] (tokens + email)
//! - `sync_state.json` — the [`SyncState`] pull cursor + per-chapter server high-water marks
//!
//! Keeping these **per profile** is what binds a profile to exactly one cloud
//! account: switching profiles switches every one of these files, so one
//! profile's reading can never sync into another account. And signing a profile
//! into a *different* account resets the sync bookkeeping (see
//! [`Account::verify`]), so a previous user's cursor can never pull their rows
//! into this profile's store.
//!
//! The project connection ([`SupabaseConfig`]) is device-global (it identifies
//! the backend, not the user), so it's passed in rather than stored here.

use std::path::{Path, PathBuf};

use gideon_core::ProgressStore;

use crate::supabase::{AuthClient, Session, SupabaseConfig, SupabaseTransport};
use crate::{Result, SyncOutcome, SyncState, Syncer};

const SESSION_FILE: &str = "sync_session.json";
const STATE_FILE: &str = "sync_state.json";
const PROGRESS_FILE: &str = "progress.json";

/// Reading-progress sync for one profile against one Supabase project.
pub struct Account {
    config: SupabaseConfig,
    /// The profile's `.gideon` directory (holds progress + sync bookkeeping).
    dir: PathBuf,
}

impl Account {
    /// `gideon_dir` is the profile's `.gideon` directory (the one that holds
    /// `progress.json`).
    pub fn new(config: SupabaseConfig, gideon_dir: impl Into<PathBuf>) -> Self {
        Self {
            config,
            dir: gideon_dir.into(),
        }
    }

    fn session_path(&self) -> PathBuf {
        self.dir.join(SESSION_FILE)
    }
    fn state_path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }
    fn progress_path(&self) -> PathBuf {
        self.dir.join(PROGRESS_FILE)
    }

    /// The signed-in session, if any (unreadable/absent ⇒ `None`).
    pub fn session(&self) -> Option<Session> {
        let bytes = std::fs::read(self.session_path()).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// The email of the signed-in account, for the account UI.
    pub fn email(&self) -> Option<String> {
        self.session().map(|s| s.email)
    }

    pub fn is_signed_in(&self) -> bool {
        self.session().is_some()
    }

    /// Step 1 of sign-in: email a one-time code.
    pub fn request_code(&self, email: &str) -> Result<()> {
        AuthClient::new(self.config.clone()).request_code(email)
    }

    /// Step 2 of sign-in: verify the code, persist the session, and — if this
    /// is a *different* account than was signed in before — reset the sync
    /// bookkeeping so the previous user's cursor can't pull their rows into this
    /// profile. The local `progress.json` is left untouched (offline-first: it's
    /// authoritative and a following sync reconciles it).
    pub fn verify(&self, email: &str, code: &str, now: u64) -> Result<Session> {
        let previous_email = self.email();
        let session = AuthClient::new(self.config.clone()).verify_code(email, code, now)?;

        if previous_email.as_deref() != Some(session.email.as_str()) {
            // New identity on this profile — drop any cursor/high-water marks
            // that belonged to the old account.
            let _ = std::fs::remove_file(self.state_path());
        }
        self.save_session(&session)?;
        Ok(session)
    }

    /// Sign out: forget the session and the sync cursor (so a later sign-in
    /// starts clean), but keep local `progress.json` — reading works offline.
    pub fn sign_out(&self) -> Result<()> {
        let _ = std::fs::remove_file(self.session_path());
        let _ = std::fs::remove_file(self.state_path());
        Ok(())
    }

    /// Reconcile this profile's progress with the cloud: refresh the access
    /// token if it's near expiry, pull remote rows and merge them
    /// (furthest-page-wins), push the chapters we're ahead on, then persist the
    /// updated store, sync state, and (possibly refreshed) session.
    ///
    /// Requires being signed in; returns a transport error otherwise. Callers
    /// run this off the UI thread and treat any error as "stay local, retry
    /// later" — the local store is only written on a successful reconcile.
    pub fn sync(&self, now: u64) -> Result<SyncOutcome> {
        let mut session = self
            .session()
            .ok_or_else(|| crate::Error::Transport("not signed in".into()))?;

        if session.needs_refresh(now) {
            session = AuthClient::new(self.config.clone()).refresh(&session, now)?;
            self.save_session(&session)?;
        }

        let mut store = ProgressStore::load(&self.progress_path()).unwrap_or_default();
        let mut state = self.load_state();

        let transport = SupabaseTransport::new(self.config.clone(), session.access_token.clone());
        let outcome = Syncer::new(transport).sync(&mut store, &mut state)?;

        // Persist only after a clean reconcile, so a mid-sync failure never
        // corrupts the local store or advances the cursor past unmerged rows.
        store
            .save(&self.progress_path())
            .map_err(|e| crate::Error::Transport(e.to_string()))?;
        self.save_state(&state)?;
        Ok(outcome)
    }

    fn load_state(&self) -> SyncState {
        std::fs::read(self.state_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save_state(&self, state: &SyncState) -> Result<()> {
        self.write_json(&self.state_path(), state)
    }

    fn save_session(&self, session: &Session) -> Result<()> {
        self.write_json(&self.session_path(), session)
    }

    fn write_json<T: serde::Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::Error::Transport(e.to_string()))?;
        }
        let bytes =
            serde_json::to_vec_pretty(value).map_err(|e| crate::Error::Transport(e.to_string()))?;
        std::fs::write(path, bytes).map_err(|e| crate::Error::Transport(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SupabaseConfig {
        SupabaseConfig {
            url: "https://proj.supabase.co".into(),
            anon_key: "anon".into(),
        }
    }

    fn session(email: &str) -> Session {
        Session {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 9_999_999_999,
            email: email.into(),
        }
    }

    #[test]
    fn session_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let acct = Account::new(config(), dir.path());
        assert!(!acct.is_signed_in());
        acct.save_session(&session("reader@example.com")).unwrap();
        assert!(acct.is_signed_in());
        assert_eq!(acct.email().as_deref(), Some("reader@example.com"));
    }

    #[test]
    fn sign_out_forgets_session_and_state_but_keeps_progress() {
        let dir = tempfile::tempdir().unwrap();
        let acct = Account::new(config(), dir.path());
        acct.save_session(&session("a@example.com")).unwrap();
        acct.save_state(&SyncState {
            cursor: Some("t1".into()),
            ..Default::default()
        })
        .unwrap();
        // A local progress file that must survive sign-out.
        let mut store = ProgressStore::default();
        store.update("Series/vol1.cbz", 3, 10);
        store.save(&acct.progress_path()).unwrap();

        acct.sign_out().unwrap();

        assert!(!acct.is_signed_in(), "session forgotten");
        assert!(!acct.state_path().exists(), "sync cursor forgotten");
        let reloaded = ProgressStore::load(&acct.progress_path()).unwrap();
        assert!(
            reloaded.get("Series/vol1.cbz").is_some(),
            "local reading progress is kept across sign-out"
        );
    }

    #[test]
    fn switching_accounts_resets_the_sync_cursor() {
        // The cross-account guard, tested without network by pre-seeding state
        // and exercising the reset branch directly: a stale cursor from account
        // A must not survive into account B on the same profile.
        let dir = tempfile::tempdir().unwrap();
        let acct = Account::new(config(), dir.path());
        acct.save_session(&session("a@example.com")).unwrap();
        acct.save_state(&SyncState {
            cursor: Some("t-from-account-a".into()),
            ..Default::default()
        })
        .unwrap();

        // Simulate the verify() reset branch for a different identity.
        let previous = acct.email();
        assert_eq!(previous.as_deref(), Some("a@example.com"));
        let new = session("b@example.com");
        if previous.as_deref() != Some(new.email.as_str()) {
            std::fs::remove_file(acct.state_path()).unwrap();
        }
        acct.save_session(&new).unwrap();

        assert!(
            !acct.state_path().exists(),
            "a different account resets the pull cursor so its rows can't leak in"
        );
    }

    #[test]
    fn same_account_keeps_its_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let acct = Account::new(config(), dir.path());
        acct.save_session(&session("a@example.com")).unwrap();
        acct.save_state(&SyncState {
            cursor: Some("keep-me".into()),
            ..Default::default()
        })
        .unwrap();

        let previous = acct.email();
        let same = session("a@example.com");
        if previous.as_deref() != Some(same.email.as_str()) {
            std::fs::remove_file(acct.state_path()).unwrap();
        }

        assert!(
            acct.state_path().exists(),
            "re-signing into the same account preserves its cursor"
        );
    }
}
