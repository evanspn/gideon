# Roadmap

## v0 — core pipeline (this PR)

- [x] Workspace skeleton (`core`, `render`, `sources`, `device`, `app`)
- [x] CBZ loading: natural page order, junk filtering, ComicInfo.xml
- [x] E-ink render pipeline: fit modes, grayscale, 16-level dithering
- [x] Library scan + JSON reading progress with resume
- [x] Preinstalled GitHub source lists (Aidoku/bobo-compatible), source
      resolution, chapter→CBZ downloads
- [x] Kobo framebuffer backend (mxcfb ioctls, full/partial refresh policy)
- [x] CI: fmt, clippy, tests, CLI smoke test, armv7 cross-check

## v1 — read manga on the device

- [ ] Touch input via evdev (tap zones: next/prev/menu)
- [ ] On-device library browser UI (cover grid, progress badges) — pure
      Rust, drawn through the `Display` trait; every text API takes explicit
      bounds and clips/ellipsizes, with headless pixel tests asserting no
      widget ever draws outside its box (see docs/LESSONS.md §1 — bobo's Lua
      UI overflow bugs must not come back)
- [ ] FitWidth scrolling within a page (the render/blit plumbing exists)
- [ ] Page pre-decoding (decode page N+1 while reading page N)
- [x] Install bundle: armv7 binary + data-preserving installer with backups
      and NickelMenu launcher entry (post-merge CI artifact)
- [ ] GitHub release publishing for install bundles
- [ ] Rotation support (landscape reading, two-page spreads)

## v2 — online sources, end to end

- [ ] Run Aidoku WASM sources (wasmi) so installed sources can search and
      list chapters, like bobo's backend does
- [ ] Search + browse UI for sources
- [ ] Chapter download queue with offline storage limits
- [ ] In-app source install/update from the configured lists

## Later

- [ ] CBR (rar) support
- [ ] OTA self-update from GitHub releases
- [ ] Reading stats
- [ ] Cloud sync of progress
