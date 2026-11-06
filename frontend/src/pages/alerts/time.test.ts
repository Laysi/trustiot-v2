import { describe, expect, test } from "bun:test";

import { formatFull, formatWhen } from "./time.ts";

const NOW = Date.parse("2026-11-04T10:00:00Z");
const MINUTE = 60_000;
const HOUR = 60 * MINUTE;

describe("formatWhen", () => {
  test("today: the time and how long ago", () => {
    expect(formatWhen(NOW - 12 * MINUTE, NOW, "en", "UTC")).toBe("09:48 (12 minutes ago)");
  });

  test("yesterday within 24 hours has the word and the relative time", () => {
    expect(formatWhen(NOW - 11 * HOUR, NOW, "en", "UTC")).toBe("yesterday 23:00 (11 hours ago)");
  });

  test("older this year: month, day and time, no relative time", () => {
    expect(formatWhen(Date.parse("2026-10-31T16:22:00Z"), NOW, "en", "UTC")).toBe("10-31 16:22");
  });

  test("an earlier year writes the year", () => {
    expect(formatWhen(Date.parse("2025-12-31T16:22:00Z"), NOW, "en", "UTC")).toBe("2025-12-31 16:22");
  });

  test("uses the user's time zone for the day", () => {
    // 23:30 UTC on the 3rd is 07:30 on the 4th in Taipei: today there, yesterday in UTC.
    const at = Date.parse("2026-11-03T23:30:00Z");
    expect(formatWhen(at, NOW, "en", "Asia/Taipei")).toContain("07:30");
    expect(formatWhen(at, NOW, "en", "UTC")).toContain("yesterday");
  });

  test("a time that has not come has no relative part", () => {
    expect(formatWhen(NOW + 40 * MINUTE, NOW, "en", "UTC")).toBe("10:40");
  });

  test("the full form carries the year", () => {
    expect(formatFull("2026-10-30T03:12:00Z", "UTC")).toBe("2026-10-30 03:12");
  });
});
