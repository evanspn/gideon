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

## Status

- ✅ Schema + RLS + furthest-page-wins RPC (this migration).
- ✅ `sync-architect` persona (`.claude/agents/`) to guide/review the system.
- ⬜ Provision the live Supabase project + apply the migration (blocked on the
  Supabase MCP approval).
- ⬜ Device sync client (a `gideon-sync` module: auth/session, push/pull,
  reconcile into `ProgressStore`).
- ⬜ Web app (reader + the same sync).
