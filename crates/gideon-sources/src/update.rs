//! Over-the-air updates from GitHub releases.
//!
//! gideon checks this repository's latest GitHub release, compares versions
//! semantically, downloads the `gideon-kobo-vX.Y.Z.zip` bundle and stages
//! the new binary next to the running one. The swap is a rename — applied
//! immediately when possible, or picked up on the next launch — so an
//! interrupted update can never leave a broken install (a hard-won lesson
//! from bobo's OTA history: "stream download into update ZIP", "do not
//! update on major bumps").

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use url::Url;

use crate::fetch::Fetcher;
use crate::{Error, Result};

/// Repository releases are checked against. Override with the
/// `GIDEON_UPDATE_REPO` environment variable (owner/name).
pub const DEFAULT_UPDATE_REPO: &str = "evanspn/gideon";

/// Name of the staged binary, next to the running one.
pub const STAGED_BINARY_NAME: &str = "gideon.new";

#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseInfo {
    /// Version without the leading `v`.
    pub version: String,
    pub tag: String,
    /// Download URL of the Kobo bundle asset.
    pub asset_url: Url,
    pub notes: Option<String>,
}

/// GitHub API response subset for /releases/latest.
#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<ApiAsset>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

/// URL of the GitHub API endpoint for the latest release of `repo`.
pub fn latest_release_url(repo: &str) -> Result<Url> {
    Ok(Url::parse(&format!(
        "https://api.github.com/repos/{repo}/releases/latest"
    ))?)
}

/// Base URL for release asset downloads. Defaults to github.com; tests and
/// mirrors can point elsewhere.
pub fn release_base() -> Url {
    std::env::var("GIDEON_UPDATE_BASE")
        .ok()
        .and_then(|raw| Url::parse(&raw).ok())
        .unwrap_or_else(|| Url::parse("https://github.com").expect("static url"))
}

/// URL of the `VERSION` asset on the latest release. GitHub serves
/// `releases/latest/download/<asset>` without the API — no rate limits and
/// no auth for public repos, which makes it the most reliable check from
/// devices.
pub fn version_asset_url(base: &Url, repo: &str) -> Result<Url> {
    Ok(base.join(&format!("{repo}/releases/latest/download/VERSION"))?)
}

/// URL of the Kobo bundle asset for `version` on the latest release.
pub fn bundle_asset_url(base: &Url, repo: &str, version: &str) -> Result<Url> {
    Ok(base.join(&format!(
        "{repo}/releases/latest/download/gideon-kobo-v{version}.zip"
    ))?)
}

/// Extract a semver from a VERSION asset. Lenient: finds the first token
/// that parses as `X.Y.Z` (so both "0.2.0" and "gideon 0.2.0 (abc123)"
/// work).
pub fn parse_version_asset(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    text.split_whitespace()
        .map(|token| token.trim_start_matches('v'))
        .find(|token| parse_semver(token).is_some())
        .map(str::to_owned)
}

/// Check for updates via the latest release's `VERSION` asset (primary
/// mechanism: no API, no rate limits, anonymous on public repos).
pub fn check_update_via_assets(
    fetcher: &dyn Fetcher,
    base: &Url,
    repo: &str,
    current_version: &str,
) -> Result<Option<ReleaseInfo>> {
    let body = fetcher.get(&version_asset_url(base, repo)?)?;
    let Some(version) = parse_version_asset(&body) else {
        return Err(Error::Fetch {
            url: version_asset_url(base, repo)?.to_string(),
            message: "VERSION asset did not contain a semantic version".into(),
        });
    };
    if !is_newer(current_version, &version) {
        return Ok(None);
    }
    Ok(Some(ReleaseInfo {
        tag: format!("v{version}"),
        asset_url: bundle_asset_url(base, repo, &version)?,
        version,
        notes: None,
    }))
}

/// bobo's auto-install rule: only updates within the same major version are
/// installed automatically; major bumps need explicit consent.
pub fn is_auto_installable(current: &str, candidate: &str) -> bool {
    match (parse_semver(current), parse_semver(candidate)) {
        (Some((cur_major, ..)), Some((cand_major, ..))) => cur_major == cand_major,
        _ => false,
    }
}

/// Parse a GitHub /releases/latest response into a [`ReleaseInfo`], looking
/// for the Kobo bundle asset. Returns `Ok(None)` for drafts/prereleases or
/// releases without a bundle.
pub fn parse_latest_release(body: &[u8]) -> Result<Option<ReleaseInfo>> {
    let release: ApiRelease = serde_json::from_slice(body).map_err(|e| Error::ParseList {
        url: "releases/latest".into(),
        message: e.to_string(),
    })?;

    if release.draft || release.prerelease {
        return Ok(None);
    }

    let version = release.tag_name.trim_start_matches('v').to_string();
    let Some(asset) = release
        .assets
        .iter()
        .find(|a| a.name.starts_with("gideon-kobo-") && a.name.ends_with(".zip"))
    else {
        return Ok(None);
    };

    Ok(Some(ReleaseInfo {
        version,
        tag: release.tag_name.clone(),
        asset_url: Url::parse(&asset.browser_download_url)?,
        notes: release.body.clone().filter(|b| !b.trim().is_empty()),
    }))
}

/// Semantic comparison: is `candidate` newer than `current`?
/// Pre-release suffixes are ignored for ordering (1.2.3-rc.1 == 1.2.3).
pub fn is_newer(current: &str, candidate: &str) -> bool {
    match (parse_semver(current), parse_semver(candidate)) {
        (Some(cur), Some(cand)) => cand > cur,
        // If either side is unparseable, never auto-update.
        _ => false,
    }
}

fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Check whether an update is available for `current_version`.
pub fn check_update(
    fetcher: &dyn Fetcher,
    repo: &str,
    current_version: &str,
) -> Result<Option<ReleaseInfo>> {
    let body = fetcher.get(&latest_release_url(repo)?)?;
    let Some(release) = parse_latest_release(&body)? else {
        return Ok(None);
    };
    if is_newer(current_version, &release.version) {
        Ok(Some(release))
    } else {
        Ok(None)
    }
}

/// Download the release bundle and stage the new binary as
/// `<bin_dir>/gideon.new`. Validates that the extracted file is an ELF
/// executable before staging — a truncated or wrong download never reaches
/// the binary directory.
pub fn stage_update(
    fetcher: &dyn Fetcher,
    release: &ReleaseInfo,
    bin_dir: &Path,
) -> Result<PathBuf> {
    let bundle = fetcher.get(&release.asset_url)?;
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bundle)).map_err(crate::Error::Zip)?;

    // The bundle contains the binary as `gideon` (possibly under a
    // versioned directory).
    let entry_name = archive
        .file_names()
        .find(|n| n == &"gideon" || n.ends_with("/gideon"))
        .map(str::to_owned)
        .ok_or_else(|| Error::Fetch {
            url: release.asset_url.to_string(),
            message: "bundle does not contain a 'gideon' binary".into(),
        })?;

    let mut binary = Vec::new();
    archive
        .by_name(&entry_name)
        .map_err(crate::Error::Zip)?
        .read_to_end(&mut binary)?;

    if binary.len() < 4 || &binary[..4] != b"\x7fELF" {
        return Err(Error::Fetch {
            url: release.asset_url.to_string(),
            message: "downloaded binary is not a valid ELF executable".into(),
        });
    }

    std::fs::create_dir_all(bin_dir)?;
    let staged = bin_dir.join(STAGED_BINARY_NAME);
    let tmp = bin_dir.join(".gideon.new.part");
    std::fs::write(&tmp, &binary)?;
    make_executable(&tmp)?;
    std::fs::rename(&tmp, &staged)?;
    Ok(staged)
}

/// If a staged update exists in `bin_dir`, swap it into place. The old
/// binary is kept as `gideon.old` as a manual rollback. Returns `true` when
/// an update was applied. Call this at startup and after staging.
pub fn apply_staged(bin_dir: &Path) -> Result<bool> {
    let staged = bin_dir.join(STAGED_BINARY_NAME);
    if !staged.exists() {
        return Ok(false);
    }
    let current = bin_dir.join("gideon");
    let old = bin_dir.join("gideon.old");

    if current.exists() {
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&current, &old)?;
    }
    std::fs::rename(&staged, &current)?;
    Ok(true)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::FakeFetcher;
    use std::io::Write;

    const API_URL: &str = "https://api.github.com/repos/evanspn/gideon/releases/latest";

    fn release_json(tag: &str, asset_name: &str) -> String {
        format!(
            r#"{{
                "tag_name": "{tag}",
                "body": "notes here",
                "draft": false,
                "prerelease": false,
                "assets": [
                    {{"name": "{asset_name}",
                      "browser_download_url": "https://github.com/evanspn/gideon/releases/download/{tag}/{asset_name}"}}
                ]
            }}"#
        )
    }

    fn fake_bundle(binary: &[u8]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("gideon-kobo-v9.9.9/INSTALL.md", opts)
            .unwrap();
        zip.write_all(b"docs").unwrap();
        zip.start_file("gideon-kobo-v9.9.9/gideon", opts).unwrap();
        zip.write_all(binary).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn semver_comparison() {
        assert!(is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.1.9", "0.2.0"));
        assert!(is_newer("0.9.9", "1.0.0"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("v0.1.0", "v0.1.1"));
        // Pre-release/build suffixes don't break parsing.
        assert!(is_newer("0.1.0", "0.1.1-rc.1"));
        // Garbage never updates.
        assert!(!is_newer("0.1.0", "latest"));
        assert!(!is_newer("???", "0.2.0"));
    }

    #[test]
    fn parses_release_with_kobo_bundle() {
        let info =
            parse_latest_release(release_json("v0.2.0", "gideon-kobo-v0.2.0.zip").as_bytes())
                .unwrap()
                .unwrap();
        assert_eq!(info.version, "0.2.0");
        assert_eq!(info.tag, "v0.2.0");
        assert!(info.asset_url.as_str().ends_with("gideon-kobo-v0.2.0.zip"));
        assert_eq!(info.notes.as_deref(), Some("notes here"));
    }

    #[test]
    fn skips_releases_without_bundle_and_prereleases() {
        let no_bundle = release_json("v0.2.0", "something-else.zip");
        assert_eq!(parse_latest_release(no_bundle.as_bytes()).unwrap(), None);

        let pre = release_json("v0.3.0", "gideon-kobo-v0.3.0.zip")
            .replace(r#""prerelease": false"#, r#""prerelease": true"#);
        assert_eq!(parse_latest_release(pre.as_bytes()).unwrap(), None);
    }

    #[test]
    fn check_update_only_reports_newer_versions() {
        let fetcher = FakeFetcher::new().with(
            API_URL,
            release_json("v0.2.0", "gideon-kobo-v0.2.0.zip").into_bytes(),
        );

        let update = check_update(&fetcher, "evanspn/gideon", "0.1.0").unwrap();
        assert_eq!(update.unwrap().version, "0.2.0");

        let no_update = check_update(&fetcher, "evanspn/gideon", "0.2.0").unwrap();
        assert!(no_update.is_none());

        let downgrade = check_update(&fetcher, "evanspn/gideon", "0.3.0").unwrap();
        assert!(downgrade.is_none());
    }

    #[test]
    fn stage_and_apply_update() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("gideon"), b"\x7fELFold-binary").unwrap();

        let asset_url =
            "https://github.com/evanspn/gideon/releases/download/v9.9.9/gideon-kobo-v9.9.9.zip";
        let fetcher = FakeFetcher::new().with(asset_url, fake_bundle(b"\x7fELFnew-binary"));
        let release = ReleaseInfo {
            version: "9.9.9".into(),
            tag: "v9.9.9".into(),
            asset_url: Url::parse(asset_url).unwrap(),
            notes: None,
        };

        let staged = stage_update(&fetcher, &release, &bin_dir).unwrap();
        assert!(staged.ends_with(STAGED_BINARY_NAME));
        assert!(staged.exists());
        // Old binary untouched until apply.
        assert_eq!(
            std::fs::read(bin_dir.join("gideon")).unwrap(),
            b"\x7fELFold-binary"
        );

        assert!(apply_staged(&bin_dir).unwrap());
        assert_eq!(
            std::fs::read(bin_dir.join("gideon")).unwrap(),
            b"\x7fELFnew-binary"
        );
        // Rollback copy kept.
        assert_eq!(
            std::fs::read(bin_dir.join("gideon.old")).unwrap(),
            b"\x7fELFold-binary"
        );
        // Idempotent: nothing staged anymore.
        assert!(!apply_staged(&bin_dir).unwrap());
    }

    #[test]
    fn stage_rejects_non_elf_payload() {
        let dir = tempfile::tempdir().unwrap();
        let asset_url =
            "https://github.com/evanspn/gideon/releases/download/v9.9.9/gideon-kobo-v9.9.9.zip";
        let fetcher = FakeFetcher::new().with(asset_url, fake_bundle(b"#!/bin/sh\necho gotcha"));
        let release = ReleaseInfo {
            version: "9.9.9".into(),
            tag: "v9.9.9".into(),
            asset_url: Url::parse(asset_url).unwrap(),
            notes: None,
        };

        let err = stage_update(&fetcher, &release, dir.path()).unwrap_err();
        assert!(err.to_string().contains("ELF"), "unexpected error: {err}");
        assert!(!dir.path().join(STAGED_BINARY_NAME).exists());
    }

    #[test]
    fn stage_rejects_bundle_without_binary() {
        let dir = tempfile::tempdir().unwrap();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("README.md", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"empty bundle").unwrap();
        let bundle = zip.finish().unwrap().into_inner();

        let asset_url =
            "https://github.com/evanspn/gideon/releases/download/v9.9.9/gideon-kobo-v9.9.9.zip";
        let fetcher = FakeFetcher::new().with(asset_url, bundle);
        let release = ReleaseInfo {
            version: "9.9.9".into(),
            tag: "v9.9.9".into(),
            asset_url: Url::parse(asset_url).unwrap(),
            notes: None,
        };
        assert!(stage_update(&fetcher, &release, dir.path()).is_err());
    }
}
