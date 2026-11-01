<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from "vue";

const props = withDefaults(defineProps<{ src: string; position?: string }>(), {
  position: "center",
});
const base = ref(props.src);
const cover = ref("");
const hidden = ref(false);
let fadeTimer = 0;
let cleanupTimer = 0;
let preload: HTMLImageElement | undefined;

function releasePreload() {
  if (!preload) return;
  preload.onload = null;
  preload.onerror = null;
  preload = undefined;
}

function beginFade() {
  // 新图就绪后再让旧图层淡出, 避免减弱动态效果时闪空.
  fadeTimer = window.setTimeout(() => {
    hidden.value = true;
    window.clearTimeout(cleanupTimer);
    cleanupTimer = window.setTimeout(() => {
      cover.value = "";
      hidden.value = false;
    }, 500);
  }, 16);
}

function styleFor(url: string) {
  return {
    backgroundImage: url ? `url("${url}")` : undefined,
    backgroundPosition: props.position,
  };
}
const baseStyle = computed(() => styleFor(base.value));
const coverStyle = computed(() => styleFor(cover.value));

// 旧图盖在新图上面淡出, 换背景时不会出现空白帧或硬切.
watch(
  () => props.src,
  (next) => {
    if (!next || next === base.value) return;
    cover.value = base.value;
    base.value = next;
    hidden.value = false;
    window.clearTimeout(fadeTimer);
    window.clearTimeout(cleanupTimer);
    releasePreload();
    const image = new Image();
    preload = image;
    image.onload = () => {
      if (preload !== image) return;
      preload = undefined;
      beginFade();
    };
    // 新图加载失败时保留旧图, 不把背景置空.
    image.onerror = () => {
      if (preload !== image) return;
      preload = undefined;
      base.value = cover.value;
    };
    image.src = next;
  },
);
onBeforeUnmount(() => {
  window.clearTimeout(fadeTimer);
  window.clearTimeout(cleanupTimer);
  releasePreload();
});
</script>

<template>
  <div class="wallpaper-fade" aria-hidden="true">
    <div class="wallpaper-fade__layer" :style="baseStyle"></div>
    <div
      v-if="cover"
      class="wallpaper-fade__layer wallpaper-fade__cover"
      :class="{ 'is-hidden': hidden }"
      :style="coverStyle"
    ></div>
  </div>
</template>

<style scoped>
.wallpaper-fade {
  position: absolute;
  inset: 0;
  overflow: hidden;
}
.wallpaper-fade__layer {
  position: absolute;
  inset: 0;
  background-position: center;
  background-size: cover;
  background-repeat: no-repeat;
}
.wallpaper-fade__cover {
  opacity: 1;
  transition: opacity 420ms ease;
}
.wallpaper-fade__cover.is-hidden {
  opacity: 0;
}
.reduce-motion .wallpaper-fade__cover {
  transition: none;
}
@media (prefers-reduced-motion: reduce) {
  .wallpaper-fade__cover {
    transition: none;
  }
}
</style>
