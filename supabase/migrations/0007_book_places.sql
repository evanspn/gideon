-- Book reading places: where gideon's book reader left off in each EPUB,
-- synced like manga progress so the place follows the reader between the
-- Kobo and the web. See docs/SYNC.md.
--
-- A place is a chapter, a paragraph (block) in it and a wrapped line in
-- that paragraph — never a page, since pages depend on the text size.
-- "Furthest place wins", like reading_progress: a late sync from a device
-- that is behind cannot rewind the reader. updated_at is only a pull cursor.

create table if not exists public.book_places (
    user_id    uuid        not null references auth.users (id) on delete cascade,
    -- The book's path under Books/, e.g. "Kurt Vonnegut/book-0001.epub".
    book_key   text        not null check (char_length(book_key) between 1 and 1024),
    chapter    integer     not null check (chapter >= 0),
    block      integer     not null check (block >= 0),
    line       integer     not null default 0 check (line >= 0),
    -- Chapters in the book when the place was saved, for "n% read".
    chapters   integer     not null default 0 check (chapters >= 0),
    -- What the book calls itself, so the dashboard can name it without
    -- the file.
    title      text        check (title is null or char_length(title) <= 512),
    author     text        check (author is null or char_length(author) <= 512),
    updated_at timestamptz not null default now(),
    primary key (user_id, book_key)
);

create index if not exists book_places_recent_idx
    on public.book_places (user_id, updated_at desc);

alter table public.book_places enable row level security;

drop policy if exists "book_places: own select" on public.book_places;
create policy "book_places: own select" on public.book_places
    for select using (auth.uid() = user_id);

drop policy if exists "book_places: own write" on public.book_places;
create policy "book_places: own write" on public.book_places
    for all using (auth.uid() = user_id) with check (auth.uid() = user_id);

-- Furthest-place-wins upsert. user_id comes from auth.uid(), never the
-- client. The place only moves forward: (chapter, block, line) compared as
-- a tuple. chapters, title and author track the latest report, since a
-- re-read of the file may know more.
create or replace function public.upsert_book_place(
    p_book_key text,
    p_chapter  integer,
    p_block    integer,
    p_line     integer,
    p_chapters integer,
    p_title    text default null,
    p_author   text default null
) returns public.book_places
language plpgsql
security definer
set search_path = public
as $$
declare
    result public.book_places;
begin
    if auth.uid() is null then
        raise exception 'not authenticated';
    end if;

    insert into public.book_places as bp
        (user_id, book_key, chapter, block, line, chapters, title, author, updated_at)
    values
        (auth.uid(), p_book_key, greatest(p_chapter, 0), greatest(p_block, 0),
         greatest(p_line, 0), greatest(p_chapters, 0), p_title, p_author, now())
    on conflict (user_id, book_key) do update
        set chapter  = case when (excluded.chapter, excluded.block, excluded.line)
                                 >= (bp.chapter, bp.block, bp.line)
                            then excluded.chapter else bp.chapter end,
            block    = case when (excluded.chapter, excluded.block, excluded.line)
                                 >= (bp.chapter, bp.block, bp.line)
                            then excluded.block else bp.block end,
            line     = case when (excluded.chapter, excluded.block, excluded.line)
                                 >= (bp.chapter, bp.block, bp.line)
                            then excluded.line else bp.line end,
            chapters = case when excluded.chapters > 0 then excluded.chapters else bp.chapters end,
            title    = coalesce(excluded.title, bp.title),
            author   = coalesce(excluded.author, bp.author),
            updated_at = now()
    returning * into result;

    return result;
end;
$$;

revoke all on function public.upsert_book_place(text, integer, integer, integer, integer, text, text) from public, anon;
grant execute on function public.upsert_book_place(text, integer, integer, integer, integer, text, text) to authenticated;
