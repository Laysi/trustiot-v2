// bun run.ts <test-binary> <runs> <label>: runs the container test binary sequentially from the
// service-kit package directory (as cargo test does), one log per run, and prints a summary.
// The environment is passed through (for example PATH with the slow-stop podman wrapper first).
import { mkdirSync, writeFileSync } from "node:fs";
const [bin, runsArg, label] = process.argv.slice(2);
const runs = Number(runsArg);
const dir = `${import.meta.dir}/logs-${label}`;
const cwd = `${process.cwd()}/crates/service-kit`;
mkdirSync(dir, { recursive: true });
let failed = 0;
const failures: string[] = [];
const started = Date.now();
for (let i = 1; i <= runs; i++) {
  const t0 = Date.now();
  const p = Bun.spawnSync([bin, "--nocapture"], { cwd, stdout: "pipe", stderr: "pipe" });
  const out = p.stdout.toString() + p.stderr.toString();
  const id = `run-${String(i).padStart(2, "0")}`;
  writeFileSync(`${dir}/${id}.log`, out);
  const secs = Math.round((Date.now() - t0) / 100) / 10;
  if (p.exitCode !== 0) {
    failed++;
    const where = out.match(/panicked at [^\n]*\n[^\n]*/g)?.join(" | ") ?? "?";
    failures.push(`${id} (${secs}s): ${where}`);
  }
}
console.log(`${label}: runs=${runs} failed=${failed} seconds=${Math.round((Date.now() - started) / 1000)}`);
for (const f of failures) console.log(f);
