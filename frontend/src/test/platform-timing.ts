import { setDefaultTimeout } from "bun:test";
import { configure } from "@testing-library/react";

/** What a platform page test may wait for a condition, on a loaded workstation or CI runner. */
const WAIT_MS = 60_000;

/**
 * Raises the per-test timeout and the wait-for timeout of this test file. Bun applies the default
 * timeout per file, so every platform page test file calls this at the top.
 */
export function allowGenerousTimeouts(): void {
  setDefaultTimeout(WAIT_MS * 2);
  configure({ asyncUtilTimeout: WAIT_MS });
}
