-- Per-chapter "first read" timestamp, for library insights on the web
-- (first read day, time to completion).
--
-- `updated_at` moves on every sync, so it can only answer "when did I last
-- read this" — never "when did I start it". `started_at` is stamped once at
-- insert and NEVER touched again: the upsert_progress RPC's conflict branch
-- doesn't set it, so it is immutable by construction (no trigger needed).
--
-- Existing rows are backfilled to the migration time (the best information
-- available); their real start dates are unknowable after the fact.

alter table public.reading_progress
    add column if not exists started_at timestamptz not null default now();
