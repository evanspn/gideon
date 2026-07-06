# gideon web

A tiny static web dashboard for gideon's cross-platform reading-progress sync
(see `../docs/SYNC.md`). Sign in with a Supabase magic link and see where you
left off on your Kobo — the same `reading_progress` rows the device syncs,
newest first.

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

After the first deploy, set the project's production domain as Supabase Auth's
**Site URL** and add it to the redirect allow-list (Auth → URL configuration),
so the magic link redirects back here.

## Auth note

The web uses the **magic link** (works on Supabase's free-tier default email).
The device uses the **6-digit code** from the same sign-in — which needs the
email template to include the code (`{{ .Token }}`), available on a paid plan or
with custom SMTP configured.
