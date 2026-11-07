//! Supabase client for reading-progress sync.
//!
//! This is the real network layer behind the pure reconcile logic in
//! [`crate`]: it authenticates against Supabase Auth (GoTrue) with **email +
//! password** — the account is created once on the web, and the device just
//! signs in (no email round-trip, so it works on the free tier) — and
//! reads/writes the `reading_progress` table through PostgREST and the
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

use std::io::Read;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    BookPlaceTransport, BookPlaceUpdate, Error, ProgressTransport, ProgressUpdate, RemoteBookPlace,
    RemoteProgress, Result,
};

/// Clock skew allowance: refresh an access token this many seconds before it
/// actually expires, so a request never goes out with a just-expired token.
const EXPIRY_SKEW_SECS: u64 = 60;

/// Network timeouts for every sync request. `ureq`'s defaults let a request sit
/// on a half-open connection indefinitely, which on a Kobo means a sync thread
/// wedged for the rest of the session — and, since sync is single-flight, every
/// later trigger silently dropped. A stalled request must fail so the retry can
/// happen.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// An agent with those timeouts, shared by both clients.
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout(CALL_TIMEOUT)
        .build()
}

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

    /// An object in the private uploads bucket. Each path segment is
    /// percent-encoded on its own: a file name from a phone has spaces and
    /// worse, and the slashes between segments must stay slashes.
    fn upload_url(&self, object_path: &str) -> String {
        let encoded: Vec<String> = object_path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(encode_query_value)
            .collect();
        format!(
            "{}/storage/v1/object/{}/{}",
            self.base(),
            UPLOADS_BUCKET,
            encoded.join("/")
        )
    }
}

/// The storage bucket the dashboard uploads files into (migration 0005).
pub const UPLOADS_BUCKET: &str = "kobo-uploads";

/// Downloads larger than this are refused before they are read: the bucket
/// caps objects at 50 MB, so anything bigger is not something we uploaded.
const MAX_UPLOAD_DOWNLOAD: u64 = 52 * 1024 * 1024;

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

/// One "send to Kobo" request the web enqueued. Two kinds: a *title* for the
/// device to search for (only a search term crosses the wire — the web can't
/// run the source search), or a *file* the phone uploaded, which the device
/// fetches from storage and places. Plus the row id so it can be marked
/// opened. The file fields default so a cache written before files existed
/// still reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub kind: SendKind,
    /// Storage object path (`<user>/<random>/<file name>`) for a file send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
}

impl SendItem {
    /// A title send, as the web has always made them.
    pub fn title(id: &str, title: &str) -> Self {
        Self {
            id: id.to_string(),
            title: title.to_string(),
            kind: SendKind::Title,
            file_path: None,
            file_size: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SendKind {
    #[default]
    Title,
    File,
}

/// An OPDS catalogue typed on the phone (`book_catalogs`, migration 0005).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCatalog {
    pub name: String,
    pub url: String,
}

/// PostgREST query for this user's pending sends, newest first.
fn send_pull_query() -> &'static str {
    "send_queue?status=eq.pending&select=id,title,kind,file_path,file_size&order=created_at.desc"
}

/// PostgREST query for this user's book catalogues, oldest first (the order
/// they were added, which is the order they show on the device).
fn catalogs_query() -> &'static str {
    "book_catalogs?select=name,url&order=created_at.asc"
}

fn parse_catalog_rows(body: &str) -> Result<Vec<RemoteCatalog>> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(body).map_err(transport_err)?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let url = r["url"].as_str()?.trim();
            if url.is_empty() {
                return None;
            }
            Some(RemoteCatalog {
                name: r["name"].as_str().unwrap_or("").trim().to_string(),
                url: url.to_string(),
            })
        })
        .collect())
}

/// PostgREST filter selecting one send row by id (for the "opened" PATCH).
fn mark_opened_query(id: &str) -> String {
    format!("send_queue?id=eq.{}", encode_query_value(id))
}

fn parse_send_rows(body: &str) -> Result<Vec<SendItem>> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(body).map_err(transport_err)?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let kind = match r["kind"].as_str() {
                Some("file") => SendKind::File,
                // Absent (a project without migration 0005) or 'title'.
                _ => SendKind::Title,
            };
            let file_path = r["file_path"].as_str().map(str::to_string);
            // A file send with no object path can't be fetched; drop it
            // rather than show a row that can never do anything.
            if kind == SendKind::File && file_path.is_none() {
                return None;
            }
            Some(SendItem {
                id: r["id"].as_str()?.to_string(),
                title: r["title"].as_str()?.to_string(),
                kind,
                file_path,
                file_size: r["file_size"].as_u64(),
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

/// The PostgREST query for a book-places pull, paged like `pull_query`.
fn pull_places_query(cursor: Option<&str>) -> String {
    let select = "book_places?select=book_key,chapter,block,line,chapters,title,author,updated_at&order=updated_at.asc";
    match cursor {
        Some(c) => format!("{select}&updated_at=gt.{}", encode_query_value(c)),
        None => select.to_string(),
    }
}

fn parse_place_rows(body: &str) -> Result<Vec<RemoteBookPlace>> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(body).map_err(transport_err)?;
    let text = |v: &serde_json::Value| {
        v.as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            Some(RemoteBookPlace {
                book_key: r["book_key"].as_str()?.to_string(),
                chapter: r["chapter"].as_u64().unwrap_or(0) as usize,
                block: r["block"].as_u64().unwrap_or(0) as usize,
                line: r["line"].as_u64().unwrap_or(0) as usize,
                chapters: r["chapters"].as_u64().unwrap_or(0) as usize,
                title: text(&r["title"]),
                author: text(&r["author"]),
                updated_at: r["updated_at"].as_str()?.to_string(),
            })
        })
        .collect())
}

/// The JSON body for one `upsert_book_place` RPC call.
fn upsert_place_body(update: &BookPlaceUpdate) -> serde_json::Value {
    json!({
        "p_book_key": update.book_key,
        "p_chapter": update.chapter,
        "p_block": update.block,
        "p_line": update.line,
        "p_chapters": update.chapters,
        "p_title": update.title,
        "p_author": update.author,
    })
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

/// The JSON body for one `set_chapter_pages` RPC call — publishes the page
/// image URLs the device resolved for a chapter so the web reader can load
/// them live. Only URLs cross the wire, never the images themselves.
fn set_chapter_pages_body(chapter_key: &str, page_urls: &[String]) -> serde_json::Value {
    json!({
        "p_chapter_key": chapter_key,
        "p_page_urls": page_urls,
    })
}

/// Authenticates against Supabase Auth (GoTrue) with email + password.
///
/// The account is created once (on the web, from a phone); the device only ever
/// *signs in* with the same email and password — no email round-trip, so it
/// works on Supabase's free tier with no custom SMTP.
pub struct AuthClient {
    config: SupabaseConfig,
    agent: ureq::Agent,
}

impl AuthClient {
    pub fn new(config: SupabaseConfig) -> Self {
        Self {
            config,
            agent: agent(),
        }
    }

    /// Sign in with an existing email + password and return a fresh [`Session`].
    /// `now` is unix seconds (injected so expiry math is testable and
    /// clock-independent).
    pub fn sign_in(&self, email: &str, password: &str, now: u64) -> Result<Session> {
        let resp = self
            .agent
            .post(&self.config.auth_url("token?grant_type=password"))
            .set("apikey", &self.config.anon_key)
            .set("Content-Type", "application/json")
            .send_json(json!({ "email": email, "password": password }));
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
            agent: agent(),
        }
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.access_token)
    }

    /// Publish a chapter's page image URLs through the `set_chapter_pages` RPC
    /// (upsert keyed by the JWT's user + `chapter_key`). Best-effort: callers
    /// treat any error as "try again next sweep".
    pub fn set_chapter_pages(&self, chapter_key: &str, page_urls: &[String]) -> Result<()> {
        let resp = self
            .agent
            .post(&self.config.rest_url("rpc/set_chapter_pages"))
            .set("apikey", &self.config.anon_key)
            .set("Authorization", &self.auth_header())
            .set("Content-Type", "application/json")
            .send_json(set_chapter_pages_body(chapter_key, page_urls));
        check_ok(resp)?;
        Ok(())
    }

    /// This user's pending "send to Kobo" titles, newest first. Best-effort:
    /// a transport error just means "no sends to show this sweep".
    pub fn fetch_pending_sends(&self) -> Result<Vec<SendItem>> {
        let resp = self
            .agent
            .get(&self.config.rest_url(send_pull_query()))
            .set("apikey", &self.config.anon_key)
            .set("Authorization", &self.auth_header())
            .call();
        let body = read_body(check_ok(resp)?)?;
        parse_send_rows(&body)
    }

    /// Fetch an uploaded file's bytes from the private bucket. The owner's
    /// token is what authorises it; the bucket serves nothing by URL.
    pub fn download_upload(&self, object_path: &str) -> Result<Vec<u8>> {
        let resp = self
            .agent
            .get(&self.config.upload_url(object_path))
            .set("apikey", &self.config.anon_key)
            .set("Authorization", &self.auth_header())
            .call();
        let resp = check_ok(resp)?;
        if let Some(len) = resp
            .header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
        {
            if len > MAX_UPLOAD_DOWNLOAD {
                return Err(Error::Transport(format!(
                    "upload is {len} bytes, over the {MAX_UPLOAD_DOWNLOAD} byte limit"
                )));
            }
        }
        let mut bytes = Vec::new();
        let mut reader = resp.into_reader();
        std::io::Read::take(&mut *reader, MAX_UPLOAD_DOWNLOAD + 1)
            .read_to_end(&mut bytes)
            .map_err(transport_err)?;
        if bytes.len() as u64 > MAX_UPLOAD_DOWNLOAD {
            return Err(Error::Transport("upload is over the size limit".into()));
        }
        Ok(bytes)
    }

    /// Remove an uploaded file once it is safely on the device, so it stops
    /// counting against the account's storage.
    pub fn delete_upload(&self, object_path: &str) -> Result<()> {
        let resp = self
            .agent
            .delete(&self.config.upload_url(object_path))
            .set("apikey", &self.config.anon_key)
            .set("Authorization", &self.auth_header())
            .call();
        check_ok(resp)?;
        Ok(())
    }

    /// The OPDS catalogues this account added on the phone, oldest first.
    pub fn fetch_book_catalogs(&self) -> Result<Vec<RemoteCatalog>> {
        let resp = self
            .agent
            .get(&self.config.rest_url(catalogs_query()))
            .set("apikey", &self.config.anon_key)
            .set("Authorization", &self.auth_header())
            .call();
        let body = read_body(check_ok(resp)?)?;
        parse_catalog_rows(&body)
    }

    /// Mark a file send `failed` with the reason, so the phone shows why it
    /// never arrived instead of the row just disappearing.
    pub fn mark_send_failed(&self, id: &str, error: &str) -> Result<()> {
        let resp = self
            .agent
            .request("PATCH", &self.config.rest_url(&mark_opened_query(id)))
            .set("apikey", &self.config.anon_key)
            .set("Authorization", &self.auth_header())
            .set("Content-Type", "application/json")
            .send_json(json!({ "status": "failed", "error": error }));
        check_ok(resp)?;
        Ok(())
    }

    /// Mark a send row `opened` so the device's notification badge clears and
    /// the title isn't offered again on the next sweep.
    pub fn mark_send_opened(&self, id: &str) -> Result<()> {
        let resp = self
            .agent
            .request("PATCH", &self.config.rest_url(&mark_opened_query(id)))
            .set("apikey", &self.config.anon_key)
            .set("Authorization", &self.auth_header())
            .set("Content-Type", "application/json")
            .send_json(json!({ "status": "opened" }));
        check_ok(resp)?;
        Ok(())
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

impl BookPlaceTransport for SupabaseTransport {
    fn pull_places(&self, cursor: Option<&str>) -> Result<Vec<RemoteBookPlace>> {
        let resp = self
            .agent
            .get(&self.config.rest_url(&pull_places_query(cursor)))
            .set("apikey", &self.config.anon_key)
            .set("Authorization", &self.auth_header())
            .call();
        let body = read_body(check_ok(resp)?)?;
        parse_place_rows(&body)
    }

    fn push_places(&self, updates: &[BookPlaceUpdate]) -> Result<()> {
        for update in updates {
            let resp = self
                .agent
                .post(&self.config.rest_url("rpc/upsert_book_place"))
                .set("apikey", &self.config.anon_key)
                .set("Authorization", &self.auth_header())
                .set("Content-Type", "application/json")
                .send_json(upsert_place_body(update));
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
    fn book_place_rows_and_bodies_round_trip() {
        assert_eq!(
            pull_places_query(Some("2026-03-09T10:00:00+00:00")),
            "book_places?select=book_key,chapter,block,line,chapters,title,author,updated_at&order=updated_at.asc&updated_at=gt.2026-03-09T10%3A00%3A00%2B00%3A00"
        );
        let rows = parse_place_rows(
            r#"[{"book_key":"kv/sh5.epub","chapter":3,"block":10,"line":1,"chapters":20,"title":"Slaughterhouse-Five","author":"  ","updated_at":"t1"},
                {"chapter":1,"updated_at":"t2"}]"#,
        )
        .unwrap();
        assert_eq!(rows.len(), 1, "a row without a key is dropped");
        assert_eq!(rows[0].book_key, "kv/sh5.epub");
        assert_eq!(
            (
                rows[0].chapter,
                rows[0].block,
                rows[0].line,
                rows[0].chapters
            ),
            (3, 10, 1, 20)
        );
        assert_eq!(rows[0].title.as_deref(), Some("Slaughterhouse-Five"));
        assert_eq!(rows[0].author, None, "blank author reads as none");
        let body = upsert_place_body(&BookPlaceUpdate {
            book_key: "kv/sh5.epub".into(),
            chapter: 3,
            block: 10,
            line: 1,
            chapters: 20,
            title: Some("Slaughterhouse-Five".into()),
            author: None,
        });
        assert_eq!(body["p_book_key"], "kv/sh5.epub");
        assert_eq!(body["p_line"], 1);
        assert_eq!(body["p_title"], "Slaughterhouse-Five");
        assert!(body["p_author"].is_null());
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
    fn send_pull_query_selects_pending_newest_first() {
        assert_eq!(
            send_pull_query(),
            "send_queue?status=eq.pending&select=id,title,kind,file_path,file_size&order=created_at.desc"
        );
    }

    #[test]
    fn parse_send_rows_reads_id_and_title() {
        let body = r#"[{"id":"abc","title":"Berserk"},{"id":"def","title":"Naruto"}]"#;
        assert_eq!(
            parse_send_rows(body).unwrap(),
            vec![
                SendItem::title("abc", "Berserk"),
                SendItem::title("def", "Naruto")
            ]
        );
        // A row missing a field is skipped, not a panic.
        assert!(parse_send_rows(r#"[{"id":"x"}]"#).unwrap().is_empty());
    }

    #[test]
    fn parse_send_rows_reads_file_sends_and_drops_pathless_ones() {
        let body = r#"[
            {"id":"f1","title":"Slaughterhouse-Five.epub","kind":"file","file_path":"u/r/Slaughterhouse-Five.epub","file_size":512000},
            {"id":"t1","title":"Berserk","kind":"title","file_path":null,"file_size":null},
            {"id":"bad","title":"ghost.epub","kind":"file","file_path":null}
        ]"#;
        let rows = parse_send_rows(body).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, SendKind::File);
        assert_eq!(
            rows[0].file_path.as_deref(),
            Some("u/r/Slaughterhouse-Five.epub")
        );
        assert_eq!(rows[0].file_size, Some(512000));
        assert_eq!(rows[1], SendItem::title("t1", "Berserk"));
    }

    #[test]
    fn a_sends_cache_from_before_file_sends_still_reads() {
        let old: Vec<SendItem> = serde_json::from_str(r#"[{"id":"a","title":"Berserk"}]"#).unwrap();
        assert_eq!(old, vec![SendItem::title("a", "Berserk")]);
    }

    #[test]
    fn upload_url_encodes_each_segment_and_keeps_the_slashes() {
        let cfg = SupabaseConfig {
            url: "https://x.supabase.co/".into(),
            anon_key: "k".into(),
        };
        assert_eq!(
            cfg.upload_url("uid-1/r2/My Book (final).epub"),
            "https://x.supabase.co/storage/v1/object/kobo-uploads/uid-1/r2/My%20Book%20%28final%29.epub"
        );
    }

    #[test]
    fn catalog_rows_parse_and_skip_the_urlless() {
        let body = r#"[{"name":"Home","url":"http://calibre.local:8080/opds"},{"name":"x","url":""},{"name":"","url":"https://b.test/opds"}]"#;
        let rows = parse_catalog_rows(body).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "Home");
        assert_eq!(rows[1].url, "https://b.test/opds");
        assert_eq!(
            catalogs_query(),
            "book_catalogs?select=name,url&order=created_at.asc"
        );
    }

    #[test]
    fn mark_opened_query_targets_the_row_by_id() {
        assert_eq!(mark_opened_query("a-b_c"), "send_queue?id=eq.a-b_c");
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
    fn set_chapter_pages_body_uses_the_rpc_parameter_names() {
        let b = set_chapter_pages_body(
            "Vagabond/ch1.cbz",
            &[
                "https://cdn/1.png".to_string(),
                "https://cdn/2.png".to_string(),
            ],
        );
        assert_eq!(b["p_chapter_key"], "Vagabond/ch1.cbz");
        assert_eq!(b["p_page_urls"][0], "https://cdn/1.png");
        assert_eq!(b["p_page_urls"][1], "https://cdn/2.png");
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
