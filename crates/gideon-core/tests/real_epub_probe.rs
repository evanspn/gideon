//! Ad-hoc: validate and read the EPUB named in GIDEON_PROBE_EPUB, the way
//! the device does with a phone send. Skipped when unset — the file is not
//! ours to ship as a fixture.
#[test]
fn probe_named_epub() {
    let Ok(path) = std::env::var("GIDEON_PROBE_EPUB") else {
        return;
    };
    let bytes = std::fs::read(&path).unwrap();
    let kind = gideon_core::upload::classify("book.epub", &bytes).unwrap();
    assert_eq!(kind, gideon_core::UploadKind::Epub);
    let meta = gideon_core::upload::epub_meta(&bytes);
    let text = gideon_core::epub::load(std::path::Path::new(&path)).unwrap();
    eprintln!(
        "classified: {kind:?}; meta: {meta:?}; chapters: {}; toc: {}",
        text.chapters.len(),
        text.toc.len()
    );
    assert!(meta.title.is_some());
    assert!(!text.chapters.is_empty());
}
