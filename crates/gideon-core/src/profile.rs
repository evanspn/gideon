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
