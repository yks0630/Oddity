import { test, expect, type Page } from "@playwright/test";

type PadWindow = Window & {
  __hold: (index: number, pressed: boolean) => void;
};

function injectGamepad(page: Page) {
  return page.addInitScript(() => {
    const buttons = Array.from({ length: 17 }, () => ({
      pressed: false,
      touched: false,
      value: 0,
    }));
    const pad = {
      id: "Xbox Wireless Controller (STANDARD GAMEPAD Vendor: 045e Product: 0b13)",
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
    (window as unknown as { __hold: PadWindow["__hold"] }).__hold = (
      index,
      pressed,
    ) => {
      const button = pad.buttons[index]!;
      button.pressed = pressed;
      button.touched = pressed;
      button.value = pressed ? 1 : 0;
    };
  });
}

const hold = (page: Page, index: number, pressed: boolean) =>
  page.evaluate(
    ([buttonIndex, down]) =>
      (window as unknown as PadWindow).__hold(buttonIndex, down),
    [index, pressed] as const,
  );

test("浏览器预览识别标准手柄并支持导航", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await injectGamepad(page);
  await page.goto("/");
  await expect(page.locator("#app")).toHaveClass(/is-intro-done/);

  // D-pad 右: 进入手柄操作模式并出现按键提示.
  await hold(page, 15, true);
  await expect(page.locator(".gamepad-hints")).toBeVisible();
  await expect(page.locator("#app")).toHaveClass(/gamepad-mode/);
  await hold(page, 15, false);

  // Start: 直接打开手柄设置, 并显示已连接设备.
  await hold(page, 9, true);
  await expect(page.locator("#page-settings")).toBeVisible();
  await hold(page, 9, false);
  const settings = page.locator('[data-setting="gamepad"]');
  await expect(settings).toContainText("Xbox Wireless Controller");
  await expect(settings).toContainText("已连接");
  await page.screenshot({ path: "test-results/gamepad-browser.png" });

  expect(errors).toEqual([]);
});
