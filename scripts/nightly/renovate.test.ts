// Guards the Renovate configuration (renovate.json5): the pinning rules the nightly issue asks
// for, that every tool pinned in .prototools is one Renovate can update, and the fixes for what
// the first Dependency Dashboard showed (Laysi/trustiot-v2#389): k3s release candidates, runner
// upgrades, two managers on the chart's images, the dashboard's labels and no-op digest pins.
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join, relative } from "node:path";

const REPO_ROOT = join(import.meta.dir, "..", "..");

interface Rule {
  description?: string;
  matchDatasources?: string[];
  matchManagers?: string[];
  pinDigests?: boolean;
  enabled?: boolean;
  matchPackageNames?: string[];
  allowedVersions?: string;
  versioning?: string;
}
interface Manager {
  customType: string;
  managerFilePatterns: string[];
  matchStrings: string[];
}
interface Config {
  extends: string[];
  dependencyDashboardLabels?: string[];
  "helm-values"?: { enabled?: boolean };
  pinDigests?: boolean;
  rangeStrategy?: string;
  ignorePresets?: string[];
  packageRules: Rule[];
  customManagers: Manager[];
}

const config = Bun.JSON5.parse(readFileSync(join(REPO_ROOT, "renovate.json5"), "utf8")) as Config;
const prototools = readFileSync(join(REPO_ROOT, ".prototools"), "utf8");

describe("renovate.json5", () => {
  test("keeps GitHub Actions pinned to a commit SHA with the release tag", () => {
    expect(config.extends).toContain("helpers:pinGitHubActionDigestsToSemver");
  });

  test("keeps container images pinned by digest", () => {
    expect(config.pinDigests).toBe(true);
    const rule = config.packageRules.find((r) => r.matchDatasources?.includes("docker") && r.pinDigests !== undefined);
    expect(rule?.pinDigests).toBe(true);
  });

  test("updates versions as exact pins and still reads the tests directories that hold pinned images", () => {
    expect(config.rangeStrategy).toBe("pin");
    expect(config.ignorePresets).toContain(":ignoreModulesAndTests");
  });

  test("the image manager matches the digest-pinned forms used in this repository", () => {
    const manager = config.customManagers.find((m) => m.managerFilePatterns.some((p) => p.includes("rs|ts|ya?ml")));
    expect(manager).toBeDefined();
    const regex = new RegExp(manager!.matchStrings[0]!);
    const digest = `sha256:${"b".repeat(64)}`;
    const groups = regex.exec(`const IMAGE = "docker.io/library/postgres:18.6-alpine@${digest}";`)?.groups;
    expect(groups).toMatchObject({ depName: "docker.io/library/postgres", currentValue: "18.6-alpine", currentDigest: digest });
    expect(regex.exec("image: rancher/k3s:v1.36.5-k3s1@" + digest)?.groups?.depName).toBe("rancher/k3s");
    // A reference without a digest is not this manager's business.
    expect(regex.test("image: docker.io/library/postgres:18.6-alpine")).toBe(false);
  });
});

describe("annotated workflow versions", () => {
  const manager = config.customManagers.find((m) => m.managerFilePatterns.some((p) => p.includes(".github/workflows")));

  test("every `# renovate:` annotation in a workflow is read by the workflow manager", () => {
    expect(manager).toBeDefined();
    const regex = new RegExp(manager!.matchStrings[0]!, "g");
    for (const file of readdirSync(join(REPO_ROOT, ".github", "workflows")).filter((name) => /\.ya?ml$/.test(name))) {
      const text = readFileSync(join(REPO_ROOT, ".github", "workflows", file), "utf8");
      const annotations = text.split("\n").filter((line) => line.includes("# renovate:")).length;
      const matches = [...text.matchAll(regex)].length;
      expect({ file, matches }).toEqual({ file, matches: annotations });
    }
  });

  test("the Istio version is one of them", () => {
    const text = readFileSync(join(REPO_ROOT, ".github", "workflows", "nightly.yml"), "utf8");
    const groups = [...text.matchAll(new RegExp(manager!.matchStrings[0]!, "g"))].map((m) => m.groups);
    expect(groups).toContainEqual(expect.objectContaining({ depName: "istio/istio", currentValue: "1.30.5" }));
  });
});

describe(".prototools", () => {
  // Renovate's proto manager reads these by itself.
  const BUILT_IN = new Set(["bun", "go", "rust"]);
  const annotation = new RegExp(config.customManagers.find((m) => m.managerFilePatterns.some((p) => p.includes("prototools")))!.matchStrings[0]!, "m");

  test("every pinned tool is known to Renovate or carries a `# renovate:` annotation", () => {
    const pinned = [...prototools.matchAll(/^([a-z0-9_-]+) = "[^"]+"$/gm)].map((match) => match[1]!);
    expect(pinned.length).toBeGreaterThan(0);
    const lines = prototools.split("\n");
    for (const tool of pinned) {
      if (BUILT_IN.has(tool)) continue;
      const index = lines.findIndex((line) => line.startsWith(`${tool} = `));
      const annotated = annotation.test(`${lines[index - 1]}\n${lines[index]}`);
      expect({ tool, annotated }).toEqual({ tool, annotated: true });
    }
  });

  test("the annotation names the upstream of the tool's schema plugin", () => {
    const plugin = (tool: string) => readFileSync(join(REPO_ROOT, ".proto", "plugins", `${tool}.toml`), "utf8");
    const gitUrl = (tool: string) => /^git-url = "([^"]+)"$/m.exec(plugin(tool))?.[1];
    // A GitHub upstream is named owner/repo and read with a github-* datasource. Any other
    // upstream (Zig's is on Codeberg) is named by its repository URL, which only git-tags reads.
    const upstream = (url = "") => {
      const github = /^https:\/\/github\.com\/(.+)$/.exec(url)?.[1];
      return github === undefined
        ? { datasource: "git-tags", depName: url }
        : { datasource: expect.stringMatching(/^github-(releases|tags)$/), depName: github };
    };
    const lines = prototools.split("\n");
    for (const [index, line] of lines.entries()) {
      const tool = /^([a-z0-9_-]+) = "[^"]+"$/.exec(line)?.[1];
      const annotation = /datasource=(\S+) depName=(\S+)/.exec(lines[index - 1] ?? "");
      if (tool === undefined || annotation === null) continue;
      expect({ tool, datasource: annotation[1], depName: annotation[2] }).toEqual({ tool, ...upstream(gitUrl(tool)) });
    }
  });
});

// Renovate merges every matching rule in order, so a later rule wins; this mirrors that.
const rulesFor = (match: (rule: Rule) => boolean) => Object.assign({}, ...config.packageRules.filter(match)) as Rule;
// A JSON5 /…/ string in allowedVersions is a regular expression.
const allowed = (rule: Rule) => new RegExp(rule.allowedVersions!.replace(/^\/|\/$/g, ""));

describe("Kubernetes versions", () => {
  const k3s = rulesFor((rule) => rule.matchPackageNames?.includes("rancher/k3s") ?? false);
  const kubectl = rulesFor((rule) => rule.matchPackageNames?.includes("kubernetes/kubernetes") ?? false);
  const versioning = () => {
    expect(k3s.versioning).toStartWith("regex:");
    return new RegExp(k3s.versioning!.slice("regex:".length));
  };

  test("k3s gets only final releases: release candidates and per-architecture tags are not versions", () => {
    const regex = versioning();
    for (const tag of ["v1.36.5-k3s1", "v1.36.6-k3s2", "v1.37.0-k3s1"]) expect({ tag, version: regex.test(tag) }).toEqual({ tag, version: true });
    // v1.36.5-rc2-k3s1 is what the dashboard proposed on 2026-10-13 under loose versioning.
    for (const tag of ["v1.36.5-rc2-k3s1", "v1.36.6-rc1-k3s1", "v1.36.5-k3s1-amd64", "v1.36.5-k3s1-arm64", "latest", "v1.36"]) {
      expect({ tag, version: regex.test(tag) }).toEqual({ tag, version: false });
    }
    // Every named group is one Renovate's regex versioning understands.
    expect(Object.keys(regex.exec("v1.36.5-k3s1")!.groups!)).toEqual(["major", "minor", "patch", "build"]);
  });

  test("the k3s image in nightly.yml is a version under that versioning", () => {
    const text = readFileSync(join(REPO_ROOT, ".github", "workflows", "nightly.yml"), "utf8");
    const tag = /rancher\/k3s:(\S+)@sha256:/.exec(text)?.[1];
    expect(tag).toBeDefined();
    expect(versioning().test(tag!)).toBe(true);
  });

  test("k3s and kubectl stay on the 1.36 minor", () => {
    for (const [rule, current, next] of [
      [k3s, "v1.36.5-k3s1", "v1.37.0-k3s1"],
      [kubectl, "1.36.5", "1.37.0"],
    ] as const) {
      expect(allowed(rule).test(current)).toBe(true);
      expect(allowed(rule).test(next)).toBe(false);
    }
    // kubectl keeps Renovate's default versioning, which treats 1.36.6-rc.0 as unstable and never
    // proposes it for a stable pin.
    expect(kubectl.versioning).toBeUndefined();
  });
});

describe("rules from the first Dependency Dashboard", () => {
  test("Renovate never proposes a runner image: runs-on is a recorded choice", () => {
    const rule = rulesFor((r) => r.matchDatasources?.includes("github-runners") ?? false);
    expect(rule.enabled).toBe(false);
    expect(rule.description).toContain("docs/architecture/ci-release.md");
  });

  test("the dashboard issue carries an area label and exactly one priority label", () => {
    const labels = config.dependencyDashboardLabels ?? [];
    expect(labels.filter((label) => label.startsWith("area:")).length).toBeGreaterThan(0);
    expect(labels.filter((label) => /^priority:P[0-2]$/.test(label))).toHaveLength(1);
  });

  test("no digest pin of a tool version in .prototools", () => {
    expect(rulesFor((r) => r.matchManagers?.includes("proto") ?? false).pinDigests).toBe(false);
    // GitHub Actions keep their commit SHA pins.
    expect(rulesFor((r) => r.matchManagers?.includes("github-actions") ?? false).pinDigests).toBe(true);
  });

  test("Flux is upgraded through the .prototools pin only, not by the flux manager", () => {
    const rule = rulesFor((r) => (r.matchManagers?.includes("flux") ?? false) && (r.matchPackageNames?.includes("fluxcd/flux2") ?? false));
    expect(rule.enabled).toBe(false);
    expect(prototools).toMatch(/^# renovate: datasource=github-releases depName=fluxcd\/flux2 .*\nflux = "/m);
  });
});

describe("chart values images", () => {
  const chartManager = config.customManagers.find((m) => m.managerFilePatterns.some((p) => p.includes("deploy/chart")))!;
  const imageManager = config.customManagers.find((m) => m.managerFilePatterns.some((p) => p.includes("rs|ts|ya?ml")))!;
  const digest = `sha256:${"c".repeat(64)}`;

  test("only the custom manager reads them: the helm-values manager is off", () => {
    expect(chartManager).toBeDefined();
    expect(config["helm-values"]?.enabled).toBe(false);
  });

  test("the tag is read without its quotes, so the lookup and the digest are for the real tag", () => {
    const regex = new RegExp(chartManager.matchStrings[0]!);
    for (const tag of ['"26.8"', "'26.8'", "26.8"]) {
      const groups = regex.exec(`    image:\n      repository: docker.io/clickhouse/clickhouse-server\n      tag: ${tag}\n      digest: ${digest}\n`)?.groups;
      expect({ tag, groups }).toEqual({ tag, groups: { depName: "docker.io/clickhouse/clickhouse-server", currentValue: "26.8", currentDigest: digest } });
    }
    // An image without a digest (our own images, digest: "") is not this manager's business.
    expect(regex.test('repository: ghcr.io/laysi/trustiot-v2/postgres\n      tag: ""\n      digest: ""')).toBe(false);
  });

  test("every pinned image in the chart's YAML files is read by exactly one of the two image managers", () => {
    const yamlFiles = (dir: string): string[] =>
      readdirSync(dir, { withFileTypes: true }).flatMap((entry) =>
        entry.isDirectory() ? yamlFiles(join(dir, entry.name)) : /\.ya?ml$/.test(entry.name) ? [join(dir, entry.name)] : [],
      );
    const fileMatches = (manager: Manager, file: string) =>
      manager.managerFilePatterns.some((pattern) => new RegExp(pattern.replace(/^\/|\/$/g, "")).test(file));
    const digests = (manager: Manager, file: string, text: string) =>
      fileMatches(manager, file) ? [...text.matchAll(new RegExp(manager.matchStrings[0]!, "g"))].map((m) => m.groups!.currentDigest!) : [];
    let pinned = 0;
    for (const path of yamlFiles(join(REPO_ROOT, "deploy", "chart"))) {
      const file = relative(REPO_ROOT, path);
      const text = readFileSync(path, "utf8");
      const expected = [...text.matchAll(/sha256:[a-f0-9]{64}/g)].map((m) => m[0]);
      pinned += expected.length;
      const read = [...digests(chartManager, file, text), ...digests(imageManager, file, text)];
      expect({ file, read: read.sort() }).toEqual({ file, read: expected.sort() });
    }
    expect(pinned).toBeGreaterThan(0);
  });
});
