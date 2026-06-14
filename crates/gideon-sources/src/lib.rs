//! gideon-sources: manga source management, compatible with the source-list
//! format used by [Aidoku](https://github.com/Aidoku/Aidoku) and by
//! [bobo](https://github.com/evanspn/bobo-koreader), gideon's KOReader-plugin
//! sibling.
//!
//! A *source list* is a JSON document — typically hosted on GitHub (raw file
//! or GitHub Pages) — describing installable manga sources. gideon ships
//! with a default list preinstalled and lets users add their own.

pub mod download;
pub mod fetch;
pub mod list;
pub mod storage;
pub mod update;

pub use download::pages_to_cbz;
pub use fetch::{Fetcher, UreqFetcher};
pub use list::{
    parse_source_list, resolve_icon_url, resolve_package_url, SourceInformation, SourceLists,
    DEFAULT_SOURCE_LISTS,
};
pub use storage::ChapterStorage;
pub use update::{apply_staged, check_update, stage_update, ReleaseInfo, DEFAULT_UPDATE_REPO};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A transport failure that survived every retry — almost always "Wi-Fi
    /// is off / out of range". Carries a plain, actionable message so the UI
    /// can show it verbatim instead of a raw library error.
    #[error("no network connection — check that Wi-Fi is on, then try again")]
    Offline,

    #[error("failed to fetch {url}: {message}")]
    Fetch { url: String, message: String },

    #[error("failed to parse source list at {url}: {message}")]
    ParseList { url: String, message: String },

    #[error("source '{0}' not found in any configured source list")]
    SourceNotFound(String),

    #[error("invalid url: {0}")]
    InvalidUrl(#[from] url::ParseError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
}

pub type Result<T> = std::result::Result<T, Error>;
