import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, posix, relative } from "node:path";

import type { WorkflowRun } from "../release/lib.ts";
import {
  affectsCargoDeny,
  affectsEveryJob,
  DECISION_CODE,
  affectsHelm,
  affectsHost,
  affectsPostgresImage,
  affectsRust,
  changedFiles,
  decide,
  type ListRuns,
  main,
  mainAncestors,
  mainRunsUrl,
  newestGreen,
  outputLines,
  PATH_JOBS,
  plan,
  pullRequestRange,
  runEverything,
  summary,
} from "./changes.ts";

const ROOT = join(import.meta.dir, "..", "..");

/** Which of the path-dependent jobs `files` run, as a sorted list. */
function running(files: readonly string[]): string[] {
  const decision = decide(files);
  return PATH_JOBS.filter((job) => decision[job].run).toSorted();
}

describe("path rules", () => {
  test("a docs-only change skips rust, cargo-deny, helm, host and postgres-image", () => {
    expect(running(["docs/architecture/ci-release.md"])).toEqual([]);
    expect(
      running([
        "docs/README.md",
        "docs/test-reports/2026-10-11-309-ci-cost.md",
        "docs/test-reports/data/2026-10-11-309-ci-cost/runs.json",
        "docs/runbooks/secrets.md",
      ]),
    ).toEqual([]);
  });

  // Every class of path the rust job reads; one example each, plus the odd places.
  test.each([
    ["Rust source in a crate", "crates/protocol/src/lib.rs"],
    ["Rust test in a crate", "crates/service-kit/tests/container.rs"],
    ["a crate's non-Rust test data", "crates/contract/tests/fixtures/openapi/m1-error-schemas.json"],
    ["a crate README a test reads", "crates/protocol/README.md"],
    ["a crate's SQL", "crates/service-kit/sql/database.sql"],
    ["sqlx offline data", "crates/outbox/.sqlx/query-0123.json"],
    ["a service", "services/ingest/src/main.rs"],
    ["a service migration", "services/ingest/migrations/clickhouse/0001_tables.sql"],
    ["a gateway crate", "edge/agent/src/main.rs"],
    ["a factory tool", "factory/provision/Cargo.toml"],
    ["a repository tool", "tools/repo-check/src/lib.rs"],
    ["a tool's C source", "tools/release-selftest/src/target.c"],
    ["a tool's build script", "tools/release-selftest/build.rs"],
    ["the OpenAPI spec", "api/openapi.json"],
    ["the workspace manifest", "Cargo.toml"],
    ["a member manifest", "crates/outbox/Cargo.toml"],
    ["the lock file", "Cargo.lock"],
    ["the toolchain pin", "rust-toolchain.toml"],
    ["the proto pins", ".prototools"],
    ["a proto plugin", ".proto/plugins/cargo-deny.toml"],
    ["clippy configuration", "clippy.toml"],
    ["rustfmt configuration", "rustfmt.toml"],
    ["Cargo configuration", ".cargo/config.toml"],
    ["the agent instructions", "CLAUDE.md"],
    ["an AGENTS.md anywhere (repo-check rejects it)", "docs/AGENTS.md"],
    ["an agents.md in any case", "scripts/lib/Agents.MD"],
    ["a Rust file outside the crates (sql-check scans it)", "docs/examples/query.rs"],
    ["a Rust file under scripts", "scripts/ci/fixture.rs"],
    ["SQL outside the crates (a Rust file may include it)", "deploy/sql/seed.sql"],
    ["the chart values a service-kit test reads", "deploy/chart/values.yaml"],
    ["the git ignore rules (the OpenAPI diff uses git status)", ".gitignore"],
    ["ci.yml", ".github/workflows/ci.yml"],
    ["a local action", ".github/actions/setup-proto/action.yml"],
    ["this decision script", "scripts/ci/changes.ts"],
    ["a dataset", "datasets/lab/README.md"],
    ["an eval", "evals/README.md"],
    ["a new top-level directory nobody has classified", "proto/telemetry.proto"],
    ["a new file at the root", "build.zig"],
  ])("rust runs for %s (%s)", (_, path) => {
    expect(affectsRust(path)).toBe(true);
    expect(decide([path]).rust.run).toBe(true);
    expect(decide(["docs/README.md", path, "scripts/dev.ts"]).rust.run).toBe(true);
  });

  test.each([
    "docs/architecture/ci-release.md",
    "deploy/chart/templates/service.yaml",
    "deploy/chart/README.md",
    "deploy/istio/values-k3s.yaml",
    "deploy/images/ingest/image.toml",
    "deploy/README.md",
    "scripts/dev.ts",
    "scripts/nightly/run.ts",
    "scripts/ci/check-workflows.ts",
    "scripts/ci/changes.test.ts",
    "scripts/package.json",
    "scripts/bun.lock",
    "frontend/README.md",
    ".github/workflows/nightly.yml",
    ".github/workflows/release.yml",
    ".github/ISSUE_TEMPLATE/task.yml",
    ".githooks/pre-commit",
    "README.md",
    "renovate.json5",
    ".gitleaksignore",
    ".dockerignore",
    "deny.toml",
  ])("rust may skip %s", (path) => {
    expect(affectsRust(path)).toBe(false);
  });

  test.each([
    "Cargo.toml",
    "Cargo.lock",
    "crates/outbox/Cargo.toml",
    "services/new-service/Cargo.toml",
    "deny.toml",
    "rust-toolchain.toml",
    ".prototools",
    ".proto/plugins/cargo-deny.toml",
    ".cargo/config.toml",
    "crates/protocol/.cargo/config.toml",
    ".github/workflows/ci.yml",
    ".github/actions/setup-proto/action.yml",
    "scripts/ci/changes.ts",
  ])("cargo-deny runs for %s", (path) => {
    expect(affectsCargoDeny(path)).toBe(true);
  });

  test.each(["crates/protocol/src/lib.rs", "api/openapi.json", "docs/README.md", "deploy/chart/values.yaml", "clippy.toml"])(
    "cargo-deny skips %s",
    (path) => {
      expect(affectsCargoDeny(path)).toBe(false);
    },
  );

  test.each([
    "deploy/chart/Chart.yaml",
    "deploy/chart/values.yaml",
    "deploy/chart/templates/networkpolicy.yaml",
    "deploy/chart/tests/chart.test.ts",
    "deploy/chart/tests/e2e/tunnel-recovery.ts",
    "deploy/istio/istiod-values.yaml",
    "scripts/lib/stack.ts",
    "services/ingest/tests/common/mod.rs",
    "services/ingest/migrations/clickhouse",
    "services/ingest/migrations/clickhouse/0006_logs.sql",
    "crates/service-kit/src/telemetry/redact.rs",
    "docs/architecture/deployment.md",
    ".prototools",
    ".proto/plugins/helm.toml",
    ".proto/plugins/kubeconform.toml",
    ".github/workflows/ci.yml",
    ".github/actions/setup-proto/action.yml",
    "scripts/ci/changes.ts",
  ])("helm runs for %s", (path) => {
    expect(affectsHelm(path)).toBe(true);
  });

  test.each([
    "crates/protocol/src/lib.rs",
    "docs/README.md",
    "deploy/images/ingest/image.toml",
    "scripts/dev.ts",
    "Cargo.lock",
    "services/ingest/migrations/clickhouse-old/0001.sql",
    "crates/service-kit/src/telemetry/mod.rs",
  ])(
    "helm skips %s",
    (path) => {
      expect(affectsHelm(path)).toBe(false);
    },
  );

  test.each([
    "deploy/host/src/install.ts",
    "deploy/host/tests/install.test.ts",
    "deploy/host/package.json",
    "deploy/host/bun.lock",
    "deploy/host/build.ts",
    "deploy/host/assets/cloudflare-packaging.asc",
    ".prototools",
    ".github/workflows/ci.yml",
    ".github/actions/setup-proto/action.yml",
    "scripts/ci/changes.ts",
  ])("host runs for %s", (path) => {
    expect(affectsHost(path)).toBe(true);
  });

  test.each(["deploy/chart/values.yaml", "deploy/README.md", "docs/architecture/deployment.md", "scripts/dev.ts", "crates/protocol/src/lib.rs"])(
    "host skips %s",
    (path) => {
      expect(affectsHost(path)).toBe(false);
    },
  );

  test.each([
    "deploy/images/postgres/Containerfile",
    "deploy/images/postgres/image.test.ts",
    "deploy/images/postgres/README.md",
    ".prototools",
    ".github/workflows/ci.yml",
    ".github/actions/setup-proto/action.yml",
    "scripts/ci/changes.ts",
    "scripts/release/lib.ts",
  ])("postgres-image runs for %s", (path) => {
    expect(affectsPostgresImage(path)).toBe(true);
  });

  test.each(["deploy/images/README.md", "deploy/images/rust-binary.Containerfile", "deploy/chart/values.yaml", "scripts/release/build.ts", "docs/architecture/ci-release.md"])(
    "postgres-image skips %s",
    (path) => {
      expect(affectsPostgresImage(path)).toBe(false);
    },
  );

  test("typical pull requests", () => {
    // The chart pins the image's PostgreSQL major and AGE version, so its tests run too.
    expect(running(["deploy/images/postgres/Containerfile", "docs/test-reports/2026-10-11-57-age-postgres-image.md"])).toEqual(["helm", "postgres-image"]);
    expect(running(["deploy/images/postgres/README.md"])).toEqual(["postgres-image"]);
    expect(running(["deploy/host/src/install.ts", "deploy/host/README.md"])).toEqual(["host"]);
    expect(running(["deploy/chart/templates/service.yaml", "deploy/chart/README.md"])).toEqual(["helm"]);
    expect(running(["scripts/dev.ts", "scripts/tests/dev.test.ts"])).toEqual([]);
    expect(running(["crates/outbox/src/lib.rs"])).toEqual(["rust"]);
    expect(running(["crates/outbox/src/lib.rs", "Cargo.lock"])).toEqual(["cargo-deny", "rust"]);
    expect(running(["deploy/chart/values.yaml"])).toEqual(["helm", "rust"]);
    expect(running([".prototools"])).toEqual(["cargo-deny", "helm", "host", "postgres-image", "rust"]);
    expect(running([".github/workflows/nightly.yml", "scripts/nightly/run.ts"])).toEqual([]);
  });

  test("changes to the workflow, its actions, this script, its imports and the tool pins run every job", () => {
    for (const path of [
      ".github/workflows/ci.yml",
      ".github/actions/setup-proto/action.yml",
      "scripts/ci/changes.ts",
      "scripts/release/lib.ts",
      ".prototools",
      ".proto/plugins/bun.toml",
    ]) {
      expect(affectsEveryJob(path)).toBe(true);
      expect(running([path])).toEqual([...PATH_JOBS].toSorted());
    }
  });

  test("an empty change runs nothing and says so", () => {
    const decision = decide([]);
    for (const job of PATH_JOBS) {
      expect(decision[job]).toEqual({ run: false, reason: "no file changed" });
    }
  });

  test("the reason names the first file that runs the job", () => {
    expect(decide(["docs/a.md", "crates/x/src/lib.rs", "Cargo.lock"]).rust.reason).toBe("`crates/x/src/lib.rs` changed");
    expect(decide(["docs/a.md"]).helm.reason).toBe("none of the 1 changed file(s) affects it");
  });
});

// The path rules are only as good as the list of files each job reads. These guards find the
// files the rust and helm jobs' tests read outside their own directories and require the rules
// to cover them, so a new test that reads, say, a doc cannot be skipped silently.
describe("guards: files the jobs read are covered", () => {
  /**
   * The repository files a TypeScript file imports with a relative path, followed transitively.
   * Fails on a specifier it cannot read (`import(name)`, `require(name)`), so the guard never
   * misses a dependency silently.
   */
  function importsOf(entry: string, root = ROOT): string[] {
    const seen = new Set<string>();
    const pending = [entry];
    while (pending.length > 0) {
      const file = pending.pop() as string;
      if (seen.has(file)) continue;
      seen.add(file);
      const source = readFileSync(join(root, file), "utf8");
      // Static forms always name a string literal: `import ... from "./x"`, `export ... from "./x"`
      // and `import "./x"`.
      const specifiers = [...source.matchAll(/(?:\bfrom|\bimport)\s+["']([^"'\n]+)["']/g)].map((match) => match[1] ?? "");
      // Calls: `import(...)`, `require(...)` and `import x = require(...)`. The argument must be
      // exactly one string literal; anything else (a variable, a template, a literal followed by
      // an expression) is reported, because the file it loads cannot be known here.
      const computed: string[] = [];
      for (const call of source.matchAll(/\b(?:import|require)\s*\(/g)) {
        const rest = source.slice((call.index ?? 0) + call[0].length);
        const literal = /^\s*(["'])([^"'\n]*)\1\s*\)/.exec(rest);
        if (literal) {
          specifiers.push(literal[2] ?? "");
        } else {
          computed.push(`${call[0]}${rest.slice(0, 40).split("\n")[0]}`);
        }
      }
      expect(`${file}: ${computed.join(", ")}`).toBe(`${file}: `);
      for (const specifier of specifiers.filter((specifier) => /^\.{1,2}\//.test(specifier))) {
        const imported = posix.normalize(posix.join(posix.dirname(file), specifier));
        expect(existsSync(join(root, imported))).toBe(true);
        pending.push(imported);
      }
    }
    seen.delete(entry);
    return [...seen].toSorted();
  }

  test("every file the decision script imports, transitively, runs every job", () => {
    const imports = importsOf("scripts/ci/changes.ts");
    expect(imports).toContain("scripts/release/lib.ts");
    expect(imports.filter((path) => !affectsEveryJob(path))).toEqual([]);
    expect([...DECISION_CODE].toSorted()).toEqual(["scripts/ci/changes.ts", ...imports].toSorted());
  });

  test("the import guard follows every import form through other files", () => {
    const root = mkdtempSync(join(tmpdir(), "changes-imports-"));
    try {
      write(root, {
        "scripts/ci/entry.ts": 'import { a } from "./a.ts";\nimport "../side/effect.ts";\nexport { a };\n',
        "scripts/ci/a.ts": 'export * from "../lib/b.ts";\nexport const a = 1;\n',
        "scripts/lib/b.ts": 'import type { C } from "./c.ts";\nexport const load = () => import("../late/d.ts");\nexport type B = C;\n',
        "scripts/lib/c.ts": "export type C = number;\n",
        "scripts/late/d.ts": 'import { readFileSync } from "node:fs";\nconst e = require("./e.cjs");\nexport { e, readFileSync };\n',
        "scripts/late/e.cjs": "module.exports = 1;\n",
        "scripts/side/effect.ts": 'import f = require("../lib/f.ts");\nexport { f };\n',
        "scripts/lib/f.ts": "export = 2;\n",
      });
      expect(importsOf("scripts/ci/entry.ts", root)).toEqual([
        "scripts/ci/a.ts",
        "scripts/late/d.ts",
        "scripts/late/e.cjs",
        "scripts/lib/b.ts",
        "scripts/lib/c.ts",
        "scripts/lib/f.ts",
        "scripts/side/effect.ts",
      ]);
      for (const call of [
        "import(name)",
        'import("./a.ts" + suffix)',
        "import(`./${name}`)",
        'require("./a.ts".replace("a", name))',
        "require(name)",
      ]) {
        write(root, { "scripts/ci/computed.ts": `const name = "a";\nconst suffix = "";\nexport const load = () => ${call};\n` });
        expect(() => importsOf("scripts/ci/computed.ts", root)).toThrow();
      }
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  /** Repository files under `dir` with one of the extensions, skipping build output and hidden directories. */
  function sources(dir: string, extensions: readonly string[]): string[] {
    const found: string[] = [];
    const walk = (current: string) => {
      for (const name of readdirSync(current)) {
        const path = join(current, name);
        if (statSync(path).isDirectory()) {
          if (!name.startsWith(".") && name !== "target" && name !== "node_modules") {
            walk(path);
          }
        } else if (extensions.some((extension) => name.endsWith(extension))) {
          found.push(path);
        }
      }
    };
    walk(dir);
    return found;
  }

  /** Names directly in the repository root, the first segment a repository path can start with. */
  const topLevel = new Set(readdirSync(ROOT));

  /**
   * The repository path a string literal names, or undefined when it names none. A literal that
   * starts with `/`, `./` or `../` is taken relative to `base`, the directory of the crate (as in
   * `concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/x")`); any other is taken relative to the
   * repository root (as in `root.join("services")`). The path must start with a name in the root.
   */
  function repositoryPath(literal: string, base = ""): string | undefined {
    if (literal.includes(" ") || literal.includes("{") || literal === "") {
      return undefined;
    }
    const path = /^\.{0,2}\//.test(literal) ? posix.normalize(posix.join(base, `./${literal}`)) : posix.normalize(literal);
    const first = path.split("/")[0] ?? "";
    return topLevel.has(first) ? path : undefined;
  }

  /** The directory of the crate a file belongs to: the nearest one holding a Cargo.toml. */
  function crateOf(file: string): string {
    let dir = dirname(file);
    while (dir.startsWith(ROOT) && dir !== ROOT && !existsSync(join(dir, "Cargo.toml"))) {
      dir = dirname(dir);
    }
    return relative(ROOT, dir);
  }

  test("every repository path a Rust source names runs rust", () => {
    const uncovered: string[] = [];
    let checked = 0;
    for (const file of sources(ROOT, [".rs"])) {
      const source = readFileSync(file, "utf8");
      const where = relative(ROOT, file);
      // include_str!/include_bytes! paths are relative to the including file.
      for (const match of source.matchAll(/include_(?:str|bytes)!\(\s*"([^"]+)"\s*\)/g)) {
        const included = posix.normalize(posix.join(posix.dirname(where), match[1] ?? ""));
        if (existsSync(join(ROOT, included))) {
          checked++;
          if (!affectsRust(included)) uncovered.push(`${where}: include of ${included}`);
        }
      }
      const crate = crateOf(file);
      for (const match of source.matchAll(/"((?:[^"\\\n]|\\.)*)"/g)) {
        const path = repositoryPath(match[1] ?? "", crate);
        if (path !== undefined) {
          checked++;
          if (!affectsRust(path)) uncovered.push(`${where}: "${match[1]}"`);
        }
      }
    }
    expect(checked).toBeGreaterThan(5);
    expect(uncovered).toEqual([]);
  });

  test("the guard resolves the paths Rust tests build", () => {
    expect(repositoryPath("/../../deploy/chart/values.yaml", "crates/service-kit")).toBe("deploy/chart/values.yaml");
    expect(repositoryPath("../../api/openapi.json", "tools/openapi")).toBe("api/openapi.json");
    expect(repositoryPath("/README.md", "crates/protocol")).toBe("crates/protocol/README.md");
    expect(repositoryPath("services")).toBe("services");
    expect(repositoryPath("not a path")).toBeUndefined();
    expect(crateOf(join(ROOT, "crates", "protocol", "tests", "readme.rs"))).toBe("crates/protocol");
    // What the guard would report: a Rust test that starts reading a doc.
    expect(affectsRust(repositoryPath("/../../docs/architecture/deployment.md", "crates/x") ?? "")).toBe(false);
  });

  test("every repository file the chart tests read runs helm", () => {
    const uncovered: string[] = [];
    let checked = 0;
    for (const file of sources(join(ROOT, "deploy", "chart", "tests"), [".ts"])) {
      const source = readFileSync(file, "utf8");
      for (const match of source.matchAll(/["']([^"'\n]+)["']/g)) {
        const path = repositoryPath(match[1] ?? "");
        if (path !== undefined && path.includes("/") && existsSync(join(ROOT, path))) {
          checked++;
          if (!affectsHelm(path)) uncovered.push(`${relative(ROOT, file)}: "${match[1]}"`);
        }
      }
    }
    expect(checked).toBeGreaterThan(3);
    expect(uncovered).toEqual([]);
  });

  test("the Postgres image test reads and imports nothing outside deploy/images/postgres, or the rules cover it", () => {
    const imageDir = join(ROOT, "deploy", "images", "postgres");
    const uncovered: string[] = [];
    let checked = 0;
    for (const file of sources(imageDir, [".ts"])) {
      const where = relative(ROOT, file);
      for (const imported of importsOf(where)) {
        checked++;
        if (!affectsPostgresImage(imported)) uncovered.push(`${where}: import of ${imported}`);
      }
      for (const match of readFileSync(file, "utf8").matchAll(/["']([^"'\n]+)["']/g)) {
        const path = repositoryPath(match[1] ?? "");
        if (path !== undefined && path.includes("/") && existsSync(join(ROOT, path))) {
          checked++;
          if (!affectsPostgresImage(path)) uncovered.push(`${where}: "${match[1]}"`);
        }
      }
    }
    expect(checked).toBeGreaterThan(0);
    expect(uncovered).toEqual([]);
  });

  test("the host installer reads and imports nothing outside deploy/host, or the rules cover it", () => {
    const hostDir = join(ROOT, "deploy", "host");
    const uncovered: string[] = [];
    let checked = 0;
    for (const file of sources(hostDir, [".ts"]).filter((path) => !relative(hostDir, path).startsWith("dist/"))) {
      const source = readFileSync(file, "utf8");
      const where = relative(ROOT, file);
      for (const match of source.matchAll(/from\s+["'](\.{1,2}\/[^"']+)["']/g)) {
        checked++;
        const imported = posix.normalize(posix.join(posix.dirname(where), match[1] ?? ""));
        if (!affectsHost(imported)) uncovered.push(`${where}: import of ${imported}`);
      }
      for (const match of source.matchAll(/["']([^"'\n]+)["']/g)) {
        const path = repositoryPath(match[1] ?? "");
        if (path !== undefined && path.includes("/") && existsSync(join(ROOT, path))) {
          checked++;
          if (!affectsHost(path)) uncovered.push(`${where}: "${match[1]}"`);
        }
      }
    }
    expect(checked).toBeGreaterThan(0);
    expect(uncovered).toEqual([]);
  });
});

// ---------------------------------------------------------------------------------------------
// Which commits are compared, in throwaway git repositories

let scratch: string;
let counter = 0;

beforeAll(() => {
  scratch = mkdtempSync(join(tmpdir(), "changes-test-"));
});

afterAll(() => {
  rmSync(scratch, { recursive: true, force: true });
});

/** Runs git in `root` with a fixed identity and no signing, so the tests run anywhere. */
function git(root: string, ...args: string[]): string {
  const result = Bun.spawnSync(
    ["git", "-c", "user.name=test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main", ...args],
    { cwd: root, stdout: "pipe", stderr: "pipe" },
  );
  if (result.exitCode !== 0) {
    throw new Error(`git ${args.join(" ")}: ${result.stderr.toString()}`);
  }
  return result.stdout.toString().trim();
}

function write(root: string, files: Record<string, string>): void {
  for (const [path, content] of Object.entries(files)) {
    mkdirSync(dirname(join(root, path)), { recursive: true });
    writeFileSync(join(root, path), content);
  }
}

function commit(root: string, files: Record<string, string>, message: string): string {
  write(root, files);
  git(root, "add", "--all");
  git(root, "commit", "--quiet", "--message", message);
  return git(root, "rev-parse", "HEAD");
}

/** A repository with one commit on main holding docs and a crate. */
function repository(): { root: string; base: string } {
  const root = join(scratch, `repo-${counter++}`);
  mkdirSync(root);
  git(root, "init", "--quiet");
  const base = commit(root, { "docs/a.md": "a\n", "crates/x/notes.txt": "x\n", "crates/x/src/lib.rs": "\n" }, "base");
  return { root, base };
}

/**
 * Checks out a pull request the way actions/checkout does: the merge commit of `branch` into
 * main, with main as the first parent. Main may have moved on since the branch started.
 */
function mergeCommit(root: string, branch: string): string {
  git(root, "checkout", "--quiet", "--detach", "main");
  git(root, "merge", "--quiet", "--no-ff", "--no-edit", branch);
  return git(root, "rev-parse", "HEAD");
}

function run(commit: string, overrides: Partial<WorkflowRun> = {}): WorkflowRun {
  return {
    id: 1,
    path: ".github/workflows/ci.yml",
    headSha: commit,
    headBranch: "main",
    event: "push",
    status: "completed",
    conclusion: "success",
    url: "https://github.com/Laysi/trustiot-v2/actions/runs/1",
    ...overrides,
  };
}

/** ListRuns answering with fixed runs. */
const runsOf =
  (...runs: WorkflowRun[]): ListRuns =>
  async () =>
    runs;

const PUSH = { GITHUB_EVENT_NAME: "push" };

function jobsRunning(result: { decision: ReturnType<typeof decide> }): string[] {
  return PATH_JOBS.filter((job) => result.decision[job].run);
}

describe("commits compared", () => {
  test("a pull request compares its merge commit with the tip of main it merged onto", async () => {
    const { root } = repository();
    git(root, "checkout", "--quiet", "-b", "feature");
    commit(root, { "docs/b.md": "b\n" }, "docs");
    // main moves on with a Rust change the pull request does not contain.
    git(root, "checkout", "--quiet", "main");
    commit(root, { "crates/x/src/lib.rs": "// main\n" }, "rust on main");
    const merge = mergeCommit(root, "feature");

    expect(pullRequestRange(root)).toEqual({ base: git(root, "rev-parse", "main"), head: merge });
    const result = await plan(root, { GITHUB_EVENT_NAME: "pull_request" }, runsOf());
    expect(result.files).toEqual(["docs/b.md"]);
    expect(result.base).toStartWith(git(root, "rev-parse", "main"));
    expect(jobsRunning(result)).toEqual([]);
  });

  test("a pull request whose checkout is not a merge commit runs everything", async () => {
    const { root } = repository();
    expect(pullRequestRange(root)).toEqual({ everything: "the checked-out commit is not the pull request's merge commit" });
    expect(jobsRunning(await plan(root, { GITHUB_EVENT_NAME: "pull_request" }, runsOf()))).toEqual([...PATH_JOBS]);
  });

  test("a file moved out of a Rust directory still runs rust", () => {
    const { root, base } = repository();
    git(root, "mv", "crates/x/notes.txt", "docs/notes.txt");
    git(root, "commit", "--quiet", "--message", "move");
    const files = changedFiles(root, base, git(root, "rev-parse", "HEAD"));
    expect(files.toSorted()).toEqual(["crates/x/notes.txt", "docs/notes.txt"]);
    expect(decide(files).rust.run).toBe(true);
  });

  test("unusual file names come back unquoted", () => {
    const { root, base } = repository();
    const head = commit(root, { 'docs/naïve "quoted".md': "x\n" }, "name");
    expect(changedFiles(root, base, head)).toEqual(['docs/naïve "quoted".md']);
  });

  test("main's first-parent history is listed newest first, without the pushed commit", () => {
    const { root, base } = repository();
    const second = commit(root, { "docs/b.md": "b\n" }, "second");
    commit(root, { "docs/c.md": "c\n" }, "third");
    expect(mainAncestors(root)).toEqual([second, base]);
    expect(mainAncestors(root, 1)).toEqual([second]);
  });

  test("a push compares with the previous commit when ci passed there", async () => {
    const { root, base } = repository();
    commit(root, { "docs/b.md": "b\n" }, "docs");
    const result = await plan(root, PUSH, runsOf(run(base)));
    expect(result.everything).toBeUndefined();
    expect(result.base).toBe(`${base} (the newest main commit whose ci passed)`);
    expect(result.files).toEqual(["docs/b.md"]);
    expect(jobsRunning(result)).toEqual([]);
  });

  test.each([
    ["is still running", { status: "in_progress", conclusion: "" }, "in_progress"],
    ["failed", { conclusion: "failure" }, "failure"],
    ["was cancelled", { conclusion: "cancelled" }, "cancelled"],
  ])("a push passes over a previous commit whose ci %s and compares with the green one before it", async (_, state, shown) => {
    const { root, base } = repository();
    const rust = commit(root, { "crates/x/src/lib.rs": "// changed\n" }, "rust");
    commit(root, { "docs/b.md": "b\n" }, "docs");
    const result = await plan(root, PUSH, runsOf(run(base), run(rust, { id: 2, ...state })));
    expect(result.base).toBe(`${base} (the newest main commit whose ci passed; passed over ${rust.slice(0, 12)} (${shown}))`);
    // The Rust change of the commit that is not known to be green is tested again.
    expect(result.files.toSorted()).toEqual(["crates/x/src/lib.rs", "docs/b.md"]);
    expect(jobsRunning(result)).toEqual(["rust"]);
  });

  test("a push passes over commits without a run of their own (several commits pushed at once)", async () => {
    const { root, base } = repository();
    commit(root, { "Cargo.lock": "# lock\n" }, "lock");
    commit(root, { "docs/b.md": "b\n" }, "docs");
    const result = await plan(root, PUSH, runsOf(run(base)));
    expect(result.files.toSorted()).toEqual(["Cargo.lock", "docs/b.md"]);
    expect(jobsRunning(result)).toEqual(["rust", "cargo-deny"]);
  });

  test("only the newest run of a commit counts, and only push runs of ci.yml on main", async () => {
    const { root, base } = repository();
    commit(root, { "docs/b.md": "b\n" }, "docs");
    const failedRerun = await plan(root, PUSH, runsOf(run(base, { id: 1 }), run(base, { id: 2, conclusion: "failure" })));
    expect(failedRerun.everything).toContain("none of the last 1 main commit(s) has a successful ci run");
    for (const other of [{ event: "pull_request" }, { headBranch: "feature" }, { path: ".github/workflows/nightly.yml" }]) {
      const result = await plan(root, PUSH, runsOf(run(base, other)));
      expect(result.everything).toContain(`(${base.slice(0, 12)} (no ci run))`);
    }
  });

  test("a push runs everything when no recent main commit is green", async () => {
    const { root, base } = repository();
    const second = commit(root, { "docs/b.md": "b\n" }, "docs");
    commit(root, { "docs/c.md": "c\n" }, "docs");
    const result = await plan(root, PUSH, runsOf(run(second, { conclusion: "failure" })));
    expect(result.everything).toBe(
      `none of the last 2 main commit(s) has a successful ci run (${second.slice(0, 12)} (failure), ${base.slice(0, 12)} (no ci run))`,
    );
    expect(jobsRunning(result)).toEqual([...PATH_JOBS]);
  });

  test("a push runs everything when the runs cannot be read", async () => {
    const { root } = repository();
    commit(root, { "docs/b.md": "b\n" }, "docs");
    const failing: ListRuns = async () => {
      throw new Error("listing the ci runs on main failed with HTTP 403");
    };
    const result = await plan(root, PUSH, failing);
    expect(result.everything).toBe("the ci runs on main cannot be read: listing the ci runs on main failed with HTTP 403");
    expect(jobsRunning(result)).toEqual([...PATH_JOBS]);
  });

  test("a push of a root commit runs everything", async () => {
    const { root, base } = repository();
    const result = await plan(root, PUSH, runsOf(run(base)));
    expect(result.everything).toBe("the pushed commit has no parent");
  });

  test("another event, or no repository at all, runs everything", async () => {
    const { root } = repository();
    expect((await plan(root, { GITHUB_EVENT_NAME: "workflow_dispatch" }, runsOf())).everything).toContain("`workflow_dispatch` event");
    expect((await plan(root, {}, runsOf())).everything).toContain("`unknown` event");
    const empty = join(scratch, `empty-${counter++}`);
    mkdirSync(empty);
    const result = await plan(empty, PUSH, runsOf());
    expect(result.everything).toContain("the changed files cannot be determined");
    expect(jobsRunning(result)).toEqual([...PATH_JOBS]);
  });

  test("the runs request lists the push runs of ci.yml on main", () => {
    expect(mainRunsUrl("https://api.github.com/", "Laysi/trustiot-v2")).toBe(
      "https://api.github.com/repos/Laysi/trustiot-v2/actions/workflows/ci.yml/runs?branch=main&event=push&per_page=100",
    );
    expect(() => mainRunsUrl("https://api.github.com", "../x")).toThrow("is not an owner/name repository");
  });

  test("newestGreen stops at the first green commit", () => {
    expect(newestGreen(["a", "b", "c"], [run("b"), run("c")])).toEqual({ commit: "b", passedOver: ["a (no ci run)"] });
    expect(newestGreen([], [])).toEqual({ problem: "none of the last 0 main commit(s) has a successful ci run" });
  });
});

describe("outputs", () => {
  test("one line per job for GITHUB_OUTPUT", () => {
    expect(outputLines(decide(["docs/a.md", "deploy/chart/Chart.yaml"]))).toBe("rust=false\ncargo-deny=false\nhelm=true\nhost=false\npostgres-image=false\n");
    expect(outputLines(runEverything("x"))).toBe("rust=true\ncargo-deny=true\nhelm=true\nhost=true\npostgres-image=true\n");
  });

  test("the summary lists each job, the reason and the files", () => {
    const text = summary({ decision: decide(["docs/a|b.md"]), files: ["docs/a|b.md"] });
    expect(text).toContain("| `rust` | skipped | none of the 1 changed file(s) affects it |");
    expect(text).toContain("- `docs/a|b.md`");
    expect(text).toContain("`gitleaks` and `workflow-policy` always run.");
    const many = Array.from({ length: 205 }, (_, index) => `docs/${index}.md`);
    expect(summary({ decision: decide(many), files: many })).toContain("- ... and 5 more");
    expect(summary({ decision: runEverything("why"), files: [], everything: "why" })).not.toContain("Changed files");
  });

  test("main writes the outputs and the summary and exits 0, also when everything runs", async () => {
    const output = join(scratch, "output.txt");
    const stepSummary = join(scratch, "summary.md");
    writeFileSync(output, "");
    writeFileSync(stepSummary, "");
    const status = await main({ GITHUB_EVENT_NAME: "workflow_dispatch", GITHUB_OUTPUT: output, GITHUB_STEP_SUMMARY: stepSummary });
    expect(status).toBe(0);
    expect(readFileSync(output, "utf8")).toBe("rust=true\ncargo-deny=true\nhelm=true\nhost=true\npostgres-image=true\n");
    expect(readFileSync(stepSummary, "utf8")).toContain("| `helm` | yes |");
  });
});
