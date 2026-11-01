import { describe, expect, test } from "bun:test";

import { dashboardFormat, NO_VALUE, wallMs } from "./format.ts";

/** ICU puts no-break spaces around some fields; the tests read them as plain spaces. */
const plain = (text: string) => text.replace(/[  ]/g, " ");
const at = (text: string) => wallMs(text) as number;
const DAY_MS = 86_400_000;

describe("dashboardFormat", () => {
  const en = dashboardFormat("en", "UTC");
  const zh = dashboardFormat("zh-TW", "UTC");

  test("numbers round for display only, to 3 decimals or 3 significant digits below 1", () => {
    expect([12.3456, 133.333, 0.000123, -7.25, 1000000.5].map(en.number)).toEqual([
      "12.346",
      "133.333",
      "0.000123",
      "-7.25",
      "1,000,000.5",
    ]);
    expect(en.number(null)).toBe(NO_VALUE);
    expect(en.number(Number.NaN)).toBe(NO_VALUE);
  });

  test("a time shows the precision it has, milliseconds included", () => {
    expect(plain(en.wallTime(at("2026-11-01 01:06:07.250")))).toBe("Nov 1, 2026, 1:06:07.250 AM");
    expect(plain(en.wallTime(at("2026-11-01 01:06:07")))).toBe("Nov 1, 2026, 1:06:07 AM");
    expect(plain(en.wallTime(at("2026-11-01 01:06:00")))).toBe("Nov 1, 2026, 1:06 AM");
    expect(plain(en.wallTime(at("2026-11-01"), true))).toBe("Nov 1, 2026");
    expect(plain(zh.wallTime(at("2026-11-01 01:06:07.250")))).toBe("2026年11月1日 凌晨1:06:07.250");
  });

  test("an axis label shows the finest unit its tick has, so zooming in never repeats a label", () => {
    const span = 45 * 60_000;
    expect(en.axisTime(at("2026-11-01 00:20:30"), span)).toBe("00:20:30");
    expect(en.axisTime(at("2026-11-01 00:20:30.500"), span)).toBe("00:20:30.500");
    expect(en.axisTime(at("2026-11-01 00:21:00"), span)).toBe("00:21");
    expect(en.axisTime(at("2026-11-01 00:00:00"), span)).toBe("11/1");
    // Over days, hours between midnights stay hours.
    expect(en.axisTime(at("2026-11-01 12:00:00"), 5 * DAY_MS)).toBe("12:00");
    // Over years, a month's first day names the month.
    expect(en.axisTime(at("2026-11-01 00:00:00"), 500 * DAY_MS)).toBe("11/2026");
    expect(en.axisTime(at("2026-11-16 00:00:00"), 500 * DAY_MS)).toBe("11/16");
  });
});
