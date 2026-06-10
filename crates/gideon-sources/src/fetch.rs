//! HTTP fetching abstraction.
//!
//! Network access goes through the [`Fetcher`] trait so everything above it
//! (list parsing, source resolution, chapter downloads) can be tested with
//! canned responses and no network.

use std::collections::HashMap;

use url::Url;

use crate::{Error, Result};

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

impl Fetcher for UreqFetcher {
    fn get(&self, url: &Url) -> Result<Vec<u8>> {
        let mut request = self.agent.get(url.as_str());
        // Authenticate GitHub API requests when a token is provided —
        // required for OTA update checks against private repositories.
        if url.domain() == Some("api.github.com") {
            if let Ok(token) = std::env::var("GIDEON_GITHUB_TOKEN") {
                request = request.set("Authorization", &format!("Bearer {token}"));
            }
        }
        let response = request.call().map_err(|e| Error::Fetch {
            url: url.to_string(),
            message: e.to_string(),
        })?;

        let mut buf = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut buf)
            .map_err(|e| Error::Fetch {
                url: url.to_string(),
                message: e.to_string(),
            })?;
        Ok(buf)
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
