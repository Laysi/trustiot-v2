import { describe, expect, test } from "bun:test";

import type { Latest } from "../../api/devices.ts";
import { dataHealth, newestReading, staleAfterMs } from "./health.ts";

const SECOND = 1000;
const MINUTE = 60 * SECOND;
const NOW = Date.parse("2026-11-01T05:36:55Z");

function latest(...agesMs: number[]): Latest {
  return {
    device_id: "d",
    earliest: new Date(NOW - 7 * 86_400_000).toISOString(),
    points: agesMs.map((age, index) => ({
      point: `p${index}`,
      time: new Date(NOW - age).toISOString(),
      value: { type: "int" as const, value: index },
    })),
  };
}

describe("data health", () => {
  test("stalls after three polling intervals, but never before two minutes", () => {
    expect(staleAfterMs(10 * SECOND)).toBe(2 * MINUTE);
    expect(staleAfterMs(40 * SECOND)).toBe(2 * MINUTE);
    expect(staleAfterMs(60 * SECOND)).toBe(3 * MINUTE);
    expect(staleAfterMs(10 * MINUTE)).toBe(30 * MINUTE);
  });

  test("the newest reading decides, whichever point it is", () => {
    expect(newestReading(latest(5 * MINUTE, 12 * SECOND, 3600 * SECOND))).toBe(NOW - 12 * SECOND);
    expect(newestReading(latest())).toBeNull();
  });

  test("is ok up to the limit and stale after it", () => {
    const poll = 60 * SECOND;
    expect(dataHealth(latest(3 * MINUTE), poll, NOW)).toEqual({ kind: "ok", newest: NOW - 3 * MINUTE });
    expect(dataHealth(latest(3 * MINUTE + 1), poll, NOW)).toEqual({ kind: "stale", newest: NOW - 3 * MINUTE - 1 });
    // One slow point does not make the device stale while another is current.
    expect(dataHealth(latest(2 * 3600 * SECOND, 30 * SECOND), poll, NOW).kind).toBe("ok");
  });

  test("tells no readings from not knowing yet", () => {
    expect(dataHealth(undefined, 60 * SECOND, NOW)).toEqual({ kind: "unknown" });
    expect(dataHealth(latest(), 60 * SECOND, NOW)).toEqual({ kind: "none" });
  });
});
