// bun run.ts <test-binary> <runs> <label> [filter...]: runs the binary sequentially from the
// service-kit package directory (as cargo test does) and writes one log per run.
import { mkdirSync, writeFileSync } from "node:fs";
const [bin, runsArg, label, ...filter] = process.argv.slice(2);
const runs = Number(runsArg);
const dir = `${import.meta.dir}/logs-${label}`;
const cwd = `${process.cwd()}/crates/service-kit`;
mkdirSync(dir, { recursive: true });
let failed = 0;
const failures: string[] = [];
const started = Date.now();
for (let i = 1; i <= runs; i++) {
  const p = Bun.spawnSync([bin, ...filter], { cwd, stdout: "pipe", stderr: "pipe" });
  const out = p.stdout.toString() + p.stderr.toString();
  writeFileSync(`${dir}/run-${String(i).padStart(2, "0")}.log`, out);
  if (p.exitCode !== 0) {
    failed++;
    failures.push(`run ${i}: ` + (out.match(/^test .* FAILED$/gm) ?? ["?"]).join("; "));
  }
}
console.log(`${label}: runs=${runs} failed=${failed} seconds=${Math.round((Date.now() - started) / 1000)}`);
for (const f of failures) console.log(f);
