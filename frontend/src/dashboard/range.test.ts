import { describe, expect, test } from "bun:test";

import { result } from "./fixtures.ts";
import { wallMs } from "./format.ts";
import { mergeTimeRanges, resultTimeRange } from "./range.ts";

const at = (text: string) => wallMs(text) as number;

describe("resultTimeRange", () => {
  const columns = [
    { name: "ts", type: "time" },
    { name: "kw", type: "number" },
    { name: "seen", type: "time" },
  ] as const;

  test("is the first and the last time over every time column, as the queries wrote them", () => {
    const range = resultTimeRange(
      result(
        "d",
        [...columns],
        [
          ["2026-11-01 00:15:00", 1, "2026-11-01 00:20:00"],
          ["2026-11-01 00:00:00", 2, null],
        ],
      ),
    );
    expect(range).toEqual({ from: at("2026-11-01 00:00:00"), to: at("2026-11-01 00:20:00"), dateOnly: false });
  });

  test("is of dates when every time is a date", () => {
    const range = resultTimeRange(
      result("d", [{ name: "day", type: "time" }], [["2026-10-30"], ["2026-10-26"], [null]]),
    );
    expect(range).toEqual({ from: at("2026-10-26"), to: at("2026-10-30"), dateOnly: true });
  });

  test("is nothing without a time column, without rows, or without a time that can be read", () => {
    expect(resultTimeRange(result("d", [{ name: "kw", type: "number" }], [[1]]))).toBeNull();
    expect(resultTimeRange(result("d", [{ name: "ts", type: "time" }], []))).toBeNull();
    expect(resultTimeRange(result("d", [{ name: "ts", type: "time" }], [[null], ["not a time"]]))).toBeNull();
  });
});

describe("mergeTimeRanges", () => {
  test("covers all the ranges, and is of dates only when each is", () => {
    const merged = mergeTimeRanges([{ from: 10, to: 20, dateOnly: true }, null, { from: 5, to: 15, dateOnly: true }]);
    expect(merged).toEqual({ from: 5, to: 20, dateOnly: true });
    expect(
      mergeTimeRanges([
        { from: 10, to: 20, dateOnly: true },
        { from: 12, to: 30, dateOnly: false },
      ]),
    ).toEqual({ from: 10, to: 30, dateOnly: false });
  });

  test("is nothing when there is no range", () => {
    expect(mergeTimeRanges([])).toBeNull();
    expect(mergeTimeRanges([null, null])).toBeNull();
  });
});
