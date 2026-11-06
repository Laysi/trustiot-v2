// The mean interval of the display-time comparisons at random times (frontend/sandbox/README.md,
// 「顯示時比對的強制」; Laysi/trustiot-v2#1081): the samples take about 1 / SAMPLE_COST_FACTOR (5%) of
// the frame's thread for every page up to SAMPLE_MEAN_MAX_MS, while the host may show the frame and
// while it hides it, and pages whose comparison costs 25 ms or less keep the mean they had before.
// The cost the mean is drawn from is the moving average, but at most SAMPLE_RECENT_FACTOR times the
// costlier of the latest two comparisons, so one costly comparison spaces out the samples after the
// next two at most twice as far as the page's cost, and a page that stays the same keeps 5%. The runtime's own
// comparisons do not make a heartbeat late, so a large page's samples do not set off comparisons
// without pause.
import { describe, expect, test } from "bun:test";

import { HEARTBEAT_INTERVAL_MS, STALE_AFTER_MS } from "../src/protocol.ts";
import {
  averageCompareCost,
  beatIsLate,
  LATE_BEAT_MS,
  overdueMs,
  SAMPLE_COST_FACTOR,
  SAMPLE_IDLE_FACTOR,
  SAMPLE_MEAN_MAX_MS,
  SAMPLE_MEAN_MIN_MS,
  SAMPLE_RECENT_FACTOR,
  sampleCostMs,
  sampleMeanMs,
} from "../src/runtime.ts";

/** The mean before Laysi/trustiot-v2#1081: the same rule with the bound at 1 s. */
function meanBefore(cost: number, clean: boolean): number {
  const mean = Math.min(1000, Math.max(100, 20 * 2 * cost));
  return clean ? mean : mean * 4;
}

/**
 * The share of the thread the samples take: while the host may show the frame a sample is two
 * comparisons around a paint; while it hides the frame, one comparison now (runtime.ts, `sample`).
 */
const shownShare = (cost: number) => (2 * cost) / sampleMeanMs(cost, true);
const hiddenShare = (cost: number) => cost / sampleMeanMs(cost, false);

/** About 5%: the share the issue caps the samples at. */
const CAP = 1 / SAMPLE_COST_FACTOR;
/** The comparison cost the bound still holds at the cap (800 ms). */
const EDGE_MS = SAMPLE_MEAN_MAX_MS / (2 * SAMPLE_COST_FACTOR);

describe("sampleMeanMs", () => {
  test("the constants are the ones the README states", () => {
    expect(SAMPLE_COST_FACTOR).toBe(20);
    expect(SAMPLE_MEAN_MIN_MS).toBe(100);
    expect(SAMPLE_IDLE_FACTOR).toBe(4);
    // The bound is the mean that keeps a comparison of STALE_AFTER_MS - HEARTBEAT_INTERVAL_MS at the cap.
    expect(SAMPLE_MEAN_MAX_MS).toBe(32_000);
    expect(EDGE_MS).toBe(STALE_AFTER_MS - HEARTBEAT_INTERVAL_MS);
  });

  test("pages whose comparison costs 25 ms or less keep the mean they had", () => {
    for (const cost of [0, 0.5, 1, 2.5, 3, 5.5, 10, 18, 24.9, 25]) {
      expect(sampleMeanMs(cost, true)).toBe(meanBefore(cost, true));
      expect(sampleMeanMs(cost, false)).toBe(meanBefore(cost, false));
    }
    expect(sampleMeanMs(25, true)).toBe(1000);
  });

  test("pages up to the bound take about 5% while the frame may be shown", () => {
    for (const cost of [25, 26, 50, 100, 224, 250, 500, 799, EDGE_MS]) {
      expect(sampleMeanMs(cost, true)).toBe(2 * SAMPLE_COST_FACTOR * cost);
      expect(shownShare(cost)).toBeCloseTo(CAP, 12);
    }
    // Before, the 1 s bound let these take more (the issue's 20% and 45% are 100 ms and 224 ms).
    expect((2 * 100) / meanBefore(100, true)).toBeCloseTo(0.2, 12);
    expect((2 * 224) / meanBefore(224, true)).toBeCloseTo(0.448, 12);
  });

  test("no page takes more than 5% below the bound, and cheap pages take less", () => {
    for (let cost = 0; cost <= EDGE_MS; cost += 0.25) {
      expect(shownShare(cost)).toBeLessThanOrEqual(CAP + 1e-12);
      expect(hiddenShare(cost)).toBeLessThanOrEqual(CAP / 2 + 1e-12);
    }
    // Below the minimum mean the share falls with the cost.
    expect(shownShare(1)).toBeCloseTo(0.02, 12);
  });

  test("the hidden frame's mean is SAMPLE_IDLE_FACTOR times longer, but never above the bound", () => {
    for (const cost of [50, 100, 199, 200]) {
      expect(sampleMeanMs(cost, false)).toBe(SAMPLE_IDLE_FACTOR * sampleMeanMs(cost, true));
    }
    for (const cost of [201, 250, 500, EDGE_MS, 801, 5_000, 1e9]) {
      expect(sampleMeanMs(cost, false)).toBe(SAMPLE_MEAN_MAX_MS);
    }
    // At the bound's edge the hidden frame takes half the shown frame's share: one comparison per sample.
    expect(hiddenShare(EDGE_MS)).toBeCloseTo(CAP / 2, 12);
  });

  test("above the bound the mean stays at the bound and the share grows with the cost", () => {
    expect(sampleMeanMs(EDGE_MS + 1, true)).toBe(SAMPLE_MEAN_MAX_MS);
    expect(sampleMeanMs(1e9, true)).toBe(SAMPLE_MEAN_MAX_MS);
    expect(shownShare(EDGE_MS + 1)).toBeGreaterThan(CAP);
    expect(shownShare(1000)).toBeCloseTo(2000 / 32_000, 12);
  });
});

/** The runtime's state after comparisons that took `costs` (runtime.ts, `timed`). */
function afterComparisons(costs: number[]): { average: number; latest: number; previous: number } {
  let average = 0;
  let latest = 0;
  let previous = 0;
  for (const took of costs) {
    average = averageCompareCost(average, took);
    previous = latest;
    latest = took;
  }
  return { average, latest, previous };
}

/** The shown frame's mean after comparisons that took `costs` (runtime.ts, `sampleMean`). */
function meanAfter(costs: number[]): number {
  const { average, latest, previous } = afterComparisons(costs);
  return sampleMeanMs(sampleCostMs(average, latest, previous), true);
}

/** `count` samples of a shown frame whose comparison before the paint takes `before` and after it `after`. */
const samples = (count: number, before: number, after: number) =>
  Array.from({ length: count }, () => [before, after]).flat();

describe("sampleCostMs", () => {
  test("a page that stays the same is sampled at its cost: 5% up to the bound", () => {
    for (const cost of [2, 25, 60, 150, 217, EDGE_MS]) {
      const steady = Array.from({ length: 20 }, () => cost);
      expect(meanAfter(steady)).toBeCloseTo(sampleMeanMs(cost, true), 6);
      expect((2 * cost) / meanAfter(steady)).toBeLessThanOrEqual(CAP + 1e-12);
    }
  });

  test("a sample's two comparisons may cost differently and still take about 5%", () => {
    // The comparison after the paint costs less (2026-11-05: a plain minimum gave about 7%).
    const pairs: [number, number][] = [
      [70, 45],
      [200, 120],
      [600, 300],
    ];
    for (const [before, after] of pairs) {
      const costs = samples(20, before, after);
      // The bound does not bind: the cost is the average.
      const { average: settled, latest: last, previous: prior } = afterComparisons(costs);
      expect(sampleCostMs(settled, last, prior)).toBe(settled);
      // Each sample's share against the mean drawn after the previous one.
      const share = (before + after) / meanAfter(costs);
      expect(share).toBeLessThanOrEqual(CAP * 1.05);
      expect(share).toBeGreaterThan(CAP * 0.95);
      // A plain minimum of the average and the latest comparison would sample more often than 5%.
      const { average, latest } = afterComparisons(costs);
      expect((before + after) / sampleMeanMs(Math.min(average, latest), true)).toBeGreaterThan(CAP * 1.2);
    }
  });

  test("the moving average starts at the first comparison and moves a fifth of the way each time", () => {
    expect(averageCompareCost(0, 40)).toBe(40);
    expect(averageCompareCost(40, 140)).toBe(60);
    expect(afterComparisons([100, 100, 100])).toEqual({ average: 100, latest: 100, previous: 100 });
    expect(SAMPLE_RECENT_FACTOR).toBe(2);
  });

  test("one costly comparison spaces out the samples only until the next two comparisons", () => {
    // A page that costs 60 ms, one comparison of 2 s, then 60 ms again.
    const spike = [...Array.from({ length: 10 }, () => 60), 2_000];
    // The average takes a fifth of the spike: the mean drawn right after it is longer...
    expect(meanAfter(spike)).toBeCloseTo(2 * SAMPLE_COST_FACTOR * (0.8 * 60 + 0.2 * 2_000), 6);
    // One comparison after it the spike is still one of the latest two: the average decides.
    const averageAfterOne = 0.8 * (0.8 * 60 + 0.2 * 2_000) + 0.2 * 60;
    expect(meanAfter([...spike, 60])).toBeCloseTo(2 * SAMPLE_COST_FACTOR * averageAfterOne, 6);
    // ...and the second comparison after it brings it to at most twice the page's cost, where the
    // average alone would still space the samples out more than five times as far as the page's.
    expect(meanAfter([...spike, 60, 60])).toBe(sampleMeanMs(SAMPLE_RECENT_FACTOR * 60, true));
    const { average } = afterComparisons([...spike, 60, 60]);
    expect(sampleMeanMs(average, true)).toBeGreaterThan(5 * sampleMeanMs(60, true));
    // The same after a first comparison that cost much more than the page does from then on.
    expect(meanAfter([4_000, 60, 60])).toBe(sampleMeanMs(SAMPLE_RECENT_FACTOR * 60, true));
  });

  test("the cost is never above the average or twice the costlier of the latest two", () => {
    const cases: [number, number, number][] = [
      [60, 60, 60],
      [60, 10, 10],
      [60, 10, 30],
      [10, 60, 60],
      [0, 0, 0],
      [800, 1e9, 1e9],
      [800, 1e9, 1],
    ];
    for (const [average, latest, previous] of cases) {
      const cost = sampleCostMs(average, latest, previous);
      expect(cost).toBeLessThanOrEqual(average);
      expect(cost).toBeLessThanOrEqual(SAMPLE_RECENT_FACTOR * Math.max(latest, previous));
      expect([average, SAMPLE_RECENT_FACTOR * latest, SAMPLE_RECENT_FACTOR * previous]).toContain(cost);
    }
  });

  test("a page that became costlier is sampled at its new cost within a few comparisons", () => {
    // From 60 ms to 200 ms: after 14 comparisons the mean is within 5% of the new cost's.
    const grown = [...Array.from({ length: 10 }, () => 60), ...Array.from({ length: 14 }, () => 200)];
    expect(meanAfter(grown)).toBeGreaterThan(0.95 * sampleMeanMs(200, true));
    expect(meanAfter(grown)).toBeLessThan(sampleMeanMs(200, true));
  });
});

describe("beatIsLate", () => {
  const due = 10_000;

  test("a beat late by more than LATE_BEAT_MS with no comparison of the runtime's is late", () => {
    expect(LATE_BEAT_MS).toBe(250);
    expect(beatIsLate(due + LATE_BEAT_MS, due, 0)).toBe(false);
    expect(beatIsLate(due + LATE_BEAT_MS + 1, due, 0)).toBe(true);
    expect(beatIsLate(due + 4_000, due, 0)).toBe(true);
  });

  test("the runtime's own comparisons after the beat was due do not make it late", () => {
    // One sample of a large page: two comparisons of 178 ms back to back, the first starting
    // 10 ms before the beat was due (2026-11-05 measurement).
    const first = overdueMs(due - 10, due + 168, due);
    const second = overdueMs(due + 168, due + 346, due);
    expect(first).toBe(168);
    expect(second).toBe(178);
    expect(beatIsLate(due + 350, due, first + second)).toBe(false);
    // The same delay without the runtime's comparisons is a blocked thread.
    expect(beatIsLate(due + 350, due, 0)).toBe(true);
  });

  test("what the page blocks still counts in full beside the runtime's comparisons", () => {
    // The runtime compares 300 ms after the beat was due, then the page blocks 260 ms.
    const own = overdueMs(due, due + 300, due);
    expect(beatIsLate(due + 300 + 260, due, own)).toBe(true);
    expect(beatIsLate(due + 300 + 240, due, own)).toBe(false);
  });

  test("only the part of a comparison after the beat was due counts", () => {
    expect(overdueMs(due - 500, due - 100, due)).toBe(0);
    expect(overdueMs(due - 100, due + 50, due)).toBe(50);
    expect(overdueMs(due + 20, due + 70, due)).toBe(50);
    // A comparison before the beat was due cannot hide a block after it.
    const before = overdueMs(due - 300, due - 1, due);
    expect(beatIsLate(due + 300, due, before)).toBe(true);
  });
});
