// A thin wrapper around the podman CLI. Arguments are always passed as an array (no shell), and
// secrets travel through stdin or `--env-file`, never through the argument list.

export interface PodmanResult {
  code: number;
  stdout: string;
  stderr: string;
}

export interface PodmanOptions {
  /** Text written to the process's standard input. */
  stdin?: string;
  /** Working directory. */
  cwd?: string;
  /** Kill the process after this many milliseconds. */
  timeoutMs?: number;
}

export async function podman(args: string[], options: PodmanOptions = {}): Promise<PodmanResult> {
  const child = Bun.spawn(["podman", ...args], {
    cwd: options.cwd,
    stdin: options.stdin === undefined ? "ignore" : new Blob([options.stdin]),
    stdout: "pipe",
    stderr: "pipe",
    timeout: options.timeoutMs,
  });
  const [stdout, stderr, code] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  return { code, stdout, stderr };
}

/** Like {@link podman} but throws with podman's own message when the command fails. */
export async function podmanOk(args: string[], options: PodmanOptions = {}): Promise<string> {
  const result = await podman(args, options);
  if (result.code !== 0) {
    const verb = args.filter((argument) => !argument.startsWith("-")).slice(0, 2).join(" ");
    throw new Error(`podman ${verb} failed (exit ${result.code}): ${result.stderr.trim()}`);
  }
  return result.stdout;
}

/** Fails early with a readable message when podman is missing or cannot run. */
export async function requirePodman(): Promise<void> {
  let result: PodmanResult;
  try {
    result = await podman(["info", "--format", "json"], { timeoutMs: 30_000 });
  } catch {
    throw new Error("podman is not installed or not on PATH; this environment needs podman");
  }
  if (result.code !== 0) {
    throw new Error(`podman is installed but does not work: ${result.stderr.trim()}`);
  }
}

/** Names of the containers carrying a label, running or not. */
export async function containersWithLabel(label: string): Promise<string[]> {
  const out = await podmanOk(["ps", "-a", "--filter", `label=${label}`, "--format", "{{.Names}}"]);
  return out.split("\n").map((line) => line.trim()).filter(Boolean);
}

export async function volumesWithLabel(label: string): Promise<string[]> {
  const out = await podmanOk(["volume", "ls", "--filter", `label=${label}`, "--format", "{{.Name}}"]);
  return out.split("\n").map((line) => line.trim()).filter(Boolean);
}

export async function networksWithLabel(label: string): Promise<string[]> {
  const out = await podmanOk(["network", "ls", "--filter", `label=${label}`, "--format", "{{.Name}}"]);
  return out.split("\n").map((line) => line.trim()).filter(Boolean);
}

export async function imagesWithLabel(label: string): Promise<string[]> {
  const out = await podmanOk(["images", "--filter", `label=${label}`, "--format", "{{.ID}}"]);
  return [...new Set(out.split("\n").map((line) => line.trim()).filter(Boolean))];
}
