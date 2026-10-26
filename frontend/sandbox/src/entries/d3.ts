// d3.js: the d3 a page draws with, as `window.d3` (and the `d3` argument of its scripts). Only the
// modules a dashboard needs: no d3-fetch (the sandbox loads nothing), no d3-transition or d3-timer
// (a page draws once per data event, without animation).
import * as array from "d3-array";
import * as axis from "d3-axis";
import * as format from "d3-format";
import * as scale from "d3-scale";
import * as selection from "d3-selection";
import * as shape from "d3-shape";
import * as time from "d3-time";
import * as timeFormat from "d3-time-format";

Object.defineProperty(globalThis, "d3", {
  value: { ...array, ...axis, ...format, ...scale, ...selection, ...shape, ...time, ...timeFormat },
  writable: true,
  configurable: true,
});
