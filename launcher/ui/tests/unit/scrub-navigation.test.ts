import { beforeEach, describe, expect, it, vi } from "vitest";
import { defineComponent, h, ref } from "vue";
import { flushPromises, mount } from "@vue/test-utils";
import { useScrubNavigation } from "../../src/composables/useScrubNavigation";

const setPointerCapture = vi.fn();
const releasePointerCapture = vi.fn();
const hasPointerCapture = vi.fn(() => true);
const scrollIntoView = vi.fn();

function rect(x: number, y: number, width: number, height: number) {
  return {
    x,
    y,
    left: x,
    top: y,
    width,
    height,
    right: x + width,
    bottom: y + height,
    toJSON: () => ({}),
  } as DOMRect;
}
function pointer(
  type: string,
  point: { x: number; y: number },
  overrides: { id?: number; button?: number; primary?: boolean } = {},
) {
  const event = new MouseEvent(type, {
    bubbles: true,
    cancelable: true,
    clientX: point.x,
    clientY: point.y,
    button: overrides.button ?? 0,
  });
  Object.defineProperty(event, "pointerId", { value: overrides.id ?? 1 });
  Object.defineProperty(event, "isPrimary", {
    value: overrides.primary ?? true,
  });
  return event as unknown as PointerEvent;
}
function harness(options: { orientation: "x" | "y"; enabled?: () => boolean }) {
  const values = ["a", "b", "c"];
  const selected = ref(values[0]);
  const scrubbing = ref(false);
  const root = ref<HTMLElement | null>(null);
  const component = defineComponent({
    setup() {
      const scrub = useScrubNavigation({
        root,
        item: "[data-value]",
        value: (element) => element.dataset.value,
        active: () => selected.value,
        select: (value) => {
          selected.value = value;
        },
        axis: "auto",
        enabled: options.enabled,
        focusOnFinish: true,
        scrollIntoView: true,
        scrubbing,
      });
      return () =>
        h(
          "div",
          {
            ref: root,
            onPointerdown: (event: Event) => scrub.down(event as PointerEvent),
            onLostpointercapture: (event: Event) =>
              scrub.lost(event as PointerEvent),
          },
          values.map((value, index) =>
            h(
              "button",
              {
                "data-value": value,
                onClick: (event: Event) => {
                  if (scrub.suppressed(event as MouseEvent)) return;
                  selected.value = value;
                },
              },
              `${value}${index}`,
            ),
          ),
        );
    },
  });
  const wrapper = mount(component, { attachTo: document.body });
  wrapper.findAll("button").forEach((button, index) => {
    vi.spyOn(button.element, "getBoundingClientRect").mockReturnValue(
      options.orientation === "x"
        ? rect(index * 100, 0, 80, 40)
        : rect(0, index * 60, 80, 40),
    );
  });
  vi.spyOn(wrapper.element, "getBoundingClientRect").mockReturnValue(
    options.orientation === "x" ? rect(0, 0, 280, 40) : rect(0, 0, 80, 200),
  );
  const buttons = wrapper.findAll("button");
  const at = (index: number) => {
    const box = buttons[index].element.getBoundingClientRect();
    return {
      x: box.left + box.width / 2,
      y: box.top + box.height / 2,
    };
  };
  const down = (index: number, overrides?: { primary?: boolean }) => {
    const event = pointer("pointerdown", at(index), overrides);
    buttons[index].element.dispatchEvent(event);
  };
  const move = (point: { x: number; y: number }) =>
    window.dispatchEvent(pointer("pointermove", point));
  const up = (point: { x: number; y: number }) =>
    window.dispatchEvent(pointer("pointerup", point));
  return { wrapper, selected, scrubbing, at, down, move, up, buttons };
}
beforeEach(() => {
  setPointerCapture.mockClear();
  releasePointerCapture.mockClear();
  hasPointerCapture.mockClear();
  scrollIntoView.mockClear();
  Object.defineProperty(Element.prototype, "setPointerCapture", {
    configurable: true,
    value: setPointerCapture,
  });
  Object.defineProperty(Element.prototype, "releasePointerCapture", {
    configurable: true,
    value: releasePointerCapture,
  });
  Object.defineProperty(Element.prototype, "hasPointerCapture", {
    configurable: true,
    value: hasPointerCapture,
  });
  Object.defineProperty(Element.prototype, "scrollIntoView", {
    configurable: true,
    value: scrollIntoView,
  });
});
describe("useScrubNavigation", () => {
  it("按住跨过选项切换, 松手提交并抑制补发的 click", async () => {
    const { wrapper, selected, scrubbing, at, down, move, up, buttons } =
      harness({ orientation: "x" });
    down(0);
    move({ x: at(1).x - 30, y: at(1).y });
    expect(scrubbing.value).toBe(true);
    expect(setPointerCapture).toHaveBeenCalledWith(1);
    move(at(2));
    expect(selected.value).toBe("c");
    up(at(2));
    await flushPromises();
    expect(scrubbing.value).toBe(false);
    expect(scrollIntoView).toHaveBeenCalled();
    expect(document.activeElement?.textContent).toBe("c2");
    const click = new MouseEvent("click", {
      bubbles: true,
      cancelable: true,
      detail: 1,
    });
    buttons[0].element.dispatchEvent(click);
    expect(click.defaultPrevented).toBe(true);
    expect(selected.value).toBe("c");
    wrapper.unmount();
  });
  it("按布局自动判断纵向分类条并沿纵向切换", async () => {
    const { wrapper, selected, at, down, move } = harness({
      orientation: "y",
    });
    down(0);
    move(at(2));
    expect(selected.value).toBe("c");
    wrapper.unmount();
  });
  it("垂直方向先移动时取消拖动并抑制 click", async () => {
    const { wrapper, selected, scrubbing, at, down, move, buttons } = harness({
      orientation: "x",
    });
    down(0);
    move({ x: at(0).x + 2, y: at(0).y + 30 });
    await flushPromises();
    expect(scrubbing.value).toBe(false);
    expect(selected.value).toBe("a");
    const click = new MouseEvent("click", {
      bubbles: true,
      cancelable: true,
      detail: 1,
    });
    buttons[0].element.dispatchEvent(click);
    expect(click.defaultPrevented).toBe(true);
    wrapper.unmount();
  });
  it("Escape 取消并回到按下前的项", async () => {
    const { wrapper, selected, at, down, move, up } = harness({
      orientation: "x",
    });
    down(0);
    move(at(1));
    expect(selected.value).toBe("b");
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    await flushPromises();
    up(at(1));
    await flushPromises();
    expect(selected.value).toBe("a");
    wrapper.unmount();
  });
  it("指针取消与窗口失焦都回退", async () => {
    for (const cancel of ["pointer", "blur"] as const) {
      const { wrapper, selected, at, down, move } = harness({
        orientation: "x",
      });
      down(0);
      move(at(1));
      expect(selected.value).toBe("b");
      if (cancel === "pointer")
        window.dispatchEvent(pointer("pointercancel", at(1)));
      else window.dispatchEvent(new Event("blur"));
      await flushPromises();
      expect(selected.value).toBe("a");
      wrapper.unmount();
    }
  });
  it("拖出分类条后松手回退", async () => {
    const { wrapper, selected, at, down, move, up } = harness({
      orientation: "x",
    });
    down(0);
    move(at(2));
    expect(selected.value).toBe("c");
    up({ x: at(2).x, y: at(2).y + 120 });
    await flushPromises();
    expect(selected.value).toBe("a");
    wrapper.unmount();
  });
  it("指针在相邻项中点附近时保持选中不来回跳", async () => {
    const { wrapper, selected, at, down, move } = harness({
      orientation: "x",
    });
    down(0);
    move({ x: at(0).x + 51, y: at(0).y });
    expect(selected.value).toBe("a");
    move(at(1));
    expect(selected.value).toBe("b");
    wrapper.unmount();
  });
  it("禁用时、非主指针与右键都不启动拖动", async () => {
    const disabled = harness({ orientation: "x", enabled: () => false });
    disabled.down(0);
    disabled.move(disabled.at(2));
    expect(disabled.scrubbing.value).toBe(false);
    expect(disabled.selected.value).toBe("a");
    disabled.wrapper.unmount();
    const blocked = harness({ orientation: "x" });
    const button = blocked.buttons[0].element;
    button.dispatchEvent(
      pointer("pointerdown", blocked.at(0), { primary: false }),
    );
    button.dispatchEvent(pointer("pointerdown", blocked.at(0), { button: 2 }));
    blocked.move(blocked.at(2));
    expect(blocked.scrubbing.value).toBe(false);
    expect(blocked.selected.value).toBe("a");
    blocked.wrapper.unmount();
  });
  it("搜索禁用时分类点击不会被上一次拖动残留的 click 抑制", async () => {
    let enabled = true;
    const { wrapper, selected, at, down, move, up, buttons } = harness({
      orientation: "x",
      enabled: () => enabled,
    });
    down(0);
    move(at(2));
    up(at(2));
    await flushPromises();
    enabled = false;
    down(1);
    up(at(1));
    await flushPromises();
    const click = new MouseEvent("click", {
      bubbles: true,
      cancelable: true,
      detail: 1,
    });
    buttons[0].element.dispatchEvent(click);
    expect(click.defaultPrevented).toBe(false);
    expect(selected.value).toBe("a");
    wrapper.unmount();
  });
  it("没有拖动时普通点击仍然生效", async () => {
    const { wrapper, selected, at, down, up, buttons } = harness({
      orientation: "x",
    });
    down(0);
    up(at(0));
    await flushPromises();
    const click = new MouseEvent("click", {
      bubbles: true,
      cancelable: true,
      detail: 1,
    });
    buttons[1].element.dispatchEvent(click);
    expect(click.defaultPrevented).toBe(false);
    expect(selected.value).toBe("b");
    wrapper.unmount();
  });
});
