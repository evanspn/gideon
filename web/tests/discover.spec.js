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
  await page.addInitScript(() => localStorage.clear());
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
