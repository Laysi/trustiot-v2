// Times in results (src/time.ts; README 「時間」): a page sees the wall time the query wrote, and
// instants turn into the viewer's zone.
import { describe, expect, test } from "bun:test";

import { instantToWall, parseInstant, parseWallTime, tickTimeDate, tickTimeText } from "../src/time.ts";

const fields = (date: Date | null) =>
  date === null
    ? null
    : [
        date.getUTCFullYear(),
        date.getUTCMonth() + 1,
        date.getUTCDate(),
        date.getUTCHours(),
        date.getUTCMinutes(),
        date.getUTCSeconds(),
        date.getUTCMilliseconds(),
      ];

describe("parseWallTime", () => {
  test("a date, an SQL time and RFC 3339 keep the written date and time of day", () => {
    expect(fields(parseWallTime("2026-10-20"))).toEqual([2026, 10, 20, 0, 0, 0, 0]);
    expect(fields(parseWallTime("2026-10-26 07:00:00"))).toEqual([2026, 10, 26, 7, 0, 0, 0]);
    expect(fields(parseWallTime("2026-10-26T01:02:03.456Z"))).toEqual([2026, 10, 26, 1, 2, 3, 456]);
    // The offset is ignored: the semantic sources write the request's zone.
    expect(fields(parseWallTime("2026-10-26T09:02:03+08:00"))).toEqual([2026, 10, 26, 9, 2, 3, 0]);
    expect(fields(parseWallTime("2026-10-26 09:02:03-05:30"))).toEqual([2026, 10, 26, 9, 2, 3, 0]);
  });

  test("fractions keep milliseconds and drop the rest", () => {
    expect(fields(parseWallTime("2026-10-26 07:00:00.5"))?.[6]).toBe(500);
    expect(fields(parseWallTime("2026-10-26 07:00:00.123456789"))?.[6]).toBe(123);
    expect(fields(parseWallTime("2026-10-26T07:00:00.09Z"))?.[6]).toBe(90);
  });

  test("years below 100 stay as written", () => {
    expect(fields(parseWallTime("0050-01-01"))).toEqual([50, 1, 1, 0, 0, 0, 0]);
  });

  test("anything else is not a time", () => {
    for (const text of [
      "2026-02-30",
      "2026-13-01",
      "2026-00-10",
      "2026-10-20 24:00:00",
      "2026-10-20 07:60:00",
      "2026-10-20 07:00:60",
      "2026-10-20 07:00",
      "2026-10-20T07:00:00",
      "2026-10-20 07:00:00.",
      "2026-10-20 07:00:00.1234567890",
      "20261020",
      " 2026-10-20",
      "2026-10-20 ",
      "2026-10-20T07:00:00+0800",
      "",
    ]) {
      expect(parseWallTime(text)).toBeNull();
    }
  });
});

describe("parseInstant", () => {
  test("the instant an offset names", () => {
    expect(parseInstant("2026-10-26T09:02:03+08:00")).toBe(Date.UTC(2026, 9, 26, 1, 2, 3));
    expect(parseInstant("2026-10-26T01:02:03.456Z")).toBe(Date.UTC(2026, 9, 26, 1, 2, 3, 456));
    expect(parseInstant("2026-10-25T19:32:03-05:30")).toBe(Date.UTC(2026, 9, 26, 1, 2, 3));
  });

  test("a time without an offset or with an impossible one is not an instant", () => {
    expect(parseInstant("2026-10-26 01:02:03")).toBeNull();
    expect(parseInstant("2026-10-26")).toBeNull();
    expect(parseInstant("2026-10-26T01:02:03+24:00")).toBeNull();
    expect(parseInstant("2026-10-26T01:02:03+08:60")).toBeNull();
  });
});

describe("instantToWall", () => {
  test("the wall time in the viewer's zone, milliseconds kept", () => {
    expect(fields(instantToWall("2026-10-26T01:02:03.456Z", "Asia/Taipei"))).toEqual([2026, 10, 26, 9, 2, 3, 456]);
    // New York is still on daylight time on 2026-10-26 (until 2026-11-01).
    expect(fields(instantToWall("2026-10-26T01:02:03.456Z", "America/New_York"))).toEqual([
      2026, 10, 25, 21, 2, 3, 456,
    ]);
  });

  test("a zone the browser does not know reads as UTC", () => {
    expect(fields(instantToWall("2026-10-26T01:02:03.456Z", "Mars/Olympus_Mons"))).toEqual([
      2026, 10, 26, 1, 2, 3, 456,
    ]);
  });

  test("text that is not an instant is not converted", () => {
    expect(instantToWall("2026-10-26 01:02:03", "Asia/Taipei")).toBeNull();
  });
});

describe("tick times", () => {
  test("a wall time as a tick reference writes it, and back", () => {
    const date = new Date(Date.UTC(2026, 9, 20, 7, 30, 0, 5));
    expect(tickTimeText(date)).toBe("2026-10-20T07:30:00.005");
    expect(tickTimeDate("2026-10-20T07:30:00.005").getTime()).toBe(date.getTime());
  });

  test("an invalid date or a year outside 0000 to 9999 has no tick text", () => {
    expect(tickTimeText(new Date(Number.NaN))).toBeNull();
    expect(tickTimeText(new Date(Date.UTC(10000, 0, 1)))).toBeNull();
    expect(tickTimeText(new Date(Date.UTC(-1, 0, 1)))).toBeNull();
    const year0 = new Date(0);
    year0.setUTCFullYear(0, 0, 1);
    expect(tickTimeText(year0)).toBe("0000-01-01T00:00:00.000");
  });
});
