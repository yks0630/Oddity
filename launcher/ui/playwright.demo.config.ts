import { defineConfig } from "@playwright/test";

// 通过 vite preview 的 --base 模拟 GitHub Pages 子路径, 绝对路径问题会在演示用例中暴露。
const port = process.env.ODDITY_DEMO_PORT || "4175";
const baseURL = `http://127.0.0.1:${port}/oddity-demo/`;

export default defineConfig({
  testDir: "./tests/demo",
  testMatch: "**/*.spec.ts",
  use: {
    baseURL,
    viewport: { width: 1180, height: 760 },
  },
  webServer: {
    command: `pnpm run preview --port ${port} --strictPort --base=/oddity-demo/`,
    url: baseURL,
    reuseExistingServer: false,
  },
});
