import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { nextTick } from "vue";
vi.mock("../../../launcher/ui/src/core/controller", async () => {
  const { reactive } = await import("vue");
  const { defaultGamepadOptions } =
    await import("../../../launcher/ui/src/types/gamepad");
  const state = reactive({ page: "records", game: "idle" });
  return {
    state,
    form: reactive({ gamepad: defaultGamepadOptions(), reduceMotion: true }),
    showPage: vi.fn(),
    handleLaunchAction: vi.fn(),
    openSettings: vi.fn(),
  };
});
const bridge = vi.hoisted(() => ({
  native: false,
  invoke: vi.fn().mockResolvedValue(null),
}));
vi.mock("../../../launcher/ui/src/core/api", () => ({
  invoke: bridge.invoke,
  isDesktop: () => bridge.native,
}));
import {
  createGamepadNavigation,
  gamepad,
  gamepadTarget,
} from "../../../launcher/ui/src/core/gamepad";
import { playerExpanded } from "../../../launcher/ui/src/composables/useSceneMotion";
import { settingsNavigation } from "../../../launcher/ui/src/core/settingsCatalog";
let navigation: ReturnType<typeof createGamepadNavigation>;
let rects: Map<Element, DOMRect>;
function region(html: string) {
  document.body.innerHTML = html;
  document
    .querySelectorAll<HTMLElement>("button,input,textarea,.modal")
    .forEach((element, index) => {
      rects.set(element, new DOMRect(20, index * 60, 180, 40));
    });
}
beforeEach(() => {
  vi.clearAllMocks();
  bridge.native = false;
  bridge.invoke.mockResolvedValue(null);
  rects = new Map();
  vi.spyOn(Element.prototype, "getClientRects").mockImplementation(function (
    this: Element,
  ) {
    return rects.has(this)
      ? ([rects.get(this)!] as unknown as DOMRectList)
      : ([] as unknown as DOMRectList);
  });
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(
    function (this: Element) {
      return rects.get(this) ?? new DOMRect(0, 0, 180, 40);
    },
  );
  Element.prototype.scrollIntoView = vi.fn();
  gamepad.active = true;
  gamepad.deviceError = "";
  gamepad.feedbackError = "";
  gamepad.notification = false;
  gamepad.windowControls = false;
  gamepad.navigation = false;
  gamepad.editing = null;
  gamepadTarget.value = null;
  playerExpanded.value = false;
  settingsNavigation.category = "appearance";
  navigation = createGamepadNavigation();
});
afterEach(() => navigation.dispose());

describe("手柄下的多层弹窗", () => {
  it("按层号而非 DOM 顺序选择上层弹窗作为导航范围", async () => {
    // 先打开的海报弹窗在 DOM 中反而靠后, 上层保存弹窗排在前面.
    region(
      '<section class="page is-active" id="page-records"><button id="poster">生成海报</button></section>' +
        '<div class="modal" data-modal-layer="2" id="picker-modal"><section class="modal__panel"><button id="picker-close" aria-label="关闭弹窗">关闭</button><input id="picker-path" /><button id="picker-save">保存</button></section></div>' +
        '<div class="modal" data-modal-layer="1" id="poster-modal"><section class="modal__panel"><button id="poster-close" aria-label="关闭弹窗">关闭</button><button id="poster-cancel">取消</button><button id="poster-save">保存图片</button></section></div>',
    );
    navigation.refresh();
    expect(gamepadTarget.value?.id).toBe("picker-path");
    expect(gamepadTarget.value?.closest(".modal")?.id).toBe("picker-modal");
  });

  it("方向键与返回只作用于上层弹窗", async () => {
    region(
      '<section class="page is-active"><button id="poster">生成海报</button></section>' +
        '<div class="modal" data-modal-layer="2" id="picker-modal"><section class="modal__panel"><button id="picker-close" aria-label="关闭弹窗">关闭</button><button id="picker-one">前往</button><button id="picker-two">取消</button></section></div>' +
        '<div class="modal" data-modal-layer="1" id="poster-modal"><section class="modal__panel"><button id="poster-close" aria-label="关闭弹窗">关闭</button><button id="poster-cancel">取消</button></section></div>',
    );
    const picker = document.getElementById("picker-modal")!;
    rects.set(
      document.getElementById("picker-one")!,
      new DOMRect(20, 100, 140, 40),
    );
    rects.set(
      document.getElementById("picker-two")!,
      new DOMRect(180, 100, 140, 40),
    );
    const closed = vi.fn();
    picker.addEventListener("keydown", (event) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      closed();
    });
    navigation.refresh();
    expect(gamepadTarget.value?.id).toBe("picker-one");
    await navigation.press(14);
    expect(gamepadTarget.value?.id).toBe("picker-two");
    await navigation.press(1);
    expect(closed).toHaveBeenCalledTimes(1);
  });

  it("淡出中的弹窗不再接收导航", async () => {
    region(
      '<section class="page is-active"><button id="poster">生成海报</button></section>' +
        '<div class="modal" inert data-modal-layer="2" id="picker-modal"><section class="modal__panel"><button id="picker-save">保存</button></section></div>' +
        '<div class="modal" data-modal-layer="1" id="poster-modal"><section class="modal__panel"><button id="poster-close" aria-label="关闭弹窗">关闭</button><button id="poster-cancel">取消</button></section></div>',
    );
    navigation.refresh();
    await nextTick();
    navigation.refresh();
    expect(gamepadTarget.value?.closest(".modal")?.id).toBe("poster-modal");
  });
});
