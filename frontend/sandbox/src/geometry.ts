// The arithmetic behind the geometry rules (frontend/sandbox/README.md, 「檢查規則」): how a
// mark's size follows its value, whether positions are a linear function of values, a pie
// slice's angle from its path, and the extent an axis's ticks may cover.
import { nice } from "d3-array";
import { scaleUtc } from "d3-scale";

/** Sizes may be off by this share of the expected size, or by one pixel. */
export const RELATIVE_TOLERANCE = 0.01;
export const PIXEL_TOLERANCE = 1;

/** The nice() tick counts whose domains an axis may use: d3's default is 10. */
const NICE_COUNTS = 20;

function median(values: number[]): number {
  const sorted = Float64Array.from(values).sort();
  const middle = sorted.length >> 1;
  if (sorted.length === 0) return Number.NaN;
  return sorted.length % 2 === 1
    ? (sorted[middle] ?? Number.NaN)
    : ((sorted[middle - 1] ?? 0) + (sorted[middle] ?? 0)) / 2;
}

export interface Sized {
  /** The value: its magnitude is what the size shows. */
  value: number;
  /** The measured size (pixels, or radians for an angle). */
  size: number;
  /** The size one unit of tolerance stands for (one pixel; for an angle, a pixel of arc). */
  unit: number;
}

/**
 * Which marks are not proportional to their values: the scale is the median of size / |value|
 * over the marks with a value, and a mark may be off by 1% of its expected size or one unit. A
 * chart whose largest expected mark is under one unit (a scale collapsed to nothing) fails every
 * mark with a value.
 */
export function disproportionate(marks: readonly Sized[]): boolean[] {
  const ratios: number[] = [];
  let largest = 0;
  for (const mark of marks) {
    const magnitude = Math.abs(mark.value);
    if (magnitude > 0) ratios.push(mark.size / magnitude);
    largest = Math.max(largest, magnitude);
  }
  const scale = ratios.length === 0 ? 0 : median(ratios);
  const unit = marks.reduce((smallest, mark) => Math.min(smallest, mark.unit), Number.POSITIVE_INFINITY);
  const collapsed = largest > 0 && scale * largest < unit;
  return marks.map((mark) => {
    const expected = scale * Math.abs(mark.value);
    if (collapsed && mark.value !== 0) return true;
    return Math.abs(mark.size - expected) > Math.max(mark.unit, RELATIVE_TOLERANCE * expected);
  });
}

export interface Placed {
  value: number;
  position: number;
}

/**
 * A robust straight line through the points (Theil–Sen: the median slope over pairs, then the
 * median intercept), so one misplaced mark does not drag the line toward itself.
 */
export function robustLine(points: readonly Placed[]): { slope: number; intercept: number } {
  const sorted = [...points].sort((a, b) => a.value - b.value);
  const slopes: number[] = [];
  const n = sorted.length;
  const pair = (a: Placed | undefined, b: Placed | undefined) => {
    if (a !== undefined && b !== undefined && b.value !== a.value) {
      slopes.push((b.position - a.position) / (b.value - a.value));
    }
  };
  if (n <= 60) {
    for (let i = 0; i < n; i++) for (let j = i + 1; j < n; j++) pair(sorted[i], sorted[j]);
  } else {
    const half = n >> 1;
    for (let i = 0; i + half < n; i++) pair(sorted[i], sorted[i + half]);
  }
  const slope = slopes.length === 0 ? 0 : median(slopes);
  const intercept = median(sorted.map((point) => point.position - slope * point.value));
  return { slope, intercept };
}

/**
 * Which points are off the line through the rest: at least three are needed to judge (fewer
 * all pass). A point may be off by 1% of the positions' spread or one pixel.
 */
export function offLine(points: readonly Placed[]): boolean[] {
  if (points.length < 3) return points.map(() => false);
  const { slope, intercept } = robustLine(points);
  let low = Number.POSITIVE_INFINITY;
  let high = Number.NEGATIVE_INFINITY;
  for (const point of points) {
    low = Math.min(low, point.position);
    high = Math.max(high, point.position);
  }
  const tolerance = Math.max(PIXEL_TOLERANCE, RELATIVE_TOLERANCE * (high - low));
  return points.map((point) => Math.abs(point.position - (slope * point.value + intercept)) > tolerance);
}

/**
 * The numbers an axis over these values may show: the extent of the values (`[v, v + 1]` when
 * they are all `v`, `[0, 1]` when there are none), widened to include 0, then by d3's nice() for
 * every tick count from 1 to 20, so a page may nice its scale with any count it likes.
 */
export function numberAxisRange(values: readonly number[]): [number, number] {
  let low = Number.POSITIVE_INFINITY;
  let high = Number.NEGATIVE_INFINITY;
  for (const value of values) {
    low = Math.min(low, value);
    high = Math.max(high, value);
  }
  if (low > high) {
    low = 0;
    high = 1;
  } else if (low === high) {
    high = low + 1;
  }
  low = Math.min(low, 0);
  high = Math.max(high, 0);
  let from = low;
  let to = high;
  for (let count = 1; count <= NICE_COUNTS; count++) {
    const [start, stop] = nice(low, high, count);
    from = Math.min(from, start);
    to = Math.max(to, stop);
  }
  return [from, to];
}

const DAY_MS = 86_400_000;

/**
 * The times (milliseconds) a time axis over these values may show: their extent (a single time
 * widened by a day each way), widened by `scaleUtc().nice()` for every tick count from 1 to 20;
 * `null` when there are no values.
 */
export function timeAxisRange(values: readonly number[]): [number, number] | null {
  if (values.length === 0) return null;
  let low = Number.POSITIVE_INFINITY;
  let high = Number.NEGATIVE_INFINITY;
  for (const value of values) {
    low = Math.min(low, value);
    high = Math.max(high, value);
  }
  if (low === high) {
    low -= DAY_MS;
    high += DAY_MS;
  }
  let from = low;
  let to = high;
  for (let count = 1; count <= NICE_COUNTS; count++) {
    const domain = scaleUtc()
      .domain([new Date(low), new Date(high)])
      .nice(count)
      .domain();
    from = Math.min(from, domain[0]?.getTime() ?? from);
    to = Math.max(to, domain[1]?.getTime() ?? to);
  }
  return [from, to];
}

/** Whether `value` lies in `range`, allowing for rounding in the last digits. */
export function within(value: number, range: readonly [number, number]): boolean {
  const slack = 1e-9 * Math.max(1, Math.abs(range[1] - range[0]));
  return value >= range[0] - slack && value <= range[1] + slack;
}

// ---------------------------------------------------------------------------------------------
// Pie slices: the angle of a d3.arc path

interface Arc {
  cx: number;
  cy: number;
  radius: number;
  sweep: number;
}

const COMMAND = /[MmZzLlHhVvCcSsQqTtAa]/;
const NUMBER = /[+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?/y;
/** Longest path a slice may have: d3.arc writes a few hundred characters. */
const MAX_PATH_LENGTH = 10_000;
const PARAMETERS: Record<string, number> = { m: 2, l: 2, h: 1, v: 1, c: 6, s: 4, q: 4, t: 2, a: 7, z: 0 };

/** The circular arcs of an SVG path, with their centres; `null` when the path does not parse. */
export function pathArcs(d: string): Arc[] | null {
  if (d.length > MAX_PATH_LENGTH) return null;
  const arcs: Arc[] = [];
  let index = 0;
  let x = 0;
  let y = 0;
  let startX = 0;
  let startY = 0;
  let command = "";
  const skip = () => {
    while (index < d.length && /[\s,]/.test(d.charAt(index))) index++;
  };
  const number = (): number | null => {
    skip();
    NUMBER.lastIndex = index;
    const match = NUMBER.exec(d);
    if (match === null) return null;
    index += match[0].length;
    return Number(match[0]);
  };
  const flag = (): number | null => {
    skip();
    const c = d.charAt(index);
    if (c !== "0" && c !== "1") return null;
    index++;
    return c === "1" ? 1 : 0;
  };
  for (;;) {
    skip();
    if (index >= d.length) return arcs;
    const c = d.charAt(index);
    if (COMMAND.test(c)) {
      command = c;
      index++;
    } else if (command === "" || command === "z" || command === "Z") {
      return null;
    }
    const lower = command.toLowerCase();
    const relative = command !== command.toUpperCase();
    const count = PARAMETERS[lower] ?? -1;
    if (count === 0) {
      x = startX;
      y = startY;
      continue;
    }
    const values: number[] = [];
    for (let i = 0; i < count; i++) {
      const value = lower === "a" && (i === 3 || i === 4) ? flag() : number();
      if (value === null) return null;
      values.push(value);
    }
    const at = (i: number) => values[i] ?? 0;
    const endX = (i: number) => (relative ? x + at(i) : at(i));
    const endY = (i: number) => (relative ? y + at(i) : at(i));
    switch (lower) {
      case "m":
        x = endX(0);
        y = endY(1);
        startX = x;
        startY = y;
        // Pairs after a moveto are linetos.
        command = relative ? "l" : "L";
        break;
      case "h":
        x = relative ? x + at(0) : at(0);
        break;
      case "v":
        y = relative ? y + at(0) : at(0);
        break;
      case "a": {
        const toX = endX(5);
        const toY = endY(6);
        const arc = centreArc(x, y, at(0), at(1), at(2), at(3), at(4), toX, toY);
        if (arc !== null) arcs.push(arc);
        x = toX;
        y = toY;
        break;
      }
      default:
        x = endX(count - 2);
        y = endY(count - 1);
    }
  }
}

/** An SVG arc's centre and sweep (SVG 1.1, F.6.5); `null` for a straight or empty one. */
function centreArc(
  x1: number,
  y1: number,
  rxIn: number,
  ryIn: number,
  rotation: number,
  large: number,
  sweepFlag: number,
  x2: number,
  y2: number,
): Arc | null {
  if ((x1 === x2 && y1 === y2) || rxIn === 0 || ryIn === 0) return null;
  const phi = (rotation * Math.PI) / 180;
  const cos = Math.cos(phi);
  const sin = Math.sin(phi);
  const dx = (x1 - x2) / 2;
  const dy = (y1 - y2) / 2;
  const x1p = cos * dx + sin * dy;
  const y1p = -sin * dx + cos * dy;
  let rx = Math.abs(rxIn);
  let ry = Math.abs(ryIn);
  const lambda = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
  if (lambda > 1) {
    rx *= Math.sqrt(lambda);
    ry *= Math.sqrt(lambda);
  }
  const numerator = rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p;
  const denominator = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
  const coefficient = (large === sweepFlag ? -1 : 1) * Math.sqrt(Math.max(0, numerator / denominator));
  const cxp = (coefficient * rx * y1p) / ry;
  const cyp = (-coefficient * ry * x1p) / rx;
  const cx = cos * cxp - sin * cyp + (x1 + x2) / 2;
  const cy = sin * cxp + cos * cyp + (y1 + y2) / 2;
  const angle = (ux: number, uy: number, vx: number, vy: number) => Math.atan2(ux * vy - uy * vx, ux * vx + uy * vy);
  let sweep = angle((x1p - cxp) / rx, (y1p - cyp) / ry, (-x1p - cxp) / rx, (-y1p - cyp) / ry);
  if (sweepFlag === 0 && sweep > 0) sweep -= 2 * Math.PI;
  if (sweepFlag === 1 && sweep < 0) sweep += 2 * Math.PI;
  if (Math.abs(rx - ry) > 0.5) return null;
  return { cx, cy, radius: (rx + ry) / 2, sweep: Math.abs(sweep) };
}

/**
 * A pie or donut slice's angle (radians) as d3.arc draws it: the summed sweep of its outer arcs,
 * the circular arcs centred on the path's origin with the largest radius. `null` when the path
 * has none. Padding (`padAngle`) and rounded corners (`cornerRadius`) shorten the outer arc, so
 * slices drawn with them do not measure their value.
 */
export function sliceAngle(d: string): { angle: number; radius: number } | null {
  const arcs = pathArcs(d);
  if (arcs === null) return null;
  const centred = arcs.filter((arc) => Math.hypot(arc.cx, arc.cy) <= 0.5);
  if (centred.length === 0) return null;
  const radius = Math.max(...centred.map((arc) => arc.radius));
  const angle = centred.filter((arc) => radius - arc.radius <= 0.5).reduce((total, arc) => total + arc.sweep, 0);
  return { angle, radius };
}
