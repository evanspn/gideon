//! Profile library layout, and converting the legacy "default" profile into
//! an ordinary one.
//!
//! Every profile owns one library directory: its books plus the `.gideon`
//! directory that holds its reading progress, series index and sync
//! bookkeeping. Named profiles live in `<root>/@<name>`; the historical
//! "default" profile is the odd one out — it *is* the library root, so its
//! books sit next to the other profiles' directories instead of inside one of
//! their own.
//!
//! [`convert_default`] fixes that for a library that already has content under
//! the root: it names the default profile and moves the root's contents into
//! `@<name>`, leaving the root as a pure container. After the move the library
//! has no special profile at all — every profile is a directory.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// The name of the legacy profile whose library is the root itself.
pub const DEFAULT_PROFILE: &str = "default";

/// The library directory of a profile: the root itself for
/// [`DEFAULT_PROFILE`], `<root>/@<name>` otherwise. The `@` prefix keeps
/// profile directories from colliding with series directories, and a root
/// scan skips them.
pub fn library_dir(base: &Path, profile: &str) -> PathBuf {
    if profile == DEFAULT_PROFILE {
        base.to_path_buf()
    } else {
        base.join(format!("@{profile}"))
    }
}

/// Whether `name` is usable as a profile name: non-empty once trimmed, not
/// the reserved "default", and free of anything that would escape or shadow
/// the `@<name>` directory it becomes.
pub fn is_valid_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name != DEFAULT_PROFILE
        && !name.starts_with('@')
        && !name.starts_with('.')
        && !name.contains(['/', '\\'])
}

/// Every profile that actually has a library under `root`, by name: one per
/// `@<name>` directory, sorted.
///
/// The library on disk — not `settings.json` — is the truth about which
/// profiles exist. A settings file that goes missing, gets truncated by a yanked
/// USB cable, or parses leniently into nothing would otherwise take a profile
/// out of the picker even though every one of its books is still sitting right
/// there. Union this into the listed profiles and such a profile comes back by
/// itself.
///
/// An unreadable root yields nothing: this only ever *adds* profiles to a list,
/// so failing to read it can't take one away.
pub fn discover(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix('@')
                .filter(|rest| !rest.is_empty())
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names
}

/// Convert the default profile — the library root — into an ordinary profile
/// named `name`, by moving everything the root holds into `<root>/@<name>`.
///
/// Other profiles' `@` directories stay where they are; everything else moves,
/// including the hidden `.gideon` directory, so the converted profile keeps its
/// reading progress, series index and sync account exactly as they were.
///
/// The move is renames within one directory, so it's cheap whatever the library
/// weighs. If one of them fails partway the already-moved entries are put back,
/// so the library is left as it was rather than half-converted.
///
/// Returns the new profile's library directory.
pub fn convert_default(root: &Path, name: &str) -> Result<PathBuf> {
    let name = name.trim();
    if !is_valid_name(name) {
        return Err(Error::ConvertProfile(format!(
            "\"{name}\" isn't a usable profile name"
        )));
    }
    let target = library_dir(root, name);
    if target.exists() {
        return Err(Error::ConvertProfile(format!(
            "profile \"{name}\" already has a library at {}",
            target.display()
        )));
    }

    // Collect first: moving entries while iterating the same directory is
    // unspecified, and a stale listing would be worse than a slightly stale one.
    let mut to_move = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_name = entry.file_name();
        // Other profiles' libraries stay at the root — they already are
        // ordinary profiles.
        if file_name.to_string_lossy().starts_with('@') {
            continue;
        }
        to_move.push(file_name);
    }

    fs::create_dir_all(&target)?;
    let mut moved: Vec<std::ffi::OsString> = Vec::new();
    for file_name in &to_move {
        if let Err(e) = fs::rename(root.join(file_name), target.join(file_name)) {
            // Put back whatever already moved, then drop the (now empty)
            // target, so a failure leaves the library exactly as it was.
            for done in moved.iter().rev() {
                let _ = fs::rename(target.join(done), root.join(done));
            }
            let _ = fs::remove_dir(&target);
            return Err(e.into());
        }
        moved.push(file_name.clone());
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn library_dir_is_the_root_only_for_default() {
        let root = Path::new("/data/Manga");
        assert_eq!(library_dir(root, "default"), root);
        assert_eq!(library_dir(root, "alex"), root.join("@alex"));
    }

    #[test]
    fn names_that_would_escape_or_shadow_are_rejected() {
        assert!(is_valid_name("alex"));
        assert!(is_valid_name(" Bo "));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("   "));
        assert!(!is_valid_name("default"));
        assert!(!is_valid_name("@alex"));
        assert!(!is_valid_name(".gideon"));
        assert!(!is_valid_name("a/b"));
        assert!(!is_valid_name("a\\b"));
    }

    #[test]
    fn conversion_moves_books_and_bookkeeping_but_not_other_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("Series/ch1.cbz"), "book");
        write(&root.join("loose.cbz"), "book");
        write(&root.join(".gideon/progress.json"), "{}");
        write(&root.join("@alex/Other/ch1.cbz"), "alex's book");

        let target = convert_default(root, " me ").unwrap();
        assert_eq!(target, root.join("@me"));

        assert!(target.join("Series/ch1.cbz").is_file());
        assert!(target.join("loose.cbz").is_file());
        assert_eq!(
            fs::read_to_string(target.join(".gideon/progress.json")).unwrap(),
            "{}"
        );
        // The root keeps only profile directories now.
        assert!(!root.join("Series").exists());
        assert!(!root.join(".gideon").exists());
        assert!(root.join("@alex/Other/ch1.cbz").is_file());
        let mut left: Vec<String> = fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, vec!["@alex".to_string(), "@me".to_string()]);
    }

    #[test]
    fn discovery_finds_every_profile_that_has_a_library() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("@alex/Series/ch1.cbz"), "book");
        fs::create_dir_all(root.join("@bo")).unwrap();
        // Not profiles: a series directory, a loose book, the bookkeeping
        // directory, and a stray file that merely starts with @.
        write(&root.join("Series/ch1.cbz"), "book");
        write(&root.join("loose.cbz"), "book");
        write(&root.join(".gideon/progress.json"), "{}");
        write(&root.join("@notadir"), "file");

        assert_eq!(discover(root), vec!["alex".to_string(), "bo".to_string()]);
    }

    #[test]
    fn discovery_of_an_unreadable_root_takes_nothing_away() {
        assert!(discover(Path::new("/definitely/not/a/library")).is_empty());
    }

    /// Every file under `dir`, as (path relative to `dir`, contents), sorted.
    fn snapshot(dir: &Path) -> Vec<(String, String)> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, String)>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, base, out);
                } else {
                    let rel = path
                        .strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    out.push((rel, fs::read_to_string(&path).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    #[test]
    fn conversion_loses_no_file_and_no_byte() {
        // The property that matters most: a conversion relocates the library
        // whole. Every file that was under the root is under @me afterwards, at
        // the same relative path, with identical contents — nothing dropped,
        // nothing truncated, nothing merged into anything else.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let files = [
            ("Series A/ch1.cbz", "first chapter bytes"),
            ("Series A/ch2.cbz", "second chapter bytes"),
            ("Series B/Volume 1/ch1.cbz", "nested deeper"),
            ("loose.cbz", "a book at the top level"),
            (
                ".gideon/progress.json",
                r#"{"progress":{"Series A/ch1.cbz":1}}"#,
            ),
            (".gideon/series.json", r#"{"series":{}}"#),
            (".gideon/sync_session.json", r#"{"refresh_token":"secret"}"#),
            (
                "cover art.png",
                "not a cbz, but the user's file all the same",
            ),
        ];
        for (path, contents) in files {
            write(&root.join(path), contents);
        }
        let before = snapshot(root);
        assert_eq!(before.len(), files.len());

        let target = convert_default(root, "me").unwrap();

        // Same files, same relative paths, same bytes — just one level deeper.
        assert_eq!(snapshot(&target), before);
        // And the root now holds nothing but the profile directory: no orphan
        // left behind, no copy left at the old location.
        let left: Vec<String> = fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["@me".to_string()]);
    }

    /// Whether a read-only directory actually blocks writes for this user.
    /// It doesn't for root, which ignores the permission bits — so a test that
    /// fault-injects that way would quietly assert nothing when run as root
    /// (which is exactly how CI and the device run).
    #[cfg(unix)]
    fn read_only_dirs_are_enforced() -> bool {
        use std::os::unix::fs::PermissionsExt;
        let probe = tempfile::tempdir().unwrap();
        let mut perms = fs::metadata(probe.path()).unwrap().permissions();
        perms.set_mode(0o500);
        fs::set_permissions(probe.path(), perms).unwrap();
        let blocked = fs::create_dir(probe.path().join("probe")).is_err();
        let mut perms = fs::metadata(probe.path()).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(probe.path(), perms).unwrap();
        blocked
    }

    #[test]
    #[cfg(unix)]
    fn a_failed_conversion_leaves_every_file_in_place() {
        // A conversion that can't complete must leave the library exactly as it
        // was, never half-moved. A read-only root makes it fail at its first
        // write — the earliest point it can go wrong.
        if !read_only_dirs_are_enforced() {
            eprintln!("skipped: running as root, where read-only directories don't block writes");
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("Series/ch1.cbz"), "book");
        write(&root.join("loose.cbz"), "book");
        let before = snapshot(root);

        let mut perms = fs::metadata(root).unwrap().permissions();
        perms.set_mode(0o500); // r-x: no creating or renaming entries here
        fs::set_permissions(root, perms).unwrap();

        let result = convert_default(root, "me");

        // Restore write access before asserting, so the temp dir can clean up.
        let mut perms = fs::metadata(root).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(root, perms).unwrap();

        assert!(
            result.is_err(),
            "the conversion must fail, not half-succeed"
        );
        assert_eq!(snapshot(root), before, "every file must still be there");
    }

    #[test]
    fn conversion_refuses_bad_and_taken_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("Series/ch1.cbz"), "book");
        fs::create_dir_all(root.join("@alex")).unwrap();

        assert!(convert_default(root, "default").is_err());
        assert!(convert_default(root, "  ").is_err());
        assert!(convert_default(root, "alex").is_err(), "@alex is taken");
        // Nothing moved.
        assert!(root.join("Series/ch1.cbz").is_file());
    }

    #[test]
    fn converting_an_empty_root_just_makes_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target = convert_default(dir.path(), "me").unwrap();
        assert!(target.is_dir());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    }
}
