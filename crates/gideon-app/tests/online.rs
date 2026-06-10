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
        assert_eq!((rendered.width, rendered.height), (1072, 1448));
        assert!(
            rendered.pixels.iter().any(|&p| p != 0xFF),
            "page {page_index} rendered fully white — decode or render is broken"
        );
        // Dithered output must respect the 16-level e-ink palette.
        assert!(rendered.pixels.iter().all(|&p| p % 17 == 0));
    }
}
