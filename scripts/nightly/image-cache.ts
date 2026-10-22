// Reuse of the nightly's Rust images by content (Laysi/trustiot-v2#60): a Rust image is built
// only when something that goes into it changed. The key is a digest of the git object hashes
// (trees and blobs) of every input of the build; nightly.yml restores the directory with
// `<name>.tar` of every image in NIGHTLY_IMAGES under that exact key with actions/cache, run.ts
// loads them instead of building, and on a miss run.ts builds the images and, in a scheduled or
// manual run on main, writes the tarballs that nightly.yml then saves together. Why a pull
// request cannot plant an image here: docs/architecture/ci-release.md, nightly.yml.
//
//   bun scripts/nightly/image-cache.ts key             prints key=<cache key of NIGHTLY_IMAGES> (for $GITHUB_OUTPUT)
//   bun scripts/nightly/image-cache.ts key <image>     prints key=<cache key of one image>
//   bun scripts/nightly/image-cache.ts complete <dir>  prints built=true when <dir> holds every tarball of NIGHTLY_IMAGES
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

/**
 * The Rust images the nightly builds from the commit and deploys: the migration Job's
 * (Laysi/trustiot-v2#60), and the identity and assets services the smoke test calls through the
 * ingress (Laysi/trustiot-v2#65), and the AI gateway, which the smoke only starts: no provider key
 * reaches CI (Laysi/trustiot-v2#536). They share one cache entry, since every Rust change reaches
 * all of them anyway (each key covers the whole workspace).
 */
export const NIGHTLY_IMAGES = ["migrate", "identity", "assets", "ai-gateway"] as const;

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

/**
 * The key of the one cache entry that holds the tarballs of `names` (NIGHTLY_IMAGES): a SHA-256
 * over each image's own key, so it changes exactly when one of them does.
 */
export async function imagesCacheKey(names: readonly string[], hashes: ObjectHashes): Promise<string> {
  if (names.length === 0) throw new Error("no image to key");
  const keys = await Promise.all(names.map((name) => imageCacheKey(name, hashes)));
  const digest = new Bun.CryptoHasher("sha256");
  digest.update(`${IMAGE_CACHE_VERSION}\n${keys.join("\n")}\n`);
  return `nightly-images-${IMAGE_PLATFORM.replace("/", "-")}-${digest.digest("hex")}`;
}

/** The tarball of image `name` in the cache directory. */
export function imageCacheFile(dir: string, name: string): string {
  return join(dir, `${name}.tar`);
}

/**
 * Whether `dir` holds the tarball of every image in `names`: nightly.yml saves the entry only
 * then, so a restored entry never lacks an image (a hit installs no build tools).
 */
export function cacheComplete(dir: string, names: readonly string[]): boolean {
  return names.every((name) => existsSync(imageCacheFile(dir, name)));
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
      `the cached ${image} cannot be used (${problem}); building it instead. Delete the Actions cache entry nightly-images-… that holds it ` +
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
  const [command, argument, ...rest] = process.argv.slice(2);
  const usage = "usage: bun scripts/nightly/image-cache.ts key [<image>] | complete <dir>";
  if (rest.length > 0 || !(command === "key" || (command === "complete" && argument !== undefined))) {
    console.error(usage);
    process.exit(2);
  }
  if (command === "complete") {
    if (cacheComplete(argument!, NIGHTLY_IMAGES)) console.log("built=true");
    process.exit(0);
  }
  const repoRoot = resolve(import.meta.dir, "..", "..");
  try {
    const hashes = gitObjectHashes(repoRoot);
    const key = argument === undefined ? await imagesCacheKey(NIGHTLY_IMAGES, hashes) : await imageCacheKey(argument, hashes);
    console.log(`key=${key}`);
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exit(1);
  }
}
