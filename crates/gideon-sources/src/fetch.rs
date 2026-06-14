//! HTTP fetching abstraction.
//!
//! Network access goes through the [`Fetcher`] trait so everything above it
//! (list parsing, source resolution, chapter downloads) can be tested with
//! canned responses and no network.

use std::collections::HashMap;
use std::time::Duration;

use url::Url;

use crate::{Error, Result};

/// Transient network failures are retried this many times before giving up:
/// a dropped packet or a Wi-Fi hiccup mid-download then heals itself instead
/// of failing the whole action (or, for a chapter, the whole CBZ).
const RETRY_ATTEMPTS: u32 = 3;

/// Backoff before retry N (1-indexed): 0.5s, 1s. The last attempt doesn't
/// sleep afterwards.
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500 * (1 << (attempt.saturating_sub(1)).min(4)))
}

/// One fetch attempt's failure, tagged for the retry loop: a transient
/// transport error is worth retrying; an HTTP/status error is not.
enum Attempt {
    Transient(Error),
    Permanent(Error),
}

/// Run `attempt` up to [`RETRY_ATTEMPTS`] times, sleeping (via `sleep`)
/// between transient failures. Permanent failures return immediately. Split
/// out from the network so the retry policy is unit-testable.
fn with_retry<T>(
    sleep: impl Fn(Duration),
    mut attempt: impl FnMut(u32) -> std::result::Result<T, Attempt>,
) -> Result<T> {
    let mut last = Error::Offline;
    for n in 1..=RETRY_ATTEMPTS {
        match attempt(n) {
            Ok(value) => return Ok(value),
            Err(Attempt::Permanent(e)) => return Err(e),
            Err(Attempt::Transient(e)) => {
                last = e;
                if n < RETRY_ATTEMPTS {
                    sleep(backoff(n));
                }
            }
        }
    }
    Err(last)
}

/// Minimal HTTP GET abstraction.
pub trait Fetcher {
    fn get(&self, url: &Url) -> Result<Vec<u8>>;
}

/// Real fetcher backed by `ureq`.
pub struct UreqFetcher {
    agent: ureq::Agent,
}

impl Default for UreqFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl UreqFetcher {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .user_agent(concat!("gideon/", env!("CARGO_PKG_VERSION")))
            .tls_config(std::sync::Arc::new(tls_config()))
            .build();
        Self { agent }
    }
}

/// TLS roots: the embedded Mozilla store (always present and current at
/// build time — Kobo firmware ships an outdated CA bundle that can't
/// validate modern chains) merged with whatever system certs exist (so
/// corporate/proxy CAs keep working).
fn tls_config() -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Ok(certs) = rustls_native_certs::load_native_certs() {
        for cert in certs {
            let _ = roots.add(cert);
        }
    }
    rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth()
}

impl UreqFetcher {
    /// One GET attempt, classifying its failure for the retry loop. A
    /// transport error (no route, DNS, timeout, dropped connection mid-body)
    /// is transient → [`Error::Offline`] once retries are exhausted; an HTTP
    /// status error is permanent and keeps its detail.
    fn fetch_once(&self, url: &Url) -> std::result::Result<Vec<u8>, Attempt> {
        let mut request = self.agent.get(url.as_str());
        // Authenticate GitHub API requests when a token is provided —
        // required for OTA update checks against private repositories.
        if url.domain() == Some("api.github.com") {
            if let Ok(token) = std::env::var("GIDEON_GITHUB_TOKEN") {
                request = request.set("Authorization", &format!("Bearer {token}"));
            }
        }
        let response = match request.call() {
            Ok(response) => response,
            // Transport = the network itself failed (unreachable, DNS,
            // timeout): retry, then surface as "offline".
            Err(ureq::Error::Transport(_)) => return Err(Attempt::Transient(Error::Offline)),
            // A status code came back: the network works, the server said no.
            Err(e @ ureq::Error::Status(..)) => {
                return Err(Attempt::Permanent(Error::Fetch {
                    url: url.to_string(),
                    message: e.to_string(),
                }))
            }
        };

        let mut buf = Vec::new();
        match response.into_reader().read_to_end(&mut buf) {
            Ok(_) => Ok(buf),
            // The connection dropped mid-body: a retry usually completes.
            Err(_) => Err(Attempt::Transient(Error::Offline)),
        }
    }
}

impl Fetcher for UreqFetcher {
    fn get(&self, url: &Url) -> Result<Vec<u8>> {
        with_retry(std::thread::sleep, |_| self.fetch_once(url))
    }
}

/// In-memory fetcher for tests: maps exact URLs to canned bodies.
#[derive(Default)]
pub struct FakeFetcher {
    responses: HashMap<String, Vec<u8>>,
}

impl FakeFetcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, url: &str, body: impl Into<Vec<u8>>) -> Self {
        self.responses.insert(url.to_string(), body.into());
        self
    }
}

impl Fetcher for FakeFetcher {
    fn get(&self, url: &Url) -> Result<Vec<u8>> {
        self.responses
            .get(url.as_str())
            .cloned()
            .ok_or_else(|| Error::Fetch {
                url: url.to_string(),
                message: "no canned response".into(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A no-op sleeper so retry tests don't actually wait.
    fn no_sleep(_: Duration) {}

    #[test]
    fn retry_returns_on_first_success() {
        let calls = Cell::new(0);
        let out: Result<u8> = with_retry(no_sleep, |_| {
            calls.set(calls.get() + 1);
            Ok(7)
        });
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.get(), 1, "no retries once it succeeds");
    }

    #[test]
    fn retry_recovers_after_transient_failures() {
        // Fails transiently twice, then succeeds on the third attempt.
        let calls = Cell::new(0);
        let out: Result<u8> = with_retry(no_sleep, |n| {
            calls.set(calls.get() + 1);
            if n < 3 {
                Err(Attempt::Transient(Error::Offline))
            } else {
                Ok(42)
            }
        });
        assert_eq!(out.unwrap(), 42);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn retry_gives_up_after_the_attempt_limit_as_offline() {
        let calls = Cell::new(0);
        let out: Result<u8> = with_retry(no_sleep, |_| {
            calls.set(calls.get() + 1);
            Err(Attempt::Transient(Error::Offline))
        });
        assert!(matches!(out, Err(Error::Offline)));
        assert_eq!(
            calls.get(),
            RETRY_ATTEMPTS as i32,
            "exactly the attempt cap"
        );
    }

    #[test]
    fn permanent_failures_are_not_retried() {
        // An HTTP status error must fail immediately, keeping its detail.
        let calls = Cell::new(0);
        let out: Result<u8> = with_retry(no_sleep, |_| {
            calls.set(calls.get() + 1);
            Err(Attempt::Permanent(Error::Fetch {
                url: "u".into(),
                message: "404".into(),
            }))
        });
        assert!(matches!(out, Err(Error::Fetch { .. })));
        assert_eq!(calls.get(), 1, "permanent errors never retry");
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff(1), Duration::from_millis(500));
        assert_eq!(backoff(2), Duration::from_millis(1000));
        assert_eq!(backoff(3), Duration::from_millis(2000));
        // Saturates rather than overflowing for absurd attempt counts.
        assert_eq!(backoff(99), Duration::from_millis(500 * 16));
    }
}
