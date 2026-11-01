import { afterEach, describe, expect, it, vi } from "vitest";
import { applyDemoPreview, DEMO_NOTICE_KEY, isDemo } from "../../src/core/demo";
import { bootstrap } from "./fixtures";

const preview = {
  protonPath:
    "~/.local/share/Steam/compatibilitytools.d/GE-Proton11-5-x86_64/proton",
  protonName: "GE-Proton11-5-x86_64",
  prefix: "~/.local/share/oddity/prefix",
};

afterEach(() => {
  vi.unstubAllEnvs();
  localStorage.removeItem(DEMO_NOTICE_KEY);
});

describe("浏览器演示初始化", () => {
  it("把模拟 bootstrap 标记为已就绪并写入演示运行环境", () => {
    const data = bootstrap({
      ok: false,
      error: "请先在设置中创建并选择 Proton 运行环境",
      prefix: "",
      prefixExists: false,
      protonName: "",
      protonPath: "",
      checks: [
        { label: "Proton", ok: false, detail: "未选择运行环境" },
        { label: "Wine prefix", ok: false, detail: "" },
        { label: "umu-launcher", ok: true, detail: "umu-run（PATH）" },
      ],
    });
    const before = Math.floor(Date.now() / 1000);
    applyDemoPreview(data, preview);
    const after = Math.floor(Date.now() / 1000);

    expect(data.ok).toBe(true);
    expect(data.error).toBeNull();
    expect(data.prefix).toBe(preview.prefix);
    expect(data.prefixExists).toBe(true);
    expect(data.protonPath).toBe(preview.protonPath);
    expect(data.protonName).toBe(preview.protonName);
    expect(data.settings.runtimeEnvironments).toHaveLength(1);
    const [environment] = data.settings.runtimeEnvironments;
    expect(environment).toEqual({
      id: "00000000000000000000000000000001",
      name: "GE-Proton11-5",
      proton: { path: preview.protonPath, identity: "browser-preview" },
      installRoot: data.installRoot,
      distribution: data.settings.distribution,
      createdAt: expect.any(Number),
    });
    expect(environment.createdAt).toBeGreaterThanOrEqual(before);
    expect(environment.createdAt).toBeLessThanOrEqual(after);
    expect(data.settings.activeEnvironment).toBe(environment.id);
    expect(data.settings.stableEnvironment).toBe(environment.id);
    expect(data.checks).toEqual([
      { label: "Proton", ok: true, detail: "GE-Proton11-5" },
      { label: "Wine prefix", ok: true, detail: preview.prefix },
      { label: "umu-launcher", ok: true, detail: "umu-run（PATH）" },
    ]);
  });

  it("默认构建保持普通浏览器预览的未配置状态", () => {
    expect(isDemo).toBe(false);
  });

  it("演示构建下 bootstrap 返回就绪状态", async () => {
    vi.stubEnv("VITE_DEMO", "1");
    vi.resetModules();
    const { invoke } = await import("../../src/core/api");
    const data = await invoke("bootstrap");
    expect(data.ok).toBe(true);
    expect(data.error).toBeNull();
    expect(data.settings.runtimeEnvironments).toHaveLength(1);
    expect(data.settings.activeEnvironment).not.toBeNull();
    expect(data.prefixExists).toBe(true);
  });
});
