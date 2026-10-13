// Applies every update of one branch (from branches.ts --deps) to a copy of the repository with
// Renovate's own auto-replace, one update after another as a branch does, and prints the lines
// each file ends up with changed. The repository is mounted read-only at /repo; the edits go to a
// scratch copy of the touched files.
const D = "/usr/local/renovate/dist";
const { doAutoReplace } = await import(`${D}/workers/repository/update/branch/auto-replace.js`);
const { GlobalConfig } = await import(`${D}/config/global.js`);
const fs = await import("node:fs");
const path = await import("node:path");

const LOCAL = "/tmp/probe-repo";
GlobalConfig.set({ localDir: LOCAL });
const upgrades = JSON.parse(fs.readFileSync(process.env.PROBE_DEPS ?? "/probe/branch-deps.json", "utf8"));
const contents = new Map();
let failed = 0;
for (const upgrade of upgrades) {
  const file = upgrade.packageFile;
  if (!contents.has(file)) {
    fs.mkdirSync(path.dirname(path.join(LOCAL, file)), { recursive: true });
    contents.set(file, fs.readFileSync(path.join("/repo", file), "utf8"));
  }
  try {
    contents.set(file, await doAutoReplace(upgrade, contents.get(file), false));
  } catch (err) {
    failed += 1;
    console.log(`FAILED ${file} ${upgrade.depName}: ${err}`);
  }
}
for (const [file, after] of contents) {
  const before = fs.readFileSync(path.join("/repo", file), "utf8").split("\n");
  const lines = after.split("\n");
  console.log(`== ${file}`);
  for (let i = 0; i < Math.max(before.length, lines.length); i++) {
    if (before[i] !== lines[i]) console.log(`  line ${i + 1}\n  - ${before[i]}\n  + ${lines[i]}`);
  }
}
console.log(`${upgrades.length} update(s), ${contents.size} file(s), ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
