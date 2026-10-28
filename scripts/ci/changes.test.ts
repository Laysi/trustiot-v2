import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, posix, relative } from "node:path";

import {
  affectsCargoDeny,
  affectsEveryJob,
  DECISION_CODE,
  affectsFrontend,
  affectsHelm,
  affectsHost,
  affectsPostgresImage,
  affectsRust,
  affectsRustCacheKey,
  changedFiles,
  decide,
  explain,
  githubReads,
  ISSUE_EVENT_PAGES,
  main,
  mainAncestors,
  mainRunsUrl,
  newestGreen,
  outputLines,
  PATH_JOBS,
  type PathJob,
  plan,
  pullRequestRange,
  report,
  runEverything,
  summary,
} from "./changes.ts";
import {
  type GitHubReads,
  parseCommitTree,
  parseIssueEventTypes,
  parseJobs,
  parseForcePushTimes,
  parseGitHubCreated,
  parseMergeBase,
  parsePullNumbers,
  parsePullRequests,
  parseRuns,
  type PullRequest,
  type Run,
  pullRequestTriggerProblem,
  type WorkflowJob,
} from "./tested-tree.ts";

const ROOT = join(import.meta.dir, "..", "..");

/** Which of the path-dependent jobs `files` run, as a sorted list. */
function running(files: readonly string[]): string[] {
  const decision = decide(files);
  return PATH_JOBS.filter((job) => decision[job].run).toSorted();
}

describe("path rules", () => {
  test("a docs-only change skips rust, cargo-deny, helm, host, postgres-image and frontend", () => {
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
    ["a crate's C source", "crates/protocol/csrc/crc.c"],
    ["a build script", "crates/protocol/build.rs"],
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
    ["the MQTT broker configuration the migrate and certs tests run", "deploy/chart/files/mosquitto/mosquitto.conf"],
    ["the MQTT broker's gateway listener the migrate and certs tests run", "deploy/chart/files/mosquitto/gateway-listener.conf"],
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
    // The chart tests check the rendered chart with its rendered-manifest rule.
    "scripts/ci/check-secret-handling.ts",
    "scripts/lib/stack.ts",
    "services/ingest/tests/common/mod.rs",
    // The MQTT broker's image pin, probe and memory limit, shared with its tests.
    "crates/protocol/tests/broker.rs",
    "tools/migrate/tests/mqtt_broker.rs",
    "services/ingest/migrations/clickhouse",
    "services/ingest/migrations/clickhouse/0006_logs.sql",
    "crates/service-kit/src/telemetry/redact.rs",
    "docs/architecture/deployment.md",
    "deploy/flux/clusters/saas/trustiot/chart.yaml",
    "deploy/flux/tests/flux.test.ts",
    "scripts/flux/lib.ts",
    "scripts/release/flux.ts",
    ".sops.yaml",
    ".prototools",
    ".proto/plugins/helm.toml",
    ".proto/plugins/flux.toml",
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

  test.each([
    "frontend/src/main.tsx",
    "frontend/src/i18n/locales/zh-TW.json",
    "frontend/package.json",
    "frontend/bun.lock",
    "frontend/vite.config.ts",
    "frontend/README.md",
    "api/openapi.json",
    "crates/contract/src/error.rs",
    "services/identity/src/tenants.rs",
    "services/new-service/src/routes/devices.rs",
    ".prototools",
    ".github/workflows/ci.yml",
    ".github/actions/setup-proto/action.yml",
    "scripts/ci/changes.ts",
  ])("frontend runs for %s", (path) => {
    expect(affectsFrontend(path)).toBe(true);
  });

  test.each([
    "crates/contract/Cargo.toml",
    "crates/contract/tests/fixtures/openapi/m1-error-schemas.json",
    "crates/service-kit/src/lib.rs",
    "services/identity/Cargo.toml",
    "services/identity/tests/sessions.rs",
    "services/ingest/migrations/clickhouse/0006_logs.sql",
    "deploy/chart/values.yaml",
    "docs/architecture/ci-release.md",
    "scripts/dev.ts",
  ])("frontend skips %s", (path) => {
    expect(affectsFrontend(path)).toBe(false);
  });

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
    expect(running([".prototools"])).toEqual(["cargo-deny", "frontend", "helm", "host", "postgres-image", "rust"]);
    expect(running([".github/workflows/nightly.yml", "scripts/nightly/run.ts"])).toEqual([]);
    expect(running(["frontend/src/app/routes.tsx", "frontend/src/app/routes.test.tsx"])).toEqual(["frontend"]);
    // A new message key in a service must come with its translations (frontend/README.md).
    expect(running(["services/identity/src/tenants.rs"])).toEqual(["frontend", "rust"]);
    expect(running(["services/identity/tests/tenants.rs", "services/identity/Cargo.toml"])).toEqual(["cargo-deny", "rust"]);
  });

  test("changes to the workflow, its actions, this script, its imports and the tool pins run every job", () => {
    for (const path of [
      ".github/workflows/ci.yml",
      ".github/actions/setup-proto/action.yml",
      "scripts/ci/changes.ts",
      "scripts/ci/tested-tree.ts",
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
        const path = posix.normalize(posix.join(posix.dirname(file), specifier));
        // Bun also resolves a specifier without its extension (`./cluster` for cluster.ts).
        const imported = [path, `${path}.ts`].find((candidate) => existsSync(join(root, candidate)) && statSync(join(root, candidate)).isFile());
        expect(imported, `${file}: ${specifier}`).toBeDefined();
        pending.push(imported ?? path);
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

  test("every repository file the chart tests read or import runs helm", () => {
    const uncovered: string[] = [];
    let checked = 0;
    for (const file of sources(join(ROOT, "deploy", "chart", "tests"), [".ts"])) {
      // Imports, followed through other files: chart.test.ts imports the secret check's
      // rendered-manifest rule (Laysi/trustiot-v2#398).
      for (const imported of importsOf(relative(ROOT, file))) {
        checked++;
        if (!affectsHelm(imported)) uncovered.push(`${relative(ROOT, file)}: import of ${imported}`);
      }
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

  test("every repository file the deploy/flux tests read or import runs helm", () => {
    const uncovered: string[] = [];
    let checked = 0;
    const testsDir = join(ROOT, "deploy", "flux", "tests");
    const seen = new Set<string>();
    const visit = (where: string) => {
      if (seen.has(where)) return;
      seen.add(where);
      const source = readFileSync(join(ROOT, where), "utf8");
      for (const match of source.matchAll(/from\s+["'](\.{1,2}\/[^"']+)["']/g)) {
        checked++;
        const imported = posix.normalize(posix.join(posix.dirname(where), match[1] ?? ""));
        if (!affectsHelm(imported)) uncovered.push(`${where}: import of ${imported}`);
        visit(imported);
      }
      // Files named in the tests themselves; the imported modules are covered as a whole above.
      if (!where.startsWith("deploy/flux/tests/")) return;
      for (const match of source.matchAll(/["']([^"'\n]+)["']/g)) {
        const path = repositoryPath(match[1] ?? "");
        if (path !== undefined && path.includes("/") && existsSync(join(ROOT, path)) && statSync(join(ROOT, path)).isFile()) {
          checked++;
          if (!affectsHelm(path)) uncovered.push(`${where}: "${match[1]}"`);
        }
      }
    };
    // The unit tests only; the lab e2e runs by hand and is not part of the job.
    for (const file of sources(testsDir, [".ts"]).filter((path) => !path.includes(`${posix.sep}e2e${posix.sep}`))) {
      visit(relative(ROOT, file));
    }
    expect(checked).toBeGreaterThan(5);
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

  /**
   * Directories the frontend tests walk from the repository root, with the files in them they
   * read: frontend/src/i18n/server-keys.test.ts scans the Rust sources for message keys. A walk of
   * any other directory fails the guard below until it is listed here and in the rules.
   */
  const FRONTEND_WALKS: Record<string, (path: string) => boolean> = {
    "crates/contract/src": (path) => path.endsWith(".rs"),
    services: (path) => /^services\/[^/]+\/src\/.+\.rs$/.test(path),
  };

  test("the frontend reads and imports nothing outside frontend/, or the rules cover it", () => {
    const frontendDir = join(ROOT, "frontend");
    const uncovered: string[] = [];
    let checked = 0;
    const walked = new Set<string>();
    for (const file of sources(frontendDir, [".ts", ".tsx"]).filter((path) => !relative(frontendDir, path).startsWith("dist/"))) {
      const source = readFileSync(file, "utf8");
      const where = relative(ROOT, file);
      const dir = posix.dirname(where);
      for (const match of source.matchAll(/from\s+["'](\.{1,2}\/[^"']+)["']/g)) {
        checked++;
        const imported = posix.normalize(posix.join(dir, match[1] ?? ""));
        if (!affectsFrontend(imported)) uncovered.push(`${where}: import of ${imported}`);
      }
      // Literals relative to the file, as in `new URL("../../api/openapi.json", import.meta.url)`,
      // or to the repository root.
      for (const match of source.matchAll(/["']([^"'\n]+)["']/g)) {
        const literal = match[1] ?? "";
        const path = /^\.{1,2}\//.test(literal) ? repositoryPath(posix.join(dir, literal)) : repositoryPath(literal);
        if (path !== undefined && path.includes("/") && existsSync(join(ROOT, path)) && statSync(join(ROOT, path)).isFile()) {
          checked++;
          if (!affectsFrontend(path)) uncovered.push(`${where}: "${literal}"`);
        }
      }
      // Paths built from the repository root, as in `join(ROOT, "crates", "contract", "src")`: the
      // literal segments before the first computed one.
      for (const match of source.matchAll(/join\(\s*ROOT\s*((?:,\s*["'][^"'\n]+["'])+)/g)) {
        const path = Array.from((match[1] ?? "").matchAll(/["']([^"'\n]+)["']/g), (segment) => segment[1]).join("/");
        checked++;
        if (existsSync(join(ROOT, path)) && statSync(join(ROOT, path)).isDirectory()) {
          if (FRONTEND_WALKS[path] === undefined) uncovered.push(`${where}: walks ${path}`);
          walked.add(path);
        } else if (!affectsFrontend(path)) {
          uncovered.push(`${where}: "${path}"`);
        }
      }
    }
    for (const [dir, reads] of Object.entries(FRONTEND_WALKS)) {
      const read = sources(join(ROOT, dir), [""]).map((path) => relative(ROOT, path)).filter(reads);
      expect(`${dir}: ${read.length > 0}`).toBe(`${dir}: true`);
      uncovered.push(...read.filter((path) => !affectsFrontend(path)).map((path) => `${dir}: ${path}`));
    }
    expect([...walked].toSorted()).toEqual(Object.keys(FRONTEND_WALKS).toSorted());
    expect(checked).toBeGreaterThan(3);
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

/** A run as GitHub lists it; main push runs default to midnight, before every pull request run. */
function run(commit: string, overrides: Partial<Run> = {}): Run {
  return {
    id: 1,
    createdAt: "2026-10-15T00:00:00Z",
    runAttempt: 1,
    headRepository: "Laysi/trustiot-v2",
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

/**
 * GitHub reads answering from fixed data: the main runs given, no pull request for any commit,
 * and the overrides. Merge bases and trees come from the test repository's git, as GitHub's would.
 */
function fakeGitHub(root: string, overrides: Partial<GitHubReads> = {}, mainRuns: Run[] = []): GitHubReads & { calls: string[] } {
  const calls: string[] = [];
  const log =
    <A extends unknown[], R>(name: string, fn: (...args: A) => Promise<R>) =>
    (...args: A) => {
      calls.push(`${name}(${args.join(", ")})`);
      return fn(...args);
    };
  const reads: GitHubReads = {
    mainRuns: async () => mainRuns,
    pullsOfCommit: async () => [],
    pullsWithHead: async () => [],
    issueEventTypes: async () => [],
    mergeBase: async (base, head) => git(root, "merge-base", base, head),
    treeOf: async (commit) => git(root, "rev-parse", `${commit}^{tree}`),
    createdByGitHub: async () => null,
    pullRequestRuns: async () => [],
    jobsOf: async () => [],
    forcePushesToMain: async () => [],
    ...overrides,
  };
  return {
    calls,
    mainRuns: log("mainRuns", reads.mainRuns),
    pullsOfCommit: log("pullsOfCommit", reads.pullsOfCommit),
    pullsWithHead: log("pullsWithHead", reads.pullsWithHead),
    issueEventTypes: log("issueEventTypes", reads.issueEventTypes),
    mergeBase: log("mergeBase", reads.mergeBase),
    treeOf: log("treeOf", reads.treeOf),
    pullRequestRuns: log("pullRequestRuns", reads.pullRequestRuns),
    createdByGitHub: log("createdByGitHub", reads.createdByGitHub),
    jobsOf: log("jobsOf", reads.jobsOf),
    forcePushesToMain: log("forcePushesToMain", reads.forcePushesToMain),
  };
}

/** GitHub reads with fixed main runs and no pull requests (the path rules alone decide). */
const runsOf = (...runs: Run[]): GitHubReads => fakeGitHub(scratch, {}, runs);

const REPOSITORY = "Laysi/trustiot-v2";
const PUSH = { GITHUB_EVENT_NAME: "push", GITHUB_REPOSITORY: REPOSITORY };

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
    const failing = fakeGitHub(root, {
      mainRuns: async () => {
        throw new Error("listing the ci runs on main failed with HTTP 403");
      },
    });
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

// Laysi/trustiot-v2#431: a push to main skips a job that ran and passed in the merged pull
// request's runs, when those runs provably tested the pushed tree (scripts/ci/tested-tree.ts).
describe("same tree as the merged pull request", () => {
  const PR_RUN = 100;
  const CI_YML = ".github/workflows/ci.yml";

  /**
   * main: `base` (ci green), then `parent` (a Cargo.lock change whose ci failed). Pull request #7
   * branches from `base`, changes a Rust file and the chart, and (unless `updateBranch` is false)
   * merges main, so its last head `head` contains `parent`. GitHub then squash-merges it into
   * `merged`, whose only parent is `parent`; HEAD is `merged`, as in the push run.
   */
  function squashMerged({ updateBranch = true, prFiles = {} as Record<string, string> } = {}) {
    const { root } = repository();
    const base = commit(root, { [CI_YML]: "on:\n  pull_request:\n  push:\n    branches: [main]\n" }, "ci");
    git(root, "checkout", "--quiet", "-b", "feature");
    commit(root, { "crates/x/src/lib.rs": "// pr\n", "deploy/chart/x.yaml": "x\n" }, "pr");
    git(root, "checkout", "--quiet", "main");
    const parent = commit(root, { "Cargo.lock": "# lock\n" }, "lock on main");
    if (updateBranch) {
      git(root, "checkout", "--quiet", "feature");
      git(root, "merge", "--quiet", "--no-ff", "--no-edit", "main");
      // `prFiles` land after the update, so they may change what main changed.
      if (Object.keys(prFiles).length > 0) {
        commit(root, prFiles, "more of the pull request");
      }
      git(root, "checkout", "--quiet", "main");
    }
    const head = git(root, "rev-parse", "feature");
    git(root, "merge", "--quiet", "--squash", "feature");
    git(root, "commit", "--quiet", "--message", "pr (#7)");
    const merged = git(root, "rev-parse", "HEAD");
    return { root, base, parent, head, merged };
  }

  type Scenario = ReturnType<typeof squashMerged>;

  function pullRequest(scenario: Scenario, overrides: Partial<PullRequest> = {}): PullRequest {
    return {
      number: 7,
      mergedAt: "2026-10-15T00:00:00Z",
      mergeCommitSha: scenario.merged,
      headSha: scenario.head,
      headRef: "feature",
      headRepository: REPOSITORY,
      baseRef: "main",
      baseRepository: REPOSITORY,
      ...overrides,
    };
  }

  /** A pull_request run of the head, made at 03:00, after main held `parent` (01:00). */
  function prRun(scenario: Scenario, overrides: Partial<Run> = {}): Run {
    return run(scenario.head, {
      id: PR_RUN,
      createdAt: "2026-10-15T03:00:00Z",
      event: "pull_request",
      headBranch: "feature",
      url: `https://github.com/${REPOSITORY}/actions/runs/${PR_RUN}`,
      ...overrides,
    });
  }

  /** The jobs of a ci run: the path-dependent ones as given (default success), the others passed. */
  function jobs(conclusions: Partial<Record<PathJob, string>> = {}, runAttempt = 1): WorkflowJob[] {
    return [
      { name: "changes", status: "completed", conclusion: "success", runAttempt },
      ...PATH_JOBS.map((job) => ({ name: job, status: "completed", conclusion: conclusions[job] ?? "success", runAttempt })),
      { name: "gitleaks", status: "completed", conclusion: "success", runAttempt },
      { name: "workflow-policy", status: "completed", conclusion: "success", runAttempt },
    ];
  }

  /** What the pull request's own run did: its path rules ran rust and helm only. */
  const PR_JOBS = jobs({ "cargo-deny": "skipped", host: "skipped", "postgres-image": "skipped", frontend: "skipped" });

  /** When GitHub created (squash-merged) the parent: just before main's push run of it at 01:00. */
  const PARENT_CREATED = "2026-10-15T00:55:00Z";
  /** An hour before PARENT_CREATED. */
  const SINCE = "2026-10-14T23:55:00.000Z";

  /** GitHub as it answers for a merged pull request whose run tested the pushed tree. */
  function github(scenario: Scenario, overrides: Partial<GitHubReads> = {}) {
    return fakeGitHub(
      scenario.root,
      {
        pullsOfCommit: async (commit) => (commit === scenario.merged ? [pullRequest(scenario)] : []),
        pullsWithHead: async (owner, ref) => (owner === "Laysi" && ref === "feature" ? [7] : []),
        pullRequestRuns: async (head) => (head === scenario.head ? [prRun(scenario)] : []),
        createdByGitHub: async (commit) => (commit === scenario.parent ? PARENT_CREATED : null),
        jobsOf: async (id) => (id === PR_RUN ? PR_JOBS : []),
        ...overrides,
      },
      mainHistory(scenario),
    );
  }

  /** main's push runs: base (22:00 the day before, green), parent (01:00, red), the pushed commit (04:00, running). */
  function mainHistory(scenario: Scenario, parentConclusion = "failure"): Run[] {
    return [
      run(scenario.base, { id: 1, createdAt: "2026-10-14T22:00:00Z" }),
      run(scenario.parent, { id: 2, createdAt: "2026-10-15T01:00:00Z", conclusion: parentConclusion }),
      run(scenario.merged, { id: 3, createdAt: "2026-10-15T04:00:00Z", status: "in_progress", conclusion: "" }),
    ];
  }

  /** Asserts that the tested-tree check did not apply, for `reason`, and the path rules decided alone. */
  function pathRulesOnly(result: Awaited<ReturnType<typeof plan>>, reason: string): void {
    expect(result.tested).toEqual({ applies: false, reason: expect.stringContaining(reason) });
    // base -> merged changes Cargo.lock (from the red parent), a Rust file and the chart.
    expect(jobsRunning(result)).toEqual(["rust", "cargo-deny", "helm"]);
    expect(summary(result)).toContain(`Not used: ${result.tested?.applies === false ? result.tested.reason : ""}.`);
  }

  test("a job that ran and passed on the same tree is skipped; a job the pull request skipped keeps the path rules", async () => {
    const scenario = squashMerged();
    const gh = github(scenario);
    const result = await plan(scenario.root, PUSH, gh);

    const tree = git(scenario.root, "rev-parse", `${scenario.merged}^{tree}`);
    expect(tree).toBe(git(scenario.root, "rev-parse", `${scenario.head}^{tree}`));
    expect(result.tested).toEqual({
      applies: true,
      pullRequest: 7,
      head: scenario.head,
      parent: scenario.parent,
      tree,
      runs: [{ id: PR_RUN, url: `https://github.com/${REPOSITORY}/actions/runs/${PR_RUN}` }],
      forwardSince: SINCE,
      passed: ["rust", "helm"],
      notPassed: {
        "cargo-deny": `skipped in run ${PR_RUN}`,
        host: `skipped in run ${PR_RUN}`,
        "postgres-image": `skipped in run ${PR_RUN}`,
        frontend: `skipped in run ${PR_RUN}`,
      },
    });
    expect(result.decision.rust).toEqual({ run: false, reason: `passed in run ${PR_RUN} of pull request #7 on the same tree` });
    expect(result.decision.helm.run).toBe(false);
    // The pull request's path rules skipped cargo-deny (it changed no Cargo file), but main's red
    // parent changed Cargo.lock: on main it is compared with the newest green commit and runs, so
    // a red main stays red.
    expect(result.decision["cargo-deny"]).toEqual({ run: true, reason: "`Cargo.lock` changed" });
    expect(jobsRunning(result)).toEqual(["cargo-deny"]);
    expect(result.base).toStartWith(scenario.base);

    const text = summary(result);
    expect(text).toContain("| `rust` | skipped | passed in run 100 of pull request #7 on the same tree |");
    expect(text).toContain("### Same tree as the merged pull request");
    expect(text).toContain(`The pushed tree \`${tree}\` is the tree of \`${scenario.head}\`, the last head of pull request #7`);
    expect(text).toContain(`[${PR_RUN}](https://github.com/${REPOSITORY}/actions/runs/${PR_RUN})`);
    expect(text).toContain("| `cargo-deny` | skipped in run 100: path rules decide |");
    expect(text).toContain("| `helm` | ran and passed: skipped here |");
    // Only metadata is read: the pull request, its events, the merge base, the head's tree, the
    // runs of the head and their jobs.
    expect(gh.calls).toEqual([
      "mainRuns()",
      `pullsOfCommit(${scenario.merged})`,
      "pullsWithHead(Laysi, feature)",
      "issueEventTypes(7)",
      `mergeBase(${scenario.parent}, ${scenario.head})`,
      `treeOf(${scenario.head})`,
      `pullRequestRuns(${scenario.head})`,
      `createdByGitHub(${scenario.parent})`,
      "forcePushesToMain()",
      `jobsOf(${PR_RUN})`,
    ]);
  });

  // Only main saves the Rust build cache, and only when its key changed (Swatinem/rust-cache).
  test.each([
    ["a crate's Cargo.toml", "crates/x/Cargo.toml", "[package]\n"],
    ["the root Cargo.toml", "Cargo.toml", "[workspace]\n"],
    ["Cargo.lock", "Cargo.lock", "# lock changed by the pull request\n"],
    ["rust-toolchain.toml", "rust-toolchain.toml", "[toolchain]\n"],
    ["rust-toolchain", "rust-toolchain", "stable\n"],
    ["the Cargo configuration", ".cargo/config.toml", "[build]\n"],
    ["ci.yml (the job's id and environment)", CI_YML, "on:\n  pull_request:\n  push:\n    branches: [main]\nenv:\n  RUSTFLAGS: -Dwarnings\n"],
    ["a local action", ".github/actions/setup-proto/action.yml", "name: setup\n"],
  ])("rust runs on main to save the cache when the merge changes %s", async (_, path, content) => {
    const scenario = squashMerged({ prFiles: { [path]: content } });
    const result = await plan(scenario.root, PUSH, github(scenario));
    expect(result.tested).toMatchObject({ applies: true, passed: ["rust", "helm"] });
    expect(result.decision.rust).toEqual({
      run: true,
      reason: `passed in run ${PR_RUN} of pull request #7 on the same tree, but \`${path}\` changes the Rust build cache key, so main runs it to save the cache`,
    });
    // Every other job that passed is still skipped.
    expect(result.decision.helm).toEqual({ run: false, reason: `passed in run ${PR_RUN} of pull request #7 on the same tree` });
    expect(summary(result)).toContain("| `rust` | ran and passed, but runs here (see why above) |");
  });

  test.each([
    ["a nested Cargo configuration", "crates/x/.cargo/config.toml"],
    ["a nested toolchain file", "crates/x/rust-toolchain.toml"],
    ["another workflow", ".github/workflows/nightly.yml"],
  ])("%s does not change the cache key: rust is still skipped", async (_, path) => {
    const scenario = squashMerged({ prFiles: { [path]: "x\n" } });
    const result = await plan(scenario.root, PUSH, github(scenario));
    expect(result.decision.rust).toEqual({ run: false, reason: `passed in run ${PR_RUN} of pull request #7 on the same tree` });
  });

  test("only what the merge itself changes counts, not what main changed before it", async () => {
    // The red parent changed Cargo.lock; the merge (parent -> merged) does not.
    const scenario = squashMerged();
    expect(changedFiles(scenario.root, scenario.parent, scenario.merged)).not.toContain("Cargo.lock");
    expect((await plan(scenario.root, PUSH, github(scenario))).decision.rust.run).toBe(false);
  });

  test("the Rust cache key inputs", () => {
    for (const path of ["Cargo.lock", "Cargo.toml", "services/a/Cargo.toml", "crates/b/Cargo.lock", "rust-toolchain", "rust-toolchain.toml", ".cargo/config.toml", ".cargo/x", CI_YML, ".github/actions/setup-proto/action.yml"]) {
      expect(`${path}: ${affectsRustCacheKey(path)}`).toBe(`${path}: true`);
    }
    for (const path of ["crates/x/src/lib.rs", "crates/x/.cargo/config.toml", "crates/x/rust-toolchain.toml", ".github/workflows/nightly.yml", "docs/Cargo.toml.md", "clippy.toml"]) {
      expect(`${path}: ${affectsRustCacheKey(path)}`).toBe(`${path}: false`);
    }
  });

  test("with no green main commit, the jobs that passed on the same tree are still skipped and the rest run", async () => {
    const scenario = squashMerged();
    const mainRuns = [
      run(scenario.base, { id: 1, createdAt: "2026-10-14T22:00:00Z", conclusion: "failure" }),
      run(scenario.parent, { id: 2, createdAt: "2026-10-15T01:00:00Z", conclusion: "failure" }),
    ];
    const result = await plan(scenario.root, PUSH, github(scenario, { mainRuns: async () => mainRuns }));
    expect(result.everything).toContain("has a successful ci run");
    expect(jobsRunning(result)).toEqual(["cargo-deny", "host", "postgres-image", "frontend"]);
  });

  test("the trees differ", async () => {
    const scenario = squashMerged();
    // The merge commit holds one more file than the pull request's head.
    write(scenario.root, { "docs/extra.md": "x\n" });
    git(scenario.root, "add", "--all");
    git(scenario.root, "commit", "--quiet", "--amend", "--no-edit");
    const merged = git(scenario.root, "rev-parse", "HEAD");
    const result = await plan(scenario.root, PUSH, github({ ...scenario, merged }));
    pathRulesOnly(result, "differs from the tree");
  });

  test.each([
    ["has no run at all", [] as Partial<Run>[], "no pull_request run of ci.yml has the head"],
    ["failed", [{ conclusion: "failure" }], `run ${PR_RUN} of the head`],
    ["waits for approval", [{ conclusion: "action_required" }], "is action_required, not success"],
    ["was cancelled", [{ conclusion: "cancelled" }], "is cancelled, not success"],
    ["is still running", [{ status: "in_progress", conclusion: "" }], "is in_progress, not success"],
    ["passed once but another run of it failed", [{}, { id: PR_RUN + 1, conclusion: "failure" }], `run ${PR_RUN + 1} of the head`],
  ])("the pull request's last head %s", async (_, overrides, reason) => {
    const scenario = squashMerged();
    const runs = overrides.map((override) => prRun(scenario, override));
    pathRulesOnly(await plan(scenario.root, PUSH, github(scenario, { pullRequestRuns: async () => runs })), reason);
  });

  test("only pull_request runs of ci.yml for that head count", async () => {
    const scenario = squashMerged();
    const others = [
      prRun(scenario, { path: ".github/workflows/nightly.yml" }),
      prRun(scenario, { event: "workflow_dispatch" }),
      prRun(scenario, { headSha: scenario.parent }),
    ];
    pathRulesOnly(await plan(scenario.root, PUSH, github(scenario, { pullRequestRuns: async () => others })), "no pull_request run of ci.yml");
  });

  test("an older head of the pull request does not count", async () => {
    const scenario = squashMerged();
    // GitHub answers runs only for the head asked for; the head asked for is the last one.
    const gh = github(scenario, { pullRequestRuns: async (head) => (head === scenario.base ? [prRun(scenario)] : []) });
    pathRulesOnly(await plan(scenario.root, PUSH, gh), "no pull_request run of ci.yml has the head");
    expect(gh.calls).toContain(`pullRequestRuns(${scenario.head})`);
  });

  test("the base the pull request's run tested cannot be confirmed: the head lacks main's tip", async () => {
    const scenario = squashMerged({ updateBranch: false });
    pathRulesOnly(await plan(scenario.root, PUSH, github(scenario)), "does not contain the pushed commit's parent");
  });

  test.each(["base_ref_changed", "automatic_base_change_succeeded", "automatic_base_change_failed"])(
    "the base the pull request's run tested cannot be confirmed: its base branch changed (%s)",
    async (event) => {
      const scenario = squashMerged();
      const gh = github(scenario, { issueEventTypes: async () => ["labeled", event, "merged", "closed"] });
      pathRulesOnly(await plan(scenario.root, PUSH, gh), `was changed (${event})`);
    },
  );

  test("several commits pushed at once: the parent never had a push run of its own", async () => {
    const scenario = squashMerged();
    const gh = github(scenario, { mainRuns: async () => [run(scenario.base)] });
    const result = await plan(scenario.root, PUSH, gh);
    expect(result.tested).toEqual({ applies: false, reason: expect.stringContaining("has no push run of ci.yml on main") });
    expect(jobsRunning(result)).toEqual(["rust", "cargo-deny", "helm"]);
    expect(gh.calls).toEqual(["mainRuns()"]);
  });

  test.each([
    ["has no pull_request trigger", "on:\n  push:\n    branches: [main]\n"],
    ["filters its pull_request trigger", "on:\n  pull_request:\n    types: [opened]\n  push:\n"],
    ["filters its pull_request trigger", "on:\n  pull_request:\n    paths: [crates/**]\n"],
    ["cannot be parsed", "on: [\n"],
  ])("the pushed ci.yml %s", async (reason, workflow) => {
    const scenario = squashMerged();
    // The pull request's last head and the merge both carry this ci.yml, so the trees stay equal.
    git(scenario.root, "checkout", "--quiet", "feature");
    const head = commit(scenario.root, { [CI_YML]: workflow }, "trigger");
    git(scenario.root, "checkout", "--quiet", "main");
    git(scenario.root, "merge", "--quiet", "--squash", "feature");
    git(scenario.root, "commit", "--quiet", "--amend", "--no-edit");
    const merged = git(scenario.root, "rev-parse", "HEAD");
    const result = await plan(scenario.root, PUSH, github({ ...scenario, head, merged }));
    expect(result.tested).toEqual({ applies: false, reason: expect.stringContaining(`the pushed ${CI_YML} ${reason}`) });
  });

  test("an unfiltered pull_request trigger, in any YAML spelling, counts", () => {
    expect(pullRequestTriggerProblem("on:\n  pull_request:\n  push:\n")).toBeUndefined();
    expect(pullRequestTriggerProblem("on:\n  pull_request: {}\n")).toBeUndefined();
    expect(pullRequestTriggerProblem("on: [pull_request, push]\n")).toBeUndefined();
    expect(pullRequestTriggerProblem("on: pull_request\n")).toBeUndefined();
    expect(pullRequestTriggerProblem("on: [push]\n")).toBe("has no pull_request trigger");
    expect(pullRequestTriggerProblem("on: pull_request_target\n")).toBe("has no pull_request trigger");
    expect(pullRequestTriggerProblem("name: x\n")).toBe("has no pull_request trigger");
    expect(pullRequestTriggerProblem("on:\n  pull_request:\n    branches: [main]\n")).toBe("filters its pull_request trigger");
  });

  test("a merge commit (two parents) is not compared", async () => {
    const scenario = squashMerged();
    git(scenario.root, "reset", "--quiet", "--hard", scenario.parent);
    git(scenario.root, "merge", "--quiet", "--no-ff", "--no-edit", "feature");
    const merged = git(scenario.root, "rev-parse", "HEAD");
    pathRulesOnly(await plan(scenario.root, PUSH, github({ ...scenario, merged })), "the pushed commit has 2 parents");
  });

  test.each([
    ["no pull request", () => [] as PullRequest[], "0 merged pull requests"],
    ["a pull request that is not merged", (s: Scenario) => [pullRequest(s, { mergedAt: null })], "0 merged pull requests"],
    ["a pull request merged as another commit", (s: Scenario) => [pullRequest(s, { mergeCommitSha: s.parent })], "0 merged pull requests"],
    ["two merged pull requests", (s: Scenario) => [pullRequest(s), pullRequest(s, { number: 8 })], "2 merged pull requests"],
    ["a pull request into another branch", (s: Scenario) => [pullRequest(s, { baseRef: "integration" })], "not Laysi/trustiot-v2:main"],
    ["a pull request into another repository", (s: Scenario) => [pullRequest(s, { baseRepository: "someone/fork" })], "not Laysi/trustiot-v2:main"],
    ["a pull request without a head", (s: Scenario) => [pullRequest(s, { headSha: "" })], "has no valid head commit"],
  ])("GitHub lists %s for the pushed commit", async (_, pulls, reason) => {
    const scenario = squashMerged();
    pathRulesOnly(await plan(scenario.root, PUSH, github(scenario, { pullsOfCommit: async () => pulls(scenario) })), reason);
  });

  test.each([
    ["the pull request", "pullsOfCommit"],
    ["the pull requests with its head branch", "pullsWithHead"],
    ["the pull request's events", "issueEventTypes"],
    ["the merge base", "mergeBase"],
    ["the head's tree", "treeOf"],
    ["the head's runs", "pullRequestRuns"],
    ["who created the parent", "createdByGitHub"],
    ["main's force pushes", "forcePushesToMain"],
    ["the jobs of a run", "jobsOf"],
  ] as const)("reading %s fails", async (_, method) => {
    const scenario = squashMerged();
    const failing = async () => {
      throw new Error(`GET ${method} failed with HTTP 502`);
    };
    const result = await plan(scenario.root, PUSH, github(scenario, { [method]: failing }));
    pathRulesOnly(result, `the pull request's runs cannot be checked: GET ${method} failed with HTTP 502`);
  });

  test.each([
    ["skipped", "skipped in run 100"],
    ["failure", "failure in run 100"],
    ["cancelled", "cancelled in run 100"],
  ])("a job that concluded %s in the pull request's run keeps the path rules", async (conclusion, reason) => {
    const scenario = squashMerged();
    const result = await plan(scenario.root, PUSH, github(scenario, { jobsOf: async () => jobs({ rust: conclusion }) }));
    expect(result.tested?.applies).toBe(true);
    expect(result.tested?.applies === true && result.tested.notPassed.rust).toBe(reason);
    // Compared with the newest green commit, `base`: the red parent's Cargo.lock comes first.
    expect(result.decision.rust).toEqual({ run: true, reason: "`Cargo.lock` changed" });
  });

  test("a job must have passed in every run of the head, exactly once each", async () => {
    const scenario = squashMerged();
    const second = PR_RUN + 1;
    const runs = [prRun(scenario), prRun(scenario, { id: second })];
    const jobsOf = async (id: number) =>
      id === PR_RUN
        ? jobs()
        : [...jobs({ helm: "skipped" }).filter((job) => job.name !== "host"), { name: "postgres-image", status: "completed", conclusion: "success", runAttempt: 1 }];
    const result = await plan(scenario.root, PUSH, github(scenario, { pullRequestRuns: async () => runs, jobsOf }));
    expect(result.tested).toMatchObject({
      applies: true,
      runs: [{ id: PR_RUN }, { id: second }],
      // cargo-deny passed too, but its result changes with time: the path rules decide it on main.
      passed: ["rust", "cargo-deny", "frontend"],
      notPassed: {
        helm: `skipped in run ${second}`,
        host: `no job of that name in attempt 1 of run ${second}`,
        "postgres-image": `2 jobs of that name in attempt 1 of run ${second}`,
      },
    });
    expect(result.decision.rust.reason).toBe(`passed in run ${PR_RUN}, ${second} of pull request #7 on the same tree`);
  });

  // F1 of the review (2026-10-15): cargo-deny reads the RustSec database when it runs.
  test.each([
    ["the merge's Cargo files are unchanged since the newest green commit", "success", false, "none of the 2 changed file(s) affects it"],
    ["main's red parent changed Cargo.lock", "failure", true, "`Cargo.lock` changed"],
  ])("cargo-deny never takes the same-tree skip; the path rules decide it: %s", async (_, parentConclusion, runs, pathReason) => {
    const scenario = squashMerged();
    const gh = github(scenario, { mainRuns: async () => mainHistory(scenario, parentConclusion), jobsOf: async () => jobs() });
    const result = await plan(scenario.root, PUSH, gh);
    expect(result.tested).toMatchObject({ applies: true, passed: [...PATH_JOBS] });
    expect(result.decision["cargo-deny"]).toEqual({
      run: runs,
      reason: `${pathReason} (it passed in run ${PR_RUN} of pull request #7 on the same tree, but its result changes with time, so the path rules decide)`,
    });
    expect(result.decision.rust.run).toBe(false);
    expect(summary(result)).toContain("| `cargo-deny` | ran and passed, but its result changes with time: path rules decide |");
  });

  // F3 of the review: a force push to main can leave the pull request's run tested against a
  // commit that is not in the pushed commit's history.
  test("main held a commit outside the pushed commit's history after GitHub created the parent", async () => {
    const scenario = squashMerged();
    // B was main's tip for a while and was then force-pushed away.
    git(scenario.root, "checkout", "--quiet", "-b", "gone", scenario.parent);
    const gone = commit(scenario.root, { "crates/x/src/fix.rs": "// fix\n" }, "B");
    git(scenario.root, "checkout", "--quiet", "main");
    const history = [...mainHistory(scenario), run(gone, { id: 4, createdAt: "2026-10-15T02:30:00Z" })];
    const result = await plan(scenario.root, PUSH, github(scenario, { mainRuns: async () => history }));
    pathRulesOnly(result, `main held ${gone.slice(0, 12)} (push run 4), which is not in the pushed commit's history, after ${SINCE}`);
    // A commit git does not have counts as outside the history too.
    const unknown = [...mainHistory(scenario), run("e".repeat(40), { id: 5, createdAt: "2026-10-15T02:30:00Z" })];
    pathRulesOnly(await plan(scenario.root, PUSH, github(scenario, { mainRuns: async () => unknown })), "main held eeeeeeeeeeee");
  });

  test("a commit outside the history counts only from an hour before GitHub created the parent on", async () => {
    const scenario = squashMerged();
    git(scenario.root, "checkout", "--quiet", "-b", "old", scenario.base);
    const old = commit(scenario.root, { "docs/old.md": "x\n" }, "old");
    git(scenario.root, "checkout", "--quiet", "main");
    // 23:54 is before 23:55, an hour before GitHub created the parent at 00:55; later main commits
    // on top of the pushed one (a later --explain) are in its history.
    git(scenario.root, "checkout", "--quiet", "-b", "later", scenario.merged);
    const later = commit(scenario.root, { "docs/later.md": "x\n" }, "later");
    git(scenario.root, "checkout", "--quiet", "--detach", scenario.merged);
    const history = [
      ...mainHistory(scenario),
      run(old, { id: 4, createdAt: "2026-10-14T23:54:00Z" }),
      run(later, { id: 5, createdAt: "2026-10-15T05:00:00Z" }),
    ];
    const result = await plan(scenario.root, PUSH, github(scenario, { mainRuns: async () => history }));
    expect(result.tested).toMatchObject({ applies: true, forwardSince: SINCE });
  });

  test("the listed push runs on main must reach back past an hour before GitHub created the parent", async () => {
    const scenario = squashMerged();
    const recent = mainHistory(scenario).map((listed) => ({ ...listed, createdAt: "2026-10-15T00:00:00Z" }));
    pathRulesOnly(
      await plan(scenario.root, PUSH, github(scenario, { mainRuns: async () => recent })),
      `the listed push runs of ci.yml on main do not reach back to ${SINCE}`,
    );
    // The time comes from the parent, not from the runs: a parent created long ago needs history
    // from back then.
    const result = await plan(scenario.root, PUSH, github(scenario, { createdByGitHub: async () => "2026-10-14T20:00:00Z" }));
    pathRulesOnly(result, "do not reach back to 2026-10-14T19:00:00.000Z");
  });

  // The Codex gate's finding (2026-10-15): GitHub may create a run long after it built the merge
  // commit, so no time is taken from the runs.
  test("a force push between the parent's creation and a much later pull request run still counts", async () => {
    const scenario = squashMerged();
    const late = [prRun(scenario, { createdAt: "2026-10-15T09:00:00Z" })];
    const result = await plan(scenario.root, PUSH, github(scenario, { pullRequestRuns: async () => late, forcePushesToMain: async () => ["2026-10-15T02:10:00Z"] }));
    pathRulesOnly(result, `main was force-pushed at 2026-10-15T02:10:00Z, after ${SINCE}`);
  });

  test("a parent that GitHub did not create and sign gives no time to start from", async () => {
    const scenario = squashMerged();
    const result = await plan(scenario.root, PUSH, github(scenario, { createdByGitHub: async () => null }));
    pathRulesOnly(result, `its parent ${scenario.parent.slice(0, 12)} is not a commit GitHub created and signed`);
  });

  test.each([
    ["after GitHub created the parent", "2026-10-15T03:30:00Z", false],
    ["in the hour before it", "2026-10-15T00:10:00Z", false],
    ["more than an hour before it", "2026-10-14T23:54:59Z", true],
  ])("a force push to main %s", async (_, at, applies) => {
    const scenario = squashMerged();
    const result = await plan(scenario.root, PUSH, github(scenario, { forcePushesToMain: async () => [at, "2026-10-01T00:00:00Z"] }));
    if (applies) {
      expect(result.tested?.applies).toBe(true);
    } else {
      pathRulesOnly(result, `main was force-pushed at ${at}, after ${SINCE}`);
    }
  });

  test("the review's force-push scenario: main A -> B, force-pushed back to A, then the squash on A", async () => {
    // Ported from the independent review: the pull request's only run tested merge(H, B).
    const scenario = squashMerged();
    git(scenario.root, "checkout", "--quiet", "-b", "b", scenario.parent);
    const b = commit(scenario.root, { "crates/x/src/fix.rs": "// fix\n" }, "B");
    git(scenario.root, "checkout", "--quiet", "--detach", scenario.merged);
    const history = [...mainHistory(scenario), run(b, { id: 4, createdAt: "2026-10-15T02:50:00Z" })];
    // Either piece of evidence stops it: B's push run, or the recorded force push.
    pathRulesOnly(await plan(scenario.root, PUSH, github(scenario, { mainRuns: async () => history })), `main held ${b.slice(0, 12)}`);
    pathRulesOnly(
      await plan(scenario.root, PUSH, github(scenario, { forcePushesToMain: async () => ["2026-10-15T03:40:00Z"] })),
      "main was force-pushed at 2026-10-15T03:40:00Z",
    );
  });

  // F4 of the review: re-running failed jobs until they pass.
  test.each([
    ["failed in attempt 1 and passed when re-run", [...jobs({ rust: "failure" }, 1), ...jobs({}, 2)], 2, "failure in attempt 1 of run 100"],
    ["was cancelled in attempt 1", [...jobs({ rust: "cancelled" }, 1), ...jobs({}, 2)], 2, "cancelled in attempt 1 of run 100"],
    ["is missing from the latest attempt", [...jobs({}, 1), ...jobs({}, 2).filter((job) => job.name !== "rust")], 2, "no job of that name in attempt 2 of run 100"],
  ])("a job that %s does not count as passed", async (_, runJobs, runAttempt, reason) => {
    const scenario = squashMerged();
    const gh = github(scenario, { pullRequestRuns: async () => [prRun(scenario, { runAttempt })], jobsOf: async () => runJobs });
    const result = await plan(scenario.root, PUSH, gh);
    expect(result.tested?.applies === true && result.tested.notPassed.rust).toBe(reason);
    expect(result.decision.rust.run).toBe(true);
    // helm passed in every attempt.
    expect(result.decision.helm.run).toBe(false);
  });

  // F5 of the review: runs of the head from another pull request.
  test.each([
    ["another head branch", { headBranch: "other" }],
    ["another head repository", { headRepository: "someone/fork" }],
    ["no head repository", { headRepository: null }],
  ])("a run of the head on %s does not count", async (_, overrides) => {
    const scenario = squashMerged();
    const result = await plan(scenario.root, PUSH, github(scenario, { pullRequestRuns: async () => [prRun(scenario, overrides)] }));
    pathRulesOnly(result, `no pull_request run of ci.yml has the head ${scenario.head.slice(0, 12)} on Laysi/trustiot-v2:feature`);
  });

  test.each([
    ["no pull request (the branch was not found)", [] as number[], "are none, not exactly #7"],
    ["another pull request too", [7, 9], "are #7, #9, not exactly #7"],
    ["only another pull request", [9], "are #9, not exactly #7"],
  ])("the head branch has %s", async (_, numbers, reason) => {
    const scenario = squashMerged();
    pathRulesOnly(await plan(scenario.root, PUSH, github(scenario, { pullsWithHead: async () => numbers })), reason);
  });

  test("a pull request whose head repository is gone is not compared", async () => {
    const scenario = squashMerged();
    const pulls = async () => [pullRequest(scenario, { headRepository: null })];
    pathRulesOnly(await plan(scenario.root, PUSH, github(scenario, { pullsOfCommit: pulls })), "the head repository or branch of pull request #7 is gone");
  });

  test("the review's scenario: the pull request has no run of its own and another pull request's run has the head", async () => {
    // Ported from the independent review: a pull request opened with GITHUB_TOKEN starts no run,
    // and another pull request from the same branch into another base ran the head.
    const scenario = squashMerged();
    const result = await plan(scenario.root, PUSH, github(scenario, { pullsWithHead: async () => [7, 12] }));
    pathRulesOnly(result, "are #7, #12, not exactly #7");
  });

  test("--explain decides for a commit that is not checked out, from the same reads", async () => {
    const scenario = squashMerged();
    git(scenario.root, "checkout", "--quiet", "--detach", scenario.base);
    const result = await explain(scenario.root, scenario.merged.slice(0, 12), {}, github(scenario));
    expect(result.tested?.applies).toBe(true);
    expect(jobsRunning(result)).toEqual(["cargo-deny"]);
    const unknown = await explain(scenario.root, "0".repeat(40), {}, github(scenario));
    expect(unknown.everything).toContain("the changed files cannot be determined");
    expect(jobsRunning(unknown)).toEqual([...PATH_JOBS]);
  });

  test("--explain prints a plain line where Actions gets a notice annotation", () => {
    const result = { decision: runEverything("why"), files: [], everything: "why" };
    const lines = (actions: boolean) => {
      const printed: string[] = [];
      report(result, actions, (line) => printed.push(line));
      return printed;
    };
    expect(lines(true)[0]).toBe("::notice title=changes::Path rules not compared, they run every job: why");
    expect(lines(false)[0]).toBe("Path rules not compared, they run every job: why");
    expect(lines(false).some((line) => line.includes("::"))).toBe(false);
  });

  test("--explain takes exactly one commit SHA", async () => {
    for (const args of [["--explain"], ["--explain", "--output=x"], ["--explain", "HEAD"], ["--other", "abc1234"]]) {
      expect(await main({}, args)).toBe(2);
    }
  });
});

// The decision reads only metadata: never an artifact, a log, a job summary, a check run's text
// or an output of a pull request run, which that run's own (unreviewed) code produced.
describe("what the decision reads from GitHub", () => {
  const responses: Record<string, unknown> = {};
  const requested: { url: string; authorization: string | null }[] = [];
  const realFetch = globalThis.fetch;

  function stubFetch(answer: (url: URL) => { status?: number; body: unknown }): void {
    globalThis.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
      const url = new URL(typeof input === "string" ? input : input instanceof URL ? input.href : input.url);
      requested.push({ url: url.href, authorization: new Headers(init?.headers).get("authorization") });
      const { status = 200, body } = answer(url);
      return new Response(JSON.stringify(body), { status });
    }) as unknown as typeof fetch;
  }

  afterAll(() => {
    globalThis.fetch = realFetch;
  });

  const env = { GITHUB_TOKEN: crypto.randomUUID(), GITHUB_REPOSITORY: REPOSITORY, GITHUB_API_URL: "https://api.example.test/" };
  const A = "a".repeat(40);
  const B = "b".repeat(40);
  const T = "c".repeat(40);

  test("every request is a read of a metadata endpoint, with the token", async () => {
    requested.length = 0;
    responses.events = Array.from({ length: 100 }, () => ({ event: "labeled" }));
    stubFetch((url) => {
      const path = url.pathname.replace("/repos/Laysi/trustiot-v2/", "");
      if (path === `commits/${A}/pulls`) {
        return { body: [{ number: 7, merged_at: "2026-10-15T00:00:00Z", merge_commit_sha: A, head: { sha: B, ref: "x", repo: { full_name: REPOSITORY } }, base: { ref: "main", repo: { full_name: REPOSITORY } } }] };
      }
      if (path === "pulls") return { body: [{ number: 7 }] };
      if (path === "activity") return { body: [{ activity_type: "force_push", timestamp: "2026-10-01T00:00:00Z" }] };
      if (path === "issues/7/events") {
        return { body: url.searchParams.get("page") === "1" ? responses.events : [{ event: "base_ref_changed" }] };
      }
      if (path === `compare/${A}...${B}`) return { body: { merge_base_commit: { sha: A } } };
      if (path === `git/commits/${B}`) {
        return { body: { sha: B, tree: { sha: T }, committer: { email: "noreply@github.com", date: "2026-10-14T20:00:00Z" }, verification: { verified: true } } };
      }
      if (path === "actions/workflows/ci.yml/runs") {
        return {
          body: {
            total_count: 1,
            workflow_runs: [
              {
                id: 5,
                path: ".github/workflows/ci.yml",
                head_sha: B,
                head_branch: "x",
                head_repository: { full_name: REPOSITORY },
                event: "pull_request",
                status: "completed",
                conclusion: "success",
                created_at: "2026-10-15T03:00:00Z",
                run_attempt: 2,
              },
            ],
          },
        };
      }
      if (path === "actions/runs/5/jobs") {
        return {
          body: {
            total_count: 2,
            jobs: [
              { name: "rust", status: "completed", conclusion: "failure", run_attempt: 1 },
              { name: "rust", status: "completed", conclusion: "success", run_attempt: 2 },
            ],
          },
        };
      }
      return { status: 404, body: { message: "Not Found" } };
    });
    const gh = githubReads(env);
    expect(await gh.pullsOfCommit(A)).toEqual([
      { number: 7, mergedAt: "2026-10-15T00:00:00Z", mergeCommitSha: A, headSha: B, headRef: "x", headRepository: REPOSITORY, baseRef: "main", baseRepository: REPOSITORY },
    ]);
    expect(await gh.pullsWithHead("Laysi", "x")).toEqual([7]);
    expect(await gh.issueEventTypes(7)).toEqual([...Array.from({ length: 100 }, () => "labeled"), "base_ref_changed"]);
    expect(await gh.mergeBase(A, B)).toBe(A);
    expect(await gh.treeOf(B)).toBe(T);
    expect(await gh.createdByGitHub(B)).toBe("2026-10-14T20:00:00Z");
    expect(await gh.pullRequestRuns(B)).toMatchObject([{ id: 5, createdAt: "2026-10-15T03:00:00Z", runAttempt: 2, headRepository: REPOSITORY, headBranch: "x" }]);
    // Every attempt, so a job re-run until it passed is seen.
    expect(await gh.jobsOf(5)).toEqual([
      { name: "rust", status: "completed", conclusion: "failure", runAttempt: 1 },
      { name: "rust", status: "completed", conclusion: "success", runAttempt: 2 },
    ]);
    expect(await gh.forcePushesToMain()).toEqual(["2026-10-01T00:00:00Z"]);
    expect(requested.map(({ url }) => url)).toEqual([
      `https://api.example.test/repos/Laysi/trustiot-v2/commits/${A}/pulls?per_page=100`,
      "https://api.example.test/repos/Laysi/trustiot-v2/pulls?state=all&head=Laysi%3Ax&per_page=100",
      "https://api.example.test/repos/Laysi/trustiot-v2/issues/7/events?per_page=100&page=1",
      "https://api.example.test/repos/Laysi/trustiot-v2/issues/7/events?per_page=100&page=2",
      `https://api.example.test/repos/Laysi/trustiot-v2/compare/${A}...${B}?per_page=1`,
      `https://api.example.test/repos/Laysi/trustiot-v2/git/commits/${B}`,
      `https://api.example.test/repos/Laysi/trustiot-v2/git/commits/${B}`,
      `https://api.example.test/repos/Laysi/trustiot-v2/actions/workflows/ci.yml/runs?event=pull_request&head_sha=${B}&per_page=100`,
      "https://api.example.test/repos/Laysi/trustiot-v2/actions/runs/5/jobs?filter=all&per_page=100",
      "https://api.example.test/repos/Laysi/trustiot-v2/activity?ref=refs%2Fheads%2Fmain&activity_type=force_push&direction=desc&per_page=100",
    ]);
    expect(requested.every(({ authorization }) => authorization === `Bearer ${env.GITHUB_TOKEN}`)).toBe(true);
    await expect(gh.jobsOf(6)).rejects.toThrow("GET actions/runs/6/jobs failed with HTTP 404");
  });

  test("an answer that does not fit, or does not parse, is an error, never a partial answer", async () => {
    stubFetch((url) => {
      if (url.pathname.endsWith("/events")) return { body: Array.from({ length: 100 }, () => ({ event: "labeled" })) };
      if (url.pathname.endsWith("/runs")) return { body: { total_count: 101, workflow_runs: [] } };
      if (url.pathname.endsWith("/trustiot-v2/pulls")) return { body: Array.from({ length: 100 }, (_, number) => ({ number })) };
      if (url.pathname.endsWith("/activity")) return { body: [{ activity_type: "push", timestamp: "2026-10-01T00:00:00Z" }] };
      return { body: {} };
    });
    const gh = githubReads(env);
    await expect(gh.issueEventTypes(7)).rejects.toThrow(`has more than ${ISSUE_EVENT_PAGES * 100} events`);
    await expect(gh.pullRequestRuns(B)).rejects.toThrow("do not fit in one page");
    await expect(gh.pullsOfCommit(A)).rejects.toThrow("did not answer with a list of pull requests");
    await expect(gh.pullsWithHead("Laysi", "x")).rejects.toThrow("do not fit in one page");
    await expect(gh.forcePushesToMain()).rejects.toThrow("is not a force push with a timestamp");
    await expect(gh.treeOf(B)).rejects.toThrow("came back without its tree");
    await expect(gh.createdByGitHub(B)).rejects.toThrow("came back without its committer or verification");
    await expect(gh.mergeBase(A, B)).rejects.toThrow("has no merge base commit");
    await expect(gh.treeOf("HEAD")).rejects.toThrow("is not a full commit SHA");
    await expect(githubReads({ ...env, GITHUB_TOKEN: "" }).treeOf(B)).rejects.toThrow("GITHUB_TOKEN is not set");
  });

  test("the parsers reject answers without the fields the decision needs", () => {
    expect(() => parsePullRequests([{ number: 1 }])).toThrow("lacks number, head, base or base.repo");
    // A deleted head repository comes back as null.
    expect(parsePullRequests([{ number: 1, merged_at: null, head: { repo: null }, base: { repo: {} } }])).toEqual([
      { number: 1, mergedAt: null, mergeCommitSha: "", headSha: "", headRef: "", headRepository: null, baseRef: "", baseRepository: "" },
    ]);
    expect(() => parsePullNumbers([{}])).toThrow("lacks its number");
    expect(() => parseJobs({ total_count: 2, jobs: [{ name: "rust", run_attempt: 1 }] })).toThrow("more than one page");
    expect(() => parseJobs({ jobs: [{ name: "rust" }] })).toThrow("lacks a name or run_attempt");
    expect(parseJobs({ jobs: [{ name: "rust", status: "queued", conclusion: null, run_attempt: 1 }] })).toEqual([
      { name: "rust", status: "queued", conclusion: "", runAttempt: 1 },
    ]);
    const listed = { id: 1, path: ".github/workflows/ci.yml", head_sha: A, event: "push" };
    expect(() => parseRuns({ total_count: 1, workflow_runs: [listed] }, false)).toThrow("lacks created_at or run_attempt");
    expect(parseRuns({ workflow_runs: [{ ...listed, created_at: "2026-10-15T00:00:00Z", run_attempt: 1, head_repository: null }] }, false)).toMatchObject([
      { id: 1, createdAt: "2026-10-15T00:00:00Z", runAttempt: 1, headRepository: null },
    ]);
    expect(() => parseRuns({ workflow_runs: [] }, true)).toThrow("do not fit in one page");
    expect(() => parseForcePushTimes([{ activity_type: "force_push" }])).toThrow("not a force push with a timestamp");
    expect(() => parseIssueEventTypes([{}])).toThrow("lacks its type");
    expect(() => parseMergeBase({ merge_base_commit: { sha: "abc" } })).toThrow("has no merge base commit");
    expect(() => parseCommitTree({ sha: A, tree: { sha: T } }, B)).toThrow("came back without its tree");
    // Only a commit GitHub committed and signed gives a time.
    const byGitHub = { sha: A, committer: { email: "noreply@github.com", date: "2026-10-15T00:55:00Z" }, verification: { verified: true } };
    expect(parseGitHubCreated(byGitHub, A)).toBe("2026-10-15T00:55:00Z");
    expect(parseGitHubCreated({ ...byGitHub, verification: { verified: false } }, A)).toBeNull();
    expect(parseGitHubCreated({ ...byGitHub, committer: { email: "someone@example.test", date: "2026-10-15T00:55:00Z" } }, A)).toBeNull();
    expect(() => parseGitHubCreated({ ...byGitHub, committer: { email: "noreply@github.com" } }, A)).toThrow("without its committer date");
    expect(() => parseGitHubCreated(byGitHub, B)).toThrow("without its committer or verification");
  });

  test("the decision code calls the API only through githubReads, at the endpoints above", () => {
    const sources = [...DECISION_CODE].map((path) => ({ path, source: readFileSync(join(ROOT, path), "utf8") }));
    const fetching = sources.filter(({ source }) => /\bfetch\(/.test(source)).map(({ path }) => path);
    expect(fetching).toEqual(["scripts/ci/changes.ts"]);
    const changes = sources.find(({ path }) => path === "scripts/ci/changes.ts")?.source ?? "";
    expect(changes.match(/\bfetch\(/g)?.length).toBe(1);
    const paths = [...changes.matchAll(/\bget\(\s*[`"]([^`"]+)[`"]/g)].map((match) => match[1]).toSorted();
    expect(paths).toEqual([
      "actions/runs/${runId}/jobs",
      "actions/workflows/${workflow}/runs",
      "activity",
      "commits/${sha(commit)}/pulls",
      "compare/${sha(base)}...${sha(head)}",
      "git/commits/${sha(commit)}",
      "issues/${number}/events",
      "pulls",
    ]);
    // Outside comments, which explain what is not read.
    for (const { path, source } of sources) {
      const code = source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/.*$/gm, "");
      expect(`${path}: ${code.match(/artifacts|\/logs\b|check-runs|check_runs|check-suites|annotations/g) ?? []}`).toBe(`${path}: `);
    }
  });
});

describe("outputs", () => {
  test("one line per job for GITHUB_OUTPUT", () => {
    expect(outputLines(decide(["docs/a.md", "deploy/chart/Chart.yaml"]))).toBe("rust=false\ncargo-deny=false\nhelm=true\nhost=false\npostgres-image=false\nfrontend=false\n");
    expect(outputLines(runEverything("x"))).toBe("rust=true\ncargo-deny=true\nhelm=true\nhost=true\npostgres-image=true\nfrontend=true\n");
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
    expect(readFileSync(output, "utf8")).toBe("rust=true\ncargo-deny=true\nhelm=true\nhost=true\npostgres-image=true\nfrontend=true\n");
    expect(readFileSync(stepSummary, "utf8")).toContain("| `helm` | yes |");
  });
});
