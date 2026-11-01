import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import WallpaperFade from "../../src/components/WallpaperFade.vue";

class FakeImage {
  static instances: FakeImage[] = [];
  onload: (() => void) | null = null;
  onerror: (() => void) | null = null;
  src = "";
  constructor() {
    FakeImage.instances.push(this);
  }
}

let wrapper: VueWrapper | undefined;
beforeEach(() => {
  FakeImage.instances = [];
  vi.stubGlobal("Image", FakeImage);
});
afterEach(() => {
  wrapper?.unmount();
  wrapper = undefined;
  vi.unstubAllGlobals();
});

describe("壁纸过渡", () => {
  it("换图时旧图淡出, 过渡结束后清理", async () => {
    wrapper = mount(WallpaperFade, { props: { src: "/a.jpg" } });
    expect(wrapper.findAll(".wallpaper-fade__layer")).toHaveLength(1);
    expect(
      wrapper.find(".wallpaper-fade__layer").attributes("style"),
    ).toContain("/a.jpg");

    await wrapper.setProps({ src: "/b.jpg" });
    await nextTick();
    expect(wrapper.findAll(".wallpaper-fade__layer")).toHaveLength(2);
    const cover = wrapper.find(".wallpaper-fade__cover");
    expect(cover.attributes("style")).toContain("/a.jpg");
    expect(
      wrapper
        .find(".wallpaper-fade__layer:not(.wallpaper-fade__cover)")
        .attributes("style"),
    ).toContain("/b.jpg");
    expect(cover.classes()).not.toContain("is-hidden");

    const image = FakeImage.instances.at(-1)!;
    expect(image.src).toBe("/b.jpg");
    image.onload?.();
    vi.advanceTimersByTime(16);
    await nextTick();
    expect(wrapper.find(".wallpaper-fade__cover").classes()).toContain(
      "is-hidden",
    );

    vi.advanceTimersByTime(500);
    await nextTick();
    expect(wrapper.findAll(".wallpaper-fade__layer")).toHaveLength(1);
  });

  it("新图加载失败时保留旧图", async () => {
    wrapper = mount(WallpaperFade, { props: { src: "/a.jpg" } });
    await wrapper.setProps({ src: "/bad.jpg" });
    await nextTick();
    const image = FakeImage.instances.at(-1)!;
    image.onerror?.();
    await nextTick();
    vi.advanceTimersByTime(1000);
    await nextTick();
    expect(wrapper.findAll(".wallpaper-fade__layer")).toHaveLength(2);
    const cover = wrapper.find(".wallpaper-fade__cover");
    expect(cover.attributes("style")).toContain("/a.jpg");
    expect(cover.classes()).not.toContain("is-hidden");
    expect(
      wrapper
        .find(".wallpaper-fade__layer:not(.wallpaper-fade__cover)")
        .attributes("style"),
    ).toContain("/a.jpg");
  });

  it("地址不变时不触发过渡", async () => {
    wrapper = mount(WallpaperFade, { props: { src: "/a.jpg" } });
    await wrapper.setProps({ src: "/a.jpg" });
    await nextTick();
    expect(wrapper.findAll(".wallpaper-fade__layer")).toHaveLength(1);
    expect(FakeImage.instances).toHaveLength(0);
  });
});
