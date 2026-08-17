# gideon web

A tiny static web dashboard for gideon's cross-platform reading-progress sync
(see `../docs/SYNC.md`). Create an account here (email + password), then see
where you left off on your Kobo — the same `reading_progress` rows the device
syncs, newest first. The device signs in with the same email + password.

It is **read-only** for progress: the device is the writer, so the web can't
rewind your place. The Supabase anon key is embedded on purpose (public by
design — RLS, keyed to `auth.uid()`, is what scopes every row to its owner).

Two write paths exist, both through `send_queue`: the **Send to Kobo** box on
Stats, and the **Discover** tab — point it at a public **MyAnimeList**
username and it recommends manga from your anime list (the source manga
of your top-rated anime, plus community picks seeded from those), each with a
one-tap Send to Kobo. The tab also carries a manga search box and Trending /
Top-rated browse rows — every card shows its community ★ score — and library
titles get a rating chip (resolved via Jikan search, cached in localStorage
for a week). Outages surface as retryable error states, and missing ratings
just don't render.

MAL data arrives one of two ways, preferred in this order:

1. **Official MAL API** via the same-origin serverless proxy `api/mal.js` —
   first-party data, immune to mirror outages. One-time *deployment* setup:
   create a Client ID at myanimelist.net → Preferences → API, then
   `vercel env add MAL_CLIENT_ID production` and
   `vercel env add MAL_CLIENT_SECRET production` (neither reaches the
   browser). Users never do any of this.
2. **Jikan** (the community mirror, public/no-key/CORS-open) — the automatic
   fallback whenever the proxy isn't configured or can't reach MAL. Jikan has
   had multi-day outages, which is why the proxy exists.

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
(CSP included) ship via `vercel.json`.

Heads-up for iOS users: Safari evicts script-writable storage after ~7 days
without a visit, so a long absence may mean tapping Connect again — it's one
tap, by design.

Live at **https://gideon-sync.vercel.app**.

## Stack

Plain static files — `index.html`, `app.js`, `styles.css`. No build step and no
SDK: `app.js` talks to Supabase's Auth + REST endpoints with plain `fetch`, so
there's no CDN dependency at runtime.

## Deploy (Vercel)

```sh
# from this directory, with a Vercel token in $VERCEL_TOKEN
npx vercel deploy --prod --yes --scope <team> --token "$VERCEL_TOKEN"
```

`.vercelignore` keeps the test tooling (and `package.json`) out of the deploy,
so Vercel serves the static files directly with no build.

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
