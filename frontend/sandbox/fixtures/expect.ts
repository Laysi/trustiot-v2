// How a verdict meets a fixture's expectation (frontend/sandbox/README.md, 「固定樣本」), for every
// test that runs the fixtures: the frontend's unit test, the checking service's image test and
// the assistant's acceptance tests.
// Only types come from protocol.ts, and the constant from constants.ts: the dashboard check image's
// test loads this module without the frontend's packages.
import type { CheckVerdict, Problem } from "../src/protocol.ts";

export { MAX_PAGE_BYTES } from "../src/constants.ts";

/** A case's `expect` in cases.json. */
export type Expectation =
  | { verdict: "passed"; marks: readonly unknown[] }
  | {
      verdict: "failed";
      /** Problems the verdict must have, each with at least these fields and values. */
      problems: readonly Record<string, unknown>[];
      /** Problem kinds the verdict may have besides the expected ones. */
      may_also?: readonly Problem["kind"][];
    };

const same = (a: unknown, b: unknown): boolean => JSON.stringify(a) === JSON.stringify(b);

/**
 * Whether `verdict` meets `expectation`: a passing case needs exactly its marks, in order; a
 * failing case needs every expected problem (a problem with the expected fields and values; other
 * fields, such as a script error's message, may differ when the expectation leaves them out) and
 * no problem of a kind that is neither expected nor in `may_also`.
 */
export function matchesExpectation(verdict: CheckVerdict, expectation: Expectation): boolean {
  if (verdict.verdict !== expectation.verdict) return false;
  if (verdict.verdict === "passed" || expectation.verdict === "passed") {
    return (
      verdict.verdict === "passed" && expectation.verdict === "passed" && same(verdict.report.marks, expectation.marks)
    );
  }
  const has = (expected: Record<string, unknown>) =>
    verdict.problems.some((problem) =>
      Object.entries(expected).every(([field, value]) => same((problem as Record<string, unknown>)[field], value)),
    );
  if (!expectation.problems.every(has)) return false;
  const allowed = new Set<unknown>([
    ...expectation.problems.map((problem) => problem.kind),
    ...(expectation.may_also ?? []),
  ]);
  return verdict.problems.every((problem) => allowed.has(problem.kind));
}
