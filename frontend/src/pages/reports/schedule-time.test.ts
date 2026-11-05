import { describe, expect, test } from "bun:test";

import { addDays, nextOccurrence, parseTime, weekdayOf, zonedNow } from "./schedule-time.ts";

const weekly = (sendOn: number, time: string, timeZone = "UTC") => ({
  kind: "weekly_energy" as const,
  sendOn,
  time,
  timeZone,
});
const monthly = (sendOn: number, time: string, timeZone = "UTC") => ({
  kind: "monthly_carbon" as const,
  sendOn,
  time,
  timeZone,
});

describe("calendar helpers", () => {
  test("add whole days across month and year ends, and name the weekday (1 Monday to 7 Sunday)", () => {
    expect(addDays("2026-10-31", 1)).toBe("2026-11-01");
    expect(addDays("2026-01-01", -1)).toBe("2025-12-31");
    expect(weekdayOf("2026-11-02")).toBe(1);
    expect(weekdayOf("2026-11-01")).toBe(7);
  });

  test("read a time of day, and nothing else", () => {
    expect(parseTime("08:00")).toBe(480);
    expect(parseTime("23:59")).toBe(1439);
    for (const bad of ["", "8:00", "24:00", "12:60", "08:00:00", "ab:cd"]) expect(parseTime(bad)).toBeNull();
  });

  test("tell the local date and minutes in a time zone", () => {
    expect(zonedNow(new Date("2026-11-01T20:00:00Z"), "Asia/Taipei")).toEqual({ date: "2026-11-02", minutes: 240 });
    expect(zonedNow(new Date("2026-11-01T20:00:00Z"), "UTC")).toEqual({ date: "2026-11-01", minutes: 1200 });
  });
});

describe("the first send of a schedule", () => {
  test("a weekly report goes out on the next such weekday and covers the week before", () => {
    expect(nextOccurrence(new Date("2026-10-30T12:00:00Z"), weekly(1, "08:00"))).toEqual({
      sendDate: "2026-11-02",
      periodStart: "2026-10-26",
      periodEnd: "2026-11-01",
    });
  });

  test("on the day itself, before the time it is today and after it is next week", () => {
    expect(nextOccurrence(new Date("2026-11-02T07:00:00Z"), weekly(1, "08:00"))?.sendDate).toBe("2026-11-02");
    expect(nextOccurrence(new Date("2026-11-02T08:00:00Z"), weekly(1, "08:00"))?.sendDate).toBe("2026-11-09");
    expect(nextOccurrence(new Date("2026-11-02T09:00:00Z"), weekly(1, "08:00"))).toEqual({
      sendDate: "2026-11-09",
      periodStart: "2026-11-02",
      periodEnd: "2026-11-08",
    });
  });

  test("a report sent on a Sunday covers the week that ended the Sunday before", () => {
    expect(nextOccurrence(new Date("2026-10-28T00:00:00Z"), weekly(7, "08:00"))).toEqual({
      sendDate: "2026-11-01",
      periodStart: "2026-10-19",
      periodEnd: "2026-10-25",
    });
  });

  test("the day is the one in the schedule's time zone", () => {
    // 20:00 UTC on a Sunday is already Monday 04:00 in Taipei.
    const now = new Date("2026-11-01T20:00:00Z");
    expect(nextOccurrence(now, weekly(1, "08:00", "Asia/Taipei"))?.sendDate).toBe("2026-11-02");
    expect(nextOccurrence(now, weekly(1, "08:00", "UTC"))?.sendDate).toBe("2026-11-02");
    expect(nextOccurrence(now, weekly(7, "21:00", "UTC"))?.sendDate).toBe("2026-11-01");
    expect(nextOccurrence(now, weekly(7, "21:00", "Asia/Taipei"))?.sendDate).toBe("2026-11-08");
  });

  test("a monthly report goes out on its day and covers the month before, across the new year too", () => {
    expect(nextOccurrence(new Date("2026-10-15T12:00:00Z"), monthly(1, "08:00"))).toEqual({
      sendDate: "2026-11-01",
      periodStart: "2026-10-01",
      periodEnd: "2026-10-31",
    });
    expect(nextOccurrence(new Date("2026-12-20T12:00:00Z"), monthly(1, "08:00"))).toEqual({
      sendDate: "2027-01-01",
      periodStart: "2026-12-01",
      periodEnd: "2026-12-31",
    });
    expect(nextOccurrence(new Date("2026-03-10T12:00:00Z"), monthly(5, "08:00"))).toEqual({
      sendDate: "2026-04-05",
      periodStart: "2026-03-01",
      periodEnd: "2026-03-31",
    });
  });

  test("February's last day is the end of its period", () => {
    expect(nextOccurrence(new Date("2027-02-20T00:00:00Z"), monthly(1, "08:00"))).toEqual({
      sendDate: "2027-03-01",
      periodStart: "2027-02-01",
      periodEnd: "2027-02-28",
    });
  });

  test("a time that is not a time gives nothing", () => {
    expect(nextOccurrence(new Date("2026-11-02T07:00:00Z"), weekly(1, "25:00"))).toBeNull();
  });
});
