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
            .build();
        Self { agent }
    }
}

impl Fetcher for UreqFetcher {
    fn get(&self, url: &Url) -> Result<Vec<u8>> {
        let response = self
            .agent
            .get(url.as_str())
            .call()
            .map_err(|e| Error::Fetch {
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
