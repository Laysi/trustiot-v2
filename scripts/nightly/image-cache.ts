// Reuse of the nightly's Rust images by content (Laysi/trustiot-v2#60): a Rust image is built
// only when something that goes into it changed. The key is a digest of the git object hashes
// (trees and blobs) of every input of the build; nightly.yml restores `<dir>/<name>.tar` under
// that exact key with actions/cache, run.ts loads it instead of building, and on a miss run.ts
// builds the image and, in a scheduled or manual run on main, writes the tarball that nightly.yml
// then saves. Why a pull request cannot plant an image here: docs/architecture/ci-release.md,
// nightly.yml.
//
//   bun scripts/nightly/image-cache.ts key <image>     prints key=<cache key> (for $GITHUB_OUTPUT)
import { existsSync, mkdirSync, renameSync, rmSync } from "node:fs";
import { join, resolve } from "node:path";

import { IMAGE_PLATFORM } from "../release/lib.ts";
import { exec, run } from "./exec.ts";
import type { ExecOptions, ExecResult, RunLog } from "./exec.ts";

/**
 * Everything that decides what is in a Rust image the nightly builds, relative to the repository
 * root: the workspace and its lock file, the toolchain and the pinned build tools, every Rust
 * directory (a false miss costs one build, a false hit tests code that is not the commit's), the
 * image's own directory, the Containerfile and the ignore file a container build reads, every
 * script (the build runs service-image.ts, which imports scripts/release, scripts/lib and the
 * nightly's helpers, and those import more), and how CI sets up the build: nightly.yml and the
 * setup-proto action that installs the tools. The whole workflow file, since the steps that build
 * cannot be told apart from the rest by path; a change to it costs one build.
 */
export function imageCacheInputs(name: string): string[] {
  return [
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    ".prototools",
    ".proto/plugins",
    "crates",
    "services",
    "tools",
    "edge",
    "factory",
    `deploy/images/${name}`,
    "deploy/images/rust-binary.Containerfile",
    ".dockerignore",
    "scripts",
    ".github/workflows/nightly.yml",
    ".github/actions/setup-proto",
  ];
}

/** Bump to drop every cached image at once, for example when the tarball's format changes. */
export const IMAGE_CACHE_VERSION = "v1";

const NAME = /^[a-z0-9][a-z0-9-]*$/;

/** The git object hash of each path at HEAD, in order; throws when a path is not in HEAD. */
export type ObjectHashes = (paths: string[]) => Promise<string[]>;

type Exec = (cmd: string[], options?: ExecOptions) => Promise<ExecResult>;

/**
 * The object hashes at HEAD from git. Refuses a working tree that differs from HEAD in any input
 * (a key of HEAD over other sources would be a false hit): CI checks out a clean commit.
 */
export function gitObjectHashes(repoRoot: string, commands: { exec: Exec } = { exec }): ObjectHashes {
  return async (paths) => {
    const status = await commands.exec(["git", "status", "--porcelain", "--untracked-files=all", "--", ...paths], { cwd: repoRoot, timeoutMs: 60_000 });
    if (status.code !== 0) throw new Error(`git status failed: ${status.stderr.trim()}`);
    if (status.stdout.trim() !== "") {
      throw new Error(`the image's inputs differ from HEAD, so a key of HEAD would not describe them:\n${status.stdout.trimEnd()}`);
    }
    const parsed = await commands.exec(["git", "rev-parse", ...paths.map((path) => `HEAD:${path}`)], { cwd: repoRoot, timeoutMs: 60_000 });
    if (parsed.code !== 0) throw new Error(`an input of the image is not in HEAD (${paths.join(", ")}): ${parsed.stderr.trim()}`);
    const hashes = parsed.stdout.trim().split("\n");
    if (hashes.length !== paths.length || hashes.some((hash) => !/^[0-9a-f]{40,64}$/.test(hash))) {
      throw new Error(`git rev-parse printed ${JSON.stringify(parsed.stdout)} for ${paths.length} paths`);
    }
    return hashes;
  };
}

/** The cache key of image `name`: its platform and a SHA-256 over every input's path and object hash. */
export async function imageCacheKey(name: string, hashes: ObjectHashes): Promise<string> {
  if (!NAME.test(name)) throw new Error(`not an image name: ${JSON.stringify(name)}`);
  const paths = imageCacheInputs(name);
  const objects = await hashes(paths);
  const digest = new Bun.CryptoHasher("sha256");
  digest.update(`${IMAGE_CACHE_VERSION}\n${IMAGE_PLATFORM}\n`);
  paths.forEach((path, index) => digest.update(`${path}\0${objects[index]}\n`));
  return `nightly-image-${name}-${IMAGE_PLATFORM.replace("/", "-")}-${digest.digest("hex")}`;
}

/** The tarball of image `name` in the cache directory. */
export function imageCacheFile(dir: string, name: string): string {
  return join(dir, `${name}.tar`);
}

/** What became of the restored tarball. */
export type CachedImage = "loaded" | "missing" | "unusable";

/**
 * Loads image `name` from the restored tarball into docker. "missing" when there is no tarball (a
 * miss); "unusable" when docker cannot load it (a partial or corrupt tarball), it does not hold the
 * expected tag, or the image is not for linux/arm64: the caller builds the image instead, and the
 * run says so (a GitHub warning in CI), since the bad entry stays under its key until the inputs
 * change.
 */
export async function loadCachedImage(
  log: RunLog,
  dir: string,
  name: string,
  image: string,
  commands: { run: typeof run } = { run },
): Promise<CachedImage> {
  const file = imageCacheFile(dir, name);
  if (!existsSync(file)) {
    log.line(`no cached image at ${file}: building ${image}`);
    return "missing";
  }
  const unusable = (problem: string): CachedImage => {
    const message =
      `the cached ${image} cannot be used (${problem}); building it instead. Delete the Actions cache entry nightly-image-${name}-… ` +
      "(the repository's Actions caches page, or `gh cache delete <key>` with the key the workflow printed), or every run until the inputs change builds again";
    log.line(message);
    if (process.env.GITHUB_ACTIONS === "true") console.log(`::warning title=Nightly image cache::${message}`);
    return "unusable";
  };
  const reason = (error: unknown) => (error instanceof Error ? error.message : String(error)).split("\n")[0]!;
  try {
    await commands.run(log, ["docker", "load", "--input", file], { timeoutMs: 300_000 });
  } catch (error) {
    return unusable(`docker load failed: ${reason(error)}`);
  }
  let platform: string;
  try {
    const inspected = await commands.run(log, ["docker", "image", "inspect", "--format", "{{.Os}}/{{.Architecture}}", image], { timeoutMs: 60_000 });
    platform = inspected.stdout.trim();
  } catch (error) {
    return unusable(`it does not hold ${image}: ${reason(error)}`);
  }
  if (platform !== IMAGE_PLATFORM) return unusable(`it is for ${platform}, not ${IMAGE_PLATFORM}`);
  log.line(`loaded ${image} from the cache, no Rust build`);
  return "loaded";
}

/** Writes image `name` as the tarball nightly.yml saves; a partial file never takes the final name. */
export async function saveImageForCache(
  log: RunLog,
  dir: string,
  name: string,
  image: string,
  commands: { run: typeof run } = { run },
): Promise<void> {
  const file = imageCacheFile(dir, name);
  const partial = `${file}.partial`;
  // A miss restores nothing, so the directory does not exist yet on the run that saves first.
  mkdirSync(dir, { recursive: true });
  rmSync(partial, { force: true });
  await commands.run(log, ["docker", "save", "--output", partial, image], { timeoutMs: 300_000 });
  renameSync(partial, file);
  log.line(`wrote ${image} to ${file} for the cache`);
}

if (import.meta.main) {
  const [command, name] = process.argv.slice(2);
  if (command !== "key" || name === undefined) {
    console.error("usage: bun scripts/nightly/image-cache.ts key <image>");
    process.exit(2);
  }
  const repoRoot = resolve(import.meta.dir, "..", "..");
  try {
    console.log(`key=${await imageCacheKey(name, gitObjectHashes(repoRoot))}`);
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exit(1);
  }
}
