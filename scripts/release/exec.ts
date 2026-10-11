/**
 * Process and GitHub Actions helpers for the release stage scripts. Tools always run without a
 * shell, with arguments as a list; secrets go through stdin, never through arguments.
 */

import { appendFileSync, existsSync } from "node:fs";
import { resolve } from "node:path";
import { ReleaseError, errorMessage, sha256Digest } from "./lib.ts";

export interface RunOptions {
  cwd?: string;
  /** Added to this process's environment; `undefined` removes a variable. */
  env?: Record<string, string | undefined>;
  /** Written to the command's stdin. Use it for secrets. */
  stdin?: string;
  /** Stream the command's output to the log instead of capturing it (for long builds). */
  stream?: boolean;
}

export interface RunResult {
  stdout: string;
  stderr: string;
  exitCode: number;
}

/** Runs a command and returns its output, whatever its exit status. */
export async function tryRun(cmd: readonly string[], options: RunOptions = {}): Promise<RunResult> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries({ ...process.env, ...options.env })) {
    if (value !== undefined) {
      env[key] = value;
    }
  }
  const proc = Bun.spawn([...cmd], {
    cwd: options.cwd,
    env,
    stdin: options.stdin === undefined ? "ignore" : new Blob([options.stdin]),
    stdout: options.stream ? "inherit" : "pipe",
    stderr: options.stream ? "inherit" : "pipe",
  });
  const [stdout, stderr, exitCode] = await Promise.all([
    options.stream ? "" : new Response(proc.stdout as ReadableStream).text(),
    options.stream ? "" : new Response(proc.stderr as ReadableStream).text(),
    proc.exited,
  ]);
  return { stdout, stderr, exitCode };
}

/** Runs a command and throws a ReleaseError naming it when it exits with a non-zero status. */
export async function run(cmd: readonly string[], options: RunOptions = {}): Promise<RunResult> {
  const result = await tryRun(cmd, options);
  if (result.exitCode !== 0) {
    const tail = lastLines(result.stderr || result.stdout, 15);
    const where = options.stream ? "; see its output above" : tail === "" ? "" : `:\n${tail}`;
    throw new ReleaseError(`${commandName(cmd)} exited with status ${result.exitCode}${where}`);
  }
  return result;
}

/** The program and subcommand, for messages; never the full argument list. */
export function commandName(cmd: readonly string[]): string {
  const program = (cmd[0] ?? "").split("/").at(-1) ?? "";
  const sub = cmd[1] !== undefined && /^[a-z][a-z-]*$/.test(cmd[1]) ? ` ${cmd[1]}` : "";
  return `\`${program}${sub}\``;
}

function lastLines(text: string, count: number): string {
  return text.trimEnd().split("\n").slice(-count).join("\n");
}

/** The repository root, where proto finds .prototools whatever the working directory is. */
const ROOT = resolve(import.meta.dir, "..", "..");
const protoTools = new Map<string, string>();

/**
 * Returns the executable of a tool pinned in .prototools. Calling it directly instead of the
 * proto shim keeps proto's own messages out of the tool's output (deploy/chart/README.md).
 */
export async function protoTool(name: string): Promise<string> {
  const cached = protoTools.get(name);
  if (cached !== undefined) {
    return cached;
  }
  const result = await tryRun([systemTool("proto"), "bin", name, "--reporter", "text"], { cwd: ROOT });
  const path = result.stdout.trim().split("\n").at(-1)?.trim() ?? "";
  if (result.exitCode !== 0 || path === "" || !existsSync(path)) {
    throw new ReleaseError(`proto has no ${name} installed; run \`proto install\``);
  }
  protoTools.set(name, path);
  return path;
}

/** Returns a tool from PATH that the machine or the runner image provides. */
export function systemTool(name: string): string {
  const path = Bun.which(name);
  if (path === null) {
    throw new ReleaseError(`${name} is not installed`);
  }
  return path;
}

/**
 * Throws unless the repository checkout is the planned commit, so that a job never builds or
 * packages anything else (for example after the tag was moved while the run was queued).
 */
export async function requireCheckout(commit: string): Promise<void> {
  const head = (await run([systemTool("git"), "-C", ROOT, "rev-parse", "HEAD"])).stdout.trim();
  if (head !== commit) {
    throw new ReleaseError(`the checkout is ${head}, but the release plan is for ${commit}`);
  }
}

/**
 * Reads a manifest with skopeo (`oci-archive:<file>` or `docker://<ref>`) and returns it with
 * its digest, the SHA-256 of the exact bytes (skopeo prints the raw manifest unchanged).
 */
export async function rawManifest(
  skopeo: string,
  transportRef: string,
  extraArgs: readonly string[] = [],
): Promise<{ manifest: unknown; digest: string }> {
  const raw = (await run([skopeo, "inspect", "--raw", ...extraArgs, transportRef])).stdout;
  return { manifest: JSON.parse(raw), digest: sha256Digest(raw) };
}

export function log(stage: string, message: string): void {
  console.log(`[release:${stage}] ${message}`);
}

/** Sets a step output in GitHub Actions; a no-op elsewhere. Multi-line values are safe. */
export function setOutput(name: string, value: string): void {
  const file = process.env.GITHUB_OUTPUT;
  if (file === undefined || file === "") {
    return;
  }
  const delimiter = `EOF_${crypto.randomUUID().replaceAll("-", "")}`;
  appendFileSync(file, `${name}<<${delimiter}\n${value}\n${delimiter}\n`);
}

function escapeAnnotation(text: string): string {
  return text.replaceAll("%", "%25").replaceAll("\r", "%0D").replaceAll("\n", "%0A");
}

/**
 * Runs a stage. A failure is printed, annotated in GitHub Actions and written to the step
 * output `error` (one line), which the workflow's report job puts in the commit status.
 */
export async function runStage(stage: string, body: () => Promise<void>): Promise<void> {
  try {
    await body();
  } catch (error) {
    const message = errorMessage(error);
    console.error(`[release:${stage}] ${message}`);
    if (process.env.GITHUB_ACTIONS === "true") {
      console.log(`::error title=release ${stage}::${escapeAnnotation(message)}`);
      setOutput("error", message.split("\n")[0]?.slice(0, 200) ?? "");
    }
    process.exit(1);
  }
}

/** Reads `--name value` options; every option is required unless it has a default. */
export function parseOptions(
  args: readonly string[],
  spec: Record<string, { default?: string; flag?: boolean }>,
): Record<string, string> {
  const values: Record<string, string> = {};
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index] ?? "";
    const name = arg.startsWith("--") ? arg.slice(2) : undefined;
    const option = name === undefined ? undefined : spec[name];
    if (name === undefined || option === undefined) {
      throw new ReleaseError(`unknown argument ${arg}; options: --${Object.keys(spec).join(", --")}`);
    }
    if (option.flag) {
      values[name] = "true";
      continue;
    }
    const value = args[index + 1];
    if (value === undefined || value.startsWith("--")) {
      throw new ReleaseError(`--${name} needs a value`);
    }
    values[name] = value;
    index += 1;
  }
  for (const [name, option] of Object.entries(spec)) {
    if (values[name] === undefined) {
      if (option.flag) {
        values[name] = "false";
      } else if (option.default !== undefined) {
        values[name] = option.default;
      } else {
        throw new ReleaseError(`--${name} is required`);
      }
    }
  }
  return values;
}
