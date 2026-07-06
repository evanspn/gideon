# Cross-platform reading-progress sync

gideon stores reading progress locally (`gideon-core`'s `ProgressStore`: a map
of `chapter_key → {current_page, total_pages}`, where `chapter_key` is the
library-relative path). This document describes syncing that across the web app
and the Kobo device so a reader's place follows them.

## Decisions

- **Backend:** Supabase (managed Postgres + Auth + auto REST/RPC API + RLS).
  Chosen over Neon because it gives a secure client API and magic-link auth
  with no server to host. (No Neon credential is configured anyway.)
- **Identity:** Supabase Auth **email magic-link** — no passwords to type on
  an e-ink keyboard.
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
- **Auth:** magic-link sign-in returns a session (access + refresh token)
  persisted on-device; expired tokens refresh silently; logged-out/expired ⇒
  read locally, queue syncs, resume after re-auth. Never hard-fail the app.

## Deploy / apply

1. Provision the Supabase project (region near the user) via the Supabase MCP
   (`create_project`), or the dashboard.
2. Apply `supabase/migrations/0001_reading_progress.sql` (`apply_migration`).
3. Enable Auth → Email (magic link); set the site URL / redirect for the web
   app.
4. Wire clients with the project URL + anon (publishable) key; never embed the
   service-role key in the device or web app — the device only ever uses the
   anon key plus the user's JWT, and RLS does the rest.

## Auth on the device

Sign-in uses **email one-time codes** (GoTrue `/auth/v1/otp` → `/auth/v1/verify`)
rather than a magic *link*: an e-ink device has no easy way to catch a
browser-redirect callback, but it can show a keyboard for an emailed 6-digit
code. The returned session (access + refresh token) is persisted per profile and
the short-lived access token is refreshed silently from the refresh token.

## Concurrency (device)

The background sync thread and the reader both write a profile's `progress.json`,
so writes are coordinated to avoid lost updates:

- All writes take a process-wide lock and use a per-write-unique temp file, so
  two writers can't corrupt each other (`ProgressStore::save` / `merge_save` /
  `overlay_save`).
- The **reader** writes with `overlay_save`: authoritative for the chapter it's
  reading (a deliberate page-back sticks), but it preserves any chapter the sync
  thread added.
- **Sync** writes with `merge_save`: furthest-page-wins, so it only ever raises a
  page and can never rewind the reader.
- Only one reconcile runs at a time (an in-flight guard in `gideon-app::sync`),
  so overlapping triggers don't double-spend the rotating refresh token.

## Account ↔ profile binding

An account's session and sync bookkeeping live **inside the profile's `.gideon`
directory** (`sync_session.json`, `sync_state.json`), next to that profile's
`progress.json`. So a profile is bound to exactly one cloud account: switching
profiles switches all of it, and signing a profile into a *different* account
resets the pull cursor (`gideon_sync::account::Account::verify`) so a previous
user's rows can never be pulled into this profile's store. Local `progress.json`
is never touched by sign-in/out — reading stays fully offline-first.

## Status

- ✅ Schema + RLS + furthest-page-wins RPC (this migration).
- ✅ `sync-architect` persona (`.claude/agents/`) to guide/review the system.
- ✅ Device sync client (`gideon-sync`): pure reconcile logic + the Supabase
  transport (PostgREST pull, `upsert_progress` RPC push), email-OTP auth with
  session refresh, and per-profile `Account` orchestration with the
  cross-account reset guard.
- ⬜ Provision the live Supabase project + apply the migration, then wire the
  project URL + anon key into the device (config) and add the account/sign-in UI.
- ⬜ Web app (reader + the same sync).
