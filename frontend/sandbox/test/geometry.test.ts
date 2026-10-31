// The arithmetic of the geometry rules (src/geometry.ts; README 「檢查規則」).
import { describe, expect, test } from "bun:test";

import { nice } from "d3-array";
import { scaleUtc } from "d3-scale";
import { arc, pie } from "d3-shape";

import {
  disproportionate,
  numberAxisRange,
  offLine,
  pathArcs,
  robustLine,
  sliceAngle,
  timeAxisRange,
  within,
} from "../src/geometry.ts";

const sized = (pairs: [number, number][]) => pairs.map(([value, size]) => ({ value, size, unit: 1 }));

describe("disproportionate", () => {
  test("sizes proportional to the values' magnitudes pass, within 1% or one unit", () => {
    expect(
      disproportionate(
        sized([
          [10, 100],
          [20, 200],
          [-5, 50],
          [0, 0.5],
        ]),
      ),
    ).toEqual([false, false, false, false]);
    expect(
      disproportionate(
        sized([
          [100, 1000],
          [200, 2019],
        ]),
      ),
    ).toEqual([false, false]);
  });

  test("a bar off by more than the tolerance fails, and the median keeps the rest", () => {
    expect(
      disproportionate(
        sized([
          [10, 100],
          [20, 200],
          [30, 300],
          [40, 330],
        ]),
      ),
    ).toEqual([false, false, false, true]);
    expect(disproportionate(sized([[0, 3]]))).toEqual([true]);
  });

  test("a scale collapsed under one unit fails every mark with a value", () => {
    expect(
      disproportionate(
        sized([
          [10, 0.01],
          [20, 0.02],
          [0, 0],
        ]),
      ),
    ).toEqual([true, true, false]);
  });

  test("an angle's unit is its own", () => {
    const marks = [
      { value: 1, size: 1, unit: 0.01 },
      { value: 2, size: 2, unit: 0.01 },
      { value: 3, size: 3.05, unit: 0.01 },
    ];
    expect(disproportionate(marks)).toEqual([false, false, true]);
  });
});

describe("positions on a line", () => {
  const points = (pairs: [number, number][]) => pairs.map(([value, position]) => ({ value, position }));

  test("the robust line ignores one point off it", () => {
    const line = robustLine(
      points([
        [0, 10],
        [1, 12],
        [2, 14],
        [3, 16],
        [4, 100],
      ]),
    );
    expect(line.slope).toBeCloseTo(2, 10);
    expect(line.intercept).toBeCloseTo(10, 10);
  });

  test("off the line by more than 1% of the spread or a pixel", () => {
    expect(
      offLine(
        points([
          [0, 10],
          [1, 12],
          [2, 14],
          [3, 16],
          [4, 100],
        ]),
      ),
    ).toEqual([false, false, false, false, true]);
    expect(
      offLine(
        points([
          [0, 0],
          [1, 100],
          [2, 200.9],
        ]),
      ),
    ).toEqual([false, false, false]);
    // A descending axis (y grows down) is a line too.
    expect(
      offLine(
        points([
          [0, 400],
          [10, 300],
          [20, 200],
          [30, 100],
        ]),
      ),
    ).toEqual([false, false, false, false]);
  });

  test("fewer than three points are not judged", () => {
    expect(
      offLine(
        points([
          [0, 0],
          [1, 1000],
        ]),
      ),
    ).toEqual([false, false]);
  });

  test("many points use the paired slopes and still find the outlier", () => {
    const many = Array.from({ length: 200 }, (_, i) => ({ value: i, position: 3 * i + 7 }));
    const outlier = many[150];
    if (outlier !== undefined) outlier.position += 50;
    const off = offLine(many);
    expect(off.filter(Boolean).length).toBe(1);
    expect(off[150]).toBe(true);
  });
});

describe("axis ranges", () => {
  test("numbers: the values and 0, widened by nice() for every count up to 20", () => {
    const [from, to] = numberAxisRange([120.5, 98.25, 131]);
    expect(from).toBe(0);
    let widest = 131;
    for (let count = 1; count <= 20; count++) widest = Math.max(widest, nice(0, 131, count)[1]);
    expect(to).toBe(widest);
    expect(within(500, [from, to])).toBe(false);
    expect(numberAxisRange([-5, 3])[0]).toBeLessThanOrEqual(-5);
    expect(numberAxisRange([0, 0])).toEqual([0, 1]);
    expect(numberAxisRange([])).toEqual([0, 1]);
  });

  test("numbers that are all the same: one unit above them, and 0", () => {
    let widest = 6;
    for (let count = 1; count <= 20; count++) widest = Math.max(widest, nice(0, 6, count)[1]);
    expect(numberAxisRange([5, 5, 5])).toEqual([0, widest]);
    expect(within(6, numberAxisRange([5]))).toBe(true);
    const negative = numberAxisRange([-5, -5]);
    expect(negative[0]).toBeLessThanOrEqual(-5);
    expect(negative[1]).toBe(0);
    expect(within(-4, negative)).toBe(true);
  });

  test("times: the values' extent, a day each way around a single time, widened by nice()", () => {
    const day = Date.UTC(2026, 9, 20);
    const range = timeAxisRange([day, day + 6 * 86_400_000]);
    expect(range).not.toBeNull();
    const [from, to] = range ?? [0, 0];
    expect(from).toBeLessThanOrEqual(day);
    expect(to).toBeGreaterThanOrEqual(day + 6 * 86_400_000);
    const widest = scaleUtc()
      .domain([new Date(day), new Date(day + 6 * 86_400_000)])
      .nice(10)
      .domain();
    expect(from).toBeLessThanOrEqual(widest[0]?.getTime() ?? Number.NaN);
    expect(timeAxisRange([day])?.[0]).toBeLessThanOrEqual(day - 86_400_000);
    expect(timeAxisRange([])).toBeNull();
  });

  test("within allows only rounding in the last digits", () => {
    expect(within(1 + 1e-12, [0, 1])).toBe(true);
    expect(within(1.001, [0, 1])).toBe(false);
    expect(within(-1e-6, [0, 1])).toBe(false);
  });
});

describe("pie slices", () => {
  const values = [120.5, 98.25, 131, 87.5];
  const total = values.reduce((sum, value) => sum + value, 0);

  test("the angle of each d3.arc slice, pie and donut", () => {
    for (const inner of [0, 40]) {
      const draw = arc<{ startAngle: number; endAngle: number }>().innerRadius(inner).outerRadius(100);
      for (const slice of pie<number>().sort(null)(values)) {
        const measured = sliceAngle(draw(slice) ?? "");
        // d3.arc rounds coordinates to three decimals.
        expect(measured?.radius).toBeCloseTo(100, 3);
        expect(measured?.angle).toBeCloseTo((2 * Math.PI * slice.value) / total, 3);
      }
    }
  });

  test("a full circle and a slice over half of it", () => {
    const draw = arc<{ startAngle: number; endAngle: number }>().innerRadius(0).outerRadius(50);
    expect(sliceAngle(draw({ startAngle: 0, endAngle: 2 * Math.PI }) ?? "")?.angle).toBeCloseTo(2 * Math.PI, 3);
    expect(sliceAngle(draw({ startAngle: 0, endAngle: 4 }) ?? "")?.angle).toBeCloseTo(4, 3);
  });

  test("a path without an arc around its origin has no slice angle", () => {
    expect(sliceAngle("M0,0L10,0L10,10Z")).toBeNull();
    expect(sliceAngle("M100,100A10,10,0,0,1,110,110Z")).toBeNull();
  });

  test("paths that do not parse, and overlong ones, have no arcs", () => {
    expect(pathArcs("M0,0 Q")).toBeNull();
    expect(pathArcs("10,10")).toBeNull();
    expect(pathArcs("M0,0A10,10,0,2,1,10,10")).toBeNull();
    expect(pathArcs(`M0,0${"L1,1".repeat(3000)}`)).toBeNull();
    expect(pathArcs("M0,0L1,1Z")).toEqual([]);
  });

  test("relative commands and an elliptical arc", () => {
    const relative = pathArcs("m0,-100a100,100,0,0,1,100,100l-100,0z");
    expect(relative?.length).toBe(1);
    expect(relative?.[0]?.sweep).toBeCloseTo(Math.PI / 2, 6);
    expect(Math.hypot(relative?.[0]?.cx ?? 1, relative?.[0]?.cy ?? 1)).toBeCloseTo(0, 6);
    // An ellipse is not a slice of a circle.
    expect(pathArcs("M0,-100A100,50,0,0,1,100,0Z")).toEqual([]);
  });
});
