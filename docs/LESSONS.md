# Lessons from bobo

gideon is the standalone successor to
[bobo](https://github.com/evanspn/bobo-koreader), a KOReader manga plugin.
This document synthesizes bobo's full git history — **685 commits, of which
209 (30%) are `fix:` commits** — into the mistakes gideon must not repeat,
and records how gideon's design addresses each one.

The churn data alone tells the story. bobo's most-modified files:

| File | Changes | What it is |
| --- | --- | --- |
| `Backend.lua` | 74 | Lua↔Rust IPC glue |
| `ChapterListing.lua` | 72 | Lua UI |
| `server/src/main.rs` | 65 | HTTP server plumbing |
| `LibraryView.lua` | 57 | Lua UI |
| `database.rs` | 47 | SQLite + migrations |
| `MangaSearchResults.lua` | 43 | Lua UI |
| `chapter_downloader.rs` | 37 | Downloads |

Three of the top six are Lua UI files, and the #1 file is the IPC boundary.

## 1. The Lua frontend was the biggest source of bugs

bobo's UI lived in untyped Lua inside KOReader's widget system. The history
is full of UI lifecycle fixes: `ReaderUI` close events handled wrong,
`on_return_callback` not firing, navigation/callback bugs in reader widgets,
context menus appearing on empty views, "inconsistent last read text
display", dialogs that couldn't be cancelled, text overflow in widgets. None
of it was unit-testable — every fix was verified by hand on a device.

**gideon:** there is no Lua. The entire stack — rendering, layout, reader
session, refresh policy — is Rust, drawn through a `Display` trait with an
in-memory backend, so every screen renders headless in CI down to the pixel.
Overflow is a *testable property* here: when the widget/text layer lands
(ROADMAP v1), every text-drawing API takes explicit bounds, clips or
ellipsizes, and gets pixel-level regression tests asserting nothing draws
outside its box. The refresh policy (bobo couldn't test it; we already do)
is covered by `reader.rs` tests today.

## 2. The two-process architecture (frontend ↔ HTTP server) was constant pain

bobo ran a Rust HTTP server that the Lua frontend talked to over unix domain
sockets. History: loopback interface setup on Kobo, `poll` before reads,
server startup failure dialogs, switching job creation to unix sockets,
timeouts on chapter refreshes, `uds_http_request` as an entire crate.
`Backend.lua` (the glue) was the single most-churned file in the repo.

**gideon:** one process, one language. Function calls instead of IPC. The
entire category of bugs is structurally impossible.

## 3. Downloads corrupted state when interrupted

bobo's fixes: "only write chapter file if it was successfully downloaded",
"store temporary file in downloads folder", "error if the image download
request didn't succeed", "sanitize chapter filenames", "use hash of
ChapterId fields for chapter filename", "stream download into update ZIP".

**gideon:** `pages_to_cbz` writes to a `.cbz.part` temp file and renames
into place — an interrupted download can never leave a half-written CBZ
where the library will find it. Any failed page download fails the whole
chapter. Both behaviors are unit-tested offline (`FakeFetcher`) and
integration-tested against live GitHub (post-merge `online` tests).

## 4. Offline behavior was bolted on late

bobo added "check for internet connection before performing requests" (by
pinging a hard-coded Cloudflare IP, which then had to be fixed), offline
mode dialogs, and "don't skip chapters when going to next chapter without
connection" — all retrofits.

**gideon:** offline-first by construction. The core reading path
(CBZ → render → display) never touches the network; `gideon-sources` is a
separate crate and everything network-facing goes through the `Fetcher`
trait, so offline behavior is the default code path, not a special case.

## 5. Database migrations bit repeatedly

bobo: "migration previously applied but has been modified", "move
manga_state table to separate migration", "create database.db file before
attempting to read settings", "remove chapters from the database that are
missing on source". 47 changes to `database.rs`.

**gideon:** no database until the feature set demands one. Progress is a
versionable JSON file written atomically (temp + rename). If/when SQLite
arrives, migrations are append-only and migration application gets tests
from day one.

## 6. Settings parsing was too strict, then patched to be lenient

bobo: "make settings.json parser more lenient", separate fixes for
`segment`, `link`, and `select` setting definitions, "allow select/segment
setting definitions to not have titles".

**gideon:** serde with `#[serde(default)]` everywhere from the start; both
source-list JSON shapes accepted; parse failures carry the URL and reason.
Parsing has dedicated unit tests including malformed input.

## 7. CI, packaging and self-update churned constantly

35 changes to `build.yml`, repeated `deploy-pages` fixes, OTA bugs ("do not
update on major bumps", "stream download into update ZIP"), remote-install
path concatenation bugs, a whole nix/devbox/devenv setup that itself needed
fixing.

**gideon:** plain GitHub Actions + cargo, no nix layer. The installer is
a tested artifact: `ci/installer_test.sh` asserts data preservation, backup
rotation and uninstall behavior on every PR, and the post-merge workflow
smoke-tests the assembled bundle before uploading it.

## 8. Error messages needed context, added piecemeal

A long tail of bobo commits just added context to errors: chapter
downloader messages, source loading flow, server startup, "show dialog with
error logs", persisted chapter size in error messages.

**gideon:** `thiserror` enums carry context (path, URL, page name, index)
in every variant from the first commit; CLI surfaces them with `anyhow`
chains.

## 9. Coupling to the host app's lifecycle caused breakage

bobo had to fix reliance on KOReader's `init`/`onExit`/`onRestart` events,
plugin path crashes, and menu entries leaking into KOReader's file manager.

**gideon:** standalone binary. The only host integration is one NickelMenu
launcher line, installed as our own file and never editing anyone else's
config.
