// Guards the Renovate configuration (renovate.json5): the pinning rules the nightly issue asks
// for, and that every tool pinned in .prototools is one Renovate can update.
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const REPO_ROOT = join(import.meta.dir, "..", "..");

interface Rule {
  matchDatasources?: string[];
  pinDigests?: boolean;
  enabled?: boolean;
  matchPackageNames?: string[];
  allowedVersions?: string;
}
interface Manager {
  customType: string;
  managerFilePatterns: string[];
  matchStrings: string[];
}
interface Config {
  extends: string[];
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
  const BUILT_IN = new Set(["bun", "rust"]);
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
    const gitUrl = (tool: string) => /^git-url = "https:\/\/github\.com\/([^"]+)"$/m.exec(plugin(tool))?.[1];
    const lines = prototools.split("\n");
    for (const [index, line] of lines.entries()) {
      const tool = /^([a-z0-9_-]+) = "[^"]+"$/.exec(line)?.[1];
      const depName = /depName=(\S+)/.exec(lines[index - 1] ?? "")?.[1];
      if (tool === undefined || depName === undefined) continue;
      expect(`${tool}: ${depName}`).toBe(`${tool}: ${gitUrl(tool)}`);
    }
  });
});
