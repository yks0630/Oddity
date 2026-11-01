import { test, expect, type Page } from "@playwright/test";

function collectErrors(page: Page) {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("response", (response) => {
    if (response.status() >= 400)
      errors.push(`${response.status()} ${response.url()}`);
  });
  return errors;
}

test("浏览器演示首屏、说明弹窗、导航与启动流程", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("./");
  await expect(page.locator("#app")).toHaveClass(/is-intro-done/);

  const dialog = page.getByRole("dialog");
  await expect(dialog).toBeVisible();
  await expect(
    dialog.getByRole("heading", { name: "浏览器演示" }),
  ).toBeVisible();
  await expect(dialog.getByText("数据与操作均为模拟")).toBeVisible();
  await expect(
    dialog.getByRole("button", { name: "下载桌面版" }),
  ).toBeVisible();
  await expect(dialog.getByRole("button", { name: "查看源码" })).toBeVisible();
  await dialog.getByRole("button", { name: "继续体验" }).click();
  await expect(dialog).toHaveCount(0);
  await expect(page.locator(".modal")).toHaveCount(0);

  await expect(page.locator("#launch-btn")).toHaveText(/启动游戏/);
  await expect(page.locator("#setup-chip")).toBeHidden();
  await expect(page.locator(".demo-chip")).toBeVisible();
  await page.screenshot({ path: "test-results/demo-home.png" });

  await page.locator(".demo-chip").click();
  await expect(dialog).toBeVisible();
  await dialog.getByRole("button", { name: "关闭弹窗" }).click();
  await expect(dialog).toHaveCount(0);
  await expect(page.locator(".modal")).toHaveCount(0);

  for (const name of ["update", "bulletin", "records", "settings"]) {
    await page.locator(`[data-page="${name}"]`).click();
    await expect(page.locator(`#page-${name}`)).toBeVisible();
    if (name !== "bulletin") continue;
    const image = page.locator("#page-bulletin .bulletin-media img").first();
    await expect(image).toBeVisible();
    await expect
      .poll(() =>
        image.evaluate((element: HTMLImageElement) => element.naturalWidth),
      )
      .toBeGreaterThan(0);
  }

  await page.locator('[data-page="home"]').click();
  await expect(page.locator("#page-home")).toBeVisible();
  await page.locator("#launch-btn").click();
  await expect(page.locator("#launch-btn")).toHaveText(
    /正在(准备环境|启动后台|等待资源|启动游戏)/,
  );
  await expect(page.locator("#launch-btn")).toHaveText(/关闭游戏/, {
    timeout: 10_000,
  });
  await page.locator("#launch-btn").click();
  await expect(page.locator("#launch-btn")).toHaveText(/正在关闭/);
  await expect(page.locator("#launch-btn")).toHaveText(/启动游戏/);

  for (const width of [900, 390, 320]) {
    await page.setViewportSize({ width, height: 760 });
    await expect(page.locator(".demo-chip")).toBeVisible();
    expect(
      await page
        .locator(".titlebar")
        .evaluate((element) => element.scrollWidth <= element.clientWidth + 1),
    ).toBe(true);
  }
  await page.setViewportSize({ width: 1180, height: 760 });

  expect(errors).toEqual([]);
});

test("浏览器演示识别手柄并从子路径加载按键图标", async ({ page }) => {
  const errors = collectErrors(page);
  await page.addInitScript(() => {
    const buttons = Array.from({ length: 17 }, () => ({
      pressed: false,
      touched: false,
      value: 0,
    }));
    const pad = {
      id: "DualSense Wireless Controller (STANDARD GAMEPAD Vendor: 054c Product: 0ce6)",
      index: 0,
      connected: true,
      mapping: "standard",
      timestamp: 0,
      axes: [0, 0, 0, 0],
      buttons,
      vibrationActuator: {
        playEffect: () => Promise.resolve("complete"),
        reset: () => Promise.resolve("complete"),
      },
    };
    Object.defineProperty(navigator, "getGamepads", {
      configurable: true,
      value: () => [pad],
    });
    (
      window as unknown as { __hold: (index: number, pressed: boolean) => void }
    ).__hold = (index, pressed) => {
      const button = buttons[index]!;
      button.pressed = pressed;
      button.touched = pressed;
      button.value = pressed ? 1 : 0;
    };
  });
  await page.goto("./");
  await expect(page.locator("#app")).toHaveClass(/is-intro-done/);
  const dialog = page.getByRole("dialog");
  if (await dialog.isVisible()) {
    await dialog.getByRole("button", { name: "继续体验" }).click();
    await expect(page.locator(".modal")).toHaveCount(0);
  }
  const hold = (index: number, pressed: boolean) =>
    page.evaluate(
      ([buttonIndex, down]) =>
        (
          window as unknown as {
            __hold: (index: number, pressed: boolean) => void;
          }
        ).__hold(buttonIndex, down),
      [index, pressed] as const,
    );
  await hold(15, true);
  await expect(page.locator(".gamepad-hints")).toBeVisible();
  await hold(15, false);
  // 按键图标走子路径资源, 不能回落到域名根目录.
  await expect(page.locator(".gamepad-glyph use").first()).toHaveAttribute(
    "href",
    /\/oddity-demo\/assets\/gamepad\/prompts\.svg#/,
  );
  await page.screenshot({ path: "test-results/demo-gamepad.png" });
  expect(errors).toEqual([]);
});
