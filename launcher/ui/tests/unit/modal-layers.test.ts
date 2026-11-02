import { beforeEach, describe, expect, it } from "vitest";
import { nextTick } from "vue";
import { mount, type VueWrapper } from "@vue/test-utils";
import BaseModal from "../../src/components/BaseModal.vue";
import {
  isTopModalLayer,
  popModalLayer,
  pushModalLayer,
  topModalLayer,
} from "../../src/core/modalLayers";

function host() {
  document.body.innerHTML = '<div id="app"></div>';
  return document.getElementById("app")!;
}
async function mountModal(title: string, open = false): Promise<VueWrapper> {
  const wrapper = mount(BaseModal, {
    props: { open, title },
    attachTo: document.body,
  });
  await nextTick();
  await nextTick();
  return wrapper;
}

describe("弹窗层级", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("按打开顺序分配层号并只让最上层接管", () => {
    const first = pushModalLayer();
    expect(isTopModalLayer(first.layer)).toBe(true);
    const second = pushModalLayer();
    expect(second.layer).toBeGreaterThan(first.layer);
    expect(isTopModalLayer(first.layer)).toBe(false);
    expect(isTopModalLayer(second.layer)).toBe(true);
    expect(topModalLayer()).toBe(second.layer);
    popModalLayer(second.id);
    expect(isTopModalLayer(first.layer)).toBe(true);
    popModalLayer(first.id);
    expect(topModalLayer()).toBe(0);
  });

  it("父弹窗保持打开时反复开关子弹窗, 层号不会持续抬高", () => {
    const parent = pushModalLayer();
    for (let index = 0; index < 30; index++) {
      const child = pushModalLayer();
      expect(child.layer).toBe(parent.layer + 1);
      popModalLayer(child.id);
      expect(topModalLayer()).toBe(parent.layer);
    }
    popModalLayer(parent.id);
    expect(topModalLayer()).toBe(0);
    const reopened = pushModalLayer();
    expect(reopened.layer).toBe(1);
    popModalLayer(reopened.id);
  });

  it("重复移除或移除未登记弹窗时不改变剩余层号", () => {
    const parent = pushModalLayer();
    const child = pushModalLayer();
    popModalLayer(child.id);
    popModalLayer(child.id);
    popModalLayer(999999);
    expect(topModalLayer()).toBe(parent.layer);
    const again = pushModalLayer();
    expect(again.layer).toBe(parent.layer + 1);
    popModalLayer(parent.id);
    popModalLayer(again.id);
    expect(topModalLayer()).toBe(0);
  });

  it("后打开的弹窗叠放在先打开的弹窗之上", async () => {
    const app = host();
    const first = await mountModal("一层");
    const second = await mountModal("二层");
    await first.setProps({ open: true });
    await nextTick();
    await nextTick();
    await second.setProps({ open: true });
    await nextTick();
    await nextTick();
    const modals = [...app.querySelectorAll<HTMLElement>(".modal")];
    expect(modals).toHaveLength(2);
    const layers = modals.map((modal) => Number(modal.dataset.modalLayer));
    expect(layers[1]).toBeGreaterThan(layers[0]);
    expect(modals[1]!.style.zIndex).toContain("var(--z-modal)");
    expect(modals[1]!.querySelector(".modal__head h2")?.textContent).toBe(
      "二层",
    );
    expect(modals[1]!.contains(document.activeElement)).toBe(true);
    first.unmount();
    second.unmount();
  });

  it("下层弹窗更新时不抢走上层焦点", async () => {
    host();
    const first = await mountModal("一层", true);
    const second = await mountModal("二层");
    await second.setProps({ open: true });
    await nextTick();
    await nextTick();
    const top = document.querySelectorAll<HTMLElement>(".modal")[1]!;
    const topButton = top.querySelector<HTMLElement>(".modal__head button")!;
    topButton.focus();
    await first.setProps({ title: "一层更新" });
    await nextTick();
    await nextTick();
    expect(top.contains(document.activeElement)).toBe(true);
    const lowerButton = document
      .querySelectorAll<HTMLElement>(".modal")[0]!
      .querySelector<HTMLElement>(".modal__head button")!;
    expect(lowerButton).not.toBe(document.activeElement);
    first.unmount();
    second.unmount();
  });
});
