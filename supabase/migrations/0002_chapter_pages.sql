-- Page-URL index so the web app can read a chapter on-demand without any
-- content storage: the device (which runs the source) resolves a chapter's
-- page image URLs and publishes the list here; the web loads them live as
-- <img>s and discards them when the reader closes. Only URLs are synced, never
-- the images.
--
-- One row per (user, chapter_key) — the same chapter_key as reading_progress.
-- RLS scopes every row to its owner, exactly like reading_progress.

create table if not exists public.chapter_pages (
    user_id     uuid        not null references auth.users (id) on delete cascade,
    chapter_key text        not null check (char_length(chapter_key) between 1 and 1024),
    page_urls   text[]      not null,
    updated_at  timestamptz not null default now(),
    primary key (user_id, chapter_key)
);

alter table public.chapter_pages enable row level security;

drop policy if exists "chapter_pages: own select" on public.chapter_pages;
create policy "chapter_pages: own select" on public.chapter_pages
    for select using (auth.uid() = user_id);

drop policy if exists "chapter_pages: own write" on public.chapter_pages;
create policy "chapter_pages: own write" on public.chapter_pages
    for all using (auth.uid() = user_id) with check (auth.uid() = user_id);

-- Publish (upsert) a chapter's page URLs. user_id comes from the JWT, never the
-- client. The whole list is replaced on each call (a re-resolve may change it).
create or replace function public.set_chapter_pages(
    p_chapter_key text,
    p_page_urls   text[]
) returns void
language plpgsql
security definer
set search_path = public
as $$
begin
    if auth.uid() is null then
        raise exception 'not authenticated';
    end if;
    insert into public.chapter_pages as cp
        (user_id, chapter_key, page_urls, updated_at)
    values
        (auth.uid(), p_chapter_key, p_page_urls, now())
    on conflict (user_id, chapter_key) do update
        set page_urls  = excluded.page_urls,
            updated_at = now();
end;
$$;

revoke all on function public.set_chapter_pages(text, text[]) from public, anon;
grant execute on function public.set_chapter_pages(text, text[]) to authenticated;
