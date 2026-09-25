import { defineConfig, devices } from "@playwright/test";

// End-to-end tests against the production build, served with the snapshot
// in RECEIPTS_SNAPSHOT_DIR (see e2e/app.spec.ts).
export default defineConfig({
  testDir: "e2e",
  timeout: 180_000,
  expect: { timeout: 60_000 },
  reporter: [["list"]],
  use: { baseURL: "http://127.0.0.1:4173", screenshot: "only-on-failure" },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: {
    command: "npx vite build && npx vite preview --host 127.0.0.1 --port 4173 --strictPort",
    url: "http://127.0.0.1:4173",
    reuseExistingServer: false,
    timeout: 120_000,
  },
});
