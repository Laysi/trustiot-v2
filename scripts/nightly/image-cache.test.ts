import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";

import { exec, RunLog } from "./exec.ts";
import type { ExecOptions, ExecResult } from "./exec.ts";
import { gitObjectHashes, imageCacheFile, imageCacheInputs, imageCacheKey, loadCachedImage, saveImageForCache } from "./image-cache.ts";
import type { ObjectHashes } from "./image-cache.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");

/** tools/migrate and every local crate it reaches through path dependencies or the workspace's [patch]. */
function migrateCrates(): string[] {
  const seen = new Set<string>();
  const visit = (dir: string) => {
    if (seen.has(dir)) return;
    seen.add(dir);
    const manifest = readFileSync(join(REPO_ROOT, dir, "Cargo.toml"), "utf8");
    for (const match of manifest.matchAll(/path\s*=\s*"([^"]+)"/g)) {
      const target = relative(REPO_ROOT, join(REPO_ROOT, dir, match[1]!));
      if (existsSync(join(REPO_ROOT, target, "Cargo.toml"))) visit(target);
    }
  };
  visit("tools/migrate");
  const root = readFileSync(join(REPO_ROOT, "Cargo.toml"), "utf8");
  for (const match of root.matchAll(/path\s*=\s*"([^"]+)"/g)) {
    if (existsSync(join(REPO_ROOT, match[1]!, "Cargo.toml"))) visit(match[1]!);
  }
  return [...seen].sort();
}
const IMAGE = "localhost/trustiot-e2e/migrate:nightly";
const ok = (stdout = ""): ExecResult => ({ code: 0, stdout, stderr: "", timedOut: false });

/** Object hashes from a table; a path that is not in it is missing from HEAD. */
function hashesFrom(table: Record<string, string>): ObjectHashes {
  return async (paths) =>
    paths.map((path) => {
      const hash = table[path];
      if (hash === undefined) throw new Error(`not in HEAD: ${path}`);
      return hash;
    });
}
const baseline = () => Object.fromEntries(imageCacheInputs("migrate").map((path, index) => [path, index.toString(16).padStart(40, "0")]));

describe("the cache key of a nightly Rust image", () => {
  test("is the same for the same inputs, and names the image and its platform", async () => {
    const key = await imageCacheKey("migrate", hashesFrom(baseline()));
    expect(key).toMatch(/^nightly-image-migrate-linux-arm64-[0-9a-f]{64}$/);
    expect(await imageCacheKey("migrate", hashesFrom(baseline()))).toBe(key);
  });

  test("changes when any single input changes", async () => {
    const key = await imageCacheKey("migrate", hashesFrom(baseline()));
    const keys = new Set([key]);
    for (const path of imageCacheInputs("migrate")) {
      const changed = { ...baseline(), [path]: "f".repeat(40) };
      keys.add(await imageCacheKey("migrate", hashesFrom(changed)));
    }
    expect(keys.size).toBe(imageCacheInputs("migrate").length + 1);
  });

  test("differs between images and covers each image's own directory", async () => {
    expect(imageCacheInputs("migrate")).toContain("deploy/images/migrate");
    expect(imageCacheInputs("identity")).toContain("deploy/images/identity");
    expect(imageCacheInputs("identity")).not.toContain("deploy/images/migrate");
    const identity = Object.fromEntries(imageCacheInputs("identity").map((path, index) => [path, index.toString(16).padStart(40, "0")]));
    expect(await imageCacheKey("identity", hashesFrom(identity))).not.toBe(await imageCacheKey("migrate", hashesFrom(baseline())));
  });

  test("fails when an input is missing, instead of leaving it out", async () => {
    const missing = baseline();
    delete missing["Cargo.lock"];
    await expect(imageCacheKey("migrate", hashesFrom(missing))).rejects.toThrow("Cargo.lock");
    await expect(imageCacheKey("../migrate", hashesFrom(baseline()))).rejects.toThrow("not an image name");
  });

  test("covers the workspace, the toolchain and every local crate the migration Job builds from", () => {
    const inputs = imageCacheInputs("migrate");
    for (const path of ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".prototools", "deploy/images/rust-binary.Containerfile", "scripts"]) {
      expect(inputs).toContain(path);
    }
    // How CI builds it: the ignore file a container build reads, the workflow and the action that
    // installs the build tools (Laysi/trustiot-v2#60 round 4, R6).
    for (const path of [".dockerignore", ".github/workflows/nightly.yml", ".github/actions/setup-proto"]) expect(inputs).toContain(path);
    // Every path dependency reachable from tools/migrate, and every [patch] path, lies under an input.
    const under = (path: string) => inputs.some((input) => path === input || path.startsWith(`${input}/`));
    const crates = migrateCrates();
    for (const dir of crates) expect(under(dir), dir).toBe(true);
    expect(crates.length).toBeGreaterThan(2);
    // Every workspace member directory is under an input (a false miss costs a build).
    const root = Bun.TOML.parse(readFileSync(join(REPO_ROOT, "Cargo.toml"), "utf8")) as { workspace: { members: string[] } };
    for (const member of root.workspace.members) expect(under(member.replace(/\/\*$/, "")), member).toBe(true);
  });

  test("a pull request that changes what goes into the migration Job's image can run the smoke test", () => {
    // nightly.yml's pull_request paths: a labelled pull request runs the smoke only when it
    // touches one of them, so a change to the image's build must be among them.
    const workflow = Bun.YAML.parse(readFileSync(join(REPO_ROOT, ".github", "workflows", "nightly.yml"), "utf8")) as {
      on: { pull_request: { paths: string[] } };
    };
    const paths = workflow.on.pull_request.paths;
    for (const dir of migrateCrates()) expect(paths, dir).toContain(`${dir}/**`);
    for (const file of ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".prototools", "deploy/images/rust-binary.Containerfile", ".dockerignore", ".github/workflows/nightly.yml"]) {
      expect(paths, file).toContain(file);
    }
    for (const dir of ["deploy/images/migrate", "scripts", ".proto/plugins"]) expect(paths, dir).toContain(`${dir}/**`);
    // Every other input of the key is in the list too, except the Rust directories as a whole:
    // only the crates above go into this image.
    for (const input of imageCacheInputs("migrate").filter((path) => !["crates", "services", "tools", "edge", "factory"].includes(path))) {
      expect(paths.includes(input) || paths.includes(`${input}/**`), input).toBe(true);
    }
    // The SQL the Job embeds.
    expect(paths).toContain("services/*/migrations/**");
  });

  test("every input exists in this repository, so the key step cannot fail on a renamed path unnoticed", () => {
    for (const path of imageCacheInputs("migrate")) expect(existsSync(join(REPO_ROOT, path)), path).toBe(true);
  });
});

describe("gitObjectHashes", () => {
  let repo: string;
  const git = (...args: string[]) => exec(["git", "-c", "user.name=t", "-c", "user.email=t@example.test", "-c", "commit.gpgsign=false", ...args], { cwd: repo });

  beforeEach(async () => {
    repo = mkdtempSync(join(tmpdir(), "nightly-image-cache-"));
    await git("init", "--quiet");
    writeFileSync(join(repo, "a.txt"), "a\n");
    await git("add", "a.txt");
    await git("commit", "--quiet", "--message", "a");
  });
  afterEach(() => rmSync(repo, { recursive: true, force: true }));

  test("returns the object hash of each path at HEAD", async () => {
    const [hash] = await gitObjectHashes(repo)(["a.txt"]);
    expect(hash).toBe((await git("rev-parse", "HEAD:a.txt")).stdout.trim());
  });

  test("refuses inputs that differ from HEAD, so the key never describes other sources", async () => {
    writeFileSync(join(repo, "a.txt"), "changed\n");
    await expect(gitObjectHashes(repo)(["a.txt"])).rejects.toThrow("differ from HEAD");
  });

  test("refuses a path that is not in HEAD", async () => {
    await expect(gitObjectHashes(repo)(["a.txt", "missing"])).rejects.toThrow("not in HEAD");
  });
});

describe("loading and saving the cached image", () => {
  let dir: string;
  let log: RunLog;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "nightly-image-cache-dir-"));
    log = new RunLog();
  });
  afterEach(() => rmSync(dir, { recursive: true, force: true }));

  function fakeDocker(platform = "linux/arm64") {
    const seen: string[][] = [];
    const run = async (_log: RunLog, cmd: string[], _options?: ExecOptions) => {
      seen.push(cmd);
      if (cmd[1] === "image") return ok(`${platform}\n`);
      if (cmd[1] === "save") writeFileSync(cmd[3]!, "tarball");
      return ok();
    };
    return { seen, commands: { run } };
  }

  test("a miss runs nothing", async () => {
    const { seen, commands } = fakeDocker();
    expect(await loadCachedImage(log, dir, "migrate", IMAGE, commands)).toBe("missing");
    expect(seen).toEqual([]);
  });

  test("a hit loads the tarball and checks the image's platform", async () => {
    writeFileSync(imageCacheFile(dir, "migrate"), "tarball");
    const { seen, commands } = fakeDocker();
    expect(await loadCachedImage(log, dir, "migrate", IMAGE, commands)).toBe("loaded");
    expect(seen).toEqual([
      ["docker", "load", "--input", join(dir, "migrate.tar")],
      ["docker", "image", "inspect", "--format", "{{.Os}}/{{.Architecture}}", IMAGE],
    ]);
  });

  /** The run log's lines, without their time stamps. */
  const logged = (path: string) =>
    readFileSync(path, "utf8")
      .trimEnd()
      .split("\n")
      .map((line) => line.replace(/^\S+ /, ""));

  test("a cached image for another platform is not used: the caller builds, and the run says why", async () => {
    const path = join(dir, "run.log");
    writeFileSync(imageCacheFile(dir, "migrate"), "tarball");
    expect(await loadCachedImage(new RunLog(path), dir, "migrate", IMAGE, fakeDocker("linux/amd64").commands)).toBe("unusable");
    expect(logged(path).at(-1)).toContain(`the cached ${IMAGE} cannot be used (it is for linux/amd64, not linux/arm64); building it instead`);
  });

  test("a partial or corrupt tarball is not used: the caller builds, and the run says how to drop the entry", async () => {
    const path = join(dir, "run.log");
    writeFileSync(imageCacheFile(dir, "migrate"), "not a tarball");
    const broken = { run: async () => Promise.reject(new Error("docker load: unexpected EOF\nmore output")) };
    expect(await loadCachedImage(new RunLog(path), dir, "migrate", IMAGE, broken)).toBe("unusable");
    const last = logged(path).at(-1)!;
    expect(last).toContain("docker load failed: docker load: unexpected EOF); building it instead");
    expect(last).toContain("Delete the Actions cache entry nightly-image-migrate-");
  });

  test("an image docker loaded without the expected tag is not used", async () => {
    writeFileSync(imageCacheFile(dir, "migrate"), "tarball");
    const run = async (_log: RunLog, cmd: string[]) => {
      if (cmd[1] === "image") throw new Error("No such image");
      return ok();
    };
    expect(await loadCachedImage(log, dir, "migrate", IMAGE, { run })).toBe("unusable");
  });

  test("saving writes the tarball under its final name only when docker save finished", async () => {
    const { seen, commands } = fakeDocker();
    await saveImageForCache(log, dir, "migrate", IMAGE, commands);
    expect(seen).toEqual([["docker", "save", "--output", join(dir, "migrate.tar.partial"), IMAGE]]);
    expect(readdirSync(dir)).toEqual(["migrate.tar"]);

    // A miss restored nothing: the directory does not exist yet on the first saving run.
    const fresh = join(dir, "not-restored");
    await saveImageForCache(log, fresh, "migrate", IMAGE, fakeDocker().commands);
    expect(readdirSync(fresh)).toEqual(["migrate.tar"]);

    rmSync(join(dir, "migrate.tar"));
    const failing = { run: async () => Promise.reject(new Error("docker save failed")) };
    await expect(saveImageForCache(log, dir, "migrate", IMAGE, failing)).rejects.toThrow("docker save failed");
    expect(existsSync(imageCacheFile(dir, "migrate"))).toBe(false);
  });
});
