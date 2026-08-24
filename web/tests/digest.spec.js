import { test, expect } from "@playwright/test";

// The digest: tapping a card's cover or title opens everything MyAnimeList
// knows about that manga — ratings, description, length, run, authors, and
// what its readers loved next — in one fetch across three tabs. MAL is
// mocked at the HTTP boundary so these run offline.

const SESSION = {
  access_token: "test-access-token",
  refresh_token: "test-refresh-token",
  expires_in: 3600,
  user: { email: "reader@example.com" },
};

const ROWS = [
  {
    chapter_key: "Berserk/vol1.cbz",
    current_page: 4,
    total_pages: 20,
    updated_at: new Date(Date.now() - 3600e3).toISOString(),
  },
];

const RANKING = {
  data: [
    {
      node: {
        id: 501,
        title: "Chainsaw Man",
        media_type: "manga",
        mean: 8.6,
        main_picture: { large: "https://img.test/csm.jpg" },
        genres: [{ name: "Action" }, { name: "Horror" }],
      },
    },
  ],
};

// One full manga record, as MAL's v2 detail call returns it.
const CSM = {
  id: 501,
  title: "Chainsaw Man",
  main_picture: { large: "https://img.test/csm.jpg" },
  alternative_titles: { en: "Chainsaw Man", ja: "チェンソーマン" },
  start_date: "2018-12-03",
  synopsis: "Denji is a teenage boy living with a Chainsaw Devil named Pochita.\n\nHe hunts devils to pay off his debt.",
  background: "Serialized in Weekly Shounen Jump.",
  mean: 8.62,
  rank: 42,
  popularity: 7,
  num_list_users: 412345,
  num_scoring_users: 98765,
  media_type: "manga",
  status: "currently_publishing",
  genres: [{ name: "Action" }, { name: "Horror" }],
  num_volumes: 0,
  num_chapters: 0,
  authors: [{ node: { first_name: "Tatsuki", last_name: "Fujimoto" }, role: "Story & Art" }],
  serialization: [{ node: { name: "Shounen Jump (Weekly)" } }],
  recommendations: [
    {
      node: { id: 601, title: "Dorohedoro", main_picture: { large: "https://img.test/doro.jpg" } },
      num_recommendations: 12,
    },
  ],
};

const DORO = {
  id: 601,
  title: "Dorohedoro",
  main_picture: { large: "https://img.test/doro.jpg" },
  alternative_titles: {},
  start_date: "2000-01-01",
  end_date: "2018-09-12",
  mean: 8.8,
  media_type: "manga",
  status: "finished",
  num_volumes: 23,
  num_chapters: 167,
  genres: [{ name: "Action" }],
  authors: [],
  serialization: [],
  recommendations: [],
};

const BERSERK = { ...DORO, id: 700, title: "Berserk", synopsis: "", recommendations: [] };

function mockMal(page, { detail = {}, onDetail } = {}) {
  const bodies = { "manga/501": CSM, "manga/601": DORO, "manga/700": BERSERK, ...detail };
  return page.route(/\/api\/mal\?path=/, (route) => {
    const raw = new URL(route.request().url()).searchParams.get("path") || "";
    const [p, qs = ""] = raw.split("?");
    const q = new URLSearchParams(qs);
    const json = (b, s = 200) =>
      route.fulfill({ status: s, contentType: "application/json", body: JSON.stringify(b) });
    if (p === "manga/ranking") return json(RANKING);
    if (p === "manga") {
      // Library titles carry no MAL id, so the digest resolves one by search.
      const term = (q.get("q") || "").toLowerCase();
      if (term.includes("berserk")) return json({ data: [{ node: { id: 700 } }] });
      return json({ data: [] });
    }
    if (bodies[p]) {
      const hit = onDetail?.(p);
      if (hit) return hit(route);
      return json(bodies[p]);
    }
    return json({ data: [] });
  });
}

async function signInAnd(page, tab) {
  await page.route("**/auth/v1/**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(SESSION) })
  );
  await page.route("**/rest/v1/reading_progress**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(ROWS) })
  );
  await page.route("**/rest/v1/chapter_pages**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: "[]" })
  );
  await page.route("**/rest/v1/send_queue**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: "[]" })
  );
  await page.goto("/");
  await page.locator("input[type=email]").fill("reader@example.com");
  await page.locator("input[type=password]").fill("password123");
  await page.getByTestId("signin").click();
  if (tab) await page.getByTestId(tab).click();
}

test("a card opens the digest, and Back returns to the rail as it was", async ({ page }) => {
  await mockMal(page);
  await signInAnd(page, "tab-discover");

  await page.getByTestId("pill-genre-horror").click();
  await expect(page.getByTestId("disc-rail").getByTestId("rec-card")).toHaveCount(1);
  await page.getByTestId("rec-open").first().click();

  // The hero: title, author, and the run/status/type facts in one glance.
  const digest = page.getByTestId("digest");
  await expect(digest.getByTestId("digest-title")).toHaveText("Chainsaw Man");
  await expect(digest).toContainText("Tatsuki Fujimoto (Story & Art)");
  await expect(digest).toContainText("2018 – ongoing");
  await expect(digest).toContainText("Ongoing");
  // Overview: genres, the description, and the score tiles.
  await expect(digest.getByTestId("digest-genres")).toContainText("Horror");
  await expect(digest.getByTestId("digest-synopsis")).toContainText("Chainsaw Devil");
  await expect(digest.getByTestId("digest-stats")).toContainText("8.62");
  await expect(digest.getByTestId("digest-stats")).toContainText("98,765 ratings");
  await expect(digest.getByTestId("digest-stats")).toContainText("#42");

  await page.getByTestId("digest-back").click();
  // Back where we were: same pill, same rail — nothing reloaded.
  await expect(page.getByTestId("pill-genre-horror")).toHaveAttribute("aria-selected", "true");
  await expect(page.getByTestId("disc-rail").getByTestId("rec-card")).toHaveCount(1);
});

test("the digest tabs carry details and the community, from one fetch", async ({ page }) => {
  let detailCalls = 0;
  await mockMal(page, {
    onDetail: (p) => {
      if (p === "manga/501") detailCalls++;
      return null;
    },
  });
  await signInAnd(page, "tab-discover");
  await page.getByTestId("rec-open").first().click();
  await expect(page.getByTestId("digest-title")).toBeVisible();

  await page.getByTestId("digest-tab-details").click();
  const facts = page.getByTestId("digest-details");
  await expect(facts).toContainText("チェンソーマン");
  await expect(facts).toContainText("Shounen Jump (Weekly)");
  await expect(facts).toContainText("Action, Horror");
  await expect(facts).toContainText("412,345");

  await page.getByTestId("digest-tab-community").click();
  const recs = page.getByTestId("digest-recs").getByTestId("rec-card");
  await expect(recs).toHaveCount(1);
  await expect(recs.first()).toContainText("Dorohedoro");
  await expect(recs.first()).toContainText("12 readers recommend it");
  // MAL serves no reviews; the tab says so rather than leaving a hole.
  await expect(page.getByTestId("digest")).toContainText("doesn't serve reviews or comments");

  // Three tabs, one request.
  expect(detailCalls).toBe(1);
});

test("a recommendation opens its own digest, and Back unwinds one hop", async ({ page }) => {
  await mockMal(page);
  await signInAnd(page, "tab-discover");
  await page.getByTestId("rec-open").first().click();
  await page.getByTestId("digest-tab-community").click();

  await page.getByTestId("digest-recs").getByTestId("rec-open").first().click();
  await expect(page.getByTestId("digest-title")).toHaveText("Dorohedoro");
  await expect(page.getByTestId("digest")).toContainText("2000 – 2018");
  await expect(page.getByTestId("digest")).toContainText("23 volumes · 167 chapters");

  // Back goes to Chainsaw Man, not all the way out.
  await page.getByTestId("digest-back").click();
  await expect(page.getByTestId("digest-title")).toHaveText("Chainsaw Man");
  await page.getByTestId("digest-back").click();
  await expect(page.getByTestId("disc-rail")).toBeVisible();
});

test("a library title with no MAL id resolves itself by search", async ({ page }) => {
  await mockMal(page);
  await signInAnd(page, "tab-library");

  // Long-press the shelf tile for the book sheet, then Title details.
  await page.getByTestId("tile").first().click({ button: "right" });
  await page.getByTestId("sheet-digest").click();
  await expect(page.getByTestId("digest-title")).toHaveText("Berserk");
  // No description on this record — say so instead of showing a blank.
  await expect(page.getByTestId("digest-no-synopsis")).toBeVisible();
});

test("a digest that can't load offers Retry, and the retry works", async ({ page }) => {
  let calls = 0;
  await mockMal(page, {
    onDetail: (p) => {
      if (p !== "manga/501") return null;
      calls++;
      if (calls > 1) return null;
      return (route) =>
        route.fulfill({
          status: 502,
          contentType: "application/json",
          body: JSON.stringify({ message: "MyAnimeList didn’t answer" }),
        });
    },
  });
  await signInAnd(page, "tab-discover");
  await page.getByTestId("rec-open").first().click();

  await expect(page.getByTestId("digest-error")).toContainText("didn’t answer");
  await page.getByTestId("digest-retry").click();
  await expect(page.getByTestId("digest-title")).toHaveText("Chainsaw Man");
});
