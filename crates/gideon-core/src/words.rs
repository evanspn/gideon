//! Saved words: every word the reader looked up in the dictionary, kept with
//! the sentence it came from and the book, newest first — a vocabulary list
//! built without having to ask for it.
//!
//! One entry per word (compared without case): looking a word up again moves
//! it to the top with the new sentence, rather than listing it twice.
//! Lives in a dotfile beside the books, like their reading places.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// One looked-up word.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SavedWord {
    /// The headword, as the dictionary spells it.
    pub word: String,
    /// The definition's first line, for the list.
    #[serde(default)]
    pub gloss: String,
    /// The sentence the word was met in.
    #[serde(default)]
    pub context: String,
    /// The book, by its path under the Books folder, and its title.
    #[serde(default)]
    pub book: String,
    #[serde(default)]
    pub title: String,
    /// Where in the book: chapter and paragraph.
    #[serde(default)]
    pub chapter: usize,
    #[serde(default)]
    pub block: usize,
    #[serde(default)]
    pub saved_at: u64,
}

/// Every saved word, newest first.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedWords {
    #[serde(default)]
    pub words: Vec<SavedWord>,
}

impl SavedWords {
    pub const FILE: &'static str = ".gideon-words.json";

    pub fn load(books_dir: &Path) -> Self {
        std::fs::read(books_dir.join(Self::FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, books_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(books_dir)?;
        let tmp = books_dir.join(".gideon-words.json.part");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, books_dir.join(Self::FILE))
    }

    /// Keep `word`, newest first; an earlier entry for the same word (any
    /// case) is replaced by this one.
    pub fn record(&mut self, word: SavedWord) {
        self.words
            .retain(|w| !w.word.eq_ignore_ascii_case(&word.word));
        self.words.insert(0, word);
    }

    /// Forget the word at `index`.
    pub fn remove(&mut self, index: usize) -> Option<SavedWord> {
        (index < self.words.len()).then(|| self.words.remove(index))
    }

    /// The words met in one book, newest first.
    pub fn in_book<'a>(&'a self, book: &'a str) -> impl Iterator<Item = &'a SavedWord> + 'a {
        self.words.iter().filter(move |w| w.book == book)
    }
}

/// The sentence of `paragraph` that holds `word` (compared without case or
/// trailing punctuation), trimmed to `max_chars`; the paragraph's start when
/// no sentence holds it.
pub fn sentence_with(paragraph: &str, word: &str, max_chars: usize) -> String {
    let want = word
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    let mut sentences: Vec<String> = Vec::new();
    let mut current = String::new();
    for c in paragraph.chars() {
        current.push(c);
        if matches!(c, '.' | '!' | '?') {
            sentences.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        sentences.push(current);
    }
    let found = sentences
        .iter()
        .find(|s| {
            s.split_whitespace().any(|w| {
                w.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
                    .contains(&want)
            })
        })
        .cloned()
        .unwrap_or_else(|| paragraph.to_string());
    let trimmed = found.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(word: &str, book: &str) -> SavedWord {
        SavedWord {
            word: word.into(),
            book: book.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_word_is_kept_once_newest_first_and_survives_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut words = SavedWords::default();
        words.record(w("gloaming", "a.epub"));
        words.record(w("sedulous", "b.epub"));
        words.record(w("Gloaming", "b.epub"));
        let list: Vec<(&str, &str)> = words
            .words
            .iter()
            .map(|w| (w.word.as_str(), w.book.as_str()))
            .collect();
        assert_eq!(list, [("Gloaming", "b.epub"), ("sedulous", "b.epub")]);
        assert_eq!(words.in_book("b.epub").count(), 2);
        assert_eq!(words.in_book("a.epub").count(), 0);
        words.save(dir.path()).unwrap();
        let mut back = SavedWords::load(dir.path());
        assert_eq!(back.words.len(), 2);
        assert_eq!(back.remove(0).unwrap().word, "Gloaming");
        assert!(back.remove(5).is_none());
    }

    #[test]
    fn the_sentence_holding_the_word_is_its_context() {
        let p = "The light was going. It had gone gloaming-grey over the water! Then dark.";
        assert_eq!(
            sentence_with(p, "gloaming", 200),
            "It had gone gloaming-grey over the water!"
        );
        assert_eq!(sentence_with(p, "Then", 200), "Then dark.");
        assert_eq!(sentence_with(p, "absent", 10), "The light…");
    }
}
