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
pub fn configured_lists(data_dir: &Path) -> Result<SourceLists> {
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

/// Download, validate and install a source from the configured lists.
/// Returns the manifest of the freshly installed source.
pub fn install_source(data_dir: &Path, source_id: &str) -> Result<gideon_aidoku::SourceManifest> {
    let fetcher = UreqFetcher::new();
    let lists = configured_lists(data_dir)?;
    let (info, package_url) = lists.find_source(&fetcher, source_id)?;

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
    Ok(source.manifest())
}

pub fn cmd_source_install(data_dir: &Path, source_id: &str) -> Result<()> {
    println!("Downloading {source_id}...");
    let manifest = install_source(data_dir, source_id)?;
    println!(
        "Installed {} v{} ({})",
        manifest.info.name, manifest.info.version, manifest.info.id
    );
    Ok(())
}

/// An installed source's identity, as read from its manifest. Sources that
/// fail to load are reported with `broken = true` so UIs can show them
/// without offering to browse.
pub struct InstalledSource {
    pub id: String,
    pub name: String,
    pub broken: bool,
}

/// List installed sources by scanning the sources directory.
pub fn installed_sources(data_dir: &Path) -> Result<Vec<InstalledSource>> {
    let dir = sources_dir(data_dir);
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "aix"))
        .collect();
    paths.sort();
    for path in paths {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match Source::from_aix_file(&path, &settings_dir(data_dir)) {
            Ok(source) => {
                let m = source.manifest();
                out.push(InstalledSource {
                    id: m.info.id,
                    name: m.info.name,
                    broken: false,
                });
            }
            Err(e) => out.push(InstalledSource {
                id: stem.clone(),
                name: format!("{stem} (broken: {e})"),
                broken: true,
            }),
        }
    }
    Ok(out)
}

pub fn cmd_source_installed(data_dir: &Path) -> Result<()> {
    let sources = installed_sources(data_dir)?;
    for source in &sources {
        let marker = if source.broken { " [broken]" } else { "" };
        println!("  {:<30} — {}{}", source.name, source.id, marker);
    }
    if sources.is_empty() {
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
    use std::io::Write;

    let source = load_source(data_dir, source_id)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let mut progress = |done: usize, total: usize| {
        if done == 0 {
            println!("Downloading {total} page(s)...");
        } else {
            print!(".");
            std::io::stdout().flush().ok();
        }
    };
    let out_path = runtime.block_on(download_chapter(
        &source,
        manga_id,
        chapter_id,
        library,
        &mut progress,
    ))?;
    println!(
        "\nSaved to {} — `gideon read` it or open the library.",
        out_path.display()
    );
    Ok(())
}

/// Download a chapter through `source` into `library` as a CBZ (with a
/// ComicInfo.xml), reporting `(pages_done, pages_total)` through `progress`
/// (called once with `(0, total)` before the first page). Returns the path
/// of the written CBZ.
///
/// Individual failed pages do not abort the chapter: each one becomes a
/// self-describing placeholder image (like bobo's downloader) so the CBZ
/// stays complete and readable; only a chapter where *every* page failed
/// is an error.
pub async fn download_chapter(
    source: &Source,
    manga_id: &str,
    chapter_id: &str,
    library: &Path,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<PathBuf> {
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
    progress(0, pages.len());

    // Client configuration mirrors bobo's proven downloader: redirects are
    // followed manually (so the source-set Referer survives every hop, see
    // `execute_with_forced_referer`) and broken CDN certificates are
    // tolerated — manga mirrors routinely have invalid TLS. The user-agent
    // here is only a fallback: `get_image_request` always sets a browser UA.
    // Timeouts are mandatory: a stalled TCP connection must surface as an
    // error page, never freeze the device forever.
    let client = reqwest::Client::builder()
        .user_agent(concat!("gideon/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .danger_accept_invalid_certs(true)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
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

    // A failed page must not abort the chapter (bobo's downloader inserts
    // an error placeholder and keeps going); a 40-page chapter with one
    // dead page must still be readable.
    let mut failed: Vec<String> = Vec::new();
    let mut downloaded = 0usize;
    for (i, page) in pages.iter().enumerate() {
        let Some(image_url) = page.image_url.clone() else {
            // Skipped pages still advance the progress display.
            progress(i + 1, pages.len());
            continue;
        };
        match fetch_page_bytes(source, &client, &token, page, &image_url).await {
            Ok(bytes) => {
                let name = page_file_name(i, pages.len(), &image_url, &bytes);
                cbz_pages.push((name, bytes));
                downloaded += 1;
            }
            Err(error) => {
                let reason = format!("{error:#}");
                eprintln!("gideon download: page {} failed: {reason}", i + 1);
                failed.push(format!("page {}: {reason}", i + 1));
                cbz_pages.push((
                    format!("{:0width$}.png", i + 1, width = width),
                    error_page_png(i + 1, pages.len(), &reason),
                ));
            }
        }
        progress(i + 1, pages.len());
    }

    if downloaded == 0 {
        match failed.first() {
            Some(first) => bail!(
                "all {} page(s) failed to download (first error: {first})",
                failed.len()
            ),
            None => bail!("no pages were downloaded"),
        }
    }
    if !failed.is_empty() {
        eprintln!(
            "gideon download: {}/{} page(s) failed; placeholders inserted",
            failed.len(),
            pages.len()
        );
    }

    let out_path = library
        .join(sanitize(&manga_title))
        .join(format!("{}.cbz", sanitize(&chapter_label)));
    pages_to_cbz(&out_path, &cbz_pages)?;
    Ok(out_path)
}

/// Image extensions the CBZ reader recognizes as pages (must mirror
/// gideon-core's `PAGE_EXTENSIONS`): any other extension would make the
/// reader silently drop the page.
const PAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp"];

/// Pick the file extension for a downloaded page: the URL path's extension
/// when it's a recognized image extension, otherwise sniff the actual bytes
/// (CDNs serve images from `.php` & friends), falling back to "jpg".
fn page_extension(image_url: &Url, bytes: &[u8]) -> String {
    if let Some(ext) = image_url
        .path()
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .filter(|e| PAGE_EXTENSIONS.contains(&e.as_str()))
    {
        return ext;
    }
    image::guess_format(bytes)
        .ok()
        .and_then(|format| format.extensions_str().first().copied())
        .unwrap_or("jpg")
        .to_string()
}

/// Archive entry name for page `index` (0-based) out of `total`.
fn page_file_name(index: usize, total: usize, image_url: &Url, bytes: &[u8]) -> String {
    let width = total.to_string().len().max(3);
    let ext = page_extension(image_url, bytes);
    format!("{:0width$}.{ext}", index + 1, width = width)
}

/// Fetch one page image through the source's request hook, with the
/// forced-referer redirect handling and connection retries bobo uses.
async fn fetch_page_bytes(
    source: &Source,
    client: &reqwest::Client,
    token: &CancellationToken,
    page: &gideon_aidoku::source::model::Page,
    image_url: &Url,
) -> Result<Vec<u8>> {
    // The source may add auth headers / referers to the request.
    let request = source
        .get_image_request(image_url.clone(), page.ctx.clone())
        .await?;
    let (req_url, req_headers) = (request.url().clone(), request.headers().clone());
    let response = execute_with_forced_referer(client, request).await?;
    let status = response.status();
    if !status.is_success() {
        bail!("HTTP {status} from {req_url}");
    }
    let resp_headers = response.headers().clone();
    let bytes = response.bytes().await?;

    // Sources with scrambled images post-process the raw bytes.
    if source.1.process_page_image {
        Ok(source
            .process_page_image(
                token.clone(),
                (req_url, req_headers),
                (status, resp_headers),
                bytes,
                page.ctx.clone(),
            )
            .await?)
    } else {
        Ok(bytes.to_vec())
    }
}

/// Maximum redirect hops followed manually.
const MAX_REDIRECTS: usize = 10;
/// Connection-level attempts per hop (bobo: 3, linear 200ms backoff).
const CONNECT_ATTEMPTS: u32 = 3;

/// Execute `request`, following redirects by hand so the Referer the
/// source set survives every hop — image CDNs commonly 403 requests whose
/// Referer was dropped or rewritten, which is exactly what reqwest's
/// automatic redirect policy does. Connection-level errors are retried
/// (HTTP error statuses are returned to the caller, not retried). Ported
/// from bobo's `request_with_forced_referer_from_request`.
async fn execute_with_forced_referer(
    client: &reqwest::Client,
    mut request: reqwest::Request,
) -> Result<reqwest::Response> {
    use reqwest::header::{LOCATION, REFERER};

    let referer = request.headers().get(REFERER).cloned();

    for _ in 0..MAX_REDIRECTS {
        let method = request.method().clone();
        let headers = request.headers().clone();

        let mut response = None;
        let mut last_err = None;
        for attempt in 1..=CONNECT_ATTEMPTS {
            let cloned = request
                .try_clone()
                .context("request cannot be retried (streaming body)")?;
            match client.execute(cloned).await {
                Ok(resp) => {
                    response = Some(resp);
                    break;
                }
                Err(error) => {
                    last_err = Some(error);
                    if attempt < CONNECT_ATTEMPTS {
                        tokio::time::sleep(std::time::Duration::from_millis(200 * attempt as u64))
                            .await;
                    }
                }
            }
        }
        let response = match (response, last_err) {
            (Some(resp), _) => resp,
            (None, Some(error)) => return Err(error.into()),
            (None, None) => bail!("request produced neither response nor error"),
        };

        if !response.status().is_redirection() {
            return Ok(response);
        }

        let location = response
            .headers()
            .get(LOCATION)
            .context("redirect without Location header")?
            .to_str()
            .context("invalid Location header")?;
        let next_url = response.url().join(location)?;

        let mut next = client.request(method, next_url).build()?;
        let next_headers = next.headers_mut();
        for (key, value) in headers.iter() {
            if key != REFERER {
                next_headers.insert(key, value.clone());
            }
        }
        // Keep the *original* Referer across the hop.
        if let Some(ref referer) = referer {
            next_headers.insert(REFERER, referer.clone());
        }
        request = next;
    }
    bail!("too many redirects")
}

/// A self-describing placeholder page for a failed download, so the
/// chapter stays complete and the error renders on-screen in the reader
/// (mirrors bobo's `generate_error_image`).
fn error_page_png(page_no: usize, total: usize, reason: &str) -> Vec<u8> {
    use gideon_render::text::draw_text;
    use gideon_render::GrayPage;

    let (w, h) = (600u32, 800u32);
    let mut page = GrayPage::new_white(w, h);

    // 2px black border so the placeholder reads as deliberate.
    for x in 0..w {
        for y in [0, 1, h - 2, h - 1] {
            page.pixels[(y * w + x) as usize] = 0;
        }
    }
    for y in 0..h {
        for x in [0, 1, w - 2, w - 1] {
            page.pixels[(y * w + x) as usize] = 0;
        }
    }

    let margin = 40u32;
    draw_text(
        &mut page,
        margin,
        80,
        40.0,
        &format!("Page {page_no}/{total}"),
        w - 2 * margin,
        true,
    );
    draw_text(
        &mut page,
        margin,
        140,
        30.0,
        "failed to download",
        w - 2 * margin,
        false,
    );
    let mut y = 220;
    for line in wrap_text(reason, 40) {
        draw_text(&mut page, margin, y, 24.0, &line, w - 2 * margin, false);
        y += 32;
        if y > h - margin {
            break;
        }
    }

    let image = image::GrayImage::from_raw(w, h, page.pixels)
        .expect("placeholder buffer matches its dimensions");
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(image)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encoding an in-memory PNG cannot fail");
    bytes.into_inner()
}

/// Greedy word wrap at `max_chars` per line.
fn wrap_text(text: &str, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + word.chars().count() + 1 > max_chars {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn xml_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The launcher script and NickelMenu config shipped inside the binary.
/// OTA updates only replace the binary, so the binary itself keeps these
/// device files current (self-heal after update / on browse startup).
const EMBEDDED_LAUNCH_SH: &str = include_str!("../../../installer/gideon-launch.sh");
const EMBEDDED_NICKELMENU: &str = include_str!("../../../installer/nickelmenu-gideon");

/// Ensure the launcher script and NickelMenu entries next to `bin_dir`
/// match the versions this binary shipped with. Quietly does nothing when
/// the layout doesn't look like a device install (e.g. desktop builds).
pub fn ensure_device_files(bin_dir: &Path) -> Result<Vec<&'static str>> {
    let mut updated = Vec::new();

    // <root>/.adds/gideon/bin -> <root>/.adds
    let adds_dir = match bin_dir.parent().and_then(|p| p.parent()) {
        Some(d) if d.file_name().is_some_and(|n| n == ".adds") => d.to_path_buf(),
        _ => return Ok(updated),
    };

    let launch = bin_dir.join("gideon-launch.sh");
    if std::fs::read_to_string(&launch).ok().as_deref() != Some(EMBEDDED_LAUNCH_SH) {
        write_executable(&launch, EMBEDDED_LAUNCH_SH)?;
        updated.push("gideon-launch.sh");
    }

    let nm_dir = adds_dir.join("nm");
    if nm_dir.is_dir() {
        let entry = nm_dir.join("gideon");
        if std::fs::read_to_string(&entry).ok().as_deref() != Some(EMBEDDED_NICKELMENU) {
            atomic_write(&entry, EMBEDDED_NICKELMENU)?;
            updated.push("NickelMenu entries");
        }
    }
    Ok(updated)
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(unix)]
fn write_executable(path: &Path, content: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    atomic_write(path, content)?;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_executable(path: &Path, content: &str) -> Result<()> {
    atomic_write(path, content)
}

#[cfg(test)]
mod download_tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    #[test]
    fn error_page_is_a_decodable_image() {
        let bytes = error_page_png(
            3,
            40,
            "HTTP 403 Forbidden from https://cdn.example.com/x.jpg",
        );
        let image = image::load_from_memory(&bytes).expect("placeholder must decode");
        assert_eq!((image.width(), image.height()), (600, 800));
    }

    #[test]
    fn wrap_text_splits_long_reasons() {
        let lines = wrap_text("one two three four", 9);
        assert_eq!(lines, vec!["one two", "three", "four"]);
        assert!(wrap_text("", 10).is_empty());
        // A single oversized word still lands on its own line.
        assert_eq!(
            wrap_text("supercalifragilistic", 5),
            vec!["supercalifragilistic"]
        );
    }

    #[test]
    fn php_url_with_png_bytes_is_named_png() {
        // CDNs commonly serve pages from script URLs; the reader only
        // accepts known image extensions, so the bytes decide the name.
        let png_bytes = error_page_png(1, 1, "fixture: a real PNG");
        let url = Url::parse("https://cdn.example.com/image.php?id=3").unwrap();
        assert_eq!(page_file_name(0, 40, &url, &png_bytes), "001.png");
    }

    #[test]
    fn known_url_extension_is_kept_without_sniffing() {
        let url = Url::parse("https://cdn.example.com/p/0042.JPG").unwrap();
        // Garbage bytes: the URL extension wins, lowercased.
        assert_eq!(page_file_name(11, 250, &url, b"not an image"), "012.jpg");
        let webp = Url::parse("https://cdn.example.com/p/1.webp").unwrap();
        assert_eq!(page_file_name(0, 9, &webp, b""), "001.webp");
    }

    #[test]
    fn unsniffable_bytes_fall_back_to_jpg() {
        let url = Url::parse("https://cdn.example.com/serve?page=1").unwrap();
        assert_eq!(page_file_name(2, 10, &url, b"\x00\x01\x02"), "003.jpg");
    }

    /// Minimal blocking HTTP server: answers each connection with the next
    /// canned response and records request heads.
    fn one_shot_server(
        responses: Vec<String>,
    ) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                    head.push(byte[0]);
                }
                let _ = tx.send(String::from_utf8_lossy(&head).into_owned());
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (addr, rx)
    }

    #[test]
    fn forced_referer_survives_redirects() {
        let (addr, heads) = one_shot_server(vec![
            "HTTP/1.1 302 Found\r\nLocation: /image.jpg\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                .to_string(),
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok".to_string(),
        ]);

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let request = client
                .get(format!("http://{addr}/start"))
                .header("Referer", "https://manga.example.com/reader")
                .build()
                .unwrap();
            let response = execute_with_forced_referer(&client, request).await.unwrap();
            assert!(response.status().is_success());
            assert_eq!(response.bytes().await.unwrap().as_ref(), b"ok");
        });

        let first = heads.recv().unwrap();
        let second = heads.recv().unwrap();
        assert!(first.contains("GET /start"));
        assert!(second.contains("GET /image.jpg"));
        // The load-bearing assertion: the original Referer survived the hop.
        assert!(
            second
                .to_ascii_lowercase()
                .contains("referer: https://manga.example.com/reader"),
            "redirected request lost the Referer:\n{second}"
        );
    }
}

#[cfg(test)]
mod device_files_tests {
    use super::*;

    fn fake_device(root: &Path) -> PathBuf {
        let bin = root.join(".adds/gideon/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(root.join(".adds/nm")).unwrap();
        bin
    }

    #[test]
    fn installs_launcher_and_menu_when_missing_or_stale() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_device(dir.path());

        let updated = ensure_device_files(&bin).unwrap();
        assert_eq!(updated, vec!["gideon-launch.sh", "NickelMenu entries"]);
        let launch = bin.join("gideon-launch.sh");
        assert_eq!(
            std::fs::read_to_string(&launch).unwrap(),
            EMBEDDED_LAUNCH_SH
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&launch).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".adds/nm/gideon")).unwrap(),
            EMBEDDED_NICKELMENU
        );

        // Up to date: second run is a no-op.
        assert!(ensure_device_files(&bin).unwrap().is_empty());

        // Stale menu gets healed.
        std::fs::write(dir.path().join(".adds/nm/gideon"), "old entry").unwrap();
        assert_eq!(
            ensure_device_files(&bin).unwrap(),
            vec!["NickelMenu entries"]
        );
    }

    #[test]
    fn non_device_layout_is_a_quiet_noop() {
        let dir = tempfile::tempdir().unwrap();
        let updated = ensure_device_files(dir.path()).unwrap();
        assert!(updated.is_empty());
        assert!(!dir.path().join("gideon-launch.sh").exists());
    }

    #[test]
    fn embedded_menu_launches_browse_ui() {
        assert!(EMBEDDED_NICKELMENU.contains("gideon-launch.sh"));
        assert!(EMBEDDED_LAUNCH_SH.contains("browse"));
    }
}
