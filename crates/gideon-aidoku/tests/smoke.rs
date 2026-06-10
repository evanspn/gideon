//! Offline smoke tests for the vendored Aidoku source runtime.
//!
//! These tests download nothing: they only exercise `.aix` loading and the
//! manifest/setting-definition plumbing.

use std::io::Write;

use gideon_aidoku::Source;
use zip::write::SimpleFileOptions;

/// A minimal, valid (empty) WASM module: just the magic number and version.
const EMPTY_WASM_MODULE: &[u8] = b"\0asm\x01\x00\x00\x00";

fn write_minimal_aix(path: &std::path::Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default();

    zip.start_file("Payload/source.json", options).unwrap();
    zip.write_all(
        br#"{
            "info": {
                "id": "test.smoke",
                "lang": "en",
                "name": "Smoke Test Source",
                "version": 1,
                "url": "https://example.invalid"
            }
        }"#,
    )
    .unwrap();

    zip.start_file("Payload/main.wasm", options).unwrap();
    zip.write_all(EMPTY_WASM_MODULE).unwrap();

    zip.finish().unwrap();
}

#[test]
fn from_aix_file_errors_cleanly_on_nonexistent_path() {
    let dir = tempfile::tempdir().unwrap();

    let result = Source::from_aix_file(
        &dir.path().join("does-not-exist.aix"),
        &dir.path().join("settings"),
    );

    let error = result.err().expect("expected an error");
    assert!(error.to_string().contains("couldn't open"), "{error:?}");
}

#[test]
fn from_aix_file_loads_a_minimal_source() {
    let dir = tempfile::tempdir().unwrap();
    let aix_path = dir.path().join("smoke.aix");
    write_minimal_aix(&aix_path);

    let source =
        Source::from_aix_file(&aix_path, dir.path()).expect("loading a minimal source works");

    let manifest = source.manifest();
    assert_eq!(manifest.info.id, "test.smoke");
    assert_eq!(manifest.info.name, "Smoke Test Source");
    assert_eq!(manifest.info.version, 1);

    // No settings.json in the archive, but the manifest URL is exposed as the
    // implicit `url` setting only when `urls` is present; here there are no
    // setting definitions at all.
    assert!(source.setting_definitions().is_empty());
}
