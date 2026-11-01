<script setup lang="ts">
import { computed } from "vue";
import { gamepadGlyph } from "../core/gamepad";
import type { GamepadButton } from "../types/gamepad";
const props = defineProps<{ button: GamepadButton; symbol?: string }>();
const glyph = computed(() => props.symbol ?? gamepadGlyph(props.button));
// 相对文档地址解析图标精灵, 桌面端与 Pages 子路径都能命中.
const href = computed(
  () =>
    new URL(`assets/gamepad/prompts.svg#${glyph.value}`, document.baseURI).href,
);
</script>
<template>
  <svg class="gamepad-glyph" viewBox="0 0 64 64" aria-hidden="true">
    <use :href="href" />
  </svg>
</template>
