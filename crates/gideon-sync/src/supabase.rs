//! Supabase client for reading-progress sync.
//!
//! This is the real network layer behind the pure reconcile logic in
//! [`crate`]: it authenticates against Supabase Auth (GoTrue) with an **email
//! one-time code** — device-friendly, no browser redirect to catch on a Kobo —
//! and reads/writes the `reading_progress` table through PostgREST and the
//! `upsert_progress` RPC (see `supabase/migrations/0001_reading_progress.sql`).
//!
//! Access is entirely scoped server-side: the device only ever holds the
//! project's publishable (anon) key plus the signed-in user's JWT, and
//! row-level security ties every row to `auth.uid()`, so one account can never
//! see or write another's progress even though the client never sends a
//! `user_id`.
//!
//! Request building and response parsing are split into pure functions so the
//! protocol is unit-tested without a live project; the `ureq` calls are thin
//! wrappers over them. Live round-trips live behind `#[ignore]` integration
//! tests that only run when a project is configured.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Error, ProgressTransport, ProgressUpdate, RemoteProgress, Result};

/// Clock skew allowance: refresh an access token this many seconds before it
/// actually expires, so a request never goes out with a just-expired token.
const EXPIRY_SKEW_SECS: u64 = 60;

fn transport_err(e: impl std::fmt::Display) -> Error {
    Error::Transport(e.to_string())
}

/// Connection details for a Supabase project. The `anon_key` is the project's
/// publishable key — safe to ship on-device, since RLS (not the key) is what
/// gates access to a user's rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupabaseConfig {
    /// Project base URL, e.g. `https://abcdefgh.supabase.co`.
    pub url: String,
    /// Publishable (anon) API key.
    pub anon_key: String,
}

impl SupabaseConfig {
    fn base(&self) -> &str {
        self.url.trim_end_matches('/')
    }

    fn rest_url(&self, path_and_query: &str) -> String {
        format!("{}/rest/v1/{}", self.base(), path_and_query)
    }

    fn auth_url(&self, path: &str) -> String {
        format!("{}/auth/v1/{}", self.base(), path)
    }
}

/// A signed-in session: the tokens and the identity they belong to. Persisted
/// on-device (serde) so sign-in survives restarts; the access token is
/// short-lived and refreshed from the long-lived refresh token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds at which `access_token` stops being valid.
    pub expires_at: u64,
    /// The email the user signed in with (shown in the account UI).
    pub email: String,
}

impl Session {
    /// Whether the access token is expired (or close enough that it should be
    /// refreshed before use) at `now` (unix seconds).
    pub fn needs_refresh(&self, now: u64) -> bool {
        now.saturating_add(EXPIRY_SKEW_SECS) >= self.expires_at
    }
}

/// Parse a GoTrue token response (`/verify`, `/token`) into a [`Session`].
/// `now` stamps the absolute expiry from the response's relative `expires_in`.
/// `fallback_email` is used when the response omits the user object (a refresh
/// response often does), so the stored identity is preserved across refreshes.
fn parse_session(body: &str, now: u64, fallback_email: &str) -> Result<Session> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(transport_err)?;
    let access_token = v["access_token"]
        .as_str()
        .ok_or_else(|| Error::Transport("auth response missing access_token".into()))?
        .to_string();
    let refresh_token = v["refresh_token"]
        .as_str()
        .ok_or_else(|| Error::Transport("auth response missing refresh_token".into()))?
        .to_string();
    let expires_in = v["expires_in"].as_u64().unwrap_or(3600);
    let email = v["user"]["email"]
        .as_str()
        .filter(|e| !e.is_empty())
        .unwrap_or(fallback_email)
        .to_string();
    Ok(Session {
        access_token,
        refresh_token,
        expires_at: now.saturating_add(expires_in),
        email,
    })
}

/// Parse a PostgREST `reading_progress` select into remote rows. Rows with a
/// negative or missing page/total are clamped to 0 (the column checks forbid
/// negatives server-side, but a client stays defensive).
fn parse_progress_rows(body: &str) -> Result<Vec<RemoteProgress>> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(body).map_err(transport_err)?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            Some(RemoteProgress {
                chapter_key: r["chapter_key"].as_str()?.to_string(),
                current_page: r["current_page"].as_u64().unwrap_or(0) as usize,
                total_pages: r["total_pages"].as_u64().unwrap_or(0) as usize,
                updated_at: r["updated_at"].as_str()?.to_string(),
            })
        })
        .collect())
}

/// The PostgREST query for a pull: newest-first is *not* wanted — we page by
/// ascending `updated_at` so the cursor advances monotonically. When `cursor`
/// is set, only rows strictly after it are fetched.
fn pull_query(cursor: Option<&str>) -> String {
    let select = "reading_progress?select=chapter_key,current_page,total_pages,updated_at&order=updated_at.asc";
    match cursor {
        Some(c) => format!("{select}&updated_at=gt.{}", encode_query_value(c)),
        None => select.to_string(),
    }
}

/// Minimal percent-encoding for a query value (timestamps contain `:` and `+`).
fn encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The JSON body for one `upsert_progress` RPC call.
fn upsert_body(update: &ProgressUpdate) -> serde_json::Value {
    json!({
        "p_chapter_key": update.chapter_key,
        "p_current_page": update.current_page,
        "p_total_pages": update.total_pages,
    })
}

/// Authenticates against Supabase Auth (GoTrue) with email one-time codes.
pub struct AuthClient {
    config: SupabaseConfig,
    agent: ureq::Agent,
}

impl AuthClient {
    pub fn new(config: SupabaseConfig) -> Self {
        Self {
            config,
            agent: ureq::Agent::new(),
        }
    }

    /// Send a one-time login code to `email`. Creates the account on first use,
    /// so there's no separate sign-up step.
    pub fn request_code(&self, email: &str) -> Result<()> {
        let resp = self
            .agent
            .post(&self.config.auth_url("otp"))
            .set("apikey", &self.config.anon_key)
            .set("Content-Type", "application/json")
            .send_json(json!({ "email": email, "create_user": true }));
        check_ok(resp).map(|_| ())
    }

    /// Verify the emailed code and return a fresh [`Session`]. `now` is unix
    /// seconds (injected so expiry math is testable and clock-independent).
    pub fn verify_code(&self, email: &str, code: &str, now: u64) -> Result<Session> {
        let resp = self
            .agent
            .post(&self.config.auth_url("verify"))
            .set("apikey", &self.config.anon_key)
            .set("Content-Type", "application/json")
            .send_json(json!({ "email": email, "token": code, "type": "email" }));
        let body = read_body(check_ok(resp)?)?;
        parse_session(&body, now, email)
    }

    /// Exchange the refresh token for a new access token, preserving identity.
    pub fn refresh(&self, session: &Session, now: u64) -> Result<Session> {
        let resp = self
            .agent
            .post(&self.config.auth_url("token?grant_type=refresh_token"))
            .set("apikey", &self.config.anon_key)
            .set("Content-Type", "application/json")
            .send_json(json!({ "refresh_token": session.refresh_token }));
        let body = read_body(check_ok(resp)?)?;
        parse_session(&body, now, &session.email)
    }
}

/// The real [`ProgressTransport`]: PostgREST for pull, the `upsert_progress`
/// RPC for push, authorized by the session's access token. Build a fresh one
/// per sync with a non-expired token (refresh via [`AuthClient`] first).
pub struct SupabaseTransport {
    config: SupabaseConfig,
    access_token: String,
    agent: ureq::Agent,
}

impl SupabaseTransport {
    pub fn new(config: SupabaseConfig, access_token: String) -> Self {
        Self {
            config,
            access_token,
            agent: ureq::Agent::new(),
        }
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.access_token)
    }
}

impl ProgressTransport for SupabaseTransport {
    fn pull(&self, cursor: Option<&str>) -> Result<Vec<RemoteProgress>> {
        let resp = self
            .agent
            .get(&self.config.rest_url(&pull_query(cursor)))
            .set("apikey", &self.config.anon_key)
            .set("Authorization", &self.auth_header())
            .call();
        let body = read_body(check_ok(resp)?)?;
        parse_progress_rows(&body)
    }

    fn push(&self, updates: &[ProgressUpdate]) -> Result<()> {
        // The RPC upserts one (user, chapter) row per call; loop the batch.
        // Furthest-page-wins is enforced inside the RPC, so ordering within the
        // batch doesn't matter.
        for update in updates {
            let resp = self
                .agent
                .post(&self.config.rest_url("rpc/upsert_progress"))
                .set("apikey", &self.config.anon_key)
                .set("Authorization", &self.auth_header())
                .set("Content-Type", "application/json")
                .send_json(upsert_body(update));
            check_ok(resp)?;
        }
        Ok(())
    }
}

/// Turn a `ureq` result into an error on any non-2xx status, attaching the
/// server's message so auth/permission failures are legible.
fn check_ok(resp: std::result::Result<ureq::Response, ureq::Error>) -> Result<ureq::Response> {
    match resp {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            Err(Error::Transport(format!("HTTP {code}: {detail}")))
        }
        Err(e) => Err(transport_err(e)),
    }
}

fn read_body(resp: ureq::Response) -> Result<String> {
    resp.into_string().map_err(transport_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SupabaseConfig {
        SupabaseConfig {
            url: "https://proj.supabase.co/".into(), // trailing slash on purpose
            anon_key: "anon123".into(),
        }
    }

    #[test]
    fn urls_are_built_without_double_slashes() {
        let c = config();
        assert_eq!(
            c.rest_url("rpc/upsert_progress"),
            "https://proj.supabase.co/rest/v1/rpc/upsert_progress"
        );
        assert_eq!(
            c.auth_url("verify"),
            "https://proj.supabase.co/auth/v1/verify"
        );
    }

    #[test]
    fn pull_query_encodes_the_cursor_timestamp() {
        assert_eq!(
            pull_query(None),
            "reading_progress?select=chapter_key,current_page,total_pages,updated_at&order=updated_at.asc"
        );
        let q = pull_query(Some("2026-03-09T10:11:12+00:00"));
        assert!(
            q.ends_with("&updated_at=gt.2026-03-09T10%3A11%3A12%2B00%3A00"),
            "got: {q}"
        );
    }

    #[test]
    fn session_parses_and_stamps_absolute_expiry() {
        let body = r#"{
            "access_token":"at","refresh_token":"rt","expires_in":3600,
            "user":{"email":"reader@example.com"}
        }"#;
        let s = parse_session(body, 1_000, "fallback@example.com").unwrap();
        assert_eq!(s.access_token, "at");
        assert_eq!(s.refresh_token, "rt");
        assert_eq!(s.expires_at, 4_600);
        assert_eq!(s.email, "reader@example.com");
    }

    #[test]
    fn refresh_response_without_user_keeps_the_known_email() {
        // GoTrue refresh responses may omit the user object; identity persists.
        let body = r#"{"access_token":"at2","refresh_token":"rt2","expires_in":3600}"#;
        let s = parse_session(body, 0, "reader@example.com").unwrap();
        assert_eq!(s.email, "reader@example.com");
    }

    #[test]
    fn missing_tokens_are_a_transport_error_not_a_panic() {
        assert!(parse_session(r#"{"expires_in":60}"#, 0, "x@y.z").is_err());
    }

    #[test]
    fn needs_refresh_respects_the_skew_window() {
        let s = Session {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 1_000,
            email: "e".into(),
        };
        assert!(!s.needs_refresh(900), "well before expiry: no refresh");
        assert!(s.needs_refresh(950), "within the 60s skew: refresh");
        assert!(s.needs_refresh(1_000), "at expiry: refresh");
    }

    #[test]
    fn progress_rows_parse_and_clamp() {
        let body = r#"[
            {"chapter_key":"One Piece/vol1.cbz","current_page":5,"total_pages":20,"updated_at":"t1"},
            {"chapter_key":"Naruto/vol2.cbz","current_page":0,"total_pages":0,"updated_at":"t2"}
        ]"#;
        let rows = parse_progress_rows(body).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].chapter_key, "One Piece/vol1.cbz");
        assert_eq!(rows[0].current_page, 5);
        assert_eq!(rows[1].current_page, 0);
    }

    #[test]
    fn upsert_body_uses_the_rpc_parameter_names() {
        let b = upsert_body(&ProgressUpdate {
            chapter_key: "One Piece/vol1.cbz".into(),
            current_page: 7,
            total_pages: 20,
        });
        assert_eq!(b["p_chapter_key"], "One Piece/vol1.cbz");
        assert_eq!(b["p_current_page"], 7);
        assert_eq!(b["p_total_pages"], 20);
    }
}
