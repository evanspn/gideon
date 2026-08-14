# Error-handling audit

Sweep of the whole workspace (2026-08-14) against the bar: the device must
never crash, freeze, or need a reinstall because of an unhandled failure.
Line numbers refer to the tree *before* the fixes in this branch; "FIXED"
findings are addressed in the same commit as this report.

Overall the codebase is already unusually defensive: atomic temp+rename writes
everywhere (progress, settings, series index, CBZ downloads, OTA staging),
`show_error` in the UI loop, `lock_recover` for poisoned source mutexes,
touch-node hotplug/rescan, ELF-magic validation of OTA payloads, retrying
fetches classified transient/permanent. The findings below are the gaps.

## P0 — can crash or freeze the device in realistic use

### P0-1: no panic hook / crash screen for panics — FIXED
`crates/gideon-app/src/main.rs`

`run()`'s "never die on an error" contract only covers `Result` errors. A
panic (a bug, or one smuggled out of a dependency) unwound straight out of
`main`, blanked the panel and silently rebooted to Nickel with nothing on
screen and only default-hook stderr in the log.
**Fix:** `std::panic::set_hook` in `main()` logs `PANIC at file:line: msg` to
stderr (→ browse.log), covering background threads too; `cmd_browse` wraps
`UiApp::run()` in `catch_unwind(AssertUnwindSafe(..))` and routes the payload
through the existing `show_fatal_on_display` path, so a panic now shows the
same photographable on-panel error screen as a fatal `Err`. Test:
`panic_message_reads_str_and_string_payloads`.

### P0-2: no ureq timeouts — a stalled connection freezes the UI forever — FIXED
`crates/gideon-sources/src/fetch.rs:73` (UreqFetcher),
`crates/gideon-sync/src/supabase.rs:216,261` (AuthClient, SupabaseTransport)

All three agents were built with no connect or overall timeout. A captive
portal that blackholes traffic, or a half-open TCP connection after an AP
handoff, blocks the calling thread indefinitely. `UreqFetcher` is called on
the **UI thread** (source-list fetch, source install, OTA check via
`ui/gateway.rs`), so this is a hard UI freeze the user can only escape by
force-rebooting. The Supabase agents run on the sync thread, whose
`SYNC_IN_FLIGHT` flag would stay set forever — sync silently dead until
restart.
**Fix:** `timeout_connect(10s)` everywhere; overall `timeout(120s)` for the
fetcher (OTA bundle on slow Wi-Fi), `timeout(30s)` for sync.

### P0-3: WASM source can freeze the UI with `env.sleep` — FIXED
`crates/gideon-aidoku/src/source/wasm_imports/next/env.rs:41`

`sleep(seconds as u64)` with a guest-controlled `i32`: the UI thread
`block_on`s every source call (`ui/gateway.rs`), so `sleep(i32::MAX)` — or a
negative value, which casts to ~5.8e11 seconds — freezes the app permanently.
Any broken or malicious source triggers it.
**Fix:** `seconds.clamp(0, 30)`.

### P0-4: WASM source can OOM-kill the process via canvas allocation — FIXED
`crates/gideon-aidoku/src/source/wasm_imports/next/canvas.rs:84`

`new_context(width, height)` only rejected `<= 0`; a huge or non-finite
request allocated `w*h*4` bytes host-side (raqote `DrawTarget`). On a
256–512 MB Kobo that is an OOM kill — no unwind, no crash screen, launcher
reboot. Guest-controlled, so one bad source image pipeline does it.
**Fix:** reject non-finite dims, sides > 8192, or area > 32 M px.
Also `canvas.rs:495`: `PngEncoder::write_image(...).expect("PNG encode
failed")` panicked on a data/dimension mismatch — now a graceful `Err`.

### P0-5: host panic on malformed page URL from a source — FIXED
`crates/gideon-aidoku/src/source/model.rs:260`

`Page::from` did `url::Url::parse(url).unwrap()` on a string produced by
third-party WASM. Any source returning a relative/garbage page URL panicked
the host. (The `spawn_blocking` + `lock_recover` design contains this to an
error + poison-recover, but with the "never panic on guest data" bar, and
because `Page::from` is callable from non-blocking contexts, it's fixed at
the site.)
**Fix:** parse failure logs and leaves `image_url = None`; the reader's
per-page error path handles the rest. Tests:
`page_from_survives_an_invalid_url`, `page_from_keeps_a_valid_url`.

### P0-6: guest-controlled host panics in WASM imports — FIXED
- `crates/gideon-aidoku/src/source/wasm_imports/next/std.rs:170` —
  `write_bytes(...).expect("REASON")`: the guest picks the write offset; an
  out-of-bounds one panicked the host. Now logs and returns `-1` like the
  surrounding error paths.
- `crates/gideon-aidoku/src/source/wasm_imports/env.rs:55` —
  `get_memory(&mut caller).unwrap()` in `abort`: a module with no exported
  memory calling `abort` panicked the host. Now returns the `AbortError`
  trap without the message.

## P1 — degrades badly (silent data loss, stuck state) — 8 findings

1. **WASM guest can still spin-loop the UI.** No wasmi fuel/epoch limit; a
   source with an infinite loop hangs the `block_on` on the UI thread just
   like P0-3 (sleep is now clamped, `loop {}` is not). The
   `CancellationToken` passed to source calls is cooperative only. Fix:
   enable wasmi fuel metering (`Config::consume_fuel`) with a generous
   per-call budget, or move source calls off the UI thread with a watchdog.
   Left unfixed — an engine-config change with real tuning risk, not minimal.
2. **Corrupt `progress.json` silently loses all reading progress.**
   `crates/gideon-core/src/library.rs:145` errors on bad JSON; every UI
   caller does `unwrap_or_default()` (`ui/mod.rs:3297,4582,4613`) and the
   next `save()` overwrites the file with the empty store. FAT32 +
   power-loss makes a torn file possible despite rename (FAT rename isn't
   fully atomic). Recommend: on parse failure, rename the corrupt file to
   `progress.json.corrupt` and log, so the data is recoverable and the loss
   visible; sync restores most progress from the server anyway.
3. **`SeriesIndex::load` swallows corruption silently**
   (`crates/gideon-core/src/series.rs:40`, `.ok()` chain): downloaded-chapter
   bookkeeping resets, orphaning CBZs from eviction accounting. Same
   quarantine-and-log recommendation.
4. **CLI `gideon read`/`library`/`shelf` hard-fail on corrupt progress.json**
   (`crates/gideon-app/src/main.rs:338,434,874`, `ProgressStore::load(..)?`)
   — with a corrupt file the command errors on every start until the file is
   deleted by hand. Acceptable for a CLI, but the NickelMenu "read" entry
   uses it. Recommend the same lenient load as the UI once finding 2's
   quarantine exists.
5. **Predownloader worker death is invisible and sticky**
   (`crates/gideon-app/src/ui/mod.rs:5929`): if the worker thread ever dies
   (panic in `record_chapter_in_index`/eviction), `queued` keeps dedup
   entries and `tx.send` silently fails — look-ahead downloads stop for the
   session with no indication. Recommend: check `send()` failure and drop
   `self.predownloader` so the next kick respawns the worker.
6. **Wi-Fi restore guard can stick** (`crates/gideon-device/src/network.rs:144`):
   a panic between spawn and the `RECONNECTING.store(false)` leaves the flag
   true; wake-reconnect never runs again until restart. `run_blocking` can't
   realistically panic today; recommend a drop-guard like sync's
   `InFlightGuard` for symmetry.
7. **`sends.json` cache written non-atomically**
   (`crates/gideon-app/src/sync.rs:155`, plain `fs::write`): power loss can
   tear it. Reads are lenient (`.ok()` → empty), so impact is a lost badge
   until next sync. Recommend temp+rename like everything else.
8. **`serialize_variant!` unwraps on mixed-type guest arrays**
   (`crates/gideon-aidoku/src/source/wasm_imports/next/std.rs:58`,
   `v.$unwrap_fn().unwrap()`): a guest array mixing value types panics the
   host (contained by spawn_blocking + lock_recover, surfaces as a one-off
   error). Recommend converting to a `bail!` like the sibling
   `Array`/`Object` arms.

## P2 — theoretical — 7 findings

1. `crates/gideon-aidoku/src/source/wasm_store.rs:223-239,722` and
   `next/env.rs:47` — `OnceLock.get().expect("Please set …")` for
   WEBVIEW_*/REQUEST_TRY_FROM/SEND_PARTIAL_RESULT: all `#[cfg(not(feature =
   "all"))]`, and `all` is a default feature the app builds with — dead in
   device builds. Would matter if the trimmed build were ever shipped.
2. `crates/gideon-aidoku/src/source/wasm_imports/html.rs:345,…` —
   `elements.last().unwrap()` after non-empty checks; `:159` strip_prefix
   unwrap after a `starts_with` guard. Invariant-safe as written.
3. `crates/gideon-render/src/lib.rs:53` — `(width * height) as usize` u32
   multiply could wrap in release for absurd dimensions; all callers pass
   screen-bounded sizes.
4. `crates/gideon-render/src/panels.rs:127` — `page.width - 1` underflows on
   a 0-width page; the image crate never decodes 0-dim images.
5. `crates/gideon-device/src/kobo_input.rs:745` — `axes.expect(..)` is
   invariant-paired with `touch_idx`; `ui/mod.rs:943` stack-never-empty
   expects; `reader.rs:279,301,385,580` "matched above" expects — all
   locally provable.
6. `crates/gideon-app/src/mal.rs:54,146`, `sources/list.rs:69`,
   `update.rs:81`, `render/text.rs:20`, `manga.rs:760,764` — static-input
   expects (const URLs, vendored font, in-memory PNG encode). Fine.
7. `crates/gideon-aidoku/src/source/next_reader.rs:29-30` — fixed-size slice
   `try_into().unwrap()` on an 8-byte local buffer; cannot fail. Guest-lied
   lengths go through `memory.read`, which errors gracefully.

## Verified-solid areas (no findings)

- **Framebuffer**: every ioctl return is checked and surfaces as `Err` →
  `show_error`/fatal screen; blit bounds are clamped (`kobo.rs`).
- **Input**: per-fd EOF/POLLERR drops the node; touch loss triggers a 6×
  rescan before giving up; inotify hotplug re-absorbs BT remotes; suspend
  wake reopens nodes (`kobo_input.rs`).
- **Suspend/frontlight sysfs**: all writes best-effort with error reporting;
  charging skips suspend (`power.rs`, `light.rs`).
- **OTA**: staged to `.part`, ELF magic checked, rename swap, previous
  binary kept as `gideon.old`, launcher self-heal on every start.
- **Downloads**: CBZ written to `.cbz.part` + rename; missing pages fail the
  download; fetch retries then classifies as Offline.
- **WASM traps**: `call_cleanup!`'s `expect("wasm call failed")` panic is
  deliberately contained by `spawn_blocking` (JoinError → `?`) and
  `lock_recover` un-poisons the source mutex, per the comment at
  `source/mod.rs:96-109`.
