import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative } from "node:path";

import { parseBuildFile } from "../ci/check-secret-handling.ts";
import { buildFileStages, containerBuildArgs } from "../lib/build-context.ts";
import {
  buildChromiumLayer,
  CHROMIUM_LAYER_BUILD_CONTEXT,
  CHROMIUM_LAYER_IMAGE,
  CHROMIUM_LAYER_INPUTS,
  CHROMIUM_LAYER_STAGE,
  CHROMIUM_LAYER_STAGE_TEXT,
  CHROMIUM_STAGE,
  DASHBOARD_CHECK_CONTAINERFILE,
  writeChromiumLayerContext,
} from "./chromium-layer.ts";
import { CommandError, RunLog } from "./exec.ts";
import type { ExecOptions, ExecResult } from "./exec.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");
const ok = (stdout = ""): ExecResult => ({ code: 0, stdout, stderr: "", timedOut: false });

/** The repository files `entry` imports, followed to the end, and every import specifier met. */
function importClosure(entry: string): { files: string[]; specifiers: string[] } {
  const files = new Set<string>();
  const specifiers = new Set<string>();
  const queue = [entry];
  while (queue.length > 0) {
    const file = queue.pop()!;
    if (files.has(file)) continue;
    files.add(file);
    const source = readFileSync(join(REPO_ROOT, file), "utf8");
    for (const match of source.matchAll(/(?:\bfrom|\bimport\s*\(|^\s*import)\s*["']([^"']+)["']/gm)) {
      const specifier = match[1]!;
      specifiers.add(specifier);
      if (!specifier.startsWith(".")) continue;
      const target = relative(REPO_ROOT, join(REPO_ROOT, dirname(file), specifier));
      expect(existsSync(join(REPO_ROOT, target)), `${file} imports ${specifier}`).toBe(true);
      queue.push(target);
    }
  }
  return { files: [...files].sort(), specifiers: [...specifiers].sort() };
}

describe("what decides the Chromium layer (the cache key's inputs)", () => {
  test("cover the Containerfile and every module the builder imports, followed to the end", () => {
    expect(CHROMIUM_LAYER_INPUTS).toContain(DASHBOARD_CHECK_CONTAINERFILE);
    const { files, specifiers } = importClosure("scripts/nightly/chromium-layer.ts");
    for (const file of files) expect(CHROMIUM_LAYER_INPUTS, `${file} (imported by the layer's build) is not an input of its key`).toContain(file);
    expect(files).toContain("scripts/lib/build-context.ts");
    expect(files).toContain("scripts/ci/check-secret-handling.ts");
    // No package import: one would bring scripts/package.json and its lock file into the inputs.
    expect(specifiers.filter((specifier) => !specifier.startsWith(".") && !specifier.startsWith("node:"))).toEqual([]);
    // Nothing else, so the key does not change with code the layer does not depend on.
    expect([...CHROMIUM_LAYER_INPUTS].sort()).toEqual([DASHBOARD_CHECK_CONTAINERFILE, ...files].sort());
  });

  test("are files in this repository, and none is a directory such as scripts/", () => {
    for (const path of CHROMIUM_LAYER_INPUTS) {
      expect(existsSync(join(REPO_ROOT, path)), path).toBe(true);
      expect(statSync(join(REPO_ROOT, path)).isFile(), path).toBe(true);
    }
  });

  test("a pull request that changes one of them can run the smoke test", () => {
    const workflow = Bun.YAML.parse(readFileSync(join(REPO_ROOT, ".github", "workflows", "nightly.yml"), "utf8")) as {
      on: { pull_request: { paths: string[] } };
    };
    const globs = workflow.on.pull_request.paths.map((path) => new Bun.Glob(path));
    for (const path of CHROMIUM_LAYER_INPUTS) expect(globs.some((glob) => glob.match(path)), path).toBe(true);
  });
});

describe("the dashboard check's Containerfile", () => {
  const text = readFileSync(join(REPO_ROOT, DASHBOARD_CHECK_CONTAINERFILE), "utf8");
  /** The instructions of each stage, in order. */
  const stages = () => {
    const result: { name: string | undefined; instructions: ReturnType<typeof parseBuildFile> }[] = [];
    for (const instruction of parseBuildFile(text)) {
      if (instruction.keyword === "FROM") {
        result.push({ name: /\sAS\s+(\S+)\s*$/i.exec(` ${instruction.args}`)?.[1]?.toLowerCase(), instructions: [] });
      }
      result.at(-1)?.instructions.push(instruction);
    }
    return result;
  };

  test("has the stage the layer replaces, and the final stage takes only /rootfs/ from it", () => {
    expect(buildFileStages(text)).toContain(CHROMIUM_STAGE);
    expect(buildFileStages(text)).not.toContain(CHROMIUM_LAYER_STAGE);
    const final = stages().at(-1)!;
    const fromChromium = final.instructions.filter((instruction) => instruction.keyword === "COPY" && instruction.args.includes(`--from=${CHROMIUM_STAGE}`));
    expect(fromChromium.map((instruction) => instruction.args)).toEqual([`--from=${CHROMIUM_STAGE} /rootfs/ /`]);
    // No other stage uses it, so replacing it changes nothing but where /rootfs/ comes from.
    for (const stage of stages().slice(0, -1)) {
      for (const instruction of stage.instructions) expect(instruction.args, `${stage.name}: ${instruction.args}`).not.toContain(`--from=${CHROMIUM_STAGE}`);
    }
  });

  test("the stages the layer builds copy nothing from the build context, so a context with the Containerfile alone is enough", () => {
    const needed = stages().filter((stage) => stage.name === CHROMIUM_STAGE || stage.name === "base");
    expect(needed.map((stage) => stage.name)).toEqual(["base", CHROMIUM_STAGE]);
    for (const stage of needed) {
      for (const instruction of stage.instructions.filter((candidate) => candidate.keyword === "COPY" || candidate.keyword === "ADD")) {
        expect(instruction.args, `${stage.name}: ${instruction.keyword} ${instruction.args}`).toMatch(/^--from=/);
      }
    }
  });
});

describe("writeChromiumLayerContext", () => {
  let root: string;
  let dir: string;
  beforeEach(() => {
    root = mkdtempSync(join(tmpdir(), "nightly-chromium-layer-root-"));
    dir = mkdtempSync(join(tmpdir(), "nightly-chromium-layer-context-"));
    mkdirSync(join(root, dirname(DASHBOARD_CHECK_CONTAINERFILE)), { recursive: true });
    copyFileSync(join(REPO_ROOT, DASHBOARD_CHECK_CONTAINERFILE), join(root, DASHBOARD_CHECK_CONTAINERFILE));
  });
  afterEach(() => {
    rmSync(root, { recursive: true, force: true });
    rmSync(dir, { recursive: true, force: true });
  });

  test("copies the Containerfile unchanged and appends the stage that keeps /rootfs/ alone", () => {
    const file = writeChromiumLayerContext(root, dir);
    expect(file).toBe(join(dir, "Containerfile"));
    expect(readdirSync(dir)).toEqual(["Containerfile"]);
    const original = readFileSync(join(REPO_ROOT, DASHBOARD_CHECK_CONTAINERFILE), "utf8");
    expect(readFileSync(file, "utf8")).toBe(`${original}${CHROMIUM_LAYER_STAGE_TEXT}`);
    expect(CHROMIUM_LAYER_STAGE_TEXT).toBe("\nFROM scratch AS nightly-chromium-rootfs\nCOPY --from=chromium /rootfs/ /rootfs/\n");
    expect(buildFileStages(readFileSync(file, "utf8")).at(-1)).toBe(CHROMIUM_LAYER_STAGE);
    // The build guard accepts the context and the build the layer runs.
    expect(() => containerBuildArgs(dir, ["--platform", "linux/arm64", "--target", CHROMIUM_LAYER_STAGE, "--file", file, "--tag", CHROMIUM_LAYER_IMAGE])).not.toThrow();
  });

  test("ends the copied text with a line break before the new stage", () => {
    writeFileSync(join(root, DASHBOARD_CHECK_CONTAINERFILE), "FROM scratch AS chromium");
    expect(readFileSync(writeChromiumLayerContext(root, dir), "utf8")).toBe(`FROM scratch AS chromium\n${CHROMIUM_LAYER_STAGE_TEXT}`);
  });

  test("refuses a Containerfile without the chromium stage, or one that has the appended stage already", () => {
    writeFileSync(join(root, DASHBOARD_CHECK_CONTAINERFILE), "FROM scratch AS browser\n");
    expect(() => writeChromiumLayerContext(root, dir)).toThrow("has no stage named chromium (stages: browser)");
    writeFileSync(join(root, DASHBOARD_CHECK_CONTAINERFILE), `FROM scratch AS chromium\nFROM scratch AS ${CHROMIUM_LAYER_STAGE}\n`);
    expect(() => writeChromiumLayerContext(root, dir)).toThrow(`already has a stage named ${CHROMIUM_LAYER_STAGE}`);
  });
});

describe("the dashboard check's build on the layer", () => {
  test("replaces the chromium stage of the real Containerfile with the local layer, which the build guard accepts", () => {
    expect(CHROMIUM_LAYER_BUILD_CONTEXT).toEqual(["--build-context", "chromium=docker-image://localhost/trustiot-e2e/dashboard-check-chromium:nightly"]);
    const dir = mkdtempSync(join(tmpdir(), "nightly-chromium-layer-final-"));
    try {
      copyFileSync(join(REPO_ROOT, DASHBOARD_CHECK_CONTAINERFILE), join(dir, "Containerfile"));
      const args = containerBuildArgs(dir, ["--platform", "linux/arm64", ...CHROMIUM_LAYER_BUILD_CONTEXT, "--file", join(dir, "Containerfile"), "--tag", "localhost/trustiot-e2e/dashboard-check:nightly"]);
      expect(args.slice(0, 5)).toEqual(["build", "--platform", "linux/arm64", ...CHROMIUM_LAYER_BUILD_CONTEXT]);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

describe("buildChromiumLayer", () => {
  let root: string;
  let log: RunLog;
  beforeEach(() => {
    root = mkdtempSync(join(tmpdir(), "nightly-chromium-layer-build-"));
    mkdirSync(join(root, dirname(DASHBOARD_CHECK_CONTAINERFILE)), { recursive: true });
    copyFileSync(join(REPO_ROOT, DASHBOARD_CHECK_CONTAINERFILE), join(root, DASHBOARD_CHECK_CONTAINERFILE));
    log = new RunLog();
  });
  afterEach(() => rmSync(root, { recursive: true, force: true }));

  /** A fake docker: `failures` builds fail first; inspect answers `platform`. */
  function fakeDocker({ failures = 0, platform = "linux/arm64" } = {}) {
    const seen: { commands: string[][]; context?: string; contextFiles?: string[]; containerfile?: string } = { commands: [] };
    const run = async (_log: RunLog, cmd: string[], _options?: ExecOptions) => {
      seen.commands.push(cmd);
      if (cmd[1] === "build") {
        seen.context = cmd.at(-1);
        seen.contextFiles = readdirSync(seen.context!).sort();
        seen.containerfile = readFileSync(cmd[cmd.indexOf("--file") + 1]!, "utf8");
        if (failures > 0) {
          failures -= 1;
          throw new CommandError(cmd, { code: 1, stdout: "", stderr: "E: Failed to fetch https://snapshot.debian.org/... 503 No healthy backends", timedOut: false });
        }
        return ok();
      }
      if (cmd[1] === "image") return ok(`${platform}\n`);
      throw new Error(`unexpected command ${cmd.join(" ")}`);
    };
    return { seen, commands: { run } };
  }

  test("builds only the layer's stage for linux/arm64 under its tag, checks it, and removes the context", async () => {
    const { seen, commands } = fakeDocker();
    expect(await buildChromiumLayer(log, root, commands, { pauseMs: 0 })).toBe(CHROMIUM_LAYER_IMAGE);
    const file = join(seen.context!, "Containerfile");
    expect(seen.commands).toEqual([
      ["docker", "build", "--platform", "linux/arm64", "--target", CHROMIUM_LAYER_STAGE, "--file", file, "--tag", CHROMIUM_LAYER_IMAGE, seen.context!],
      ["docker", "image", "inspect", "--format", "{{.Os}}/{{.Architecture}}", CHROMIUM_LAYER_IMAGE],
    ]);
    expect(seen.contextFiles).toEqual(["Containerfile"]);
    expect(seen.containerfile?.endsWith(CHROMIUM_LAYER_STAGE_TEXT)).toBe(true);
    expect(existsSync(seen.context!)).toBe(false);
  });

  test("tries the build twice, since snapshot.debian.org fails at times, and removes the context when both fail", async () => {
    const twice = fakeDocker({ failures: 1 });
    expect(await buildChromiumLayer(log, root, twice.commands, { pauseMs: 0 })).toBe(CHROMIUM_LAYER_IMAGE);
    expect(twice.seen.commands.filter((cmd) => cmd[1] === "build")).toHaveLength(2);
    const failing = fakeDocker({ failures: 2 });
    await expect(buildChromiumLayer(log, root, failing.commands, { pauseMs: 0 })).rejects.toThrow("503 No healthy backends");
    expect(failing.seen.commands.some((cmd) => cmd[1] === "image")).toBe(false);
    expect(existsSync(failing.seen.context!)).toBe(false);
  });

  test("fails when docker does not hold the layer for linux/arm64 after the build", async () => {
    const { commands } = fakeDocker({ platform: "linux/amd64" });
    await expect(buildChromiumLayer(log, root, commands, { pauseMs: 0 })).rejects.toThrow(`docker built ${CHROMIUM_LAYER_IMAGE} for linux/amd64, not linux/arm64`);
  });

  test("builds nothing from a Containerfile without the chromium stage", async () => {
    writeFileSync(join(root, DASHBOARD_CHECK_CONTAINERFILE), "FROM scratch\n");
    const { seen, commands } = fakeDocker();
    await expect(buildChromiumLayer(log, root, commands, { pauseMs: 0 })).rejects.toThrow("has no stage named chromium");
    expect(seen.commands).toEqual([]);
  });
});
