import { describe, expect, it, vi } from "vitest";
import {
  browserGamepadFamily,
  browserGamepadFeedback,
  browserGamepadName,
  createBrowserGamepadSource,
} from "../../src/core/gamepadBrowser";
import { defaultGamepadOptions } from "../../src/types/gamepad";

function gamepad(overrides: Partial<Gamepad> = {}) {
  return {
    id: "Xbox Wireless Controller (STANDARD GAMEPAD Vendor: 045e Product: 0b13)",
    index: 0,
    connected: true,
    mapping: "standard",
    timestamp: 0,
    axes: [0, 0, 0, 0],
    buttons: Array.from({ length: 17 }, () => ({
      pressed: false,
      touched: false,
      value: 0,
    })),
    ...overrides,
  } as unknown as Gamepad;
}

function hold(pad: Gamepad, index: number, value = 1) {
  (pad.buttons as GamepadButton[])[index] = {
    pressed: value > 0,
    touched: value > 0,
    value,
  } as GamepadButton;
}

describe("浏览器手柄适配", () => {
  it("识别主流手柄并清理浏览器附加的厂商标记", () => {
    expect(
      browserGamepadFamily(
        "DualSense Wireless Controller (STANDARD GAMEPAD Vendor: 054c Product: 0ce6)",
      ),
    ).toBe("dualsense");
    expect(
      browserGamepadFamily(
        "Wireless Controller (STANDARD GAMEPAD Vendor: 054c Product: 09cc)",
      ),
    ).toBe("ds4");
    expect(
      browserGamepadFamily(
        "Pro Controller (STANDARD GAMEPAD Vendor: 057e Product: 2009)",
      ),
    ).toBe("switch");
    expect(
      browserGamepadFamily("Xbox 360 Controller (XInput STANDARD GAMEPAD)"),
    ).toBe("xbox");
    expect(browserGamepadFamily("USB Gamepad")).toBe("generic");
    expect(
      browserGamepadName(
        "DualSense Wireless Controller (STANDARD GAMEPAD Vendor: 054c Product: 0ce6)",
      ),
    ).toBe("DualSense Wireless Controller");
    expect(
      browserGamepadName("Xbox 360 Controller (XInput STANDARD GAMEPAD)"),
    ).toBe("Xbox 360 Controller");
    expect(browserGamepadName("USB Gamepad (Vendor: 1234 Product: abcd)")).toBe(
      "USB Gamepad",
    );
    expect(browserGamepadName("(STANDARD GAMEPAD)")).toBe("手柄");
  });

  it("把标准按键、摇杆与扳机换算成桌面端快照", () => {
    const pad = gamepad({
      vibrationActuator: { playEffect: vi.fn(), reset: vi.fn() },
    });
    const source = createBrowserGamepadSource(() => [pad]);
    hold(pad, 0);
    hold(pad, 12);
    hold(pad, 6, 0.4);
    hold(pad, 7, 0.8);
    const axes = pad.axes as number[];
    axes[0] = 0.5;
    axes[1] = -0.25;
    axes[2] = 0.75;
    axes[3] = 0.9;
    const first = source.poll(true);
    expect(first.focused).toBe(true);
    expect(first.device).toMatchObject({
      id: 0,
      name: "Xbox Wireless Controller",
      family: "xbox",
      rumble: true,
      triggerRumble: false,
      adaptive: false,
      audio: false,
    });
    expect(first.devices).toHaveLength(1);
    expect(first.buttons).toEqual([0, 11]);
    // 首次采样不把已经按住的键当成新的按下.
    expect(first.pressed).toEqual([]);
    expect(first.axes).toEqual([0.5, -0.25, 0.75, 0.9, 0.4, 0.8]);
    hold(pad, 1);
    expect(source.poll(true).pressed).toEqual([1]);
  });

  it("丢失焦点时清空输入, 重新聚焦后从当前状态继续", () => {
    const pad = gamepad();
    const source = createBrowserGamepadSource(() => [pad]);
    hold(pad, 0);
    source.poll(true);
    hold(pad, 1);
    expect(source.poll(true).pressed).toEqual([1]);
    expect(source.poll(false)).toMatchObject({
      focused: false,
      buttons: [],
      axes: [0, 0, 0, 0, 0, 0],
      pressed: [],
    });
    hold(pad, 2);
    expect(source.poll(true).pressed).toEqual([]);
    hold(pad, 3);
    expect(source.poll(true).pressed).toEqual([3]);
  });

  it("手柄切换或断开后重选活动手柄", () => {
    const first = gamepad();
    const second = gamepad({
      index: 1,
      id: "DualSense Wireless Controller (STANDARD GAMEPAD Vendor: 054c Product: 0ce6)",
    });
    let pads: (Gamepad | null)[] = [first, second];
    const source = createBrowserGamepadSource(() => pads);
    expect(source.poll(true).device?.id).toBe(0);
    hold(second, 0);
    pads = [second];
    const switched = source.poll(true);
    expect(switched.device?.id).toBe(1);
    expect(switched.device?.family).toBe("dualsense");
    expect(switched.pressed).toEqual([]);
    hold(second, 1);
    expect(source.poll(true).pressed).toEqual([1]);
    pads = [];
    expect(source.poll(true)).toMatchObject({
      devices: [],
      device: null,
      focused: false,
      error: null,
    });
  });

  it("按设备能力发送与停止浏览器触觉反馈", async () => {
    const playEffect = vi.fn().mockResolvedValue("complete");
    const reset = vi.fn().mockResolvedValue("complete");
    const pad = gamepad({ vibrationActuator: { playEffect, reset } });
    const options = defaultGamepadOptions();
    await browserGamepadFeedback(0, "confirm", options, () => [pad]);
    expect(playEffect).toHaveBeenCalledWith(
      "dual-rumble",
      expect.objectContaining({ duration: 90, strongMagnitude: 0.8 }),
    );
    await browserGamepadFeedback(0, "complete", options, () => [pad]);
    expect(playEffect).toHaveBeenLastCalledWith(
      "dual-rumble",
      expect.objectContaining({ duration: 220 }),
    );
    await browserGamepadFeedback(0, "error", options, () => [pad]);
    expect(playEffect).toHaveBeenLastCalledWith(
      "dual-rumble",
      expect.objectContaining({
        duration: 140,
        strongMagnitude: 1,
        weakMagnitude: 0.9,
      }),
    );
    await browserGamepadFeedback(0, "stop", options, () => [pad]);
    expect(reset).toHaveBeenCalled();
    playEffect.mockClear();
    await browserGamepadFeedback(
      0,
      "confirm",
      { ...options, vibration: false },
      () => [pad],
    );
    await browserGamepadFeedback(
      0,
      "confirm",
      { ...options, strength: 0 },
      () => [pad],
    );
    expect(playEffect).not.toHaveBeenCalled();
    await expect(
      browserGamepadFeedback(9, "confirm", options, () => [pad]),
    ).resolves.toBeUndefined();
    await expect(
      browserGamepadFeedback(0, "confirm", options, () => [gamepad()]),
    ).resolves.toBeUndefined();
    playEffect.mockRejectedValueOnce(new Error("blocked"));
    await expect(
      browserGamepadFeedback(0, "confirm", options, () => [pad]),
    ).resolves.toBeUndefined();
    reset.mockRejectedValueOnce(new Error("blocked"));
    await expect(
      browserGamepadFeedback(0, "stop", options, () => [pad]),
    ).resolves.toBeUndefined();
  });

  it("默认从浏览器的 Gamepad API 读取设备", () => {
    const source = createBrowserGamepadSource();
    expect(source.poll(true)).toMatchObject({ devices: [], device: null });
    const pad = gamepad();
    const original = Object.getOwnPropertyDescriptor(navigator, "getGamepads");
    Object.defineProperty(navigator, "getGamepads", {
      configurable: true,
      value: () => [pad, null],
    });
    try {
      expect(source.poll(true).device?.id).toBe(0);
      expect(
        createBrowserGamepadSource(() => [pad]).poll(true).devices,
      ).toHaveLength(1);
      expect(() =>
        createBrowserGamepadSource(() => [null]).poll(true),
      ).not.toThrow();
    } finally {
      if (original) Object.defineProperty(navigator, "getGamepads", original);
      else
        delete (navigator as unknown as { getGamepads?: unknown }).getGamepads;
    }
  });
});
