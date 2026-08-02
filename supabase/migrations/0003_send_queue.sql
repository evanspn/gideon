-- "Send to Kobo" queue: the web app drops a manga title here, and the device
-- picks it up on its next sync and shows it as a notification. Tapping it on
-- the device runs the on-device source search for the title so the reader can
-- pick the right match and add it to the library.
--
-- The website can't run the Aidoku WASM source search (that engine only lives
-- on the Kobo), so only a title (a search term) crosses the wire — never a
-- resolved source/manga id. The device does the actual searching.
--
-- One row per send. RLS scopes every row to its owner, like reading_progress.
-- user_id defaults to auth.uid() so the web can insert with just a title.

create table if not exists public.send_queue (
    id         uuid        not null default gen_random_uuid(),
    user_id    uuid        not null default auth.uid() references auth.users (id) on delete cascade,
    title      text        not null check (char_length(title) between 1 and 512),
    cover_url  text,
    -- 'pending' until the device has shown/opened it, then 'opened' so the
    -- notification badge clears and it isn't offered again.
    status     text        not null default 'pending' check (status in ('pending', 'opened')),
    created_at timestamptz not null default now(),
    primary key (id)
);

-- The device polls "my pending sends, newest first".
create index if not exists send_queue_pending_idx
    on public.send_queue (user_id, created_at desc)
    where status = 'pending';

alter table public.send_queue enable row level security;

drop policy if exists "send_queue: own select" on public.send_queue;
create policy "send_queue: own select" on public.send_queue
    for select using (auth.uid() = user_id);

-- Insert (web enqueues), update (device marks opened) and delete (web removes a
-- pending send) are all scoped to the owner. with_check pins the row to the
-- caller so a client can never enqueue into someone else's queue.
drop policy if exists "send_queue: own write" on public.send_queue;
create policy "send_queue: own write" on public.send_queue
    for all using (auth.uid() = user_id) with check (auth.uid() = user_id);

grant select, insert, update, delete on public.send_queue to authenticated;
