import { describe, expect, test } from "bun:test";

import { DEFAULT_PRESET, fromLocalInput, offsetAt, parseRange, resolveRange, toLocalInput, withRange } from "./time.ts";

const at = (iso: string) => Date.parse(iso);
const HOUR = 3_600_000;

describe("time zones", () => {
  test("the offset of a zone without daylight saving, and of one with it on either side of the jump", () => {
    expect(offsetAt(at("2026-10-30T12:00:00Z"), "Asia/Taipei")).toBe(8 * HOUR);
    expect(offsetAt(at("2026-10-30T12:00:00Z"), "UTC")).toBe(0);
    expect(offsetAt(at("2026-03-08T06:59:00Z"), "America/New_York")).toBe(-5 * HOUR);
    expect(offsetAt(at("2026-03-08T07:00:00Z"), "America/New_York")).toBe(-4 * HOUR);
    expect(offsetAt(at("2026-10-30T12:00:00Z"), "Asia/Kolkata")).toBe(5.5 * HOUR);
  });

  test("a datetime-local value is a wall-clock time in the user's zone", () => {
    expect(fromLocalInput("2026-10-30T09:00", "Asia/Taipei")).toBe(at("2026-10-30T01:00:00Z"));
    expect(fromLocalInput("2026-10-30T09:00", "UTC")).toBe(at("2026-10-30T09:00:00Z"));
    expect(toLocalInput(at("2026-10-30T01:00:00Z"), "Asia/Taipei")).toBe("2026-10-30T09:00");
    // Round trip at every hour across a daylight-saving year boundary.
    for (let instant = at("2026-03-07T00:00:00Z"); instant < at("2026-03-10T00:00:00Z"); instant += HOUR) {
      expect(fromLocalInput(toLocalInput(instant, "America/New_York"), "America/New_York")).toBe(instant);
    }
  });

  test("a wall time the spring jump skips moves past the gap; a repeated one is its first occurrence", () => {
    // 02:30 does not exist in New York on 2026-03-08: it comes out as 03:30 EDT.
    expect(fromLocalInput("2026-03-08T02:30", "America/New_York")).toBe(at("2026-03-08T07:30:00Z"));
    // 01:30 happens twice on 2026-11-01: the first is EDT.
    expect(fromLocalInput("2026-11-01T01:30", "America/New_York")).toBe(at("2026-11-01T05:30:00Z"));
  });

  test("anything that is not a date and time is refused", () => {
    for (const value of ["", "2026-10-30", "2026-02-31T10:00", "2026-10-30T24:00", "2026-10-30T10:60", "now"]) {
      expect(fromLocalInput(value, "UTC")).toBeNull();
    }
  });
});

describe("the range in the address bar", () => {
  test("a preset, fixed instants, and the default for anything else", () => {
    expect(parseRange(new URLSearchParams("range=7d"))).toEqual({ kind: "preset", preset: "7d" });
    expect(parseRange(new URLSearchParams("from=2026-10-01T00:00:00Z&to=2026-10-02T00:00:00Z"))).toEqual({
      kind: "custom",
      from: at("2026-10-01T00:00:00Z"),
      to: at("2026-10-02T00:00:00Z"),
    });
    for (const query of ["", "range=2y", "from=2026-10-02T00:00:00Z&to=2026-10-01T00:00:00Z", "from=x&to=y"]) {
      expect(parseRange(new URLSearchParams(query))).toEqual({ kind: "preset", preset: DEFAULT_PRESET });
    }
  });

  test("writing a range keeps the other parameters and drops the previous range", () => {
    const custom = withRange(new URLSearchParams("point=voltage&range=7d"), {
      kind: "custom",
      from: at("2026-10-01T00:00:00Z"),
      to: at("2026-10-02T00:00:00Z"),
    });
    expect(custom.toString()).toBe("point=voltage&from=2026-10-01T00%3A00%3A00.000Z&to=2026-10-02T00%3A00%3A00.000Z");
    const preset = withRange(custom, { kind: "preset", preset: "1h" });
    expect(preset.toString()).toBe("point=voltage&range=1h");
    // The default preset needs no parameter.
    expect(withRange(preset, { kind: "preset", preset: DEFAULT_PRESET }).toString()).toBe("point=voltage");
  });

  test("a preset ends at the anchor; fixed instants stay as they are", () => {
    const now = at("2026-10-30T12:00:00Z");
    expect(resolveRange({ kind: "preset", preset: "24h" }, now)).toEqual({ from: now - 24 * HOUR, to: now });
    expect(resolveRange({ kind: "custom", from: 1, to: 2 }, now)).toEqual({ from: 1, to: 2 });
  });
});
