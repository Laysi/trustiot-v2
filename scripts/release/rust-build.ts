/**
 * How a Rust service binary is built for an image, shared by release.yml's build job
 * (build.ts) and the nightly's service images (scripts/nightly/service-image.ts), so that the
 * nightly deploys what a release would publish. Like lib.ts, it only parses, validates and
 * renders; rust-build.test.ts covers it without tools.
 *
 * Kept apart from lib.ts on purpose: scripts/ci/changes.ts imports lib.ts, so every change to
 * lib.ts runs every CI job.
 */

import { dirname } from "node:path";
import { RUST_TARGET, ReleaseError, errorMessage, isMap } from "./lib.ts";

/** `p_type` of the program header that names the dynamic loader. */
const ELF_PT_INTERP = 3;

/**
 * Returns why a 64-bit little-endian ELF file is not statically linked, or undefined. A file
 * that names a program interpreter (PT_INTERP, for example /lib/ld-musl-aarch64.so.1) needs
 * that loader and a libc at run time, which the distroless static base does not have, so the
 * container would fail to start with "no such file or directory". A static PIE, what Rust
 * builds for musl targets, has no interpreter and passes. Check the machine with lib.ts's
 * aarch64ElfProblem.
 */
export function staticElfProblem(bytes: Uint8Array): string | undefined {
  if (bytes.byteLength < 64 || bytes[4] !== 2 || bytes[5] !== 1) {
    return "it is not a 64-bit little-endian ELF file";
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const phoff = Number(view.getBigUint64(32, true));
  const phentsize = view.getUint16(54, true);
  const phnum = view.getUint16(56, true);
  if (phnum === 0) {
    return "it has no program headers, so it is not an executable";
  }
  if (phentsize < 4 || phoff + phentsize * phnum > bytes.byteLength) {
    return "its program header table is truncated";
  }
  for (let index = 0; index < phnum; index += 1) {
    if (view.getUint32(phoff + index * phentsize, true) === ELF_PT_INTERP) {
      return "it is dynamically linked (it names a program interpreter); build it for a musl target with static linking";
    }
  }
  return undefined;
}

/** Reads `toolchain.channel` from rust-toolchain.toml, the toolchain every Rust build uses. */
export function toolchainChannel(rustToolchainToml: string): string {
  let file: unknown;
  try {
    file = Bun.TOML.parse(rustToolchainToml);
  } catch (error) {
    throw new ReleaseError(`rust-toolchain.toml: cannot parse TOML: ${errorMessage(error)}`);
  }
  const channel = isMap(file) && isMap(file.toolchain) ? file.toolchain.channel : undefined;
  if (typeof channel !== "string" || channel === "") {
    throw new ReleaseError("rust-toolchain.toml has no toolchain.channel");
  }
  return channel;
}

/** A Cargo package name as an argument: never with a leading hyphen, which cargo reads as an option. */
const CARGO_PACKAGE = /^[A-Za-z0-9_][A-Za-z0-9_-]*$/;

/** Where the tools of a Rust cross-compilation are, and what it builds. */
export interface RustCrossCompile {
  /** The `cargo` that rustup provides. */
  cargo: string;
  /** Executables pinned in .prototools. */
  zig: string;
  cargoZigbuild: string;
  cargoAuditable: string;
  /** The Cargo packages whose binaries are built. */
  packages: readonly string[];
  /** The repository root and the Cargo home, which panic messages' source paths start with. */
  root: string;
  cargoHome: string;
  /** The PATH to extend with the two Cargo subcommands. */
  path: string;
  /** Set by the release only: the version (TRUSTIOT_RELEASE_VERSION) and SOURCE_DATE_EPOCH. */
  version?: string;
  sourceDateEpoch?: number;
}

/**
 * The one Cargo command that cross-compiles service binaries for images, and its environment:
 * aarch64-unknown-linux-musl, statically linked, with Zig as C compiler and linker
 * (cargo-zigbuild) and the dependency list embedded (cargo-auditable). Run it from `root`.
 */
export function rustCrossCompileCommand(build: RustCrossCompile): { cmd: string[]; env: Record<string, string> } {
  if (build.packages.length === 0) {
    throw new ReleaseError("no Cargo package to build");
  }
  for (const pkg of build.packages) {
    if (!CARGO_PACKAGE.test(pkg)) {
      throw new ReleaseError(`\`${pkg}\` is not a Cargo package name`);
    }
  }
  const env: Record<string, string> = {
    // `cargo auditable` and `cargo zigbuild` are found as cargo subcommands on PATH.
    PATH: `${dirname(build.cargoAuditable)}:${dirname(build.cargoZigbuild)}:${build.path}`,
    CARGO_ZIGBUILD_ZIG_PATH: build.zig,
    // Panic messages carry source paths, which start with the machine's Cargo home or
    // checkout. Fixed prefixes keep the binary, and so the image digest, the same wherever
    // it is built. This replaces any RUSTFLAGS from the environment.
    CARGO_ENCODED_RUSTFLAGS: [`--remap-path-prefix=${build.cargoHome}=/cargo`, `--remap-path-prefix=${build.root}=/build`].join("\x1f"),
  };
  if (build.version !== undefined) {
    // Readable by crates with option_env!, for example tools/release-selftest.
    env.TRUSTIOT_RELEASE_VERSION = build.version;
  }
  if (build.sourceDateEpoch !== undefined) {
    env.SOURCE_DATE_EPOCH = String(build.sourceDateEpoch);
  }
  const cmd = [
    build.cargo, "auditable", "zigbuild", "--locked", "--release", "--target", RUST_TARGET, "--bins",
    ...build.packages.flatMap((pkg) => ["--package", pkg]),
  ];
  return { cmd, env };
}
