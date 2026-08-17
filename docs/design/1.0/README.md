# gideon 1.0 UI refresh — design mockups

Two directions for the on-device Kobo Libra Colour UI, drawn at the real panel
size (1264×1680). These are design source files, not shipping code.

| File | Artboard |
| --- | --- |
| `Main.dc.html` | Direction A · Shelf — Library |
| `SeriesShelf.dc.html` | Direction A · Shelf — Series detail |
| `Dossier.dc.html` | Direction B · Dossier — Today |
| `DossierLibrary.dc.html` | Direction B · Dossier — Library |
| `Palettes.dc.html` | Kaleido palette reference — swatches, panel approximation, what collapses |
| `Profiles.dc.html` | All five profiles side by side on real components |
| `canvas.json` | Canvas layout + the annotations explaining both directions |

## Geometry is lifted from the source, not invented

Every number in the mockups comes from the code, so a chosen direction can be
built against them directly:

- `row_h` **102px** and body text **45px** at 1264×1680 (`ui/layout.rs:106-121`)
- `pad` **19px** (`width / 64`)
- title/nav rules `#555555`, row separators `#DDDDDD`, card borders `#AAAAAA`
- progress fill `#222222` on a `#CCCCCC` track; shelf `gap 16 / title 44 / bar 8`
  (`gideon-render/src/shelf.rs`)
- DejaVu Sans, regular and bold only — there is no weight scale
- flat fills and 1px rules only (no gradients, shadows or radii), so partial
  refresh stays clean; every tap target ≥ 88px

## Color, and what it costs on other devices

The Libra Colour has a Kaleido filter, so both directions use color. Two
physical facts shape how:

1. **The color layer resolves at ~150ppi against 300ppi for black.** Color in
   thin strokes or body text reads soft and fringed, so color lives in
   **blocks** — chip fills, progress bars, heatmap cells, the nav indicator —
   and text stays black. The one exception is type large and bold enough to
   carry it (the 38px status value on the series screen).
2. **Kaleido subtracts saturation.** The palette is already pulled back from
   sRGB rather than relying on the panel to mute it: one family at shared
   lightness and chroma with hue varied — rust `#A85F38`, gold `#B08A2E`,
   green `#4C7A55`, blue `#3F6B8C`, plus pale tints `#DBE7DD` / `#DAE4EB`
   for chip fills.

**Color is never the only carrier.** Every colored element also differs in
value, position or label, so the same layout survives on a panel without
Kaleido. Each artboard has a `mono` tweak that renders exactly that: the
palette collapses to the existing grayscale constants (`#222222` progress on
`#CCCCCC`, `#555555` labels) and the status swatch separates by value
(`#000000` publishing, `#999999` finished) instead of hue.

So if gideon ever ships to a non-Kaleido device — Clara, older Libras — the
mono rendering is the build, not a degraded afterthought. Implementations
should read these as two token sets behind one layout, not two designs.

## The data gap these mockups assume

Both directions show data the device does not store today:

- `SeriesRef` (`gideon-core/src/series.rs`) holds only `source_id`,
  `source_name`, `manga_id`, `manga_title`, `cover_url`, `downloaded` — no
  genres, status, score or total chapter count.
- `parse_popular` in `gideon-app/src/mal.rs` reads only `title` and
  `cover_url`; the Jikan payload's mean score, status, genres and rank arrive
  in the same response and are discarded.
- `ReadingProgress` is per-chapter only (`current_page`, `total_pages`,
  `last_read_at`). Streaks, pages-read totals and per-day history are derived
  by `computeStats` in `web/app.js` from Supabase — web-side only.

So Direction A's metadata chips need only the fields `mal.rs` already throws
away; Direction B's stat band and heatmap need a stats store on-device or a
pull from Supabase. That is the cost difference between the two.

## Choosing a scheme

Every screen artboard carries a **Profile** tweak with five options, so any
screen can be flipped to any scheme:

| Profile | Character |
| --- | --- |
| `ink-rust` | warm neutral, one earth accent — the default |
| `indigo` | cool editorial, blue-led |
| `sumi` | near-monochrome, a single vermilion |
| `botanical` | four hues for genre coding |
| `mono` | the non-Kaleido build |

`Profiles.dc.html` shows all five side by side on the same components.
`Palettes.dc.html` is the swatch reference: each colour as authored sRGB beside
an approximation of the panel rendering, with the mono value twin underneath,
plus a strip of hues the filter cannot carry at all.

`mono` is a **target, not a fallback** — it is what ships if gideon reaches a
Clara or an older Libra. Nothing in any layout depends on hue alone; where a
scheme uses colour to separate two states, the mono profile separates them by
value instead (`#000000` publishing against `#999999` finished).

The panel approximation is a **model, not a measurement** — it pulls each
channel toward luminance and compresses the range the way a colour filter array
does. It is good enough to rank schemes and to catch two roles collapsing to the
same value; it is not a substitute for looking at the hardware. The `desat`
tweak on that artboard adjusts how hard the model pushes.

Two facts from this repo shape all four:

- The saturation "boost" is a hardware CFA gain flag
  (`HWTCON_FLAG_CFA_EINK_G2`, `crates/gideon-device/src/kobo.rs:62`), not a
  software transform — gideon applies no colour correction of its own.
- `vivid` is documented as banding on gradients
  (`crates/gideon-core/src/settings.rs:70-73`), which is an independent reason
  to stay on flat fills.

## Regenerating the canvas

The published canvas is assembled from these files by the `/design` skill; the
seeded output (`gideon-1.0-ui-refresh.html`, ~2 MB) is a build artifact and is
gitignored. Edit the `.dc.html` files and re-seed to update it.
