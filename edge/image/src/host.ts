// The host side of build.ts and check.ts: argument checks, the verified download of the source
// image, and the tool container the real work runs in (src/inside/). The host needs only Bun and
// rootless podman; every other tool comes from the pinned container image.
import { createHash } from "node:crypto";
import { createReadStream, realpathSync } from "node:fs";
import { mkdir, mkdtemp, open, readdir, rename, rm, stat } from "node:fs/promises";
import { homedir } from "node:os";
import { basename, dirname, join, resolve, sep } from "node:path";
import { InputError, aarch64ElfProblem, parsePublicKeys } from "./inputs.ts";
import { SOURCE, TOOL_IMAGE } from "./pins.ts";

/** The repository checkout this file belongs to. */
export const REPO = realpathSync(resolve(import.meta.dir, "../../.."));
const IMAGE_DIR = join(REPO, "edge/image");
const AGENT_SYSTEMD_DIR = join(REPO, "edge/agent/systemd");

export class UsageError extends Error {}

/** Parses `--name value` pairs; every name must be one of `allowed`, each at most once. */
export function parseArgs(argv: string[], allowed: readonly string[], required: readonly string[]): Map<string, string> {
  const values = new Map<string, string>();
  for (let i = 0; i < argv.length; i += 2) {
    const name = argv[i] ?? "";
    const value = argv[i + 1];
    if (!name.startsWith("--") || !allowed.includes(name.slice(2))) throw new UsageError(`unknown argument ${name}`);
    if (value === undefined || value.startsWith("--")) throw new UsageError(`${name} needs a value`);
    if (values.has(name.slice(2))) throw new UsageError(`${name} is given twice`);
    values.set(name.slice(2), value);
  }
  for (const name of required) if (!values.has(name)) throw new UsageError(`--${name} is required`);
  return values;
}

/** True when `path` (resolved through symbolic links as far as it exists) is inside `dir`. */
export function isInside(path: string, dir: string): boolean {
  let probe = resolve(path);
  const rest: string[] = [];
  for (;;) {
    try {
      probe = realpathSync(probe);
      break;
    } catch {
      rest.unshift(basename(probe));
      const parent = dirname(probe);
      if (parent === probe) break;
      probe = parent;
    }
  }
  const full = join(probe, ...rest);
  return full === dir || full.startsWith(dir + sep);
}

export function defaultCache(): string {
  return join(process.env.XDG_CACHE_HOME || join(homedir(), ".cache"), "trustiot-v2-image");
}

/** A path outside the repository: images and keys must never land where git can pick them up. */
export function outsideRepo(path: string, what: string): string {
  const full = resolve(path);
  if (isInside(full, REPO)) throw new UsageError(`${what} must be outside the repository (${full})`);
  return full;
}

export async function readPublicKeys(path: string): Promise<string[]> {
  const full = outsideRepo(path, "--ssh-key");
  const info = await stat(full).catch(() => null);
  if (!info?.isFile()) throw new UsageError(`--ssh-key ${full} is not a file`);
  if (info.size > 64 * 1024) throw new UsageError(`--ssh-key ${full} is larger than 64 KiB`);
  try {
    return parsePublicKeys(await Bun.file(full).text());
  } catch (error) {
    if (error instanceof InputError) throw new UsageError(`--ssh-key ${full}: ${error.message}`);
    throw error;
  }
}

export async function checkAgent(path: string): Promise<string> {
  const full = resolve(path);
  const info = await stat(full).catch(() => null);
  if (!info?.isFile()) throw new UsageError(`--agent ${full} is not a file`);
  const problem = aarch64ElfProblem(new Uint8Array(await Bun.file(full).slice(0, 64).arrayBuffer()));
  if (problem) throw new UsageError(`--agent ${full}: ${problem}`);
  return full;
}

/** The build and check run the image's arm64 programs, so the host must run arm64 code. */
export function requireArm64Host(): void {
  if (process.platform !== "linux" || process.arch !== "arm64") {
    throw new UsageError(
      `this host is ${process.platform}/${process.arch}; the build runs the image's arm64 programs (package scripts, sshd -G, the agent), so it needs an arm64 Linux host (README.md, section 「建置主機」)`,
    );
  }
  if (!Bun.which("podman")) throw new UsageError("podman is not installed (README.md, section 「建置主機」)");
}

/**
 * Why podman must not run here, or undefined: the container gets SYS_ADMIN and /dev/fuse and runs
 * the image's own programs, which is only contained when podman is rootless (README.md, section
 * 「容器」). Run as root, podman is rootful and those programs would run as the host's root.
 */
export function rootlessProblem(uid: number, podmanRootless: string): string | undefined {
  if (uid === 0) return "refusing to run as root: run the build as an ordinary user with rootless podman";
  if (podmanRootless.trim() !== "true") return `podman is not rootless here (podman info says Rootless=${JSON.stringify(podmanRootless.trim())})`;
  return undefined;
}

/** Throws unless this process is not root and podman runs rootless. */
export async function requireRootlessPodman(): Promise<void> {
  const uid = process.getuid?.() ?? 0;
  const early = rootlessProblem(uid, "true");
  if (early) throw new UsageError(early);
  const info = Bun.spawn(["podman", "info", "--format", "{{.Host.Security.Rootless}}"], { stdout: "pipe", stderr: "pipe" });
  const [out, code] = [await new Response(info.stdout).text(), await info.exited];
  if (code !== 0) throw new UsageError(`podman info failed (exit ${code})`);
  const problem = rootlessProblem(uid, out);
  if (problem) throw new UsageError(problem);
}

async function sha256Of(path: string): Promise<string> {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(path, { highWaterMark: 4 * 1024 * 1024 })) hash.update(chunk as Buffer);
  return hash.digest("hex");
}

/**
 * The source image in the cache, downloaded once and verified against the pinned SHA-256 every
 * time it is used. A file that does not match is deleted, never used.
 */
export async function sourceImage(cache: string): Promise<string> {
  const dir = join(cache, "src");
  await mkdir(dir, { recursive: true, mode: 0o700 });
  const target = join(dir, SOURCE.file);
  if (await Bun.file(target).exists()) {
    if ((await sha256Of(target)) === SOURCE.sha256) return target;
    console.error(`${target} does not match the pinned SHA-256; downloading it again`);
    await rm(target);
  }
  const partial = `${target}.part`;
  console.log(`downloading ${SOURCE.url}`);
  const response = await fetch(SOURCE.url, { redirect: "follow" });
  if (!response.ok || !response.body) throw new Error(`download failed: HTTP ${response.status}`);
  if (!response.url.startsWith("https://")) throw new Error(`download was redirected away from HTTPS: ${response.url}`);
  const file = await open(partial, "w", 0o600);
  const hash = createHash("sha256");
  let bytes = 0;
  try {
    for await (const chunk of response.body as unknown as AsyncIterable<Uint8Array>) {
      bytes += chunk.length;
      if (bytes > SOURCE.xzBytes) throw new Error(`download is larger than the pinned ${SOURCE.xzBytes} bytes`);
      hash.update(chunk);
      await file.write(chunk);
    }
  } finally {
    await file.close();
  }
  const sha256 = hash.digest("hex");
  if (sha256 !== SOURCE.sha256) {
    await rm(partial, { force: true });
    throw new Error(`downloaded image has SHA-256 ${sha256}, expected ${SOURCE.sha256}`);
  }
  await rename(partial, target);
  return target;
}

/** A fresh directory for one run's intermediate files; the caller removes it. */
export async function workDir(cache: string): Promise<string> {
  await mkdir(join(cache, "work"), { recursive: true, mode: 0o700 });
  return mkdtemp(join(cache, "work", "run-"));
}

export interface Mount {
  host: string;
  container: string;
  readOnly: boolean;
}

/** podman arguments for the tool container (README.md, section 「容器」 explains each option). */
export function containerArgs(mounts: Mount[], script: string, env: Record<string, string> = {}): string[] {
  const args = [
    "podman", "run", "--rm", "--init", "--pull=never",
    // Mounts inside the container's own user namespace: fuse2fs, chroot bind mounts, tmpfs.
    "--cap-add=SYS_ADMIN", "--device=/dev/fuse",
    "--security-opt=no-new-privileges",
    // SELinux hosts would otherwise refuse the container access to the mounted host paths.
    "--security-opt=label=disable",
    "--env=BUN_RUNTIME_TRANSPILER_CACHE_PATH=0",
  ];
  for (const [name, value] of Object.entries(env)) args.push(`--env=${name}=${value}`);
  for (const m of mounts) {
    if (m.host.includes(":") || m.container.includes(":")) throw new UsageError(`a path with ":" cannot be mounted: ${m.host}`);
    args.push(`--volume=${m.host}:${m.container}:${m.readOnly ? "ro" : "rw"}`);
  }
  args.push(TOOL_IMAGE, "/opt/bun/bun", script);
  return args;
}

/** Mounts every run needs: Bun itself, this directory and the agent's systemd files. */
export function baseMounts(work: string): Mount[] {
  return [
    { host: process.execPath, container: "/opt/bun/bun", readOnly: true },
    { host: IMAGE_DIR, container: "/src/image", readOnly: true },
    { host: AGENT_SYSTEMD_DIR, container: "/src/agent-systemd", readOnly: true },
    { host: work, container: "/work", readOnly: false },
  ];
}

export async function runContainer(mounts: Mount[], script: string, env: Record<string, string> = {}): Promise<number> {
  const pull = Bun.spawn(["podman", "pull", "--quiet", TOOL_IMAGE], { stdout: "ignore", stderr: "inherit" });
  if ((await pull.exited) !== 0) throw new Error(`podman pull ${TOOL_IMAGE} failed`);
  const child = Bun.spawn(containerArgs(mounts, script, env), { stdin: "ignore", stdout: "inherit", stderr: "inherit" });
  return child.exited;
}

/** The directory the image is written to: new, or empty. */
export async function outputDir(path: string): Promise<string> {
  const full = outsideRepo(path, "--out");
  await mkdir(full, { recursive: true, mode: 0o700 });
  if ((await readdir(full)).length > 0) throw new UsageError(`--out ${full} is not empty`);
  return realpathSync(full);
}

/** Resolves a pinned tool's executable through proto (a proto shim cannot be mounted). */
export async function protoBin(tool: string): Promise<string> {
  const env = { ...process.env };
  delete env.AI_AGENT;
  delete env.CLAUDECODE;
  const child = Bun.spawn(["proto", "bin", tool], { env, cwd: REPO, stdout: "pipe", stderr: "pipe" });
  const [out, code] = await Promise.all([new Response(child.stdout).text(), child.exited]);
  const path = out.trim().split("\n").at(-1)?.trim() ?? "";
  if (code !== 0 || path === "" || !(await Bun.file(path).exists())) {
    throw new UsageError(`proto bin ${tool} did not give a path; run proto install ${tool} or pass --${tool} <path>`);
  }
  return realpathSync(path);
}

export function fail(error: unknown): never {
  if (error instanceof UsageError) {
    console.error(`error: ${error.message}`);
    process.exit(2);
  }
  console.error(error instanceof Error ? `error: ${error.message}` : error);
  process.exit(1);
}
