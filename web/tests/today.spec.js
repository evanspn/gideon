import { test, expect } from "@playwright/test";

// Today: the landing page, and the month calendar ported from the device's
// Today screen. The heatmap answers "how much, over months"; this answers
// "what was I reading, and for how many days running" — so a series read
// three evenings in a row is ONE bar three days wide.

const SESSION = {
  access_token: "test-access-token",
  refresh_token: "test-refresh-token",
  expires_in: 3600,
  user: { email: "reader@example.com" },
};

// A fixed month, so the grid and its wrap points are the same every run:
// March 2026 — the 1st is a Sunday, so week one is a single cell and the
// month needs six rows.
const MARCH = (day, hour = 12) => new Date(2026, 2, day, hour).toISOString();

const ROWS = [
  // Berserk on the 4th, 5th and 6th: one run, one bar three days wide.
  { chapter_key: "Berserk/vol01.cbz", current_page: 9, total_pages: 20, updated_at: MARCH(4) },
  { chapter_key: "Berserk/vol02.cbz", current_page: 9, total_pages: 20, updated_at: MARCH(5) },
  { chapter_key: "Berserk/vol03.cbz", current_page: 9, total_pages: 20, updated_at: MARCH(6) },
  // Vagabond on the 5th too: it has to stack, not overlap.
  { chapter_key: "Vagabond/vol01.cbz", current_page: 4, total_pages: 22, updated_at: MARCH(5) },
  // Monster from Sunday the 8th into Monday the 9th: the run wraps a week.
  { chapter_key: "Monster/vol01.cbz", current_page: 4, total_pages: 18, updated_at: MARCH(8) },
  { chapter_key: "Monster/vol02.cbz", current_page: 4, total_pages: 18, updated_at: MARCH(9) },
];

// A fixed clock, so "this month" IS March 2026 and the grid never shifts
// under the tests: Friday the 20th.
async function signIn(page, { rows = ROWS } = {}) {
  await page.clock.install({ time: new Date(2026, 2, 20, 12, 0, 0) });
  await page.route("**/auth/v1/**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(SESSION) })
  );
  await page.route("**/rest/v1/send_queue**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: "[]" })
  );
  await page.route("**/rest/v1/chapter_pages**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: "[]" })
  );
  await page.route("**/rest/v1/reading_progress**", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(rows) })
  );
  await page.route(/\/api\/mal\?path=/, (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: '{"data":[]}' })
  );
  await page.goto("/");
  await page.locator("input[type=email]").fill("reader@example.com");
  await page.locator("input[type=password]").fill("password123");
  await page.getByTestId("signin").click();
}

test("the landing page is Today: what you're mid-way through, then the month", async ({ page }) => {
  await signIn(page);

  await expect(page.getByTestId("tab-today")).toHaveClass(/on/);
  await expect(page.getByTestId("today-head")).toBeVisible();
  await expect(page.getByTestId("calendar")).toBeVisible();
  // The continue card is the most recently read chapter, and it opens it.
  const cont = page.getByTestId("today-continue");
  await expect(cont).toContainText("Monster");
  await page.getByTestId("continue").click();
  await expect(page.getByText("isn't available to read on the web yet")).toBeVisible();
});

test("a run of days is one bar, and a second series that day stacks", async ({ page }) => {
  await signIn(page);
  await expect(page.getByTestId("cal-month")).toHaveText("March 2026");

  const cal = page.getByTestId("calendar");
  // Berserk read on the 4th–6th: ONE bar, spanning Wednesday to Friday.
  const berserk = cal.getByTestId("cal-bar").filter({ hasText: "Berserk" });
  await expect(berserk).toHaveCount(1);
  await expect(berserk).toHaveAttribute("style", /grid-column:3\/6/);
  // Vagabond shares the 5th, so it takes the next lane down rather than
  // painting over Berserk.
  const vagabond = cal.getByTestId("cal-bar").filter({ hasText: "Vagabond" });
  await expect(vagabond).toHaveAttribute("style", /grid-column:4\/5/);
  await expect(vagabond).toHaveAttribute("style", /grid-row:2/);
  await expect(berserk).toHaveAttribute("style", /grid-row:1/);
});

test("a run across a week boundary reads as a continuation", async ({ page }) => {
  await signIn(page);
  await expect(page.getByTestId("cal-month")).toHaveText("March 2026");

  // Monster: Sunday the 8th (week one's last column) into Monday the 9th.
  const monster = page.getByTestId("cal-bar").filter({ hasText: "Monster" });
  await expect(monster).toHaveCount(2);
  // The first piece opens the run; the second is marked as continuing, so it
  // drops the accent edge instead of reading as a new book.
  await expect(monster.nth(0)).not.toHaveClass(/cont/);
  await expect(monster.nth(1)).toHaveClass(/cont/);
  await expect(monster.nth(0)).toHaveAttribute("style", /grid-column:7\/8/);
  await expect(monster.nth(1)).toHaveAttribute("style", /grid-column:1\/2/);
});

test("the month pages back and forth, and Today comes home", async ({ page }) => {
  await signIn(page);
  const heading = page.getByTestId("cal-month");
  await expect(heading).toHaveText("March 2026");

  await page.getByTestId("cal-prev").click();
  await expect(heading).toHaveText("February 2026");
  await page.getByTestId("cal-next").click();
  await page.getByTestId("cal-next").click();
  await expect(heading).toHaveText("April 2026");
  // Off the current month, a Today button appears and brings it back.
  await page.getByTestId("cal-today").click();
  await expect(heading).toHaveText("March 2026");
  await expect(page.getByTestId("cal-today")).toHaveCount(0);
});

test("a calendar bar opens where that series was left off", async ({ page }) => {
  await signIn(page);
  await expect(page.getByTestId("cal-month")).toHaveText("March 2026");

  await page.getByTestId("cal-bar").filter({ hasText: "Vagabond" }).first().click();
  await expect(page.getByTestId("reader-back")).toBeVisible();
  await expect(page.locator(".reader-title")).toContainText("Vagabond");
});

test("an empty month says so instead of showing a bare grid", async ({ page }) => {
  await signIn(page);
  await expect(page.getByTestId("cal-foot")).toContainText("Read on 5 days this month");
  await page.getByTestId("cal-prev").click();
  await expect(page.getByTestId("cal-foot")).toContainText("Nothing read this month yet");
});
