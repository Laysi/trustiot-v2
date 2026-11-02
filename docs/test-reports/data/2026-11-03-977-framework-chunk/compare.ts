/**
 * Applies the decision rule of docs/test-reports/2026-11-03-977-framework-chunk.md to the four
 * results.json files next to this script: bun docs/test-reports/data/2026-11-03-977-framework-chunk/compare.ts
 */
import { dirname } from "node:path";

const DIR = dirname(import.meta.path);
const median = (values: number[]) => {
  const sorted = [...values].sort((a, b) => a - b);
  const n = sorted.length;
  return n % 2 ? (sorted[(n - 1) / 2] as number) : ((sorted[n / 2 - 1] as number) + (sorted[n / 2] as number)) / 2;
};
type Record = {
  scenario: string;
  profile: string;
  cache: string;
  load: { ready: number | null; transferBytes: number };
};
async function records(variant: string, ingress: string): Promise<Record[]> {
  return (await Bun.file(`${DIR}/${variant}-${ingress}/results.json`).json()).records;
}
const round = (n: number) => Math.round(n);

const RUNS = 7;
const SCENARIOS = ["login", "home", "settings"];
const PROFILES = ["fast4g", "slow4g"];
const CACHES = ["cold", "warm", "upgrade"];

/** The grid is complete: every cell has all its runs, each with a ready time and a transfer size. */
function requireComplete(name: string, rs: Record[]) {
  for (const scenario of SCENARIOS) {
    for (const profile of PROFILES) {
      for (const cache of CACHES) {
        const cell = rs.filter((r) => r.scenario === scenario && r.profile === profile && r.cache === cache);
        const valid = cell.filter(
          (r) => Number.isFinite(r.load.ready) && Number.isFinite(r.load.transferBytes) && r.load.transferBytes > 0,
        );
        if (cell.length !== RUNS || valid.length !== RUNS) {
          throw new Error(
            `${name}: ${scenario} ${profile} ${cache} has ${valid.length} valid of ${cell.length} runs, want ${RUNS}`,
          );
        }
      }
    }
  }
  if (rs.length !== SCENARIOS.length * PROFILES.length * CACHES.length * RUNS) {
    throw new Error(`${name}: ${rs.length} records, which is not the whole grid`);
  }
}

const timing: string[] = [
  "| 入口 | 頁面 | 網路 | 快取 | B1 ready ms（最小-最大） | B1 加 framework（最小-最大） | 差 | 容許 | 判定 |",
  "| --- | --- | --- | --- | --- | --- | --- | --- | --- |",
];
const revisit: string[] = [
  "| 入口 | 頁面 | 網路 | B1 下載 KB | 加 framework 下載 KB | 降幅 | B1 再訪 ready ms | 加 framework 再訪 ready ms |",
  "| --- | --- | --- | --- | --- | --- | --- | --- |",
];
let slower = 0;
let smallestDrop = Number.POSITIVE_INFINITY;
for (const ingress of ["none", "br"]) {
  const b1 = await records("b1", ingress);
  const fw = await records("fw", ingress);
  requireComplete(`b1-${ingress}`, b1);
  requireComplete(`fw-${ingress}`, fw);
  for (const scenario of SCENARIOS) {
    for (const profile of PROFILES) {
      const pick = (rs: Record[], cache: string) =>
        rs.filter((r) => r.scenario === scenario && r.profile === profile && r.cache === cache);
      for (const cache of ["cold", "warm"]) {
        const a = pick(b1, cache).map((r) => r.load.ready as number);
        const b = pick(fw, cache).map((r) => r.load.ready as number);
        const spread = Math.max(Math.max(...a) - Math.min(...a), Math.max(...b) - Math.min(...b));
        const allowed = Math.max(spread, median(a) * 0.05);
        const diff = median(b) - median(a);
        if (diff > allowed) slower++;
        timing.push(
          `| ${ingress} | ${scenario} | ${profile} | ${cache} | ${round(median(a))} (${round(Math.min(...a))}-${round(Math.max(...a))}) | ${round(median(b))} (${round(Math.min(...b))}-${round(Math.max(...b))}) | ${diff >= 0 ? "+" : ""}${round(diff)} | ${round(allowed)} | ${diff > allowed ? "較慢" : "通過"} |`,
        );
      }
      const ua = pick(b1, "upgrade");
      const ub = pick(fw, "upgrade");
      const kbA = median(ua.map((r) => r.load.transferBytes)) / 1000;
      const kbB = median(ub.map((r) => r.load.transferBytes)) / 1000;
      const drop = (1 - kbB / kbA) * 100;
      if (ingress === "br") smallestDrop = Math.min(smallestDrop, drop);
      revisit.push(
        `| ${ingress} | ${scenario} | ${profile} | ${kbA.toFixed(1)} | ${kbB.toFixed(1)} | ${round(drop)}% | ${round(median(ua.map((r) => r.load.ready as number)))} | ${round(median(ub.map((r) => r.load.ready as number)))} |`,
      );
    }
  }
}
console.log(`${timing.join("\n")}\n\n${revisit.join("\n")}\n`);
console.log(`Brotli revisit download: smallest drop ${smallestDrop.toFixed(1)}% (rule: at least 30%)`);
console.log(`cold/warm cells slower than the allowance: ${slower} (rule: 0)`);
console.log(slower === 0 && smallestDrop >= 30 ? "adopt" : "do not adopt");
