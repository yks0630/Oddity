import { test, expect, type Page } from "@playwright/test";

async function center(page: Page, selector: string) {
  const box = await page.locator(selector).boundingBox();
  expect(box).not.toBeNull();
  return { x: box!.x + box!.width / 2, y: box!.y + box!.height / 2 };
}
async function openDrawer(page: Page, hash: string, selector: string) {
  await page.goto(`/#${hash}`);
  await expect(page.locator(selector)).toBeVisible();
  // 抽屉入场是 520ms 弹簧, 等落位后再取坐标, 否则拖动的起点会落在内容上。
  await page.waitForTimeout(650);
}
async function readyNotice(page: Page) {
  await page.goto("/");
  await expect(page.locator(".notification-stack")).toHaveAttribute(
    "data-phase",
    "0",
  );
  const point = await center(page, ".notification-message");
  await page.mouse.move(point.x, point.y);
  return point;
}

for (const direction of [-1, 1]) {
  test(`通知向${direction < 0 ? "左" : "右"}滑动关闭并直接离场`, async ({
    page,
  }) => {
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const point = await readyNotice(page);
    await page.mouse.down();
    await page.mouse.move(point.x + direction * 125, point.y + 2, {
      steps: 12,
    });
    await page.mouse.up();
    await expect(page.locator(".notification-slot")).toHaveCount(0);
    expect(errors).toEqual([]);
  });
}

test("通知短拖回弹、垂直拖动与指针取消保留通知", async ({ page }) => {
  const point = await readyNotice(page);
  const card = page.locator(".notification-card");
  const original = await card.boundingBox();
  await page.mouse.down();
  await page.mouse.move(point.x + 35, point.y, { steps: 8 });
  await page.waitForTimeout(120);
  await page.mouse.up();
  await expect
    .poll(async () => Math.abs((await card.boundingBox())!.x - original!.x))
    .toBeLessThan(1);
  await page.mouse.move(point.x, point.y);
  await page.mouse.down();
  await page.mouse.move(point.x + 10, point.y + 70, { steps: 10 });
  await page.mouse.up();
  await expect(card).toHaveCount(1);
  await page.mouse.move(point.x, point.y);
  await page.mouse.down();
  await page.mouse.move(point.x + 75, point.y, { steps: 8 });
  await page
    .locator(".notification-swipe")
    .dispatchEvent("pointercancel", { pointerId: 1 });
  await page.mouse.up();
  await expect
    .poll(async () => Math.abs((await card.boundingBox())!.x - original!.x))
    .toBeLessThan(1);
});

test("按住导航连续跨图标、缝隙与反向滑动，松手不回跳", async ({ page }) => {
  await page.goto("/");
  const home = await center(page, '.rail-item[data-page="home"]');
  const update = await center(page, '.rail-item[data-page="update"]');
  const settings = await center(page, '.rail-item[data-page="settings"]');
  await page.mouse.move(home.x, home.y);
  await page.mouse.down();
  await page.mouse.move(update.x, update.y, { steps: 8 });
  await expect(page.locator('[data-page="update"]')).toHaveAttribute(
    "aria-current",
    "page",
  );
  await page.mouse.move(settings.x, settings.y, { steps: 16 });
  await expect(page.locator('[data-page="settings"]')).toHaveAttribute(
    "aria-current",
    "page",
  );
  await expect(page.locator(".rail__nav")).toHaveAttribute(
    "data-scrubbing",
    "true",
  );
  await page.mouse.move(update.x, update.y, { steps: 16 });
  await page.mouse.up();
  await expect(page.locator('[data-page="update"]')).toHaveAttribute(
    "aria-current",
    "page",
  );
  await expect(page.locator('[data-page="update"]')).toBeFocused();
  await page.locator('[data-page="settings"]').click();
  await expect(page.locator('[data-page="settings"]')).toHaveAttribute(
    "aria-current",
    "page",
  );
});

test("导航可以从设置拖回主页并关闭抽屉", async ({ page }) => {
  await page.goto("/#settings");
  const home = await center(page, '.rail-item[data-page="home"]');
  const settings = await center(page, '.rail-item[data-page="settings"]');
  await expect(page.locator("#page-settings")).toBeVisible();
  await page.mouse.move(settings.x, settings.y);
  await page.mouse.down();
  await page.mouse.move(home.x, home.y, { steps: 20 });
  await expect(page.locator('[data-page="home"]')).toHaveAttribute(
    "aria-current",
    "page",
  );
  await page.mouse.up();
  await expect(page.locator("#page-home")).toBeVisible();
  await expect(page.locator(".rail__nav")).toHaveAttribute(
    "data-scrubbing",
    "false",
  );
});

test("导航纵向移出、Escape 与指针取消撤销切换", async ({ page }) => {
  await page.goto("/");
  const home = await center(page, '.rail-item[data-page="home"]');
  const settings = await center(page, '.rail-item[data-page="settings"]');
  for (const cancel of ["outside", "escape", "pointer"] as const) {
    await page.mouse.move(home.x, home.y);
    await page.mouse.down();
    await page.mouse.move(settings.x, settings.y, { steps: 12 });
    await expect(page.locator('[data-page="settings"]')).toHaveAttribute(
      "aria-current",
      "page",
    );
    if (cancel === "outside")
      await page.mouse.move(settings.x, settings.y + 100, { steps: 8 });
    else if (cancel === "escape") await page.keyboard.press("Escape");
    else
      await page
        .locator(".rail__nav")
        .dispatchEvent("pointercancel", { pointerId: 1 });
    await page.mouse.up();
    await expect(page.locator('[data-page="home"]')).toHaveAttribute(
      "aria-current",
      "page",
    );
    await expect(page.locator(".rail__nav")).toHaveAttribute(
      "data-scrubbing",
      "false",
    );
  }
  await page.locator('[data-page="settings"]').focus();
  await page.keyboard.press("Enter");
  await expect(page.locator('[data-page="settings"]')).toHaveAttribute(
    "aria-current",
    "page",
  );
});

test("快速短甩可以关闭，按钮上的拖动不带动通知", async ({ page }) => {
  const point = await readyNotice(page);
  const card = page.locator(".notification-card");
  const original = await card.boundingBox();
  const close = await center(page, ".notification-dismiss");
  await page.mouse.move(close.x, close.y);
  await page.mouse.down();
  await page.mouse.move(close.x + 120, close.y, { steps: 8 });
  await page.mouse.up();
  await expect(card).toHaveCount(1);
  expect((await card.boundingBox())!.x).toBeCloseTo(original!.x, 0);
  await page.mouse.move(point.x, point.y);
  await page.mouse.down();
  await page.mouse.move(point.x + 35, point.y);
  await page.mouse.up();
  await expect(card).toHaveCount(0);
});

test("减少动效下展开通知可以逐条滑走，回弹后按钮仍可点击", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  const point = await readyNotice(page);
  await page.mouse.down();
  await page.mouse.move(point.x + 30, point.y, { steps: 5 });
  await page.waitForTimeout(120);
  await page.mouse.up();
  await page.locator(".notification-dismiss").click();
  await expect(page.locator(".notification-card")).toHaveCount(0);
  await page.locator("#launch-btn").click();
  await page.locator("#settings-redetect").click();
  await page.getByRole("button", { name: "展开 2 条通知" }).click();
  const slots = page.locator(".notification-slot");
  await expect
    .poll(async () => {
      const first = await slots.first().boundingBox();
      const last = await slots.last().boundingBox();
      return last!.y > first!.y + first!.height;
    })
    .toBe(true);
  const oldId = await slots.last().getAttribute("data-notification-id");
  const second = await center(
    page,
    `.notification-slot[data-notification-id="${oldId}"] .notification-message`,
  );
  await page.mouse.move(second.x, second.y);
  await page.mouse.down();
  await page.mouse.move(second.x + 125, second.y, { steps: 12 });
  await page.mouse.up();
  await expect(slots).toHaveCount(1);
  await expect(
    page.locator(`.notification-slot[data-notification-id="${oldId}"]`),
  ).toHaveCount(0);
});

test("导航键盘焦点连续移动，失焦会取消拖动", async ({ page }) => {
  await page.goto("/");
  const home = page.locator('[data-page="home"]');
  await home.focus();
  await page.keyboard.press("ArrowRight");
  await expect(page.locator('[data-page="update"]')).toBeFocused();
  await page.keyboard.press("End");
  await expect(page.locator('[data-page="settings"]')).toBeFocused();
  await page.keyboard.press("Home");
  await expect(home).toBeFocused();
  const start = await center(page, '[data-page="home"]');
  const end = await center(page, '[data-page="settings"]');
  await page.mouse.move(start.x, start.y);
  await page.mouse.down();
  await page.mouse.move(end.x, end.y, { steps: 12 });
  await page.dispatchEvent("body", "blur", { bubbles: true });
  await page.mouse.up();
  await expect(home).toHaveAttribute("aria-current", "page");
});

test("按住设置分类条拖动切类, Escape 取消回到原类", async ({ page }) => {
  await openDrawer(
    page,
    "settings",
    '#page-settings [data-category="appearance"]',
  );
  const nav = page.locator("#page-settings .settings-categories");
  const appearance = await center(
    page,
    '#page-settings [data-category="appearance"]',
  );
  const runtime = await center(
    page,
    '#page-settings [data-category="runtime"]',
  );
  const gamepad = await center(
    page,
    '#page-settings [data-category="gamepad"]',
  );
  await page.mouse.move(appearance.x, appearance.y);
  await page.mouse.down();
  await page.mouse.move(runtime.x, runtime.y, { steps: 14 });
  await expect(
    page.locator('#page-settings [data-category="runtime"]'),
  ).toHaveAttribute("aria-current", "true");
  await expect(nav).toHaveAttribute("data-scrubbing", "true");
  await page.mouse.up();
  await expect(page.locator("#settings-panel-runtime")).toBeVisible();
  await expect(nav).toHaveAttribute("data-scrubbing", "false");
  await expect(
    page.locator('#page-settings [data-category="runtime"]'),
  ).toBeFocused();

  await page.mouse.move(runtime.x, runtime.y);
  await page.mouse.down();
  await page.mouse.move(gamepad.x, gamepad.y, { steps: 10 });
  await expect(
    page.locator('#page-settings [data-category="gamepad"]'),
  ).toHaveAttribute("aria-current", "true");
  await page.keyboard.press("Escape");
  await page.mouse.up();
  await expect(
    page.locator('#page-settings [data-category="runtime"]'),
  ).toHaveAttribute("aria-current", "true");
  await expect(nav).toHaveAttribute("data-scrubbing", "false");
  await expect(page.locator("#settings-panel-runtime")).toBeVisible();
});

test("按住游戏模式分类条拖动切类", async ({ page }) => {
  await openDrawer(
    page,
    "game-mode",
    '#page-game-mode [data-category="general"]',
  );
  const general = await center(
    page,
    '#page-game-mode [data-category="general"]',
  );
  const gamescope = await center(
    page,
    '#page-game-mode [data-category="gamescope"]',
  );
  await page.mouse.move(general.x, general.y);
  await page.mouse.down();
  await page.mouse.move(gamescope.x, gamescope.y, { steps: 14 });
  await expect(
    page.locator('#page-game-mode [data-category="gamescope"]'),
  ).toHaveAttribute("aria-current", "true");
  await page.mouse.up();
  await expect(page.locator("#game-mode-panel-gamescope")).toBeVisible();
});

test("按住公告分类条拖动切换", async ({ page }) => {
  await openDrawer(page, "bulletin", '.bulletin-filters [data-value="all"]');
  const all = await center(page, '.bulletin-filters [data-value="all"]');
  const maintenance = await center(
    page,
    '.bulletin-filters [data-value="maintenance"]',
  );
  await page.mouse.move(all.x, all.y);
  await page.mouse.down();
  await page.mouse.move(maintenance.x, maintenance.y, { steps: 14 });
  await expect(
    page.locator('.bulletin-filters [data-value="maintenance"]'),
  ).toHaveAttribute("aria-selected", "true");
  await expect(page.locator(".bulletin-filters")).toHaveAttribute(
    "data-scrubbing",
    "true",
  );
  await page.mouse.up();
  await expect(page.locator(".bulletin-filters")).toHaveAttribute(
    "data-scrubbing",
    "false",
  );
});

test("搜索时设置分类条不进入拖动", async ({ page }) => {
  await openDrawer(
    page,
    "settings",
    '#page-settings [data-category="appearance"]',
  );
  const search = page.getByRole("searchbox", { name: "搜索设置" });
  await search.fill("路径");
  const appearance = await center(
    page,
    '#page-settings [data-category="appearance"]',
  );
  const runtime = await center(
    page,
    '#page-settings [data-category="runtime"]',
  );
  await page.mouse.move(appearance.x, appearance.y);
  await page.mouse.down();
  await page.mouse.move(runtime.x, runtime.y, { steps: 10 });
  await expect(
    page.locator("#page-settings .settings-categories"),
  ).toHaveAttribute("data-scrubbing", "false");
  await page.mouse.move(runtime.x + 240, runtime.y);
  await page.mouse.up();
  await expect(search).toHaveValue("路径");
  await expect(page.locator("#page-settings .settings-result")).toHaveCount(2);
});

test("触摸拖动设置分类条也能切类", async ({ browser }) => {
  const context = await browser.newContext({
    hasTouch: true,
    viewport: { width: 1180, height: 760 },
  });
  const page = await context.newPage();
  await page.goto(
    `${
      test.info().project.use.baseURL ??
      `http://127.0.0.1:${process.env.ODDITY_PREVIEW_PORT || "4173"}`
    }/#settings`,
  );
  await expect(
    page.locator('#page-settings [data-category="appearance"]'),
  ).toBeVisible();
  await page.waitForTimeout(650);
  const appearance = await center(
    page,
    '#page-settings [data-category="appearance"]',
  );
  const runtime = await center(
    page,
    '#page-settings [data-category="runtime"]',
  );
  const cdp = await context.newCDPSession(page);
  await cdp.send("Input.dispatchTouchEvent", {
    type: "touchStart",
    touchPoints: [{ ...appearance, id: 1 }],
  });
  for (let step = 1; step <= 14; step++) {
    await cdp.send("Input.dispatchTouchEvent", {
      type: "touchMove",
      touchPoints: [
        {
          x: appearance.x + ((runtime.x - appearance.x) * step) / 14,
          y: appearance.y + ((runtime.y - appearance.y) * step) / 14,
          id: 1,
        },
      ],
    });
  }
  await cdp.send("Input.dispatchTouchEvent", {
    type: "touchEnd",
    touchPoints: [],
  });
  await expect(
    page.locator('#page-settings [data-category="runtime"]'),
  ).toHaveAttribute("aria-current", "true");
  await context.close();
});

test("触摸手势保留通知组纵向滚动，并支持通知与导航横滑", async ({
  browser,
}) => {
  const context = await browser.newContext({
    hasTouch: true,
    viewport: { width: 1180, height: 260 },
  });
  const page = await context.newPage();
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.goto(
    test.info().project.use.baseURL ??
      `http://127.0.0.1:${process.env.ODDITY_PREVIEW_PORT || "4173"}`,
  );
  const cdp = await context.newCDPSession(page);
  async function touch(
    from: { x: number; y: number },
    to: { x: number; y: number },
  ) {
    await cdp.send("Input.dispatchTouchEvent", {
      type: "touchStart",
      touchPoints: [{ ...from, id: 1 }],
    });
    for (let step = 1; step <= 12; step++) {
      await cdp.send("Input.dispatchTouchEvent", {
        type: "touchMove",
        touchPoints: [
          {
            x: from.x + ((to.x - from.x) * step) / 12,
            y: from.y + ((to.y - from.y) * step) / 12,
            id: 1,
          },
        ],
      });
    }
    await cdp.send("Input.dispatchTouchEvent", {
      type: "touchEnd",
      touchPoints: [],
    });
  }
  const home = await center(page, '[data-page="home"]');
  const settings = await center(page, '[data-page="settings"]');
  await touch(home, settings);
  await expect(page.locator('[data-page="settings"]')).toHaveAttribute(
    "aria-current",
    "page",
  );
  await page.locator("#settings-redetect").click();
  await page.getByRole("button", { name: "展开 2 条通知" }).click();
  const stack = page.locator(".notification-stack");
  await expect
    .poll(() => stack.evaluate((el) => el.scrollHeight - el.clientHeight))
    .toBeGreaterThan(10);
  const first = page.locator(".notification-message").first();
  const firstBox = await first.boundingBox();
  const start = { x: firstBox!.x + 70, y: firstBox!.y + firstBox!.height / 2 };
  await touch(start, { x: start.x + 2, y: start.y - 90 });
  await expect
    .poll(() => stack.evaluate((el) => el.scrollTop))
    .toBeGreaterThan(0);
  await expect(page.locator(".notification-slot")).toHaveCount(2);
  const message = page.locator(".notification-message").last();
  await message.scrollIntoViewIfNeeded();
  const box = await message.boundingBox();
  const point = { x: box!.x + 80, y: box!.y + box!.height / 2 };
  await touch(point, { x: point.x + 130, y: point.y });
  await expect(page.locator(".notification-slot")).toHaveCount(1);
  await context.close();
});
