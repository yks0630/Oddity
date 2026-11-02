<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from "vue";
import { motion, useReducedMotion } from "motion-v";
import { createTimeline } from "animejs";
import {
  state,
  form,
  launchView,
  updateView,
  homeDownloadNotice,
  showPage,
  handleLaunchAction,
} from "../core/controller";
import { playerExpanded } from "../composables/useSceneMotion";
import HomeGameRecords from "../components/HomeGameRecords.vue";
import { gamepad } from "../core/gamepad";
import GamepadGlyph from "../components/GamepadGlyph.vue";
const isActive = computed(() => state.page === "home");
const prefersReduced = useReducedMotion();
const reduced = computed(() => form.reduceMotion || prefersReduced.value);
const gameName = computed(() =>
  state.bootstrap?.settings.distribution === "global" ? "NTE" : "异环",
);
// 状态行始终占位, 避免提示显隐时把标题和启动按钮一起顶动.
const statusVisible = computed(
  () =>
    state.game !== "launching" &&
    (homeDownloadNotice.value.visible || updateView.value.showHomeStatus),
);

// 文案切换沿用 GamepadHints 的 Anime.js 编排: 先模糊退出, 再换字。
const label = computed(() => launchView.value.label);
const displayedLabel = ref(label.value);
const labelVisible = ref(true);
let labelTimeline: ReturnType<typeof createTimeline> | undefined;
watch(label, (next) => {
  labelTimeline?.cancel();
  if (next === displayedLabel.value) {
    labelVisible.value = true;
    return;
  }
  if (reduced.value) {
    displayedLabel.value = next;
    labelVisible.value = true;
    return;
  }
  labelVisible.value = false;
  labelTimeline = createTimeline().call(() => {
    displayedLabel.value = next;
    labelVisible.value = true;
  }, 140);
});
onBeforeUnmount(() => labelTimeline?.cancel());

// 启动中的可见文案仍是阶段进度, 可访问名称补上「暂停启动」这个实际动作。
const launchAriaLabel = computed(() => {
  const view = launchView.value;
  return view.pauseAvailable && !view.pausing
    ? `${view.label}，暂停启动`
    : view.label;
});

function handleMainLaunchClick() {
  if (launchView.value.disabled) return;
  handleLaunchAction();
}

// 手柄确认键落在启动区时转给主按钮; 按钮自身的按键激活由浏览器处理, 不重复触发。
function handleLaunchRegionKey(event: KeyboardEvent) {
  if (event.target !== event.currentTarget) return;
  event.preventDefault();
  handleMainLaunchClick();
}
</script>

<template>
  <section
    class="page page--home"
    :class="{ 'is-active': isActive }"
    id="page-home"
    :inert="!isActive"
    aria-labelledby="home-title"
  >
    <div class="hero">
      <div class="hero__inner">
        <HomeGameRecords />
        <motion.div
          class="hero__content"
          :initial="{ opacity: 0, y: 22 }"
          :animate="{
            opacity: !isActive ? 0.25 : playerExpanded ? 0.65 : 1,
            y: 0,
          }"
          :transition="{
            type: 'spring',
            stiffness: 180,
            damping: 25,
            delay: 0.1,
          }"
        >
          <h1 class="hero__title" id="home-title">
            <span class="hero__cn">{{ gameName }}</span>
          </h1>
          <div class="hero__actions">
            <div
              class="hero__launch"
              id="launch-region"
              :data-pad-region="state.game === 'launching' ? '' : undefined"
              :tabindex="state.game === 'launching' ? -1 : undefined"
              :aria-label="
                state.game === 'launching' ? launchView.label : undefined
              "
              @keydown.enter="handleLaunchRegionKey"
            >
              <button
                class="btn btn--primary btn--trailing hero__launch-main"
                id="launch-btn"
                @click="handleMainLaunchClick"
                type="button"
                :aria-busy="launchView.busy"
                :aria-disabled="launchView.disabled || undefined"
                :aria-label="launchAriaLabel"
                :class="{
                  'is-busy': launchView.disabled,
                  'is-running': state.game === 'running',
                }"
              >
                <motion.span
                  class="btn__text"
                  :initial="false"
                  :animate="{
                    opacity: labelVisible ? 1 : 0,
                    filter: labelVisible || reduced ? 'blur(0px)' : 'blur(6px)',
                    y: labelVisible || reduced ? 0 : -4,
                  }"
                  :transition="{
                    duration: reduced ? 0 : 0.14,
                    ease: 'easeOut',
                  }"
                  >{{ displayedLabel }}</motion.span
                >
                <span class="btn__icon"
                  ><GamepadGlyph v-if="gamepad.active" button="west" /><svg
                    v-else
                    class="icon"
                    viewBox="0 0 256 256"
                    aria-hidden="true"
                  >
                    <use :href="'#' + launchView.icon"></use></svg
                ></span>
              </button>
            </div>
          </div>
          <div
            class="hero__status-slot"
            :class="{ 'is-visible': statusVisible }"
          >
            <button
              class="hero__status"
              id="home-status"
              @click="showPage('update')"
              type="button"
              :tabindex="statusVisible ? undefined : -1"
              :aria-hidden="!statusVisible || undefined"
              :data-state="
                homeDownloadNotice.visible
                  ? homeDownloadNotice.kind
                  : updateView.homeStatusKind
              "
            >
              <i class="status-dot status-dot--warn"></i>
              <span id="home-status-text">{{
                homeDownloadNotice.visible
                  ? homeDownloadNotice.text
                  : updateView.homeStatusText
              }}</span>
            </button>
          </div>
        </motion.div>
      </div>
    </div>
  </section>
</template>
