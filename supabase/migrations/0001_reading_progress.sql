-- Cross-platform reading-progress sync (gideon web + device).
--
-- One row per (user, chapter). The chapter_key is gideon's existing progress
-- key — the library-relative path, e.g. 'One Piece/vol1.cbz' — so the device
-- can sync without any new identifier scheme.
--
-- Conflict rule: FURTHEST-PAGE-WINS. Two devices reading the same chapter
-- offline must never move the reader *backward*; the higher current_page
-- always wins, regardless of which synced last. This is enforced server-side
-- (the upsert_progress RPC), so a stale client can't rewind a user's place.
--
-- Auth: Supabase Auth (email magic-link). Row-level security scopes every row
-- to its owner; the RPC derives user_id from auth.uid(), so a client can only
-- ever write its own progress even though it never sends a user_id.

create table if not exists public.reading_progress (
    user_id      uuid        not null references auth.users (id) on delete cascade,
    chapter_key  text        not null check (char_length(chapter_key) between 1 and 1024),
    current_page integer     not null check (current_page >= 0),
    total_pages  integer     not null check (total_pages  >= 0),
    updated_at   timestamptz not null default now(),
    primary key (user_id, chapter_key)
);

-- The library/home view ("continue reading") wants a user's most-recent
-- chapters first.
create index if not exists reading_progress_recent_idx
    on public.reading_progress (user_id, updated_at desc);

alter table public.reading_progress enable row level security;

-- A user may only see and change their own rows.
drop policy if exists "reading_progress: own select" on public.reading_progress;
create policy "reading_progress: own select" on public.reading_progress
    for select using (auth.uid() = user_id);

drop policy if exists "reading_progress: own write" on public.reading_progress;
create policy "reading_progress: own write" on public.reading_progress
    for all using (auth.uid() = user_id) with check (auth.uid() = user_id);

-- Furthest-page-wins upsert. Clients call this RPC with their JWT; user_id is
-- taken from auth.uid() (never trusted from the client). The row is only
-- advanced when the incoming page is >= the stored page, so a late sync from
-- a behind device cannot rewind the reader. total_pages always tracks the
-- latest report (a re-scan may change a chapter's length).
create or replace function public.upsert_progress(
    p_chapter_key  text,
    p_current_page integer,
    p_total_pages  integer
) returns public.reading_progress
language plpgsql
security definer
set search_path = public
as $$
declare
    result public.reading_progress;
begin
    if auth.uid() is null then
        raise exception 'not authenticated';
    end if;

    insert into public.reading_progress as rp
        (user_id, chapter_key, current_page, total_pages, updated_at)
    values
        (auth.uid(), p_chapter_key, greatest(p_current_page, 0), greatest(p_total_pages, 0), now())
    on conflict (user_id, chapter_key) do update
        set current_page = greatest(rp.current_page, excluded.current_page),
            total_pages  = excluded.total_pages,
            updated_at   = now()
    returning * into result;

    return result;
end;
$$;

-- The RPC runs as definer (to bypass RLS for the controlled upsert), so only
-- authenticated end users may call it — never anon.
revoke all on function public.upsert_progress(text, integer, integer) from public, anon;
grant execute on function public.upsert_progress(text, integer, integer) to authenticated;
