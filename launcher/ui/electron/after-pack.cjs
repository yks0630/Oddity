const {
  rename,
  copyFile,
  chmod,
  mkdir,
  writeFile,
  symlink,
  readdir,
  lstat,
  realpath,
  stat,
} = require("node:fs/promises");
const { createHash } = require("node:crypto");
const { createReadStream } = require("node:fs");
const { join, resolve, sep } = require("node:path");

const MANIFEST = "FILES.sha256";

function sha256File(path) {
  return new Promise((resolveHash, reject) => {
    const hash = createHash("sha256");
    const stream = createReadStream(path);
    stream.on("error", reject);
    stream.on("data", (chunk) => hash.update(chunk));
    stream.on("end", () => resolveHash(hash.digest("hex")));
  });
}

// 生成安装包内的文件清单; 符号链接按目标内容记录, 跳过清单自身.
async function writeFileManifest(appOutDir) {
  const files = [];
  const pending = [""];
  const root = resolve(appOutDir);
  while (pending.length) {
    const relative = pending.pop();
    const info = await lstat(join(appOutDir, relative || "."));
    if (info.isDirectory()) {
      for (const name of await readdir(join(appOutDir, relative || ".")))
        pending.push(relative ? `${relative}/${name}` : name);
      continue;
    }
    if (relative === MANIFEST) continue;
    if (info.isSymbolicLink()) {
      const target = await realpath(join(appOutDir, relative));
      if (target !== root && !target.startsWith(root + sep))
        throw new Error(`符号链接指向应用目录外: ${relative} -> ${target}`);
      if (!(await stat(target)).isFile())
        throw new Error(`符号链接目标不是文件: ${relative} -> ${target}`);
      files.push(relative);
      continue;
    }
    if (info.isFile()) files.push(relative);
  }
  files.sort();
  const lines = [];
  for (const relative of files)
    lines.push(`${await sha256File(join(appOutDir, relative))}  ${relative}`);
  await writeFile(join(appOutDir, MANIFEST), `${lines.join("\n")}\n`);
  return files.length;
}

module.exports = async ({ appOutDir }) => {
  await rename(join(appOutDir, "oddity"), join(appOutDir, "oddity-shell"));
  await copyFile(resolve("../packaging/oddity"), join(appOutDir, "oddity"));
  await chmod(join(appOutDir, "oddity"), 0o755);
  const bin = join(appOutDir, "resources/bin");
  await mkdir(bin, { recursive: true });
  await writeFile(
    join(bin, "oddity"),
    '#!/bin/sh\nexec "$(dirname "$(readlink -f "$0")")/../../oddity" "$@"\n',
    { mode: 0o755 },
  );
  for (const name of ["nte-cli", "oddity-recovery"])
    await symlink("resources/bin/" + name, join(appOutDir, name));
  const pending = [appOutDir];
  while (pending.length) {
    const path = pending.pop();
    const info = await lstat(path);
    if (info.isSymbolicLink()) continue;
    await chmod(path, info.mode & 0o755);
    if (info.isDirectory())
      for (const name of await readdir(path)) pending.push(join(path, name));
  }
  const count = await writeFileManifest(appOutDir);
  console.log(`FILES.sha256: ${count} 个文件`);
};

module.exports.writeFileManifest = writeFileManifest;
