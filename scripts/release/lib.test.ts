import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import {
  CI_WORKFLOW_PATH,
  GITHUB_OIDC_ISSUER,
  REGISTRY,
  RUST_CONTAINERFILE,
  type ReleasePlan,
  type WorkflowRun,
  aarch64ElfProblem,
  baseImageProblems,
  ciRunsUrl,
  compareReleaseTags,
  discoverImages,
  greenCiProblem,
  helmPushDigest,
  imageNameProblem,
  imagePlatformProblems,
  isMap,
  keylessVerifyArgs,
  parseImageToml,
  parsePlan,
  parsePublished,
  parseReleaseTag,
  parseWorkflowRuns,
  planImage,
  previousReleaseTag,
  renderReleaseBody,
  renderRustContainerfile,
  requireLoopbackRegistry,
  requirePublishedAsPlanned,
  requireRegistry,
  resolveBinary,
  rustSpec,
  sbomAssetName,
  signingIdentity,
  summarizeSpdx,
} from "./lib.ts";

const ROOT = resolve(import.meta.dir, "..", "..");
const DIGEST_A = `sha256:${"a".repeat(64)}`;
const DIGEST_B = `sha256:${"b".repeat(64)}`;

function samplePlan(overrides: Partial<ReleasePlan> = {}): ReleasePlan {
  return {
    tag: "v1.2.0-rc.1",
    version: "1.2.0-rc.1",
    prerelease: true,
    commit: "0123456789abcdef0123456789abcdef01234567",
    sourceDateEpoch: 1_791_630_056,
    repository: "Laysi/trustiot-v2",
    images: [
      planImage({ kind: "rust", name: "identity", package: "identity" }, "1.2.0-rc.1"),
      planImage({ kind: "containerfile", name: "postgres-age", context: "deploy/images/postgres-age" }, "1.2.0-rc.1"),
    ],
    chart: { name: "trustiot", repository: `${REGISTRY}/charts/trustiot`, version: "1.2.0-rc.1" },
    ...overrides,
  };
}

function tempDir(): string {
  return mkdtempSync(join(tmpdir(), "release-lib-test-"));
}

describe("parseReleaseTag", () => {
  test("accepts final releases and release candidates", () => {
    expect(parseReleaseTag("v1.2.3")).toEqual({ tag: "v1.2.3", version: "1.2.3", prerelease: false });
    expect(parseReleaseTag("v0.0.0-rc.1")).toEqual({ tag: "v0.0.0-rc.1", version: "0.0.0-rc.1", prerelease: true });
    expect(parseReleaseTag("v10.20.30-rc.40").prerelease).toBe(true);
  });

  test.each([
    "1.2.3",
    "v1.2",
    "v1.2.3.4",
    "v01.2.3",
    "v1.02.3",
    "v1.2.3-rc.0",
    "v1.2.3-rc",
    "v1.2.3-rc.01",
    "v1.2.3-beta.1",
    "v1.2.3+build.1",
    "v1.2.3-rc.1\n",
    "v1.2.3;id",
    "",
  ])("rejects %p", (tag) => {
    expect(() => parseReleaseTag(tag)).toThrow("is not a release tag");
  });
});

describe("release order", () => {
  test("follows semantic version precedence, numerically", () => {
    const sorted = ["v1.10.0", "v1.2.0", "v1.2.0-rc.10", "v1.2.0-rc.2", "v0.9.9", "v2.0.0-rc.1"].sort(compareReleaseTags);
    expect(sorted).toEqual(["v0.9.9", "v1.2.0-rc.2", "v1.2.0-rc.10", "v1.2.0", "v1.10.0", "v2.0.0-rc.1"]);
    expect(compareReleaseTags("v1.0.0", "v1.0.0")).toBe(0);
  });

  const tags = ["v0.9.0", "v1.0.0-rc.1", "v1.0.0-rc.2", "v1.0.0", "v1.1.0-rc.1", "latest", "v1.0.0-beta.1", "v1.1.0"];

  test("a final release starts from the previous final release", () => {
    expect(previousReleaseTag(tags, parseReleaseTag("v1.1.0"))).toBe("v1.0.0");
    expect(previousReleaseTag(tags, parseReleaseTag("v1.0.0"))).toBe("v0.9.0");
  });

  test("a release candidate starts from the previous tag of either kind", () => {
    expect(previousReleaseTag(tags, parseReleaseTag("v1.0.0-rc.2"))).toBe("v1.0.0-rc.1");
    expect(previousReleaseTag(tags, parseReleaseTag("v1.0.0-rc.1"))).toBe("v0.9.0");
    expect(previousReleaseTag(tags, parseReleaseTag("v1.1.0-rc.1"))).toBe("v1.0.0");
    expect(previousReleaseTag(tags, parseReleaseTag("v1.2.0-rc.1"))).toBe("v1.1.0");
  });

  test("the first release has none", () => {
    expect(previousReleaseTag(tags, parseReleaseTag("v0.9.0"))).toBeUndefined();
    expect(previousReleaseTag(["v0.0.0-rc.1"], parseReleaseTag("v0.0.0-rc.1"))).toBeUndefined();
    expect(previousReleaseTag([], parseReleaseTag("v1.0.0"))).toBeUndefined();
  });
});

describe("image names", () => {
  test("accept the chart's service key style", () => {
    for (const name of ["identity", "field-context", "telemetry-query", "postgres-age", "a1"]) {
      expect(imageNameProblem(name)).toBeUndefined();
    }
  });

  test.each(["Identity", "field_context", "-x", "x-", "a--b", "a.b", ""])("reject %p", (name) => {
    expect(imageNameProblem(name)).toContain("lowercase");
  });

  test("reject names that are too long or reserved for the chart", () => {
    expect(imageNameProblem("a".repeat(64))).toContain("at most 63");
    expect(imageNameProblem("charts")).toContain("reserved");
  });
});

describe("parseImageToml", () => {
  test("reads the package and the optional binary", () => {
    expect(parseImageToml("identity", '[rust]\npackage = "identity"\n', "f")).toEqual({
      kind: "rust",
      name: "identity",
      package: "identity",
    });
    expect(parseImageToml("migrate", '[rust]\npackage = "migrate"\nbin = "trustiot-migrate"\n', "f")).toEqual({
      kind: "rust",
      name: "migrate",
      package: "migrate",
      bin: "trustiot-migrate",
    });
  });

  test("reads the optional static project", () => {
    expect(parseImageToml("web", '[rust]\npackage = "web"\n\n[static]\nproject = "frontend"\n', "f")).toEqual({
      kind: "rust",
      name: "web",
      package: "web",
      staticSite: "frontend",
    });
    expect(parseImageToml("x", '[rust]\npackage = "x"\n[static]\nproject = "apps/site-2"\n', "f").staticSite).toBe("apps/site-2");
  });

  test.each([
    ["not TOML", "[rust", "cannot parse TOML"],
    ["no [rust] table", 'package = "x"\n', "unknown key(s) package"],
    ["an empty file", "", "missing the [rust] table"],
    ["a misspelt key", '[rust]\npackage = "x"\nbinary = "x"\n', "unknown key(s) binary"],
    ["a package name with a space", '[rust]\npackage = "a b"\n', "package must be"],
    ["a numeric bin", '[rust]\npackage = "x"\nbin = 1\n', "bin must be"],
    ["a static key that is not a table", 'static = "frontend"\n\n[rust]\npackage = "x"\n', "[static] must be a table"],
    ["a static table without project", '[rust]\npackage = "x"\n[static]\n', "[static] project must be"],
    ["a misspelt static key", '[rust]\npackage = "x"\n[static]\nproject = "a"\ndist = "out"\n', "unknown key(s) dist in [static]"],
    ["a static project above the root", '[rust]\npackage = "x"\n[static]\nproject = "../frontend"\n', "[static] project must be"],
    ["an absolute static project", '[rust]\npackage = "x"\n[static]\nproject = "/srv/frontend"\n', "[static] project must be"],
    ["a static project with a dot segment", '[rust]\npackage = "x"\n[static]\nproject = "a/./b"\n', "[static] project must be"],
    ["a static project with a trailing slash", '[rust]\npackage = "x"\n[static]\nproject = "frontend/"\n', "[static] project must be"],
    ["a static table on its own", '[static]\nproject = "frontend"\n', "missing the [rust] table"],
  ])("rejects %s", (_label, source, message) => {
    expect(() => parseImageToml("x", source, "deploy/images/x/image.toml")).toThrow(message);
  });
});

describe("discoverImages", () => {
  function layout(entries: Record<string, string>): string {
    const root = tempDir();
    mkdirSync(join(root, "deploy/images"), { recursive: true });
    for (const [path, content] of Object.entries(entries)) {
      mkdirSync(join(root, path, ".."), { recursive: true });
      writeFileSync(join(root, path), content);
    }
    return root;
  }

  test("lists Rust and Containerfile images by directory name and ignores loose files", () => {
    const root = layout({
      "deploy/images/rust-binary.Containerfile": "FROM scratch\n",
      "deploy/images/README.md": "notes\n",
      "deploy/images/postgres-age/Containerfile": "FROM scratch\n",
      "deploy/images/identity/image.toml": '[rust]\npackage = "identity"\n',
    });
    try {
      expect(discoverImages(root)).toEqual([
        { kind: "rust", name: "identity", package: "identity" },
        { kind: "containerfile", name: "postgres-age", context: "deploy/images/postgres-age" },
      ]);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  const STATIC_TOML = '[rust]\npackage = "web"\n[static]\nproject = "site"\n';
  const SITE_PACKAGE = JSON.stringify({ name: "site", scripts: { build: "vite build", "check:dist": "bun check.ts" } });

  test("lists a Rust image with a static project that the build can install and run", () => {
    const root = layout({
      "deploy/images/web/image.toml": STATIC_TOML,
      "site/package.json": SITE_PACKAGE,
      "site/bun.lock": "{}\n",
    });
    try {
      expect(discoverImages(root)).toEqual([{ kind: "rust", name: "web", package: "web", staticSite: "site" }]);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test.each([
    ["no project directory", {}, "deploy/images/web/image.toml: the [static] project site has no package.json"],
    ["no lockfile", { "site/package.json": SITE_PACKAGE }, "has no bun.lock"],
    ["a package.json that is not JSON", { "site/package.json": "{", "site/bun.lock": "{}\n" }, "is not JSON"],
    [
      "no check:dist script",
      { "site/package.json": JSON.stringify({ scripts: { build: "vite build" } }), "site/bun.lock": "{}\n" },
      "has no `check:dist` script",
    ],
    ["no scripts", { "site/package.json": "{}", "site/bun.lock": "{}\n" }, "has no `build` or `check:dist` script"],
  ])("rejects a static project with %s", (_label, entries, message) => {
    const root = layout({ "deploy/images/web/image.toml": STATIC_TOML, ...(entries as Record<string, string>) });
    try {
      expect(() => discoverImages(root)).toThrow(message);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test.each([
    ["both files", { "deploy/images/x/image.toml": '[rust]\npackage = "x"\n', "deploy/images/x/Containerfile": "FROM scratch\n" }, "has both"],
    ["neither file", { "deploy/images/x/notes.md": "" }, "add image.toml"],
    ["an invalid directory name", { "deploy/images/Bad_Name/Containerfile": "FROM scratch\n" }, "directory name is the image name"],
    ["the reserved name", { "deploy/images/charts/Containerfile": "FROM scratch\n" }, "reserved"],
    ["an unpinned base image", { "deploy/images/x/Containerfile": "FROM postgres:17\n" }, "x/Containerfile: `FROM postgres:17` is not pinned"],
  ])("rejects %s", (_label, entries, message) => {
    const root = layout(entries as Record<string, string>);
    try {
      expect(() => discoverImages(root)).toThrow(message);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("the repository's own images are valid", () => {
    const images = discoverImages(ROOT);
    expect(images).toContainEqual({ kind: "containerfile", name: "postgres", context: "deploy/images/postgres" });
    expect(images).toContainEqual({ kind: "rust", name: "migrate", package: "migrate" });
    // Laysi/trustiot-v2#308: the identity service ships as the chart's `identity` service.
    expect(images).toContainEqual({ kind: "rust", name: "identity", package: "identity" });
  });
});

describe("resolveBinary", () => {
  const packages = [
    { name: "identity", targets: [{ name: "identity", kind: ["bin"] }] },
    { name: "tools", targets: [{ name: "a", kind: ["bin"] }, { name: "b", kind: ["bin"] }, { name: "tools", kind: ["lib"] }] },
    { name: "contract", targets: [{ name: "contract", kind: ["lib"] }] },
  ];

  test("uses the only binary or the named one", () => {
    expect(resolveBinary({ kind: "rust", name: "identity", package: "identity" }, packages)).toBe("identity");
    expect(resolveBinary({ kind: "rust", name: "b", package: "tools", bin: "b" }, packages)).toBe("b");
  });

  test("rejects a missing package, a missing binary and an ambiguous package", () => {
    expect(() => resolveBinary({ kind: "rust", name: "x", package: "nope" }, packages)).toThrow("not a workspace member");
    expect(() => resolveBinary({ kind: "rust", name: "x", package: "tools", bin: "c" }, packages)).toThrow("has no binary `c`");
    expect(() => resolveBinary({ kind: "rust", name: "x", package: "tools" }, packages)).toThrow("has 2 binaries");
    expect(() => resolveBinary({ kind: "rust", name: "x", package: "contract" }, packages)).toThrow("has 0 binaries");
  });
});

describe("renderRustContainerfile", () => {
  test("writes the binary name everywhere and keeps the base image pinned by digest", () => {
    const template = readFileSync(join(ROOT, RUST_CONTAINERFILE), "utf8");
    const rendered = renderRustContainerfile(template, "identity");
    expect(rendered).not.toContain("@BIN@");
    expect(rendered).toContain("COPY --chmod=0555 identity /usr/local/bin/identity");
    expect(rendered).toContain('ENTRYPOINT ["/usr/local/bin/identity"]');
    expect(rendered).toMatch(/^USER [0-9]+:[0-9]+$/m);
    const from = rendered.split("\n").filter((line) => line.startsWith("FROM "));
    expect(from).toHaveLength(1);
    expect(from[0]).toMatch(/@sha256:[0-9a-f]{64}$/);
  });

  test("copies a static site to /srv/www only for an image with a [static] project", () => {
    const template = readFileSync(join(ROOT, RUST_CONTAINERFILE), "utf8");
    const plain = renderRustContainerfile(template, "identity");
    const site = renderRustContainerfile(template, "web", { staticSite: true });
    for (const rendered of [plain, site]) {
      expect(rendered).not.toContain("@STATIC@");
    }
    // Only the placeholder goes: every instruction of the other images stays as it was.
    expect(plain.split("\n").filter((line) => !line.startsWith("#"))).toEqual(
      template.split("\n").filter((line) => !line.startsWith("#")).map((line) => line.replaceAll("@BIN@", "identity")),
    );
    const lines = site.split("\n");
    const copy = lines.indexOf("COPY www/ /srv/www/");
    expect(copy).toBeGreaterThan(lines.indexOf("COPY --chmod=0555 web /usr/local/bin/web"));
    expect(copy).toBeLessThan(lines.findIndex((line) => line.startsWith("USER ")));
    expect(() => renderRustContainerfile("FROM scratch\nCOPY @BIN@ /\n", "x", { staticSite: true })).toThrow("no `# @STATIC@` line");
    expect(renderRustContainerfile("FROM scratch\nCOPY @BIN@ /\n", "x")).toBe("FROM scratch\nCOPY x /\n");
  });

  test("rejects a template without the placeholder and a binary name that is not a Cargo name", () => {
    expect(() => renderRustContainerfile("FROM scratch\n", "x")).toThrow("no @BIN@");
    expect(() => renderRustContainerfile("COPY @BIN@ /\n", "x y")).toThrow("not a Cargo binary name");
    expect(() => renderRustContainerfile("COPY @BIN@ /\n", "../x")).toThrow("not a Cargo binary name");
  });

  test("rejects a template whose base image is not pinned", () => {
    expect(() => renderRustContainerfile("FROM gcr.io/distroless/static-debian13:nonroot\nCOPY @BIN@ /\n", "x")).toThrow(
      "is not pinned to a digest",
    );
  });
});

describe("baseImageProblems", () => {
  const pinned = `docker.io/library/postgres:17@sha256:${"c".repeat(64)}`;

  test("accepts digests, scratch and earlier stages", () => {
    expect(baseImageProblems(`FROM ${pinned}\n`)).toEqual([]);
    expect(baseImageProblems("# comment\nFROM scratch\nCOPY x /\n")).toEqual([]);
    expect(
      baseImageProblems(`FROM --platform=$BUILDPLATFORM ${pinned} AS build\nRUN make\nfrom \\\n  ${pinned}\nCOPY --from=build /out /\nFROM build\n`),
    ).toEqual([]);
  });

  test("rejects tags, variables, self references and files without FROM", () => {
    expect(baseImageProblems("FROM postgres:17\n")).toEqual([
      "`FROM postgres:17` is not pinned to a digest; use <image>:<tag>@sha256:<digest>",
    ]);
    expect(baseImageProblems("ARG BASE\nFROM ${BASE}\n")[0]).toContain("uses a variable");
    expect(baseImageProblems("FROM base AS base\n")[0]).toContain("`FROM base` is not pinned");
    expect(baseImageProblems("FROM postgres@sha256:short\n")[0]).toContain("is not pinned");
    expect(baseImageProblems("COPY x /\n")).toEqual(["there is no FROM line"]);
    expect(baseImageProblems("FROM --platform=linux/arm64\n")).toEqual(["a FROM line names no image"]);
  });
});

describe("parsePlan", () => {
  test("round-trips a plan", () => {
    const plan = samplePlan();
    expect(parsePlan(JSON.stringify(plan))).toEqual(plan);
  });

  test("round-trips a Rust image with a static project and rejects a tampered one", () => {
    const web = planImage({ kind: "rust", name: "web", package: "web", staticSite: "frontend" }, "1.2.0-rc.1");
    expect(web.staticSite).toBe("frontend");
    const plan = samplePlan({ images: [web] });
    expect(parsePlan(JSON.stringify(plan))).toEqual(plan);
    expect(rustSpec(parsePlan(JSON.stringify(plan)).images[0]!)).toEqual({ kind: "rust", name: "web", package: "web", staticSite: "frontend" });
    for (const staticSite of ["../frontend", "/srv/www", "", 1]) {
      const doc = JSON.parse(JSON.stringify(plan));
      doc.images[0].staticSite = staticSite;
      expect(() => parsePlan(JSON.stringify(doc)), String(staticSite)).toThrow("invalid static project");
    }
  });

  test.each([
    ["a version that does not match the tag", { version: "1.2.1-rc.1" }, "does not match its tag"],
    ["a final flag on a release candidate", { prerelease: false }, "does not match its tag"],
    ["a short commit", { commit: "0123456" }, "full commit SHA"],
    ["a non-integer epoch", { sourceDateEpoch: 1.5 }, "sourceDateEpoch"],
    ["a malformed repository", { repository: "trustiot-v2" }, "owner/name"],
    ["a chart of another version", { chart: { name: "trustiot", repository: `${REGISTRY}/charts/trustiot`, version: "1.2.0" } }, "chart version"],
  ])("rejects %s", (_label, overrides, message) => {
    expect(() => parsePlan(JSON.stringify(samplePlan(overrides as Partial<ReleasePlan>)))).toThrow(message);
  });

  test("rejects tampered images", () => {
    const plan = samplePlan();
    const tamper = (change: (images: Record<string, unknown>[]) => void) => {
      const doc = JSON.parse(JSON.stringify(plan));
      change(doc.images);
      return () => parsePlan(JSON.stringify(doc));
    };
    expect(tamper((images) => images.push({ ...images[0] }))).toThrow("invalid or repeated");
    expect(tamper((images) => { images[0]!.ref = `${REGISTRY}/identity:latest`; })).toThrow("inconsistent reference");
    expect(tamper((images) => { images[0]!.repository = "ghcr.io/other/identity-x"; })).toThrow("inconsistent reference");
    expect(tamper((images) => { images[0]!.kind = "shell"; })).toThrow("unknown kind");
    expect(tamper((images) => { images[1]!.context = "deploy"; })).toThrow("unexpected build context");
    expect(tamper((images) => { images[0]!.package = "a b"; })).toThrow("invalid package or bin");
  });

  test("rejects input that is not a plan", () => {
    expect(() => parsePlan("not json")).toThrow("not JSON");
    expect(() => parsePlan("[]")).toThrow("JSON object");
  });

  test("requireRegistry accepts only plans for the release registry", () => {
    expect(() => requireRegistry(samplePlan())).not.toThrow();
    const local = samplePlan({
      images: [planImage({ kind: "rust", name: "identity", package: "identity" }, "1.2.0-rc.1", "127.0.0.1:5068/laysi/trustiot-v2")],
    });
    expect(() => requireRegistry(local)).toThrow("127.0.0.1:5068/laysi/trustiot-v2/identity");
    const chart = samplePlan({ chart: { name: "trustiot", repository: "ghcr.io/laysi/trustiot-v2-fork/charts/trustiot", version: "1.2.0-rc.1" } });
    expect(() => requireRegistry(chart)).toThrow("releases publish only to ghcr.io/laysi/trustiot-v2");
  });

  test("requireLoopbackRegistry keeps local tests on this machine", () => {
    const local = (registry: string) =>
      samplePlan({
        images: [planImage({ kind: "rust", name: "identity", package: "identity" }, "1.2.0-rc.1", registry)],
        chart: { name: "trustiot", repository: `${registry}/charts/trustiot`, version: "1.2.0-rc.1" },
      });
    for (const registry of ["127.0.0.1:5068/laysi/trustiot-v2", "localhost:5000/x", "localhost/x", "[::1]:5068/x"]) {
      expect(() => requireLoopbackRegistry(local(registry))).not.toThrow();
    }
    expect(() => requireLoopbackRegistry(samplePlan())).toThrow("local tests publish only to a registry on this machine");
    for (const registry of ["127.0.0.2:5068/x", "localhost.example.com/x", "registry.local:5000/x", "ghcr.io/laysi/trustiot-v2"]) {
      expect(() => requireLoopbackRegistry(local(registry))).toThrow(registry);
    }
  });
});

describe("ci on main", () => {
  const COMMIT = "0123456789abcdef0123456789abcdef01234567";
  const OTHER = "89abcdef0123456789abcdef0123456789abcdef";

  function run(overrides: Partial<WorkflowRun> = {}): WorkflowRun {
    return {
      id: 100,
      path: CI_WORKFLOW_PATH,
      headSha: COMMIT,
      headBranch: "main",
      event: "push",
      status: "completed",
      conclusion: "success",
      url: "https://github.com/Laysi/trustiot-v2/actions/runs/100",
      ...overrides,
    };
  }

  test("asks for the push runs of ci.yml on main for the commit", () => {
    const url = new URL(ciRunsUrl("https://api.github.com/", "Laysi/trustiot-v2", COMMIT));
    expect(url.origin + url.pathname).toBe("https://api.github.com/repos/Laysi/trustiot-v2/actions/workflows/ci.yml/runs");
    expect(Object.fromEntries(url.searchParams)).toEqual({ head_sha: COMMIT, branch: "main", event: "push", per_page: "100" });
    expect(() => ciRunsUrl("https://api.github.com", "Laysi/trustiot-v2/x", COMMIT)).toThrow("not an owner/name repository");
    expect(() => ciRunsUrl("https://api.github.com", "Laysi/trustiot-v2", "abc123")).toThrow("not a full commit SHA");
  });

  test("reads the runs of a GitHub API answer", () => {
    const answer = {
      total_count: 1,
      workflow_runs: [
        {
          id: 38052766358,
          name: "ci",
          path: ".github/workflows/ci.yml",
          head_sha: COMMIT,
          head_branch: "main",
          event: "push",
          status: "completed",
          conclusion: "success",
          html_url: "https://github.com/Laysi/trustiot-v2/actions/runs/38052766358",
        },
        { id: 2, path: ".github/workflows/ci.yml", head_sha: COMMIT, head_branch: null, event: "push", status: "queued", conclusion: null },
      ],
    };
    expect(parseWorkflowRuns(answer)).toEqual([
      run({ id: 38052766358, url: "https://github.com/Laysi/trustiot-v2/actions/runs/38052766358" }),
      run({ id: 2, headBranch: "", status: "queued", conclusion: "", url: "" }),
    ]);
    expect(parseWorkflowRuns({ total_count: 0, workflow_runs: [] })).toEqual([]);
    expect(() => parseWorkflowRuns({ message: "Not Found" })).toThrow("without a workflow_runs list");
    expect(() => parseWorkflowRuns({ workflow_runs: [{ id: "1" }] })).toThrow("workflow run 0");
  });

  test("passes when the newest push run on main for the commit succeeded", () => {
    expect(greenCiProblem([run()], COMMIT)).toBeUndefined();
    expect(greenCiProblem([run({ id: 1, conclusion: "failure" }), run({ id: 2 })], COMMIT)).toBeUndefined();
    expect(greenCiProblem([run({ path: `${CI_WORKFLOW_PATH}@refs/heads/main` })], COMMIT)).toBeUndefined();
  });

  test("refuses when there is no run on main for the commit", () => {
    expect(greenCiProblem([], COMMIT)).toContain(`no ci run on main for ${COMMIT}`);
    // Runs that do not count: another commit, branch, event or workflow.
    for (const other of [
      run({ headSha: OTHER }),
      run({ headBranch: "ci/68-release-workflow" }),
      run({ event: "pull_request" }),
      run({ event: "workflow_dispatch" }),
      run({ path: ".github/workflows/release.yml" }),
      run({ path: ".github/workflows/ci.yml.bak" }),
    ]) {
      expect(greenCiProblem([other], COMMIT)).toContain("no ci run on main");
    }
  });

  test("refuses while ci is still running, and when it did not succeed", () => {
    expect(greenCiProblem([run({ status: "in_progress", conclusion: "" })], COMMIT)).toContain("is in_progress");
    expect(greenCiProblem([run({ status: "queued", conclusion: "" })], COMMIT)).toContain("re-run this release");
    for (const conclusion of ["failure", "cancelled", "timed_out", "skipped", "action_required", ""]) {
      expect(greenCiProblem([run({ conclusion })], COMMIT)).toContain("ended with");
    }
    // A newer run that failed outweighs an older success.
    expect(greenCiProblem([run({ id: 1 }), run({ id: 2, conclusion: "failure" })], COMMIT)).toContain("ended with failure");
    expect(greenCiProblem([run({ id: 2, conclusion: "failure" }), run({ id: 1 })], COMMIT)).toContain("ended with failure");
  });
});

describe("signing identity", () => {
  test("is release.yml at the tag, with the GitHub OIDC issuer", () => {
    const plan = samplePlan();
    expect(signingIdentity(plan.repository, plan.tag)).toBe(
      "https://github.com/Laysi/trustiot-v2/.github/workflows/release.yml@refs/tags/v1.2.0-rc.1",
    );
    const args = keylessVerifyArgs(plan);
    const flag = (name: string) => args[args.indexOf(name) + 1];
    expect(flag("--certificate-identity")).toBe(signingIdentity(plan.repository, plan.tag));
    expect(flag("--certificate-oidc-issuer")).toBe(GITHUB_OIDC_ISSUER);
    expect(flag("--certificate-github-workflow-repository")).toBe("Laysi/trustiot-v2");
    expect(flag("--certificate-github-workflow-ref")).toBe("refs/tags/v1.2.0-rc.1");
    expect(flag("--certificate-github-workflow-sha")).toBe(plan.commit);
    expect(flag("--certificate-github-workflow-trigger")).toBe("push");
  });
});

describe("imagePlatformProblems", () => {
  const manifest = { mediaType: "application/vnd.oci.image.manifest.v1+json" };

  test("accepts one linux/arm64 image", () => {
    expect(imagePlatformProblems(manifest, { os: "linux", architecture: "arm64" })).toEqual([]);
    expect(
      imagePlatformProblems({ mediaType: "application/vnd.docker.distribution.manifest.v2+json" }, { os: "linux", architecture: "arm64" }),
    ).toEqual([]);
  });

  test("rejects an index and another platform", () => {
    expect(imagePlatformProblems({ mediaType: "application/vnd.oci.image.index.v1+json" }, { os: "linux", architecture: "arm64" })[0]).toContain(
      "single-platform",
    );
    expect(imagePlatformProblems(manifest, { os: "linux", architecture: "amd64" })[0]).toContain("linux/amd64, expected linux/arm64");
    expect(imagePlatformProblems("nope", {})).toEqual(["the manifest is not a JSON object"]);
  });
});

describe("summarizeSpdx", () => {
  test("lists packages with versions", () => {
    expect(
      summarizeSpdx({ spdxVersion: "SPDX-2.3", packages: [{ name: "serde", versionInfo: "1.0.0" }, { name: "base-files" }] }),
    ).toEqual({ spdxVersion: "SPDX-2.3", packages: ["serde@1.0.0", "base-files"] });
  });

  test("rejects other documents and empty SBOMs", () => {
    expect(() => summarizeSpdx({ bomFormat: "CycloneDX" })).toThrow("not an SPDX JSON document");
    expect(() => summarizeSpdx({ spdxVersion: "SPDX-2.3", packages: [] })).toThrow("lists no packages");
    expect(() => summarizeSpdx({ spdxVersion: "SPDX-2.3", packages: [{}] })).toThrow("has no name");
  });
});

describe("helmPushDigest", () => {
  test("reads the digest line", () => {
    expect(helmPushDigest(`Pushed: ghcr.io/x/charts/trustiot:1.0.0\nDigest: ${DIGEST_A}\n`)).toBe(DIGEST_A);
  });

  test("fails without one", () => {
    expect(() => helmPushDigest("Pushed: ghcr.io/x/charts/trustiot:1.0.0\n")).toThrow("did not print the chart digest");
  });
});

describe("aarch64ElfProblem", () => {
  const header = (elfClass: number, machine: number) => {
    const bytes = new Uint8Array(64);
    bytes.set([0x7f, 0x45, 0x4c, 0x46, elfClass, 1], 0);
    bytes[18] = machine % 256;
    bytes[19] = Math.floor(machine / 256);
    return bytes;
  };

  test("accepts a 64-bit aarch64 ELF header", () => {
    expect(aarch64ElfProblem(header(2, 183))).toBeUndefined();
  });

  test("rejects other machines, 32-bit files and non-ELF files", () => {
    expect(aarch64ElfProblem(header(2, 62))).toContain("ELF machine is 62");
    expect(aarch64ElfProblem(header(1, 183))).toContain("64-bit");
    expect(aarch64ElfProblem(new TextEncoder().encode("#!/bin/sh\n"))).toBe("it is not an ELF file");
    expect(aarch64ElfProblem(new Uint8Array(0))).toBe("it is not an ELF file");
  });

  // The Bun executable running this test is a real ELF file: accepted on the arm64 runners
  // (ci.yml, release.yml), rejected on an x86_64 workstation.
  test.if(process.platform === "linux" && process.arch === "arm64")("accepts the aarch64 binary running this test", () => {
    expect(aarch64ElfProblem(new Uint8Array(readFileSync(process.execPath)))).toBeUndefined();
  });

  test.if(process.platform === "linux" && process.arch === "x64")("rejects the x86_64 binary running this test", () => {
    expect(aarch64ElfProblem(new Uint8Array(readFileSync(process.execPath)))).toContain("ELF machine is 62");
  });
});

describe("published release", () => {
  const published = {
    images: [{ name: "identity", ref: `${REGISTRY}/identity:1.2.0-rc.1`, digest: DIGEST_A, sbomAsset: sbomAssetName("identity", "1.2.0-rc.1") }],
    chart: { name: "trustiot", ref: `${REGISTRY}/charts/trustiot:1.2.0-rc.1`, digest: DIGEST_B },
  };

  test("parsePublished checks every artifact", () => {
    expect(parsePublished(JSON.stringify({ ...published, releaseUrl: "u" }))).toEqual({ ...published, releaseUrl: "u" });
    expect(() => parsePublished(JSON.stringify({ ...published, chart: { ...published.chart, digest: "sha256:1" } }))).toThrow(
      "no name, reference or digest",
    );
    expect(() => parsePublished(JSON.stringify({ images: [{ ...published.images[0], sbomAsset: "" }], chart: published.chart }))).toThrow(
      "no SBOM asset",
    );
  });

  test("requirePublishedAsPlanned accepts only the planned images and chart", () => {
    const plan = samplePlan({ images: [planImage({ kind: "rust", name: "identity", package: "identity" }, "1.2.0-rc.1")] });
    expect(() => requirePublishedAsPlanned(plan, published)).not.toThrow();
    const image = published.images[0]!;
    const cases: [typeof published, string][] = [
      [{ ...published, images: [] }, "differ from the planned ones"],
      [{ ...published, images: [image, image] }, "differ from the planned ones"],
      [{ ...published, images: [{ ...image, ref: `evil.example/laysi/trustiot-v2/identity:1.2.0-rc.1` }] }, "is not the planned"],
      [{ ...published, images: [{ ...image, name: "assets" }] }, "is not the planned image"],
      [{ ...published, chart: { ...published.chart, ref: "evil.example/charts/trustiot:1.2.0-rc.1" } }, "the published chart"],
      [{ ...published, chart: { ...published.chart, name: "other" } }, "the published chart"],
    ];
    for (const [result, message] of cases) {
      expect(() => requirePublishedAsPlanned(plan, result)).toThrow(message);
    }
  });

  test("the release body lists digests and the exact verification commands", () => {
    const plan = samplePlan();
    const builder = { podman: "4.9.3", buildah: "1.33.7", packages: { podman: "4.9.3+ds1-1ubuntu0.2", buildah: "1.33.7+ds1-1ubuntu0.24.04.3" } };
    const body = renderReleaseBody("### 新功能\n\n- **x**：y\n", plan, published, builder);
    expect(body.startsWith("### 新功能")).toBe(true);
    expect(body).toContain(`\`${DIGEST_A}\``);
    expect(body).toContain(`\`${DIGEST_B}\``);
    expect(body).toContain(`cosign verify ${REGISTRY}/identity:1.2.0-rc.1`);
    expect(body).toContain(`--certificate-identity '${signingIdentity(plan.repository, plan.tag)}'`);
    expect(body).toContain(`--certificate-oidc-issuer ${GITHUB_OIDC_ISSUER}`);
    expect(body).toContain("cosign verify-attestation --type spdxjson");
    expect(body).toContain(plan.commit);
    expect(body).toContain("映像以 podman `4.9.3`（buildah `1.33.7`）打包，套件版本：`podman 4.9.3+ds1-1ubuntu0.2`、`buildah 1.33.7+ds1-1ubuntu0.24.04.3`。");
    expect(renderReleaseBody("notes", plan, published, { ...builder, packages: {} })).toContain("映像以 podman `4.9.3`（buildah `1.33.7`）打包。");
    expect(sbomAssetName("identity", "1.2.0-rc.1")).toBe("identity-1.2.0-rc.1.spdx.json");
  });
});

describe("consistency with the chart", () => {
  test("images go where the chart pulls them from", () => {
    const values = Bun.YAML.parse(readFileSync(join(ROOT, "deploy/chart/values.yaml"), "utf8"));
    const image = isMap(values) && isMap(values.serviceDefaults) && isMap(values.serviceDefaults.image) ? values.serviceDefaults.image : {};
    expect(image.registry).toBe(REGISTRY);
  });
});
