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
- [x] FitWidth scrolling within a page (the render/blit plumbing exists;
      `reader_fit` setting, taps scroll with a 60px overlap before turning)
- [x] Page pre-decoding (decode page N+1 while reading page N)
- [x] Settings (settings.json: source lists, languages, storage limit,
      pre-download count, auto-update) with lenient parsing
- [x] Chapter storage with size-budget eviction + pre-download engine
      (gideon-sources::storage; sources WASM runtime in v2 plugs into it)
- [x] OTA updates from GitHub releases (`gideon update`, staged atomic
      binary swap with rollback; requires the repo to be public or a
      GIDEON_GITHUB_TOKEN)
- [x] Library cover view foundation (`gideon shelf`: cover grid + progress
      bars, headless pixel-tested so nothing draws outside its cell)
- [x] Install bundle: armv7 binary + data-preserving installer with backups
      and NickelMenu launcher entry (post-merge CI artifact)
- [x] Semantic-versioned, tag-driven GitHub releases (scripts/release.sh + release workflow)
- [x] Rotation support (landscape reading via `reader_rotation`
      0/90/180/270 with rotation-aware tap zones; two-page spreads later)
- [x] Accelerometer auto-rotation (the "auto" orientation follows how the
      device is physically held, via the Kobo gyro's `EV_MSC`/`MSC_RAW`
      gsensor codes; settle window so the panel doesn't thrash at a tilt
      boundary)

## v2 — online sources, end to end

- [x] Run Aidoku WASM sources (wasmi) so installed sources can search and
      list chapters, like bobo's backend does (gideon-aidoku, ported from
      bobo; handles classic and next-SDK sources, unknown host imports
      degrade gracefully)
- [ ] Search + browse UI for sources
- [ ] Chapter download queue with offline storage limits
- [x] Source install from the configured lists (`gideon source install`)

## Later

- [ ] CBR (rar) support
- [ ] OTA self-update from GitHub releases
- [ ] Reading stats
- [ ] Cloud sync of progress
