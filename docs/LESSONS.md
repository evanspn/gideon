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

## 10. Killing nickel early trips NickelMenu's failsafe (it "uninstalls itself")

NickelMenu has an anti-bootloop failsafe: while nickel's first ~20 seconds
tick down, `libnm.so` is parked at `libnm.so.failsafe` and only renamed
back once nickel survived the window. If nickel dies inside that window the
library stays parked, and on the next boot NickelMenu is simply gone — from
the user's point of view it "uninstalled itself", taking the gideon menu
entry with it.

This became easy to hit the moment gideon's exit started restarting nickel
in place (fast) instead of rebooting (slow): leave gideon, nickel is back in
seconds, tap gideon again — and the launcher killed a nickel that was still
inside its failsafe window.

**gideon:** `gideon-launch.sh` never kills nickel while the failsafe is
armed (it waits, bounded at ~25 s, checking both the parked-library marker
and nickel's process age), and after gideon exits it restores a parked
`libnm.so` before nickel — or the reboot fallback — comes up, so even a
tripped failsafe self-heals instead of silently removing NickelMenu.

**Verified internals** — primary sources, so future fixes don't run on
folklore (all in [pgaskin/NickelHook](https://github.com/pgaskin/NickelHook)
`nh.c` and [pgaskin/NickelMenu](https://github.com/pgaskin/NickelMenu)
unless noted):

- `nh_init` is `__attribute__((constructor))` (`nh.c`, declaration block),
  so the failsafe arms when nickel's Qt plugin loader dlopens the library.
- Install path `/usr/local/Kobo/imageformats/libnm.so`: `NickelHook.mk`,
  `KOBOROOT += $(LIBRARY):/usr/local/Kobo/imageformats/...`.
- `nh_failsafe_create` parks the lib: `rename(orig, orig.failsafe)`.
- Disarm: `nh_failsafe_destroy(fs, failsafe_delay)` at the end of
  `nh_init` — reached from the success *and* the error label — spawns a
  detached thread that sleeps then renames back; a failed rename-back only
  logs (so our restoring first is harmless). NickelMenu sets
  `failsafe_delay = 3` (`nickelmenu.cc`).
- A trip means "the disarm thread never fired". Nothing else restores the
  library; explicit uninstall is a separate flag file
  (`/mnt/onboard/.adds/nm/uninstall`).
- The reference nickel restart (KOReader `platform/kobo/nickel.sh`)
  launches hindenburg + nickel + `udevadm trigger` only — it kills
  `sickel` on entry (`koreader.sh`) and never restarts it. gideon now
  matches; relaunching sickel ourselves was an unsourced deviation.
- **The reboot that actually strands the failsafe** (gideon issue #120):
  on the MediaTek Libra Colour family, exiting a reader app to Nickel
  with a Bluetooth device still connected can spontaneously reboot the
  device (koreader/koreader#12739 — clean exit logs, then a reboot;
  pgaskin/NickelMenu#220 reports the resulting NickelMenu loss). No
  in-script babysitter survives a reboot, so the launcher now soft-blocks
  the BT radio before restarting nickel, via the kernel's stable rfkill
  sysfs ABI (`Documentation/ABI/stable/sysfs-class-rfkill`); Nickel
  re-enables Bluetooth itself on next use.

That exposed a second hole, hit after the first guard shipped: the window
re-arms on OUR restart too. When gideon exits and relaunches nickel in
place, NickelMenu parks the library again — and the launcher used to exit
as soon as `pidof nickel` succeeded, i.e. *inside* that window. If nickel
then crashed (or `sickel`, the FW5 watchdog we also relaunch, culled it),
the library stayed parked with nobody left to restore it. The launcher now
babysits the restart: it only exits once `libnm.so.failsafe` is gone, and
if nickel dies first it restores the library and reboots (~30 s bound).
