//! Aidoku WASM source runtime for gideon.
//!
//! This crate is ported from the bobo-koreader project's `backend/shared`
//! source runtime (<https://github.com/tachibana-shin/bobo-koreader>), which
//! itself derives from rakuyomi. Both are licensed under the AGPL-3.0, and so
//! is this crate.
//!
//! The port is intentionally close to the upstream code: only the pieces that
//! coupled the runtime to bobo's HTTP server, database and `SourceManager`
//! have been removed or replaced with minimal standalone equivalents (see
//! [`settings`]).

pub mod settings;
pub mod source;
pub(crate) mod util;

pub use settings::{Settings, SourceSettingValue};
pub use source::{
    model::{Chapter, DeepLink, Filter, Manga, MangaPageResult, Page, SettingDefinition},
    NextMangaPageResult, Source, SourceConfig, SourceFeatures, SourceInfo, SourceManifest,
    SourceMeta,
};
