import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

const require = createRequire(import.meta.url);
const { writeFileManifest } = require("../launcher/ui/electron/after-pack.cjs");

test("FILES.sha256 覆盖常规文件, 跳过符号链接和自身", async () => {
  const root = mkdtempSync(join(tmpdir(), "oddity-manifest-"));
  try {
    mkdirSync(join(root, "resources/bin"), { recursive: true });
    writeFileSync(join(root, "oddity-shell"), "shell");
    writeFileSync(join(root, "resources/bin/oddity-backend"), "backend");
    writeFileSync(join(root, "FILES.sha256"), "stale");
    symlinkSync("resources/bin/oddity-backend", join(root, "nte-cli"));

    const count = await writeFileManifest(root);
    assert.equal(count, 3);
    const lines = readFileSync(join(root, "FILES.sha256"), "utf8")
      .trim()
      .split("\n");
    assert.deepEqual(
      lines.map((line) => line.slice(66)),
      ["nte-cli", "oddity-shell", "resources/bin/oddity-backend"],
    );
    for (const line of lines) {
      const hash = line.slice(0, 64);
      const name = line.slice(66);
      const actual = createHash("sha256")
        .update(readFileSync(join(root, name)))
        .digest("hex");
      assert.equal(hash, actual);
    }

    // 符号链接按目标内容记录, 不需要额外格式.
    assert.equal(
      lines.find((line) => line.endsWith("  nte-cli")).slice(0, 64),
      createHash("sha256").update("backend").digest("hex"),
    );

    // 重复执行时清单内容保持稳定, 方便多个打包目标复用同一份目录.
    const first = readFileSync(join(root, "FILES.sha256"), "utf8");
    await writeFileManifest(root);
    assert.equal(readFileSync(join(root, "FILES.sha256"), "utf8"), first);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("应用目录外的符号链接无法进入清单", async () => {
  const root = mkdtempSync(join(tmpdir(), "oddity-manifest-outside-"));
  try {
    writeFileSync(join(root, "oddity-shell"), "shell");
    symlinkSync(tmpdir(), join(root, "escape"));
    await assert.rejects(
      () => writeFileManifest(root),
      /符号链接目标不是文件|符号链接指向应用目录外/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
