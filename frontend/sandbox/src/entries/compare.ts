// compare.js: the comparison, for the checking service to inject into an isolated world of the
// sandbox frame. It defines `tqCompare(input)` in that world's global.
import { createCompare } from "../checker.ts";

Object.defineProperty(globalThis, "tqCompare", { value: createCompare(window), configurable: true });
