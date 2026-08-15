import { existsSync } from "node:fs";
import { defineConfig } from "@playwright/test";

// Regression tests for the static dashboard. Everything is mocked at the
// Supabase HTTP boundary (see tests/ui.spec.js), so these run fully offline
// against a local static server — no real backend, no network.
export default defineConfig({
  testDir: "./tests",
  fullyParallel: true,
  reporter: "list",
  // A fixed light theme + viewport so UI snapshots are deterministic.
  use: {
    baseURL: "http://127.0.0.1:3210",
    colorScheme: "light",
    viewport: { width: 420, height: 900 },
    // Cloud/CI images preinstall Chromium at a fixed path and export
    // PW_CHROMIUM (or rely on the /opt fallback). On a dev machine neither
    // exists — use Playwright's own managed browser (npx playwright install).
    launchOptions:
      process.env.PW_CHROMIUM || existsSync("/opt/pw-browsers/chromium")
        ? { executablePath: process.env.PW_CHROMIUM || "/opt/pw-browsers/chromium" }
        : {},
  },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
  webServer: {
    command: "python3 -m http.server 3210",
    port: 3210,
    reuseExistingServer: true,
  },
});
