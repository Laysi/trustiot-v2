// Service images for the nightly cluster (Laysi/trustiot-v2#308): builds the image of a Rust
// service the way release.yml does (deploy/images/<name>/image.toml, cross-compiled for
// aarch64-unknown-linux-musl with rust-build.ts's rustCrossCompileCommand and packed with
// rust-context.ts's build context: deploy/images/rust-binary.Containerfile, or the image's own
// Containerfile, and the checked build output of its [static] project, built with
// static-build.ts), tags it `localhost/trustiot-e2e/<name>:nightly` and imports it into every k3d
// node with importImageIntoCluster. Nothing is pushed. The dashboard check's image test
// (deploy/images/dashboard-check/image.test.ts) builds its image with buildServiceImage too.
//
// run.ts builds the migration Job's image (deploy/images/migrate, Laysi/trustiot-v2#60) with it;
// the nightly enables a real service (`services.identity`) the same way once it has a database
// for it (Laysi/trustiot-v2#65, Laysi/trustiot-v2#77). An image whose inputs have not changed
// since main last built it comes from the cache instead (image-cache.ts). For a build the
// workflow provides rustup (the pinned toolchain comes from rust-toolchain.toml) and the pinned
// zig, cargo-zigbuild and cargo-auditable (`tools:` of setup-proto). Cost:
// docs/architecture/ci-release.md, nightly.yml.
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";

import { containerBuildArgs } from "../lib/build-context.ts";
import { type CargoPackage, IMAGE_PLATFORM, RUST_TARGET, aarch64ElfProblem, discoverImages, resolveBinary } from "../release/lib.ts";
import { rustCrossCompileCommand, staticElfProblem, toolchainChannel } from "../release/rust-build.ts";
import { writeRustImageContext } from "../release/rust-context.ts";
import { buildStaticSite } from "../release/static-build.ts";
import { exec, pinnedTool, retry, run } from "./exec.ts";
import { loadCachedImage, programCacheFile, reportUnusableCache, saveImageForCache } from "./image-cache.ts";
import { importImageIntoCluster } from "./platform.ts";
import type { ImportCommands, Platform } from "./platform.ts";

/** Where the nightly keeps service images; build-context.ts accepts `localhost/trustiot-e2e/<image>:<tag>`. */
export const SERVICE_IMAGE_PREFIX = "localhost/trustiot-e2e";
export const SERVICE_IMAGE_TAG = "nightly";
/** The migration Job's image (deploy/images/migrate), which tests/values/smoke.yaml names. */
export const MIGRATE_IMAGE_NAME = "migrate";

/** The local tag of a service image, for example `localhost/trustiot-e2e/identity:nightly`. */
export function serviceImageRef(name: string): string {
  return `${SERVICE_IMAGE_PREFIX}/${name}:${SERVICE_IMAGE_TAG}`;
}

/**
 * The chart values that make `services.<name>` run the imported image. IfNotPresent: nothing
 * can pull a `localhost` image, so the pod starts only from the copy on the node.
 */
export function serviceImageValues(name: string): { repository: string; tag: string; digest: string; pullPolicy: string } {
  return { repository: `${SERVICE_IMAGE_PREFIX}/${name}`, tag: SERVICE_IMAGE_TAG, digest: "", pullPolicy: "IfNotPresent" };
}

/** The programs a service image build runs. */
export interface ServiceImageTools {
  /** rustup and its cargo; proto installs Rust through rustup and does not shim cargo. */
  rustup: string;
  cargo: string;
  /** Pinned in .prototools. */
  zig: string;
  cargoZigbuild: string;
  cargoAuditable: string;
  /** The container engine whose images k3d imports: docker on the nightly runner. */
  containerTool: string;
}

/** Finds the tools: rustup and cargo on PATH, the rest at the versions pinned in .prototools. */
/** The pinned tools a build needs besides rustup (`tools:` of nightly.yml's setup-proto step). */
export const BUILD_TOOLS = ["zig", "cargo-zigbuild", "cargo-auditable"];

/**
 * Installs the pinned build tools, one `proto install <tool>` each (proto installs a single tool
 * per call when it is given a name; the version comes from .prototools).
 */
export async function installBuildTools(context: Pick<Platform, "repoRoot" | "log">, commands: Pick<ImportCommands, "run"> = { run }): Promise<void> {
  for (const tool of BUILD_TOOLS) {
    await commands.run(context.log, ["proto", "install", tool], { cwd: context.repoRoot, timeoutMs: 600_000 });
  }
}

export async function findServiceImageTools(repoRoot: string): Promise<ServiceImageTools> {
  const onPath = (name: string): string => {
    const path = Bun.which(name);
    if (path === null) throw new Error(`${name} is not on PATH; a service image build needs rustup and its cargo`);
    return path;
  };
  const [zig, cargoZigbuild, cargoAuditable] = await Promise.all(
    BUILD_TOOLS.map((tool) => pinnedTool(tool, repoRoot)),
  );
  return { rustup: onPath("rustup"), cargo: onPath("cargo"), zig: zig!, cargoZigbuild: cargoZigbuild!, cargoAuditable: cargoAuditable!, containerTool: "docker" };
}

/**
 * A bound for a cold release build of one service, well above what it took when measured
 * (docs/test-reports/2026-10-11-308-identity-image.md); a cargo cache makes it much shorter.
 */
const CARGO_TIMEOUT_MS = 40 * 60_000;
/**
 * A bound for one image build. The dashboard check's image downloads Chromium and its libraries
 * from snapshot.debian.org, which is slow at times (deploy/images/dashboard-check/README.md).
 */
const IMAGE_BUILD_TIMEOUT_MS = 20 * 60_000;
/** A bound for one command of a static site build (static-build.ts): installing or building. */
const SITE_BUILD_TIMEOUT_MS = 15 * 60_000;
const RETRY_PAUSE_MS = 10_000;

/**
 * Programs of an image's package that the run needs on the runner (image-cache.ts,
 * NIGHTLY_PROGRAMS): `names` are binaries of the package, written to `dir` as `dir/<name>`.
 */
export interface ImagePrograms {
  names: readonly string[];
  dir: string;
}

/** Why `bytes` cannot run on the arm64 runner as it is: not an aarch64 ELF, or not static. */
export function programProblem(bytes: Uint8Array): string | undefined {
  return aarch64ElfProblem(bytes) ?? staticElfProblem(bytes);
}

/** Copies the program at `from` to `to` (a new file, mode 0755), through a partial file. */
function installProgram(from: string, to: string): void {
  const partial = `${to}.partial`;
  rmSync(partial, { force: true });
  copyFileSync(from, partial);
  chmodSync(partial, 0o755);
  renameSync(partial, to);
}

/** Options of buildServiceImage. */
export interface BuildServiceImageOptions {
  pauseMs?: number;
  programs?: ImagePrograms;
  /** The local tag; serviceImageRef(name) by default. */
  tag?: string;
  /**
   * A checked build output to pack instead of building the image's [static] project (only for
   * an image that has one): the dashboard check's image test takes one from the environment.
   */
  site?: string;
  /** The Bun that builds the [static] project: this process's own, the pinned one, by default. */
  bun?: string;
}

/**
 * Builds the image of the Rust service `name` (a directory under deploy/images/ with an
 * image.toml) for linux/arm64 and returns its local tag. The binary must be an aarch64 ELF
 * without a program interpreter, because the distroless static base has no loader or libc.
 * An image with a [static] project gets that project built and checked as release.yml builds it
 * (static-build.ts), from the repository root. `programs`: other binaries of the same package,
 * which the build compiles anyway (`--bins`), checked the same way and copied to `programs.dir`;
 * they never go into the image.
 */
export async function buildServiceImage(
  context: Pick<Platform, "repoRoot" | "log">,
  name: string,
  tools: ServiceImageTools,
  commands: Pick<ImportCommands, "run"> = { run },
  { pauseMs = RETRY_PAUSE_MS, programs, tag, site, bun = process.execPath }: BuildServiceImageOptions = {},
): Promise<string> {
  const { repoRoot, log } = context;
  const images = discoverImages(repoRoot);
  const spec = images.find((image) => image.name === name);
  if (spec === undefined) {
    throw new Error(`deploy/images has no image named ${name} (images: ${images.map((image) => image.name).join(", ") || "none"})`);
  }
  if (spec.kind !== "rust") {
    throw new Error(`deploy/images/${name} is not a Rust service image (it has a Containerfile, not an image.toml)`);
  }
  if (site !== undefined && spec.staticSite === undefined) {
    throw new Error(`deploy/images/${name} has no [static] project, so it takes no site`);
  }
  const image = tag ?? serviceImageRef(name);
  log.line(`== service image ${image}: package ${spec.package} for ${RUST_TARGET}`);

  const toolchain = toolchainChannel(readFileSync(join(repoRoot, "rust-toolchain.toml"), "utf8"));
  await commands.run(log, [tools.rustup, "toolchain", "install", toolchain, "--profile", "minimal", "--no-self-update"], { timeoutMs: 600_000 });
  await commands.run(log, [tools.rustup, "target", "add", "--toolchain", toolchain, RUST_TARGET], { timeoutMs: 300_000 });

  const metadataOut = await commands.run(log, [tools.cargo, "metadata", "--no-deps", "--format-version", "1", "--locked"], {
    cwd: repoRoot,
    timeoutMs: 120_000,
    quiet: true,
  });
  let metadata: { packages: CargoPackage[]; target_directory: string };
  try {
    metadata = JSON.parse(metadataOut.stdout) as typeof metadata;
  } catch {
    throw new Error(`cargo metadata did not print JSON: ${metadataOut.stdout.slice(0, 200)}`);
  }
  const bin = resolveBinary(spec, metadata.packages);

  const { cmd, env } = rustCrossCompileCommand({
    cargo: tools.cargo,
    zig: tools.zig,
    cargoZigbuild: tools.cargoZigbuild,
    cargoAuditable: tools.cargoAuditable,
    packages: [spec.package],
    root: repoRoot,
    cargoHome: process.env.CARGO_HOME || join(homedir(), ".cargo"),
    path: process.env.PATH ?? "",
  });
  await commands.run(log, cmd, { cwd: repoRoot, env, timeoutMs: CARGO_TIMEOUT_MS });

  const binary = join(metadata.target_directory, RUST_TARGET, "release", bin);
  if (!existsSync(binary)) throw new Error(`cargo did not produce ${binary}`);
  const bytes = new Uint8Array(readFileSync(binary));
  const problem = programProblem(bytes);
  if (problem !== undefined) throw new Error(`${binary} cannot go into the image: ${problem}`);

  if (programs !== undefined) {
    const pkg = metadata.packages.find((candidate) => candidate.name === spec.package);
    const bins = pkg?.targets.filter((target) => target.kind.includes("bin")).map((target) => target.name) ?? [];
    for (const program of programs.names) {
      if (program === bin || !bins.includes(program)) {
        throw new Error(`package ${spec.package} has no other binary named ${program} (binaries: ${bins.join(", ") || "none"})`);
      }
      const built = join(metadata.target_directory, RUST_TARGET, "release", program);
      if (!existsSync(built)) throw new Error(`cargo did not produce ${built}`);
      const programBytes = new Uint8Array(readFileSync(built));
      const unusable = programProblem(programBytes);
      if (unusable !== undefined) throw new Error(`${built} cannot run on the arm64 runner: ${unusable}`);
      mkdirSync(programs.dir, { recursive: true });
      installProgram(built, join(programs.dir, program));
      log.line(`built ${program} with ${spec.package} (${programBytes.byteLength} bytes) for this run; it does not go into the image`);
    }
  }

  let checkedSite = site;
  if (spec.staticSite !== undefined && checkedSite === undefined) {
    log.line(`== static site ${spec.staticSite} for ${image}`);
    checkedSite = await buildStaticSite(repoRoot, spec.staticSite, bun, (cmd) =>
      commands.run(log, cmd, { cwd: repoRoot, timeoutMs: SITE_BUILD_TIMEOUT_MS }),
    );
  }

  // The build context is release.yml's (rust-context.ts): the binary, the static site when the
  // image has one, and the Containerfile.
  const dir = mkdtempSync(join(tmpdir(), `nightly-image-${name}-`));
  try {
    const containerfile = writeRustImageContext({
      root: repoRoot,
      bin,
      binary,
      dir,
      // A nightly image is never published, so its files' times only need to be fixed.
      epoch: 0,
      ...(spec.containerfile === undefined ? {} : { containerfile: spec.containerfile }),
      ...(checkedSite === undefined ? {} : { site: checkedSite }),
    });
    // Pulling the pinned base image can fail on a network hiccup.
    await retry(log, 2, pauseMs, () =>
      commands.run(
        log,
        // containerBuildArgs checks the context and the Containerfile first (docs/architecture/secrets.md).
        [tools.containerTool, ...containerBuildArgs(dir, ["--platform", IMAGE_PLATFORM, "--file", containerfile, "--tag", image])],
        { timeoutMs: IMAGE_BUILD_TIMEOUT_MS },
      ),
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  log.line(`built ${image} from ${bin} (${bytes.byteLength} bytes)`);
  return image;
}

/** Where nightly.yml restores and saves the image tarballs, and whether this run writes one. */
export interface ImageCache {
  dir: string;
  /** Write the tarball after a build: scheduled and manual runs on main only (nightly.yml). */
  save: boolean;
}

/**
 * Imports the image of the Rust service `name` into every node of the nightly's k3d cluster;
 * returns the tag. With a cache, the image comes from the tarball nightly.yml restored for the
 * exact key of its inputs (image-cache.ts), and no Rust tool is looked up or run; without one, or
 * on a miss, it is built, and written for the cache when `save` is set. A tarball that cannot be
 * used is reported and the image built instead, after installing the build tools nightly.yml
 * skipped on the hit. The chart runs it with
 * `services.<name>.image` set to serviceImageValues(name). For Laysi/trustiot-v2#65 and
 * Laysi/trustiot-v2#77.
 * `programs` (Laysi/trustiot-v2#517): the package's other binaries the run executes on the runner
 * end up in `programs.dir`, from the same cache entry (a missing or broken one counts like an
 * unusable tarball) or from the build, which then also writes them for the cache when `save` is set.
 */
export async function loadServiceImage(
  platform: Pick<Platform, "repoRoot" | "log" | "k3d" | "cluster">,
  name: string,
  options: { tools?: ServiceImageTools; commands?: ImportCommands; pauseMs?: number; cache?: ImageCache; programs?: ImagePrograms } = {},
): Promise<string> {
  const commands = options.commands ?? { run, exec };
  const pause = options.pauseMs === undefined ? {} : { pauseMs: options.pauseMs };
  const image = serviceImageRef(name);
  const cache = options.cache;
  const programs = options.programs;
  let cached = cache === undefined ? "missing" : await loadCachedImage(platform.log, cache.dir, name, image, commands);
  if (cached === "loaded" && cache !== undefined && programs !== undefined) {
    // The programs come from the same entry; one that is missing or broken means a build, as
    // for an unusable tarball.
    const problem = cachedProgramsProblem(cache.dir, programs.names);
    if (problem === undefined) {
      for (const program of programs.names) {
        mkdirSync(programs.dir, { recursive: true });
        installProgram(programCacheFile(cache.dir, program), join(programs.dir, program));
      }
      platform.log.line(`took ${programs.names.join(", ")} from the cache, no Rust build`);
    } else {
      reportUnusableCache(platform.log, image, problem);
      cached = "unusable";
    }
  }
  if (cached !== "loaded") {
    if (cached === "unusable" && options.tools === undefined) {
      // nightly.yml installs the build tools only on a miss; a hit whose tarball turned out
      // unusable installs them here (the pinned versions, through proto).
      await installBuildTools(platform, commands);
    }
    const tools = options.tools ?? (await findServiceImageTools(platform.repoRoot));
    await buildServiceImage(platform, name, tools, commands, { ...pause, ...(programs === undefined ? {} : { programs }) });
    if (cache?.save === true) {
      await saveImageForCache(platform.log, cache.dir, name, image, commands);
      for (const program of programs?.names ?? []) {
        installProgram(join(programs!.dir, program), programCacheFile(cache.dir, program));
        platform.log.line(`wrote ${program} to ${programCacheFile(cache.dir, program)} for the cache`);
      }
    }
  }
  await importImageIntoCluster(platform, image, commands, pause);
  return image;
}

/** Why the cached programs cannot be used: one is missing, or cannot run on the arm64 runner. */
export function cachedProgramsProblem(dir: string, names: readonly string[]): string | undefined {
  for (const program of names) {
    const file = programCacheFile(dir, program);
    if (!existsSync(file)) return `the entry has no ${program}`;
    const problem = programProblem(new Uint8Array(readFileSync(file)));
    if (problem !== undefined) return `its ${program} cannot run on the arm64 runner: ${problem}`;
  }
  return undefined;
}
