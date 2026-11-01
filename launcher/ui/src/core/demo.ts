import type { Bootstrap } from "../types/launcher";

// 演示构建在编译期注入, 普通预览与桌面版保持原来的初始化流程。
export const isDemo = import.meta.env.VITE_DEMO === "1";
export const DEMO_NOTICE_KEY = "oddity.demo.notice";

// 把浏览器预览的初始状态改成已就绪, 访客打开就能看到完整界面。
export function applyDemoPreview(
  data: Bootstrap,
  preview: { protonPath: string; protonName: string; prefix: string },
): void {
  const id = "00000000000000000000000000000001";
  const environment = {
    id,
    name: "GE-Proton11-5",
    proton: { path: preview.protonPath, identity: "browser-preview" },
    installRoot: data.installRoot,
    distribution: data.settings.distribution,
    createdAt: Math.floor(Date.now() / 1000),
  };
  data.ok = true;
  data.error = null;
  data.prefix = preview.prefix;
  data.prefixExists = true;
  data.protonPath = preview.protonPath;
  data.protonName = preview.protonName;
  data.settings.runtimeEnvironments = [environment];
  data.settings.activeEnvironment = id;
  data.settings.stableEnvironment = id;
  for (const check of data.checks) {
    if (check.label === "Proton") {
      check.ok = true;
      check.detail = environment.name;
    } else if (check.label === "Wine prefix") {
      check.ok = true;
      check.detail = preview.prefix;
    }
  }
}
