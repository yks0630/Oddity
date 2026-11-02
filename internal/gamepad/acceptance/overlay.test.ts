import { afterEach, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import { nextTick } from "vue";
import GamepadOverlay from "../../../launcher/ui/src/components/GamepadOverlay.vue";
import { gamepad, gamepadTarget } from "../../../launcher/ui/src/core/gamepad";
import { gamepadActions } from "../../../launcher/ui/src/core/gamepadActions";
import { fileRequest } from "../../../launcher/ui/src/core/filePicker";
vi.mock("../../../launcher/ui/src/core/controller", () => ({
  form: { reduceMotion: true, theme: "light" },
  state: { page: "records" },
  launchPauseAvailable: { value: false },
}));
vi.mock("../../../launcher/ui/src/composables/useSceneMotion", async () => {
  const { ref } = await import("vue");
  return { playerExpanded: ref(false), sceneEntering: ref(false) };
});
vi.mock("../../../launcher/ui/src/core/gamepad", async () => {
  const { reactive, shallowRef, computed } = await import("vue");
  return {
    gamepad: reactive({
      active: true,
      pressing: false,
      navigation: false,
      notification: false,
      editing: null as HTMLInputElement | HTMLTextAreaElement | null,
    }),
    gamepadTarget: shallowRef<HTMLElement | null>(null),
    gamepadViewport: () => ({ top: 0, bottom: 760 }),
    gamepadFamily: computed(() => "dualsense"),
    gamepadGlyph: (button: string) => `ps-${button}`,
    confirmButton: computed(() => 0),
    backButton: computed(() => 1),
  };
});
afterEach(() => {
  delete (Element.prototype as unknown as Record<string, unknown>)
    .getAnimations;
});
it("静止时停止测量, 滚动、尺寸和过渡变化重新跟随, 浏览图表时收起确认图标", async () => {
  let resized!: ResizeObserverCallback;
  vi.stubGlobal(
    "ResizeObserver",
    class {
      constructor(callback: ResizeObserverCallback) {
        resized = callback;
      }
      observe() {}
      disconnect() {}
    },
  );
  let moving = false;
  Object.defineProperty(Element.prototype, "getAnimations", {
    configurable: true,
    value: () => (moving ? [{ playState: "running" }] : []),
  });
  document.body.innerHTML =
    '<section class="page is-active"><div data-pad-region></div></section>';
  const target = document.querySelector<HTMLElement>("[data-pad-region]")!;
  const bounds = vi
    .spyOn(target, "getBoundingClientRect")
    .mockReturnValue(new DOMRect(40, 200, 500, 100));
  gamepadTarget.value = target;
  gamepad.active = true;
  const wrapper = mount(GamepadOverlay, { attachTo: document.body });
  await vi.advanceTimersByTimeAsync(80);
  const measured = bounds.mock.calls.length;
  await vi.advanceTimersByTimeAsync(1000);
  expect(bounds).toHaveBeenCalledTimes(measured);
  target.parentElement!.style.transform = "translateY(20px)";
  await nextTick();
  await vi.advanceTimersByTimeAsync(40);
  expect(bounds.mock.calls.length).toBeGreaterThan(measured);
  const previous = bounds.mock.calls.length;
  moving = true;
  resized([], {} as ResizeObserver);
  await vi.advanceTimersByTimeAsync(80);
  expect(bounds.mock.calls.length - previous).toBeGreaterThan(2);
  moving = false;
  await vi.advanceTimersByTimeAsync(40);
  const settled = bounds.mock.calls.length;
  await vi.advanceTimersByTimeAsync(1000);
  expect(bounds).toHaveBeenCalledTimes(settled);
  document.dispatchEvent(new Event("scroll"));
  await vi.advanceTimersByTimeAsync(40);
  expect(bounds).toHaveBeenCalledTimes(settled + 1);
  target.setAttribute("data-pad-browsing", "true");
  await nextTick();
  await vi.advanceTimersByTimeAsync(40);
  expect(document.querySelector(".gamepad-select__key")).toBeNull();
  wrapper.unmount();
  const unmounted = bounds.mock.calls.length;
  document.dispatchEvent(new Event("scroll"));
  await vi.advanceTimersByTimeAsync(100);
  expect(bounds).toHaveBeenCalledTimes(unmounted);
});

it("容器位移动画期间按稳定位置定位, 非位移过渡仍逐帧跟随", async () => {
  let animations: Animation[] = [];
  Object.defineProperty(Element.prototype, "getAnimations", {
    configurable: true,
    value: () => animations,
  });
  const animation = (keyframes: Record<string, unknown>[]) =>
    ({
      playState: "running",
      effect: {
        getKeyframes: () => keyframes,
        getComputedTiming: () => ({ endTime: 520 }),
      },
    }) as unknown as Animation;
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  document.body.innerHTML =
    '<section class="page is-active"><div data-pad-region></div><div class="drawer"><button id="first"></button><button id="second"></button></div></section>';
  const home = document.querySelector<HTMLElement>("[data-pad-region]")!;
  const drawer = document.querySelector<HTMLElement>(".drawer")!;
  const first = document.querySelector<HTMLElement>("#first")!;
  const second = document.querySelector<HTMLElement>("#second")!;
  vi.spyOn(home, "getBoundingClientRect").mockReturnValue(
    new DOMRect(40, 200, 500, 100),
  );
  // 抽屉滑入时整体右移 800px, 目标的稳定位置是 (400, 120).
  let sliding = true;
  vi.spyOn(first, "getBoundingClientRect").mockImplementation(() =>
    sliding ? new DOMRect(1200, 120, 152, 44) : new DOMRect(400, 120, 152, 44),
  );
  vi.spyOn(second, "getBoundingClientRect").mockReturnValue(
    new DOMRect(400, 300, 100, 40),
  );
  gamepadTarget.value = home;
  gamepad.active = true;
  const wrapper = mount(GamepadOverlay, { attachTo: document.body });
  await vi.advanceTimersByTimeAsync(200);
  const select = document.querySelector<HTMLElement>(".gamepad-select")!;
  expect(select.style.transform).toContain("translateX(35px)");

  // 滑入动画期间按稳定位置定位, 不等动画结束.
  drawer.style.transform = "matrix(1, 0, 0, 1, 800, 0)";
  animations = [animation([{ transform: "translateX(110%)" }])];
  gamepadTarget.value = first;
  await nextTick();
  await vi.advanceTimersByTimeAsync(200);
  expect(select.style.transform).toContain("translateX(395px)");
  expect(select.style.transform).toContain("translateY(116px)");

  // 动画结束后目标回到布局位置, 选中框不跳动.
  animations = [];
  sliding = false;
  drawer.style.transform = "matrix(1, 0, 0, 1, 0, 0)";
  document.dispatchEvent(new Event("scroll"));
  await vi.advanceTimersByTimeAsync(200);
  expect(select.style.transform).toContain("translateX(395px)");

  // 仅透明度过渡时仍按实时矩形定位.
  animations = [animation([{ opacity: 0 }])];
  gamepadTarget.value = second;
  await nextTick();
  await vi.advanceTimersByTimeAsync(200);
  expect(select.style.transform).toContain("translateY(296px)");

  wrapper.unmount();
});

it("屏幕键盘和文件选择框显示各自的快捷键提示", async () => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  document.body.innerHTML = '<section class="page is-active"></section>';
  const wrapper = mount(GamepadOverlay, { attachTo: document.body });

  gamepad.editing = document.createElement("input");
  gamepadActions.keyboard = {
    cycle: vi.fn(),
    commit: vi.fn(),
    backspace: vi.fn(),
    clear: vi.fn(),
  };
  await nextTick();
  expect(document.querySelector('[data-hint="select"]')?.textContent).toContain(
    "切换布局",
  );
  expect(document.querySelector('[data-hint="start"]')?.textContent).toContain(
    "保存",
  );
  expect(document.querySelector('[data-hint="west"]')?.textContent).toContain(
    "退格",
  );

  const numeric = document.createElement("input");
  numeric.type = "number";
  gamepad.editing = numeric;
  await nextTick();
  expect(document.querySelector('[data-hint="clear"]')?.textContent).toContain(
    "清空",
  );
  expect(document.querySelector('[data-hint="select"]')).toBeNull();

  gamepadActions.keyboard = null;
  gamepad.editing = null;
  const modal = document.createElement("div");
  modal.className = "modal";
  const files = document.createElement("div");
  files.className = "pad-files";
  const target = document.createElement("button");
  files.append(target);
  modal.append(files);
  document.body.append(modal);
  gamepadTarget.value = target;
  fileRequest.value = {
    title: "目录",
    directory: true,
    save: false,
    extensions: [],
    resolve: vi.fn(),
  };
  gamepadActions.picker = {
    primary: vi.fn(),
    parent: vi.fn(),
    reset: vi.fn(),
  };
  await nextTick();
  expect(document.querySelector('[data-hint="start"]')?.textContent).toContain(
    "选择",
  );
  expect(document.querySelector('[data-hint="west"]')?.textContent).toContain(
    "上一级",
  );
  expect(document.querySelector('[data-hint="reset"]')?.textContent).toContain(
    "重置",
  );

  wrapper.unmount();
  gamepadActions.picker = null;
  fileRequest.value = null;
  gamepadTarget.value = null;
});
