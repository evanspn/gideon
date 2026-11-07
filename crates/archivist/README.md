# archivist

Search and download public-domain books from the
[Internet Archive](https://archive.org), as a Rust library and a small CLI.
Built for [gideon](https://github.com/evanspn/gideon) on the Kobo, where it
backs the in-app "Internet Archive" catalogue; standalone on purpose (no
gideon dependency) so it lifts out into its own repository unchanged.

archive.org's OPDS feed (BookServer) is gone. Its JSON search and metadata
endpoints are not, and they serve direct EPUB downloads for everything that
isn't in the lending library — which is what this uses. Lending-library and
access-restricted items are filtered out: a reader cannot borrow.

```
archivist search "slaughterhouse five"            # title/author, most downloaded first
archivist search --anywhere --pdf dresden         # full text, PDFs allowed
archivist popular                                 # the most downloaded free EPUBs
archivist show slaughterhouse-five-kurt-vonnegut  # metadata + files
archivist get slaughterhouse-five-kurt-vonnegut --out /mnt/onboard/Books
```

`get` writes `Author/Title.epub` (the layout gideon's Books tab and Nickel
both read), through a temp file, and refuses a response that is not the
format it asked for (archive.org answers with an HTML page when throttling).

## Library

```rust
let client = archivist::Client::new();
let page = client.search("vonnegut", &archivist::SearchOptions::default())?;
let item = client.item(&page.hits[0].identifier)?;
let epub = item.best_epub().expect("search only returns items with one");
let (path, _existed) = client.download(&item, epub, std::path::Path::new("Books"))?;
```

All network access goes through the `Fetcher` trait; `FakeFetcher` serves
canned bodies for tests, and `tests/fixtures/` holds real captured responses.
