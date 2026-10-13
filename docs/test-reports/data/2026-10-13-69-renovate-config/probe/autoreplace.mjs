// Applies each planned update of deploy/chart/values.yaml with Renovate's own auto-replace (the
// code a Renovate branch runs to edit the file, including its re-extraction check) and prints
// the lines that change.
const D = "/usr/local/renovate/dist";
const { doAutoReplace } = await import(`${D}/workers/repository/update/branch/auto-replace.js`);
const { extractPackageFile } = await import(`${D}/modules/manager/index.js`);
const { GlobalConfig } = await import(`${D}/config/global.js`);
const fs = await import("node:fs");
// Auto-replace writes each attempt to the working copy, as in a branch; use a scratch copy.
fs.mkdirSync("/tmp/probe-repo/deploy/chart", { recursive: true });
GlobalConfig.set({ localDir: "/tmp/probe-repo" });

const files = JSON.parse(fs.readFileSync("/probe/values-deps.json", "utf8"));
const file = files.find((f) => f.packageFile === "deploy/chart/values.yaml");
const content = fs.readFileSync("/repo-after/deploy/chart/values.yaml", "utf8");
const { deps, ...managerConfig } = file;
const manager = process.env.PROBE_MANAGER ?? "regex";
let ex;
try {
  ex = await extractPackageFile(manager, content, file.packageFile, managerConfig);
} catch (err) {
  ex = String(err);
}
console.log(`re-extract with manager=${manager}: ${JSON.stringify(ex)?.slice(0, 400)}`);
for (const [depIndex, dep] of deps.entries()) {
  for (const update of dep.updates ?? []) {
    const upgrade = { ...managerConfig, ...dep, ...update, manager, packageFile: file.packageFile, depIndex };
    delete upgrade.updates;
    let result;
    try {
      result = await doAutoReplace(upgrade, content, false);
    } catch (err) {
      console.log(`== ${dep.depName} ${update.updateType} -> ${update.newValue}: ${err}`);
      continue;
    }
    const before = content.split("\n");
    const after = (result ?? "").split("\n");
    console.log(`== ${dep.depName} ${update.updateType} -> ${update.newValue} (result: ${result === content ? "unchanged" : "changed"})`);
    for (let i = 0; i < Math.max(before.length, after.length); i++) {
      if (before[i] !== after[i]) console.log(`  line ${i + 1}\n  - ${before[i]}\n  + ${after[i]}`);
    }
  }
}
