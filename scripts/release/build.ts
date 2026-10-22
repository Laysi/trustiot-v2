/**
 * Release stage 2, the `build` job of .github/workflows/release.yml: builds every planned image
 * for linux/arm64, with an SPDX SBOM each (packing: pack.ts). Nothing is pushed.
 *
 *   bun scripts/release/build.ts --plan <plan.json> --out <dir>
 *
 * Rust images: the binaries are cross-compiled for aarch64-unknown-linux-musl in one Cargo
 * run, with Zig as C compiler and linker (cargo-zigbuild) and the dependency list embedded
 * (cargo-auditable), then each is packed with deploy/images/rust-binary.Containerfile. The
 * Cargo command is rust-build.ts's rustCrossCompileCommand, which the nightly's service images share.
 * A Rust image whose image.toml has a `[static]` table also gets that Bun project's checked build
 * output (static-build.ts): the SPA in the web image.
 * release.yml runs this on the arm64 runner and the local flow on any machine, with the same
 * command: the host's own triple is never the musl target. Containerfile images are built from
 * their directory. Builds never run code of the image's platform, so x86 machines need no
 * emulator: a RUN may only execute in a `FROM --platform=$BUILDPLATFORM` stage, which plan
 * checks (containerfile.ts's targetStageRunProblems).
 *
 * A Rust image's digest does not depend on whether podman runs as root or rootless
 * (Laysi/trustiot-v2#384; pack.ts says how).
 *
 * This job runs third-party build code (Cargo build scripts and proc macros, the static sites'
 * build tools), so it has no secrets and a read-only token, and its output is only data:
 *   <out>/images/<name>.oci.tar     the image as an OCI archive, pushed byte for byte by publish
 *   <out>/images/<name>.spdx.json   its SBOM from syft
 *   <out>/build.json                digest and size of each image, for the log, and the podman
 *                                   and buildah versions that packed them, for the release page
 */

import { existsSync, mkdirSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { log, parseOptions, protoTool, requireCheckout, run, runStage, systemTool } from "./exec.ts";
import {
  type CargoPackage,
  RUST_TARGET,
  type ReleasePlan,
  ReleaseError,
  aarch64ElfProblem,
  parsePlan,
  resolveBinary,
  rustSpec,
} from "./lib.ts";
import { packImages } from "./pack.ts";
import { rustCrossCompileCommand, staticElfProblem, toolchainChannel } from "./rust-build.ts";
import { buildStaticSites } from "./static-build.ts";

const ROOT = resolve(import.meta.dir, "..", "..");

/** The toolchain that rust-toolchain.toml pins; the same version as `rust` in .prototools. */
function pinnedToolchain(): string {
  return toolchainChannel(readFileSync(join(ROOT, "rust-toolchain.toml"), "utf8"));
}

/** Where Cargo keeps the registry sources that dependency paths start with. */
function cargoHome(): string {
  return process.env.CARGO_HOME || join(homedir(), ".cargo");
}

interface CargoMetadata {
  packages: CargoPackage[];
  target_directory: string;
}

/** Cross-compiles the binaries of every Rust image in one Cargo run; returns image → binary path. */
async function buildRustBinaries(plan: ReleasePlan): Promise<Map<string, string>> {
  const images = plan.images.filter((image) => image.kind === "rust");
  const binaries = new Map<string, string>();
  if (images.length === 0) {
    return binaries;
  }
  const rustup = systemTool("rustup");
  const cargo = systemTool("cargo");
  const toolchain = pinnedToolchain();
  // rustup is how proto installs Rust too; installing here keeps the job independent of it.
  await run([rustup, "toolchain", "install", toolchain, "--profile", "minimal", "--no-self-update"], { stream: true });
  await run([rustup, "target", "add", "--toolchain", toolchain, RUST_TARGET], { stream: true });

  const metadata = JSON.parse(
    (await run([cargo, "metadata", "--no-deps", "--format-version", "1", "--locked"], { cwd: ROOT })).stdout,
  ) as CargoMetadata;
  const bins = new Map<string, string>();
  for (const image of images) {
    bins.set(image.name, resolveBinary(rustSpec(image), metadata.packages));
  }

  const packages = [...new Set(images.map((image) => image.package ?? ""))];
  const { cmd, env } = rustCrossCompileCommand({
    cargo,
    zig: await protoTool("zig"),
    cargoZigbuild: await protoTool("cargo-zigbuild"),
    cargoAuditable: await protoTool("cargo-auditable"),
    packages,
    root: ROOT,
    cargoHome: cargoHome(),
    path: process.env.PATH ?? "",
    version: plan.version,
    sourceDateEpoch: plan.sourceDateEpoch,
  });
  log("build", `cross-compiling ${packages.join(", ")} for ${RUST_TARGET}`);
  await run(cmd, { cwd: ROOT, stream: true, env });
  for (const image of images) {
    const bin = bins.get(image.name) ?? "";
    const path = join(metadata.target_directory, RUST_TARGET, "release", bin);
    if (!existsSync(path)) {
      throw new ReleaseError(`cargo did not produce ${path}`);
    }
    const bytes = new Uint8Array(readFileSync(path));
    const problem = aarch64ElfProblem(bytes);
    if (problem !== undefined) {
      throw new ReleaseError(`${path} is not an aarch64 binary: ${problem}`);
    }
    // distroless/static has no dynamic loader or libc.
    const linking = staticElfProblem(bytes);
    if (linking !== undefined) {
      throw new ReleaseError(`${path} is not a static binary: ${linking}`);
    }
    binaries.set(image.name, path);
  }
  return binaries;
}

await runStage("build", async () => {
  const options = parseOptions(Bun.argv.slice(2), { plan: {}, out: {} });
  const plan = parsePlan(readFileSync(options.plan ?? "", "utf8"));
  await requireCheckout(plan.commit);
  const outDir = resolve(options.out ?? "");
  mkdirSync(outDir, { recursive: true });
  const binaries = await buildRustBinaries(plan);
  const sites = await buildStaticSites(plan, ROOT);
  await packImages(plan, binaries, outDir, sites);
});
