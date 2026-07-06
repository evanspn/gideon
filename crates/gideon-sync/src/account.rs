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
//! into a *different* account wipes this profile's local reading + bookkeeping
//! (see [`Account::verify`]), so a previous user's rows can neither be pushed up
//! into the new account nor linger in its library.
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
/// Durable record of which account `progress.json` belongs to. Unlike the
/// session, it survives sign-out — so signing a profile into a *different*
/// account is detected even across the usual sign-out-then-sign-in flow.
const OWNER_FILE: &str = "sync_owner";

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
    fn owner_path(&self) -> PathBuf {
        self.dir.join(OWNER_FILE)
    }

    /// The account `progress.json` currently belongs to (durable across
    /// sign-out), or `None` if this profile has never been signed in.
    fn owner(&self) -> Option<String> {
        std::fs::read_to_string(self.owner_path())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
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

    /// Step 2 of sign-in: verify the code and persist the session. On a
    /// *different* account than was signed in before, wipe this profile's local
    /// reading data and sync bookkeeping too — a profile is bound to one
    /// account, so account A's chapters must not (a) get pushed up into account
    /// B, nor (b) linger in B's library. First-time sign-in (no previous
    /// account) keeps the local `progress.json` and pushes it up — it's this
    /// device's own reading.
    pub fn verify(&self, email: &str, code: &str, now: u64) -> Result<Session> {
        // Compare against the *durable owner*, not the live session: the usual
        // flow is sign-out (session gone, progress kept) then sign-in, so a
        // session-based check would miss the switch and re-push the prior user's
        // rows into the new account.
        let previous_owner = self.owner();
        let session = AuthClient::new(self.config.clone()).verify_code(email, code, now)?;

        let switching_accounts = previous_owner
            .as_deref()
            .is_some_and(|prev| prev != session.email);
        if switching_accounts {
            // Different user on this profile: drop the prior account's local
            // reading and cursor entirely, so nothing of theirs leaks into the
            // new account (in either direction).
            let _ = std::fs::remove_file(self.progress_path());
            let _ = std::fs::remove_file(self.state_path());
        }
        self.write_owner(&session.email)?;
        self.save_session(&session)?;
        Ok(session)
    }

    /// Record `email` as the durable owner of this profile's `progress.json`.
    fn write_owner(&self, email: &str) -> Result<()> {
        if let Some(parent) = self.owner_path().parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::Error::Transport(e.to_string()))?;
        }
        std::fs::write(self.owner_path(), email).map_err(|e| crate::Error::Transport(e.to_string()))
    }

    /// Sign out: forget the session and the sync cursor (so a later sign-in
    /// starts clean), but keep local `progress.json` — reading works offline —
    /// and keep the durable owner marker, so signing this profile into a
    /// *different* account afterwards is still detected as a switch and wipes the
    /// prior user's rows (see [`Self::verify`]).
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
        // merge_save (not save): this runs on a background thread that races the
        // reader's own writes — folding in (furthest-page-wins) rather than
        // blind-overwriting means a page the reader just advanced isn't lost.
        store
            .merge_save(&self.progress_path())
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

    /// Mirror the local wipe-decision half of `verify` (the only network step is
    /// the code exchange; the reset is pure filesystem), keyed off the durable
    /// owner exactly as `verify` is.
    fn simulate_verify(acct: &Account, new_email: &str) {
        let switching = acct.owner().is_some_and(|prev| prev != new_email);
        if switching {
            let _ = std::fs::remove_file(acct.progress_path());
            let _ = std::fs::remove_file(acct.state_path());
        }
        acct.write_owner(new_email).unwrap();
        acct.save_session(&session(new_email)).unwrap();
    }

    #[test]
    fn sign_out_then_different_account_wipes_the_prior_users_data() {
        // The real UI path: sign in A, sign OUT (which keeps progress), then sign
        // in B. B must not inherit A's reading list — this is the leak the
        // session-based check missed, so it's keyed off the durable owner.
        let dir = tempfile::tempdir().unwrap();
        let acct = Account::new(config(), dir.path());

        simulate_verify(&acct, "a@example.com");
        let mut store = ProgressStore::default();
        store.update("A Secret Manga/vol1.cbz", 40, 40);
        store.save(&acct.progress_path()).unwrap();
        acct.save_state(&SyncState {
            cursor: Some("t-a".into()),
            ..Default::default()
        })
        .unwrap();

        acct.sign_out().unwrap(); // session gone, progress + owner kept
        assert_eq!(acct.owner().as_deref(), Some("a@example.com"));

        simulate_verify(&acct, "b@example.com"); // roommate signs in

        assert!(
            !acct.progress_path().exists(),
            "A's reading list is wiped, so it can't be pushed into B's account"
        );
        assert!(!acct.state_path().exists(), "A's cursor is gone");
        assert_eq!(acct.owner().as_deref(), Some("b@example.com"));
    }

    #[test]
    fn first_sign_in_keeps_local_reading_to_push_up() {
        // No previous owner: the device's own offline reading must be kept
        // (and later pushed up), not wiped.
        let dir = tempfile::tempdir().unwrap();
        let acct = Account::new(config(), dir.path());
        let mut store = ProgressStore::default();
        store.update("My Manga/vol1.cbz", 12, 30);
        store.save(&acct.progress_path()).unwrap();

        simulate_verify(&acct, "me@example.com");

        let reloaded = ProgressStore::load(&acct.progress_path()).unwrap();
        assert!(
            reloaded.get("My Manga/vol1.cbz").is_some(),
            "first sign-in keeps the device's own reading to sync up"
        );
    }

    #[test]
    fn signing_back_into_the_same_account_keeps_cursor_and_reading() {
        let dir = tempfile::tempdir().unwrap();
        let acct = Account::new(config(), dir.path());
        simulate_verify(&acct, "a@example.com");
        acct.save_state(&SyncState {
            cursor: Some("keep-me".into()),
            ..Default::default()
        })
        .unwrap();
        let mut store = ProgressStore::default();
        store.update("Mine/vol1.cbz", 4, 10);
        store.save(&acct.progress_path()).unwrap();

        acct.sign_out().unwrap();
        // No sync_state after sign-out, but progress + owner persist; re-signing
        // into the SAME account must not wipe the reading.
        simulate_verify(&acct, "a@example.com");

        let reloaded = ProgressStore::load(&acct.progress_path()).unwrap();
        assert!(
            reloaded.get("Mine/vol1.cbz").is_some(),
            "re-signing into the same account keeps the reading"
        );
    }
}
