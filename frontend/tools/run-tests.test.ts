import { describe, expect, test } from "bun:test";

import { parseSummary, shardCount } from "./run-tests.ts";

describe("parseSummary", () => {
  test("reads the counts of a bun test summary", () => {
    const output = [
      "src/app/paths.test.ts:",
      "(pass) a path > is encoded",
      "",
      " 345 pass",
      " 2 fail",
      " 1 skip",
      " 901 expect() calls",
      "Ran 348 tests across 21 files. [41.20s]",
    ].join("\n");
    expect(parseSummary(output)).toEqual({ pass: 345, fail: 2, tests: 348, files: 21 });
  });

  test("a run with one test and one file", () => {
    expect(parseSummary(" 1 pass\n 0 fail\nRan 1 test across 1 file. [5.00ms]")).toEqual({
      pass: 1,
      fail: 0,
      tests: 1,
      files: 1,
    });
  });

  test("a run that crashed before its summary counts nothing", () => {
    expect(parseSummary("src/pages/home/Home.test.tsx:\nSegmentation fault")).toEqual({
      pass: 0,
      fail: 0,
      tests: 0,
      files: 0,
    });
  });

  test("a test whose name ends in a count is not read as the summary", () => {
    const output = "(pass) shows 3 pass\n 1 pass\n 0 fail\nRan 1 test across 1 file.";
    expect(parseSummary(output).pass).toBe(1);
  });
});

describe("shardCount", () => {
  test("defaults to 4 shards", () => {
    expect(shardCount(undefined)).toBe(4);
    expect(shardCount("")).toBe(4);
  });

  test("takes a whole number of at least 1", () => {
    expect(shardCount("1")).toBe(1);
    expect(shardCount("6")).toBe(6);
  });

  test("refuses anything else", () => {
    for (const value of ["0", "-2", "2.5", "four"]) {
      expect(() => shardCount(value)).toThrow("FRONTEND_TEST_SHARDS");
    }
  });
});
