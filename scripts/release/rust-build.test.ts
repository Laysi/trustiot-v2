import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { ELF_AARCH64, PT_INTERP, PT_LOAD, dynamicAarch64Binary, elfFile, staticAarch64Binary } from "./elf-fixture.ts";
import { RUST_TARGET, aarch64ElfProblem } from "./lib.ts";
import { rustCrossCompileCommand, staticElfProblem, toolchainChannel } from "./rust-build.ts";

const ROOT = resolve(import.meta.dir, "..", "..");

describe("staticElfProblem", () => {
  test("accepts a static PIE, which has a dynamic section but no interpreter", () => {
    expect(staticElfProblem(staticAarch64Binary())).toBeUndefined();
    expect(aarch64ElfProblem(staticAarch64Binary())).toBeUndefined();
  });

  test("rejects a binary that names a program interpreter", () => {
    expect(staticElfProblem(dynamicAarch64Binary())).toContain("dynamically linked");
    expect(staticElfProblem(elfFile(ELF_AARCH64, [PT_LOAD, PT_LOAD, PT_INTERP]))).toContain("dynamically linked");
  });

  test("rejects files it cannot read as an executable", () => {
    expect(staticElfProblem(elfFile(ELF_AARCH64, []))).toContain("no program headers");
    expect(staticElfProblem(staticAarch64Binary().slice(0, 100))).toContain("truncated");
    expect(staticElfProblem(elfFile(ELF_AARCH64, [PT_LOAD], 1))).toContain("64-bit");
    expect(staticElfProblem(new Uint8Array(10))).toContain("64-bit");
  });

  test("rejects the dynamically linked bun running this test", () => {
    // Bun's Linux builds link glibc dynamically.
    if (process.platform === "linux") {
      expect(staticElfProblem(new Uint8Array(readFileSync(process.execPath)))).toContain("dynamically linked");
    }
  });
});

describe("toolchainChannel", () => {
  test("reads the repository's pinned toolchain", () => {
    expect(toolchainChannel(readFileSync(join(ROOT, "rust-toolchain.toml"), "utf8"))).toMatch(/^\d+\.\d+\.\d+$/);
  });

  test("rejects a file without a channel", () => {
    expect(() => toolchainChannel('[toolchain]\nprofile = "minimal"\n')).toThrow("no toolchain.channel");
    expect(() => toolchainChannel("not toml = = =")).toThrow("cannot parse");
  });
});

describe("rustCrossCompileCommand", () => {
  const base = {
    cargo: "/home/u/.cargo/bin/cargo",
    zig: "/proto/zig/0.16.0/zig",
    cargoZigbuild: "/proto/cargo-zigbuild/0.23.4/cargo-zigbuild",
    cargoAuditable: "/proto/cargo-auditable/0.7.6/cargo-auditable",
    packages: ["identity", "migrate"],
    root: "/work/trustiot-v2",
    cargoHome: "/home/u/.cargo",
    path: "/usr/bin:/bin",
  };

  test("cross-compiles every package for musl in one Cargo run, with cargo-auditable and cargo-zigbuild", () => {
    const { cmd, env } = rustCrossCompileCommand(base);
    expect(cmd).toEqual([
      base.cargo, "auditable", "zigbuild", "--locked", "--release", "--target", RUST_TARGET, "--bins",
      "--package", "identity", "--package", "migrate",
    ]);
    expect(RUST_TARGET).toBe("aarch64-unknown-linux-musl");
    expect(env.PATH).toBe("/proto/cargo-auditable/0.7.6:/proto/cargo-zigbuild/0.23.4:/usr/bin:/bin");
    expect(env.CARGO_ZIGBUILD_ZIG_PATH).toBe(base.zig);
    expect(env.CARGO_ENCODED_RUSTFLAGS?.split("\x1f")).toEqual([
      "--remap-path-prefix=/home/u/.cargo=/cargo",
      "--remap-path-prefix=/work/trustiot-v2=/build",
    ]);
  });

  test("sets the release version and timestamp only when given", () => {
    const plain = rustCrossCompileCommand(base).env;
    expect(plain.TRUSTIOT_RELEASE_VERSION).toBeUndefined();
    expect(plain.SOURCE_DATE_EPOCH).toBeUndefined();
    const release = rustCrossCompileCommand({ ...base, version: "1.2.0-rc.1", sourceDateEpoch: 1_791_630_056 }).env;
    expect(release.TRUSTIOT_RELEASE_VERSION).toBe("1.2.0-rc.1");
    expect(release.SOURCE_DATE_EPOCH).toBe("1791630056");
  });

  test("rejects no package, and names that cargo would read as an option or that are not Cargo names", () => {
    expect(() => rustCrossCompileCommand({ ...base, packages: [] })).toThrow("no Cargo package");
    expect(() => rustCrossCompileCommand({ ...base, packages: ["--workspace"] })).toThrow("not a Cargo package name");
    expect(() => rustCrossCompileCommand({ ...base, packages: ["a b"] })).toThrow("not a Cargo package name");
  });
});
