# gideon web

A tiny static web dashboard for gideon's cross-platform reading-progress sync
(see `../docs/SYNC.md`). Create an account here (email + password), then see
where you left off on your Kobo — the same `reading_progress` rows the device
syncs, newest first. The device signs in with the same email + password.

It is **read-only** for progress: the device is the writer, so the web can't
rewind your place. The Supabase anon key is embedded on purpose (public by
design — RLS, keyed to `auth.uid()`, is what scopes every row to its owner).

Two write paths exist, both through `send_queue`: the **Send to Kobo** box on
Stats, and the **Discover** tab — point it at a public AniList or MyAnimeList
(via the Jikan mirror) username and it recommends manga from your anime list
(the source manga of your top-rated anime, plus community picks seeded from
those), each with a one-tap Send to Kobo. Recommendations run entirely
client-side against public, no-key, CORS-open APIs; provider outages surface
as a retryable error state.

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
