// What references resolve to (src/values.ts), against the JSON the Rust types write: a passing
// verdict's values are `CheckRequest::value_of`'s, so the runtime, the comparison and the
// assistant agree on every value.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import { CheckRequestSchema, CheckVerdictSchema, parseReference, type Reference } from "../src/protocol.ts";
import { declares, type Model, type Resolved, resolve, resolves } from "../src/values.ts";

const DASHBOARD = "../../../crates/contract/tests/fixtures/dashboard/";
const fixture = (name: string): unknown =>
  JSON.parse(readFileSync(new URL(`${DASHBOARD}${name}`, import.meta.url), "utf8"));

const request = CheckRequestSchema.parse(fixture("check-request.json"));
const model: Model = { spec: request.spec, results: request.datasets, tz: request.tz };

/** The time a resolved value shows, or NaN when it is not a Date. */
const displayTime = (resolved: Resolved | null) =>
  resolved?.display instanceof Date ? resolved.display.getTime() : Number.NaN;

const ref = (text: string): Reference => {
  const parsed = parseReference(text);
  if (parsed === null) throw new Error(`not a reference: ${text}`);
  return parsed;
};

describe("the Rust fixtures", () => {
  test("every mark of the passing verdict has the value the reference resolves to", () => {
    const verdict = CheckVerdictSchema.parse(fixture("verdict-passed.json"));
    if (verdict.verdict !== "passed") throw new Error("verdict-passed.json is not a passing verdict");
    expect(verdict.report.marks.length).toBeGreaterThan(0);
    for (const mark of verdict.report.marks) {
      expect({ reference: mark.reference, value: resolve(model, ref(mark.reference))?.value }).toEqual(mark);
    }
  });
});

describe("resolves and declares", () => {
  test.each([
    // reference, resolves, declares
    ["c|daily_energy|kwh|2", true, true],
    ["c|daily_energy|kwh|3", false, true],
    ["c|daily_energy|watts|0", false, false],
    ["c|weekly_energy|kwh|0", false, false],
    ["t|daily_energy|kwh|5", true, true],
    ["t|daily_energy|watts|5", false, false],
    ["m|daily_energy|queried_at", true, true],
    ["m|daily_energy|row_count", true, true],
    ["m|daily_energy|range_from", false, true],
    ["m|meter_usage|range_to", true, true],
    ["m|weekly_energy|queried_at", false, false],
    ["f|week_kwh", true, true],
    ["f|month_kwh", false, false],
    ["k|line_name", true, true],
    ["k|line_code", false, false],
  ] as const)("%s", (text, resolvesTo, declaresTo) => {
    expect(resolves(model, ref(text))).toBe(resolvesTo);
    expect(declares(model, ref(text))).toBe(declaresTo);
    expect(resolve(model, ref(text)) !== null).toBe(resolvesTo);
  });

  test("a dataset that failed has no cells, but its meta references are still declared", () => {
    const failed: Model = { ...model, results: model.results.filter((result) => result.dataset !== "daily_energy") };
    expect(resolves(failed, ref("c|daily_energy|kwh|0"))).toBe(false);
    expect(declares(failed, ref("c|daily_energy|kwh|0"))).toBe(false);
    expect(resolves(failed, ref("m|daily_energy|queried_at"))).toBe(false);
    expect(declares(failed, ref("m|daily_energy|queried_at"))).toBe(true);
    // A computation over it still resolves, to null.
    expect(resolve(failed, ref("f|first_day_share"))?.value).toBeNull();
  });
});

describe("values and how a page sees them", () => {
  test("a cell: the value as the result holds it, a time as a wall-clock Date", () => {
    expect(resolve(model, ref("c|daily_energy|kwh|0"))).toEqual({ value: 120.5, display: 120.5 });
    expect(resolve(model, ref("c|daily_energy|kwh|2"))).toEqual({ value: null, display: null });
    const day = resolve(model, ref("c|daily_energy|day|0"));
    expect(day?.value).toBe("2026-10-20");
    expect(displayTime(day)).toBe(Date.UTC(2026, 9, 20));
  });

  test("computations and constants", () => {
    expect(resolve(model, ref("f|week_kwh"))?.value).toBe(1223.75);
    expect(resolve(model, ref("f|first_day_share"))?.value).toBe(120.5 / (120.5 + 98.25));
    expect(resolve(model, ref("k|target_kwh"))).toEqual({ value: 1200, display: 1200 });
    expect(resolve(model, ref("k|line_name"))).toEqual({ value: "Line 2", display: "Line 2" });
  });

  test("a computation whose formula does not parse has no value", () => {
    const broken: Model = {
      ...model,
      spec: { ...model.spec, computations: [{ name: "broken", title: "Broken", formula: "sum(" }] },
    };
    expect(resolves(broken, ref("f|broken"))).toBe(true);
    expect(resolve(broken, ref("f|broken"))).toBeNull();
  });

  test("meta: the instant as text, shown as the viewer's wall time", () => {
    const queried = resolve(model, ref("m|daily_energy|queried_at"));
    expect(queried?.value).toBe("2026-10-26T01:02:03.456Z");
    expect(displayTime(queried)).toBe(Date.UTC(2026, 9, 26, 9, 2, 3, 456));
    expect(resolve(model, ref("m|daily_energy|row_count"))).toEqual({ value: 3, display: 3 });
    const usage = model.results.find((result) => result.dataset === "meter_usage");
    if (usage === undefined || usage.meta.source.kind === "sql") throw new Error("meter_usage is not semantic");
    expect(resolve(model, ref("m|meter_usage|range_from"))?.value).toBe(usage.meta.source.range.from);
  });

  test("ticks: the number, or the time text as a wall-clock Date", () => {
    expect(resolve(model, ref("t|daily_energy|kwh|-2.5"))).toEqual({ value: -2.5, display: -2.5 });
    const tick = resolve(model, ref("t|daily_energy|day|2026-10-20T00:00:00.000"));
    expect(tick?.value).toBe("2026-10-20T00:00:00.000");
    expect(displayTime(tick)).toBe(Date.UTC(2026, 9, 20));
  });
});
