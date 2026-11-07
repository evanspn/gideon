-- Chunked file sends.
--
-- The storage bucket caps an object at 50 MB (the free plan's ceiling), and a
-- novel with its illustrations left at full resolution can be three times
-- that. Rather than tell the phone "too big", the dashboard uploads such a
-- file as several objects — "<path>.part0001", ".part0002", … — and queues ONE
-- row naming the set. The device downloads the parts in order, streaming each
-- to disk, stitches them back together, validates the whole, places it, and
-- deletes every part. file_size is the whole file; parts is how many pieces.
--
-- A single-part send keeps the plain object path, so rows and clients from
-- before this migration are unchanged (parts defaults to 1).

alter table public.send_queue
    add column if not exists parts integer not null default 1;

alter table public.send_queue drop constraint if exists send_queue_parts_check;
alter table public.send_queue
    add constraint send_queue_parts_check check (parts between 1 and 32);

-- Whole-file size: up to 512 MB (32 parts under the 50 MB object cap, with
-- headroom; also what a Kobo can sensibly take in one download).
alter table public.send_queue drop constraint if exists send_queue_file_size_check;
alter table public.send_queue
    add constraint send_queue_file_size_check check (
        file_size is null or (file_size > 0 and file_size <= 536870912)
    );
