// Formatting (frontend/sandbox/README.md, 「格式化」): what `tq.format` and the comparison print,
// and the formats they refuse because the format itself would add digits.
import { describe, expect, test } from "bun:test";

import { DEFAULT_NUMBER_FORMAT, FormatError, formatValue, MAX_FORMAT_LEN, NO_VALUE } from "../src/format.ts";
import { parseWallTime } from "../src/time.ts";

const wall = (text: string) => {
  const date = parseWallTime(text);
  if (date === null) throw new Error(`not a wall time: ${text}`);
  return date;
};

describe("numbers", () => {
  test("the default format groups thousands and keeps at most two decimals", () => {
    expect(DEFAULT_NUMBER_FORMAT).toBe(",.2~f");
    expect(formatValue(1234.5, undefined, "zh-TW")).toBe("1,234.5");
    expect(formatValue(1234.567, undefined, "en")).toBe("1,234.57");
    expect(formatValue(690, undefined, "zh-TW")).toBe("690");
    expect(formatValue(-0.5, undefined, "en")).toBe("\u22120.5");
  });

  test("a d3-format specifier with the locale's currency", () => {
    expect(formatValue(0.5, ".0%", "en")).toBe("50%");
    expect(formatValue(1234, "$,.0f", "zh-TW")).toBe("NT$1,234");
    expect(formatValue(1234, "$,.0f", "en")).toBe("$1,234");
    expect(formatValue(1500, ".2s", "en")).toBe("1.5k");
    expect(formatValue(5, "05", "en")).toBe("00005");
    expect(formatValue(5, "0=5", "en")).toBe("00005");
    expect(formatValue(5, "x>4", "en")).toBe("xxx5");
  });

  test("a format that pads with digits other than the 0 flag is refused", () => {
    expect(() => formatValue(5, "5>10", "en")).toThrow(FormatError);
    expect(() => formatValue(5, "0>10", "en")).toThrow(FormatError);
    // A full-width digit is a digit too.
    expect(() => formatValue(5, "\uFF15>10", "en")).toThrow(FormatError);
  });

  test("width, length and syntax limits", () => {
    expect(formatValue(5, ">32", "en")).toHaveLength(32);
    expect(() => formatValue(5, ">33", "en")).toThrow(FormatError);
    expect(() => formatValue(5, `${" ".repeat(MAX_FORMAT_LEN)}d`, "en")).toThrow(FormatError);
    // d3 would read an unknown type letter as `g`.
    expect(() => formatValue(5, "q", "en")).toThrow(FormatError);
    expect(() => formatValue(5, ".2F", "en")).toThrow(FormatError);
    expect(formatValue(255, "x", "en")).toBe("ff");
    expect(() => formatValue(5, ",.2~f!", "en")).toThrow(FormatError);
  });

  test("past the cache limit every format still formats", () => {
    const fills = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    for (const fill of fills) {
      for (let width = 2; width <= 8; width++) {
        expect(formatValue(7, `${fill}>${width}`, "en")).toBe(`${fill.repeat(width - 1)}7`);
      }
    }
  });
});

describe("other values", () => {
  test("null reads as an em dash whatever the format", () => {
    expect(NO_VALUE).toBe("\u2014");
    expect(formatValue(null, undefined, "zh-TW")).toBe(NO_VALUE);
    expect(formatValue(null, "5>10", "en")).toBe(NO_VALUE);
  });

  test("text as it is, booleans in the locale's words", () => {
    expect(formatValue("Line 2", ".2f", "en")).toBe("Line 2");
    expect(formatValue(true, undefined, "zh-TW")).toBe("是");
    expect(formatValue(false, undefined, "zh-TW")).toBe("否");
    expect(formatValue(true, undefined, "en")).toBe("Yes");
    expect(formatValue(false, undefined, "en")).toBe("No");
  });
});

describe("times", () => {
  test("the default is the date at midnight, else date, hour and minute", () => {
    expect(formatValue(wall("2026-10-20"), undefined, "zh-TW")).toBe("2026-10-20");
    expect(formatValue(wall("2026-10-26 07:30:00"), undefined, "en")).toBe("2026-10-26 07:30");
  });

  test("the wall time shows as the query wrote it, with the locale's names", () => {
    expect(formatValue(wall("2026-10-26 07:00:00"), "%H:%M", "en")).toBe("07:00");
    expect(formatValue(wall("2026-10-20"), "%b %-d", "zh-TW")).toBe("10月 20");
    expect(formatValue(wall("2026-10-20"), "%b %-d", "en")).toBe("Oct 20");
    expect(formatValue(wall("2026-10-20"), "%Y年%-m月%-d日", "zh-TW")).toBe("2026年10月20日");
    expect(formatValue(wall("2026-10-20"), "%A", "zh-TW")).toBe("星期二");
    expect(formatValue(wall("2026-10-20"), "%x", "en")).toBe("10/20/2026");
  });

  test("a time format may not write digits of its own or use an unknown directive", () => {
    expect(() => formatValue(wall("2026-10-20"), "%Y (99)", "en")).toThrow(FormatError);
    expect(() => formatValue(wall("2026-10-20"), "%Y\uFF19", "en")).toThrow(FormatError);
    expect(() => formatValue(wall("2026-10-20"), "%Z", "en")).toThrow(FormatError);
    expect(() => formatValue(wall("2026-10-20"), "%k", "en")).toThrow(FormatError);
    expect(() => formatValue(wall("2026-10-20"), "%Y %", "en")).toThrow(FormatError);
    expect(() => formatValue(wall("2026-10-20"), "%-", "en")).toThrow(FormatError);
    expect(() => formatValue(wall("2026-10-20"), "%".repeat(MAX_FORMAT_LEN + 1), "en")).toThrow(FormatError);
    expect(() => formatValue(wall("2026-10-20"), "100%%", "en")).toThrow(FormatError);
    expect(formatValue(wall("2026-10-20"), "%Y%%", "en")).toBe("2026%");
  });

  test("an invalid date reads as an em dash", () => {
    expect(formatValue(new Date(Number.NaN), "%Y", "en")).toBe(NO_VALUE);
  });
});
