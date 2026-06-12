//! Online integration tests against the real preinstalled GitHub source list.
//!
//! These hit the network, so they're `#[ignore]`d in normal runs; the
//! post-merge workflow runs them with `--ignored`. They verify the same
//! pipeline a manga chapter goes through: resolve a source from the GitHub
//! list, download real files, pack them into a CBZ, open it with
//! gideon-core and render pages with gideon-render.
//!
//! Until the Aidoku WASM runtime lands (ROADMAP v2), chapter page URLs
//! can't be produced by sources yet, so the download-and-render test uses
//! the source icons — real PNGs hosted in the same GitHub repository —
//! as stand-in pages. The transport, packaging, decoding and rendering
//! code paths are identical.

use gideon_core::CbzDocument;
use gideon_render::{render_page, FitMode, RenderOptions};
use gideon_sources::download::download_chapter_to_cbz;
use gideon_sources::list::resolve_icon_url;
use gideon_sources::{Fetcher, SourceLists, UreqFetcher};
use url::Url;

fn default_list_url() -> Url {
    Url::parse(gideon_sources::DEFAULT_SOURCE_LISTS[0]).unwrap()
}

#[test]
#[ignore = "network: fetches the live GitHub source list"]
fn live_source_list_has_sources() {
    let lists = SourceLists::default();
    let sources = lists
        .available_sources(&UreqFetcher::new())
        .expect("fetching the preinstalled source list should work");
    assert!(
        sources.len() >= 10,
        "expected a healthy number of sources, got {}",
        sources.len()
    );
    // Every source must be resolvable to a download URL or explicitly fileless.
    let with_files = sources.iter().filter(|s| s.file.is_some()).count();
    assert!(with_files > 0, "no sources have downloadable packages");
}

#[test]
#[ignore = "network: downloads a real source package from GitHub"]
fn download_real_source_package() {
    let fetcher = UreqFetcher::new();
    let lists = SourceLists::default();
    let sources = lists.available_sources(&fetcher).unwrap();
    let source = sources
        .iter()
        .find(|s| s.file.is_some())
        .expect("at least one source with a package file");

    let (_, package_url) = lists.find_source(&fetcher, &source.id).unwrap();
    let bytes = fetcher.get(&package_url).unwrap();
    assert!(!bytes.is_empty(), "package download was empty");

    // .aix packages are zip archives containing the source's WASM payload.
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .expect("source package should be a valid zip");
    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
    assert!(
        names.iter().any(|n| n.ends_with(".wasm")),
        "package has no WASM payload: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with("source.json")),
        "package has no source.json: {names:?}"
    );
    // The manifest must be readable.
    let manifest_name = names
        .iter()
        .find(|n| n.ends_with("source.json"))
        .unwrap()
        .clone();
    let manifest = archive.by_name(&manifest_name).unwrap();
    let parsed: serde_json::Value = serde_json::from_reader(manifest).unwrap();
    assert!(parsed.is_object(), "source.json is not a JSON object");
}

#[test]
#[ignore = "network: downloads real images from a GitHub source and renders them"]
fn download_and_render_pages_from_github_source() {
    let fetcher = UreqFetcher::new();
    let lists = SourceLists::default();
    let sources = lists.available_sources(&fetcher).unwrap();
    let list_url = default_list_url();

    // Real PNGs hosted in the GitHub source repository.
    let page_urls: Vec<Url> = sources
        .iter()
        .filter_map(|s| resolve_icon_url(&list_url, s))
        .take(3)
        .collect();
    assert!(
        !page_urls.is_empty(),
        "no sources in the live list have icons to download"
    );

    // Download → pack as CBZ, exactly like an offline chapter.
    let dir = tempfile::tempdir().unwrap();
    let cbz_path = dir.path().join("downloads/integration/ch1.cbz");
    download_chapter_to_cbz(&fetcher, &page_urls, &cbz_path).unwrap();

    // Open and render every page through the e-ink pipeline.
    let mut doc = CbzDocument::open(&cbz_path).expect("downloaded CBZ should open");
    assert_eq!(doc.page_count(), page_urls.len());

    let opts = RenderOptions {
        screen_width: 1072,
        screen_height: 1448,
        fit: FitMode::Contain,
        dither: true,
    };
    for page_index in 0..doc.page_count() {
        let image = doc
            .decode_page(page_index)
            .unwrap_or_else(|e| panic!("page {page_index} failed to decode: {e}"));
        let rendered = render_page(&image, &opts);
        assert_eq!((rendered.width(), rendered.height()), (1072, 1448));
        match rendered {
            gideon_render::PageBuf::Gray(page) => {
                assert!(
                    page.pixels.iter().any(|&p| p != 0xFF),
                    "page {page_index} rendered fully white — decode or render is broken"
                );
                // Dithered output must respect the 16-level e-ink palette.
                assert!(page.pixels.iter().all(|&p| p % 17 == 0));
            }
            // Source icons are usually color art: it keeps its RGB (and is
            // hardware-dithered on the panel, never software-quantized).
            gideon_render::PageBuf::Rgb(page) => {
                assert!(
                    page.pixels.iter().any(|&p| p != 0xFF),
                    "page {page_index} rendered fully white — decode or render is broken"
                );
            }
        }
    }
}

#[test]
#[ignore = "network: checks the real latest gideon release for OTA"]
fn ota_version_asset_resolves_on_latest_release() {
    use gideon_sources::update;

    let fetcher = UreqFetcher::new();
    let base = update::release_base();
    // An ancient current version guarantees any published release is "newer".
    match update::check_update_via_assets(&fetcher, &base, update::DEFAULT_UPDATE_REPO, "0.0.0") {
        Ok(Some(release)) => {
            assert!(!release.version.is_empty());
            assert!(release
                .asset_url
                .as_str()
                .ends_with(&format!("gideon-kobo-v{}.zip", release.version)));
            // The bundle asset must actually be downloadable.
            let bundle = fetcher
                .get(&release.asset_url)
                .expect("bundle should download");
            assert!(bundle.len() > 1024, "bundle suspiciously small");
        }
        Ok(None) => panic!("VERSION asset exists but reports nothing newer than 0.0.0"),
        Err(e) => {
            let msg = e.to_string();
            // No release published yet (or repo still private): skip rather
            // than fail, so this gate activates with the first release.
            assert!(
                msg.contains("404") || msg.contains("status"),
                "unexpected failure: {msg}"
            );
            eprintln!("skipping: no published release with VERSION asset yet ({msg})");
        }
    }
}

#[test]
#[ignore = "network: full WASM source pipeline — install, search, chapters, download, render"]
fn wasm_source_end_to_end() {
    use gideon_aidoku::source::Source;
    use tokio_util::sync::CancellationToken;

    let fetcher = UreqFetcher::new();
    let lists = SourceLists::default();
    let data_dir = tempfile::tempdir().unwrap();

    // Install a real source from the live GitHub list.
    let (_, package_url) = lists
        .find_source(&fetcher, "multi.mangadex")
        .expect("multi.mangadex should exist in the community list");
    let aix_path = data_dir.path().join("multi.mangadex.aix");
    std::fs::write(&aix_path, fetcher.get(&package_url).unwrap()).unwrap();

    let settings_dir = data_dir.path().join("settings");
    let source = Source::from_aix_file(&aix_path, &settings_dir)
        .expect("source should load in the WASM runtime");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let token = CancellationToken::new();

        // Search through the WASM source.
        let mangas = source
            .search_mangas(token.clone(), "berserk".to_string())
            .await
            .expect("search should succeed");
        assert!(!mangas.is_empty(), "no search results from the source");

        // Find one with chapters and pages (try a few results).
        for manga in mangas.iter().take(5) {
            let chapters = match source
                .get_chapter_list(token.clone(), manga.id.clone())
                .await
            {
                Ok(c) if !c.is_empty() => c,
                _ => continue,
            };
            let chapter = &chapters[0];
            let pages = match source
                .get_page_list(
                    token.clone(),
                    manga.id.clone(),
                    chapter.id.clone(),
                    chapter.chapter_num,
                )
                .await
            {
                Ok(p) if !p.is_empty() => p,
                _ => continue,
            };

            // Download the first page through the source's request hook and
            // render it through the e-ink pipeline.
            let page = pages
                .iter()
                .find(|p| p.image_url.is_some())
                .expect("chapter has no image pages");
            let image_url = page.image_url.clone().unwrap();
            let request = source
                .get_image_request(image_url, page.ctx.clone())
                .await
                .expect("image request hook failed");
            let client = reqwest::Client::new();
            let bytes = client
                .execute(request)
                .await
                .expect("page download failed")
                .bytes()
                .await
                .unwrap();
            assert!(bytes.len() > 1024, "page suspiciously small");

            let image = image::load_from_memory(&bytes).expect("page should decode");
            let rendered = render_page(
                &image,
                &RenderOptions {
                    screen_width: 1072,
                    screen_height: 1448,
                    fit: FitMode::Contain,
                    dither: true,
                },
            );
            assert!(rendered.into_gray().pixels.iter().any(|&p| p != 0xFF));
            return; // full pipeline verified
        }
        panic!("no manga in the first 5 results had downloadable pages");
    });
}
