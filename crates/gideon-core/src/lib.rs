//! gideon-core: document model, CBZ parsing and library management.
//!
//! This crate is the heart of gideon's v0: opening `.cbz` archives, listing
//! their pages in natural reading order, decoding page images and tracking
//! reading progress across a library directory.

pub mod books;
pub mod cbz;
pub mod comicinfo;
pub mod dict;
pub mod epub;
pub mod error;
pub mod library;
pub mod natsort;
pub mod profile;
pub mod series;
pub mod settings;
pub mod stats;
pub mod upload;
pub mod words;
pub mod zipread;

pub use books::{Book, BookPlace, BookPlaces, Bookmark, Highlight, HIGHLIGHT_COLORS};
pub use cbz::CbzDocument;
pub use comicinfo::ComicInfo;
pub use dict::{Definition, Dictionary};
pub use epub::{Block as BookBlock, EpubText, Rich, Span};
pub use error::Error;
pub use library::{Library, LibraryEntry, ProgressStore, ReadingProgress};
pub use profile::DEFAULT_PROFILE;
pub use series::{SeriesIndex, SeriesMeta, SeriesRef};
pub use settings::{
    BookCatalog, ProfileSettings, Settings, StorageSize, DEFAULT_FINISHED_CLEANUP_HOURS,
    FINISHED_CLEANUP_STEPS,
};
pub use stats::{ReadingSpan, ReadingStats};
pub use upload::{sanitize_filename, BookMeta, UploadKind, MAX_UPLOAD_BYTES};
pub use words::{SavedWord, SavedWords};

pub type Result<T> = std::result::Result<T, Error>;
