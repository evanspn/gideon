# gideon web

A tiny static web dashboard for gideon's cross-platform reading-progress sync
(see `../docs/SYNC.md`). Create an account here (email + password), then see
where you left off on your Kobo — the same `reading_progress` rows the device
syncs, newest first. The device signs in with the same email + password.

It is **read-only** for progress: the device is the writer, so the web can't
rewind your place. The Supabase anon key is embedded on purpose (public by
design — RLS, keyed to `auth.uid()`, is what scopes every row to its owner).

The landing page is **Today**, ported from the device's Today screen: the
chapter you're mid-way through, then a **month calendar** of what you read.
A series read three evenings running is ONE bar three days wide — the
heatmap on Stats answers "how much, over months"; this answers "what was I
reading, and for how long". Monday-first columns, lanes so two series on one
day stack instead of overlapping, and a run that wraps a week keeps its fill
but drops the accent edge so it reads as a continuation, not a new book
(`crates/gideon-render/src/calendar.rs` is the original). Bars open where
that series was left off, ‹ › page through months, and Today comes home.

Two write paths exist, both through `send_queue`: the **Send to Kobo** box on
Stats, and the **Discover** tab.

Discover is **one** left-to-right library rail, and a row of preference pills
decides what fills it: **For you** (recommendations from a connected
MyAnimeList — the source manga of your top-rated anime, plus community picks
seeded from those and from what you have read), **Trending**, **Top rated**,
and a pill per genre. Every card carries its community ★ score and a one-tap
Send to Kobo. Genre pills filter one cached 200-title top-manga fetch —
MAL's API has no genre query, so a request per pill tap would be the only
alternative. The tab also carries a manga search box, whose results replace
the rail until cleared, and library titles get a rating chip (resolved via a
MAL search, cached in localStorage for a week). Outages surface as retryable
error states, and missing ratings just don't render.

Tapping a card's cover or title (or **Title details** in a library book's
action sheet, which resolves the id by title) opens the **digest**: a
full-screen view of everything MyAnimeList knows about that manga — cover,
English/Japanese titles, authors, run and status, length, genres, the
description, and the score/rank/popularity/members tiles — across three tabs
(Overview · Details · Community) filled by a **single** `manga/{id}` fetch,
cached by id. The Community tab also carries the community's own
recommendations, each opening its own digest; Back unwinds one hop at a time
and hands the dashboard back with its pill, rail and "Sent ✓" states intact.
MAL's public API serves no reviews or comments (Jikan did, and that mirror is
deliberately out of this stack), so the tab says so rather than leaving a gap.

All MAL data is first-party, through the same-origin serverless proxy
`api/mal.js` (browsers can't call MAL directly — no CORS). One-time
*deployment* setup: create a Client ID at myanimelist.net → Preferences →
API, then `vercel env add MAL_CLIENT_ID production` and
`vercel env add MAL_CLIENT_SECRET production` (neither reaches the browser).
Users never do any of this. There is no third-party mirror in the stack —
the Jikan fallback was removed once the official path proved out (the mirror
had multi-day outages while MAL itself was up).

Recommendations are seeded from BOTH the anime list (source manga of
top-rated shows) and the manga the user has actually read ("Because you read
X") — so a manga-only reader gets real picks with an empty anime list.

## Per-user MyAnimeList accounts (multi-user)

Any signed-in user can tap **Connect MyAnimeList** on the Discover tab: a
standard S256-PKCE OAuth dance against the shared app registration, with the
code exchanged by `api/mal-oauth.js` (the only holder of the client secret)
and tokens stored only in that user's browser, keyed to their gideon account.
Connected users get automatic recommendations (private lists included) and a
**Sync Kobo reading to MAL** button that writes their reading history to
their MAL manga list — exact-title matches only, never rewinding MAL-side
progress, idempotent so re-running resumes. The `/api/mal` proxy forwards the
user's token on an explicit method+path allowlist; the single write
(`PATCH manga/{id}/my_list_status`) is field-allowlisted. Security headers
(CSP included) ship via the **root** `vercel.json` — the only one Vercel
reads. A `web/vercel.json` is silently ignored on a root deploy, which is why
those headers, like the API functions, were never actually live.

Heads-up for iOS users: Safari evicts script-writable storage after ~7 days
without a visit, so a long absence may mean tapping Connect again — it's one
tap, by design.

Live at **https://gideon-sync.vercel.app**.

## Stack

Plain static files — `index.html`, `app.js`, `styles.css`. No build step and no
SDK: `app.js` talks to Supabase's Auth + REST endpoints with plain `fetch`, so
there's no CDN dependency at runtime.

## Deploy (Vercel)

**Deploys run from the repository root, not from this directory.** The layout
Vercel expects:

* `vercel.json` (`outputDirectory: "web"`) — serves this directory's static
  files, no build step;
* `api/` **at the repo root** — the serverless functions (`api/mal.js`,
  `api/mal-oauth.js`). Vercel only turns a root-level `api/` directory into
  functions; when these lived in `web/api/` they were never deployed at all,
  and every MyAnimeList call in the browser got a 404 ("connection isn't
  configured", "MyAnimeList error (404)"). Keep them at the root.

```sh
# from the REPOSITORY ROOT, with a Vercel token in $VERCEL_TOKEN
npx vercel deploy --prod --yes --scope <team> --token "$VERCEL_TOKEN"
```

The root `.vercelignore` keeps the device app and the test tooling (including
`web/package.json`, so Vercel doesn't treat this as a Node build) out of the
upload. Pushing to `main` deploys automatically via
`.github/workflows/deploy-web.yml`, which verifies afterwards that both the
static files AND the API functions actually answer on the live site.

## Tests

Playwright regression tests (functionality + UI snapshots) live in `tests/`.
They mock Supabase at the HTTP boundary, so they run fully offline against a
local static server — no real backend.

```sh
npm install            # installs @playwright/test
npx playwright install chromium   # dev machines only; CI images have it preinstalled
npm test               # run the suite
npm run test:update    # re-baseline the UI snapshots after an intentional design change
```

`tests/live.spec.js` is an opt-in integration test that runs the real
send-to-Kobo chain (web enqueue → device pull → mark opened → delete) against
the production Supabase project with a throwaway account:

```sh
GIDEON_LIVE=1 npx playwright test tests/live.spec.js
```

## Auth

**Email + password** (Supabase Auth), so there's no email round-trip and it
works on the free tier with no custom SMTP. New signups are auto-confirmed
(`mailer_autoconfirm` on the project), so an account created here works
immediately on the device. The device (`gideon-sync`) signs in with the same
credentials.
