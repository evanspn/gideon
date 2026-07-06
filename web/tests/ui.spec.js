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

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => localStorage.clear());
});

// --- functionality --------------------------------------------------------

test("shows the sign-in form on first load", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Your reading, everywhere" })).toBeVisible();
  await expect(page.locator("input[type=email]")).toBeVisible();
  await expect(page.locator("input[type=password]")).toBeVisible();
  await expect(page.getByTestId("signin")).toBeVisible();
  await expect(page.getByTestId("create")).toBeVisible();
});

test("signing in shows the continue-reading list", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.goto("/");
  await fillAndSubmit(page);

  await expect(page.getByText("Continue reading")).toBeVisible();
  const items = page.getByTestId("item");
  await expect(items).toHaveCount(2);
  // Newest-first order + page shown as 1-based.
  await expect(items.nth(0)).toContainText("One Piece");
  await expect(items.nth(0)).toContainText("11/20");
  await expect(items.nth(1)).toContainText("Naruto");
  await expect(page.getByTestId("signout")).toBeVisible();
  await expect(page.getByText("reader@example.com")).toBeVisible();
});

test("chapters of the same series collapse to one card (most recent)", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, [
    {
      chapter_key: "One Piece/vol1.cbz",
      current_page: 19,
      total_pages: 20,
      updated_at: new Date(Date.now() - 5 * 86400e3).toISOString(),
    },
    {
      chapter_key: "One Piece/vol3.cbz",
      current_page: 5,
      total_pages: 20,
      updated_at: new Date(Date.now() - 3600e3).toISOString(),
    },
    {
      chapter_key: "Naruto/vol1.cbz",
      current_page: 2,
      total_pages: 18,
      updated_at: new Date(Date.now() - 2 * 86400e3).toISOString(),
    },
  ]);
  await page.goto("/");
  await fillAndSubmit(page);

  const items = page.getByTestId("item");
  // Two series -> two cards, even though One Piece has two chapters.
  await expect(items).toHaveCount(2);
  // The One Piece card shows its most-recent chapter (vol3), newest first.
  await expect(items.nth(0)).toContainText("One Piece");
  await expect(items.nth(0)).toContainText("vol3");
  await expect(items.nth(1)).toContainText("Naruto");
});

test("progress bar width tracks percent read", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.goto("/");
  await fillAndSubmit(page);
  // One Piece: page 11 of 20 -> 55%. Target the summary's bar (the expanded
  // list has its own per-chapter bars).
  const bar = page.getByTestId("item").nth(0).locator("summary .bar > i");
  await expect(bar).toHaveAttribute("style", /width:\s*55%/);
});

test("tapping a series card expands its chapter list", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, [
    {
      chapter_key: "One Piece/vol1.cbz",
      current_page: 19,
      total_pages: 20,
      updated_at: new Date(Date.now() - 5 * 86400e3).toISOString(),
    },
    {
      chapter_key: "One Piece/vol3.cbz",
      current_page: 5,
      total_pages: 20,
      updated_at: new Date(Date.now() - 3600e3).toISOString(),
    },
  ]);
  await page.goto("/");
  await fillAndSubmit(page);

  const card = page.getByTestId("item").first();
  const chapters = card.getByTestId("chapters");
  // Collapsed by default, revealed on tap.
  await expect(chapters).toBeHidden();
  await card.locator("summary").click();
  await expect(chapters).toBeVisible();
  // Both chapters of the series are listed, in natural order.
  const subs = chapters.locator(".sub-title");
  await expect(subs).toHaveText(["vol1", "vol3"]);
});

// --- reader ---------------------------------------------------------------

const READER_PAGES = ["p1", "p2", "p3", "p4", "p5"].map((s) => `https://cdn.test/${s}.png`);

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
  await page.getByTestId("item").first().locator("summary").click(); // expand
  await page.getByTestId("chapter").first().click(); // open the reader
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

test("the reader pushes progress and Back returns to the library", async ({ page }) => {
  const pushes = [];
  await page.route("**/rest/v1/rpc/upsert_progress", (route) => {
    pushes.push(JSON.parse(route.request().postData() || "{}"));
    route.fulfill({ status: 200, contentType: "application/json", body: "{}" });
  });
  await openReader(page);
  await page.getByTestId("reader-next").click(); // -> page index 1
  await page.getByTestId("reader-back").click();

  await expect(page.getByTestId("signout")).toBeVisible(); // back on the library
  expect(
    pushes.some((p) => p.p_chapter_key === "Vagabond/ch1.cbz" && p.p_current_page === 1)
  ).toBeTruthy();
});

test("a chapter with no published pages shows an unavailable message", async ({ page }) => {
  await openReader(page, { pages: [] });
  await expect(page.getByText("isn't available to read on the web yet")).toBeVisible();
});

test("empty progress shows the empty state", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, []);
  await page.goto("/");
  await fillAndSubmit(page);
  await expect(page.getByTestId("empty")).toContainText("No reading progress yet");
});

test("bad credentials show an error and stay on sign-in", async ({ page }) => {
  await mockAuthFail(page, "Invalid login credentials");
  await page.goto("/");
  // ≥6 chars so the field's own minlength validation passes and the request
  // actually goes out to be rejected.
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
  await expect(page.getByText("Continue reading")).toBeVisible();
  await expect(page.getByTestId("item")).toHaveCount(2);
});

// --- UI regression (visual snapshots) -------------------------------------

test("sign-in screen looks right", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Your reading, everywhere" })).toBeVisible();
  await expect(page).toHaveScreenshot("signin.png", { maxDiffPixelRatio: 0.02 });
});

test("library screen looks right", async ({ page }) => {
  await mockAuthOk(page);
  await mockProgress(page, ROWS);
  await page.goto("/");
  await fillAndSubmit(page);
  await expect(page.getByTestId("item").first()).toBeVisible();
  await expect(page).toHaveScreenshot("library.png", { maxDiffPixelRatio: 0.02 });
});
