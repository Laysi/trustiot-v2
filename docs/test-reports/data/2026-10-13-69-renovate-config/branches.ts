// Groups the updates of a Renovate debug log by branch: which files, read by which manager, one
// branch (one pull request) changes. With --deps <out.json> it also writes, for the branches whose
// name matches the filter, everything auto-replace needs to apply them (for probe/autoreplace-branch.mjs).
// Usage: bun branches.ts <log> [branch filter regex] [--deps <out.json>]
import { readFileSync, writeFileSync } from "node:fs";

const args = Bun.argv.slice(2);
const depsAt = args.indexOf("--deps");
const depsOut = depsAt >= 0 ? args[depsAt + 1] : undefined;
const [log, filter] = depsAt >= 0 ? args.slice(0, depsAt) : args;
const rec = readFileSync(log!, "utf8")
  .split("\n")
  .filter((l) => l.includes('"packageFiles with updates"'))
  .map((l) => JSON.parse(l))[0];
const re = filter ? new RegExp(filter) : undefined;
const branches = new Map<string, string[]>();
const forProbe: unknown[] = [];
for (const [manager, files] of Object.entries(rec.config as Record<string, any[]>)) {
  for (const { deps, ...file } of files) {
    for (const [depIndex, dep] of (deps ?? []).entries()) {
      for (const update of dep.updates ?? []) {
        if (re && !re.test(update.branchName)) continue;
        const short = (d?: string) => (d?.startsWith("sha256:") ? `@${d.slice(7, 19)}` : "");
        const line = `${manager}\t${file.packageFile}\t${dep.depName}\t${dep.currentValue}${short(dep.currentDigest)} -> ${update.updateType} ${update.newValue}${short(update.newDigest)}`;
        branches.set(update.branchName, [...(branches.get(update.branchName) ?? []), line]);
        const { updates: _updates, ...depOnly } = dep;
        forProbe.push({ ...file, ...depOnly, ...update, manager, depIndex });
      }
    }
  }
}
for (const [branch, lines] of [...branches].sort()) console.log(`${branch} (${lines.length} update(s), ${new Set(lines.map((l) => l.split("\t")[1])).size} file(s))\n  ${lines.join("\n  ")}`);
if (depsOut) writeFileSync(depsOut, JSON.stringify(forProbe, null, 2));
