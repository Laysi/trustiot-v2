// The parts of the browser end-to-end runner that need no environment: its arguments, the reading
// of `dev.ts env`, and that the pinned browser is the one the runner starts.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import { PLAYWRIGHT_IMAGE } from "../../frontend/e2e/image.ts";
import { parseArguments, parseEnv } from "./run.ts";

describe("parseArguments", () => {
  test("takes --out and --grep", () => {
    expect(parseArguments(["--out", "/x", "--grep", "sign out"])).toEqual({ out: "/x", grep: "sign out" });
    expect(parseArguments([])).toEqual({ out: undefined, grep: undefined });
  });

  test("refuses anything else, and a flag without a value", () => {
    expect(() => parseArguments(["--keep"])).toThrow("usage:");
    expect(() => parseArguments(["--out"])).toThrow("usage:");
    expect(() => parseArguments(["--out", "--grep", "x"])).toThrow("usage:");
  });
});

describe("parseEnv", () => {
  test("reads KEY=value lines and removes single quotes", () => {
    expect(parseEnv("A=1\nB='two words'\nC=\n\nnot a setting\n")).toEqual({ A: "1", B: "two words", C: "" });
  });
});

describe("the runner", () => {
  test("starts the pinned image, by digest, and never a floating one", () => {
    const source = readFileSync(new URL("./run.ts", import.meta.url), "utf8");
    expect(source).toContain("PLAYWRIGHT_IMAGE,");
    expect(PLAYWRIGHT_IMAGE).toMatch(/@sha256:[0-9a-f]{64}$/);
    expect(source).not.toMatch(/playwright:(latest|v[\d.]+-noble)["'`]/);
  });
});
