import type {
  Feedback,
  GamepadDevice,
  GamepadOptions,
  GamepadSnapshot,
} from "../types/gamepad";

export type GamepadReader = () => (Gamepad | null)[];

const readNavigatorGamepads: GamepadReader = () =>
  typeof navigator.getGamepads === "function"
    ? Array.from(navigator.getGamepads())
    : [];

// 浏览器按 W3C 标准序号上报按键, 这里换算成 SDL 的 GamepadButton 序号,
// 让浏览器预览与桌面端共用同一套手柄导航.
const SDL_BUTTONS: [standard: number, sdl: number][] = [
  [0, 0],
  [1, 1],
  [2, 2],
  [3, 3],
  [4, 9],
  [5, 10],
  [8, 4],
  [9, 6],
  [10, 7],
  [11, 8],
  [12, 11],
  [13, 12],
  [14, 13],
  [15, 14],
  [16, 5],
];

const FAMILIES: [RegExp, GamepadDevice["family"]][] = [
  [/dualsense|054c.*0ce6/i, "dualsense"],
  // "Wireless Controller" 是 PS4 的通用名称, 但 Xbox 手柄也常带同样的词, 因此按厂商号判断.
  [/dualshock|054c|ps4/i, "ds4"],
  [/nintendo|switch|joy-?con|057e/i, "switch"],
  [/xbox|045e/i, "xbox"],
];

export function browserGamepadFamily(id: string): GamepadDevice["family"] {
  for (const [pattern, family] of FAMILIES) if (pattern.test(id)) return family;
  return "generic";
}

// Chrome 的 id 末尾附带厂商与产品号, 界面只显示设备名称.
export function browserGamepadName(id: string): string {
  const name = id
    .replace(/\s*\([^()]*(?:STANDARD GAMEPAD|Vendor:)[^()]*\)\s*$/i, "")
    .trim();
  return name || "手柄";
}

function toDevice(pad: Gamepad): GamepadDevice {
  const family = browserGamepadFamily(pad.id);
  const actuator = pad.vibrationActuator as
    GamepadHapticActuator | null | undefined;
  return {
    id: pad.index,
    name: browserGamepadName(pad.id),
    family,
    rumble: Boolean(actuator),
    // 浏览器无法使用扳机振动与自适应扳机, 音频触觉同理.
    triggerRumble: false,
    adaptive: false,
    audio: false,
  };
}

function toAxes(
  pad: Gamepad,
): [number, number, number, number, number, number] {
  const [leftX = 0, leftY = 0, rightX = 0, rightY = 0] = pad.axes;
  // 标准映射的扳机是按键值, 换到与桌面端一致的最后两个轴.
  const leftTrigger = pad.buttons[6]?.value ?? 0;
  const rightTrigger = pad.buttons[7]?.value ?? 0;
  return [leftX, leftY, rightX, rightY, leftTrigger, rightTrigger];
}

export interface BrowserGamepadSource {
  poll(focused: boolean): GamepadSnapshot;
}

export function createBrowserGamepadSource(
  readPads: GamepadReader = readNavigatorGamepads,
): BrowserGamepadSource {
  let active: number | undefined;
  let previous = new Set<number>();
  let previousFocused = false;
  return {
    poll(currentlyFocused: boolean): GamepadSnapshot {
      const pads = readPads().filter((pad): pad is Gamepad =>
        Boolean(pad?.connected),
      );
      const devices = pads.map(toDevice);
      const pad = pads.find((item) => item.index === active) ?? pads[0];
      const focused = currentlyFocused && Boolean(pad);
      const held = new Set<number>();
      let axes: GamepadSnapshot["axes"] = [0, 0, 0, 0, 0, 0];
      if (pad && focused) {
        for (const [standard, sdl] of SDL_BUTTONS)
          if (pad.buttons[standard]?.pressed) held.add(sdl);
        axes = toAxes(pad);
      }
      const switched = pad?.index !== active;
      // 刚获得焦点或刚切换手柄时不把按住的键当成新的按下.
      const pressed =
        focused && previousFocused && !switched
          ? [...held].filter((button) => !previous.has(button))
          : [];
      previous = held;
      previousFocused = focused;
      active = pad?.index;
      return {
        idle: { requested: false, error: null },
        devices,
        device: pad ? toDevice(pad) : null,
        buttons: focused ? [...held].sort((left, right) => left - right) : [],
        pressed,
        axes: focused ? axes : [0, 0, 0, 0, 0, 0],
        focused,
        error: null,
      };
    },
  };
}

export function browserGamepadFeedback(
  deviceId: number,
  effect: Feedback | "stop",
  options: GamepadOptions,
  readPads: GamepadReader = readNavigatorGamepads,
): Promise<void> {
  if (effect !== "stop" && (!options.vibration || options.strength <= 0))
    return Promise.resolve();
  const actuator = readPads().find((pad) => pad?.index === deviceId)
    ?.vibrationActuator as GamepadHapticActuator | null | undefined;
  if (!actuator) return Promise.resolve();
  if (effect === "stop") {
    return Promise.resolve(actuator.reset()).then(
      () => {},
      () => {},
    );
  }
  const strength = Math.min(1, Math.max(0, options.strength / 100));
  const duration =
    effect === "confirm" ? 90 : effect === "complete" ? 220 : 140;
  return Promise.resolve(
    actuator.playEffect("dual-rumble", {
      duration,
      startDelay: 0,
      strongMagnitude: effect === "error" ? strength : strength * 0.8,
      weakMagnitude: effect === "error" ? strength * 0.9 : strength * 0.5,
    }),
  ).then(
    () => {},
    () => {},
  );
}
