// Summarizes a Renovate debug JSON log: every extracted dependency with its proposed updates,
// plus warnings and skipped dependencies. Usage: bun summarize.ts <log> [filter regex]
import { readFileSync } from "node:fs";

const [logPath, filter] = Bun.argv.slice(2);
const lines = readFileSync(logPath!, "utf8").split("\n").filter((l) => l.startsWith("{"));
const records = lines.map((l) => JSON.parse(l));
const pf = records.find((r) => r.msg === "packageFiles with updates");
if (!pf) throw new Error("no 'packageFiles with updates' record");
const short = (d?: string) => (d ? d.replace(/^sha256:/, "").slice(0, 12) : "");
const re = filter ? new RegExp(filter) : undefined;
const out: string[] = [];
for (const [manager, files] of Object.entries(pf.config as Record<string, any[]>)) {
  for (const file of files) {
    for (const dep of file.deps ?? []) {
      const head = `${manager}\t${file.packageFile}\t${dep.depName}\t${dep.currentValue ?? ""}${dep.currentDigest ? "@" + short(dep.currentDigest) : ""}\tversioning=${dep.versioning ?? ""}`;
      const extra = [dep.skipReason ? `skip=${dep.skipReason}` : "", dep.enabled === false ? "disabled" : ""].filter(Boolean).join(" ");
      const updates = (dep.updates ?? []).map(
        (u: any) => `${u.updateType} -> ${u.newValue ?? ""}${u.newDigest && u.newDigest.startsWith("sha256:") ? "@" + short(u.newDigest) : ""} [${u.branchName}]`,
      );
      const warn = (dep.warnings ?? []).map((w: any) => `WARN ${w.message ?? JSON.stringify(w)}`);
      const line = `${head}\t${extra}\t${updates.length ? updates.join(" ; ") : "no updates"}${warn.length ? "\t" + warn.join(" ; ") : ""}`;
      if (!re || re.test(line)) out.push(line);
    }
  }
}
console.log(out.join("\n"));
