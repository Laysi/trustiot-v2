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
 * image's own directory, the Containerfile, and every script: the build runs service-image.ts,
 * which imports scripts/release, scripts/lib and the nightly's helpers, and those import more.
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
    `deploy/images/${name}`,
    "deploy/images/rust-binary.Containerfile",
    "scripts",
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

/**
 * Loads image `name` from the restored tarball into docker and returns true, or returns false when
 * there is no tarball (a miss). The loaded image must be the expected tag for linux/arm64.
 */
export async function loadCachedImage(
  log: RunLog,
  dir: string,
  name: string,
  image: string,
  commands: { run: typeof run } = { run },
): Promise<boolean> {
  const file = imageCacheFile(dir, name);
  if (!existsSync(file)) {
    log.line(`no cached image at ${file}: building ${image}`);
    return false;
  }
  // A bad entry stays under its key until the inputs change, so say how to get rid of it.
  const unusable = (problem: string) =>
    new Error(
      `the cached ${image} cannot be used: ${problem}. Delete the Actions cache entry nightly-image-${name}-… ` +
        "(the repository's Actions caches page, or `gh cache delete <key>` with the key the workflow printed) and run again",
    );
  try {
    await commands.run(log, ["docker", "load", "--input", file], { timeoutMs: 300_000 });
  } catch (error) {
    throw unusable(error instanceof Error ? error.message : String(error));
  }
  const inspected = await commands.run(log, ["docker", "image", "inspect", "--format", "{{.Os}}/{{.Architecture}}", image], { timeoutMs: 60_000 }).catch(
    (error: unknown) => {
      throw unusable(`it does not hold ${image} (${error instanceof Error ? error.message : String(error)})`);
    },
  );
  const platform = inspected.stdout.trim();
  if (platform !== IMAGE_PLATFORM) throw unusable(`it is for ${platform}, not ${IMAGE_PLATFORM}`);
  log.line(`loaded ${image} from the cache, no Rust build`);
  return true;
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
