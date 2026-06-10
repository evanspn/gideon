//! Source list parsing and source resolution.
//!
//! Two JSON shapes are accepted, mirroring bobo's behavior:
//!
//! 1. A bare array: `[{"id": "...", "name": "...", "version": 1, "file": "..."}]`
//! 2. A wrapper object: `{"sources": [ ... ]}`
//!
//! Package download paths come from either `file` or `downloadURL`, resolved
//! relative to the source list URL (with the `sources/` directory convention
//! used by Aidoku-style repositories).

use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::fetch::Fetcher;
use crate::{Error, Result};

/// Source lists preinstalled in gideon — the same default bobo ships in its
/// `default-settings.json` (the Aidoku community source list, hosted on
/// GitHub Pages). Users can add their own lists on top; any
/// Aidoku-compatible list works.
pub const DEFAULT_SOURCE_LISTS: &[&str] =
    &["https://aidoku-community.github.io/sources/index.min.json"];

/// One installable source as described by a source list.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SourceInformation {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: u32,
    #[serde(default, alias = "downloadURL")]
    pub file: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
    /// Newer lists (like the Aidoku community list) use a `languages` array
    /// instead of a single `lang` string.
    #[serde(default)]
    pub languages: Vec<String>,
    /// Icon file name, resolved relative to the list's `icons/` directory.
    #[serde(default, alias = "iconURL")]
    pub icon: Option<String>,
    /// Domain of the source list this entry came from (filled in by us).
    #[serde(skip)]
    pub origin: Option<String>,
}

impl SourceInformation {
    /// The source's primary language, whichever format the list used.
    pub fn primary_language(&self) -> Option<&str> {
        self.lang
            .as_deref()
            .or_else(|| self.languages.first().map(String::as_str))
    }
}

/// The set of configured source lists (defaults + user additions).
#[derive(Debug, Clone)]
pub struct SourceLists {
    lists: Vec<Url>,
}

impl Default for SourceLists {
    fn default() -> Self {
        Self {
            lists: DEFAULT_SOURCE_LISTS
                .iter()
                .map(|u| Url::parse(u).expect("default source list URL is valid"))
                .collect(),
        }
    }
}

impl SourceLists {
    pub fn new(lists: Vec<Url>) -> Self {
        Self { lists }
    }

    pub fn urls(&self) -> &[Url] {
        &self.lists
    }

    pub fn add(&mut self, url: Url) {
        if !self.lists.contains(&url) {
            self.lists.push(url);
        }
    }

    /// Fetch every configured list and return all available sources,
    /// sorted by name.
    pub fn available_sources(&self, fetcher: &dyn Fetcher) -> Result<Vec<SourceInformation>> {
        let mut sources = Vec::new();
        for list_url in &self.lists {
            let body = fetcher.get(list_url)?;
            let mut parsed = parse_source_list(&body).map_err(|message| Error::ParseList {
                url: list_url.to_string(),
                message,
            })?;
            let origin = list_url.domain().unwrap_or("").to_string();
            for source in &mut parsed {
                source.origin = Some(origin.clone());
            }
            sources.extend(parsed);
        }
        sources.sort_by_key(|source| source.name.to_lowercase());
        Ok(sources)
    }

    /// Find a source by id across all lists and return it together with the
    /// resolved package download URL.
    pub fn find_source(
        &self,
        fetcher: &dyn Fetcher,
        source_id: &str,
    ) -> Result<(SourceInformation, Url)> {
        for list_url in &self.lists {
            let body = fetcher.get(list_url)?;
            let parsed = parse_source_list(&body).map_err(|message| Error::ParseList {
                url: list_url.to_string(),
                message,
            })?;
            if let Some(source) = parsed.into_iter().find(|s| s.id == source_id) {
                let package_url = resolve_package_url(list_url, &source)?;
                return Ok((source, package_url));
            }
        }
        Err(Error::SourceNotFound(source_id.to_string()))
    }
}

/// Parse a source list body in either accepted JSON shape.
pub fn parse_source_list(body: &[u8]) -> std::result::Result<Vec<SourceInformation>, String> {
    let value: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;

    let array = if value.is_array() {
        value
    } else if let Some(sources) = value.get("sources").filter(|v| v.is_array()) {
        sources.clone()
    } else {
        return Err("expected a JSON array or an object with a 'sources' array".to_string());
    };

    serde_json::from_value(array).map_err(|e| e.to_string())
}

/// Resolve where to download a source package from, following the same
/// conventions bobo uses: `file` is relative to the list URL, and lives in
/// the `sources/` directory unless the path already says so.
pub fn resolve_package_url(list_url: &Url, source: &SourceInformation) -> Result<Url> {
    let file = source
        .file
        .as_deref()
        .ok_or_else(|| Error::SourceNotFound(format!("{} has no download file", source.id)))?;

    // Absolute URLs are used as-is.
    if let Ok(absolute) = Url::parse(file) {
        return Ok(absolute);
    }

    let relative = if file.starts_with("sources/") {
        file.to_string()
    } else {
        format!("sources/{file}")
    };
    Ok(list_url.join(&relative)?)
}

/// Resolve a source's icon URL, following the `icons/` directory convention
/// used by Aidoku-style repositories.
pub fn resolve_icon_url(list_url: &Url, source: &SourceInformation) -> Option<Url> {
    let icon = source.icon.as_deref()?;
    if let Ok(absolute) = Url::parse(icon) {
        return Some(absolute);
    }
    let relative = if icon.starts_with("icons/") {
        icon.to_string()
    } else {
        format!("icons/{icon}")
    };
    list_url.join(&relative).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::FakeFetcher;

    const LIST_URL: &str = "https://raw.githubusercontent.com/example/sources/gh-pages/index.json";

    #[test]
    fn parses_bare_array_format() {
        let body = br#"[
            {"id": "en.mangadex", "name": "MangaDex", "version": 3, "file": "en.mangadex-v3.aix", "lang": "en"},
            {"id": "en.other", "name": "Other", "version": 1, "downloadURL": "sources/en.other-v1.aix"}
        ]"#;
        let sources = parse_source_list(body).unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].id, "en.mangadex");
        assert_eq!(sources[0].version, 3);
        assert_eq!(sources[0].file.as_deref(), Some("en.mangadex-v3.aix"));
        assert_eq!(sources[1].file.as_deref(), Some("sources/en.other-v1.aix"));
    }

    #[test]
    fn parses_wrapped_object_format() {
        let body = br#"{"sources": [{"id": "a", "name": "A", "version": 1}]}"#;
        let sources = parse_source_list(body).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "A");
    }

    #[test]
    fn rejects_unknown_shapes() {
        assert!(parse_source_list(br#"{"not_sources": []}"#).is_err());
        assert!(parse_source_list(b"42").is_err());
        assert!(parse_source_list(b"not json").is_err());
    }

    #[test]
    fn default_lists_are_valid_urls() {
        let lists = SourceLists::default();
        assert!(!lists.urls().is_empty());
        assert!(lists.urls().iter().all(|u| u.scheme() == "https"));
    }

    #[test]
    fn add_deduplicates() {
        let mut lists = SourceLists::default();
        let before = lists.urls().len();
        lists.add(Url::parse(DEFAULT_SOURCE_LISTS[0]).unwrap());
        assert_eq!(lists.urls().len(), before);
        lists.add(Url::parse("https://example.com/index.json").unwrap());
        assert_eq!(lists.urls().len(), before + 1);
    }

    #[test]
    fn available_sources_merges_and_sorts() {
        let other_url = "https://example.com/index.json";
        let fetcher = FakeFetcher::new()
            .with(
                LIST_URL,
                br#"[{"id": "z", "name": "Zeta", "version": 1}]"#.to_vec(),
            )
            .with(
                other_url,
                br#"{"sources": [{"id": "a", "name": "alpha", "version": 1}]}"#.to_vec(),
            );

        let lists = SourceLists::new(vec![
            Url::parse(LIST_URL).unwrap(),
            Url::parse(other_url).unwrap(),
        ]);
        let sources = lists.available_sources(&fetcher).unwrap();
        let names: Vec<&str> = sources.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "Zeta"]);
        assert_eq!(sources[0].origin.as_deref(), Some("example.com"));
        assert_eq!(
            sources[1].origin.as_deref(),
            Some("raw.githubusercontent.com")
        );
    }

    #[test]
    fn resolve_relative_package_path() {
        let list_url = Url::parse(LIST_URL).unwrap();
        let source = SourceInformation {
            id: "en.x".into(),
            name: "X".into(),
            version: 1,
            file: Some("en.x-v1.aix".into()),
            lang: None,
            languages: Vec::new(),
            icon: None,
            origin: None,
        };
        let url = resolve_package_url(&list_url, &source).unwrap();
        assert_eq!(
            url.as_str(),
            "https://raw.githubusercontent.com/example/sources/gh-pages/sources/en.x-v1.aix"
        );
    }

    #[test]
    fn resolve_keeps_existing_sources_prefix_and_absolute_urls() {
        let list_url = Url::parse(LIST_URL).unwrap();

        let prefixed = SourceInformation {
            id: "a".into(),
            name: "A".into(),
            version: 1,
            file: Some("sources/a.aix".into()),
            lang: None,
            languages: Vec::new(),
            icon: None,
            origin: None,
        };
        assert_eq!(
            resolve_package_url(&list_url, &prefixed).unwrap().as_str(),
            "https://raw.githubusercontent.com/example/sources/gh-pages/sources/a.aix"
        );

        let absolute = SourceInformation {
            file: Some("https://cdn.example.com/b.aix".into()),
            ..prefixed
        };
        assert_eq!(
            resolve_package_url(&list_url, &absolute).unwrap().as_str(),
            "https://cdn.example.com/b.aix"
        );
    }

    #[test]
    fn find_source_resolves_download_url() {
        let fetcher = FakeFetcher::new().with(
            LIST_URL,
            br#"[{"id": "en.target", "name": "Target", "version": 2, "file": "en.target-v2.aix"}]"#
                .to_vec(),
        );
        let lists = SourceLists::new(vec![Url::parse(LIST_URL).unwrap()]);

        let (source, url) = lists.find_source(&fetcher, "en.target").unwrap();
        assert_eq!(source.name, "Target");
        assert!(url.as_str().ends_with("/sources/en.target-v2.aix"));

        assert!(matches!(
            lists.find_source(&fetcher, "en.missing"),
            Err(Error::SourceNotFound(_))
        ));
    }

    #[test]
    fn parses_aidoku_community_format() {
        // Entry shape from https://aidoku-community.github.io/sources/index.min.json,
        // the same default source list bobo ships.
        let body = br#"[{
            "id": "en.aquamanga",
            "name": "Aqua Manga",
            "version": 1,
            "iconURL": "icons/en.aquamanga-v1.png",
            "downloadURL": "sources/en.aquamanga-v1.aix",
            "languages": ["en"],
            "contentRating": 1,
            "baseURL": "https://aquareader.net"
        }]"#;
        let sources = parse_source_list(body).unwrap();
        assert_eq!(sources.len(), 1);
        let s = &sources[0];
        assert_eq!(s.file.as_deref(), Some("sources/en.aquamanga-v1.aix"));
        assert_eq!(s.icon.as_deref(), Some("icons/en.aquamanga-v1.png"));
        assert_eq!(s.primary_language(), Some("en"));

        let list_url =
            Url::parse("https://aidoku-community.github.io/sources/index.min.json").unwrap();
        assert_eq!(
            resolve_package_url(&list_url, s).unwrap().as_str(),
            "https://aidoku-community.github.io/sources/sources/en.aquamanga-v1.aix"
        );
        assert_eq!(
            resolve_icon_url(&list_url, s).unwrap().as_str(),
            "https://aidoku-community.github.io/sources/icons/en.aquamanga-v1.png"
        );
    }

    #[test]
    fn resolve_icon_url_follows_icons_convention() {
        let list_url = Url::parse(LIST_URL).unwrap();
        let source = SourceInformation {
            id: "a".into(),
            name: "A".into(),
            version: 1,
            file: None,
            lang: None,
            languages: Vec::new(),
            icon: Some("a-v1.png".into()),
            origin: None,
        };
        assert_eq!(
            resolve_icon_url(&list_url, &source).unwrap().as_str(),
            "https://raw.githubusercontent.com/example/sources/gh-pages/icons/a-v1.png"
        );

        let no_icon = SourceInformation {
            icon: None,
            ..source.clone()
        };
        assert!(resolve_icon_url(&list_url, &no_icon).is_none());

        let absolute = SourceInformation {
            icon: Some("https://cdn.example.com/i.png".into()),
            ..source
        };
        assert_eq!(
            resolve_icon_url(&list_url, &absolute).unwrap().as_str(),
            "https://cdn.example.com/i.png"
        );
    }
}
