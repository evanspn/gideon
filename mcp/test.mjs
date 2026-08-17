// Unit tests for the MCP server's core (mcp/lib.js) with an injected fetch —
// no network, no real credentials. Run: node --test mcp/test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { GideonClient, parseKey, displayTitle } from "./lib.js";

function tmpFiles({ withAuth = true } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "gideon-mcp-"));
  const authFile = path.join(dir, "auth.json");
  if (withAuth) fs.writeFileSync(authFile, JSON.stringify({ email: "e@x.com", password: "pw" }));
  return { authFile, sessionFile: path.join(dir, "session.json") };
}

const json = (body, status = 200) =>
  Promise.resolve(new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }));

test("parseKey/displayTitle mirror the web dashboard", () => {
  assert.deepEqual(parseKey("One Piece/vol3.cbz"), { series: "One Piece", chapter: "vol3" });
  assert.equal(displayTitle("Frieren_ Beyond Journey's End"), "Frieren Beyond Journey's End");
});

test("signs in with creds, persists the session, and sends a title", async () => {
  const calls = [];
  const fetchImpl = (url, opts = {}) => {
    calls.push({ url, opts });
    if (url.includes("/auth/v1/token?grant_type=password")) {
      return json({ access_token: "at1", refresh_token: "rt1" });
    }
    if (url.includes("/rest/v1/send_queue")) {
      assert.equal(opts.headers.Authorization, "Bearer at1");
      assert.deepEqual(JSON.parse(opts.body), { title: "Berserk", cover_url: "https://c/x.jpg" });
      return json([{ id: "id1", title: "Berserk", created_at: "2026-08-17T00:00:00Z" }], 201);
    }
    throw new Error(`unexpected ${url}`);
  };
  const files = tmpFiles();
  const c = new GideonClient({ fetchImpl, ...files });
  const out = await c.sendToKobo("Berserk", "https://c/x.jpg");
  assert.equal(out.id, "id1");
  assert.deepEqual(JSON.parse(fs.readFileSync(files.sessionFile, "utf8")), {
    access_token: "at1",
    refresh_token: "rt1",
  });
});

test("a 401 triggers refresh and a retry", async () => {
  let restCalls = 0;
  const fetchImpl = (url, opts = {}) => {
    if (url.includes("grant_type=refresh_token")) return json({ access_token: "at2", refresh_token: "rt2" });
    if (url.includes("/rest/v1/send_queue?status=eq.pending")) {
      restCalls++;
      if (restCalls === 1) return json({}, 401);
      assert.equal(opts.headers.Authorization, "Bearer at2");
      return json([{ id: "s1", title: "Vagabond", created_at: "x" }]);
    }
    throw new Error(`unexpected ${url}`);
  };
  const files = tmpFiles();
  fs.writeFileSync(files.sessionFile, JSON.stringify({ access_token: "stale", refresh_token: "rt1" }));
  const c = new GideonClient({ fetchImpl, ...files });
  const sends = await c.pendingSends();
  assert.equal(sends[0].title, "Vagabond");
  assert.equal(restCalls, 2);
});

test("without an auth file, the error explains the one-time setup", async () => {
  const files = tmpFiles({ withAuth: false });
  const c = new GideonClient({ fetchImpl: () => json({}, 401), ...files });
  await assert.rejects(() => c.pendingSends(), /Create .*auth\.json.*email/);
});

test("search prefers the MAL proxy and maps its shape", async () => {
  const fetchImpl = (url) => {
    if (url.includes("gideon-sync.vercel.app/api/mal")) {
      return json({
        data: [
          { node: { title: "Berserk", mean: 9.3, media_type: "manga", nsfw: "white", genres: [{ name: "Dark Fantasy" }], main_picture: { large: "https://c/b.jpg" }, synopsis: "Guts.", start_date: "1989-08-25" } },
          { node: { title: "A Novel", media_type: "light_novel" } },
        ],
      });
    }
    throw new Error(`unexpected ${url}`);
  };
  const c = new GideonClient({ fetchImpl, ...tmpFiles() });
  const rows = await c.searchManga("berserk");
  assert.equal(rows.length, 1);
  assert.deepEqual(rows[0], {
    title: "Berserk", score: 9.3, year: "1989", genres: ["Dark Fantasy"],
    cover_url: "https://c/b.jpg", synopsis: "Guts.", source: "myanimelist",
  });
});

test("search falls back to Jikan when the proxy is unconfigured", async () => {
  const fetchImpl = (url) => {
    if (url.includes("/api/mal")) return json({ error: "proxy-unconfigured" }, 503);
    if (url.includes("api.jikan.moe")) {
      return json({ data: [{ title: "Berserk", type: "Manga", score: 9.4, images: { jpg: { large_image_url: "https://c/j.jpg" } }, genres: [], published: { from: "1989-08-25T00:00:00+00:00" }, synopsis: "Guts." }] });
    }
    throw new Error(`unexpected ${url}`);
  };
  const c = new GideonClient({ fetchImpl, ...tmpFiles() });
  const rows = await c.searchManga("berserk");
  assert.equal(rows[0].source, "jikan");
  assert.equal(rows[0].year, "1989");
});

test("library groups chapters into one entry per series, newest first", async () => {
  const fetchImpl = (url) => {
    if (url.includes("reading_progress")) {
      return json([
        { chapter_key: "One Piece/vol3.cbz", current_page: 10, total_pages: 20, updated_at: "2026-08-16T10:00:00Z" },
        { chapter_key: "One Piece/vol1.cbz", current_page: 19, total_pages: 20, updated_at: "2026-08-10T10:00:00Z" },
        { chapter_key: "Berserk/ch1.cbz", current_page: 4, total_pages: 18, updated_at: "2026-08-15T10:00:00Z" },
      ]);
    }
    throw new Error(`unexpected ${url}`);
  };
  const files = tmpFiles();
  fs.writeFileSync(files.sessionFile, JSON.stringify({ access_token: "at", refresh_token: "rt" }));
  const c = new GideonClient({ fetchImpl, ...files });
  const lib = await c.library();
  assert.equal(lib.length, 2);
  assert.deepEqual(lib[0], {
    series: "One Piece", chapters_tracked: 2, current_chapter: "vol3",
    current_page: 11, total_pages: 20, last_read: "2026-08-16T10:00:00Z",
  });
});
