// Estimates how long after `podman stop` started the service logged the SIGTERM:
// signal_latency ~= requested_ts - (complete_ts - stop_took). stop_took is measured in the test from
// just before the `stop` thread starts to when `stop` returned; `stop` returns slightly after the
// "shutdown complete" log, so the estimate is a lower bound of the latency by that overhead.
// usage: bun latency.ts <log dir>
import { readdirSync, readFileSync } from "node:fs";
const dir = process.argv[2];
const rows: number[] = [];
for (const f of readdirSync(dir).sort()) {
  const t = readFileSync(`${dir}/${f}`, "utf8");
  const took = t.match(/stop returned after (\d+) ms/)?.[1];
  const req = t.match(/"timestamp":"([^"]+)"[^\n]*shutdown requested/)?.[1];
  const done = t.match(/"timestamp":"([^"]+)"[^\n]*shutdown complete/)?.[1];
  if (!took || !req || !done) continue;
  const latency = Date.parse(req) - (Date.parse(done) - Number(took));
  rows.push(latency);
}
rows.sort((a, b) => a - b);
console.log(`n=${rows.length} min=${rows[0]} median=${rows[Math.floor(rows.length / 2)]} max=${rows[rows.length - 1]} (ms)`);
console.log(rows.join(" "));
