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

// LIVE check of the deployed MAL official-API proxy (web/api/mal.js) on
// production. Passes in either supported state: configured (MAL_CLIENT_ID
// env var set on the Vercel project → real ranking data with the shape the
// app consumes) or not yet configured (clean 503 "proxy-unconfigured", which
// the app surfaces as a clear configuration error).
test.describe("live MAL proxy on production", () => {
  test.skip(!LIVE, "opt-in: set GIDEON_LIVE=1 to run against production");

  test("api/mal responds configured-or-unconfigured, never broken", async () => {
    test.setTimeout(30_000);
    const path = encodeURIComponent("manga/ranking?ranking_type=all&limit=3&fields=mean");
    const res = await fetch(`https://gideon-sync.vercel.app/api/mal?path=${path}`);
    const body = await res.json().catch(() => ({}));
    if (res.status === 503) {
      expect(body.error).toBe("proxy-unconfigured"); // add MAL_CLIENT_ID to go live
      return;
    }
    expect(res.status, JSON.stringify(body).slice(0, 200)).toBe(200);
    expect(body.data.length).toBeGreaterThan(0);
    expect(body.data[0].node.title).toBeTruthy();
    expect(typeof body.data[0].node.mean).toBe("number");
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
