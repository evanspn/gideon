# gideon 1.0 UI refresh — design mockups

Two directions for the on-device Kobo Libra Colour UI, drawn at the real panel
size (1264×1680). These are design source files, not shipping code.

| File | Artboard |
| --- | --- |
| `Main.dc.html` | Direction A · Shelf — Library |
| `SeriesShelf.dc.html` | Direction A · Shelf — Series detail |
| `Dossier.dc.html` | Direction B · Dossier — Today |
| `DossierLibrary.dc.html` | Direction B · Dossier — Library |
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

The single accent (`#B4633A`, the web app's `--accent`) appears only as the nav
underline, the score star and the Continue label, so it degrades to a mid-gray
gracefully on Kaleido.

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

## Regenerating the canvas

The published canvas is assembled from these files by the `/design` skill; the
seeded output (`gideon-1.0-ui-refresh.html`, ~2 MB) is a build artifact and is
gitignored. Edit the `.dc.html` files and re-seed to update it.
