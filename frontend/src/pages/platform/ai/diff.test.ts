import { describe, expect, test } from "bun:test";

import {
  applyDefaultChange,
  countChanges,
  diffLines,
  findVariables,
  isChanged,
  splitRows,
  unknownVariables,
  wordSegments,
} from "./diff.ts";

describe("line differences", () => {
  test("marks added, deleted and unchanged lines with their numbers", () => {
    const lines = diffLines("a\nb\nc", "a\nB\nc\nd");
    expect(lines.map((line) => `${line.kind}:${line.text}:${line.oldNo ?? "-"}:${line.newNo ?? "-"}`)).toEqual([
      "context:a:1:1",
      "del:b:2:-",
      "add:B:-:2",
      "context:c:3:3",
      "add:d:-:4",
    ]);
    expect(countChanges(lines)).toEqual({ added: 2, deleted: 1 });
    expect(isChanged(diffLines("x", "x"))).toBe(false);
  });

  test("the empty text has no lines", () => {
    expect(diffLines("", "a").map((line) => line.kind)).toEqual(["add"]);
    expect(diffLines("a", "").map((line) => line.kind)).toEqual(["del"]);
  });

  test("words that changed within a line are told apart, Chinese by character", () => {
    const words = wordSegments("再列出數字。", "再列出依據的數字與期間。");
    expect(words.old.filter((part) => part.changed)).toEqual([]);
    expect(words.new.filter((part) => part.changed).map((part) => part.text)).toEqual(["依據的", "與期間"]);
    const latin = wordSegments("use alpha now", "use beta now");
    expect(latin.old.filter((part) => part.changed).map((part) => part.text)).toEqual(["alpha"]);
    expect(latin.new.filter((part) => part.changed).map((part) => part.text)).toEqual(["beta"]);
  });

  test("the side-by-side layout puts a replaced line next to its replacement", () => {
    const rows = splitRows(diffLines("a\nb\nc", "a\nB\nB2\nc"));
    expect(rows.map((row) => [row.left?.text ?? null, row.right?.text ?? null])).toEqual([
      ["a", "a"],
      ["b", "B"],
      [null, "B2"],
      ["c", "c"],
    ]);
  });
});

describe("applying the default's change to a modified prompt", () => {
  const from = "one\ntwo\nthree";
  const to = "one\ntwo\nnew line\nthree";

  test("adds the new line after the line it follows in the default", () => {
    expect(applyDefaultChange("one\nmine\ntwo\nthree", from, to)).toBe("one\nmine\ntwo\nnew line\nthree");
  });

  test("appends it when the modified prompt no longer has that line", () => {
    expect(applyDefaultChange("mine only", from, to)).toBe("mine only\nnew line");
  });

  test("drops a line the default deleted where the prompt still has it", () => {
    expect(applyDefaultChange("one\nthree\nmine", "one\ntwo\nthree", "one\nthree")).toBe("one\nthree\nmine");
    expect(applyDefaultChange("one\ntwo\nthree", "one\ntwo\nthree", "one\nthree")).toBe("one\nthree");
  });
});

describe("variables", () => {
  test("finds every {{name}} with its line, and the ones the service does not list", () => {
    const text = "hi {{user_language}}\n\n{{ today }} and {{customer_name}}";
    expect(findVariables(text).map((variable) => [variable.name, variable.line])).toEqual([
      ["user_language", 1],
      ["today", 3],
      ["customer_name", 3],
    ]);
    expect(unknownVariables(text, ["user_language", "today"]).map((variable) => variable.raw)).toEqual([
      "{{customer_name}}",
    ]);
  });

  test("an empty {{}} is unknown too", () => {
    expect(unknownVariables("a {{}} b", ["today"]).map((variable) => variable.raw)).toEqual(["{{}}"]);
  });
});

describe("applying the default's change: runs of lines", () => {
  test("keeps the order of several lines added at the start", () => {
    expect(applyDefaultChange("a", "a", "x\ny\na")).toBe("x\ny\na");
    expect(applyDefaultChange("mine\na", "a", "x\ny\na")).toBe("x\ny\nmine\na");
  });

  test("keeps the order of several lines added after a line", () => {
    expect(applyDefaultChange("a\nb", "a\nb", "a\nx\ny\nb")).toBe("a\nx\ny\nb");
  });
});

describe("applying the default's change: repeated lines", () => {
  test("a line added after the second of two equal lines goes after the second", () => {
    expect(applyDefaultChange("x\nx", "x\nx", "x\nx\nnew")).toBe("x\nx\nnew");
  });
});
