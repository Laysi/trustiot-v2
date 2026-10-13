// Copies the custom-regex dependencies of the chart values and nightly.yml (with their planned
// updates) out of a Renovate debug log. Usage: bun extract-deps.ts <log> <out.json>
import { readFileSync, writeFileSync } from "node:fs";

const [log, out] = Bun.argv.slice(2);
const rec = readFileSync(log!, "utf8")
  .split("\n")
  .filter((l) => l.includes('"packageFiles with updates"'))
  .map((l) => JSON.parse(l))[0];
const files = rec.config.regex.filter(
  (f: { packageFile: string }) => f.packageFile === "deploy/chart/values.yaml" || f.packageFile === ".github/workflows/nightly.yml",
);
writeFileSync(out!, JSON.stringify(files, null, 2));
