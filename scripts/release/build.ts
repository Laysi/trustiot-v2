/**
 * Release stage 2, the `build` job of .github/workflows/release.yml: builds every planned image
 * for linux/arm64, with an SPDX SBOM each. Nothing is pushed.
 *
 *   bun scripts/release/build.ts --plan <plan.json> --out <dir>
 *
 * Rust images: the binaries are cross-compiled for aarch64-unknown-linux-musl in one Cargo
 * run, with Zig as C compiler and linker (cargo-zigbuild) and the dependency list embedded
 * (cargo-auditable), then each is packed with deploy/images/rust-binary.Containerfile.
 * Containerfile images are built from their directory. Builds never run arm64 code: a
 * Containerfile whose RUN steps execute in the arm64 stage fails without an emulator.
 *
 * This job runs third-party build code (Cargo build scripts and proc macros), so it has no
 * secrets and a read-only token, and its output is only data:
 *   <out>/images/<name>.oci.tar     the image as an OCI archive, pushed byte for byte by publish
 *   <out>/images/<name>.spdx.json   its SBOM from syft
 *   <out>/build.json                digest and size of each image, for the log
 */

import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { containerBuildArgs } from "../lib/build-context.ts";
import { log, parseOptions, protoTool, rawManifest, requireCheckout, run, runStage, systemTool, tryRun } from "./exec.ts";
import {
  type CargoPackage,
  IMAGE_PLATFORM,
  type PlannedImage,
  RUST_CONTAINERFILE,
  RUST_TARGET,
  type ReleasePlan,
  ReleaseError,
  aarch64ElfProblem,
  imagePlatformProblems,
  isMap,
  parsePlan,
  renderRustContainerfile,
  resolveBinary,
  rustSpec,
  summarizeSpdx,
} from "./lib.ts";

const ROOT = resolve(import.meta.dir, "..", "..");

/** The toolchain that rust-toolchain.toml pins; the same version as `rust` in .prototools. */
function pinnedToolchain(): string {
  const file = Bun.TOML.parse(readFileSync(join(ROOT, "rust-toolchain.toml"), "utf8"));
  const channel = isMap(file) && isMap(file.toolchain) ? file.toolchain.channel : undefined;
  if (typeof channel !== "string" || channel === "") {
    throw new ReleaseError("rust-toolchain.toml has no toolchain.channel");
  }
  return channel;
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

  const zigbuild = await protoTool("cargo-zigbuild");
  const auditable = await protoTool("cargo-auditable");
  const zig = await protoTool("zig");
  const packages = [...new Set(images.map((image) => image.package ?? ""))];
  log("build", `cross-compiling ${packages.join(", ")} for ${RUST_TARGET}`);
  await run(
    [cargo, "auditable", "zigbuild", "--locked", "--release", "--target", RUST_TARGET, "--bins", ...packages.flatMap((pkg) => ["--package", pkg])],
    {
      cwd: ROOT,
      stream: true,
      env: {
        // `cargo auditable` and `cargo zigbuild` are found as cargo subcommands on PATH.
        PATH: `${dirname(auditable)}:${dirname(zigbuild)}:${process.env.PATH ?? ""}`,
        CARGO_ZIGBUILD_ZIG_PATH: zig,
        // Readable by crates with option_env!, for example tools/release-selftest.
        TRUSTIOT_RELEASE_VERSION: plan.version,
        SOURCE_DATE_EPOCH: String(plan.sourceDateEpoch),
        // Panic messages carry source paths, which start with the machine's Cargo home or
        // checkout. Fixed prefixes keep the binary, and so the image digest, the same wherever
        // it is built. This replaces any RUSTFLAGS from the environment.
        CARGO_ENCODED_RUSTFLAGS: [`--remap-path-prefix=${cargoHome()}=/cargo`, `--remap-path-prefix=${ROOT}=/build`].join("\x1f"),
      },
    },
  );
  for (const image of images) {
    const bin = bins.get(image.name) ?? "";
    const path = join(metadata.target_directory, RUST_TARGET, "release", bin);
    if (!existsSync(path)) {
      throw new ReleaseError(`cargo did not produce ${path}`);
    }
    const problem = aarch64ElfProblem(new Uint8Array(readFileSync(path)));
    if (problem !== undefined) {
      throw new ReleaseError(`${path} is not an aarch64 binary: ${problem}`);
    }
    binaries.set(image.name, path);
  }
  return binaries;
}

interface BuiltImage {
  name: string;
  digest: string;
  size: number;
  archive: string;
  sbom: string;
  sbomPackages: number;
}

async function buildImage(
  plan: ReleasePlan,
  image: PlannedImage,
  binary: string | undefined,
  outDir: string,
  workDir: string,
): Promise<BuiltImage> {
  const podman = systemTool("podman");
  const skopeo = systemTool("skopeo");
  const syft = await protoTool("syft");
  const localTag = `localhost/trustiot-release/${image.name}:${plan.version}`;

  let context: string;
  let containerfile: string;
  if (image.kind === "rust") {
    if (binary === undefined) {
      throw new ReleaseError(`no binary was built for ${image.name}`);
    }
    // The build context holds only the binary and the rendered Containerfile.
    context = join(workDir, image.name);
    mkdirSync(context, { recursive: true });
    const bin = binary.split("/").at(-1) ?? "";
    copyFileSync(binary, join(context, bin));
    chmodSync(join(context, bin), 0o755);
    containerfile = join(context, "Containerfile");
    writeFileSync(containerfile, renderRustContainerfile(readFileSync(join(ROOT, RUST_CONTAINERFILE), "utf8"), bin));
  } else {
    context = join(ROOT, image.context ?? "");
    containerfile = join(context, "Containerfile");
  }

  const labels: Record<string, string> = {
    "org.opencontainers.image.source": `https://github.com/${plan.repository}`,
    "org.opencontainers.image.revision": plan.commit,
    "org.opencontainers.image.version": plan.version,
    "org.opencontainers.image.title": image.name,
  };
  log("build", `building ${image.ref} for ${IMAGE_PLATFORM}`);
  // containerBuildArgs checks the context and the Containerfile for key and credential files
  // first, and allows only known options (docs/architecture/secrets.md).
  await run(
    [
      podman,
      ...containerBuildArgs(context, [
        "--platform",
        IMAGE_PLATFORM,
        // Fixed timestamps (the commit time) make a rebuild of the same commit byte-identical.
        "--timestamp",
        String(plan.sourceDateEpoch),
        "--pull=missing",
        "--format",
        "oci",
        ...Object.entries(labels).flatMap(([key, value]) => ["--label", `${key}=${value}`]),
        "--file",
        containerfile,
        "--tag",
        localTag,
      ]),
    ],
    { stream: true },
  );

  const imagesDir = join(outDir, "images");
  mkdirSync(imagesDir, { recursive: true });
  const archive = join(imagesDir, `${image.name}.oci.tar`);
  rmSync(archive, { force: true });
  await run([podman, "push", "--format", "oci", "--compression-format", "gzip", localTag, `oci-archive:${archive}`]);
  await tryRun([podman, "image", "rm", localTag]);

  const { manifest, digest } = await rawManifest(skopeo, `oci-archive:${archive}`);
  const config = JSON.parse((await run([skopeo, "inspect", "--config", "--raw", `oci-archive:${archive}`])).stdout);
  const problems = imagePlatformProblems(manifest, config);
  if (problems.length > 0) {
    throw new ReleaseError(`${image.name}: ${problems.join("; ")}`);
  }

  const sbom = join(imagesDir, `${image.name}.spdx.json`);
  await run(
    [syft, "scan", `oci-archive:${archive}`, "--output", `spdx-json=${sbom}`, "--source-name", image.repository, "--source-version", plan.version],
    {
      env: {
        SYFT_CHECK_FOR_APP_UPDATE: "false",
        // Packages, their dependencies and the files they were found in. Every file a package
        // owns (about a thousand for tzdata alone) would make each SBOM attestation hundreds
        // of kilobytes without naming one more package.
        SYFT_FILE_METADATA_SELECTION: "none",
        SYFT_RELATIONSHIPS_PACKAGE_FILE_OWNERSHIP: "false",
      },
    },
  );
  const summary = summarizeSpdx(JSON.parse(readFileSync(sbom, "utf8")));
  // A Rust image's SBOM must list its crates, which only cargo-auditable data provides.
  if (image.kind === "rust" && !summary.packages.some((pkg) => pkg.startsWith(`${image.package}@`))) {
    throw new ReleaseError(`${image.name}: the SBOM does not list the Rust package ${image.package}; was the binary built with cargo-auditable?`);
  }
  log("build", `${image.ref} → ${digest}, SBOM ${summary.spdxVersion} with ${summary.packages.length} packages`);
  return { name: image.name, digest, size: statSync(archive).size, archive, sbom, sbomPackages: summary.packages.length };
}

await runStage("build", async () => {
  const options = parseOptions(Bun.argv.slice(2), { plan: {}, out: {} });
  const plan = parsePlan(readFileSync(options.plan ?? "", "utf8"));
  await requireCheckout(plan.commit);
  const outDir = resolve(options.out ?? "");
  mkdirSync(outDir, { recursive: true });
  const binaries = await buildRustBinaries(plan);
  const workDir = mkdtempSync(join(tmpdir(), "trustiot-release-"));
  try {
    const built: BuiltImage[] = [];
    for (const image of plan.images) {
      built.push(await buildImage(plan, image, binaries.get(image.name), outDir, workDir));
    }
    writeFileSync(
      join(outDir, "build.json"),
      `${JSON.stringify({ tag: plan.tag, commit: plan.commit, images: built.map(({ archive, sbom, ...rest }) => ({ ...rest, archive: archive.slice(outDir.length + 1), sbom: sbom.slice(outDir.length + 1) })) }, null, 2)}\n`,
    );
  } finally {
    rmSync(workDir, { recursive: true, force: true });
  }
});
