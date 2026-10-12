// Process and logging helpers for the nightly run: every command is logged with its output
// so the uploaded run log explains a failure without rerunning it.
import { appendFileSync } from "node:fs";

export interface ExecOptions {
  cwd?: string;
  env?: Record<string, string>;
  /** Kills the process (SIGKILL) after this many milliseconds. */
  timeoutMs?: number;
  /** Text written to the process's standard input; closed right after. */
  stdin?: string;
  /**
   * Keeps the standard output out of the log. For commands whose output can hold credentials:
   * a kubeconfig, or rendered manifests that contain Secrets. The run log is uploaded as an
   * artifact and parts of it are posted in issues and pull request comments.
   */
  quiet?: boolean;
}

export interface ExecResult {
  code: number | null;
  stdout: string;
  stderr: string;
  timedOut: boolean;
}

function spawn(cmd: string[], options: ExecOptions) {
  return Bun.spawn(cmd, {
    cwd: options.cwd,
    env: { ...process.env, ...options.env },
    stdin: options.stdin === undefined ? "ignore" : new Blob([options.stdin]),
    stdout: "pipe",
    stderr: "pipe",
  });
}

/** Runs a command to completion and captures its output; never throws for a non-zero exit. */
export async function exec(cmd: string[], options: ExecOptions = {}): Promise<ExecResult> {
  let proc: ReturnType<typeof spawn>;
  try {
    proc = spawn(cmd, options);
  } catch (error) {
    // The program does not exist or cannot start: report it like a shell would.
    return { code: 127, stdout: "", stderr: error instanceof Error ? error.message : String(error), timedOut: false };
  }
  let timedOut = false;
  const timer =
    options.timeoutMs === undefined
      ? undefined
      : setTimeout(() => {
          timedOut = true;
          proc.kill("SIGKILL");
        }, options.timeoutMs);
  const [stdout, stderr, code] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  if (timer !== undefined) clearTimeout(timer);
  return { code: timedOut ? null : code, stdout, stderr, timedOut };
}

/** Writes to the console and, when a path is given, appends to the run log. */
export class RunLog {
  constructor(private readonly path?: string) {}

  line(message: string): void {
    const stamped = `${new Date().toISOString()} ${message}`;
    console.log(stamped);
    if (this.path !== undefined) appendFileSync(this.path, `${stamped}\n`);
  }

  /** Output of a command, indented under the line that announced it. */
  block(text: string): void {
    const trimmed = text.trimEnd();
    if (trimmed === "") return;
    const indented = trimmed
      .split("\n")
      .map((line) => `    ${line}`)
      .join("\n");
    console.log(indented);
    if (this.path !== undefined) appendFileSync(this.path, `${indented}\n`);
  }
}

export class CommandError extends Error {
  constructor(
    readonly cmd: string[],
    readonly result: ExecResult,
  ) {
    const tail = (result.stderr.trim() || result.stdout.trim()).split("\n").slice(-12).join("\n");
    const how = result.timedOut ? "timed out" : `exited with ${result.code}`;
    super(`\`${cmd.join(" ")}\` ${how}${tail === "" ? "" : `:\n${tail}`}`);
    this.name = "CommandError";
  }
}

/** Runs a command, logs it and its output, and throws a CommandError on failure. */
export async function run(log: RunLog, cmd: string[], options: ExecOptions = {}): Promise<ExecResult> {
  log.line(`$ ${cmd.join(" ")}`);
  const result = await exec(cmd, options);
  if (options.quiet !== true) log.block(result.stdout);
  log.block(result.stderr);
  if (result.code !== 0) throw new CommandError(cmd, options.quiet === true ? { ...result, stdout: "" } : result);
  return result;
}

/** Retries `fn` with a growing pause; rethrows the last error. */
export async function retry<T>(
  log: RunLog,
  attempts: number,
  pauseMs: number,
  fn: () => Promise<T>,
): Promise<T> {
  let last: unknown;
  for (let attempt = 1; attempt <= attempts; attempt++) {
    try {
      return await fn();
    } catch (error) {
      last = error;
      const reason = error instanceof Error ? (error.message.split("\n")[0] ?? "") : String(error);
      log.line(`attempt ${attempt}/${attempts} failed: ${reason}`);
      if (attempt < attempts) await Bun.sleep(pauseMs * attempt);
    }
  }
  throw last;
}

/** The clock waitUntilGone reads and sleeps on; tests pass a fake one. */
export interface Clock {
  now(): number;
  sleep(ms: number): Promise<void>;
}
const SYSTEM_CLOCK: Clock = { now: () => Date.now(), sleep: (ms) => Bun.sleep(ms) };

/**
 * Waits until `list` returns nothing: for objects that Kubernetes removes in the background, such
 * as the dependents its garbage collector deletes after their owner is gone. Returns how long it
 * took; after `timeoutMs` it fails with what it waited for and what was still there. An error
 * from `list` (a failed kubectl) ends the wait at once.
 */
export async function waitUntilGone(
  what: string,
  list: () => Promise<string[]>,
  { timeoutMs, intervalMs = 3_000, clock = SYSTEM_CLOCK }: { timeoutMs: number; intervalMs?: number; clock?: Clock },
): Promise<number> {
  const started = clock.now();
  for (;;) {
    const left = await list();
    const waited = clock.now() - started;
    if (left.length === 0) return waited;
    if (waited >= timeoutMs) {
      throw new Error(`timed out after ${Math.round(waited / 1000)} s waiting for ${what}; still there: ${left.join(", ")}`);
    }
    await clock.sleep(Math.min(intervalMs, timeoutMs - waited));
  }
}

/** Path of a tool at the version pinned in .prototools (see deploy/chart/README.md). */
export async function pinnedTool(tool: string, cwd: string): Promise<string> {
  const result = await exec(["proto", "bin", tool, "--reporter", "text"], { cwd, timeoutMs: 60_000 });
  const path = result.stdout.trim().split("\n").at(-1)?.trim() ?? "";
  if (result.code !== 0 || !path.startsWith("/")) {
    throw new Error(`cannot find the pinned ${tool}; run \`proto install\`\n${result.stderr.trim()}`);
  }
  return path;
}
