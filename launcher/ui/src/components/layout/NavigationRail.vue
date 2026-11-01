<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from "vue";
import { motion } from "motion-v";
import { state, showPage } from "../../core/controller";
import { navigationScrubbing } from "../../composables/navigationGesture";
import { useScrubNavigation } from "../../composables/useScrubNavigation";

const items = [
  { page: "home", label: "启动", icon: "play" },
  { page: "update", label: "更新", icon: "download-simple" },
  { page: "game-mode", label: "游戏模式", icon: "game-controller" },
  { page: "bulletin", label: "公告", icon: "newspaper-clipping" },
  { page: "records", label: "记录", icon: "clock" },
  { page: "settings", label: "设置", icon: "gear-six" },
] as const;
const itemWidth = 40;
const itemGap = 8;
const activeIndex = computed(() =>
  items.findIndex((item) => item.page === state.page),
);
const hoveredPage = ref<string | null>(null);
const pressedPage = ref<string | null>(null);
const nav = ref<HTMLElement | null>(null);
const { down, lost, cancel, suppressed } = useScrubNavigation({
  root: nav,
  item: "[data-page]",
  value: (element) => element.dataset.page,
  active: () => state.page,
  select: (page) => showPage(page),
  axis: "x",
  corridor: 24,
  focusOnFinish: true,
  scrubbing: navigationScrubbing,
});
function buttons() {
  return [
    ...(nav.value?.querySelectorAll<HTMLButtonElement>("[data-page]") ?? []),
  ];
}
function click(event: MouseEvent, page: string) {
  if (suppressed(event)) return;
  showPage(page);
}
function key(event: KeyboardEvent) {
  if (!(event.target instanceof Element) || !nav.value?.contains(event.target))
    return;
  const current = buttons().findIndex(
    (button) => button === document.activeElement,
  );
  const index =
    event.key === "ArrowRight"
      ? (current + 1) % items.length
      : event.key === "ArrowLeft"
        ? (current - 1 + items.length) % items.length
        : event.key === "Home"
          ? 0
          : event.key === "End"
            ? items.length - 1
            : -1;
  if (index < 0) return;
  event.preventDefault();
  cancel();
  navigationScrubbing.value = true;
  showPage(items[index].page);
  void nextTick(() => {
    navigationScrubbing.value = false;
    buttons()[index]?.focus({ preventScroll: true });
  });
}
function blur() {
  hoveredPage.value = null;
  pressedPage.value = null;
}
onMounted(() => {
  window.addEventListener("blur", blur);
  window.addEventListener("keydown", key, true);
});
onBeforeUnmount(() => {
  window.removeEventListener("blur", blur);
  window.removeEventListener("keydown", key, true);
});
</script>

<template>
  <aside class="rail">
    <nav
      ref="nav"
      class="rail__nav"
      aria-label="主导航"
      :data-scrubbing="navigationScrubbing"
      @pointerdown="down"
      @lostpointercapture="lost"
      @mousedown.stop
      @dragstart.prevent
      :style="{
        '--rail-item-width': `${itemWidth}px`,
        '--rail-item-gap': `${itemGap}px`,
      }"
    >
      <motion.span
        class="nav-focus"
        aria-hidden="true"
        :initial="false"
        :animate="{
          x: activeIndex * (itemWidth + itemGap),
          y: hoveredPage === state.page ? -2 : 0,
          scale:
            pressedPage === state.page ? 0.88 : navigationScrubbing ? 1.09 : 1,
        }"
        :transition="{ type: 'spring', stiffness: 420, damping: 32 }"
      />
      <motion.button
        v-for="item in items"
        :key="item.page"
        v-memo="[state.page === item.page, navigationScrubbing]"
        type="button"
        :while-hover="{ y: -2 }"
        :while-press="{ scale: 0.88 }"
        :animate="{
          scale: navigationScrubbing && state.page === item.page ? 1.09 : 1,
        }"
        :transition="{ type: 'spring', stiffness: 460, damping: 25 }"
        class="rail-item"
        :data-page="item.page"
        :aria-label="item.label"
        :title="navigationScrubbing ? undefined : item.label"
        :class="{ 'is-active': state.page === item.page }"
        :aria-current="state.page === item.page ? 'page' : undefined"
        @click="click($event, item.page)"
        @pointerenter="
          (event) => {
            if (event.pointerType === 'mouse') hoveredPage = item.page;
          }
        "
        @pointerleave="
          hoveredPage = null;
          pressedPage = null;
        "
        @pointerdown="pressedPage = item.page"
        @pointerup="pressedPage = null"
        @pointercancel="pressedPage = null"
        @keydown="
          (event) => {
            if (event.key === 'Enter' || event.key === ' ')
              pressedPage = item.page;
          }
        "
        @keyup="pressedPage = null"
        @blur="pressedPage = null"
      >
        <svg class="icon" viewBox="0 0 256 256" aria-hidden="true">
          <use :href="`#i-${item.icon}`" />
        </svg>
      </motion.button>
    </nav>
  </aside>
</template>
