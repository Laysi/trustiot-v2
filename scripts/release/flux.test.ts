import { describe, expect, test } from "bun:test";
import {
  APP_DIR,
  CHART_SOURCE_FILE,
  CLUSTER_DIR,
  CLUSTERS,
  SHAPE_VALUES_FILE,
  appDir,
  chartSourceFile,
  clusterDir,
  deployBranch,
  exactPattern,
  isNewerRelease,
  parseChartSource,
  parseShapeValues,
  renderChartSource,
  renderShapeValues,
  shapeValuesFile,
} from "./flux.ts";
import { REGISTRY, signingIdentity } from "./lib.ts";

const REPOSITORY = "Laysi/trustiot-v2";
const CHART = `${REGISTRY}/charts/trustiot`;
const DIGEST = `sha256:${"c".repeat(64)}`;
const source = { version: "1.2.0", digest: DIGEST, repository: REPOSITORY, chartRepository: CHART };

describe("exactPattern", () => {
  test("matches only the exact text", () => {
    const pattern = new RegExp(exactPattern(signingIdentity(REPOSITORY, "v1.2.0")));
    expect(pattern.test(signingIdentity(REPOSITORY, "v1.2.0"))).toBe(true);
    expect(pattern.test(signingIdentity(REPOSITORY, "v1.2.0-rc.1"))).toBe(false);
    expect(pattern.test(signingIdentity(REPOSITORY, "v1x2y0"))).toBe(false);
    expect(pattern.test(`x${signingIdentity(REPOSITORY, "v1.2.0")}`)).toBe(false);
  });
});

describe("chart source", () => {
  test("pins tag and digest and accepts only release.yml at that tag", () => {
    const doc = Bun.YAML.parse(renderChartSource(source)) as {
      spec: { url: string; ref: unknown; verify: { provider: string; matchOIDCIdentity: { issuer: string; subject: string }[] } };
    };
    expect(doc.spec.url).toBe(`oci://${CHART}`);
    expect(doc.spec.ref).toEqual({ tag: "1.2.0", digest: DIGEST });
    expect(doc.spec.verify.provider).toBe("cosign");
    expect(doc.spec.verify.matchOIDCIdentity).toEqual([
      {
        issuer: "^https://token\\.actions\\.githubusercontent\\.com$",
        subject: "^https://github\\.com/Laysi/trustiot-v2/\\.github/workflows/release\\.yml@refs/tags/v1\\.2\\.0$",
      },
    ]);
  });

  test("round-trips, and rejects any edit", () => {
    const text = renderChartSource(source);
    expect(parseChartSource(text, REPOSITORY, CHART)).toEqual(source);
    expect(() => parseChartSource(text.replace("provider: cosign", "provider: notation"), REPOSITORY, CHART)).toThrow(/edited by hand/);
    expect(() => parseChartSource(`${text}# note\n`, REPOSITORY, CHART)).toThrow(/edited by hand/);
    expect(() => parseChartSource(text, "Laysi/other", CHART)).toThrow(/edited by hand/);
    expect(() => parseChartSource(text.replace(DIGEST, "sha256:short"), REPOSITORY, CHART)).toThrow(/not a sha256 digest/);
    expect(() => parseChartSource(text.replace("tag: 1.2.0", "tag: latest"), REPOSITORY, CHART)).toThrow(/not a release tag/);
    expect(() => parseChartSource("{", REPOSITORY, CHART)).toThrow();
  });

  test("errors name the file of the cluster that was read", () => {
    const text = renderChartSource(source).replace("provider: cosign", "provider: notation");
    expect(() => parseChartSource(text, REPOSITORY, CHART, chartSourceFile("demo"))).toThrow(
      "deploy/flux/clusters/demo/trustiot/chart.yaml is not what scripts/release/flux.ts renders",
    );
    expect(() => parseShapeValues("global: {}\n", shapeValuesFile("demo"))).toThrow(
      'deploy/flux/clusters/demo/trustiot/values-saas.yaml has no "Copied by release.yml" header',
    );
  });

  test("each cluster has its own directory under deploy/flux/clusters, and the SaaS paths keep their names", () => {
    expect(CLUSTERS.map((cluster) => chartSourceFile(cluster))).toEqual([
      "deploy/flux/clusters/saas/trustiot/chart.yaml",
      "deploy/flux/clusters/demo/trustiot/chart.yaml",
    ]);
    expect(CHART_SOURCE_FILE).toBe(chartSourceFile("saas"));
    expect(SHAPE_VALUES_FILE).toBe(shapeValuesFile("saas"));
    expect(APP_DIR).toBe(appDir("saas"));
    expect(CLUSTER_DIR).toBe(clusterDir("saas"));
  });

  test("refuses versions that are not release versions", () => {
    expect(() => renderChartSource({ ...source, version: "1.2" })).toThrow(/not a release tag/);
    expect(() => renderChartSource({ ...source, version: "1.2.0'\nfoo: bar" })).toThrow(/not a release tag/);
  });
});

describe("shape values copy", () => {
  test("names its release and keeps the text", () => {
    const text = renderShapeValues("1.2.0-rc.1", "global:\n  deploymentMode: saas\n");
    expect(parseShapeValues(text)).toEqual({ version: "1.2.0-rc.1", source: "global:\n  deploymentMode: saas\n" });
    expect(() => parseShapeValues("global: {}\n")).toThrow(/no "Copied by release.yml" header/);
  });
});

describe("deploy decisions", () => {
  test("only a higher version replaces the deployed one", () => {
    expect(isNewerRelease("1.2.0", "1.2.0-rc.3")).toBe(true);
    expect(isNewerRelease("1.2.0-rc.4", "1.2.0-rc.3")).toBe(true);
    expect(isNewerRelease("1.2.0", "1.2.0")).toBe(false);
    expect(isNewerRelease("1.1.9", "1.2.0")).toBe(false);
    expect(isNewerRelease("1.2.0-rc.1", "1.2.0")).toBe(false);
  });

  test("branch names", () => {
    expect(deployBranch("1.2.0-rc.1")).toBe("deploy/trustiot-1.2.0-rc.1");
    expect(() => deployBranch("../main")).toThrow();
  });
});
