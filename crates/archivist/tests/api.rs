//! The client against captured archive.org responses: no network.

use url::Url;

use archivist::{Client, FakeFetcher, SearchOptions};

const SEARCH: &[u8] = include_bytes!("fixtures/search.json");
const ITEM: &[u8] = include_bytes!("fixtures/item.json");

fn client_with(search_url: Option<&str>) -> Client<FakeFetcher> {
    let mut f = FakeFetcher::new()
        .with("https://archive.org/metadata/slaughterhouse-five-kurt-vonnegut", ITEM)
        .with(
            "https://archive.org/download/slaughterhouse-five-kurt-vonnegut/Slaughterhouse%20Five%20-%20Kurt%20Vonnegut.epub",
            b"PK\x03\x04 an epub".to_vec(),
        )
        .with(
            "https://archive.org/download/slaughterhouse-five-kurt-vonnegut/Slaughterhouse%20Five%20-%20Kurt%20Vonnegut.pdf",
            b"<html>rate limited</html>".to_vec(),
        );
    if let Some(u) = search_url {
        f = f.with(u, SEARCH);
    }
    Client::with_fetcher(f)
}

#[test]
fn search_parses_hits_and_keeps_only_those_with_an_epub() {
    let probe = Client::with_fetcher(FakeFetcher::new());
    let url = probe
        .search_url("slaughterhouse five", &SearchOptions::default())
        .unwrap();
    let client = client_with(Some(url.as_str()));
    let page = client
        .search("slaughterhouse five", &SearchOptions::default())
        .unwrap();
    assert_eq!(page.total, 7);
    assert_eq!(page.hits.len(), 5, "every captured hit carries an EPUB");
    let danish = &page.hits[0];
    assert_eq!(danish.identifier, "the-danish-slaughterhouse-by-decada");
    assert_eq!(danish.creator.as_deref(), Some("Decada"));
    assert_eq!(danish.year.as_deref(), Some("2012"));
    assert_eq!(
        danish.label(),
        "The Danish Slaughterhouse By Decada — Decada (2012)"
    );
    // A list of creators is joined.
    let zine = page
        .hits
        .iter()
        .find(|h| h.identifier.starts_with("fight-for"))
        .unwrap();
    assert_eq!(zine.creator.as_deref(), Some("Vandal, N.W.O.C Crew"));
    assert!(!page.has_more(), "7 hits fit on a 25-row page");
}

#[test]
fn an_item_knows_its_best_epub_and_real_pdf() {
    let client = client_with(None);
    let item = client.item("slaughterhouse-five-kurt-vonnegut").unwrap();
    assert_eq!(
        item.label(),
        "Slaughterhouse Five - Kurt Vonnegut — Kurt Vonnegut"
    );
    assert_eq!(
        item.license_url.as_deref(),
        Some("https://creativecommons.org/publicdomain/mark/1.0/")
    );
    let epub = item.best_epub().unwrap();
    assert_eq!(epub.name, "Slaughterhouse Five - Kurt Vonnegut.epub");
    assert_eq!(epub.size, 1153574);
    let pdf = item.best_pdf().unwrap();
    assert_eq!(pdf.format, "Text PDF");
    // OCR derivatives are not books.
    assert!(item.files.iter().filter(|f| f.is_pdf()).count() == 1);
}

#[test]
fn download_lands_as_author_title_and_refuses_an_error_page() {
    let client = client_with(None);
    let item = client.item("slaughterhouse-five-kurt-vonnegut").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let epub = item.best_epub().unwrap().clone();
    let (path, existed) = client.download(&item, &epub, dir.path()).unwrap();
    assert!(!existed);
    assert_eq!(
        path,
        dir.path()
            .join("Kurt Vonnegut/Slaughterhouse Five - Kurt Vonnegut.epub")
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"PK\x03\x04 an epub");
    assert!(!dir
        .path()
        .join("Kurt Vonnegut/Slaughterhouse Five - Kurt Vonnegut.epub.part")
        .exists());
    // Again: nothing fetched, reported as already there.
    let (_, existed) = client.download(&item, &epub, dir.path()).unwrap();
    assert!(existed);
    // The PDF URL answers with HTML (archive.org does this when throttling):
    // not saved.
    let pdf = item.best_pdf().unwrap().clone();
    let err = client.download(&item, &pdf, dir.path()).unwrap_err();
    assert!(err.to_string().contains("not a PDF"), "{err}");
    assert!(!dir
        .path()
        .join("Kurt Vonnegut/Slaughterhouse Five - Kurt Vonnegut.pdf")
        .exists());
}

#[test]
fn a_missing_item_is_an_error_with_the_servers_words() {
    let client = Client::with_fetcher(FakeFetcher::new().with(
        "https://archive.org/metadata/nope",
        br#"{"error":"Couldn't locate item 'nope'"}"#.to_vec(),
    ));
    let err = client.item("nope").unwrap_err();
    assert!(err.to_string().contains("Couldn't locate item"), "{err}");
}

#[test]
fn another_base_url_is_honoured() {
    let client = Client::with_fetcher(FakeFetcher::new())
        .with_base(Url::parse("http://localhost:8080").unwrap());
    assert_eq!(
        client.item_url("x").unwrap().as_str(),
        "http://localhost:8080/metadata/x"
    );
}
