-- Two things the phone can now hand the Kobo besides a manga title:
--
--   1. A FILE. The dashboard uploads an EPUB/PDF/CBZ into a private storage
--      bucket and drops a send_queue row pointing at it; the device downloads
--      the object on its next sync, validates it, places it (EPUB/PDF beside
--      the manga library for Nickel, CBZ into the manga library for gideon),
--      marks the row opened and deletes the object. Title sends are unchanged:
--      `kind` defaults to 'title', so existing rows and old clients keep
--      working.
--
--   2. A BOOK CATALOGUE (OPDS). Typing a Calibre server's address on an e-ink
--      keyboard is nobody's idea of setup, so it is typed once on the phone and
--      reaches the device on its next sync. The device merges these rows with
--      its local list (Project Gutenberg by default); removing a row here
--      removes it there.
--
-- RLS scopes everything to its owner, like every other table here.

-- --- send_queue: file sends ---------------------------------------------------

alter table public.send_queue
    add column if not exists kind      text   not null default 'title',
    add column if not exists file_path text,
    add column if not exists file_size bigint,
    -- Why the device refused a file send ("named .cbz but the contents are an
    -- EPUB"), set with status 'failed' so the phone shows it rather than the
    -- row quietly disappearing.
    add column if not exists error     text;

-- 'failed' joins the statuses: a file the device could not place. The phone
-- lists failed sends with their reason until the sender dismisses them.
alter table public.send_queue drop constraint if exists send_queue_status_check;
alter table public.send_queue
    add constraint send_queue_status_check check (status in ('pending', 'opened', 'failed'));

alter table public.send_queue drop constraint if exists send_queue_kind_check;
alter table public.send_queue
    add constraint send_queue_kind_check check (kind in ('title', 'file'));

-- A file send must say which object; a title send must not pretend to.
alter table public.send_queue drop constraint if exists send_queue_file_path_check;
alter table public.send_queue
    add constraint send_queue_file_path_check check (
        (kind = 'file' and file_path is not null and char_length(file_path) between 1 and 1024)
        or (kind = 'title' and file_path is null)
    );

alter table public.send_queue drop constraint if exists send_queue_file_size_check;
alter table public.send_queue
    add constraint send_queue_file_size_check check (
        file_size is null or (file_size > 0 and file_size <= 52428800)
    );

-- --- storage: the uploads bucket --------------------------------------------
--
-- Private: nothing in it is ever served by URL, only fetched by the owner with
-- their token. 50 MB per object — the free-tier ceiling, and plenty for a
-- novel; a manga volume over it gets a clear error on the phone, not a silent
-- failure. MIME allow-list is belt-and-braces: browsers label .cbz and .epub
-- inconsistently, so the device validates the bytes regardless.

insert into storage.buckets (id, name, public, file_size_limit, allowed_mime_types)
values (
    'kobo-uploads',
    'kobo-uploads',
    false,
    52428800,
    array[
        'application/epub+zip',
        'application/pdf',
        'application/vnd.comicbook+zip',
        'application/x-cbz',
        'application/x-cbr',
        'application/zip',
        'application/octet-stream'
    ]
)
on conflict (id) do update
    set public = excluded.public,
        file_size_limit = excluded.file_size_limit,
        allowed_mime_types = excluded.allowed_mime_types;

-- Objects live under "<user id>/<random>/<file name>". The first path segment
-- is the owner, and every policy checks it against the caller — so one user
-- can never read, overwrite or delete another's upload, whatever name they
-- pick.

drop policy if exists "kobo-uploads: own read" on storage.objects;
create policy "kobo-uploads: own read" on storage.objects
    for select to authenticated
    using (bucket_id = 'kobo-uploads' and (storage.foldername(name))[1] = auth.uid()::text);

drop policy if exists "kobo-uploads: own insert" on storage.objects;
create policy "kobo-uploads: own insert" on storage.objects
    for insert to authenticated
    with check (bucket_id = 'kobo-uploads' and (storage.foldername(name))[1] = auth.uid()::text);

drop policy if exists "kobo-uploads: own delete" on storage.objects;
create policy "kobo-uploads: own delete" on storage.objects
    for delete to authenticated
    using (bucket_id = 'kobo-uploads' and (storage.foldername(name))[1] = auth.uid()::text);

-- --- book_catalogs: OPDS catalogues typed on the phone ------------------------

create table if not exists public.book_catalogs (
    id         uuid        not null default gen_random_uuid(),
    user_id    uuid        not null default auth.uid() references auth.users (id) on delete cascade,
    name       text        not null default '' check (char_length(name) <= 120),
    url        text        not null check (
                               char_length(url) between 1 and 2048
                               and (url like 'http://%' or url like 'https://%')
                           ),
    created_at timestamptz not null default now(),
    primary key (id),
    -- The same catalogue twice is a mistake, not two catalogues.
    unique (user_id, url)
);

create index if not exists book_catalogs_user_idx
    on public.book_catalogs (user_id, created_at asc);

alter table public.book_catalogs enable row level security;

drop policy if exists "book_catalogs: own select" on public.book_catalogs;
create policy "book_catalogs: own select" on public.book_catalogs
    for select using (auth.uid() = user_id);

drop policy if exists "book_catalogs: own write" on public.book_catalogs;
create policy "book_catalogs: own write" on public.book_catalogs
    for all using (auth.uid() = user_id) with check (auth.uid() = user_id);

grant select, insert, update, delete on public.book_catalogs to authenticated;
