use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to open archive {path}: {source}")]
    OpenArchive {
        path: PathBuf,
        source: zip::result::ZipError,
    },

    #[error("archive contains no readable pages")]
    EmptyArchive,

    #[error("page index {index} out of bounds (document has {count} pages)")]
    PageOutOfBounds { index: usize, count: usize },

    #[error("failed to read page {name}: {source}")]
    ReadPage {
        name: String,
        source: zip::result::ZipError,
    },

    #[error("failed to decode image {name}: {source}")]
    DecodeImage {
        name: String,
        source: image::ImageError,
    },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse progress store: {0}")]
    ProgressStore(#[from] serde_json::Error),
}
