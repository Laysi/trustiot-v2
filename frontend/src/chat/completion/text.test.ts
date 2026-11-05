import { describe, expect, test } from "bun:test";
import {
  applyEdit,
  detectTrigger,
  diffEdit,
  findSlots,
  type Mention,
  mentionInsertion,
  neighbourSlot,
  rank,
  reconcileMentions,
} from "./text.ts";
import { describeRange, timeRanges } from "./time-ranges.ts";

const device = (start: number, label: string): Mention => ({
  start,
  end: start + label.length,
  label,
  target: { kind: "device", deviceId: `id-${label}` },
});

describe("when an @ or a / opens a list", () => {
  test("an @ at the start, after a space and after punctuation opens it, with what follows it as the query", () => {
    expect(detectTrigger("@", 1, false)).toEqual({ mode: "mention", start: 0, query: "" });
    expect(detectTrigger("compare @comp", 13, false)).toEqual({ mode: "mention", start: 8, query: "comp" });
    expect(detectTrigger("比較，@空壓", 6, false)).toEqual({ mode: "mention", start: 3, query: "空壓" });
  });

  test("an @ inside a word (an address) does not, nor does a query with a space in it", () => {
    expect(detectTrigger("mail me@example", 15, false)).toBeNull();
    expect(detectTrigger("@Compressor A", 13, false)).toBeNull();
  });

  test("the caret decides, not the end of the text", () => {
    expect(detectTrigger("@co and more", 3, false)).toEqual({ mode: "mention", start: 0, query: "co" });
  });

  test("a / opens the templates only at the start and only when templates are on offer", () => {
    expect(detectTrigger("/abn", 4, true)).toEqual({ mode: "template", start: 0, query: "abn" });
    expect(detectTrigger("/abn", 4, false)).toBeNull();
    expect(detectTrigger("a /abn", 6, true)).toBeNull();
  });
});

describe("mentions follow the text", () => {
  test("text typed before a mention moves it; the same text after it leaves it", () => {
    const mentions = [device(2, "@A2")];
    expect(reconcileMentions(mentions, "x @A2 ", "xyz @A2 ")).toEqual([device(4, "@A2")]);
    expect(reconcileMentions(mentions, "x @A2 ", "x @A2 more")).toEqual([device(2, "@A2")]);
  });

  test("an edit inside a mention turns it back into plain text", () => {
    expect(reconcileMentions([device(0, "@Compressor")], "@Compressor ", "@Compresor ")).toEqual([]);
  });

  test("a word typed right after a mention runs into it and takes it away; a space or a mark does not", () => {
    const mentions = [device(0, "@A2")];
    expect(reconcileMentions(mentions, "@A2", "@A2s")).toEqual([]);
    expect(reconcileMentions(mentions, "@A2", "@A2 ")).toEqual(mentions);
    expect(reconcileMentions(mentions, "@A2", "@A2?")).toEqual(mentions);
  });

  test("deleting everything removes every mention", () => {
    expect(reconcileMentions([device(0, "@A2"), device(4, "@B1")], "@A2 @B1 ", "")).toEqual([]);
  });

  test("the edit between two texts is found at both ends", () => {
    expect(diffEdit("abcdef", "abXYef")).toEqual({ start: 2, end: 4, inserted: 2 });
    expect(diffEdit("abc", "abc")).toEqual({ start: 3, end: 3, inserted: 0 });
    expect(diffEdit("aa", "aaa")).toEqual({ start: 2, end: 2, inserted: 1 });
  });

  test("an edit that only touches a mention's edge drops it only when it covers a character", () => {
    const mentions = [device(2, "@A2")];
    expect(applyEdit(mentions, 5, 5, 3)).toEqual(mentions);
    expect(applyEdit(mentions, 0, 2, 0)).toEqual([device(0, "@A2")]);
    expect(applyEdit(mentions, 4, 5, 0)).toEqual([]);
  });
});

describe("inserting a mention", () => {
  test("puts the mention where the query was, with a space after it", () => {
    expect(mentionInsertion("@co", 0, 3, "Compressor A1")).toEqual({
      text: "@Compressor A1 ",
      caret: 15,
      labelStart: 0,
      labelEnd: 14,
    });
  });

  test("keeps the words around it apart with a space on each side, without doubling one that is there", () => {
    expect(mentionInsertion("Energy of［設備］", 9, 13, "A2").text).toBe("Energy of @A2 ");
    expect(mentionInsertion("在［時間範圍］的用電", 1, 7, "上週").text).toBe("在 @上週 的用電");
    expect(mentionInsertion("a ［x］ b", 2, 5, "N").text).toBe("a @N b");
  });
});

describe("the blanks of a template", () => {
  test("are found in order, with the words inside the brackets", () => {
    expect(findSlots("［設備］在［時間範圍］的用電量")).toEqual([
      { start: 0, end: 4, label: "設備" },
      { start: 5, end: 11, label: "時間範圍" },
    ]);
    expect(findSlots("no blanks")).toEqual([]);
  });

  test("Tab goes to the next blank and Shift+Tab to the one before, and neither wraps round", () => {
    const slots = findSlots("［a］ x ［b］ y ［c］");
    expect(neighbourSlot(slots, 0, 3, false)?.label).toBe("b");
    expect(neighbourSlot(slots, 8, 11, false)?.label).toBe("c");
    expect(neighbourSlot(slots, 16, 19, false)).toBeNull();
    expect(neighbourSlot(slots, 8, 11, true)?.label).toBe("a");
    expect(neighbourSlot(slots, 0, 3, true)).toBeNull();
  });
});

describe("searching the list", () => {
  const entries = [{ name: "Cooling tower" }, { name: "Air compressor" }, { name: "Compressor A1", words: "pressure" }];

  test("an empty query keeps the order given", () => {
    expect(rank(entries, "").map((entry) => entry.name)).toEqual(entries.map((entry) => entry.name));
  });

  test("names that start with the query come first, then the ones that hold it, and case does not matter", () => {
    expect(rank(entries, "COMP").map((entry) => entry.name)).toEqual(["Compressor A1", "Air compressor"]);
  });

  test("a word the entry carries (a data point's key) finds it, after the names", () => {
    expect(rank(entries, "pressur").map((entry) => entry.name)).toEqual(["Compressor A1"]);
    expect(rank(entries, "zzz")).toEqual([]);
  });
});

describe("the time ranges", () => {
  const sunday = Date.parse("2026-11-01T10:00:00Z");
  const range = (now: number, zone: string, id: string) => {
    const found = timeRanges(now, zone).find((candidate) => candidate.id === id);
    if (!found) throw new Error(`no range ${id}`);
    return [new Date(found.from).toISOString(), new Date(found.to).toISOString()];
  };

  test("last week on a Sunday is the Monday to Sunday before it (weeks start on Monday)", () => {
    expect(range(sunday, "UTC", "last_week")).toEqual(["2026-10-19T00:00:00.000Z", "2026-10-26T00:00:00.000Z"]);
    expect(range(sunday, "UTC", "this_week")).toEqual(["2026-10-26T00:00:00.000Z", "2026-11-01T10:00:00.000Z"]);
  });

  test("the day, the month, the last 7 days and the last quarter", () => {
    expect(range(sunday, "UTC", "today")).toEqual(["2026-11-01T00:00:00.000Z", "2026-11-01T10:00:00.000Z"]);
    expect(range(sunday, "UTC", "yesterday")).toEqual(["2026-10-31T00:00:00.000Z", "2026-11-01T00:00:00.000Z"]);
    expect(range(sunday, "UTC", "this_month")).toEqual(["2026-11-01T00:00:00.000Z", "2026-11-01T10:00:00.000Z"]);
    expect(range(sunday, "UTC", "last_month")).toEqual(["2026-10-01T00:00:00.000Z", "2026-11-01T00:00:00.000Z"]);
    expect(range(sunday, "UTC", "last_7_days")).toEqual(["2026-10-26T00:00:00.000Z", "2026-11-01T10:00:00.000Z"]);
    expect(range(sunday, "UTC", "last_quarter")).toEqual(["2026-07-01T00:00:00.000Z", "2026-10-01T00:00:00.000Z"]);
  });

  test("the first quarter looks back into the year before", () => {
    expect(range(Date.parse("2026-02-10T00:00:00Z"), "UTC", "last_quarter")).toEqual([
      "2025-10-01T00:00:00.000Z",
      "2026-01-01T00:00:00.000Z",
    ]);
  });

  test("the user's time zone decides the day: 01:00 on Monday in Taipei is still Sunday in UTC", () => {
    const now = Date.parse("2026-11-01T17:00:00Z");
    expect(range(now, "Asia/Taipei", "today")).toEqual(["2026-11-01T16:00:00.000Z", "2026-11-01T17:00:00.000Z"]);
    expect(range(now, "Asia/Taipei", "last_week")).toEqual(["2026-10-25T16:00:00.000Z", "2026-11-01T16:00:00.000Z"]);
  });

  test("a span is written with the real dates, a week with its weekdays", () => {
    const find = (id: string) => timeRanges(sunday, "UTC").find((candidate) => candidate.id === id);
    const describe = (id: string) => {
      const found = find(id);
      if (!found) throw new Error(id);
      return describeRange(found, "en", "UTC", "now");
    };
    expect(describe("last_week")).toBe("10/19 (Mon) – 10/25 (Sun)");
    expect(describe("yesterday")).toBe("10/31");
    expect(describe("last_month")).toBe("10/01 – 10/31");
    expect(describe("today")).toBe("11/01 00:00 – now");
    expect(describe("this_month")).toBe("11/01 – now");
  });
});
