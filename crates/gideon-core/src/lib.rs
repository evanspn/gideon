//! gideon-core: document model, CBZ parsing and library management.
//!
//! This crate is the heart of gideon's v0: opening `.cbz` archives, listing
//! their pages in natural reading order, decoding page images and tracking
//! reading progress across a library directory.

pub mod cbz;
pub mod comicinfo;
pub mod error;
pub mod library;
pub mod natsort;
pub mod series;
pub mod settings;

pub use cbz::CbzDocument;
pub use comicinfo::ComicInfo;
pub use error::Error;
pub use library::{Library, LibraryEntry, ProgressStore, ReadingProgress};
pub use series::{SeriesIndex, SeriesRef};
pub use settings::{Settings, StorageSize};

pub type Result<T> = std::result::Result<T, Error>;
