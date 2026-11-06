import { describe, expect, test } from "bun:test";

import { diffLines } from "./diff.ts";

describe("diffLines", () => {
  test("marks removed, added and unchanged lines in order", () => {
    const lines = diffLines("a\nb < 6.0\nc", "a\nguard\nb < 6.2\nc");
    expect(lines.map((line) => `${line.kind}:${line.text}`)).toEqual([
      "same:a",
      "del:b < 6.0",
      "add:guard",
      "add:b < 6.2",
      "same:c",
    ]);
  });

  test("gives every line its own id, also when the text repeats", () => {
    const lines = diffLines("x\nx", "x\nx\nx");
    expect(new Set(lines.map((line) => line.id)).size).toBe(lines.length);
  });

  test("equal scripts have no change", () => {
    expect(diffLines("a\nb", "a\nb").every((line) => line.kind === "same")).toBe(true);
  });
});
