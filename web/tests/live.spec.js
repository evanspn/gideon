import { test, expect } from "@playwright/test";

// LIVE integration test of the "Send to Kobo" chain against the real Supabase
// project — the exact HTTP calls the web app and the device make, end to end:
//
//   web enqueue (POST send_queue)
//     → device pull  (GET  send_queue?status=eq.pending, the gideon-sync query)
//     → device opens (PATCH status=opened)
//     → web removes  (DELETE)
//
// Opt-in because it touches production: run with
//
//   GIDEON_LIVE=1 npm test -- tests/live.spec.js
//
// By default it signs up a throwaway auto-confirmed account (one auth user is
// left behind per run — they're inert, but you can point it at a dedicated
// test account instead with GIDEON_LIVE_EMAIL / GIDEON_LIVE_PASSWORD).
// Everything it inserts into send_queue is deleted before the test ends.

const LIVE = process.env.GIDEON_LIVE === "1";
const SUPABASE_URL = "https://sqlkceqkdtmejhdoycsr.supabase.co";
const ANON_KEY =
  "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InNxbGtjZXFrZHRtZWpoZG95Y3NyIiwicm9sZSI6ImFub24iLCJpYXQiOjE3ODMyOTE5MDAsImV4cCI6MjA5ODg2NzkwMH0.K8kXfcIihjw0Mz5qm1hW7nXHcymhN-yMLrV6CaLU1eo";

// LIVE integration tests of the MyAnimeList integration — the real Jikan
// endpoints the app calls, asserting the exact response shapes it consumes
// (title, images.jpg.*, score, relations, user lists). Opt-in with
// GIDEON_LIVE=1 like the Supabase chain below. Jikan is community infra:
// when these fail with a 504 "MyAnimeList may be down", that is exactly the
// outage state the UI is built to surface, not an app bug.
test.describe("live MyAnimeList (Jikan) integration", () => {
  // Jikan rate-limits per IP (~3 req/s) — run these in one worker, in order,
  // with a polite gap, or the suite rate-limits itself into 429s.
  test.describe.configure({ mode: "default" });
  test.skip(!LIVE, "opt-in: set GIDEON_LIVE=1 to run against the real Jikan API");

  const JIKAN = "https://api.jikan.moe/v4";
  const getData = async (path) => {
    await new Promise((r) => setTimeout(r, 700));
    const res = await fetch(`${JIKAN}/${path}`);
    const body = await res.json().catch(() => ({}));
    expect(res.ok, `${path} → HTTP ${res.status}: ${body.message || ""}`).toBeTruthy();
    return body.data;
  };

  test("top manga: the browse row's shape", async () => {
    test.setTimeout(30_000);
    const rows = await getData("top/manga?limit=5");
    expect(rows.length).toBeGreaterThan(0);
    const m = rows[0];
    expect(typeof m.title).toBe("string");
    expect(m.images?.jpg?.large_image_url || m.images?.jpg?.image_url).toBeTruthy();
    expect(typeof m.score).toBe("number");
  });

  test("search: the search box's shape", async () => {
    test.setTimeout(30_000);
    const rows = await getData("manga?q=berserk&sfw=true&limit=5&order_by=members&sort=desc");
    expect(rows.some((m) => /berserk/i.test(m.title))).toBeTruthy();
  });

  test("anime → source-manga relation: the recommendation seed", async () => {
    test.setTimeout(30_000);
    // Sousou no Frieren (anime 52991) must expose its manga adaptation.
    const full = await getData("anime/52991/full");
    const manga = (full.relations || [])
      .filter((r) => r.relation === "Adaptation")
      .flatMap((r) => r.entry || [])
      .find((e) => e.type === "manga");
    expect(manga?.mal_id).toBeTruthy();
    expect(manga?.name).toContain("Frieren");
  });

  test("a public user's completed animelist is readable", async () => {
    test.setTimeout(30_000);
    const user = process.env.GIDEON_LIVE_MAL_USER || "Xinil"; // MAL's founder; public
    const rows = await getData(`users/${user}/animelist?status=completed`);
    expect(Array.isArray(rows)).toBeTruthy();
    expect(rows.length).toBeGreaterThan(0);
    expect(rows[0].anime?.mal_id).toBeTruthy();
  });
});

test.describe("live send-to-Kobo chain", () => {
  test.skip(!LIVE, "opt-in: set GIDEON_LIVE=1 to run against the real backend");

  test("web enqueue → device pull → device opens → web delete", async () => {
    test.setTimeout(60_000);

    // -- sign in (or sign up a throwaway) -----------------------------------
    const email = process.env.GIDEON_LIVE_EMAIL;
    const password = process.env.GIDEON_LIVE_PASSWORD;
    const creds = email
      ? { path: "token?grant_type=password", body: { email, password } }
      : {
          path: "signup",
          body: {
            email: `gideon-live-test-${Date.now()}@example.com`,
            password: `live-test-${Date.now()}`,
          },
        };
    const authRes = await fetch(`${SUPABASE_URL}/auth/v1/${creds.path}`, {
      method: "POST",
      headers: { apikey: ANON_KEY, "Content-Type": "application/json" },
      body: JSON.stringify(creds.body),
    });
    expect(authRes.ok, `auth failed: ${authRes.status}`).toBeTruthy();
    const { access_token } = await authRes.json();
    expect(access_token).toBeTruthy();
    const authed = (extra = {}) => ({
      apikey: ANON_KEY,
      Authorization: `Bearer ${access_token}`,
      ...extra,
    });

    // -- 1. web enqueue (what app.js enqueueSend sends) ----------------------
    const title = `Live Test ${Date.now()} — safe to delete`;
    const postRes = await fetch(`${SUPABASE_URL}/rest/v1/send_queue`, {
      method: "POST",
      headers: authed({ "Content-Type": "application/json", Prefer: "return=representation" }),
      body: JSON.stringify({ title, cover_url: "https://example.com/cover.jpg" }),
    });
    expect(postRes.status, "enqueue must return 201").toBe(201);
    const [row] = await postRes.json();
    expect(row.id).toBeTruthy();
    expect(row.status).toBe("pending");

    try {
      // -- 2. device pull (gideon-sync's exact pending query) ----------------
      const pullRes = await fetch(
        `${SUPABASE_URL}/rest/v1/send_queue?status=eq.pending&select=id,title&order=created_at.desc`,
        { headers: authed() }
      );
      expect(pullRes.status).toBe(200);
      const pending = await pullRes.json();
      expect(pending.map((r) => r.id)).toContain(row.id);

      // -- 3. device marks it opened (badge clears) --------------------------
      const patchRes = await fetch(`${SUPABASE_URL}/rest/v1/send_queue?id=eq.${row.id}`, {
        method: "PATCH",
        headers: authed({ "Content-Type": "application/json" }),
        body: JSON.stringify({ status: "opened" }),
      });
      expect(patchRes.status).toBe(204);

      const afterRes = await fetch(
        `${SUPABASE_URL}/rest/v1/send_queue?status=eq.pending&select=id`,
        { headers: authed() }
      );
      const after = await afterRes.json();
      expect(after.map((r) => r.id)).not.toContain(row.id);
    } finally {
      // -- 4. web delete (cleanup — always runs) -----------------------------
      const delRes = await fetch(`${SUPABASE_URL}/rest/v1/send_queue?id=eq.${row.id}`, {
        method: "DELETE",
        headers: authed(),
      });
      expect(delRes.status).toBe(204);
    }
  });
});
