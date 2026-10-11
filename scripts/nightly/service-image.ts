// Service images for the nightly cluster (Laysi/trustiot-v2#308): builds the image of a Rust
// service the way release.yml does (deploy/images/<name>/image.toml, cross-compiled for
// aarch64-unknown-linux-musl with rust-build.ts's rustCrossCompileCommand and packed with
// deploy/images/rust-binary.Containerfile), tags it `localhost/trustiot-e2e/<name>:nightly` and
// imports it into every k3d node with importImageIntoCluster. Nothing is pushed.
//
// Nothing calls it yet: the nightly enables a real service (`services.identity`) once it has a
// database for it (Laysi/trustiot-v2#65, Laysi/trustiot-v2#77). The workflow then has to provide
// rustup (the pinned toolchain comes from rust-toolchain.toml) and the pinned zig, cargo-zigbuild
// and cargo-auditable (`tools:` of setup-proto). Cost: docs/architecture/ci-release.md, nightly.yml.
import { chmodSync, copyFileSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";

import { containerBuildArgs } from "../lib/build-context.ts";
import {
  type CargoPackage,
  IMAGE_PLATFORM,
  RUST_CONTAINERFILE,
  RUST_TARGET,
  aarch64ElfProblem,
  discoverImages,
  renderRustContainerfile,
  resolveBinary,
} from "../release/lib.ts";
import { rustCrossCompileCommand, staticElfProblem, toolchainChannel } from "../release/rust-build.ts";
import { exec, pinnedTool, retry, run } from "./exec.ts";
import { importImageIntoCluster } from "./platform.ts";
import type { ImportCommands, Platform } from "./platform.ts";

/** Where the nightly keeps service images; build-context.ts accepts `localhost/trustiot-e2e/<image>:<tag>`. */
export const SERVICE_IMAGE_PREFIX = "localhost/trustiot-e2e";
export const SERVICE_IMAGE_TAG = "nightly";

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
export async function findServiceImageTools(repoRoot: string): Promise<ServiceImageTools> {
  const onPath = (name: string): string => {
    const path = Bun.which(name);
    if (path === null) throw new Error(`${name} is not on PATH; a service image build needs rustup and its cargo`);
    return path;
  };
  const [zig, cargoZigbuild, cargoAuditable] = await Promise.all(
    ["zig", "cargo-zigbuild", "cargo-auditable"].map((tool) => pinnedTool(tool, repoRoot)),
  );
  return { rustup: onPath("rustup"), cargo: onPath("cargo"), zig: zig!, cargoZigbuild: cargoZigbuild!, cargoAuditable: cargoAuditable!, containerTool: "docker" };
}

/**
 * A bound for a cold release build of one service, well above what it took when measured
 * (docs/test-reports/2026-10-11-308-identity-image.md); a cargo cache makes it much shorter.
 */
const CARGO_TIMEOUT_MS = 40 * 60_000;
const IMAGE_BUILD_TIMEOUT_MS = 10 * 60_000;
const RETRY_PAUSE_MS = 10_000;

/**
 * Builds the image of the Rust service `name` (a directory under deploy/images/ with an
 * image.toml) for linux/arm64 and returns its local tag. The binary must be an aarch64 ELF
 * without a program interpreter, because the distroless static base has no loader or libc.
 */
export async function buildServiceImage(
  context: Pick<Platform, "repoRoot" | "log">,
  name: string,
  tools: ServiceImageTools,
  commands: Pick<ImportCommands, "run"> = { run },
  { pauseMs = RETRY_PAUSE_MS }: { pauseMs?: number } = {},
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
  const image = serviceImageRef(name);
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
  const problem = aarch64ElfProblem(bytes) ?? staticElfProblem(bytes);
  if (problem !== undefined) throw new Error(`${binary} cannot go into the image: ${problem}`);

  // The build context holds only the binary and the rendered Containerfile, as in release.yml.
  const dir = mkdtempSync(join(tmpdir(), `nightly-image-${name}-`));
  try {
    copyFileSync(binary, join(dir, bin));
    chmodSync(join(dir, bin), 0o755);
    const containerfile = join(dir, "Containerfile");
    writeFileSync(containerfile, renderRustContainerfile(readFileSync(join(repoRoot, RUST_CONTAINERFILE), "utf8"), bin));
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

/**
 * Builds the image of the Rust service `name` and imports it into every node of the nightly's
 * k3d cluster; returns the tag. The chart runs it with `services.<name>.image` set to
 * serviceImageValues(name). For Laysi/trustiot-v2#65 and Laysi/trustiot-v2#77.
 */
export async function loadServiceImage(
  platform: Pick<Platform, "repoRoot" | "log" | "k3d" | "cluster">,
  name: string,
  options: { tools?: ServiceImageTools; commands?: ImportCommands; pauseMs?: number } = {},
): Promise<string> {
  const commands = options.commands ?? { run, exec };
  const tools = options.tools ?? (await findServiceImageTools(platform.repoRoot));
  const pause = options.pauseMs === undefined ? {} : { pauseMs: options.pauseMs };
  const image = await buildServiceImage(platform, name, tools, commands, pause);
  await importImageIntoCluster(platform, image, commands, pause);
  return image;
}
