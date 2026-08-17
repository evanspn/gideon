import { test, expect } from "@playwright/test";

// Discover tab: manga recommendations from a MyAnimeList account (official
// API via the site's serverless proxy), plus search and trending/top browse — all with
// one-tap Send to Kobo. MAL and Supabase are mocked at the HTTP boundary,
// so these run fully offline and deterministically even while the real
// services are having outages. tests/live.spec.js covers the real MAL API.

const SESSION = {
  access_token: "test-access-token",
  refresh_token: "test-refresh-token",
  expires_in: 3600,
  user: { email: "reader@example.com" },
};

// The gideon library already contains One Piece — the engine must not
// recommend what's already on the shelf.
const ROWS = [
  {
    chapter_key: "One Piece/vol3.cbz",
    current_page: 10,
    total_pages: 20,
    updated_at: new Date(Date.now() - 3600e3).toISOString(),
  },
];

// --- MAL proxy fixtures -------------------------------------------------------

const MAL = {
  animelist: {
    data: [
      { node: { id: 51, title: "Sousou no Frieren" }, list_status: { score: 10 } },
      { node: { id: 52, title: "One Piece" }, list_status: { score: 8 } },
    ],
  },
  mangalist: { data: [{ node: { id: 900, title: "Vagabond" }, list_status: { score: 9, num_chapters_read: 104 } }] },
  // Vagabond's own page — the read-history recommendation seed.
  manga900: {
    mean: 9.2,
    main_picture: { large: "https://img.test/vag.jpg" },
    recommendations: [{ node: { id: 301, title: "Real", main_picture: { large: "https://img.test/real.jpg" } } }],
  },
  // Frieren's anime carries the manga relation; One Piece's is empty (MAL's
  // related_manga is often empty), forcing the search-by-title fallback.
  anime51: {
    related_manga: [
      { node: { id: 101, title: "Frieren: Beyond Journey's End", main_picture: { large: "https://img.test/frieren.jpg" } }, relation_type: "adaptation" },
    ],
  },
  anime52: { related_manga: [] },
  searchOnePiece: { data: [{ node: { id: 102, title: "One Piece", main_picture: { large: "https://img.test/op.jpg" }, mean: 9.0 } }] },
  manga101: {
    mean: 8.6,
    main_picture: { large: "https://img.test/frieren.jpg" },
    recommendations: [
      { node: { id: 201, title: "Yokohama Kaidashi Kikou", main_picture: { large: "https://img.test/ykk.jpg" } } },
      { node: { id: 900, title: "Vagabond", main_picture: { large: "https://img.test/vag.jpg" } } },
    ],
  },
  manga102: { mean: 9.0, main_picture: { large: "https://img.test/op.jpg" }, recommendations: [] },
  ranking: {
    data: [
      { node: { id: 501, title: "Chainsaw Man", media_type: "manga", mean: 8.6, main_picture: { large: "https://img.test/csm.jpg" }, genres: [{ name: "Action" }, { name: "Horror" }] } },
      { node: { id: 502, title: "A Light Novel", media_type: "light_novel", mean: 7.0 } },
      { node: { id: 503, title: "Vagabond", media_type: "manga", mean: 9.2, main_picture: { large: "https://img.test/vag2.jpg" }, genres: [{ name: "Drama" }] } },
    ],
  },
  searchBerserk: { data: [{ node: { id: 601, title: "Berserk", media_type: "manga", mean: 9.3, main_picture: { large: "https://img.test/berserk.jpg" }, genres: [{ name: "Dark Fantasy" }] } }] },
};

function mockMalProxy(page, { animelistStatus, overrides = {} } = {}) {
  return page.route("**/api/mal**", (route) => {
    const url = new URL(route.request().url());
    const [p, qs = ""] = (url.searchParams.get("path") || "").split("?");
    const q = new URLSearchParams(qs);
    const json = (b, status = 200) =>
      route.fulfill({ status, contentType: "application/json", body: JSON.stringify(b) });
    for (const [key, body] of Object.entries(overrides)) {
      if (p.includes(key)) return json(body);
    }
    if (p.endsWith("/animelist")) {
      if (animelistStatus) return json({ message: "", error: "not_found" }, animelistStatus);
      return json(MAL.animelist);
    }
    if (p.endsWith("/mangalist")) return json(MAL.mangalist);
    if (p === "anime/51") return json(MAL.anime51);
    if (p === "anime/52") return json(MAL.anime52);
    if (p === "manga/101") return json(MAL.manga101);
    if (p === "manga/102") return json(MAL.manga102);
    if (p === "manga/900") return json(MAL.manga900);
    if (p === "manga/ranking") return json(MAL.ranking);
    if (p === "manga") {
      if (q.get("limit") === "1") {
        // Source-manga fallback + library-ratings lookups.
        const term = (q.get("q") || "").toLowerCase();
        return json(term.includes("one piece") ? MAL.searchOnePiece : { data: [] });
      }
      return json(MAL.searchBerserk);
    }
    return json({ data: [] });
  });
}


// --- shared setup -------------------------------------------------------------

// An in-memory send_queue that records enqueued bodies, so tests can assert
// what actually crossed the wire (title + cover_url).
function mockSends(page, posted) {
  let items = [];
  let n = 0;
  return page.route("**/rest/v1/send_queue**", (route) => {
    const req = route.request();
    if (req.method() === "POST") {
      const body = JSON.parse(req.postData() || "{}");
      posted?.push(body);
      const row = { id: `id-${++n}`, ...body, created_at: new Date().toISOString() };
      items = [row, ...items];
      return route.fulfill({ status: 201, contentType: "application/json", body: JSON.stringify([row]) });
    }
    if (req.method() === "DELETE") {
      const m = req.url().match(/id=eq\.([^&]+)/);
      if (m) items = items.filter((x) => x.id !== decodeURIComponent(m[1]));
      return route.fulfill({ status: 204, body: "" });
    }
    return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(items) });
  });
}

async function signInAnd(page, tab) {
  await page.route("**/auth/v1/**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(SESSION) })
  );
  await page.route("**/rest/v1/reading_progress**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(ROWS) })
  );
  await page.goto("/");
  await page.locator("input[type=email]").fill("reader@example.com");
  await page.locator("input[type=password]").fill("password123");
  await page.getByTestId("signin").click();
  if (tab) await page.getByTestId(tab).click();
}

test.beforeEach(async ({ page }) => {
  // No localStorage.clear() here: each test already gets an isolated browser
  // context, and an init-script clear re-runs on every navigation — which
  // wipes the session mid-OAuth-redirect.
  await page.route("**/rest/v1/chapter_pages**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: "[]" })
  );
  // Default MAL proxy: empty everything, so the background ratings lookup and
  // the auto-loaded browse row never touch the real network. Tests that need
  // data re-route with mockMalProxy/mockMalApi (later registrations win).
  await page.route(/\/api\/mal\?path=/, (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: '{"data":[]}' })
  );
});

// --- tests --------------------------------------------------------------------

test("the Discover tab shows the MyAnimeList connect form", async ({ page }) => {
  await mockSends(page);
  await signInAnd(page, "tab-discover");

  await expect(page.getByTestId("mal-connect")).toBeVisible();
  await expect(page.getByTestId("disc-user")).toHaveCount(0); // username flow is gone
});










// --- official-API proxy path -------------------------------------------------

test("the full recommend flow runs on MAL's official API", async ({ page }) => {
  const posted = [];
  await mockSends(page, posted);
  await mockMalProxy(page);
  await page.addInitScript(CONNECTED);
  await signInAnd(page, "tab-discover"); // connected → recs auto-run

  // Frieren via the related_manga edge (One Piece resolves via the
  // search-by-title fallback and is then excluded — it's in the library).
  const sources = page.getByTestId("rec-sources").getByTestId("rec-card");
  await expect(sources).toHaveCount(1, { timeout: 15000 });
  await expect(sources.first()).toContainText("Frieren");
  await expect(sources.first()).toContainText("You rated the anime 10/10");
  await expect(sources.first()).toContainText("★ 8.6");

  // Similar: Yokohama (loved by readers of the source) AND Real (because
  // they read Vagabond); Vagabond itself is dropped (already on their list).
  const similar = page.getByTestId("rec-similar").getByTestId("rec-card");
  await expect(similar).toHaveCount(2);
  await expect(similar.filter({ hasText: "Yokohama" })).toContainText("Loved by readers of");
  await expect(similar.filter({ hasText: "Real" })).toContainText("Because you read Vagabond");

  // A card sends title + cover into the Kobo queue.
  await sources.first().getByTestId("rec-send").click();
  await expect(sources.first().getByTestId("rec-send")).toHaveText(/Sent to Kobo/);
  expect(posted).toEqual([
    { title: "Frieren: Beyond Journey's End", cover_url: "https://img.test/frieren.jpg" },
  ]);
});

test("recommendations work from reading history alone (no anime list)", async ({ page }) => {
  await mockSends(page);
  await mockMalProxy(page, { overrides: { "/animelist": { data: [] } } });
  await page.addInitScript(CONNECTED);
  await signInAnd(page, "tab-discover"); // connected → recs auto-run

  // No anime → no "read the source" section, but the manga they've READ
  // (Vagabond) seeds community picks.
  const similar = page.getByTestId("rec-similar").getByTestId("rec-card");
  await expect(similar).toHaveCount(1, { timeout: 15000 });
  await expect(similar.first()).toContainText("Real");
  await expect(similar.first()).toContainText("Because you read Vagabond");
  await expect(page.getByTestId("rec-sources")).toHaveCount(0);
});

test("an unconfigured deployment surfaces a clear browse error", async ({ page }) => {
  await mockSends(page);
  await page.route(/\/api\/mal\?path=/, (route) =>
    route.fulfill({ status: 503, contentType: "application/json", body: '{"error":"proxy-unconfigured"}' })
  );
  await signInAnd(page, "tab-discover");

  await expect(page.getByTestId("browse-error")).toContainText("isn't configured");
});

test("a browse outage shows an inline error with a working Retry", async ({ page }) => {
  await mockSends(page);
  let rankingCalls = 0;
  await page.route(/\/api\/mal\?path=/, (route) => {
    const [p] = (new URL(route.request().url()).searchParams.get("path") || "").split("?");
    const json = (b, s = 200) =>
      route.fulfill({ status: s, contentType: "application/json", body: JSON.stringify(b) });
    if (p !== "manga/ranking") return json({ data: [] });
    rankingCalls++;
    if (rankingCalls === 1) return json({ error: "MyAnimeList didn't answer" }, 502);
    return json(MAL.ranking);
  });
  await signInAnd(page, "tab-discover");

  await expect(page.getByTestId("browse-error")).toContainText("didn't answer");
  await page.getByTestId("browse-retry").click();
  await expect(page.getByTestId("browse-results").getByTestId("rec-card")).toHaveCount(2);
});

test("the browse row arriving does not wipe a half-typed username", async ({ page }) => {
  await mockSends(page);
  await page.route(/\/api\/mal\?path=/, async (route) => {
    const [p] = (new URL(route.request().url()).searchParams.get("path") || "").split("?");
    const json = (b) =>
      route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(b) });
    if (p === "manga/ranking") {
      await new Promise((r) => setTimeout(r, 800));
      return json(MAL.ranking);
    }
    return json({ data: [] });
  });
  await signInAnd(page, "tab-discover");

  await page.getByTestId("search-input").fill("halftyped");
  await expect(page.getByTestId("browse-results")).toBeVisible();
  await expect(page.getByTestId("search-input")).toHaveValue("halftyped");
});

test("browse and search run on official rankings/search with ratings", async ({ page }) => {
  await mockSends(page);
  await mockMalProxy(page);
  await signInAnd(page, "tab-discover");

  // Ranking-backed browse: light novel filtered by media_type, ★ from mean.
  const cards = page.getByTestId("browse-results").getByTestId("rec-card");
  await expect(cards).toHaveCount(2);
  await expect(cards.nth(0)).toContainText("Chainsaw Man");
  await expect(cards.nth(0)).toContainText("★ 8.6");

  await page.getByTestId("search-input").fill("berserk");
  await page.getByTestId("search-btn").click();
  const results = page.getByTestId("search-results").getByTestId("rec-card");
  await expect(results).toHaveCount(1);
  await expect(results.first()).toContainText("★ 9.3");

});



// --- "Connect MyAnimeList" (per-user OAuth) ----------------------------------

function mockOauth(page, { configStatus = 200, breakState = false } = {}) {
  const exchanges = [];
  page.route("**/api/mal-oauth**", (route) => {
    const url = new URL(route.request().url());
    const action = url.searchParams.get("action");
    const json = (b, status = 200) =>
      route.fulfill({ status, contentType: "application/json", body: JSON.stringify(b) });
    if (action === "config") {
      if (configStatus !== 200) return json({ error: "proxy-unconfigured" }, configStatus);
      // redirect_uri points back at the test server so the dance completes.
      return json({ client_id: "test-cid", redirect_uri: "http://127.0.0.1:3210/" });
    }
    if (action === "token") {
      exchanges.push(JSON.parse(route.request().postData() || "{}"));
      return json({ access_token: "mal-at", refresh_token: "mal-rt", expires_in: 3600 });
    }
    return json({ error: "unknown" }, 400);
  });
  // MAL's authorize page: immediately bounce back with a code, echoing (or
  // corrupting) the state — the shape of a user tapping Allow.
  page.route(/myanimelist\.net\/v1\/oauth2\/authorize/, (route) => {
    const u = new URL(route.request().url());
    const st = breakState ? "WRONG" : u.searchParams.get("state");
    return route.fulfill({
      status: 302,
      headers: { Location: `http://127.0.0.1:3210/?code=CODE123&state=${st}` },
    });
  });
  return exchanges;
}

test("Connect MyAnimeList completes the OAuth dance and lands connected", async ({ page }) => {
  await mockSends(page);
  const exchanges = mockOauth(page);
  await page.route("**/api/mal?path=users%2F%40me**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: '{"id":1,"name":"TestUser"}' })
  );
  await signInAnd(page, "tab-discover");

  await page.getByTestId("mal-connect").click();
  // Back from MAL: the app auto-completes and lands on Discover, connected.
  await expect(page.getByTestId("mal-connected")).toBeVisible({ timeout: 10000 });
  await expect(page.getByTestId("mal-connected")).toContainText("TestUser");
  await expect(page.getByTestId("mal-toast")).toContainText("connected");

  // The exchange carried a real S256-grade verifier, and tokens are scoped
  // to the signed-in gideon account.
  expect(exchanges).toHaveLength(1);
  expect(exchanges[0].code).toBe("CODE123");
  expect(exchanges[0].verifier.length).toBeGreaterThanOrEqual(43);
  const stored = await page.evaluate(() =>
    JSON.parse(localStorage.getItem("gideon.mal.reader@example.com") || "null")
  );
  expect(stored.access_token).toBe("mal-at");
  expect(stored.username).toBe("TestUser");
  // No OAuth residue in the address bar.
  expect(new URL(page.url()).search).toBe("");
});

test("the authorize URL uses MAL's only supported PKCE method (plain)", async ({ page }) => {
  await mockSends(page);
  let authorizeUrl = null;
  await page.route("**/api/mal-oauth**", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ client_id: "cid", redirect_uri: "http://127.0.0.1:3210/" }),
    })
  );
  // Bounce back to our own origin (without a code, so the pending PKCE
  // record survives) — otherwise localStorage would be read on MAL's origin.
  await page.route(/myanimelist\.net\/v1\/oauth2\/authorize/, (route) => {
    authorizeUrl = new URL(route.request().url());
    return route.fulfill({ status: 302, headers: { Location: "http://127.0.0.1:3210/" } });
  });
  await signInAnd(page, "tab-discover");
  await page.getByTestId("mal-connect").click();
  await expect.poll(() => authorizeUrl !== null).toBeTruthy();

  // MAL supports ONLY code_challenge_method=plain; S256 makes every token
  // exchange fail with invalid_grant. The challenge must equal the verifier.
  expect(authorizeUrl.searchParams.get("code_challenge_method")).toBe("plain");
  const challenge = authorizeUrl.searchParams.get("code_challenge");
  expect(challenge.length).toBeGreaterThanOrEqual(43);
  const stored = await page.evaluate(() => JSON.parse(localStorage.getItem("gideon.mal.pkce") || "{}"));
  expect(challenge).toBe(stored.verifier);
});

test("a failed exchange explains itself without OAuth jargon", async ({ page }) => {
  await mockSends(page);
  await page.route("**/api/mal-oauth**", (route) => {
    const action = new URL(route.request().url()).searchParams.get("action");
    if (action === "config") {
      return route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ client_id: "cid", redirect_uri: "http://127.0.0.1:3210/" }),
      });
    }
    return route.fulfill({ status: 400, contentType: "application/json", body: '{"error":"invalid_grant"}' });
  });
  await page.route(/myanimelist\.net\/v1\/oauth2\/authorize/, (route) => {
    const u = new URL(route.request().url());
    return route.fulfill({
      status: 302,
      headers: { Location: `http://127.0.0.1:3210/?code=C&state=${u.searchParams.get("state")}` },
    });
  });
  await signInAnd(page, "tab-discover");
  await page.getByTestId("mal-connect").click();

  const err = page.getByTestId("mal-error");
  await expect(err).toContainText("didn't go through");
  await expect(err).not.toContainText("invalid_grant");
  await expect(page.getByTestId("mal-connect")).toBeVisible();
});

test("a corrupted OAuth state is discarded quietly — no token exchange", async ({ page }) => {
  await mockSends(page);
  const exchanges = mockOauth(page, { breakState: true });
  await signInAnd(page, "tab-discover");

  await page.getByTestId("mal-connect").click();
  await expect(page.getByTestId("mal-error")).toContainText("didn't finish");
  await expect(page.getByTestId("mal-connect")).toBeVisible(); // Connect card is back
  expect(exchanges).toHaveLength(0);
  expect(
    await page.evaluate(() => localStorage.getItem("gideon.mal.reader@example.com"))
  ).toBeNull();
});

test("an unconfigured deployment says so instead of redirecting", async ({ page }) => {
  await mockSends(page);
  mockOauth(page, { configStatus: 503 });
  await signInAnd(page, "tab-discover");

  await page.getByTestId("mal-connect").click();
  await expect(page.getByTestId("mal-error")).toContainText("isn't configured");
});

test("a connected account runs its recommendations automatically", async ({ page }) => {
  await mockSends(page);
  mockMalApi(page); // connected reads go through @me on the proxy
  await page.addInitScript(() => {
    localStorage.setItem(
      "gideon.mal.reader@example.com",
      JSON.stringify({
        access_token: "mal-at",
        refresh_token: "mal-rt",
        expires_at: Math.floor(Date.now() / 1000) + 3600,
        username: "evan_mal",
      })
    );
  });
  await signInAnd(page, "tab-discover");

  // No typing, no button: the linked account's picks just arrive.
  await expect(page.getByTestId("mal-connected")).toContainText("evan_mal");
  await expect(page.getByTestId("disc-user")).toHaveCount(0); // manual form hidden
  const sources = page.getByTestId("rec-sources").getByTestId("rec-card");
  await expect(sources).toHaveCount(1, { timeout: 15000 });
  await expect(sources.first()).toContainText("Frieren");

  // Disconnect restores the manual path.
  await page.getByTestId("mal-disconnect").click();
  await expect(page.getByTestId("mal-connect")).toBeVisible(); // gateway is back
});

// --- connected-account data (user token) and Kobo→MAL sync -------------------

// One dispatcher for /api/mal covering both client-id catalog calls and
// user-token personal calls; records PATCH writes and every personal path hit.
function mockMalApi(page, { patches = [], personal = [] } = {}) {
  page.route(/\/api\/mal\?path=/, (route) => {
    const req = route.request();
    const url = new URL(req.url());
    const [p, qs = ""] = (url.searchParams.get("path") || "").split("?");
    const q = new URLSearchParams(qs);
    if (req.headers()["x-mal-user-token"]) personal.push(`${req.method()} ${p}`);
    const json = (b, s = 200) =>
      route.fulfill({ status: s, contentType: "application/json", body: JSON.stringify(b) });
    if (req.method() === "PATCH") {
      patches.push({ path: p, body: JSON.parse(req.postData() || "{}") });
      return json({ status: "reading" });
    }
    if (p === "users/@me") return json({ id: 1, name: "evan_mal" });
    if (p === "users/@me/animelist")
      return json({ data: [{ node: { id: 51, title: "Sousou no Frieren" }, list_status: { score: 10 } }] });
    if (p === "users/@me/mangalist")
      return json({
        data: [{ node: { id: 13, title: "One Piece" }, list_status: { status: "reading", num_chapters_read: 5 } }],
      });
    if (p === "manga") {
      const term = (q.get("q") || "").toLowerCase();
      if (term.includes("berserk"))
        return json({ data: [{ node: { id: 2, title: "Berserk", media_type: "manga", num_chapters: 0 } }] });
      if (term.includes("one piece"))
        return json({ data: [{ node: { id: 13, title: "One Piece", media_type: "manga", num_chapters: 0, mean: 9.0 } }] });
      if (term.includes("sousou") || term.includes("frieren"))
        return json({ data: [{ node: { id: 101, title: "Sousou no Frieren", media_type: "manga", num_chapters: 0, main_picture: { large: "https://img.test/frieren.jpg" } } }] });
      return json({ data: [{ node: { id: 999, title: "Something Else", media_type: "manga" } }] });
    }
    return json({ data: [] });
  });
  return { patches, personal };
}

const CONNECTED = () =>
  localStorage.setItem(
    "gideon.mal.reader@example.com",
    JSON.stringify({
      access_token: "mal-at",
      refresh_token: "mal-rt",
      expires_at: Math.floor(Date.now() / 1000) + 3600,
      username: "evan_mal",
    })
  );

async function signInWithRows(page, rows) {
  await page.route("**/auth/v1/**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(SESSION) })
  );
  await page.route("**/rest/v1/reading_progress**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(rows) })
  );
  await page.goto("/");
  await page.locator("input[type=email]").fill("reader@example.com");
  await page.locator("input[type=password]").fill("password123");
  await page.getByTestId("signin").click();
  await page.getByTestId("tab-discover").click();
}

test("Sync Kobo→MAL: exact matches written furthest-wins, the rest reported", async ({ page }) => {
  await mockSends(page);
  const { patches } = mockMalApi(page);
  await page.addInitScript(CONNECTED);
  await signInWithRows(page, [
    // Finished 1 chapter of Berserk — not on MAL yet → written.
    { chapter_key: "Berserk/ch1.cbz", current_page: 19, total_pages: 20, updated_at: new Date().toISOString() },
    // Finished 1 chapter of One Piece — MAL already at 5 → kept, never rewound.
    { chapter_key: "One Piece/vol1.cbz", current_page: 19, total_pages: 20, updated_at: new Date().toISOString() },
    // No confident MAL match → skipped, never written.
    { chapter_key: "Weird Unknown Manga/c1.cbz", current_page: 9, total_pages: 10, updated_at: new Date().toISOString() },
  ]);

  await page.getByTestId("mal-sync").click();
  await expect(page.getByTestId("mal-sync-done")).toBeVisible({ timeout: 20000 });
  await expect(page.getByTestId("mal-sync-done")).toContainText("1 update");

  // Only the safe write happened, with only the allowlisted fields.
  expect(patches).toEqual([
    { path: "manga/2/my_list_status", body: { status: "reading", num_chapters_read: 1 } },
  ]);

  const rows = page.getByTestId("mal-sync-row");
  await expect(rows.filter({ hasText: "kept" })).toContainText("already at 5");
  await expect(rows.filter({ hasText: "skipped" })).toContainText("no confident match");
  await expect(rows.filter({ hasText: "updated" })).toContainText("Berserk");
});

test("connected recommendations read the private @me list, never the public path", async ({ page }) => {
  await mockSends(page);
  const { personal } = mockMalApi(page);
  const publicListHits = [];
  await page.route(/\/api\/mal\?path=users%2Fevan_mal/, (route) => {
    publicListHits.push(route.request().url());
    return route.fulfill({ status: 200, contentType: "application/json", body: '{"data":[]}' });
  });
  await page.addInitScript(CONNECTED);
  await signInWithRows(page, ROWS);

  // Auto-run recommendations for the connected account arrive via @me.
  await expect(page.getByTestId("rec-sources").getByTestId("rec-card").first()).toBeVisible({
    timeout: 15000,
  });
  expect(personal).toContain("GET users/@me/animelist");
  expect(publicListHits).toEqual([]);
});

test("a connected account's failed recommendations offer retry, not a username form", async ({ page }) => {
  await mockSends(page);
  // @me animelist errors hard (500) → the recommend flow fails for a
  // connected user; they must get Try again, never the type-a-username card.
  await page.route(/\/api\/mal\?path=/, (route) => {
    const [p] = (new URL(route.request().url()).searchParams.get("path") || "").split("?");
    if (p === "users/@me/animelist") {
      return route.fulfill({ status: 500, contentType: "application/json", body: '{"error":"boom"}' });
    }
    return route.fulfill({ status: 200, contentType: "application/json", body: '{"data":[]}' });
  });
  await page.addInitScript(CONNECTED);
  await signInWithRows(page, ROWS);

  await expect(page.getByTestId("disc-error")).toBeVisible({ timeout: 15000 });
  await expect(page.getByTestId("disc-retry")).toBeVisible();
  await expect(page.getByTestId("disc-user")).toHaveCount(0);
});

test("an OAuth return with no local record explains itself instead of silence", async ({ page }) => {
  await mockSends(page);
  await page.route("**/rest/v1/reading_progress**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(ROWS) })
  );
  // Persisted gideon session, but NO pending PKCE record — the cross-browser
  // return case. The site must say what happened, not shrug.
  await page.addInitScript(() => {
    localStorage.setItem(
      "gideon.session",
      JSON.stringify({ access_token: "t", refresh_token: "r", email: "reader@example.com", expires_at: Math.floor(Date.now() / 1000) + 3600 })
    );
  });
  await page.goto("/?code=STRAY&state=whatever");

  await expect(page.getByTestId("mal-error")).toContainText("different browser");
  await expect(page.getByTestId("mal-connect")).toBeVisible();
  expect(new URL(page.url()).search).toBe(""); // code scrubbed from the bar
});

test("an unrelated ?code link (no state) is left completely alone", async ({ page }) => {
  await mockSends(page);
  await page.route("**/rest/v1/reading_progress**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(ROWS) })
  );
  await page.addInitScript(() => {
    localStorage.setItem(
      "gideon.session",
      JSON.stringify({ access_token: "t", refresh_token: "r", email: "reader@example.com", expires_at: Math.floor(Date.now() / 1000) + 3600 })
    );
  });
  await page.goto("/?code=NOT_OURS&utm_source=x");

  await expect(page.getByTestId("signout")).toBeVisible();
  await page.getByTestId("tab-discover").click();
  await expect(page.getByTestId("mal-error")).toHaveCount(0); // no false claim
  expect(new URL(page.url()).search).toBe("?code=NOT_OURS&utm_source=x"); // untouched
});

test("scrubbing an OAuth return preserves the recovery-link hash", async ({ page }) => {
  await mockSends(page);
  await page.route("**/rest/v1/reading_progress**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: "[]" })
  );
  // Stray code+state AND a Supabase recovery hash on one URL: the MAL scrub
  // must not destroy the hash before the session adopter reads it.
  await page.goto(
    "/?code=STRAY&state=whatever#access_token=rec-at&refresh_token=rec-rt&type=recovery&expires_in=3600"
  );

  await expect(page.getByTestId("recovery-banner")).toBeVisible(); // hash survived
  const session = await page.evaluate(() => JSON.parse(localStorage.getItem("gideon.session")));
  expect(session.access_token).toBe("rec-at");
});

test("a cross-browser OAuth return while signed out surfaces on the sign-in screen", async ({ page }) => {
  await mockSends(page);
  await page.goto("/?code=STRAY&state=whatever");

  await expect(page.getByTestId("note")).toContainText("different browser");
  await expect(page.getByTestId("signin")).toBeVisible();
});

test("a return finishing while signed out lands under the account that started it", async ({ page }) => {
  await mockSends(page);
  await page.route("**/auth/v1/**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(SESSION) })
  );
  mockOauth(page); // token exchange endpoint
  await page.route(/\/api\/mal\?path=users%2F%40me/, (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: '{"id":1,"name":"evan_mal"}' })
  );
  // The dance was started by reader@example.com (recorded in the PKCE
  // record), but the session is gone when MAL redirects back.
  await page.addInitScript(() => {
    localStorage.setItem(
      "gideon.mal.pkce",
      JSON.stringify({ verifier: "v".repeat(60), state: "STATE1", at: Date.now(), email: "reader@example.com" })
    );
  });
  await page.goto("/?code=CODE1&state=STATE1");
  await expect(page.getByTestId("signin")).toBeVisible(); // still signed out

  // Tokens went straight to that account's key — no shared anonymous slot a
  // different user could inherit.
  const stored = await page.evaluate(() =>
    JSON.parse(localStorage.getItem("gideon.mal.reader@example.com") || "null")
  );
  expect(stored?.access_token).toBe("mal-at");
  expect(await page.evaluate(() => localStorage.getItem("gideon.mal.anon"))).toBeNull();

  // Signing in as that user finds the connection waiting.
  await page.route("**/rest/v1/reading_progress**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(ROWS) })
  );
  mockMalApi(page);
  await page.locator("input[type=email]").fill("reader@example.com");
  await page.locator("input[type=password]").fill("password123");
  await page.getByTestId("signin").click();
  await page.getByTestId("tab-discover").click();
  await expect(page.getByTestId("mal-connected")).toBeVisible();
  await expect(page.getByTestId("mal-badge")).toContainText("MyAnimeList");
});

test("an already-connected browser replaying a code stays quiet", async ({ page }) => {
  await mockSends(page);
  mockMalApi(page);
  await page.route("**/rest/v1/reading_progress**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(ROWS) })
  );
  await page.addInitScript(CONNECTED);
  await page.addInitScript(() => {
    localStorage.setItem(
      "gideon.session",
      JSON.stringify({ access_token: "t", refresh_token: "r", email: "reader@example.com", expires_at: Math.floor(Date.now() / 1000) + 3600 })
    );
  });
  // Back-button / history replay: a code arrives but we're already connected.
  await page.goto("/?code=REPLAY&state=whatever");

  await page.getByTestId("tab-discover").click();
  await expect(page.getByTestId("mal-connected")).toBeVisible();
  await expect(page.getByTestId("mal-error")).toHaveCount(0); // no bogus advice
  expect(new URL(page.url()).search).toBe(""); // code still scrubbed
});

// --- late-arriving recommendations must not disturb the app -----------------

// Holds the manga-list call open so recommendations are still running while
// the test drives the UI elsewhere; release() lets them finish.
function mockSlowRecs(page) {
  let release;
  const gate = new Promise((r) => (release = r));
  page.route(/\/api\/mal\?path=/, async (route) => {
    const [p] = (new URL(route.request().url()).searchParams.get("path") || "").split("?");
    const json = (b) =>
      route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(b) });
    if (p.includes("/animelist")) return json({ data: [] });
    if (p.includes("/mangalist")) {
      await gate;
      return json({ data: [{ node: { id: 900, title: "Vagabond" }, list_status: { score: 9, num_chapters_read: 104 } }] });
    }
    if (p === "manga/900") return json(MAL.manga900);
    if (p === "manga/ranking") return json(MAL.ranking);
    return json({ data: [] });
  });
  return () => release();
}

test("recommendations finishing while you're reading don't kick you out", async ({ page }) => {
  await mockSends(page);
  const finishRecs = mockSlowRecs(page);
  await page.route("**/rest/v1/chapter_pages**", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify([{ chapter_key: "One Piece/vol3.cbz", page_urls: ["https://cdn.test/p1.png"] }]),
    })
  );
  await page.addInitScript(CONNECTED);
  await signInWithRows(page, ROWS); // lands on Discover, recs start

  await expect(page.getByTestId("disc-loading")).toBeVisible();
  // Go read a chapter while the recommendations are still running.
  await page.getByTestId("tab-library").click();
  await page.getByTestId("tile").first().click();
  await expect(page.getByTestId("reader-img")).toBeVisible();

  finishRecs();
  await page.waitForTimeout(1200);
  // Still reading — the reader was never torn down.
  await expect(page.getByTestId("reader-img")).toBeVisible();
  await expect(page.getByTestId("disc-loading")).toHaveCount(0);
});

test("recommendations arriving don't wipe a half-typed search or double-send", async ({ page }) => {
  const posted = [];
  await mockSends(page, posted);
  const finishRecs = mockSlowRecs(page);
  await page.addInitScript(CONNECTED);
  await signInWithRows(page, ROWS);

  await expect(page.getByTestId("disc-loading")).toBeVisible();
  await page.getByTestId("search-input").fill("berserk");
  // Send a browse card while recs are still in flight.
  const card = page.getByTestId("browse-results").getByTestId("rec-card").first();
  await card.getByTestId("rec-send").click();

  finishRecs();
  await expect(page.getByTestId("rec-similar")).toBeVisible({ timeout: 15000 });

  // Typed query survived, and the card still reads as sent — one row only.
  await expect(page.getByTestId("search-input")).toHaveValue("berserk");
  await expect(
    page.getByTestId("browse-results").getByTestId("rec-card").first().getByTestId("rec-send")
  ).toHaveText(/Sent to Kobo/);
  await page.waitForTimeout(300);
  expect(posted).toHaveLength(1);
});

test("library cards and the stats sheet show the community rating", async ({ page }) => {
  await mockSends(page);
  mockMalApi(page); // the limit=1 lookup answers One Piece with mean 9.0
  await signInAnd(page, "tab-library");
  await page.getByTestId("view-list").click();

  await expect(page.getByTestId("rating").first()).toContainText("★ 9.0");

  // Same number in the long-press sheet's stats view.
  await page.getByTestId("item").first().locator("summary").click({ button: "right" });
  await page.getByTestId("sheet-stats").click();
  await expect(page.getByTestId("sheet-fact").filter({ hasText: "Community score" })).toContainText("★ 9.0 / 10");
});
