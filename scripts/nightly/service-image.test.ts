import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import { dynamicAarch64Binary, ELF_X86_64, elfFile, PT_LOAD, staticAarch64Binary } from "../release/elf-fixture.ts";
import { discoverImages, resolveBinary, RUST_CONTAINERFILE } from "../release/lib.ts";
import { apiValues } from "./api.ts";
import { CHROMIUM_LAYER_BUILD_CONTEXT, CHROMIUM_LAYER_IMAGE, CHROMIUM_LAYER_STAGE, DASHBOARD_CHECK_CONTAINERFILE } from "./chromium-layer.ts";
import { CommandError, RunLog } from "./exec.ts";
import type { ExecOptions, ExecResult } from "./exec.ts";
import { NIGHTLY_IMAGES } from "./image-cache.ts";
import type { ImportCommands } from "./platform.ts";
import { buildServiceImage, cachedProgramsProblem, installBuildTools, loadServiceImage, MIGRATE_IMAGE_NAME, serviceImageRef, serviceImageValues } from "./service-image.ts";
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
const BUN = "/pinned/bun";

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

  test("the service images the nightly builds are the ones the API check's values deploy", () => {
    // Laysi/trustiot-v2#65: the smoke enables identity and assets and calls them through the ingress (api.ts).
    const values = apiValues("saas", []) as { services: Record<string, { enabled?: boolean; image?: Record<string, string> }> };
    const services = NIGHTLY_IMAGES.filter((image) => image !== MIGRATE_IMAGE_NAME);
    expect(Object.keys(values.services).sort()).toEqual([...services].sort());
    for (const name of services) {
      expect(discoverImages(REPO_ROOT).find((image) => image.name === name), name).toMatchObject({ kind: "rust", package: name });
      const { digest: _digest, ...expected } = serviceImageValues(name);
      expect(values.services[name]?.enabled, name).toBe(true);
      expect(values.services[name]?.image, name).toEqual(expected);
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
    /** Every image build: its arguments and the files of its context while it ran. */
    builds: { cmd: string[]; contextFiles: string[] }[];
    imports: number;
  }

  /**
   * Fake rustup, cargo, docker and k3d. `binary` is what cargo writes, or null for a build
   * that produces nothing; `imageBuildFailures` docker builds fail first; `hasImage` says whether
   * the node holds the image after an import; `extraBins` are the package's other binaries and
   * what cargo writes for each (null: nothing); `image` is the tag the nodes are asked for.
   */
  function fakeTools({
    binary = staticAarch64Binary() as Uint8Array | null,
    imageBuildFailures = 0,
    hasImage = true,
    loadFails = false,
    extraBins = {} as Record<string, Uint8Array | null>,
    image = IMAGE,
  } = {}) {
    const seen: Seen = { commands: [], builds: [], imports: 0 };
    const target = join(root, "target");
    const commands: ImportCommands = {
      run: async (runLog, cmd, options?: ExecOptions) => {
        runLog.line(`$ ${cmd.join(" ")}`);
        seen.commands.push(cmd);
        if (cmd[0] === TOOLS.rustup) return ok();
        // A static site build (static-build.ts): `bun run --cwd <project> build` writes dist/.
        if (cmd[0] === BUN) {
          expect(options?.cwd).toBe(root);
          if (cmd[1] === "run" && cmd[4] === "build") {
            const dist = join(root, cmd[3]!, "dist");
            mkdirSync(join(dist, "sandbox"), { recursive: true });
            writeFileSync(join(dist, "index.html"), "<!doctype html>");
            writeFileSync(join(dist, "sandbox", "check.html"), "<!doctype html>");
          }
          return ok();
        }
        if (cmd[0] === TOOLS.cargo && cmd[1] === "metadata") {
          expect(options?.cwd).toBe(root);
          return ok(JSON.stringify({ packages: [{ name: "identity", targets: [{ name: "identity", kind: ["bin"] }, ...Object.keys(extraBins).map((name) => ({ name, kind: ["bin"] }))] }], target_directory: target }));
        }
        if (cmd[0] === TOOLS.cargo) {
          seen.cargoEnv = options?.env;
          seen.cargoCwd = options?.cwd;
          if (binary !== null) {
            const out = join(target, "aarch64-unknown-linux-musl", "release");
            mkdirSync(out, { recursive: true });
            writeFileSync(join(out, "identity"), binary);
            for (const [name, bytes] of Object.entries(extraBins)) if (bytes !== null) writeFileSync(join(out, name), bytes);
          }
          return ok();
        }
        // The image cache (image-cache.ts): load, inspect and save.
        if (cmd[0] === "docker" && cmd[1] === "load") {
          if (loadFails) throw new Error("docker load: unexpected EOF");
          return ok();
        }
        // The fallback's tool install: a test that reaches it stops there.
        if (cmd[0] === "proto") throw new Error("proto install reached");
        if (cmd[0] === "docker" && cmd[1] === "image") return ok("linux/arm64\n");
        if (cmd[0] === "docker" && cmd[1] === "save") {
          writeFileSync(cmd[3]!, "tarball");
          return ok();
        }
        if (cmd[0] === TOOLS.containerTool) {
          const file = cmd[cmd.indexOf("--file") + 1]!;
          seen.context = cmd.at(-1);
          seen.contextFiles = readdirSync(seen.context!).sort();
          seen.containerfile = readFileSync(file, "utf8");
          seen.builds.push({ cmd, contextFiles: seen.contextFiles });
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
        expect(cmd).toEqual(["docker", "exec", NODE, "crictl", "inspecti", image]);
        return hasImage ? ok("{}") : failed(`no such image: "${image}"`);
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

  test("loadServiceImage takes a cached image without looking up or running any Rust tool", async () => {
    const cacheDir = join(root, "image-cache");
    mkdirSync(cacheDir);
    writeFileSync(join(cacheDir, "identity.tar"), "tarball");
    const { seen, commands } = fakeTools();
    // No `tools`: on a hit the pinned build tools need not be installed, and are not looked up.
    const image = await loadServiceImage({ repoRoot: root, log, k3d: K3D, cluster: CLUSTER }, "identity", {
      commands,
      pauseMs: 0,
      cache: { dir: cacheDir, save: true },
    });
    expect(image).toBe(IMAGE);
    expect(seen.commands.map((cmd) => cmd.slice(0, 2))).toEqual([
      ["docker", "load"],
      ["docker", "image"],
      [K3D, "image"],
    ]);
    expect(seen.cargoEnv).toBeUndefined();
  });

  test("loadServiceImage builds on a miss, and writes the image for the cache only when asked", async () => {
    for (const save of [false, true]) {
      const cacheDir = mkdtempSync(join(root, "image-cache-"));
      const { seen, commands } = fakeTools();
      await loadServiceImage({ repoRoot: root, log, k3d: K3D, cluster: CLUSTER }, "identity", { tools: TOOLS, commands, pauseMs: 0, cache: { dir: cacheDir, save } });
      expect(seen.cargoEnv).toBeDefined();
      expect(seen.commands.some((cmd) => cmd[0] === "docker" && cmd[1] === "save")).toBe(save);
      expect(existsSync(join(cacheDir, "identity.tar"))).toBe(save);
      expect(seen.commands.at(-1)?.slice(0, 3)).toEqual([K3D, "image", "import"]);
    }
  });

  test("loadServiceImage builds when the cached tarball cannot be loaded, and says so", async () => {
    const cacheDir = join(root, "image-cache");
    mkdirSync(cacheDir);
    writeFileSync(join(cacheDir, "identity.tar"), "partial");
    const { seen, commands } = fakeTools({ loadFails: true });
    const image = await loadServiceImage({ repoRoot: root, log, k3d: K3D, cluster: CLUSTER }, "identity", {
      tools: TOOLS,
      commands,
      pauseMs: 0,
      cache: { dir: cacheDir, save: false },
    });
    expect(image).toBe(IMAGE);
    expect(seen.commands[0]?.slice(0, 2)).toEqual(["docker", "load"]);
    expect(seen.cargoEnv).toBeDefined();
    expect(seen.commands.at(-1)?.slice(0, 3)).toEqual([K3D, "image", "import"]);
    expect(readFileSync(logPath, "utf8")).toContain(`the cached ${IMAGE} cannot be used (docker load failed: docker load: unexpected EOF); building it instead`);
  });

  test("loadServiceImage installs the build tools the workflow skipped before it falls back to a build", async () => {
    const cacheDir = join(root, "image-cache");
    mkdirSync(cacheDir);
    writeFileSync(join(cacheDir, "identity.tar"), "partial");
    const { seen, commands } = fakeTools({ loadFails: true });
    // No `tools`: as in CI, where a hit skipped installing them.
    await expect(
      loadServiceImage({ repoRoot: root, log, k3d: K3D, cluster: CLUSTER }, "identity", { commands, pauseMs: 0, cache: { dir: cacheDir, save: false } }),
    ).rejects.toThrow("proto install reached");
    expect(seen.commands.at(-1)).toEqual(["proto", "install", "zig"]);
  });

  test("installBuildTools installs each pinned build tool with its own proto install, in the repository", async () => {
    const seen: { cmd: string[]; cwd?: string }[] = [];
    const run = async (_log: RunLog, cmd: string[], options?: ExecOptions) => {
      seen.push({ cmd, cwd: options?.cwd });
      return ok();
    };
    await installBuildTools({ repoRoot: root, log }, { run });
    expect(seen).toEqual(
      ["zig", "cargo-zigbuild", "cargo-auditable"].map((tool) => ({ cmd: ["proto", "install", tool], cwd: root })),
    );
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

  describe("programs built with an image (certs-ca, Laysi/trustiot-v2#517)", () => {
    const PROGRAM = "identity-tool";
    const programsDir = () => join(root, "programs");
    // A package with two binaries names the image's, like deploy/images/certs/image.toml.
    beforeEach(() => writeFileSync(join(root, "deploy/images/identity/image.toml"), '[rust]\npackage = "identity"\nbin = "identity"\n'));

    test("the build checks another binary of the package and copies it out, never into the image", async () => {
      const { seen, commands } = fakeTools({ extraBins: { [PROGRAM]: staticAarch64Binary() } });
      expect(await buildServiceImage({ repoRoot: root, log }, "identity", TOOLS, commands, { pauseMs: 0, programs: { names: [PROGRAM], dir: programsDir() } })).toBe(IMAGE);
      const copied = join(programsDir(), PROGRAM);
      expect(readFileSync(copied)).toEqual(Buffer.from(staticAarch64Binary()));
      expect(statSync(copied).mode & 0o777).toBe(0o755);
      // One compile for both (`--bins`), and the image's context holds the service binary only.
      expect(seen.commands.filter((cmd) => cmd[0] === TOOLS.cargo && cmd[1] === "auditable")).toHaveLength(1);
      expect(seen.contextFiles).toEqual(["Containerfile", "identity"]);
    });

    test.each([
      ["a name that is not a binary of the package", "missing", { [PROGRAM]: staticAarch64Binary() }, "package identity has no other binary named missing"],
      ["the image's own binary", "identity", {}, "package identity has no other binary named identity"],
      ["a program cargo did not produce", PROGRAM, { [PROGRAM]: null }, "cargo did not produce"],
      ["a dynamically linked program", PROGRAM, { [PROGRAM]: dynamicAarch64Binary() }, "cannot run on the arm64 runner: it is dynamically linked"],
    ])("refuses %s and builds no image", async (_label, name, extraBins, message) => {
      const { seen, commands } = fakeTools({ extraBins });
      await expect(
        buildServiceImage({ repoRoot: root, log }, "identity", TOOLS, commands, { pauseMs: 0, programs: { names: [name], dir: programsDir() } }),
      ).rejects.toThrow(message);
      expect(seen.commands.some((cmd) => cmd[0] === "docker")).toBe(false);
      expect(existsSync(join(programsDir(), name))).toBe(false);
    });

    test("a cache hit takes the programs from the entry, without a Rust build", async () => {
      const cacheDir = join(root, "image-cache");
      mkdirSync(cacheDir);
      writeFileSync(join(cacheDir, "identity.tar"), "tarball");
      writeFileSync(join(cacheDir, PROGRAM), staticAarch64Binary());
      const { seen, commands } = fakeTools();
      await loadServiceImage({ repoRoot: root, log, k3d: K3D, cluster: CLUSTER }, "identity", {
        commands,
        pauseMs: 0,
        cache: { dir: cacheDir, save: true },
        programs: { names: [PROGRAM], dir: programsDir() },
      });
      expect(seen.cargoEnv).toBeUndefined();
      expect(readFileSync(join(programsDir(), PROGRAM))).toEqual(Buffer.from(staticAarch64Binary()));
      expect(cachedProgramsProblem(cacheDir, [PROGRAM])).toBeUndefined();
    });

    test.each([
      ["lacks a program", null, "the entry has no identity-tool"],
      ["holds a program that cannot run on the runner", elfFile(ELF_X86_64, [PT_LOAD]), "its identity-tool cannot run on the arm64 runner"],
    ])("a cache entry that %s is built again, and the run says so", async (_label, cachedProgram, message) => {
      const cacheDir = join(root, "image-cache");
      mkdirSync(cacheDir);
      writeFileSync(join(cacheDir, "identity.tar"), "tarball");
      if (cachedProgram !== null) writeFileSync(join(cacheDir, PROGRAM), cachedProgram);
      const { seen, commands } = fakeTools({ extraBins: { [PROGRAM]: staticAarch64Binary() } });
      await loadServiceImage({ repoRoot: root, log, k3d: K3D, cluster: CLUSTER }, "identity", {
        tools: TOOLS,
        commands,
        pauseMs: 0,
        cache: { dir: cacheDir, save: false },
        programs: { names: [PROGRAM], dir: programsDir() },
      });
      expect(seen.cargoEnv).toBeDefined();
      expect(readFileSync(logPath, "utf8")).toContain(`the cached ${IMAGE} cannot be used (${message}`);
      expect(readFileSync(join(programsDir(), PROGRAM))).toEqual(Buffer.from(staticAarch64Binary()));
    });

    test("a build writes the programs for the cache only when asked, so a saved entry has them", async () => {
      for (const save of [false, true]) {
        const cacheDir = mkdtempSync(join(root, "image-cache-"));
        const { commands } = fakeTools({ extraBins: { [PROGRAM]: staticAarch64Binary() } });
        await loadServiceImage({ repoRoot: root, log, k3d: K3D, cluster: CLUSTER }, "identity", {
          tools: TOOLS,
          commands,
          pauseMs: 0,
          cache: { dir: cacheDir, save },
          programs: { names: [PROGRAM], dir: programsDir() },
        });
        expect(existsSync(join(cacheDir, PROGRAM))).toBe(save);
        expect(existsSync(join(cacheDir, `${PROGRAM}.partial`))).toBe(false);
      }
    });
  });

  describe("an image with its own Containerfile and a static site (dashboard-check, Laysi/trustiot-v2#150)", () => {
    const OWN = "# the image's own Containerfile\nFROM gcr.io/distroless/cc-debian13:nonroot@sha256:" + "a".repeat(64) + "\nCOPY identity /usr/local/bin/identity\nCOPY www/sandbox/ /srv/www/sandbox/\n";
    beforeEach(() => {
      writeFileSync(join(root, "deploy/images/identity/image.toml"), '[rust]\npackage = "identity"\ncontainerfile = "Containerfile"\n\n[static]\nproject = "site"\n');
      writeFileSync(join(root, "deploy/images/identity/Containerfile"), OWN);
      mkdirSync(join(root, "site"));
      writeFileSync(join(root, "site", "package.json"), JSON.stringify({ scripts: { build: "vite build", "check:dist": "bun check-dist.ts" } }));
      writeFileSync(join(root, "site", "bun.lock"), "{}");
    });

    test("builds the project as release.yml does and packs its output with the image's own Containerfile", async () => {
      const { seen, commands } = fakeTools();
      expect(await buildServiceImage({ repoRoot: root, log }, "identity", TOOLS, commands, { pauseMs: 0, bun: BUN })).toBe(IMAGE);
      expect(seen.commands.filter((cmd) => cmd[0] === BUN)).toEqual([
        [BUN, "install", "--cwd", "site", "--frozen-lockfile", "--ignore-scripts"],
        [BUN, "run", "--cwd", "site", "build"],
        [BUN, "run", "--cwd", "site", "check:dist"],
      ]);
      expect(seen.contextFiles).toEqual(["Containerfile", "identity", "www"]);
      expect(seen.containerfile).toBe(OWN);
      expect(seen.commands.at(-1)?.slice(0, 2)).toEqual(["docker", "build"]);
    });

    test("packs a given site instead of building the project, under a given tag", async () => {
      const site = join(root, "given");
      mkdirSync(join(site, "sandbox"), { recursive: true });
      writeFileSync(join(site, "sandbox", "check.html"), "given");
      const tag = "localhost/trustiot-e2e/identity:image-test";
      let www: string[] = [];
      const { seen, commands } = fakeTools();
      const recording: ImportCommands = {
        ...commands,
        run: async (runLog, cmd, options) => {
          if (cmd[0] === "docker" && cmd[1] === "build") www = readdirSync(join(cmd.at(-1)!, "www", "sandbox"));
          return commands.run(runLog, cmd, options);
        },
      };
      expect(await buildServiceImage({ repoRoot: root, log }, "identity", TOOLS, recording, { pauseMs: 0, bun: BUN, site, tag })).toBe(tag);
      expect(seen.commands.some((cmd) => cmd[0] === BUN)).toBe(false);
      expect(www).toEqual(["check.html"]);
      const imageBuild = seen.commands.at(-1)!;
      expect(imageBuild[imageBuild.indexOf("--tag") + 1]).toBe(tag);
    });

    test("refuses an image's own Containerfile that runs a program in the arm64 stage, before building it", async () => {
      writeFileSync(join(root, "deploy/images/identity/Containerfile"), `${OWN}RUN ["/usr/local/bin/identity", "--version"]\n`);
      const { seen, commands } = fakeTools();
      await expect(buildServiceImage({ repoRoot: root, log }, "identity", TOOLS, commands, { pauseMs: 0, bun: BUN })).rejects.toThrow("RUN");
      expect(seen.commands.some((cmd) => cmd[0] === "docker")).toBe(false);
    });

    test("refuses a site for an image without a static project, before running anything", async () => {
      writeFileSync(join(root, "deploy/images/identity/image.toml"), '[rust]\npackage = "identity"\n');
      rmSync(join(root, "deploy/images/identity/Containerfile"));
      const { seen, commands } = fakeTools();
      await expect(buildServiceImage({ repoRoot: root, log }, "identity", TOOLS, commands, { pauseMs: 0, site: join(root, "given") })).rejects.toThrow(
        "deploy/images/identity has no [static] project, so it takes no site",
      );
      expect(seen.commands).toHaveLength(0);
    });
  });

  describe("the dashboard check's image on the nightly's Chromium layer (Laysi/trustiot-v2#1127)", () => {
    const DASHBOARD = "localhost/trustiot-e2e/dashboard-check:nightly";
    const LAYER_TAR = "dashboard-check-chromium.tar";
    // The real Containerfile, packing the fake identity binary: what matters here is its stages.
    beforeEach(() => {
      mkdirSync(join(root, "deploy/images/dashboard-check"), { recursive: true });
      writeFileSync(join(root, "deploy/images/dashboard-check/image.toml"), '[rust]\npackage = "identity"\nbin = "identity"\ncontainerfile = "Containerfile"\n');
      copyFileSync(join(REPO_ROOT, DASHBOARD_CHECK_CONTAINERFILE), join(root, DASHBOARD_CHECK_CONTAINERFILE));
    });
    const caches = () => ({ rust: mkdtempSync(join(root, "image-cache-")), chromium: mkdtempSync(join(root, "chromium-cache-")) });
    const loadDashboard = (commands: ImportCommands, rust: string, chromium: { cache?: { dir: string; save: boolean } }) =>
      loadServiceImage({ repoRoot: root, log, k3d: K3D, cluster: CLUSTER }, "dashboard-check", {
        tools: TOOLS,
        commands,
        pauseMs: 0,
        cache: { dir: rust, save: false },
        chromiumLayer: chromium,
      });
    const layerBuilds = (seen: Seen) => seen.builds.filter(({ cmd }) => cmd.includes("--target"));
    const finalBuild = (seen: Seen) => {
      const builds = seen.builds.filter(({ cmd }) => !cmd.includes("--target"));
      expect(builds).toHaveLength(1);
      return builds[0]!.cmd;
    };

    test("a hit of the layer's entry loads it, checks its platform and builds the image on it, with no download", async () => {
      const { rust, chromium } = caches();
      writeFileSync(join(chromium, LAYER_TAR), "tarball");
      const { seen, commands } = fakeTools({ image: DASHBOARD });
      expect(await loadDashboard(commands, rust, { cache: { dir: chromium, save: true } })).toBe(DASHBOARD);
      expect(seen.commands.slice(0, 2)).toEqual([
        ["docker", "load", "--input", join(chromium, LAYER_TAR)],
        ["docker", "image", "inspect", "--format", "{{.Os}}/{{.Architecture}}", CHROMIUM_LAYER_IMAGE],
      ]);
      expect(layerBuilds(seen)).toEqual([]);
      // A hit is never saved again.
      expect(seen.commands.some((cmd) => cmd[1] === "save")).toBe(false);
      const build = finalBuild(seen);
      expect(build.slice(0, 6)).toEqual(["docker", "build", "--platform", "linux/arm64", ...CHROMIUM_LAYER_BUILD_CONTEXT]);
      expect(build[build.indexOf("--tag") + 1]).toBe(DASHBOARD);
      expect(readFileSync(logPath, "utf8")).toContain(`loaded ${CHROMIUM_LAYER_IMAGE} from the cache, no download from snapshot.debian.org`);
    });

    test("a miss builds the layer first, writes it for the cache only when asked, and builds the image on it", async () => {
      for (const save of [false, true, undefined]) {
        const { rust, chromium } = caches();
        const { seen, commands } = fakeTools({ image: DASHBOARD });
        await loadDashboard(commands, rust, save === undefined ? {} : { cache: { dir: chromium, save } });
        const layer = layerBuilds(seen);
        expect(layer).toHaveLength(1);
        expect(layer[0]!.cmd.slice(0, 6)).toEqual(["docker", "build", "--platform", "linux/arm64", "--target", CHROMIUM_LAYER_STAGE]);
        expect(layer[0]!.cmd[layer[0]!.cmd.indexOf("--tag") + 1]).toBe(CHROMIUM_LAYER_IMAGE);
        expect(layer[0]!.contextFiles).toEqual(["Containerfile"]);
        // Before the Rust build, so a failing download stops the run before the compile.
        const index = (match: (cmd: string[]) => boolean) => seen.commands.findIndex(match);
        expect(index((cmd) => cmd[0] === "docker" && cmd.includes(CHROMIUM_LAYER_STAGE))).toBeLessThan(index((cmd) => cmd[0] === TOOLS.cargo && cmd[1] === "auditable"));
        expect(seen.commands.some((cmd) => cmd[0] === "docker" && cmd[1] === "save")).toBe(save === true);
        expect(existsSync(join(chromium, LAYER_TAR))).toBe(save === true);
        expect(finalBuild(seen)).toEqual(expect.arrayContaining([...CHROMIUM_LAYER_BUILD_CONTEXT]));
        expect(seen.commands.at(-1)?.slice(0, 3)).toEqual([K3D, "image", "import"]);
      }
    });

    test("a layer tarball that cannot be loaded is reported, naming its own entry, and the layer is built", async () => {
      const { rust, chromium } = caches();
      writeFileSync(join(chromium, LAYER_TAR), "partial");
      const { seen, commands } = fakeTools({ image: DASHBOARD, loadFails: true });
      await loadDashboard(commands, rust, { cache: { dir: chromium, save: false } });
      expect(readFileSync(logPath, "utf8")).toContain(
        `the cached ${CHROMIUM_LAYER_IMAGE} cannot be used (docker load failed: docker load: unexpected EOF); building it instead. Delete the Actions cache entry nightly-chromium-layer-…`,
      );
      expect(layerBuilds(seen)).toHaveLength(1);
      expect(finalBuild(seen)).toEqual(expect.arrayContaining([...CHROMIUM_LAYER_BUILD_CONTEXT]));
    });

    test("a dashboard check image from the Rust images' entry needs no layer", async () => {
      const { rust, chromium } = caches();
      writeFileSync(join(rust, "dashboard-check.tar"), "tarball");
      writeFileSync(join(chromium, LAYER_TAR), "tarball");
      const { seen, commands } = fakeTools({ image: DASHBOARD });
      await loadDashboard(commands, rust, { cache: { dir: chromium, save: true } });
      expect(seen.commands.map((cmd) => cmd.slice(0, 2))).toEqual([
        ["docker", "load"],
        ["docker", "image"],
        [K3D, "image"],
      ]);
      expect(seen.commands[0]).toEqual(["docker", "load", "--input", join(rust, "dashboard-check.tar")]);
    });

    test("without the option the image is built as release.yml builds it, running its chromium stage", async () => {
      const { seen, commands } = fakeTools({ image: DASHBOARD });
      await buildServiceImage({ repoRoot: root, log }, "dashboard-check", TOOLS, commands, { pauseMs: 0 });
      expect(seen.builds).toHaveLength(1);
      expect(seen.builds[0]!.cmd).not.toContain("--build-context");
    });

    test("no other image is built on the layer", async () => {
      const { seen, commands } = fakeTools();
      await expect(buildServiceImage({ repoRoot: root, log }, "identity", TOOLS, commands, { pauseMs: 0, chromiumLayer: true })).rejects.toThrow(
        "only deploy/images/dashboard-check is built on the nightly's Chromium layer, not deploy/images/identity",
      );
      await expect(
        loadServiceImage({ repoRoot: root, log, k3d: K3D, cluster: CLUSTER }, "identity", { tools: TOOLS, commands, pauseMs: 0, chromiumLayer: {} }),
      ).rejects.toThrow("only deploy/images/dashboard-check is built on the nightly's Chromium layer");
      expect(seen.commands).toEqual([]);
    });
  });

  test("refuses an image that is not declared, or not a Rust service, before running anything", async () => {
    const { seen, commands } = fakeTools();
    await expect(build(commands, "assets")).rejects.toThrow("deploy/images has no image named assets (images: identity, postgres-age)");
    await expect(build(commands, "postgres-age")).rejects.toThrow("not a Rust service image");
    expect(seen.commands).toHaveLength(0);
  });
});
