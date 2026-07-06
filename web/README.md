# gideon web

A tiny static web dashboard for gideon's cross-platform reading-progress sync
(see `../docs/SYNC.md`). Create an account here (email + password), then see
where you left off on your Kobo — the same `reading_progress` rows the device
syncs, newest first. The device signs in with the same email + password.

It is **read-only**: the device is the writer, so the web can't rewind your
place. The Supabase anon key is embedded on purpose (public by design — RLS,
keyed to `auth.uid()`, is what scopes every row to its owner).

## Stack

Plain static files — `index.html`, `app.js` (ES module importing
`@supabase/supabase-js` from a CDN), `styles.css`. No build step.

## Deploy (Vercel)

```sh
# from this directory, with a Vercel token in $VERCEL_TOKEN
npx vercel deploy --prod --yes --scope <team> --token "$VERCEL_TOKEN"
```

## Auth

**Email + password** (Supabase Auth), so there's no email round-trip and it
works on the free tier with no custom SMTP. New signups are auto-confirmed
(`mailer_autoconfirm` on the project), so an account created here works
immediately on the device. The device (`gideon-sync`) signs in with the same
credentials.
