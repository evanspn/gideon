//! HTTP, behind a trait so the client is testable with canned bodies.

use std::collections::HashMap;
use std::io::Read;

use url::Url;

use crate::{Error, Result};

/// Minimal HTTP GET.
pub trait Fetcher {
    fn get(&self, url: &Url) -> Result<Vec<u8>>;
}

/// The real thing: `ureq` with the bundled Mozilla roots plus whatever the
/// host trusts. The Kobo's own CA bundle is years stale, which is why the
/// bundled roots come first.
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
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Ok(certs) = rustls_native_certs::load_native_certs() {
            for cert in certs {
                let _ = roots.add(cert);
            }
        }
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let agent = ureq::AgentBuilder::new()
            .user_agent(concat!("archivist/", env!("CARGO_PKG_VERSION")))
            .tls_config(std::sync::Arc::new(tls))
            .build();
        Self { agent }
    }
}

impl Fetcher for UreqFetcher {
    fn get(&self, url: &Url) -> Result<Vec<u8>> {
        let response = self.agent.get(url.as_str()).call().map_err(|e| match e {
            ureq::Error::Transport(_) => Error::Offline,
            e @ ureq::Error::Status(..) => Error::Http {
                url: url.to_string(),
                message: e.to_string(),
            },
        })?;
        let mut buf = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut buf)
            .map_err(|_| Error::Offline)?;
        Ok(buf)
    }
}

/// Canned responses by exact URL, for tests.
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
            .ok_or_else(|| Error::Http {
                url: url.to_string(),
                message: "no canned response".into(),
            })
    }
}
