// The fixture pages (frontend/sandbox/fixtures, README 「固定樣本」): their files and expectations
// are well formed, and every case marked `unit` gets its expected verdict from the runtime and the
// comparison in a DOM. The checking service's image test runs every case in a browser.
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";

import { z } from "../../src/lib/zod.ts";
import { MAX_PAGE_BYTES, matchesExpectation } from "../fixtures/expect.ts";
import { type CheckRequest, CheckRequestSchema, CheckVerdictSchema, PROBLEM_KINDS } from "../src/protocol.ts";
import { checkInFrame } from "./harness.ts";

const FIXTURES = new URL("../fixtures/", import.meta.url);

const CasesSchema = z.strictObject({
  request: z.literal("request.json"),
  cases: z.array(
    z.strictObject({
      name: z.string().regex(/^[a-z0-9-]+$/),
      page: z.string().regex(/^pages\/[a-z0-9-]+\.html$/),
      unit: z.boolean(),
      about: z.string().min(1),
      expect: z.union([
        z.strictObject({ verdict: z.literal("passed"), marks: z.array(z.unknown()) }),
        z.strictObject({
          verdict: z.literal("failed"),
          problems: z.array(z.record(z.string(), z.unknown())).min(1),
          may_also: z.array(z.enum(PROBLEM_KINDS)).optional(),
        }),
      ]),
    }),
  ),
});

const cases = CasesSchema.parse(JSON.parse(readFileSync(new URL("cases.json", FIXTURES), "utf8")));
const base = JSON.parse(readFileSync(new URL("request.json", FIXTURES), "utf8")) as Omit<CheckRequest, "page">;
const pageOf = (path: string) => readFileSync(new URL(path, FIXTURES), "utf8");
const requestOf = (path: string): CheckRequest => CheckRequestSchema.parse({ ...base, page: pageOf(path) });

// The two that wait out the render time limit, and the rest, take far less than this.
const CASE_LIMIT_MS = 60_000;

describe("the fixtures", () => {
  test("every page has one case and every case one page, within the page size limit", () => {
    const pages = readdirSync(new URL("pages/", FIXTURES)).sort();
    expect(cases.cases.map((entry) => entry.page.slice("pages/".length)).sort()).toEqual(pages);
    expect(new Set(cases.cases.map((entry) => entry.name)).size).toBe(cases.cases.length);
    for (const entry of cases.cases) {
      expect(entry.page).toBe(`pages/${entry.name}.html`);
      const bytes = new TextEncoder().encode(pageOf(entry.page)).length;
      expect(bytes).toBeGreaterThan(0);
      expect(bytes).toBeLessThanOrEqual(MAX_PAGE_BYTES);
    }
  });

  test("the request parses as a check request once a page is added", () => {
    expect(() => requestOf("pages/correct.html")).not.toThrow();
  });

  test("every expected verdict is one the contract allows", () => {
    for (const entry of cases.cases) {
      const verdict =
        entry.expect.verdict === "passed"
          ? { verdict: "passed", report: { marks: entry.expect.marks } }
          : { verdict: "failed", problems: entry.expect.problems };
      expect(() => CheckVerdictSchema.parse(verdict)).not.toThrow();
    }
  });

  test("the cases cover design B's list and run the checks a DOM can show", () => {
    const names = new Set(cases.cases.map((entry) => entry.name));
    for (const name of [
      "correct",
      "literal-number",
      "wrong-row",
      "wrong-bar-height",
      "unmapped-mark",
      "script-error-sync",
      "script-error-async",
      "navigation-location",
      "prototype-tampering",
    ]) {
      expect(names.has(name)).toBe(true);
    }
    expect(cases.cases.filter((entry) => entry.unit).length).toBeGreaterThanOrEqual(20);
  });
});

describe("matchesExpectation", () => {
  const failed = { verdict: "failed" as const, problems: [{ kind: "navigated" as const }] };

  test("needs every expected problem and allows only the listed kinds besides", () => {
    expect(matchesExpectation(failed, { verdict: "failed", problems: [{ kind: "navigated" }] })).toBe(true);
    expect(
      matchesExpectation(
        { verdict: "failed", problems: [{ kind: "navigated" }, { kind: "not_rendered" }] },
        { verdict: "failed", problems: [{ kind: "navigated" }] },
      ),
    ).toBe(false);
    expect(
      matchesExpectation(
        { verdict: "failed", problems: [{ kind: "navigated" }, { kind: "not_rendered" }] },
        { verdict: "failed", problems: [{ kind: "navigated" }], may_also: ["not_rendered"] },
      ),
    ).toBe(true);
    expect(matchesExpectation(failed, { verdict: "failed", problems: [{ kind: "requested" }] })).toBe(false);
  });

  test("compares the given fields of a problem and the whole list of marks", () => {
    const verdict = { verdict: "failed" as const, problems: [{ kind: "script_error" as const, message: "Error: x" }] };
    expect(matchesExpectation(verdict, { verdict: "failed", problems: [{ kind: "script_error" }] })).toBe(true);
    expect(matchesExpectation(verdict, { verdict: "failed", problems: [{ kind: "script_error", message: "y" }] })).toBe(
      false,
    );
    const passed = { verdict: "passed" as const, report: { marks: [{ reference: "f|week_kwh", value: 690 }] } };
    expect(matchesExpectation(passed, { verdict: "passed", marks: [{ reference: "f|week_kwh", value: 690 }] })).toBe(
      true,
    );
    expect(matchesExpectation(passed, { verdict: "passed", marks: [] })).toBe(false);
    expect(matchesExpectation(passed, { verdict: "failed", problems: [{ kind: "navigated" }] })).toBe(false);
  });
});

describe("the fixtures a DOM can check", () => {
  for (const entry of cases.cases.filter((candidate) => candidate.unit)) {
    test(
      `${entry.name}: ${entry.expect.verdict}`,
      async () => {
        const verdict = await checkInFrame(requestOf(entry.page));
        expect(CheckVerdictSchema.parse(verdict)).toEqual(verdict);
        // Here the verdict is the expectation itself: the same problems in the same order.
        if (entry.expect.verdict === "passed") {
          expect(verdict).toEqual({ verdict: "passed", report: { marks: entry.expect.marks } } as never);
        } else {
          expect(verdict).toEqual({ verdict: "failed", problems: entry.expect.problems } as never);
        }
        expect(matchesExpectation(verdict, entry.expect)).toBe(true);
      },
      CASE_LIMIT_MS,
    );
  }
});
