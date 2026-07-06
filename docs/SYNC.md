# Cross-platform reading-progress sync

gideon stores reading progress locally (`gideon-core`'s `ProgressStore`: a map
of `chapter_key → {current_page, total_pages}`, where `chapter_key` is the
library-relative path). This document describes syncing that across the web app
and the Kobo device so a reader's place follows them.

## Decisions

- **Backend:** Supabase (managed Postgres + Auth + auto REST/RPC API + RLS).
  Chosen over Neon because it gives a secure client API and hosted auth with no
  server to host. (No Neon credential is configured anyway.)
- **Identity:** Supabase Auth **email + password**. The account is created once
  on the web (from a phone); the device just signs in with the same email +
  password. No email round-trip at sign-in, so it needs no custom SMTP and works
  on the free tier (signups are auto-confirmed).
- **Conflict rule:** **furthest-page-wins** (monotonic). Two devices reading
  the same chapter offline converge to the higher `current_page`; a stale
  device can never rewind another. Enforced server-side by the
  `upsert_progress` RPC, not by clients. `updated_at` orders the UI
  ("continue reading"), it does **not** decide conflicts (clock skew is not
  trusted).
- **Offline-first:** the local `ProgressStore` stays authoritative; sync is a
  background reconcile that catches up when online and never blocks reading or
  corrupts the local store on failure.
- **Privacy:** row-level security scopes every row to its owner; clients send
  a JWT, never a `user_id`; the RPC derives identity from `auth.uid()`; no
  anon access to user data.

## Schema

See `supabase/migrations/0001_reading_progress.sql`:

- `public.reading_progress (user_id, chapter_key, current_page, total_pages,
  updated_at)`, PK `(user_id, chapter_key)`, RLS owner-only.
- `public.upsert_progress(chapter_key, current_page, total_pages)` — a
  `security definer` RPC that upserts with `current_page = greatest(stored,
  incoming)` (furthest-page-wins), `user_id := auth.uid()`. `execute` granted
  only to `authenticated`.

## Sync protocol (client ↔ Supabase)

- **Push** (on chapter close / app background / short idle — debounced, never
  per page turn): for each locally-changed chapter, call
  `rpc('upsert_progress', { p_chapter_key, p_current_page, p_total_pages })`
  with the user's JWT.
- **Pull** (on app foreground / login): `select * from reading_progress where
  updated_at > <last_pull>`; merge each into the local store with the same
  furthest-page-wins rule (`local = max(local_page, remote_page)`), so pull and
  push are symmetric and idempotent.
- **Auth:** email + password sign-in (`grant_type=password`) returns a session
  (access + refresh token) persisted on-device; expired tokens refresh silently;
  logged-out/expired ⇒ read locally, queue syncs, resume after re-auth. Never
  hard-fail the app.

## Deploy / apply

1. Provision the Supabase project (region near the user) via the Supabase MCP
   (`create_project`), or the dashboard.
2. Apply `supabase/migrations/0001_reading_progress.sql` (`apply_migration`).
3. Enable Auth → Email (email + password); turn on auto-confirm
   (`mailer_autoconfirm`) so a web signup works immediately on the device.
4. Wire clients with the project URL + anon (publishable) key; never embed the
   service-role key in the device or web app — the device only ever uses the
   anon key plus the user's JWT, and RLS does the rest.

## Auth on the device

Sign-in uses **email + password** (GoTrue `token?grant_type=password`): the
account is created once on the web, and the device shows an email keyboard then
a password keyboard and signs in — no browser-redirect callback to catch and no
emailed code to wait for (which the free tier can't deliver without custom
SMTP). The returned session (access + refresh token) is persisted per profile
and the short-lived access token is refreshed silently from the refresh token.

## Concurrency (device)

The background sync thread and the reader both write a profile's `progress.json`,
so writes are coordinated to avoid lost updates:

- All writes take a process-wide lock and use a per-write-unique temp file, so
  two writers can't corrupt each other (`ProgressStore::save` / `merge_save` /
  `overlay_save`).
- Both the **reader** and **sync** write with `merge_save`: furthest-page-wins,
  so a write only ever *raises* a page and neither can rewind the other. Local
  reading never diverges below the furthest page the account has reached — a
  deliberate flip-back to re-read doesn't record a lower position (never-rewind
  is the cardinal rule). Mark-unread is the deliberate exception: it uses
  `ProgressStore::forget` (a lock-held remove) since a merge can't remove.
- Only one reconcile runs at a time (an in-flight guard in `gideon-app::sync`),
  so overlapping triggers don't double-spend the rotating refresh token.

## Account ↔ profile binding

An account's session and sync bookkeeping live **inside the profile's `.gideon`
directory** (`sync_session.json`, `sync_state.json`), next to that profile's
`progress.json`. So a profile is bound to exactly one cloud account: switching
profiles switches all of it.

Signing a profile into a *different* account wipes this profile's local reading
and cursor (`gideon_sync::account::Account::verify`), so the prior user's rows
neither get pushed up into the new account nor linger in its library. The switch
is detected against a **durable owner marker** (`sync_owner`) rather than the
live session — it survives sign-out, so the common sign-out-then-sign-in-as-
someone-else flow is caught, not just a direct account swap. A *first* sign-in
(no prior owner) keeps the device's own offline reading and pushes it up.

## Status

- ✅ Schema + RLS + furthest-page-wins RPC (this migration).
- ✅ `sync-architect` persona (`.claude/agents/`) to guide/review the system.
- ✅ Device sync client (`gideon-sync`): pure reconcile logic + the Supabase
  transport (PostgREST pull, `upsert_progress` RPC push), email + password auth
  with session refresh, and per-profile `Account` orchestration with the
  cross-account reset guard.
- ✅ Live Supabase project provisioned + migration applied; project URL + anon
  key wired into the device, with the account/sign-in UI (email → password).
- ✅ Web dashboard (`web/`): sign in with the same email + password, view the
  synced `reading_progress`. Deployed to Vercel.
