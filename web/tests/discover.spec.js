import { test, expect } from "@playwright/test";

// Discover tab: manga recommendations from a public MyAnimeList (read via the
// community Jikan mirror), plus search and trending/top browse — all with
// one-tap Send to Kobo. Jikan and Supabase are mocked at the HTTP boundary,
// so these run fully offline and deterministically even while the real
// services are having outages. tests/live.spec.js covers the real Jikan API.

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

// --- Jikan fixtures ---------------------------------------------------------

const img = (name) => ({ jpg: { large_image_url: `https://img.test/${name}.jpg` } });

const ANIMELIST = [
  { score: 10, anime: { mal_id: 51, title: "Sousou no Frieren" } },
  { score: 8, anime: { mal_id: 52, title: "One Piece" } },
];
// They already read Vagabond on MAL — excluded from "similar".
const MANGALIST = [{ manga: { title: "Vagabond" } }];
const RELATIONS = {
  51: { relations: [{ relation: "Adaptation", entry: [{ mal_id: 101, type: "manga", name: "Frieren: Beyond Journey's End" }] }] },
  52: { relations: [{ relation: "Adaptation", entry: [{ mal_id: 102, type: "manga", name: "One Piece" }] }] },
};
const MANGA = {
  101: { images: img("frieren"), score: 8.6 },
  102: { images: img("op"), score: 9.0 },
};
const RECOMMENDATIONS = {
  101: [
    { entry: { mal_id: 201, title: "Yokohama Kaidashi Kikou", images: img("ykk") } },
    { entry: { mal_id: 900, title: "Vagabond", images: img("vag") } }, // excluded via mangalist
  ],
  102: [],
};
const TOP = [
  { mal_id: 501, type: "Manga", title: "Chainsaw Man", images: img("csm"), score: 8.6, genres: [{ name: "Action" }, { name: "Horror" }] },
  { mal_id: 502, type: "Light Novel", title: "A Light Novel", images: img("ln"), score: 7.0, genres: [] },
  { mal_id: 503, type: "Manga", title: "Vagabond", images: img("vag2"), score: 9.2, genres: [{ name: "Drama" }] },
];
const SEARCH_RES = [
  { mal_id: 601, type: "Manga", title: "Berserk", images: img("berserk"), score: 9.3, genres: [{ name: "Dark Fantasy" }] },
];

// One dispatcher for the whole Jikan surface the app uses. Overrides let a
// test fail a single endpoint (e.g. the animelist) while the rest works.
function mockJikan(page, { overrides = {} } = {}) {
  return page.route(/api\.jikan\.moe/, (route) => {
    const url = new URL(route.request().url());
    const p = url.pathname;
    const json = (data) =>
      route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ data }) });
    const outage = () =>
      route.fulfill({
        status: 504,
        contentType: "application/json",
        body: JSON.stringify({ status: 504, message: "Jikan failed to connect to MyAnimeList. MyAnimeList may be down/unavailable or refuses to connect" }),
      });
    for (const [key, kind] of Object.entries(overrides)) {
      if (p.includes(key)) return kind === "outage" ? outage() : json(kind);
    }
    if (p.includes("/animelist")) return json(ANIMELIST);
    if (p.includes("/mangalist")) return json(MANGALIST);
    const anime = p.match(/\/anime\/(\d+)\/full/);
    if (anime) return json(RELATIONS[anime[1]] || { relations: [] });
    const recs = p.match(/\/manga\/(\d+)\/recommendations/);
    if (recs) return json(RECOMMENDATIONS[recs[1]] || []);
    const manga = p.match(/\/manga\/(\d+)$/);
    if (manga) return json(MANGA[manga[1]] || {});
    if (p.includes("/top/manga")) return json(TOP);
    if (p.endsWith("/manga")) {
      // limit=1 is the library-ratings lookup; larger limits are the search box.
      return json(url.searchParams.get("limit") === "1" ? [{ score: 9.2 }] : SEARCH_RES);
    }
    return json([]);
  });
}

// --- MAL official-API proxy fixtures (api/mal.js shapes) --------------------

const MAL = {
  animelist: {
    data: [
      { node: { id: 51, title: "Sousou no Frieren" }, list_status: { score: 10 } },
      { node: { id: 52, title: "One Piece" }, list_status: { score: 8 } },
    ],
  },
  mangalist: { data: [{ node: { id: 900, title: "Vagabond" } }] },
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

function mockMalProxy(page, { animelistStatus } = {}) {
  return page.route("**/api/mal**", (route) => {
    const url = new URL(route.request().url());
    const [p, qs = ""] = (url.searchParams.get("path") || "").split("?");
    const q = new URLSearchParams(qs);
    const json = (b, status = 200) =>
      route.fulfill({ status, contentType: "application/json", body: JSON.stringify(b) });
    if (p.endsWith("/animelist")) {
      if (animelistStatus) return json({ message: "", error: "not_found" }, animelistStatus);
      return json(MAL.animelist);
    }
    if (p.endsWith("/mangalist")) return json(MAL.mangalist);
    if (p === "anime/51") return json(MAL.anime51);
    if (p === "anime/52") return json(MAL.anime52);
    if (p === "manga/101") return json(MAL.manga101);
    if (p === "manga/102") return json(MAL.manga102);
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

// Counts Jikan traffic; register before mockMalProxy so proxy-path tests can
// assert the mirror was never touched.
function trackJikan(page) {
  const calls = [];
  page.route(/api\.jikan\.moe/, (route) => {
    calls.push(route.request().url());
    return route.fulfill({ status: 200, contentType: "application/json", body: '{"data":[]}' });
  });
  return calls;
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
  // Default Jikan: empty everything, so the background ratings lookup and the
  // auto-loaded browse row never touch the real network. Tests that need data
  // re-route with mockJikan (later registrations win).
  await page.route(/api\.jikan\.moe/, (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: '{"data":[]}' })
  );
});

// --- tests --------------------------------------------------------------------

test("the Discover tab shows the MyAnimeList connect form", async ({ page }) => {
  await mockSends(page);
  await signInAnd(page, "tab-discover");

  await expect(page.getByTestId("disc-user")).toBeVisible();
  await expect(page.getByTestId("disc-user")).toHaveAttribute("placeholder", /MyAnimeList/);
  await expect(page.getByTestId("disc-go")).toBeVisible();
});

test("recommends the source manga and similar reads, filtered", async ({ page }) => {
  await mockSends(page);
  await mockJikan(page);
  await signInAnd(page, "tab-discover");

  await page.getByTestId("disc-user").fill("evan_mal");
  await page.getByTestId("disc-go").click();

  // "Read the source": Frieren (their 10/10) is in, with cover art and the
  // community score resolved from its manga entry; One Piece is dropped
  // because it's already in the gideon library.
  const sources = page.getByTestId("rec-sources").getByTestId("rec-card");
  await expect(sources).toHaveCount(1, { timeout: 15000 });
  await expect(sources.first()).toContainText("Frieren");
  await expect(sources.first()).toContainText("You rated the anime 10/10");
  await expect(sources.first()).toContainText("★ 8.6");

  // "More like what you love": Yokohama is in; Vagabond is dropped (already
  // on their MAL manga list).
  const similar = page.getByTestId("rec-similar").getByTestId("rec-card");
  await expect(similar).toHaveCount(1);
  await expect(similar.first()).toContainText("Yokohama");

  // The username is remembered, and "Change" reopens the form prefilled.
  await expect(page.getByTestId("disc-who")).toContainText("evan_mal");
  await expect(page.getByTestId("disc-who")).toContainText("MyAnimeList");
  await page.getByTestId("disc-change").click();
  await expect(page.getByTestId("disc-user")).toHaveValue("evan_mal");
});

test("a card's Send to Kobo enqueues title + cover and shows in pending sends", async ({ page }) => {
  const posted = [];
  await mockSends(page, posted);
  await mockJikan(page);
  await signInAnd(page, "tab-discover");

  await page.getByTestId("disc-user").fill("evan_mal");
  await page.getByTestId("disc-go").click();

  const card = page.getByTestId("rec-sources").getByTestId("rec-card").first();
  await card.getByTestId("rec-send").click({ timeout: 15000 });
  await expect(card.getByTestId("rec-send")).toHaveText(/Sent to Kobo/);
  await expect(card.getByTestId("rec-send")).toBeDisabled();

  // The exact row the device will pick up: the title to search for, plus the
  // cover art for the notification.
  expect(posted).toEqual([
    { title: "Frieren: Beyond Journey's End", cover_url: "https://img.test/frieren.jpg" },
  ]);

  // It shows up in the Stats tab's pending-sends list, thumbnail included.
  await page.getByTestId("tab-stats").click();
  const item = page.getByTestId("send-item").first();
  await expect(item).toContainText("Frieren");
  await expect(item.locator(".send-cover")).toHaveAttribute("src", "https://img.test/frieren.jpg");
});

test("a MyAnimeList outage shows a clear error, not an endless spinner", async ({ page }) => {
  await mockSends(page);
  await mockJikan(page, { overrides: { "/animelist": "outage" } });
  await signInAnd(page, "tab-discover");

  await page.getByTestId("disc-user").fill("evan_mal");
  await page.getByTestId("disc-go").click();

  await expect(page.getByTestId("disc-error")).toContainText("MyAnimeList may be down");
  await expect(page.getByTestId("disc-go")).toBeVisible(); // form is back for a retry
});

test("browse loads trending automatically, filters novels, shows ratings, and sends", async ({ page }) => {
  const posted = [];
  await mockSends(page, posted);
  await mockJikan(page);
  await signInAnd(page, "tab-discover");

  // Trending loads on tab open with no interaction. The light novel is
  // filtered; the manga carry ★ badges (MAL's 0-10 scale).
  const cards = page.getByTestId("browse-results").getByTestId("rec-card");
  await expect(cards).toHaveCount(2);
  await expect(cards.nth(0)).toContainText("Chainsaw Man");
  await expect(cards.nth(0)).toContainText("★ 8.6");
  await expect(cards.nth(0)).toContainText("Action · Horror");

  // Cards in browse are sendable like recommendation cards.
  await cards.nth(0).getByTestId("rec-send").click();
  await expect(cards.nth(0).getByTestId("rec-send")).toHaveText(/Sent to Kobo/);
  expect(posted).toEqual([{ title: "Chainsaw Man", cover_url: "https://img.test/csm.jpg" }]);

  // The Top rated chip re-queries and keeps the section alive.
  await page.getByTestId("browse-top").click();
  await expect(page.getByTestId("browse-results").getByTestId("rec-card")).toHaveCount(2);
  await expect(page.getByTestId("browse-top")).toHaveClass(/on/);
});

test("search shows rated results and Clear restores the tab", async ({ page }) => {
  await mockSends(page);
  await mockJikan(page);
  await signInAnd(page, "tab-discover");

  await page.getByTestId("search-input").fill("berserk");
  await page.getByTestId("search-btn").click();

  const results = page.getByTestId("search-results").getByTestId("rec-card");
  await expect(results).toHaveCount(1);
  await expect(results.first()).toContainText("Berserk");
  await expect(results.first()).toContainText("★ 9.3");
  // Search results hide the browse/recs sections until cleared.
  await expect(page.getByTestId("browse-results")).toHaveCount(0);

  await page.getByTestId("search-clear").click();
  await expect(page.getByTestId("search-results")).toHaveCount(0);
  await expect(page.getByTestId("browse-results")).toBeVisible();
});

test("a browse outage shows an inline error with a working Retry", async ({ page }) => {
  await mockSends(page);
  let calls = 0;
  await page.route(/api\.jikan\.moe/, (route) => {
    const p = new URL(route.request().url()).pathname;
    const json = (data) =>
      route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ data }) });
    if (!p.includes("/top/manga")) return json([]);
    calls++;
    if (calls === 1) {
      return route.fulfill({
        status: 504,
        contentType: "application/json",
        body: JSON.stringify({ status: 504, message: "Jikan failed to connect to MyAnimeList. MyAnimeList may be down/unavailable or refuses to connect" }),
      });
    }
    return json(TOP);
  });
  await signInAnd(page, "tab-discover");

  await expect(page.getByTestId("browse-error")).toContainText("MyAnimeList may be down");
  await page.getByTestId("browse-retry").click();
  await expect(page.getByTestId("browse-results").getByTestId("rec-card")).toHaveCount(2);
});

test("a 429 from Jikan is retried once after a pause", async ({ page }) => {
  await mockSends(page);
  let topCalls = 0;
  await page.route(/api\.jikan\.moe/, (route) => {
    const p = new URL(route.request().url()).pathname;
    const json = (data) =>
      route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ data }) });
    if (!p.includes("/top/manga")) return json([]);
    topCalls++;
    if (topCalls === 1) {
      return route.fulfill({
        status: 429,
        contentType: "application/json",
        body: JSON.stringify({ status: 429, message: "You are being rate-limited." }),
      });
    }
    return json(TOP);
  });
  await signInAnd(page, "tab-discover");

  // The single transparent retry absorbs the 429 — the user just sees cards.
  await expect(page.getByTestId("browse-results").getByTestId("rec-card")).toHaveCount(2, {
    timeout: 10000,
  });
  expect(topCalls).toBe(2);
});

test("concurrent features share one Jikan rate limit (requests are spaced)", async ({ page }) => {
  await mockSends(page);
  const times = [];
  await page.route(/api\.jikan\.moe/, (route) => {
    times.push(Date.now());
    const p = new URL(route.request().url()).pathname;
    const data = p.includes("/top/manga") ? TOP : [{ score: 9.2 }];
    return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ data }) });
  });
  // Signing in kicks the library-ratings lookup; opening Discover kicks the
  // browse row — two features hitting Jikan at once. The global limiter must
  // space them out instead of letting them fire together.
  await signInAnd(page, "tab-discover");
  await expect(page.getByTestId("browse-results").getByTestId("rec-card")).toHaveCount(2);
  await expect.poll(() => times.length, { timeout: 10000 }).toBeGreaterThanOrEqual(2);

  const gaps = times.slice(1).map((t, i) => t - times[i]);
  for (const gap of gaps) expect(gap).toBeGreaterThanOrEqual(300);
});

test("the browse row arriving does not wipe a half-typed username", async ({ page }) => {
  await mockSends(page);
  // Delay only the browse query so it lands after typing has started.
  await page.route(/api\.jikan\.moe/, async (route) => {
    const p = new URL(route.request().url()).pathname;
    const json = (data) =>
      route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ data }) });
    if (p.includes("/top/manga")) {
      await new Promise((r) => setTimeout(r, 800));
      return json(TOP);
    }
    return json([]);
  });
  await signInAnd(page, "tab-discover");

  await page.getByTestId("disc-user").fill("halftyped");
  await expect(page.getByTestId("browse-results")).toBeVisible();
  await expect(page.getByTestId("disc-user")).toHaveValue("halftyped");
});

// --- official-API proxy path -------------------------------------------------

test("proxy: the full recommend flow runs on MAL's official API, Jikan untouched", async ({ page }) => {
  await mockSends(page);
  const jikanCalls = trackJikan(page);
  await mockMalProxy(page);
  await signInAnd(page, "tab-discover");

  await page.getByTestId("disc-user").fill("evan_mal");
  await page.getByTestId("disc-go").click();

  // Frieren via the related_manga edge (One Piece resolves via the
  // search-by-title fallback and is then excluded — it's in the library).
  const sources = page.getByTestId("rec-sources").getByTestId("rec-card");
  await expect(sources).toHaveCount(1, { timeout: 15000 });
  await expect(sources.first()).toContainText("Frieren");
  await expect(sources.first()).toContainText("You rated the anime 10/10");
  await expect(sources.first()).toContainText("★ 8.6");

  // Similar: Yokohama in, Vagabond dropped (their MAL manga list).
  const similar = page.getByTestId("rec-similar").getByTestId("rec-card");
  await expect(similar).toHaveCount(1);
  await expect(similar.first()).toContainText("Yokohama");

  expect(jikanCalls).toEqual([]);
});

test("proxy: browse and search run on official rankings/search with ratings", async ({ page }) => {
  await mockSends(page);
  const jikanCalls = trackJikan(page);
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

  expect(jikanCalls).toEqual([]);
});

test("proxy: an unknown MAL user is authoritative — clear error, no Jikan fallback", async ({ page }) => {
  await mockSends(page);
  const jikanCalls = trackJikan(page);
  await mockMalProxy(page, { animelistStatus: 404 });
  await signInAnd(page, "tab-discover");

  await page.getByTestId("disc-user").fill("no_such_user");
  await page.getByTestId("disc-go").click();

  await expect(page.getByTestId("disc-error")).toContainText("wasn't found");
  expect(jikanCalls.filter((u) => u.includes("animelist"))).toEqual([]);
});

test("proxy unconfigured (503) falls back to Jikan transparently", async ({ page }) => {
  await mockSends(page);
  await page.route("**/api/mal**", (route) =>
    route.fulfill({ status: 503, contentType: "application/json", body: '{"error":"proxy-unconfigured"}' })
  );
  await mockJikan(page);
  await signInAnd(page, "tab-discover");

  // Browse arrives via Jikan as before.
  const cards = page.getByTestId("browse-results").getByTestId("rec-card");
  await expect(cards).toHaveCount(2);
  await expect(cards.nth(0)).toContainText("Chainsaw Man");
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
  await mockJikan(page);
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

test("a corrupted OAuth state is discarded quietly — no token exchange", async ({ page }) => {
  await mockSends(page);
  await mockJikan(page);
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
  await mockJikan(page);
  mockOauth(page, { configStatus: 503 });
  await signInAnd(page, "tab-discover");

  await page.getByTestId("mal-connect").click();
  await expect(page.getByTestId("mal-error")).toContainText("isn't configured");
});

test("a connected account runs its recommendations automatically", async ({ page }) => {
  await mockSends(page);
  await mockJikan(page);
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
  await expect(page.getByTestId("mal-connect")).toBeVisible();
  await expect(page.getByTestId("disc-user")).toBeVisible();
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
        return json({ data: [{ node: { id: 13, title: "One Piece", media_type: "manga", num_chapters: 0 } }] });
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

test("library cards and the stats sheet show the community rating", async ({ page }) => {
  await mockSends(page);
  await mockJikan(page); // the limit=1 lookup answers ★ 9.2 for every series
  await signInAnd(page, "tab-library");
  await page.getByTestId("view-list").click();

  await expect(page.getByTestId("rating").first()).toContainText("★ 9.2");

  // Same number in the long-press sheet's stats view.
  await page.getByTestId("item").first().locator("summary").click({ button: "right" });
  await page.getByTestId("sheet-stats").click();
  await expect(page.getByTestId("sheet-fact").filter({ hasText: "Community score" })).toContainText("★ 9.2 / 10");
});
