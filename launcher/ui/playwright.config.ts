import { defineConfig } from "@playwright/test";

const port = process.env.ODDITY_PREVIEW_PORT || "4173";
const baseURL = `http://127.0.0.1:${port}`;

export default defineConfig({
  testDir: "./tests/e2e",
  testMatch: "**/*.spec.ts",
  // CI 上给动画/时序偶发抖动一次重试，失败时保留现场。
  retries: process.env.CI ? 1 : 0,
  use: {
    baseURL,
    viewport: { width: 1180, height: 760 },
    trace: "retain-on-failure",
  },
  webServer: {
    command: `pnpm run preview --port ${port} --strictPort`,
    url: baseURL,
    reuseExistingServer: false,
  },
});
