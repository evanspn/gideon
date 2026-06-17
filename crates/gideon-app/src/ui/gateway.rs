//! The [`SourceGateway`] trait: everything the browse UI needs from the
//! manga-source backend, behind one object so the UI is unit-testable with
//! a fake (no network, no WASM runtime).
//!
//! [`AidokuGateway`] is the production implementation, built on the Aidoku
//! WASM runtime (`gideon-aidoku`) and the source-list machinery
//! (`gideon-sources`), reusing the same functions the CLI commands use.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use gideon_aidoku::source::Source;
use gideon_sources::UreqFetcher;

use crate::manga;

/// A source as shown in the Sources screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    pub id: String,
    pub name: String,
}

/// A manga as shown in list screens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MangaEntry {
    pub id: String,
    pub title: String,
    /// Cover art URL from the source, fetched once per series at download
    /// time so library cards can show the real manga cover.
    pub cover_url: Option<String>,
}

/// A chapter as shown in the chapter list.
#[derive(Debug, Clone, PartialEq)]
pub struct ChapterEntry {
    pub id: String,
    pub num: Option<f32>,
    pub title: Option<String>,
    pub lang: Option<String>,
}

impl ChapterEntry {
    /// Row label: `Ch {num} — {title} [{lang}]`, omitting missing pieces.
    pub fn label(&self) -> String {
        let mut out = match self.num {
            Some(n) => format!("Ch {n}"),
            None => "Ch ?".to_string(),
        };
        if let Some(title) = self.title.as_deref().filter(|t| !t.is_empty()) {
            out.push_str(" — ");
            out.push_str(title);
        }
        if let Some(lang) = self.lang.as_deref().filter(|l| !l.is_empty()) {
            out.push_str(&format!(" [{lang}]"));
        }
        out
    }
}

/// What the browse UI needs from the source backend. All methods are
/// blocking; errors are surfaced to the user on an error screen, never
/// panicked on.
pub trait SourceGateway {
    /// Installed sources, in stable order.
    fn installed_sources(&self) -> Result<Vec<SourceEntry>>;

    /// Sources available from the configured source lists (may include ones
    /// that are already installed; the UI filters).
    fn available_sources(&self) -> Result<Vec<SourceEntry>>;

    /// Download and install a source by id.
    fn install_source(&self, source_id: &str) -> Result<()>;

    /// Fetch a listing ("Popular", "Latest") from a source, falling back to
    /// an empty search when the source has no such listing.
    fn list_manga(&self, source_id: &str, listing: &str) -> Result<Vec<MangaEntry>>;

    /// Search a source for manga matching `query`.
    fn search_manga(&self, source_id: &str, query: &str) -> Result<Vec<MangaEntry>>;

    /// Download a manga cover image to `dest` (best-effort metadata).
    fn download_cover(&self, url: &str, dest: &Path) -> Result<()>;

    /// Chapter list for a manga.
    fn chapters(&self, source_id: &str, manga_id: &str) -> Result<Vec<ChapterEntry>>;

    /// Download a chapter into `library` as a CBZ, reporting
    /// `(pages_done, pages_total)`. Returns the written CBZ path.
    fn download_chapter(
        &self,
        source_id: &str,
        manga_id: &str,
        chapter_id: &str,
        library: &Path,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<PathBuf>;

    /// A standalone copy of this gateway that can run on a background thread,
    /// for pre-downloading chapters while the user reads. `None` (the default)
    /// means "no background pre-download" — callers fall back to a foreground
    /// download. The clone shares no mutable state with `self`; it builds its
    /// own HTTP client / runtime / source cache lazily on first use.
    fn background_clone(&self) -> Option<Box<dyn SourceGateway + Send>> {
        None
    }

    /// Check for app updates.
    fn check_updates(&self) -> Result<UpdateCheck>;

    /// Download and install the available update; returns a status line.
    fn install_update(&self) -> Result<String>;
}

/// Result of an update check.
pub struct UpdateCheck {
    pub message: String,
    pub available: bool,
}

/// Production gateway: Aidoku WASM sources + GitHub-hosted source lists.
pub struct AidokuGateway {
    data_dir: PathBuf,
    /// Loaded WASM sources, cached per id — instantiating a source is slow.
    cache: RefCell<HashMap<String, Source>>,
    /// One tokio runtime for the whole session: building a runtime (and
    /// its thread pool) per gateway call added latency to every tap.
    runtime: OnceCell<tokio::runtime::Runtime>,
    /// One HTTP client (= connection pool) shared by every chapter and
    /// cover download, so keep-alive connections and TLS sessions are
    /// reused across calls.
    client: OnceCell<reqwest::Client>,
}

impl AidokuGateway {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            cache: RefCell::new(HashMap::new()),
            runtime: OnceCell::new(),
            client: OnceCell::new(),
        }
    }

    fn source(&self, source_id: &str) -> Result<Source> {
        if let Some(source) = self.cache.borrow().get(source_id) {
            return Ok(source.clone());
        }
        let source = manga::load_source(&self.data_dir, source_id)?;
        self.cache
            .borrow_mut()
            .insert(source_id.to_string(), source.clone());
        Ok(source)
    }

    /// The shared runtime, built on first use (fallible, so not
    /// `get_or_init`).
    fn runtime(&self) -> Result<&tokio::runtime::Runtime> {
        if self.runtime.get().is_none() {
            let runtime =
                tokio::runtime::Runtime::new().context("failed to start async runtime")?;
            let _ = self.runtime.set(runtime);
        }
        Ok(self.runtime.get().expect("initialized above"))
    }

    /// The shared download client, built on first use.
    fn client(&self) -> Result<&reqwest::Client> {
        if self.client.get().is_none() {
            let _ = self.client.set(manga::http_client()?);
        }
        Ok(self.client.get().expect("initialized above"))
    }
}

impl SourceGateway for AidokuGateway {
    fn installed_sources(&self) -> Result<Vec<SourceEntry>> {
        Ok(manga::installed_sources(&self.data_dir)?
            .into_iter()
            .map(|s| SourceEntry {
                id: s.id,
                name: s.name,
            })
            .collect())
    }

    fn available_sources(&self) -> Result<Vec<SourceEntry>> {
        let lists = manga::configured_lists(&self.data_dir)?;
        let fetcher = UreqFetcher::new();
        let sources = lists.available_sources(&fetcher)?;
        Ok(sources
            .into_iter()
            .map(|s| SourceEntry {
                id: s.id,
                name: s.name,
            })
            .collect())
    }

    fn install_source(&self, source_id: &str) -> Result<()> {
        manga::install_source(&self.data_dir, source_id)?;
        Ok(())
    }

    fn list_manga(&self, source_id: &str, listing: &str) -> Result<Vec<MangaEntry>> {
        let source = self.source(source_id)?;
        let runtime = self.runtime()?;

        let aidoku_listing = gideon_aidoku::aidoku::Listing {
            id: listing.to_lowercase(),
            name: listing.to_string(),
            kind: gideon_aidoku::aidoku::ListingKind::default(),
        };
        let mangas =
            match runtime.block_on(source.get_manga_list(CancellationToken::new(), aidoku_listing))
            {
                Ok(mangas) => mangas,
                // The source may not implement this listing — fall back to an
                // empty search, which most sources answer with a default list.
                Err(_) => runtime
                    .block_on(source.search_mangas(CancellationToken::new(), String::new()))?,
            };

        Ok(mangas
            .into_iter()
            .map(|m| MangaEntry {
                title: m.title.unwrap_or_else(|| m.id.clone()),
                cover_url: m.cover_url.map(|u| u.to_string()),
                id: m.id,
            })
            .collect())
    }

    fn search_manga(&self, source_id: &str, query: &str) -> Result<Vec<MangaEntry>> {
        let source = self.source(source_id)?;
        let runtime = self.runtime()?;
        let mangas =
            runtime.block_on(source.search_mangas(CancellationToken::new(), query.to_string()))?;
        Ok(mangas
            .into_iter()
            .map(|m| MangaEntry {
                title: m.title.unwrap_or_else(|| m.id.clone()),
                cover_url: m.cover_url.map(|u| u.to_string()),
                id: m.id,
            })
            .collect())
    }

    fn download_cover(&self, url: &str, dest: &Path) -> Result<()> {
        let runtime = self.runtime()?;
        runtime.block_on(manga::download_cover(self.client()?, url, dest))
    }

    fn chapters(&self, source_id: &str, manga_id: &str) -> Result<Vec<ChapterEntry>> {
        let source = self.source(source_id)?;
        let runtime = self.runtime()?;
        let chapters = runtime
            .block_on(source.get_chapter_list(CancellationToken::new(), manga_id.to_string()))?;
        Ok(chapters
            .into_iter()
            .map(|c| ChapterEntry {
                id: c.id,
                num: c.chapter_num,
                title: c.title,
                lang: c.lang,
            })
            .collect())
    }

    fn download_chapter(
        &self,
        source_id: &str,
        manga_id: &str,
        chapter_id: &str,
        library: &Path,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<PathBuf> {
        let source = self.source(source_id)?;
        let runtime = self.runtime()?;
        runtime.block_on(manga::download_chapter(
            &source,
            self.client()?,
            manga_id,
            chapter_id,
            library,
            progress,
        ))
    }

    fn check_updates(&self) -> Result<UpdateCheck> {
        let release = latest_release()?;
        let current = env!("CARGO_PKG_VERSION");
        Ok(match release {
            Some(release) => UpdateCheck {
                message: format!("Update available: {current} -> {}.", release.version),
                available: true,
            },
            None => UpdateCheck {
                message: format!("gideon {current} is up to date."),
                available: false,
            },
        })
    }

    fn install_update(&self) -> Result<String> {
        use gideon_sources::update;

        let Some(release) = latest_release()? else {
            return Ok("Already up to date.".to_string());
        };
        let current = env!("CARGO_PKG_VERSION");
        if !update::is_auto_installable(current, &release.version) {
            return Ok(format!(
                "{} is a major upgrade from {current}; install it over USB.",
                release.version
            ));
        }

        let exe = std::env::current_exe()?;
        let bin_dir = exe
            .parent()
            .context("can't determine the binary's directory")?;
        let fetcher = UreqFetcher::new();
        update::stage_update(&fetcher, &release, bin_dir)?;
        update::apply_staged(bin_dir)?;
        crate::manga::ensure_device_files(bin_dir)?;
        Ok(format!(
            "Updated to {}.\nClose gideon and reopen it to use the new version.",
            release.version
        ))
    }

    fn background_clone(&self) -> Option<Box<dyn SourceGateway + Send>> {
        // A fresh gateway over the same data dir: it loads its own WASM
        // sources and HTTP client lazily, sharing no mutable state with the
        // foreground one — safe to move onto the pre-download thread.
        Some(Box::new(AidokuGateway::new(self.data_dir.clone())))
    }
}

/// The newest published release, via the VERSION asset with API fallback.
fn latest_release() -> Result<Option<gideon_sources::update::ReleaseInfo>> {
    use gideon_sources::update;
    let repo = std::env::var("GIDEON_UPDATE_REPO")
        .unwrap_or_else(|_| update::DEFAULT_UPDATE_REPO.to_string());
    let current = env!("CARGO_PKG_VERSION");
    let fetcher = UreqFetcher::new();
    let base = update::release_base();
    let release = update::check_update_via_assets(&fetcher, &base, &repo, current)
        .or_else(|_| update::check_update(&fetcher, &repo, current))?;
    Ok(release)
}
