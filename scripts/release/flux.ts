/**
 * The clusters in deploy/flux/ as release.yml sees them (Laysi/trustiot-v2#70, #457): the files
 * the deploy-pr job rewrites for each release, rendered and parsed here so that the job, the
 * tests and a hand review agree on their exact content.
 *
 * Each cluster under deploy/flux/clusters/ has its own copy of the two files, so each one runs the
 * version that its own files name:
 * - chartSourceFile(cluster) is the OCIRepository that Flux pulls the chart from. It pins the
 *   chart by tag and digest and only accepts a keyless cosign signature by release.yml at that
 *   exact tag (decision 30). It is always exactly `renderChartSource(...)` of its own version, so
 *   a hand edit that drops or widens the verification fails `bun test deploy/flux`.
 * - shapeValuesFile(cluster) is deploy/chart/values-saas.yaml as of the deployed release, so the
 *   deployment shape always matches the chart version it is applied to.
 */

import { GITHUB_OIDC_ISSUER, ReleaseError, compareReleaseTags, isDigest, isMap, parseReleaseTag, signingIdentity } from "./lib.ts";

export const FLUX_DIR = "deploy/flux";

/**
 * The clusters that Flux deploys from this repository: the SaaS cluster (MS4) and the demo
 * environment on the lab Mac mini M1 (Laysi/trustiot-v2#457, deploy/flux/README.md 「展示叢集」).
 */
export const CLUSTERS = ["saas", "demo"] as const;
export type ClusterName = (typeof CLUSTERS)[number];

/** The directory Flux applies on a cluster (its GitRepository sync path). */
export function clusterDir(cluster: ClusterName): string {
  return `${FLUX_DIR}/clusters/${cluster}`;
}

/** The TrustIoT release of a cluster: namespace, chart source, HelmRelease, values, SOPS Secrets. */
export function appDir(cluster: ClusterName): string {
  return `${clusterDir(cluster)}/trustiot`;
}

/** A cluster's OCIRepository of the chart; rewritten for a release by the deploy PR or scripts/flux/pin-chart.ts. */
export function chartSourceFile(cluster: ClusterName): string {
  return `${appDir(cluster)}/chart.yaml`;
}

/** A cluster's copy of deploy/chart/values-saas.yaml from its deployed release; rewritten with chartSourceFile. */
export function shapeValuesFile(cluster: ClusterName): string {
  return `${appDir(cluster)}/values-saas.yaml`;
}

/** The SaaS cluster's paths, which the Flux lab e2e test and the GHCR pull Secret script default to. */
export const CLUSTER_DIR = clusterDir("saas");
export const APP_DIR = appDir("saas");
export const CHART_SOURCE_FILE = chartSourceFile("saas");
export const SHAPE_VALUES_FILE = shapeValuesFile("saas");
/** Where the shape values come from, in the release's own checkout. */
export const CHART_SHAPE_VALUES = "deploy/chart/values-saas.yaml";
/** Namespace and Helm release name of the SaaS install (deploy/istio and the runbooks use them). */
export const APP_NAMESPACE = "trustiot";
/** Name of the Kubernetes Secret (kubernetes.io/dockerconfigjson) that holds the read-only GHCR pull token. */
export const GHCR_PULL_NAME = "ghcr-pull";
/** Media type of the chart layer that `helm push` writes. */
export const HELM_CHART_LAYER = "application/vnd.cncf.helm.chart.content.v1.tar+gzip";

/** A regular expression that matches exactly `text`. */
export function exactPattern(text: string): string {
  return `^${text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`;
}

export interface ChartSource {
  /** Chart version, the release tag without `v`. */
  version: string;
  /** Digest of the chart manifest that release.yml pushed and signed. */
  digest: string;
  /** `owner/name` as GitHub spells it; part of the signing identity. */
  repository: string;
  /** `<registry>/<path>/<chart name>`, without `oci://` and without tag. */
  chartRepository: string;
}

/**
 * The OCIRepository for one release. Flux pulls the chart by digest (the tag is for people), with
 * the read-only pull token, and installs it only when its signature was made by release.yml
 * running for exactly this tag (Flux 2.8 or later reads cosign v3 bundles).
 */
export function renderChartSource(source: ChartSource): string {
  const tag = `v${source.version}`;
  parseReleaseTag(tag);
  if (!isDigest(source.digest)) {
    throw new ReleaseError(`chart digest ${source.digest} is not a sha256 digest`);
  }
  const chartName = source.chartRepository.split("/").at(-1) ?? "";
  return [
    "# The TrustIoT chart that Flux deploys. release.yml rewrites this file in its deploy PR for every",
    "# release (scripts/release/flux.ts, renderChartSource); deploy a version by merging that PR and",
    "# roll back by reverting it. Do not edit by hand: `bun test deploy/flux` fails unless the file is",
    "# exactly what renderChartSource writes for its version, so the signature check cannot be",
    "# dropped or widened by accident.",
    "apiVersion: source.toolkit.fluxcd.io/v1",
    "kind: OCIRepository",
    "metadata:",
    `  name: ${chartName}`,
    `  namespace: ${APP_NAMESPACE}`,
    "spec:",
    "  interval: 10m",
    `  url: oci://${source.chartRepository}`,
    "  ref:",
    `    tag: ${source.version}`,
    "    # Takes precedence over the tag: the exact chart that release.yml pushed and signed.",
    `    digest: ${source.digest}`,
    "  layerSelector:",
    `    mediaType: ${HELM_CHART_LAYER}`,
    "    operation: copy",
    "  # The read-only GHCR pull token (SOPS-encrypted in this directory, deploy/flux/README.md).",
    "  secretRef:",
    `    name: ${GHCR_PULL_NAME}`,
    "  # Keyless cosign signature by release.yml at this tag (decision 30).",
    "  verify:",
    "    provider: cosign",
    "    matchOIDCIdentity:",
    `      - issuer: '${exactPattern(GITHUB_OIDC_ISSUER)}'`,
    `        subject: '${exactPattern(signingIdentity(source.repository, tag))}'`,
    "",
  ].join("\n");
}

/**
 * Reads the chart source back. It must be exactly what renderChartSource writes for the
 * repository and chart it names, so a hand-edited file is an error, not a silent change.
 * `file` only names the file in the errors.
 */
export function parseChartSource(text: string, repository: string, chartRepository: string, file = CHART_SOURCE_FILE): ChartSource {
  let doc: unknown;
  try {
    doc = Bun.YAML.parse(text);
  } catch (error) {
    throw new ReleaseError(`${file} is not YAML: ${error instanceof Error ? error.message : String(error)}`);
  }
  const ref = isMap(doc) && isMap(doc.spec) && isMap(doc.spec.ref) ? doc.spec.ref : {};
  const version = typeof ref.tag === "string" ? ref.tag : "";
  const digest = typeof ref.digest === "string" ? ref.digest : "";
  let expected: string;
  try {
    expected = renderChartSource({ version, digest, repository, chartRepository });
  } catch (error) {
    throw new ReleaseError(`${file}: ${error instanceof Error ? error.message : String(error)}`);
  }
  if (text !== expected) {
    throw new ReleaseError(
      `${file} is not what scripts/release/flux.ts renders for ${version}: it was edited by hand. ` +
        "Revert the edit, or roll back by reverting the deploy PR",
    );
  }
  return { version, digest, repository, chartRepository };
}

const SHAPE_HEADER = /^# Copied by release\.yml from deploy\/chart\/values-saas\.yaml at (v\S+)\. Do not edit; see deploy\/flux\/README\.md\.\n/;

/** deploy/chart/values-saas.yaml of release `version`, with a header that names the release. */
export function renderShapeValues(version: string, source: string): string {
  const tag = `v${version}`;
  parseReleaseTag(tag);
  return `# Copied by release.yml from deploy/chart/values-saas.yaml at ${tag}. Do not edit; see deploy/flux/README.md.\n${source}`;
}

/** The release a shape values copy came from, and the copied text. `file` only names the file in errors. */
export function parseShapeValues(text: string, file = SHAPE_VALUES_FILE): { version: string; source: string } {
  const match = SHAPE_HEADER.exec(text);
  if (match?.[1] === undefined) {
    throw new ReleaseError(`${file} has no "Copied by release.yml" header`);
  }
  return { version: parseReleaseTag(match[1]).version, source: text.slice(match[0].length) };
}

/** Whether `next` should replace `current` in a deploy PR: only a higher version does. */
export function isNewerRelease(next: string, current: string): boolean {
  return compareReleaseTags(`v${next}`, `v${current}`) > 0;
}

/** The branch release.yml pushes the deploy change to. */
export function deployBranch(version: string): string {
  parseReleaseTag(`v${version}`);
  return `deploy/trustiot-${version}`;
}
