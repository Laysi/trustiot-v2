import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { CommandError, exec, retry, run, RunLog, waitUntilGone } from "./exec.ts";

describe("exec and run", () => {
  let dir: string;
  let logPath: string;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "nightly-exec-"));
    logPath = join(dir, "run.log");
  });
  afterEach(() => rmSync(dir, { recursive: true, force: true }));

  test("exec captures output and reports a missing program like a shell", async () => {
    const ok = await exec(["sh", "-c", "echo out; echo err >&2; exit 3"]);
    expect(ok).toMatchObject({ code: 3, stdout: "out\n", stderr: "err\n", timedOut: false });
    expect((await exec(["nightly-no-such-program"])).code).toBe(127);
  });

  test("exec kills a process that exceeds its timeout", async () => {
    const result = await exec(["sleep", "30"], { timeoutMs: 100 });
    expect(result).toMatchObject({ code: null, timedOut: true });
  });

  test("exec passes standard input", async () => {
    expect((await exec(["cat"], { stdin: "hello" })).stdout).toBe("hello");
  });

  test("run logs the command and its output", async () => {
    await run(new RunLog(logPath), ["sh", "-c", "echo visible"]);
    const log = readFileSync(logPath, "utf8");
    expect(log).toContain("$ sh -c echo visible");
    expect(log).toContain("    visible");
  });

  test("a quiet run keeps standard output out of the log and out of the error", async () => {
    const log = new RunLog(logPath);
    // The marker is in the environment, not on the command line, which a log always shows.
    const env = { MARKER: "leak-canary" };
    await run(log, ["sh", "-c", 'echo "$MARKER"'], { quiet: true, env });
    const failure = run(log, ["sh", "-c", 'echo "$MARKER"; echo failed >&2; exit 1'], { quiet: true, env });
    await expect(failure).rejects.toBeInstanceOf(CommandError);
    await expect(failure).rejects.toThrow("failed");
    const text = readFileSync(logPath, "utf8");
    expect(text).not.toContain("leak-canary");
    expect(text).toContain("failed");
    await failure.catch((error: Error) => expect(error.message).not.toContain("leak-canary"));
  });

  test("retry returns the first success and rethrows the last failure", async () => {
    const log = new RunLog(logPath);
    let calls = 0;
    expect(
      await retry(log, 3, 1, async () => {
        calls += 1;
        if (calls < 3) throw new Error("not yet");
        return "done";
      }),
    ).toBe("done");
    await expect(retry(log, 2, 1, async () => Promise.reject(new Error("always")))).rejects.toThrow("always");
  });
});

describe("waitUntilGone", () => {
  /** A clock that only moves when the code under test sleeps, recording each pause. */
  function fakeClock() {
    let time = 0;
    const pauses: number[] = [];
    return {
      pauses,
      clock: {
        now: () => time,
        sleep: async (ms: number) => {
          pauses.push(ms);
          time += ms;
        },
      },
    };
  }
  const VOLUME = "persistentvolumeclaim/trustiot-postgres-1";

  test("returns at once when nothing is left", async () => {
    const { clock, pauses } = fakeClock();
    expect(await waitUntilGone("volumes", async () => [], { timeoutMs: 10_000, clock })).toBe(0);
    expect(pauses).toEqual([]);
  });

  test("polls until the objects are gone and reports how long it took", async () => {
    // The garbage collector removes the volume a few polls after its owner was deleted.
    const answers = [[VOLUME], [VOLUME], []];
    const { clock, pauses } = fakeClock();
    let calls = 0;
    const waited = await waitUntilGone("volumes", async () => answers[calls++] ?? [], { timeoutMs: 10_000, intervalMs: 1_000, clock });
    expect(calls).toBe(3);
    expect(pauses).toEqual([1_000, 1_000]);
    expect(waited).toBe(2_000);
  });

  test("fails after the bound, naming what it waited for and what is still there", async () => {
    const { clock, pauses } = fakeClock();
    let calls = 0;
    const left = [VOLUME, "persistentvolumeclaim/other"];
    await expect(
      waitUntilGone(
        "the deleted Cluster's volumes",
        async () => {
          calls++;
          return left;
        },
        { timeoutMs: 10_000, intervalMs: 3_000, clock },
      ),
    ).rejects.toThrow(`timed out after 10 s waiting for the deleted Cluster's volumes; still there: ${VOLUME}, persistentvolumeclaim/other`);
    // The last pause is cut to the bound, and the objects are listed once more at the bound.
    expect(pauses).toEqual([3_000, 3_000, 3_000, 1_000]);
    expect(calls).toBe(5);
  });

  test("objects that are gone at the check on the bound pass", async () => {
    const { clock } = fakeClock();
    let calls = 0;
    const list = async () => (++calls < 3 ? [VOLUME] : []);
    expect(await waitUntilGone("volumes", list, { timeoutMs: 2_000, intervalMs: 1_000, clock })).toBe(2_000);
  });

  test("a failing list ends the wait with its own error", async () => {
    const { clock, pauses } = fakeClock();
    await expect(
      waitUntilGone("volumes", async () => Promise.reject(new Error("kubectl get pvc failed")), { timeoutMs: 10_000, clock }),
    ).rejects.toThrow("kubectl get pvc failed");
    expect(pauses).toEqual([]);
  });
});
