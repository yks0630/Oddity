<script setup lang="ts">
import BaseButton from "./BaseButton.vue";
import BaseModal from "./BaseModal.vue";
import { ui, closeDemoNotice } from "../core/controller";

const RELEASES_URL = "https://github.com/LaT-SKY/Oddity/releases/latest";
const SOURCE_URL = "https://github.com/LaT-SKY/Oddity";

function openExternal(url: string) {
  window.open(url, "_blank", "noopener,noreferrer");
}

function update(open: boolean) {
  if (!open) closeDemoNotice();
}
</script>

<template>
  <BaseModal :open="ui.demoNotice" title="浏览器演示" @update:open="update">
    <div class="demo-notice">
      <p>这是 Oddity 的浏览器界面演示, 数据与操作均为模拟.</p>
      <p>
        安装、下载、更新、签到等系统功能需要 Linux 桌面版;
        部分操作会提示只能在桌面版完成.
      </p>
      <div class="demo-notice__actions">
        <BaseButton
          variant="primary"
          icon="i-download-simple"
          @click="openExternal(RELEASES_URL)"
          >下载桌面版</BaseButton
        >
        <BaseButton variant="ghost" @click="openExternal(SOURCE_URL)"
          >查看源码</BaseButton
        >
        <BaseButton variant="ghost" @click="closeDemoNotice"
          >继续体验</BaseButton
        >
      </div>
      <p class="demo-notice__legal">
        Oddity 是第三方项目, 与《异环》开发方和发行方无隶属关系;
        游戏名称、商标与官方美术素材归其权利人所有.
      </p>
    </div>
  </BaseModal>
</template>
