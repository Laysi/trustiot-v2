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
  matchUpdateTypes?: string[];
  prBodyNotes?: string[];
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
  ignorePaths?: string[];
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
    // Read from the workflow, not written here: a Renovate pull request changes only nightly.yml.
    const istio = /ISTIO_VERSION: (\d+\.\d+\.\d+)$/m.exec(text)?.[1];
    expect(istio).toBeDefined();
    expect(groups).toContainEqual(expect.objectContaining({ depName: "istio/istio", currentValue: istio }));
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

// Every copy of a pin must be one Renovate reads and updates in the same pull request; a copy it
// cannot read stays behind, and the pull request fails CI or leaves the copies disagreeing.
describe("every copy of a pin is updated by the same pull request", () => {
  const imageManagers = config.customManagers.filter((m) => m.matchStrings.some((s) => s.includes("currentDigest")));
  const generic = imageManagers.find((m) => m.managerFilePatterns.some((p) => p.includes("rs|ts|ya?ml")))!;
  const patternOf = (pattern: string) => new RegExp(pattern.replace(/^\/|\/$/g, ""));
  // The repository's files as Renovate sees them: tracked files, minus ignorePaths.
  const files = Bun.spawnSync(["git", "ls-files", "--cached", "--others", "--exclude-standard"], { cwd: REPO_ROOT })
    .stdout.toString()
    .split("\n")
    .filter((file) => file !== "" && !(config.ignorePaths ?? []).some((glob) => new Bun.Glob(glob).match(file)));
  // The dockerfile manager reads FROM lines in the same name:tag@digest form.
  const isContainerfile = (file: string) => /(^|\/)(Containerfile|Dockerfile)$|\.Containerfile$/.test(file);
  // Digests that no image manager reads, each written by a tool instead of Renovate.
  const WRITTEN_BY_TOOLS: Record<string, string> = {
    "deploy/flux/clusters/saas/flux-system/kustomization.yaml": "Flux controller images, rewritten by scripts/flux/components.ts with the flux pin",
    "deploy/flux/clusters/saas/trustiot/chart.yaml": "the chart release.yml published, written by scripts/release/flux.ts",
    "deploy/flux/clusters/demo/trustiot/chart.yaml": "the chart release.yml published, written by scripts/release/flux.ts",
  };

  interface Pin {
    file: string;
    image: string;
    tag: string;
    digest: string;
  }
  const pins: Pin[] = [];
  const unread: string[] = [];
  for (const file of files) {
    const managers = imageManagers.filter((m) => m.managerFilePatterns.some((p) => patternOf(p).test(file)));
    const readers = isContainerfile(file) ? [generic] : managers;
    if (readers.length === 0) continue;
    const text = readFileSync(join(REPO_ROOT, file), "utf8");
    const read = new Set<string>();
    for (const manager of readers) {
      for (const match of text.matchAll(new RegExp(manager.matchStrings[0]!, "g"))) {
        const { depName, currentValue, currentDigest } = match.groups!;
        pins.push({ file, image: depName!, tag: currentValue!, digest: currentDigest! });
        read.add(currentDigest!);
      }
    }
    const all = [...text.matchAll(/sha256:[a-f0-9]{64}/g)].map((m) => m[0]);
    if (all.some((digest) => !read.has(digest)) && WRITTEN_BY_TOOLS[file] === undefined) unread.push(file);
  }

  test("every digest in a file Renovate reads for images is part of a pin it reads", () => {
    expect(pins.length).toBeGreaterThan(10);
    // A bare name@sha256:… (no tag) is the usual cause: write it as name:tag@sha256:….
    expect(unread).toEqual([]);
  });

  test("an image is always spelled the same way (Renovate tells copies apart by the spelling)", () => {
    const canonical = (image: string) => image.replace(/^docker\.io\//, "").replace(/^library\//, "");
    const spellings = new Map<string, Set<string>>();
    for (const { image } of pins) spellings.set(canonical(image), (spellings.get(canonical(image)) ?? new Set()).add(image));
    expect([...spellings].filter(([, set]) => set.size > 1).map(([, set]) => [...set])).toEqual([]);
  });

  test("all copies of an image and tag carry the same digest", () => {
    const digests = new Map<string, Set<string>>();
    for (const { image, tag, digest } of pins) digests.set(`${image}:${tag}`, (digests.get(`${image}:${tag}`) ?? new Set()).add(digest));
    expect([...digests].filter(([, set]) => set.size > 1).map(([pin]) => pin)).toEqual([]);
    // The images this was written for, each pinned in more than one file.
    const copies = (image: string) => new Set(pins.filter((p) => p.image === image).map((p) => p.file)).size;
    // The chart, the dev environment, services/ingest/tests and tools/migrate/tests.
    expect(copies("docker.io/clickhouse/clickhouse-server")).toBe(4);
    // The chart, the dev environment, crates/protocol/tests, tools/migrate/tests and
    // services/certs/tests (workstation.rs, broker.rs and broker_certificate.rs).
    expect(copies("docker.io/library/eclipse-mosquitto")).toBe(7);
    expect(copies("gcr.io/distroless/cc-debian13")).toBe(4);
  });

  test("ClickHouse gets LTS minors only, and its floating tag is one", () => {
    const lts = allowed(rulesFor((r) => r.matchPackageNames?.includes("docker.io/clickhouse/clickhouse-server") ?? false));
    for (const tag of ["26.8", "27.3", "27.8"]) expect({ tag, allowed: lts.test(tag) }).toEqual({ tag, allowed: true });
    // 26.9 is what the dashboard proposed on 2026-10-13; a full version would replace the floating tag.
    for (const tag of ["26.9", "27.1", "26.8.22.13", "latest"]) expect({ tag, allowed: lts.test(tag) }).toEqual({ tag, allowed: false });
    for (const { tag } of pins.filter((p) => p.image === "docker.io/clickhouse/clickhouse-server")) expect(lts.test(tag)).toBe(true);
  });

  test("a collector version change carries a note about the guard in the chart tests", () => {
    const rule = rulesFor((r) => r.matchPackageNames?.includes("docker.io/otel/opentelemetry-collector-contrib") ?? false);
    expect(rule.matchUpdateTypes).toEqual(["major", "minor", "patch"]);
    expect(rule.prBodyNotes?.join(" ")).toContain("EXPORTER_VERSION");
    const chartTests = readFileSync(join(REPO_ROOT, "deploy", "chart", "tests", "chart.test.ts"), "utf8");
    const version = /^const EXPORTER_VERSION = "([^"]+)";$/m.exec(chartTests)?.[1];
    const collector = pins.find((p) => p.image === "docker.io/otel/opentelemetry-collector-contrib" && p.file === "deploy/chart/values.yaml");
    expect(collector?.tag).toBe(version);
  });

  test("the nightly's k3s and Istio are the versions the SaaS host and the lab install", () => {
    const nightly = readFileSync(join(REPO_ROOT, ".github", "workflows", "nightly.yml"), "utf8");
    const k3s = /K3S_IMAGE: rancher\/k3s:(v\d+\.\d+\.\d+)-(k3s\d+)@/.exec(nightly);
    const host = readFileSync(join(REPO_ROOT, "deploy", "host", "src", "desired.ts"), "utf8");
    const hostVersion = /^export const K3S_VERSION = "([^"]+)";$/m.exec(host)?.[1];
    expect(hostVersion).toBe(`${k3s?.[1]}+${k3s?.[2]}`);
    const installer = `https://raw.githubusercontent.com/k3s-io/k3s/${encodeURIComponent(hostVersion!)}/install.sh`;
    expect(host).toContain(`K3S_INSTALLER_URL = "${installer}"`);
    const istio = /ISTIO_VERSION: (\S+)/.exec(nightly)?.[1];
    const readme = readFileSync(join(REPO_ROOT, "deploy", "istio", "README.md"), "utf8");
    const installed = [...readme.matchAll(/--version (\S+)/g)].map((m) => m[1]);
    expect(installed).toHaveLength(4);
    expect(new Set(installed)).toEqual(new Set([istio]));
  });
});
