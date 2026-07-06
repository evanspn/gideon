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
  // One Piece: page 11 of 20 -> 55%.
  const bar = page.getByTestId("item").nth(0).locator(".bar > i");
  await expect(bar).toHaveAttribute("style", /width:\s*55%/);
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
