//! Integration test for the OTA update flow over real HTTP.
//!
//! A minimal HTTP server (std TcpListener, no test-only frameworks) plays
//! the role of GitHub's `releases/latest/download/` endpoint, and the real
//! `UreqFetcher` drives the full pipeline: version check → bundle download
//! → ELF validation → staging → atomic apply with rollback. This runs in
//! normal CI — no network access beyond localhost.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;

use gideon_sources::update::{
    apply_staged, check_update_via_assets, is_auto_installable, stage_update, STAGED_BINARY_NAME,
};
use gideon_sources::UreqFetcher;
use url::Url;

/// Serve canned responses on a local port. Each request gets matched by
/// path; unknown paths get a 404.
fn serve(routes: HashMap<String, Vec<u8>>) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
    let port = listener.local_addr().unwrap().port();
    let routes = Arc::new(routes);

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let routes = Arc::clone(&routes);
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();

                let response = match routes.get(&path) {
                    Some(body) => {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(body);
                        r
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = stream.write_all(&response);
            });
        }
    });

    Url::parse(&format!("http://127.0.0.1:{port}")).unwrap()
}

fn fake_bundle(binary: &[u8]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("gideon-kobo-v0.2.0/install.sh", opts)
        .unwrap();
    zip.write_all(b"#!/bin/sh\n").unwrap();
    zip.start_file("gideon-kobo-v0.2.0/gideon", opts).unwrap();
    zip.write_all(binary).unwrap();
    zip.finish().unwrap().into_inner()
}

#[test]
fn full_ota_flow_over_http() {
    let mut routes = HashMap::new();
    routes.insert(
        "/evanspn/gideon/releases/latest/download/VERSION".to_string(),
        b"gideon 0.2.0 (abc123def)".to_vec(),
    );
    routes.insert(
        "/evanspn/gideon/releases/latest/download/gideon-kobo-v0.2.0.zip".to_string(),
        fake_bundle(b"\x7fELFnew-release-binary"),
    );
    let base = serve(routes);
    let fetcher = UreqFetcher::new();

    // 1. Check: 0.1.0 -> 0.2.0 is available and auto-installable.
    let release = check_update_via_assets(&fetcher, &base, "evanspn/gideon", "0.1.0")
        .expect("check should succeed")
        .expect("update should be available");
    assert_eq!(release.version, "0.2.0");
    assert_eq!(release.tag, "v0.2.0");
    assert!(is_auto_installable("0.1.0", &release.version));

    // 2. Already up to date: same version reports no update.
    let none = check_update_via_assets(&fetcher, &base, "evanspn/gideon", "0.2.0").unwrap();
    assert!(none.is_none());

    // 3. Stage: bundle is downloaded over HTTP, the ELF binary extracted.
    let dir = tempfile::tempdir().unwrap();
    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(bin_dir.join("gideon"), b"\x7fELFold-binary").unwrap();

    let staged = stage_update(&fetcher, &release, &bin_dir).expect("staging should succeed");
    assert!(staged.ends_with(STAGED_BINARY_NAME));

    // 4. Apply: atomic swap with rollback copy.
    assert!(apply_staged(&bin_dir).unwrap());
    assert_eq!(
        std::fs::read(bin_dir.join("gideon")).unwrap(),
        b"\x7fELFnew-release-binary"
    );
    assert_eq!(
        std::fs::read(bin_dir.join("gideon.old")).unwrap(),
        b"\x7fELFold-binary"
    );

    // 5. On Unix the new binary must be executable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(bin_dir.join("gideon"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "updated binary is not executable");
    }
}

#[test]
fn missing_release_is_a_clear_error_not_a_crash() {
    let base = serve(HashMap::new());
    let fetcher = UreqFetcher::new();

    let err = check_update_via_assets(&fetcher, &base, "evanspn/gideon", "0.1.0").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("404") || msg.contains("status"),
        "unhelpful error: {msg}"
    );
}

#[test]
fn garbage_version_asset_never_updates() {
    let mut routes = HashMap::new();
    routes.insert(
        "/evanspn/gideon/releases/latest/download/VERSION".to_string(),
        b"<html>not a version</html>".to_vec(),
    );
    let base = serve(routes);
    let fetcher = UreqFetcher::new();

    let err = check_update_via_assets(&fetcher, &base, "evanspn/gideon", "0.1.0").unwrap_err();
    assert!(err.to_string().contains("semantic version"));
}

#[test]
fn major_bump_is_not_auto_installable() {
    let mut routes = HashMap::new();
    routes.insert(
        "/evanspn/gideon/releases/latest/download/VERSION".to_string(),
        b"2.0.0".to_vec(),
    );
    let base = serve(routes);
    let fetcher = UreqFetcher::new();

    let release = check_update_via_assets(&fetcher, &base, "evanspn/gideon", "1.5.0")
        .unwrap()
        .unwrap();
    assert_eq!(release.version, "2.0.0");
    assert!(!is_auto_installable("1.5.0", &release.version));
}
