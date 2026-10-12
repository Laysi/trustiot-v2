import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import { dynamicAarch64Binary, ELF_X86_64, elfFile, PT_LOAD, staticAarch64Binary } from "../release/elf-fixture.ts";
import { discoverImages, resolveBinary, RUST_CONTAINERFILE } from "../release/lib.ts";
import { CommandError, RunLog } from "./exec.ts";
import type { ExecOptions, ExecResult } from "./exec.ts";
import type { ImportCommands } from "./platform.ts";
import { buildServiceImage, loadServiceImage, MIGRATE_IMAGE_NAME, serviceImageRef, serviceImageValues } from "./service-image.ts";
import type { ServiceImageTools } from "./service-image.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");
const TOOLS: ServiceImageTools = {
  rustup: "/home/runner/.cargo/bin/rustup",
  cargo: "/home/runner/.cargo/bin/cargo",
  zig: "/proto/tools/zig/0.16.0/zig",
  cargoZigbuild: "/proto/tools/cargo-zigbuild/0.23.4/cargo-zigbuild",
  cargoAuditable: "/proto/tools/cargo-auditable/0.7.6/cargo-auditable",
  containerTool: "docker",
};
const IMAGE = "localhost/trustiot-e2e/identity:nightly";
const K3D = "/pinned/k3d";
const CLUSTER = "nightly";
const NODE = "k3d-nightly-server-0";

const ok = (stdout = ""): ExecResult => ({ code: 0, stdout, stderr: "", timedOut: false });
const failed = (stderr: string): ExecResult => ({ code: 1, stdout: "", stderr, timedOut: false });

describe("service image names", () => {
  test("are the local e2e names that the build guard and the chart accept", () => {
    expect(serviceImageRef("identity")).toBe(IMAGE);
    const values = serviceImageValues("identity");
    expect(`${values.repository}:${values.tag}`).toBe(IMAGE);
    expect(values).toMatchObject({ digest: "", pullPolicy: "IfNotPresent" });
  });

  test("the migration Job's image the nightly builds is the one the smoke values name", () => {
    const migrate = discoverImages(REPO_ROOT).find((image) => image.name === MIGRATE_IMAGE_NAME);
    expect(migrate).toMatchObject({ kind: "rust", package: "migrate" });
    const smoke = Bun.YAML.parse(readFileSync(join(REPO_ROOT, "deploy", "chart", "tests", "values", "smoke.yaml"), "utf8")) as {
      migrate: { image: { repository: string; tag: string; pullPolicy: string } };
    };
    const { digest: _digest, ...expected } = serviceImageValues(MIGRATE_IMAGE_NAME);
    expect(smoke.migrate.image).toEqual(expected);
  });

  test("the repository declares the identity service image, and its package has the one binary", () => {
    const identity = discoverImages(REPO_ROOT).find((image) => image.name === "identity");
    expect(identity).toEqual({ kind: "rust", name: "identity", package: "identity" });
    // The package's binary targets as `cargo metadata` would list them, from its Cargo.toml.
    const manifest = Bun.TOML.parse(readFileSync(join(REPO_ROOT, "services", "identity", "Cargo.toml"), "utf8")) as {
      package: { name: string };
      bin?: { name: string }[];
    };
    const bins = (manifest.bin ?? []).map((bin) => ({ name: bin.name, kind: ["bin"] }));
    if (identity?.kind === "rust") {
      expect(resolveBinary(identity, [{ name: manifest.package.name, targets: bins }])).toBe("identity");
    }
  });
});

describe("buildServiceImage and loadServiceImage", () => {
  let root: string;
  let log: RunLog;
  let logPath: string;

  /** A repository with the real image template and toolchain file, and two image declarations. */
  beforeEach(() => {
    root = mkdtempSync(join(tmpdir(), "nightly-service-image-"));
    const copy = (path: string) => {
      mkdirSync(dirname(join(root, path)), { recursive: true });
      copyFileSync(join(REPO_ROOT, path), join(root, path));
    };
    copy(RUST_CONTAINERFILE);
    copy("rust-toolchain.toml");
    copy("deploy/images/identity/image.toml");
    mkdirSync(join(root, "deploy/images/postgres-age"), { recursive: true });
    writeFileSync(join(root, "deploy/images/postgres-age/Containerfile"), "FROM scratch\n");
    logPath = join(root, "run.log");
    log = new RunLog(logPath);
  });
  afterEach(() => rmSync(root, { recursive: true, force: true }));

  interface Seen {
    commands: string[][];
    cargoEnv?: Record<string, string>;
    cargoCwd?: string;
    /** The files of the build context while the image is built, and the rendered Containerfile. */
    context?: string;
    contextFiles?: string[];
    containerfile?: string;
    imports: number;
  }

  /**
   * Fake rustup, cargo, docker and k3d. `binary` is what cargo writes, or null for a build
   * that produces nothing; `imageBuildFailures` docker builds fail first; `hasImage` says whether
   * the node holds the image after an import.
   */
  function fakeTools({
    binary = staticAarch64Binary() as Uint8Array | null,
    imageBuildFailures = 0,
    hasImage = true,
  } = {}) {
    const seen: Seen = { commands: [], imports: 0 };
    const target = join(root, "target");
    const commands: ImportCommands = {
      run: async (runLog, cmd, options?: ExecOptions) => {
        runLog.line(`$ ${cmd.join(" ")}`);
        seen.commands.push(cmd);
        if (cmd[0] === TOOLS.rustup) return ok();
        if (cmd[0] === TOOLS.cargo && cmd[1] === "metadata") {
          expect(options?.cwd).toBe(root);
          return ok(JSON.stringify({ packages: [{ name: "identity", targets: [{ name: "identity", kind: ["bin"] }] }], target_directory: target }));
        }
        if (cmd[0] === TOOLS.cargo) {
          seen.cargoEnv = options?.env;
          seen.cargoCwd = options?.cwd;
          if (binary !== null) {
            const out = join(target, "aarch64-unknown-linux-musl", "release");
            mkdirSync(out, { recursive: true });
            writeFileSync(join(out, "identity"), binary);
          }
          return ok();
        }
        if (cmd[0] === TOOLS.containerTool) {
          const file = cmd[cmd.indexOf("--file") + 1]!;
          seen.context = cmd.at(-1);
          seen.contextFiles = readdirSync(seen.context!).sort();
          seen.containerfile = readFileSync(file, "utf8");
          if (imageBuildFailures > 0) {
            imageBuildFailures -= 1;
            throw new CommandError(cmd, failed("failed to resolve source metadata for gcr.io/distroless/static-debian13"));
          }
          return ok();
        }
        expect(cmd.slice(0, 3)).toEqual([K3D, "image", "import"]);
        seen.imports += 1;
        return ok();
      },
      exec: async (cmd: string[]) => {
        if (cmd[0] === K3D) {
          return ok(JSON.stringify([{ name: NODE, role: "server", runtimeLabels: { "k3d.cluster": CLUSTER } }]));
        }
        expect(cmd).toEqual(["docker", "exec", NODE, "crictl", "inspecti", IMAGE]);
        return hasImage ? ok("{}") : failed(`no such image: "${IMAGE}"`);
      },
    };
    return { seen, commands };
  }

  const build = (commands: ImportCommands, name = "identity") => buildServiceImage({ repoRoot: root, log }, name, TOOLS, commands, { pauseMs: 0 });
  const load = (commands: ImportCommands) =>
    loadServiceImage({ repoRoot: root, log, k3d: K3D, cluster: CLUSTER }, "identity", { tools: TOOLS, commands, pauseMs: 0 });

  test("cross-compiles with the release's Cargo command and builds an arm64 image from the binary alone", async () => {
    const { seen, commands } = fakeTools();
    expect(await build(commands)).toBe(IMAGE);
    const toolchain = (Bun.TOML.parse(readFileSync(join(root, "rust-toolchain.toml"), "utf8")) as { toolchain: { channel: string } }).toolchain.channel;
    expect(seen.commands.slice(0, 4)).toEqual([
      [TOOLS.rustup, "toolchain", "install", toolchain, "--profile", "minimal", "--no-self-update"],
      [TOOLS.rustup, "target", "add", "--toolchain", toolchain, "aarch64-unknown-linux-musl"],
      [TOOLS.cargo, "metadata", "--no-deps", "--format-version", "1", "--locked"],
      [TOOLS.cargo, "auditable", "zigbuild", "--locked", "--release", "--target", "aarch64-unknown-linux-musl", "--bins", "--package", "identity"],
    ]);
    expect(seen.cargoCwd).toBe(root);
    expect(seen.cargoEnv?.CARGO_ZIGBUILD_ZIG_PATH).toBe(TOOLS.zig);
    expect(seen.cargoEnv?.PATH?.startsWith("/proto/tools/cargo-auditable/0.7.6:/proto/tools/cargo-zigbuild/0.23.4:")).toBe(true);
    // A nightly image is not a release: no release version is compiled in.
    expect(seen.cargoEnv?.TRUSTIOT_RELEASE_VERSION).toBeUndefined();

    const imageBuild = seen.commands[4]!;
    expect(imageBuild.slice(0, 2)).toEqual(["docker", "build"]);
    expect(imageBuild).toContain("--platform");
    expect(imageBuild[imageBuild.indexOf("--platform") + 1]).toBe("linux/arm64");
    expect(imageBuild[imageBuild.indexOf("--tag") + 1]).toBe(IMAGE);
    expect(seen.contextFiles).toEqual(["Containerfile", "identity"]);
    expect(seen.containerfile).toContain("COPY --chmod=0555 identity /usr/local/bin/identity");
    expect(seen.containerfile).toContain('ENTRYPOINT ["/usr/local/bin/identity"]');
    expect(seen.containerfile).toMatch(/^FROM gcr\.io\/distroless\/static-debian13:nonroot@sha256:[0-9a-f]{64}$/m);
    // The context is a throwaway directory, removed after the build.
    expect(existsSync(seen.context!)).toBe(false);
    expect(seen.commands).toHaveLength(5);
  });

  test("loadServiceImage imports the image into the cluster and confirms it on every node", async () => {
    const { seen, commands } = fakeTools();
    expect(await load(commands)).toBe(IMAGE);
    expect(seen.commands.at(-1)).toEqual([K3D, "image", "import", IMAGE, "--cluster", CLUSTER, "--mode", "direct"]);
    expect(readFileSync(logPath, "utf8")).toContain(`image ${IMAGE} confirmed on 1 node(s): ${NODE}`);
  });

  test("loadServiceImage fails when the image never reaches the node", async () => {
    const { seen, commands } = fakeTools({ hasImage: false });
    await expect(load(commands)).rejects.toThrow(`image ${IMAGE} is missing on node(s) ${NODE}`);
    expect(seen.imports).toBe(3);
  });

  test("retries an image build that failed to pull the base image", async () => {
    const { seen, commands } = fakeTools({ imageBuildFailures: 1 });
    expect(await build(commands)).toBe(IMAGE);
    expect(seen.commands.filter((cmd) => cmd[0] === "docker")).toHaveLength(2);
    expect(existsSync(seen.context!)).toBe(false);
  });

  test("removes the build context when the image build fails", async () => {
    const { seen, commands } = fakeTools({ imageBuildFailures: 2 });
    await expect(build(commands)).rejects.toThrow("failed to resolve source metadata");
    expect(existsSync(seen.context!)).toBe(false);
  });

  test.each([
    ["a dynamically linked binary", dynamicAarch64Binary(), "dynamically linked"],
    ["an x86_64 binary", elfFile(ELF_X86_64, [PT_LOAD]), "ELF machine is 62"],
    ["no binary", null, "cargo did not produce"],
  ])("refuses %s and builds no image", async (_label, binary, message) => {
    const { seen, commands } = fakeTools({ binary });
    await expect(build(commands)).rejects.toThrow(message);
    expect(seen.commands.some((cmd) => cmd[0] === "docker")).toBe(false);
  });

  test("refuses an image that is not declared, or not a Rust service, before running anything", async () => {
    const { seen, commands } = fakeTools();
    await expect(build(commands, "assets")).rejects.toThrow("deploy/images has no image named assets (images: identity, postgres-age)");
    await expect(build(commands, "postgres-age")).rejects.toThrow("not a Rust service image");
    expect(seen.commands).toHaveLength(0);
  });
});
