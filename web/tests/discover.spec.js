import { test, expect } from "@playwright/test";

// Discover tab: manga recommendations from a public anime list (AniList
// GraphQL, or MyAnimeList via the Jikan mirror), with one-tap Send to Kobo.
// Both providers are mocked at the HTTP boundary, like the Supabase mocks in
// ui.spec.js — fully offline and deterministic even while the real services
// are having outages.

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

// --- AniList fixtures -------------------------------------------------------

const ANIME_LIST = {
  lists: [
    {
      entries: [
        { score: 9, media: { id: 1, title: { romaji: "Sousou no Frieren", english: "Frieren: Beyond Journey's End" } } },
        { score: 8, media: { id: 2, title: { romaji: "One Piece" } } },
      ],
    },
  ],
};
// They already read Vagabond on AniList — excluded from "similar".
const MANGA_LIST = {
  lists: [{ entries: [{ score: 7, media: { id: 900, title: { romaji: "Vagabond" } } }] }],
};
const RELATIONS = [
  {
    id: 1,
    title: { romaji: "Sousou no Frieren" },
    relations: {
      edges: [
        {
          relationType: "SOURCE",
          node: {
            id: 101,
            type: "MANGA",
            format: "MANGA",
            title: { romaji: "Sousou no Frieren", english: "Frieren: Beyond Journey's End" },
            coverImage: { large: "https://img.test/frieren.jpg" },
            averageScore: 86,
          },
        },
      ],
    },
  },
  {
    id: 2,
    title: { romaji: "One Piece" },
    relations: {
      edges: [
        {
          relationType: "SOURCE",
          node: {
            id: 102,
            type: "MANGA",
            format: "MANGA",
            title: { romaji: "One Piece" },
            coverImage: { large: "https://img.test/op.jpg" },
            averageScore: 90,
          },
        },
      ],
    },
  },
];
const SIMILAR = [
  {
    id: 101,
    title: { romaji: "Sousou no Frieren" },
    recommendations: {
      nodes: [
        {
          rating: 140,
          mediaRecommendation: {
            id: 201,
            type: "MANGA",
            format: "MANGA",
            title: { romaji: "Yokohama Kaidashi Kikou" },
            coverImage: { large: "https://img.test/ykk.jpg" },
            averageScore: 84,
          },
        },
        {
          // Already on their AniList manga list — must be excluded.
          rating: 120,
          mediaRecommendation: {
            id: 900,
            type: "MANGA",
            format: "MANGA",
            title: { romaji: "Vagabond" },
            coverImage: { large: "https://img.test/vag.jpg" },
            averageScore: 92,
          },
        },
        {
          // A light novel — not manga, must be excluded.
          rating: 100,
          mediaRecommendation: {
            id: 301,
            type: "MANGA",
            format: "NOVEL",
            title: { romaji: "Some Light Novel" },
            coverImage: { large: "" },
            averageScore: 70,
          },
        },
      ],
    },
  },
];

// Browse/search fixtures: one light novel that must be filtered out, and
// scores on the 0-100 AniList scale.
const BROWSE = [
  { id: 501, format: "MANGA", title: { romaji: "Chainsaw Man" }, coverImage: { large: "https://img.test/csm.jpg" }, averageScore: 86, genres: ["Action", "Horror"] },
  { id: 502, format: "NOVEL", title: { romaji: "A Light Novel" }, coverImage: { large: "" }, averageScore: 70, genres: [] },
  { id: 503, format: "MANGA", title: { romaji: "Vagabond" }, coverImage: { large: "https://img.test/vag2.jpg" }, averageScore: 92, genres: ["Drama"] },
];
const SEARCH_RES = [
  { id: 601, format: "MANGA", title: { romaji: "Berserk" }, coverImage: { large: "https://img.test/berserk.jpg" }, averageScore: 93, genres: ["Dark Fantasy"] },
];

function mockAniList(page, { failWith } = {}) {
  return page.route(/graphql\.anilist\.co/, (route) => {
    const json = (body) =>
      route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(body) });
    if (failWith) return json({ errors: [{ message: failWith, status: 403 }] });
    const { query, variables } = JSON.parse(route.request().postData() || "{}");
    if (query.includes("MediaListCollection")) {
      return json({ data: { MediaListCollection: variables.type === "MANGA" ? MANGA_LIST : ANIME_LIST } });
    }
    if (/q\d+: Page/.test(query)) {
      // Library-ratings batch: one aliased sub-query per series.
      const n = (query.match(/q\d+: Page/g) || []).length;
      const data = {};
      for (let i = 0; i < n; i++) data[`q${i}`] = { media: [{ averageScore: 92 }] };
      return json({ data });
    }
    if (query.includes("$search")) return json({ data: { Page: { media: SEARCH_RES } } });
    if (query.includes("$sort")) return json({ data: { Page: { media: BROWSE } } });
    if (query.includes("recommendations")) return json({ data: { Page: { media: SIMILAR } } });
    return json({ data: { Page: { media: RELATIONS } } });
  });
}

// --- Jikan (MyAnimeList) fixtures -------------------------------------------

function mockJikan(page) {
  return page.route(/api\.jikan\.moe/, (route) => {
    const path = new URL(route.request().url()).pathname;
    const json = (data) =>
      route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ data }) });
    if (path.includes("/users/")) {
      return json([{ score: 10, anime: { mal_id: 51, title: "Vinland Saga" } }]);
    }
    if (path.includes("/anime/51/full")) {
      return json({
        relations: [{ relation: "Adaptation", entry: [{ mal_id: 642, type: "manga", name: "Vinland Saga" }] }],
      });
    }
    if (path.includes("/manga/642/recommendations")) {
      return json([{ entry: { mal_id: 656, title: "Berserk", images: { jpg: { large_image_url: "https://img.test/berserk.jpg" } } } }]);
    }
    if (path.includes("/manga/642")) {
      return json({ images: { jpg: { large_image_url: "https://img.test/vinland.jpg" } }, score: 8.8 });
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

async function signInToDiscover(page) {
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
  await page.getByTestId("tab-discover").click();
}

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => localStorage.clear());
  await page.route("**/rest/v1/chapter_pages**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: "[]" })
  );
  // Default AniList: empty everything, so the background ratings lookup and
  // the auto-loaded browse row never touch the real network. Tests that need
  // data re-route with mockAniList (later registrations win).
  await page.route(/graphql\.anilist\.co/, (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: '{"data":{}}' })
  );
});

// --- tests --------------------------------------------------------------------

test("the Discover tab shows the connect form", async ({ page }) => {
  await mockSends(page);
  await signInToDiscover(page);

  await expect(page.getByTestId("disc-user")).toBeVisible();
  await expect(page.getByTestId("disc-go")).toBeVisible();
  await expect(page.getByTestId("disc-anilist")).toHaveClass(/on/); // default provider
  await expect(page.getByTestId("disc-mal")).toBeVisible();
});

test("AniList: recommends the source manga and similar reads, filtered", async ({ page }) => {
  await mockSends(page);
  await mockAniList(page);
  await signInToDiscover(page);

  await page.getByTestId("disc-user").fill("evan");
  await page.getByTestId("disc-go").click();

  // "Read the source": Frieren (their 9/10) is in; One Piece is dropped
  // because it's already in the gideon library.
  const sources = page.getByTestId("rec-sources").getByTestId("rec-card");
  await expect(sources).toHaveCount(1);
  await expect(sources.first()).toContainText("Frieren");
  await expect(sources.first()).toContainText("You rated the anime 9/10");

  // "More like what you love": Yokohama is in; Vagabond is dropped (already
  // on their AniList manga list) and the light novel is dropped (not manga).
  const similar = page.getByTestId("rec-similar").getByTestId("rec-card");
  await expect(similar).toHaveCount(1);
  await expect(similar.first()).toContainText("Yokohama");

  // The list is remembered for next time, and "Change" reopens the form
  // prefilled.
  await expect(page.getByTestId("disc-who")).toContainText("evan");
  await page.getByTestId("disc-change").click();
  await expect(page.getByTestId("disc-user")).toHaveValue("evan");
});

test("a card's Send to Kobo enqueues title + cover and shows in pending sends", async ({ page }) => {
  const posted = [];
  await mockSends(page, posted);
  await mockAniList(page);
  await signInToDiscover(page);

  await page.getByTestId("disc-user").fill("evan");
  await page.getByTestId("disc-go").click();

  const card = page.getByTestId("rec-sources").getByTestId("rec-card").first();
  await card.getByTestId("rec-send").click();
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

test("a provider outage shows a clear error, not an endless spinner", async ({ page }) => {
  await mockSends(page);
  await mockAniList(page, {
    failWith: "The AniList API has been temporarily disabled due to severe stability issues.",
  });
  await signInToDiscover(page);

  await page.getByTestId("disc-user").fill("evan");
  await page.getByTestId("disc-go").click();

  await expect(page.getByTestId("disc-error")).toContainText("temporarily disabled");
  await expect(page.getByTestId("disc-go")).toBeVisible(); // form is back for a retry
});

test("browse loads trending automatically, filters novels, shows ratings, and sends", async ({ page }) => {
  const posted = [];
  await mockSends(page, posted);
  await mockAniList(page);
  await signInToDiscover(page);

  // Trending loads on tab open with no interaction. The light novel is
  // filtered; the manga carry ★ badges on the AniList 0-100 scale ÷ 10.
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
  await mockAniList(page);
  await signInToDiscover(page);

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
  await page.route(/graphql\.anilist\.co/, (route) => {
    const { query } = JSON.parse(route.request().postData() || "{}");
    const json = (body) =>
      route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(body) });
    if (!query.includes("$sort")) return json({ data: {} }); // ratings batch etc.
    calls++;
    if (calls === 1) return json({ errors: [{ message: "The AniList API has been temporarily disabled due to severe stability issues." }] });
    return json({ data: { Page: { media: BROWSE } } });
  });
  await signInToDiscover(page);

  await expect(page.getByTestId("browse-error")).toContainText("temporarily disabled");
  await page.getByTestId("browse-retry").click();
  await expect(page.getByTestId("browse-results").getByTestId("rec-card")).toHaveCount(2);
});

test("the browse row arriving does not wipe a half-typed username", async ({ page }) => {
  await mockSends(page);
  // Delay only the browse query so it lands after typing has started.
  await page.route(/graphql\.anilist\.co/, async (route) => {
    const { query } = JSON.parse(route.request().postData() || "{}");
    const json = (body) =>
      route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(body) });
    if (query.includes("$sort")) {
      await new Promise((r) => setTimeout(r, 800));
      return json({ data: { Page: { media: BROWSE } } });
    }
    return json({ data: {} });
  });
  await signInToDiscover(page);

  await page.getByTestId("disc-user").fill("halftyped");
  await expect(page.getByTestId("browse-results")).toBeVisible();
  await expect(page.getByTestId("disc-user")).toHaveValue("halftyped");
});

test("library cards and the stats sheet show the community rating", async ({ page }) => {
  await mockSends(page);
  await mockAniList(page); // ratings batch answers ★ 9.2 for every series
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
  await page.getByTestId("tab-library").click();
  await page.getByTestId("view-list").click();

  await expect(page.getByTestId("rating").first()).toContainText("★ 9.2");

  // Same number in the long-press sheet's stats view.
  await page.getByTestId("item").first().locator("summary").click({ button: "right" });
  await page.getByTestId("sheet-stats").click();
  await expect(page.getByTestId("sheet-fact").filter({ hasText: "Community score" })).toContainText("★ 9.2 / 10");
});

test("MyAnimeList (via Jikan): the full flow produces sendable cards", async ({ page }) => {
  await mockSends(page);
  await mockJikan(page);
  await signInToDiscover(page);

  await page.getByTestId("disc-mal").click();
  await page.getByTestId("disc-user").fill("evan_mal");
  await page.getByTestId("disc-go").click();

  // Jikan is throttled client-side (350ms between calls), so allow a bit more
  // time than the default for the cards to arrive.
  const sources = page.getByTestId("rec-sources").getByTestId("rec-card");
  await expect(sources).toHaveCount(1, { timeout: 15000 });
  await expect(sources.first()).toContainText("Vinland Saga");
  await expect(sources.first()).toContainText("You rated the anime 10/10");

  const similar = page.getByTestId("rec-similar").getByTestId("rec-card");
  await expect(similar).toHaveCount(1);
  await expect(similar.first()).toContainText("Berserk");
});
