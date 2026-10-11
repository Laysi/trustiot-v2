// Time limits shared by the chart tests (`bun test deploy/chart`), so that no test depends on
// bun's default of 5 s per test (Laysi/trustiot-v2#275).
//
// The tests start helm, kubeconform and Bun child processes. On an idle machine one helm
// process takes 0.1-0.4 s and the slowest test (four renders and kubeconform for one
// Kubernetes version) about 1.5 s. A workstation or CI runner that also compiles code runs
// them several times slower, so a fixed 5 s turned a slow machine into a failed test. The
// limits below are an order of magnitude above the idle durations: they stay far from a
// healthy run and still end a hung process in a minute with a message that names it.
import { setDefaultTimeout } from "bun:test";

/** The longest one helm, kubeconform or child process may take before its test fails. */
export const COMMAND_TIMEOUT_MS = 60_000;

/** The default limit of one test: one to a few commands. */
export const TEST_TIMEOUT_MS = 60_000;

/** The limit of a test that runs many commands, such as one render and kubeconform per case. */
export const SLOW_TEST_MS = 120_000;

/**
 * The client timeout of a test that lets a server answer with a response head and then stall.
 * The timeout is the thing under test, so the test waits all of it on every run. It only has
 * to be long enough that a head written at once has been read before it fires, because a
 * timeout before the head is a lost connection (`ConnectionError`) and after it is not.
 */
export const HEAD_STALL_TIMEOUT_MS = 2_000;

/**
 * Makes `TEST_TIMEOUT_MS` the default limit of every test in the calling file. Bun applies
 * `setDefaultTimeout` per file and reads no test timeout from bunfig.toml, so each test file
 * that starts processes calls this once at its top. A test passes its own limit (for example
 * `SLOW_TEST_MS`) as the third argument of `test`.
 */
export function useProcessTestTimeouts(): void {
  setDefaultTimeout(TEST_TIMEOUT_MS);
}

/**
 * Throws a message that names the command when `Bun.spawnSync` killed it at
 * `COMMAND_TIMEOUT_MS`. Without it a killed helm looks like a helm that failed without output.
 */
export function throwIfTimedOut(
  cmd: readonly string[],
  result: { exitedDueToTimeout?: boolean },
  hint = "a hung process, or a machine so busy that it never ran (check `uptime`)",
): void {
  if (!result.exitedDueToTimeout) return;
  throw new Error(`\`${cmd.join(" ")}\` did not finish within ${COMMAND_TIMEOUT_MS / 1000} s and was killed: ${hint}`);
}
