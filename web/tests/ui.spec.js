import { test, expect } from "@playwright/test";

// The dashboard talks to Supabase over plain fetch, so we mock at that HTTP
// boundary — no SDK internals, no real backend. Each test declares exactly the
// responses it needs.

const SESSION = {
  access_token: "test-access-token",
  refresh_token: "test-refresh-token",
  expires_in: 3600,
  user: { email: "reader@example.com" },
};

const ROWS = [
  {
    chapter_key: "One Piece/vol3.cbz",
    current_page: 10,
    total_pages: 20,
    updated_at: new Date(Date.now() - 3600e3).toISOString(),
  },
  {
    chapter_key: "Naruto/vol1.cbz",
    current_page: 4,
    total_pages: 18,
    updated_at: new Date(Date.now() - 2 * 86400e3).toISOString(),
  },
];

// Data with finished chapters, for the stat-tile / most-read assertions.
const STATS_ROWS = [
  { chapter_key: "Berserk/Chapter 1.cbz", current_page: 19, total_pages: 20, updated_at: new Date(Date.now() - 3600e3).toISOString() },
  { chapter_key: "Berserk/Chapter 2.cbz", current_page: 21, total_pages: 22, updated_at: new Date(Date.now() - 2 * 3600e3).toISOString() },
  { chapter_key: "Berserk/Chapter 3.cbz", current_page: 17, total_pages: 18, updated_at: new Date(Date.now() - 26 * 3600e3).toISOString() },
  { chapter_key: "Naruto/Chapter 1.cbz", current_page: 4, total_pages: 18, updated_at: new Date(Date.now() - 50 * 3600e3).toISOString() },
];

function mockAuthOk(page) {
  return page.route("**/auth/v1/**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(SESSION) })
  );
}
function mockAuthFail(page, message) {
  return page.route("**/auth/v1/**", (route) =>
    route.fulfill({
      status: 400,
      contentType: "application/json",
      body: JSON.stringify({ error_description: message }),
    })
  );
}
function mockProgress(page, rows) {
  return page.route("**/rest/v1/reading_progress**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(rows) })
  );
}

async function fillAndSubmit(page, { email = "reader@example.com", password = "password123", action = "signin" } = {}) {
  await page.locator("input[type=email]").fill(email);
  await page.locator("input[type=password]").fill(password);
  await page.getByTestId(action).click();
}

// An in-memory send_queue mock (GET pending / POST enqueue / DELETE). A later
// registration wins over the empty default in beforeEach.
function mockSends(page, initial = []) {
  let items = initial.slice();
  let n = initial.length;
  return page.route("**/rest/v1/send_queue**", (route) => {
    const req = route.request();
    if (req.method() === "GET") {
      return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(items) });
    }
    if (req.method() === "POST") {
      const { title } = JSON.parse(req.postData() || "{}");
      const row = { id: `id-${++n}`, title, created_at: new Date().toISOString() };
      items = [row, ...items];
      return route.fulfill({ status: 201, contentType: "application/json", body: JSON.stringify([row]) });
    }
    if (req.method() === "DELETE") {
      const m = req.url().match(/id=eq\.([^&]+)/);
      if (m) items = items.filter((x) => x.id !== decodeURIComponent(m[1]));
      return route.fulfill({ status: 204, body: "" });
    }
    return route.fulfill({ status: 200, body: "[]" });
  });
}

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => localStorage.clear());
  // Default: no pending sends, so the dashboard's send fetch never hits the net.
  await page.route("**/rest/v1/send_queue**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: "[]" })
  );
  // Default: no published chapter pages (library covers) — same reason. Reader
  // tests override this with their own routes (later registrations win).
  await page.route("**/rest/v1/chapter_pages**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: "[]" })
  );
});

// --- sign-in --------------------------------------------------------------

test("shows the sign-in form on first load", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Your reading, everywhere" })).toBeVisible();
  await expect(page.locator("input[type=email]")).toBeVisible();
  await expect(page.locator("input[type=password]")).toBeVisible();
  await expect(page.getByTestId("signin")).toBeVisible();
  await expect(page.getByTestId("create")).toBeVisible();
});

test("bad credentials show an error and stay on sign-in", async ({ page }) => {
  await mockAuthFail(page, "Invalid login credentials");
  await page.goto("/");
  await fillAndSubmit(page, { password: "wrongpass" });
  await expect(page.getByTestId("note")).toContainText("Invalid login credentials");
  await expect(page.locator("input[type=email]")).toBeVisible();
  await expect(page.getByTestId("signin")).toBeEnabled();
});

test("create account signs in", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, []);
  await page.goto("/");
  await fillAndSubmit(page, { email: "new@example.com", action: "create" });
  await expect(page.getByTestId("empty")).toBeVisible();
});

test("sign out returns to the sign-in screen", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, []);
  await page.goto("/");
  await fillAndSubmit(page);
  await expect(page.getByTestId("signout")).toBeVisible();
  await page.getByTestId("signout").click();
  await expect(page.getByTestId("signin")).toBeVisible();
});

test("empty progress shows the empty state", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, []);
  await page.goto("/");
  await fillAndSubmit(page);
  await expect(page.getByTestId("empty")).toContainText("No reading progress yet");
});

// --- stats (default view) -------------------------------------------------

test("signing in lands on the stats dashboard", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.goto("/");
  await fillAndSubmit(page);

  await expect(page.getByTestId("tab-stats")).toHaveClass(/on/);
  await expect(page.getByTestId("stat")).toHaveCount(4);
  await expect(page.getByTestId("heatmap")).toBeVisible();
  await expect(page.getByText("Reading activity")).toBeVisible();
  await expect(page.getByText("reader@example.com")).toBeVisible();
});

test("stat tiles and most-read reflect the data", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, STATS_ROWS);
  await page.goto("/");
  await fillAndSubmit(page);

  // 3 of the 4 chapters are finished; 20+22+18+5 = 65 pages; 2 series.
  await expect(page.getByTestId("stat").filter({ hasText: "Chapters read" })).toContainText("3");
  await expect(page.getByTestId("stat").filter({ hasText: "Pages read" })).toContainText("65");
  await expect(page.getByTestId("stat").filter({ hasText: "Pages read" })).toContainText("2 series");

  // Most-read: all 3 finished chapters are Berserk.
  const top = page.getByTestId("top-series");
  await expect(top).toHaveCount(1);
  await expect(top.first()).toContainText("Berserk");
  await expect(top.first()).toContainText("3");
});

test("recently read lists books (not chapters) newest-first and opens the reader", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, STATS_ROWS);
  await page.route("**/rest/v1/chapter_pages**", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify([{ page_urls: ["https://cdn.test/p1.png"] }]),
    })
  );
  await page.goto("/");
  await fillAndSubmit(page);

  // One row per book, newest first: Berserk (read an hour ago) on top, and the
  // row shows the book, not the individual chapter.
  const recent = page.getByTestId("chapter");
  await expect(recent.first()).toContainText("Berserk");
  await expect(recent.first()).not.toContainText("Chapter");
  await recent.first().click(); // opens the book's latest chapter
  await expect(page.getByTestId("reader-img")).toBeVisible();
});

test("defaults to dark mode and the toggle switches theme", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.goto("/");
  await fillAndSubmit(page);
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await page.getByTestId("theme").click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
});

// --- send to Kobo ---------------------------------------------------------

test("the stats view has a Send to Kobo box", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.goto("/");
  await fillAndSubmit(page);
  await expect(page.getByText("Send to Kobo")).toBeVisible();
  await expect(page.getByTestId("send-input")).toBeVisible();
  await expect(page.getByTestId("send-btn")).toBeVisible();
});

test("sending a title enqueues it and lists it", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await mockSends(page); // starts empty
  await page.goto("/");
  await fillAndSubmit(page);

  await expect(page.getByTestId("send-item")).toHaveCount(0);
  await page.getByTestId("send-input").fill("Berserk");
  await page.getByTestId("send-btn").click();

  const item = page.getByTestId("send-item");
  await expect(item).toHaveCount(1);
  await expect(item.first()).toContainText("Berserk");
});

test("a send on a stale session refreshes and retries (401 → refresh → 201)", async ({ page }) => {
  await mockProgress(page, ROWS);
  let refreshes = 0;
  await page.route("**/auth/v1/**", (route) => {
    if (route.request().url().includes("grant_type=refresh_token")) refreshes++;
    return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(SESSION) });
  });
  let posts = 0;
  let items = [];
  await page.route("**/rest/v1/send_queue**", (route) => {
    const req = route.request();
    if (req.method() === "POST") {
      posts++;
      // First attempt hits an expired access token; the retry must succeed.
      if (posts === 1) return route.fulfill({ status: 401, contentType: "application/json", body: "{}" });
      const { title } = JSON.parse(req.postData() || "{}");
      items = [{ id: "id-1", title, created_at: new Date().toISOString() }];
      return route.fulfill({ status: 201, contentType: "application/json", body: JSON.stringify(items) });
    }
    return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(items) });
  });
  await page.goto("/");
  await fillAndSubmit(page);

  await page.getByTestId("send-input").fill("Berserk");
  await page.getByTestId("send-btn").click();
  await expect(page.getByTestId("send-item")).toHaveCount(1);
  await expect(page.getByTestId("send-item").first()).toContainText("Berserk");
  expect(refreshes).toBe(1);
  expect(posts).toBe(2);
});

test("a failed send surfaces the error and keeps the typed title", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.route("**/rest/v1/send_queue**", (route) =>
    route.request().method() === "POST"
      ? route.fulfill({ status: 500, contentType: "application/json", body: "{}" })
      : route.fulfill({ status: 200, contentType: "application/json", body: "[]" })
  );
  await page.goto("/");
  await fillAndSubmit(page);

  await page.getByTestId("send-input").fill("Berserk");
  await page.getByTestId("send-btn").click();
  await expect(page.getByTestId("send-note")).toContainText("Couldn't send");
  await expect(page.getByTestId("send-input")).toHaveValue("Berserk"); // retry is one tap
  await expect(page.getByTestId("send-btn")).toBeEnabled();
});

test("a pending send can be removed", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await mockSends(page, [{ id: "id-1", title: "Vagabond", created_at: new Date().toISOString() }]);
  await page.goto("/");
  await fillAndSubmit(page);

  await expect(page.getByTestId("send-item")).toHaveCount(1);
  await page.getByTestId("send-remove").first().click();
  await expect(page.getByTestId("send-item")).toHaveCount(0);
});

// --- library tab ----------------------------------------------------------

test("the Library tab shows the continue-reading list", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.goto("/");
  await fillAndSubmit(page);
  await page.getByTestId("tab-library").click();
  await page.getByTestId("view-list").click();

  await expect(page.getByText("Continue reading")).toBeVisible();
  const items = page.getByTestId("item");
  await expect(items).toHaveCount(2);
  await expect(items.nth(0)).toContainText("One Piece");
  await expect(items.nth(0)).toContainText("11/20");
  await expect(items.nth(1)).toContainText("Naruto");
});

test("the Library tab defaults to the cover shelf and a tile opens the reader", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.goto("/");
  await fillAndSubmit(page);
  await page.getByTestId("tab-library").click();

  // Grid by default: one tile per series, no list cards.
  await expect(page.getByTestId("shelf")).toBeVisible();
  await expect(page.getByTestId("tile")).toHaveCount(2);
  await expect(page.getByTestId("item")).toHaveCount(0);
  await expect(page.getByTestId("tile").nth(0)).toContainText("One Piece");

  // Tapping a tile goes straight to the reader for the current chapter.
  await page.getByTestId("tile").first().click();
  await expect(page.getByTestId("reader-back")).toBeVisible();
});

test("the library view choice persists across renders", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.goto("/");
  await fillAndSubmit(page);
  await page.getByTestId("tab-library").click();
  await page.getByTestId("view-list").click();
  await expect(page.getByTestId("item")).toHaveCount(2);

  // Leave and come back: still the list view.
  await page.getByTestId("tab-stats").click();
  await page.getByTestId("tab-library").click();
  await expect(page.getByTestId("item")).toHaveCount(2);
  await expect(page.getByTestId("shelf")).toHaveCount(0);
});

test("long-press sheet: right-click a tile opens the book actions", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.goto("/");
  await fillAndSubmit(page);
  await page.getByTestId("tab-library").click();

  await page.getByTestId("tile").first().click({ button: "right" });
  await expect(page.getByTestId("sheet")).toBeVisible();
  await expect(page.getByTestId("sheet-open")).toBeVisible();
  await expect(page.getByTestId("sheet-stats")).toBeVisible();
  await expect(page.getByTestId("sheet-hide")).toBeVisible();
  await expect(page.getByTestId("sheet-remove")).toBeVisible();
  await page.getByTestId("sheet-cancel").click();
  await expect(page.getByTestId("sheet")).toHaveCount(0);
});

test("book sheet: View stats shows the per-series numbers", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, STATS_ROWS);
  await page.goto("/");
  await fillAndSubmit(page);
  await page.getByTestId("tab-library").click();

  await page.getByTestId("tile").first().click({ button: "right" });
  await page.getByTestId("sheet-stats").click();
  const facts = page.getByTestId("sheet-fact");
  await expect(facts.filter({ hasText: "Chapters" })).toContainText("3 finished · 3 tracked");
  await expect(facts.filter({ hasText: "Status" })).toContainText("Completed");
});

test("book sheet: Remove deletes the synced rows and drops the card", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  const deletes = [];
  await page.route("**/rest/v1/reading_progress**", (route) => {
    if (route.request().method() === "DELETE") {
      deletes.push(route.request().url());
      return route.fulfill({ status: 204, body: "" });
    }
    return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(ROWS) });
  });
  await page.goto("/");
  await fillAndSubmit(page);
  await page.getByTestId("tab-library").click();

  await expect(page.getByTestId("tile")).toHaveCount(2);
  await page.getByTestId("tile").first().click({ button: "right" });
  await page.getByTestId("sheet-remove").click();
  await page.getByTestId("sheet-confirm-remove").click();

  await expect(page.getByTestId("tile")).toHaveCount(1);
  await expect(page.getByTestId("tile").first()).toContainText("Naruto");
  await expect
    .poll(() => deletes.some((u) => u.includes("chapter_key=like.One%20Piece")))
    .toBeTruthy();
});

test("book sheet works from the list view too", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.goto("/");
  await fillAndSubmit(page);
  await page.getByTestId("tab-library").click();
  await page.getByTestId("view-list").click();

  await page.getByTestId("item").first().locator("summary").click({ button: "right" });
  await expect(page.getByTestId("sheet")).toBeVisible();
  await page.getByTestId("sheet-hide").click();
  await expect(page.getByTestId("item")).toHaveCount(1);
});

test("chapters of the same series collapse to one card (most recent)", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, [
    { chapter_key: "One Piece/vol1.cbz", current_page: 19, total_pages: 20, updated_at: new Date(Date.now() - 5 * 86400e3).toISOString() },
    { chapter_key: "One Piece/vol3.cbz", current_page: 5, total_pages: 20, updated_at: new Date(Date.now() - 3600e3).toISOString() },
    { chapter_key: "Naruto/vol1.cbz", current_page: 2, total_pages: 18, updated_at: new Date(Date.now() - 2 * 86400e3).toISOString() },
  ]);
  await page.goto("/");
  await fillAndSubmit(page);
  await page.getByTestId("tab-library").click();
  await page.getByTestId("view-list").click();

  const items = page.getByTestId("item");
  await expect(items).toHaveCount(2);
  await expect(items.nth(0)).toContainText("One Piece");
  await expect(items.nth(0)).toContainText("vol3");
  await expect(items.nth(1)).toContainText("Naruto");
});

test("progress bar width tracks percent read", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.goto("/");
  await fillAndSubmit(page);
  await page.getByTestId("tab-library").click();
  await page.getByTestId("view-list").click();
  const bar = page.getByTestId("item").nth(0).locator("summary .bar > i");
  await expect(bar).toHaveAttribute("style", /width:\s*55%/);
});

test("tapping a series card expands its chapter list", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, [
    { chapter_key: "One Piece/vol1.cbz", current_page: 19, total_pages: 20, updated_at: new Date(Date.now() - 5 * 86400e3).toISOString() },
    { chapter_key: "One Piece/vol3.cbz", current_page: 5, total_pages: 20, updated_at: new Date(Date.now() - 3600e3).toISOString() },
  ]);
  await page.goto("/");
  await fillAndSubmit(page);
  await page.getByTestId("tab-library").click();
  await page.getByTestId("view-list").click();

  const card = page.getByTestId("item").first();
  const chapters = card.getByTestId("chapters");
  await expect(chapters).toBeHidden();
  await card.locator("summary").click();
  await expect(chapters).toBeVisible();
  const subs = chapters.locator(".sub-title");
  await expect(subs).toHaveText(["vol1", "vol3"]);
});

// --- reader ---------------------------------------------------------------

const READER_PAGES = ["p1", "p2", "p3", "p4", "p5"].map((s) => `https://cdn.test/${s}.png`);

// Opens the reader from the stats "recently read" list (a chapter row), which
// is the default view after sign-in.
async function openReader(page, { pages = READER_PAGES, currentPage = 0 } = {}) {
  await mockAuthOk(page);
  await mockProgress(page, [
    {
      chapter_key: "Vagabond/ch1.cbz",
      current_page: currentPage,
      total_pages: pages.length,
      updated_at: new Date().toISOString(),
    },
  ]);
  await page.route("**/rest/v1/chapter_pages**", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(pages.length ? [{ page_urls: pages }] : []),
    })
  );
  await page.goto("/");
  await fillAndSubmit(page);
  await page.getByTestId("chapter").first().click(); // recently-read → reader
}

test("reading a chapter shows its pages and navigates", async ({ page }) => {
  await openReader(page);
  const img = page.getByTestId("reader-img");
  await expect(page.getByTestId("reader-count")).toHaveText("1 / 5");
  await expect(img).toHaveAttribute("src", READER_PAGES[0]);

  await page.getByTestId("reader-next").click();
  await expect(page.getByTestId("reader-count")).toHaveText("2 / 5");
  await expect(img).toHaveAttribute("src", READER_PAGES[1]);

  await page.getByTestId("reader-prev").click();
  await expect(page.getByTestId("reader-count")).toHaveText("1 / 5");
  await expect(img).toHaveAttribute("src", READER_PAGES[0]);
});

test("the reader resumes at the saved page", async ({ page }) => {
  await openReader(page, { currentPage: 3 });
  await expect(page.getByTestId("reader-count")).toHaveText("4 / 5");
  await expect(page.getByTestId("reader-img")).toHaveAttribute("src", READER_PAGES[3]);
});

test("the reader pushes progress and Back returns to the dashboard", async ({ page }) => {
  const pushes = [];
  await page.route("**/rest/v1/rpc/upsert_progress", (route) => {
    pushes.push(JSON.parse(route.request().postData() || "{}"));
    route.fulfill({ status: 200, contentType: "application/json", body: "{}" });
  });
  await openReader(page);
  await page.getByTestId("reader-next").click(); // -> page index 1
  await page.getByTestId("reader-back").click();

  await expect(page.getByTestId("signout")).toBeVisible(); // back on the dashboard
  expect(
    pushes.some((p) => p.p_chapter_key === "Vagabond/ch1.cbz" && p.p_current_page === 1)
  ).toBeTruthy();
});

test("a chapter with no published pages shows an unavailable message", async ({ page }) => {
  await openReader(page, { pages: [] });
  await expect(page.getByText("isn't available to read on the web yet")).toBeVisible();
});

test("a persisted session skips the sign-in screen", async ({ page }) => {
  await mockProgress(page, ROWS);
  await page.addInitScript(() => {
    localStorage.setItem(
      "gideon.session",
      JSON.stringify({
        access_token: "persisted",
        refresh_token: "r",
        email: "reader@example.com",
        expires_at: Math.floor(Date.now() / 1000) + 3600,
      })
    );
  });
  await page.goto("/");
  await expect(page.getByTestId("heatmap")).toBeVisible();
  await page.getByTestId("tab-library").click();
  await expect(page.getByTestId("tile")).toHaveCount(2);
});

// --- UI regression (visual snapshots) -------------------------------------

test("sign-in screen looks right", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Your reading, everywhere" })).toBeVisible();
  await expect(page).toHaveScreenshot("signin.png", { maxDiffPixelRatio: 0.02 });
});

test("stats dashboard looks right", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, STATS_ROWS);
  await page.goto("/");
  await fillAndSubmit(page);
  await expect(page.getByTestId("heatmap")).toBeVisible();
  await expect(page).toHaveScreenshot("stats.png", { maxDiffPixelRatio: 0.02 });
});

test("library tab looks right", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.goto("/");
  await fillAndSubmit(page);
  await page.getByTestId("tab-library").click();
  await expect(page.getByTestId("tile").first()).toBeVisible();
  await expect(page).toHaveScreenshot("library.png", { maxDiffPixelRatio: 0.02 });
});

test("library list view looks right", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.goto("/");
  await fillAndSubmit(page);
  await page.getByTestId("tab-library").click();
  await page.getByTestId("view-list").click();
  await expect(page.getByTestId("item").first()).toBeVisible();
  await expect(page).toHaveScreenshot("library-list.png", { maxDiffPixelRatio: 0.02 });
});
