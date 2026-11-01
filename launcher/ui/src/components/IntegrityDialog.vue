<script setup lang="ts">
import { computed, ref, watch } from "vue";
import BaseButton from "./BaseButton.vue";
import BaseModal from "./BaseModal.vue";
import {
  state,
  ui,
  dismissIntegrityDialog,
  openIntegrityRelease,
} from "../core/controller";

const details = ref(false);
const status = computed(() => state.bootstrap?.integrity);
const changed = computed(() => status.value?.changed ?? []);
const truncated = computed(
  () =>
    !!status.value && status.value.changedTotal > status.value.changed.length,
);
watch(
  () => ui.integrityDialog,
  (open) => {
    if (open) details.value = false;
  },
);
function update(open: boolean) {
  if (!open) dismissIntegrityDialog();
}
</script>

<template>
  <BaseModal
    :open="ui.integrityDialog"
    title="Oddity 安全性验证未通过"
    @update:open="update"
  >
    <div class="integrity-dialog">
      <p>
        启动器文件与官方发布不一致, 可能影响账号安全. 建议从官方 Release
        重新安装 Oddity, 不要继续使用可能被修改的版本.
      </p>
      <p class="integrity-dialog__hint">
        这项检查只核对本地文件, 不能证明启动器本身可信.
        下载安装包时请优先使用官方来源.
      </p>
      <div class="integrity-dialog__actions">
        <BaseButton
          variant="primary"
          icon="i-download-simple"
          @click="openIntegrityRelease"
          >前往下载官方版本</BaseButton
        >
        <BaseButton variant="ghost" @click="dismissIntegrityDialog"
          >稍后处理</BaseButton
        >
        <BaseButton
          v-if="changed.length"
          variant="ghost"
          @click="details = !details"
          >{{ details ? "收起变更文件" : "查看变更文件" }}</BaseButton
        >
      </div>
      <ul v-if="details" class="integrity-dialog__files">
        <li v-for="file in changed" :key="file">{{ file }}</li>
      </ul>
      <p v-if="details && truncated" class="integrity-dialog__hint">
        共 {{ status!.changedTotal }} 项, 仅显示前 {{ changed.length }} 项.
      </p>
    </div>
  </BaseModal>
</template>
