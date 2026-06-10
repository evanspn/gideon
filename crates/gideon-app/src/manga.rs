//! Source-driven manga commands: install sources from the configured
//! (GitHub-hosted) source lists, then search/browse/download manga through
//! the Aidoku WASM runtime — the same flow bobo provides inside KOReader.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tokio_util::sync::CancellationToken;
use url::Url;

use gideon_aidoku::source::Source;
use gideon_core::Settings;
use gideon_sources::storage::sanitize;
use gideon_sources::{pages_to_cbz, Fetcher, SourceLists, UreqFetcher};

fn sources_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sources")
}

fn settings_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("source-settings")
}

/// Source lists from defaults + settings.json.
fn configured_lists(data_dir: &Path) -> Result<SourceLists> {
    let mut lists = SourceLists::default();
    let settings = Settings::load(data_dir)?;
    for raw in &settings.source_lists {
        if let Ok(url) = Url::parse(raw) {
            lists.add(url);
        }
    }
    Ok(lists)
}

/// Load an installed source by id.
pub fn load_source(data_dir: &Path, source_id: &str) -> Result<Source> {
    let path = sources_dir(data_dir).join(format!("{}.aix", sanitize(source_id)));
    if !path.exists() {
        bail!(
            "source '{source_id}' is not installed — run `gideon source install {source_id}` first"
        );
    }
    Source::from_aix_file(&path, &settings_dir(data_dir))
        .with_context(|| format!("failed to load source {source_id}"))
}

pub fn cmd_source_install(data_dir: &Path, source_id: &str) -> Result<()> {
    let fetcher = UreqFetcher::new();
    let lists = configured_lists(data_dir)?;
    let (info, package_url) = lists.find_source(&fetcher, source_id)?;

    println!("Downloading {} from {package_url}...", info.name);
    let bytes = fetcher.get(&package_url)?;

    let dir = sources_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.aix", sanitize(source_id)));
    let tmp = path.with_extension("aix.part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;
    Source::write_meta_file(&path, info.origin.clone().unwrap_or_default())?;

    // Validate by loading it through the WASM runtime.
    let source = Source::from_aix_file(&path, &settings_dir(data_dir))
        .context("downloaded source failed to load — it was not installed")
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&path);
        })?;
    let manifest = source.manifest();
    println!(
        "Installed {} v{} ({})",
        manifest.info.name, manifest.info.version, manifest.info.id
    );
    Ok(())
}

pub fn cmd_source_installed(data_dir: &Path) -> Result<()> {
    let dir = sources_dir(data_dir);
    let mut found = false;
    if dir.exists() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "aix"))
            .collect();
        paths.sort();
        for path in paths {
            match Source::from_aix_file(&path, &settings_dir(data_dir)) {
                Ok(source) => {
                    let m = source.manifest();
                    println!("  {:<30} v{} — {}", m.info.name, m.info.version, m.info.id);
                    found = true;
                }
                Err(e) => println!("  {:<30} (broken: {e})", path.display()),
            }
        }
    }
    if !found {
        println!("No sources installed. Try `gideon sources` to see what's available,");
        println!("then `gideon source install <id>`.");
    }
    Ok(())
}

pub fn cmd_manga_search(data_dir: &Path, source_id: &str, query: &str) -> Result<()> {
    let source = load_source(data_dir, source_id)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let mangas =
        runtime.block_on(source.search_mangas(CancellationToken::new(), query.to_string()))?;

    if mangas.is_empty() {
        println!("No results for '{query}' on {source_id}.");
        return Ok(());
    }
    println!("{} result(s):\n", mangas.len());
    for manga in mangas {
        println!(
            "  {:<40} id: {}",
            manga.title.as_deref().unwrap_or("(untitled)"),
            manga.id
        );
    }
    Ok(())
}

pub fn cmd_manga_chapters(data_dir: &Path, source_id: &str, manga_id: &str) -> Result<()> {
    let source = load_source(data_dir, source_id)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let chapters = runtime
        .block_on(source.get_chapter_list(CancellationToken::new(), manga_id.to_string()))?;

    if chapters.is_empty() {
        println!("No chapters found for {manga_id}.");
        return Ok(());
    }
    println!("{} chapter(s):\n", chapters.len());
    for chapter in &chapters {
        println!(
            "  ch {:<8} {:<40} [{}] id: {}",
            chapter
                .chapter_num
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into()),
            chapter.title.as_deref().unwrap_or(""),
            chapter.lang.as_deref().unwrap_or("?"),
            chapter.id
        );
    }
    Ok(())
}

pub fn cmd_manga_download(
    data_dir: &Path,
    source_id: &str,
    manga_id: &str,
    chapter_id: &str,
    library: &Path,
) -> Result<()> {
    let source = load_source(data_dir, source_id)?;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(download_chapter(&source, manga_id, chapter_id, library))
}

async fn download_chapter(
    source: &Source,
    manga_id: &str,
    chapter_id: &str,
    library: &Path,
) -> Result<()> {
    let token = CancellationToken::new();

    // Manga details give us a human title for the library path + metadata.
    let manga = source
        .get_manga_details(token.clone(), manga_id.to_string())
        .await
        .ok();
    let manga_title = manga
        .as_ref()
        .and_then(|m| m.title.clone())
        .unwrap_or_else(|| manga_id.to_string());

    let chapters = source
        .get_chapter_list(token.clone(), manga_id.to_string())
        .await?;
    let chapter = chapters.iter().find(|c| c.id == chapter_id);
    let chapter_num = chapter.and_then(|c| c.chapter_num);
    let chapter_label = match (chapter_num, chapter.and_then(|c| c.title.clone())) {
        (Some(n), _) => format!("Chapter {n}"),
        (None, Some(t)) => t,
        _ => chapter_id.to_string(),
    };

    println!("Fetching page list for {manga_title} — {chapter_label}...");
    let pages = source
        .get_page_list(
            token.clone(),
            manga_id.to_string(),
            chapter_id.to_string(),
            chapter_num,
        )
        .await?;
    if pages.is_empty() {
        bail!("source returned no pages for this chapter");
    }
    println!("Downloading {} page(s)...", pages.len());

    let client = reqwest::Client::builder()
        .user_agent(concat!("gideon/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let width = pages.len().to_string().len().max(3);
    let mut cbz_pages: Vec<(String, Vec<u8>)> = Vec::with_capacity(pages.len() + 1);

    // ComicInfo.xml so the library shows proper titles.
    let comic_info = format!(
        "<ComicInfo><Series>{}</Series><Title>{}</Title>{}</ComicInfo>",
        xml_escape(&manga_title),
        xml_escape(&chapter_label),
        chapter_num
            .map(|n| format!("<Number>{n}</Number>"))
            .unwrap_or_default()
    );
    cbz_pages.push(("ComicInfo.xml".to_string(), comic_info.into_bytes()));

    for (i, page) in pages.iter().enumerate() {
        let Some(image_url) = page.image_url.clone() else {
            continue;
        };
        // The source may add auth headers / referers to the request.
        let request = source
            .get_image_request(image_url.clone(), page.ctx.clone())
            .await?;
        let (req_url, req_headers) = (request.url().clone(), request.headers().clone());
        let response = client.execute(request).await?;
        let status = response.status();
        if !status.is_success() {
            bail!("page {} failed: HTTP {status} from {req_url}", i + 1);
        }
        let resp_headers = response.headers().clone();
        let bytes = response.bytes().await?;

        // Sources with scrambled images post-process the raw bytes.
        let bytes = if source.1.process_page_image {
            source
                .process_page_image(
                    token.clone(),
                    (req_url, req_headers),
                    (status, resp_headers),
                    bytes,
                    page.ctx.clone(),
                )
                .await?
        } else {
            bytes.to_vec()
        };

        let ext = image_url
            .path()
            .rsplit('.')
            .next()
            .filter(|e| e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or("jpg")
            .to_ascii_lowercase();
        cbz_pages.push((format!("{:0width$}.{ext}", i + 1, width = width), bytes));
        print!(".");
        use std::io::Write;
        std::io::stdout().flush().ok();
    }
    println!();

    if cbz_pages.len() <= 1 {
        bail!("no pages were downloaded");
    }

    let out_path = library
        .join(sanitize(&manga_title))
        .join(format!("{}.cbz", sanitize(&chapter_label)));
    pages_to_cbz(&out_path, &cbz_pages)?;
    println!(
        "Saved {} page(s) to {} — `gideon read` it or open the library.",
        cbz_pages.len() - 1,
        out_path.display()
    );
    Ok(())
}

fn xml_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
