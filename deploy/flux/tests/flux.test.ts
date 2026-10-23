// Tests of the GitOps trees in deploy/flux/clusters: the SaaS cluster and the demo environment
// (deploy/flux/README.md). Run from the repository root:
//   bun test deploy/flux
// Needs kubectl (kustomize), kubeconform, helm and flux from .prototools; kubeconform downloads
// the Kubernetes schemas on first use. The Flux resources are validated against the CRDs in
// gotk-components.yaml, so the check follows the pinned Flux version without network access.
import { describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { isAbsolute, join } from "node:path";
import {
  APP_DIR,
  APP_NAMESPACE,
  CLUSTERS,
  type ClusterName,
  HELM_CHART_LAYER,
  GHCR_PULL_NAME,
  appDir,
  chartSourceFile,
  clusterDir,
  exactPattern,
  parseChartSource,
  parseShapeValues,
  shapeValuesFile,
} from "../../../scripts/release/flux.ts";
import { CHART_REGISTRY, GITHUB_OIDC_ISSUER, signingIdentity } from "../../../scripts/release/lib.ts";
import {
  DEMO_PRIVATE_VALUES,
  DEMO_VALUES_NAME,
  FLUX_COMPONENTS,
  FLUX_SYSTEM_DIR,
  SOPS_SECRETS,
  controllerImages,
  renderDemoValuesSecret,
  renderOpaqueSecret,
  renderPullSecret,
  sopsFileProblems,
  sopsRecipientProblems,
  sopsRuleTexts,
} from "../../../scripts/flux/lib.ts";
import { SOPS_SCOPE_LINE, enc, fakeSops } from "../../../scripts/flux/sops-fixture.ts";
import { ipv4ToNumber, staticBandOf } from "../../../scripts/nightly/ingress-address.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..", "..");
const REPOSITORY = "Laysi/trustiot-v2";
const CHART_REPOSITORY = `${CHART_REGISTRY}/trustiot`;
const SLOW_TEST_MS = 120_000;

type YamlMap = { [key: string]: unknown };
type Manifest = YamlMap & { apiVersion: string; kind: string; metadata: { name: string; namespace?: string; labels?: Record<string, string> } };

function isMap(value: unknown): value is YamlMap {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function pinnedTool(tool: string): string {
  const result = Bun.spawnSync({ cmd: ["proto", "bin", tool, "--reporter", "text"], cwd: REPO_ROOT, stdout: "pipe", stderr: "pipe" });
  const path = result.stdout.toString().trim().split("\n").at(-1)?.trim() ?? "";
  if (result.exitCode !== 0 || !isAbsolute(path) || !existsSync(path)) {
    throw new Error(`cannot find the pinned ${tool}; run \`proto install\`\n${result.stderr.toString()}`);
  }
  return path;
}

function run(cmd: string[], stdin?: string) {
  const proc = Bun.spawnSync({
    cmd,
    cwd: REPO_ROOT,
    stdin: stdin === undefined ? "ignore" : new TextEncoder().encode(stdin),
    stdout: "pipe",
    stderr: "pipe",
  });
  return { code: proc.exitCode, stdout: proc.stdout.toString(), stderr: proc.stderr.toString() };
}

function parseAll(yaml: string): Manifest[] {
  const parsed: unknown = Bun.YAML.parse(yaml);
  return (Array.isArray(parsed) ? parsed : [parsed]).filter(isMap) as Manifest[];
}

function read(path: string): string {
  return readFileSync(join(REPO_ROOT, path), "utf8");
}

const KUBECTL = pinnedTool("kubectl");
const components = read(`${FLUX_SYSTEM_DIR}/gotk-components.yaml`);
const fluxIds = new Set(parseAll(components).map((m) => `${m.kind}/${m.metadata.name}`));

interface Tree {
  built: { code: number; stdout: string; stderr: string };
  manifests: Manifest[];
}

const trees = Object.fromEntries(
  CLUSTERS.map((cluster) => {
    const built = run([KUBECTL, "kustomize", clusterDir(cluster)]);
    return [cluster, { built, manifests: built.code === 0 ? parseAll(built.stdout) : [] }];
  }),
) as Record<ClusterName, Tree>;

function only(cluster: ClusterName, kind: string): Manifest[] {
  return trees[cluster].manifests.filter((m) => m.kind === kind);
}

function one(cluster: ClusterName, kind: string, name: string): Manifest {
  const found = trees[cluster].manifests.find((m) => m.kind === kind && m.metadata.name === name);
  if (found === undefined) throw new Error(`${kind}/${name} is not in the ${cluster} build`);
  return found;
}

function spec(manifest: Manifest): YamlMap {
  return isMap(manifest.spec) ? manifest.spec : {};
}

/** The SOPS-encrypted Secret files of a cluster's release directory. */
function sopsFiles(cluster: ClusterName): string[] {
  return readdirSync(join(REPO_ROOT, appDir(cluster)))
    .filter((file) => file.endsWith(".sops.yaml"))
    .sort();
}

/** The Secrets that a cluster's SOPS files declare, as `Secret/<name>`. */
function sopsSecretIds(cluster: ClusterName): string[] {
  return sopsFiles(cluster).flatMap((file) => parseAll(read(`${appDir(cluster)}/${file}`)).map((m) => `${m.kind}/${m.metadata.name}`));
}

/**
 * A manifest as the API server would see it after kustomize-controller decrypted it: without the
 * `sops` block, and with placeholder values for the encrypted ones, so kubeconform checks the rest.
 */
function asDecrypted(manifest: Manifest): Manifest {
  if (manifest.sops === undefined) return manifest;
  const { sops: _sops, ...rest } = manifest;
  const placeholders = (values: unknown, encode: boolean) =>
    isMap(values) ? Object.fromEntries(Object.keys(values).map((key) => [key, encode ? Buffer.from("x").toString("base64") : "x"])) : values;
  return { ...rest, ...(rest.data === undefined ? {} : { data: placeholders(rest.data, true) }), ...(rest.stringData === undefined ? {} : { stringData: placeholders(rest.stringData, false) }) } as Manifest;
}

const EXPECTED_RELEASE_OBJECTS: Record<ClusterName, string[]> = {
  saas: ["ConfigMap/trustiot-values", "GitRepository/flux-system", "HelmRelease/trustiot", "Kustomization/flux-system", "Namespace/trustiot", "OCIRepository/trustiot"],
  demo: ["ConfigMap/trustiot-values", "GitRepository/flux-system", "HelmRelease/trustiot", "Kustomization/flux-system", "Namespace/trustiot", "OCIRepository/trustiot"],
};

describe.each([...CLUSTERS])("the %s tree", (cluster) => {
  const tree = trees[cluster];

  test("builds", () => {
    expect(tree.built.stderr).toBe("");
    expect(tree.built.code).toBe(0);
  });

  test("holds Flux, its sync, the TrustIoT release and its SOPS Secrets, and nothing else", () => {
    const rest = tree.manifests.map((m) => `${m.kind}/${m.metadata.name}`).filter((id) => !fluxIds.has(id)).sort();
    expect(rest).toEqual([...EXPECTED_RELEASE_OBJECTS[cluster], ...sopsSecretIds(cluster)].sort());
  });

  test(
    // The CRDs themselves come verbatim from the pinned flux CLI (checked below), and kubeconform
    // has no schema for CustomResourceDefinition, so they are left out here.
    "every other resource passes kubeconform -strict (Flux kinds against the pinned CRDs)",
    () => {
      const resources = tree.manifests.filter((m) => m.kind !== "CustomResourceDefinition").map(asDecrypted);
      const result = kubeconform(resources);
      expect(`${result.stdout}${result.stderr}`).toMatch(/Invalid: 0, Errors: 0, Skipped: 0/);
      expect(result.code).toBe(0);
      // The check is not vacuous: a field the pinned OCIRepository CRD does not know fails.
      const broken = structuredClone(resources).map((m) =>
        m.kind === "OCIRepository" ? { ...m, spec: { ...spec(m), verify: { ...(spec(m).verify as YamlMap), keyless: true } } } : m,
      );
      expect(`${kubeconform(broken).stdout}`).toMatch(/Invalid: 1, Errors: 0, Skipped: 0/);
    },
    SLOW_TEST_MS,
  );

  test("every controller image is pinned by digest at the exported tag", () => {
    const expected = controllerImages(components).map((image) => `${image.name}:${image.tag}`);
    const deployed = only(cluster, "Deployment").flatMap((d) => {
      const pod = (spec(d).template as { spec: { containers: { image: string }[] } }).spec;
      return pod.containers.map((c) => c.image);
    });
    expect(deployed.length).toBe(FLUX_COMPONENTS.length);
    for (const image of deployed) {
      const [ref, digest] = image.split("@");
      expect(expected).toContain(ref ?? "");
      expect(digest).toMatch(/^sha256:[0-9a-f]{64}$/);
    }
  });

  test("the cluster reads main of this repository over SSH, only deploy/flux/, applies its own directory and decrypts SOPS", () => {
    const git = spec(one(cluster, "GitRepository", "flux-system"));
    expect(git.url).toBe(`ssh://git@github.com/${REPOSITORY}.git`);
    expect(git.ref).toEqual({ branch: "main" });
    expect(git.secretRef).toEqual({ name: "flux-system" });
    expect(String(git.ignore).trim().split("\n")).toEqual(["/*", "!/deploy/", "/deploy/*", "!/deploy/flux/"]);
    const sync = spec(one(cluster, "Kustomization", "flux-system"));
    expect(sync.path).toBe(`./${clusterDir(cluster)}`);
    expect(sync.prune).toBe(true);
    expect(sync.decryption).toEqual({ provider: "sops", secretRef: { name: "sops-age" } });
  });

  test("no notification or image automation controllers: the cluster only pulls", () => {
    const deployments = only(cluster, "Deployment").map((d) => d.metadata.name).sort();
    expect(deployments).toEqual([...FLUX_COMPONENTS].sort());
  });

  const chartSourceText = read(chartSourceFile(cluster));

  test("chart.yaml is exactly what release.yml writes for its version (scripts/release/flux.ts)", () => {
    expect(() => parseChartSource(chartSourceText, REPOSITORY, CHART_REPOSITORY, chartSourceFile(cluster))).not.toThrow();
  });

  test("the chart is pulled by digest with the pull Secret and only with release.yml's signature for that tag", () => {
    const { version, digest } = parseChartSource(chartSourceText, REPOSITORY, CHART_REPOSITORY);
    const source = spec(one(cluster, "OCIRepository", "trustiot"));
    expect(source.url).toBe(`oci://${CHART_REPOSITORY}`);
    expect(source.ref).toEqual({ tag: version, digest });
    expect(source.secretRef).toEqual({ name: GHCR_PULL_NAME });
    expect(source.layerSelector).toEqual({ mediaType: HELM_CHART_LAYER, operation: "copy" });
    expect(source.insecure).toBeUndefined();
    expect(source.verify).toEqual({
      provider: "cosign",
      matchOIDCIdentity: [
        { issuer: exactPattern(GITHUB_OIDC_ISSUER), subject: exactPattern(signingIdentity(REPOSITORY, `v${version}`)) },
      ],
    });
    const subject = new RegExp((source.verify as { matchOIDCIdentity: { subject: string }[] }).matchOIDCIdentity[0]!.subject);
    expect(subject.test(signingIdentity(REPOSITORY, `v${version}`))).toBe(true);
    // Another tag, another workflow or another repository is not accepted.
    expect(subject.test(signingIdentity(REPOSITORY, "v9.9.9"))).toBe(false);
    expect(subject.test(signingIdentity(REPOSITORY, `v${version}`).replace("release.yml", "ci.yml"))).toBe(false);
    expect(subject.test(signingIdentity("Laysi/other", `v${version}`))).toBe(false);
  });

  test("a hand edit of chart.yaml is rejected", () => {
    const edits = [
      chartSourceText.replace(/  verify:\n(    .*\n|      .*\n|        .*\n)+/, ""),
      chartSourceText.replace(/subject: '.*'/, "subject: '.*'"),
      chartSourceText.replace(/    digest: .*\n/, ""),
      chartSourceText.replace("interval: 10m", "interval: 1h"),
    ];
    for (const edited of edits) {
      expect(edited).not.toBe(chartSourceText);
      expect(() => parseChartSource(edited, REPOSITORY, CHART_REPOSITORY)).toThrow(/edited by hand|not a/);
    }
  });

  test("no source that could skip the signature check: no HelmRepository, Bucket or other OCIRepository", () => {
    expect(only(cluster, "HelmRepository")).toEqual([]);
    expect(only(cluster, "Bucket")).toEqual([]);
    expect(only(cluster, "HelmChart")).toEqual([]);
    expect(only(cluster, "OCIRepository").map((m) => m.metadata.name)).toEqual(["trustiot"]);
    expect(only(cluster, "GitRepository").map((m) => m.metadata.name)).toEqual(["flux-system"]);
  });

  test("the values copy comes from the deployed release", () => {
    const { version } = parseChartSource(chartSourceText, REPOSITORY, CHART_REPOSITORY);
    expect(parseShapeValues(read(shapeValuesFile(cluster)), shapeValuesFile(cluster)).version).toBe(version);
  });

  test("every SOPS file in the release directory is applied, and nothing applied is missing", () => {
    const kustomization = Bun.YAML.parse(read(`${appDir(cluster)}/kustomization.yaml`)) as { resources: string[] };
    for (const file of sopsFiles(cluster)) expect(kustomization.resources).toContain(file);
    for (const resource of kustomization.resources) expect(existsSync(join(REPO_ROOT, appDir(cluster), resource))).toBe(true);
  });

  test("every SOPS file is a Secret in the release namespace, encrypted for exactly the recipients .sops.yaml names for this cluster", () => {
    const recipients = [...ruleFor(cluster).age].sort();
    for (const file of sopsFiles(cluster)) {
      const docs = parseAll(read(`${appDir(cluster)}/${file}`));
      expect(docs.length).toBe(1);
      const secret = docs[0]!;
      expect(secret.kind).toBe("Secret");
      expect(secret.metadata.namespace).toBe(APP_NAMESPACE);
      expect(file).toBe(`${secret.metadata.name}.sops.yaml`);
      const used = ((secret.sops as { age?: { recipient: string }[] } | undefined)?.age ?? []).map((r) => r.recipient).sort();
      expect(used).toEqual(recipients);
    }
  });

  test("every SOPS file is a Secret this cluster may hold, of its type, and keeps the SOPS rules", () => {
    // SOPS_SECRETS (scripts/flux/lib.ts) and sopsDocumentProblems (scripts/ci/check-secret-handling.ts).
    for (const file of sopsFiles(cluster)) {
      expect({ file, problems: sopsFileProblems(cluster, file, read(`${appDir(cluster)}/${file}`)) }).toEqual({ file, problems: [] });
    }
  });
});

describe("what a SOPS file may be (Laysi/trustiot-v2#464)", () => {
  // A SOPS file in the shape sops writes for a Secret the scripts render; nothing here is a secret.
  const opaque = (name: string) => fakeSops(renderOpaqueSecret({ namespace: APP_NAMESPACE, name, key: "value", value: "fixture-value", format: "token" }));
  const pull = () => fakeSops(renderPullSecret(APP_NAMESPACE, "ghcr.io", "trustiot-bot", "fixture-token"));
  const fileOf = (name: string) => (name === GHCR_PULL_NAME ? pull() : opaque(name));
  const metadataLine = `  namespace: ${APP_NAMESPACE}\n`;

  test("every Secret SOPS_SECRETS lists passes as sops writes it", () => {
    for (const cluster of CLUSTERS) {
      for (const name of Object.keys(SOPS_SECRETS[cluster])) {
        expect({ cluster, name, problems: sopsFileProblems(cluster, `${name}.sops.yaml`, fileOf(name)) }).toEqual({ cluster, name, problems: [] });
      }
    }
  });

  test("refuses a name the chart reserves, the wrong type, a wider sops scope, an encrypted label and a Helm owner", () => {
    const cases: [string, ClusterName, string, string, RegExp][] = [
      // The chart generates this Secret in the cluster and nobody sets it (deploy/chart, _identity.tpl):
      // from git, Helm would adopt it and keep its data, and internal identity tokens could be forged.
      ["the identity signing keys on saas", "saas", "trustiot-identity-signing-keys.sops.yaml", opaque("trustiot-identity-signing-keys"), /cluster saas holds no SOPS Secret trustiot-identity-signing-keys/],
      ["the identity signing keys on demo", "demo", "trustiot-identity-signing-keys.sops.yaml", opaque("trustiot-identity-signing-keys"), /cluster demo holds no SOPS Secret trustiot-identity-signing-keys/],
      ["ghcr-pull as Opaque", "demo", "ghcr-pull.sops.yaml", opaque(GHCR_PULL_NAME), /ghcr-pull of cluster demo has type kubernetes\.io\/dockerconfigjson, not Opaque/],
      ["a sops scope with annotations", "demo", "trustiot-smtp.sops.yaml", opaque("trustiot-smtp").replace(SOPS_SCOPE_LINE, "  encrypted_regex: ^(data|stringData|annotations)$\n"), /sops\.encrypted_regex must be exactly/],
      ["an encrypted label", "demo", "trustiot-smtp.sops.yaml", opaque("trustiot-smtp").replace(metadataLine, `${metadataLine}  labels:\n    owner: ${enc(77)}\n`), /metadata\.labels\.owner holds an encrypted value/],
      [
        "a Helm ownership annotation",
        "demo",
        "trustiot-smtp.sops.yaml",
        opaque("trustiot-smtp").replace(metadataLine, `${metadataLine}  annotations:\n    meta.helm.sh/release-name: trustiot\n    meta.helm.sh/release-namespace: trustiot\n`),
        /metadata\.annotations is not allowed on a SOPS Secret/,
      ],
      ["another cluster's Secret", "saas", "trustiot-smtp.sops.yaml", opaque("trustiot-smtp"), /cluster saas holds no SOPS Secret trustiot-smtp/],
      ["another namespace", "demo", "trustiot-smtp.sops.yaml", opaque("trustiot-smtp").replace(metadataLine, "  namespace: default\n"), /not in namespace trustiot/],
      ["a file not named after its Secret", "demo", "smtp.sops.yaml", opaque("trustiot-smtp"), /smtp\.sops\.yaml is not named after its Secret/],
      ["values in clear text", "demo", "trustiot-smtp.sops.yaml", renderOpaqueSecret({ namespace: APP_NAMESPACE, name: "trustiot-smtp", key: "value", value: "fixture-value", format: "token" }), /not a SOPS-encrypted Secret/],
    ];
    for (const [what, cluster, file, source, message] of cases) {
      const problems = sopsFileProblems(cluster, file, source);
      expect({ what, refused: problems.some((p) => message.test(p)) }).toEqual({ what, refused: true });
    }
  });
});

function kubeconform(resources: readonly YamlMap[]) {
  const schemas = mkdtempSync(join(tmpdir(), "trustiot-flux-schemas-"));
  try {
    writeCrdSchemas(components, schemas);
    const cache = join(tmpdir(), "trustiot-kubeconform-cache");
    mkdirSync(cache, { recursive: true });
    return run(
      [
        pinnedTool("kubeconform"),
        "-strict",
        "-summary",
        "-kubernetes-version",
        "1.36.0",
        "-cache",
        cache,
        "-schema-location",
        "default",
        "-schema-location",
        `${schemas}/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json`,
        "-",
      ],
      resources.map((m) => JSON.stringify(m)).join("\n---\n"),
    );
  } finally {
    rmSync(schemas, { recursive: true, force: true });
  }
}

/** Writes each CRD version's openAPIV3Schema as a kubeconform schema, strict like kubeconform's own. */
function writeCrdSchemas(text: string, dir: string): void {
  const strict = (schema: unknown): unknown => {
    if (Array.isArray(schema)) return schema.map(strict);
    if (!isMap(schema)) return schema;
    const out: YamlMap = {};
    for (const [key, value] of Object.entries(schema)) out[key] = strict(value);
    if (isMap(out.properties) && out.additionalProperties === undefined && out["x-kubernetes-preserve-unknown-fields"] !== true) {
      out.additionalProperties = false;
    }
    return out;
  };
  for (const crd of parseAll(text).filter((m) => m.kind === "CustomResourceDefinition")) {
    const crdSpec = spec(crd) as { group: string; names: { kind: string }; versions: { name: string; schema: { openAPIV3Schema: unknown } }[] };
    mkdirSync(join(dir, crdSpec.group), { recursive: true });
    for (const version of crdSpec.versions) {
      const file = join(dir, crdSpec.group, `${crdSpec.names.kind.toLowerCase()}_${version.name}.json`);
      writeFileSync(file, JSON.stringify(strict(version.schema.openAPIV3Schema)));
    }
  }
}

describe("Flux itself", () => {
  test(
    "gotk-components.yaml is what the pinned flux CLI exports (scripts/flux/components.ts)",
    () => {
      const result = run([pinnedTool("flux"), "install", "--export", `--components=${FLUX_COMPONENTS.join(",")}`]);
      expect(result.code).toBe(0);
      expect(components).toBe(result.stdout);
    },
    SLOW_TEST_MS,
  );

  test("the demo cluster runs exactly the SaaS cluster's Flux; only the directory it applies differs", () => {
    const flux = (cluster: ClusterName) =>
      trees[cluster].manifests
        .filter((m) => fluxIds.has(`${m.kind}/${m.metadata.name}`) || m.metadata.namespace === "flux-system")
        .map((m) => (m.kind === "Kustomization" && m.metadata.name === "flux-system" ? { ...m, spec: { ...spec(m), path: "<cluster>" } } : m));
    expect(flux("demo")).toEqual(flux("saas"));
    expect(flux("saas").length).toBeGreaterThan(FLUX_COMPONENTS.length);
  });
});

describe("the SaaS release", () => {
  test("the HelmRelease installs the chart into its namespace with the shape and instance values, in that order", () => {
    const release = one("saas", "HelmRelease", "trustiot");
    expect(release.metadata.namespace).toBe(APP_NAMESPACE);
    const releaseSpec = spec(release);
    expect(releaseSpec.chartRef).toEqual({ kind: "OCIRepository", name: "trustiot" });
    expect(releaseSpec.releaseName).toBe("trustiot");
    expect(releaseSpec.valuesFrom).toEqual([
      { kind: "ConfigMap", name: "trustiot-values", valuesKey: "values-saas.yaml" },
      { kind: "ConfigMap", name: "trustiot-values", valuesKey: "values-instance.yaml" },
    ]);
    const values = one("saas", "ConfigMap", "trustiot-values");
    expect(values.metadata.namespace).toBe(APP_NAMESPACE);
    expect(values.metadata.labels?.["reconcile.fluxcd.io/watch"]).toBe("Enabled");
    const data = values.data as Record<string, string>;
    expect(data["values-saas.yaml"]).toBe(read(shapeValuesFile("saas")));
    expect(data["values-instance.yaml"]).toBe(read(`${APP_DIR}/values-instance.yaml`));
    const namespace = one("saas", "Namespace", APP_NAMESPACE);
    expect(namespace.metadata.labels?.["pod-security.kubernetes.io/enforce"]).toBe("restricted");
  });
});

describe("values-instance.yaml", () => {
  const instance = Bun.YAML.parse(read(`${APP_DIR}/values-instance.yaml`)) as YamlMap;
  // Decisions for whoever sets up the SaaS cluster (deploy/flux/README.md, "上線前要填的值").
  const PENDING = [
    "clickhouse.bundled.storage.size",
    "hosts.app",
    "hosts.gateway",
    "hosts.ota",
    "postgres.bundled.storage.size",
    "qdrant.external.url",
  ];

  const emptyFields = (value: unknown, path = ""): string[] =>
    isMap(value)
      ? Object.entries(value).flatMap(([key, child]) => emptyFields(child, path === "" ? key : `${path}.${key}`))
      : value === ""
        ? [path]
        : [];

  test("the only empty fields are the documented pending decisions", () => {
    expect(emptyFields(instance).sort()).toEqual(PENDING);
  });

  test(
    "with the pending fields filled in, the chart at HEAD accepts it and pulls with the pull Secret",
    () => {
      // Only the pending fields take the chart's example values, so a field the chart requires
      // and this file lacks fails here instead of on the cluster.
      const placeholders = Bun.YAML.parse(read("deploy/chart/tests/values/saas-instance.yaml")) as YamlMap;
      const at = (map: YamlMap, path: string): unknown => path.split(".").reduce<unknown>((v, key) => (isMap(v) ? v[key] : undefined), map);
      const filled = structuredClone(instance);
      for (const path of PENDING) {
        const keys = path.split(".");
        const parent = at(filled, keys.slice(0, -1).join("."));
        const example = at(placeholders, path);
        if (!isMap(parent) || example === undefined || example === "") throw new Error(`no example value for ${path}`);
        parent[keys.at(-1)!] = example;
      }
      const result = helmTemplate([filled], ["--set", "services.identity.enabled=true"]);
      expect(result.stderr).toBe("");
      expect(result.code).toBe(0);
      const identity = parseAll(result.stdout).find((m) => m.kind === "Deployment" && m.metadata.name === "trustiot-identity");
      const pod = (spec(identity!).template as { spec: { imagePullSecrets: { name: string }[] } }).spec;
      expect(pod.imagePullSecrets).toEqual([{ name: GHCR_PULL_NAME }]);
      // Laysi/trustiot-v2#403: the bundled ClickHouse's admin password is generated in the
      // cluster into the Secret the chart declares, so this directory needs no Secret for it.
      const objects = parseAll(result.stdout);
      const admin = objects.find((m) => m.kind === "Secret" && m.metadata.name === "trustiot-clickhouse-admin-password");
      expect(admin?.metadata.labels?.["trustiot.io/filled-by"]).toBe("migrate");
      const clickhouse = objects.find((m) => m.kind === "StatefulSet" && m.metadata.name === "trustiot-clickhouse");
      const clickhousePod = (spec(clickhouse!).template as { spec: { initContainers?: { name: string }[]; imagePullSecrets: { name: string }[] } }).spec;
      expect(clickhousePod.initContainers?.map((c) => c.name)).toEqual(["admin-password"]);
      expect(clickhousePod.imagePullSecrets).toEqual([{ name: GHCR_PULL_NAME }]);
    },
    SLOW_TEST_MS,
  );
});

/** `helm template` of the chart at HEAD with deploy/chart/values-saas.yaml and the given overlays, in order. */
function helmTemplate(overlays: readonly YamlMap[], extra: string[] = []) {
  const dir = mkdtempSync(join(tmpdir(), "trustiot-flux-values-"));
  try {
    const files = overlays.map((overlay, index) => {
      const file = join(dir, `values-${index}.yaml`);
      writeFileSync(file, JSON.stringify(overlay));
      return file;
    });
    return run([
      pinnedTool("helm"),
      "template",
      "trustiot",
      "deploy/chart",
      "--namespace",
      APP_NAMESPACE,
      "--values",
      "deploy/chart/values-saas.yaml",
      ...files.flatMap((file) => ["--values", file]),
      ...extra,
    ]);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

/**
 * What is wrong with the Secret entries of the demo HelmRelease's valuesFrom (empty when nothing).
 * The private values Secret is encrypted, so a reviewer never sees what it holds: each entry must
 * put one key into one allowed chart value (DEMO_PRIVATE_VALUES), verbatim (`literal: true`; without
 * it Flux reads the value with Helm's --set syntax, where a comma starts another path=value), and
 * no entry may merge a whole key as values.
 */
function privateValuesProblems(valuesFrom: unknown): string[] {
  if (!Array.isArray(valuesFrom)) return ["valuesFrom is not a list"];
  const problems: string[] = [];
  const set = new Set<string>();
  valuesFrom.forEach((entry, index) => {
    if (!isMap(entry) || entry.kind !== "Secret") return;
    const where = `valuesFrom[${index}]`;
    if (entry.name !== DEMO_VALUES_NAME) problems.push(`${where} reads Secret ${String(entry.name)}, not ${DEMO_VALUES_NAME}`);
    if (typeof entry.targetPath !== "string") {
      problems.push(`${where} has no targetPath, so its whole value would be merged as Helm values`);
      return;
    }
    const key = Object.keys(DEMO_PRIVATE_VALUES).find((k) => DEMO_PRIVATE_VALUES[k] === entry.targetPath);
    if (key === undefined) {
      problems.push(`${where} sets ${entry.targetPath}, which is not one of the private values`);
      return;
    }
    if (entry.valuesKey !== key) problems.push(`${where} sets ${entry.targetPath} from the key ${String(entry.valuesKey)}, not ${key}`);
    if (entry.literal !== true) problems.push(`${where} is not literal: true, so Flux would read the value with Helm's --set syntax`);
    if (entry.optional === true) problems.push(`${where} is optional`);
    if (set.has(entry.targetPath)) problems.push(`${where} sets ${entry.targetPath} a second time`);
    set.add(entry.targetPath);
  });
  const missing = Object.values(DEMO_PRIVATE_VALUES).filter((path) => !set.has(path));
  if (missing.length > 0) problems.push(`no entry sets ${missing.join(", ")}`);
  return problems;
}

/** File names, which the host pattern below also matches. */
const FILE_NAME = /\.(?:ya?ml|md|json|ts)$/;
/** Public services the demo overlay may name by host. */
const PUBLIC_SERVICES = ["smtp.resend.com"];
/** Anything that looks like a host name: two or more dot-separated labels. */
const HOST_LIKE = /[a-z0-9](?:[-a-z0-9]*[a-z0-9])?(?:\.[a-z0-9](?:[-a-z0-9]*[a-z0-9])?)+/gi;
const chartDefaults = Bun.YAML.parse(read("deploy/chart/values.yaml")) as YamlMap;

/** Whether a dotted word is a path of the chart's values, such as smtp.from in a comment. */
function isChartValuesPath(word: string): boolean {
  let at: unknown = chartDefaults;
  for (const label of word.split(".")) {
    if (!isMap(at) || !(label in at)) return false;
    at = at[label];
  }
  return true;
}

/**
 * The demo machine's Service range, k3s's default (deploy/flux/README.md, 「展示叢集」). The only IP
 * address the public overlay may hold is the ingress Service's fixed ClusterIP in its static band:
 * an address inside the cluster, never one of the demo machine's LAN, which could not share the
 * range with the cluster's Services.
 */
const DEMO_SERVICE_RANGE = "10.43.0.0/16";

/**
 * Host names, mail addresses and IP addresses in a public file of the demo environment (empty when
 * none): every host-like word, in values, URLs and comments alike, must be under .invalid or .test
 * (RFC 6761), a listed public service, a file name or a path of the chart's values; no `@` and no
 * IPv4 address other than `allowedAddresses`. Problems name the line and column only, so that the
 * CI log does not repeat what should not have been written.
 */
function hostProblems(text: string, allowedAddresses: readonly string[] = []): string[] {
  const problems: string[] = [];
  text.split("\n").forEach((line, index) => {
    const at = line.indexOf("@");
    if (at >= 0) problems.push(`line ${index + 1}, column ${at + 1}: "@", as in a mail address`);
    for (const match of line.matchAll(HOST_LIKE)) {
      const word = match[0].toLowerCase();
      const where = `line ${index + 1}, column ${(match.index ?? 0) + 1}`;
      if (/^\d+(?:\.\d+){3}$/.test(word)) {
        if (allowedAddresses.includes(word)) continue;
        problems.push(`${where}: an IP address`);
        continue;
      }
      // A version number such as 1.2 or v1.2: no top-level domain is all digits.
      if (!/[a-z]/.test(word.split(".").at(-1) ?? "")) continue;
      if (word.endsWith(".invalid") || word.endsWith(".test") || PUBLIC_SERVICES.includes(word) || FILE_NAME.test(word) || isChartValuesPath(word)) continue;
      problems.push(`${where}: a host name that is not under .invalid or .test and not a listed public service`);
    }
  });
  return problems;
}

describe("the demo release", () => {
  const DEMO_DIR = appDir("demo");
  const overlayText = read(`${DEMO_DIR}/values-demo.yaml`);
  const overlay = Bun.YAML.parse(overlayText) as YamlMap;
  // What trustiot-demo-values.sops.yaml holds, with reserved names (RFC 6761) in place of the real,
  // private ones. The sender has a space, <...@...> and a comma followed by another path=value,
  // which Helm's --set syntax would read as a second value.
  const privateExample: Record<string, string> = {
    "app-host": "app.demo.trustiot.test",
    "gateway-host": "gw.demo.trustiot.test",
    "ota-host": "ota.demo.trustiot.test",
    "smtp-from": "TrustIoT Demo <no-reply@mail.demo.trustiot.test>,serviceDefaults.image.registry=registry.evil.test",
  };
  /** --set-literal for each private value: what the HelmRelease's literal valuesFrom entries do. */
  const setLiteral = Object.entries(privateExample).flatMap(([key, value]) => ["--set-literal", `${DEMO_PRIVATE_VALUES[key]}=${value}`]);
  /** The same values as a values file, which Helm reads without any --set syntax. */
  const asValues: YamlMap = {};
  for (const [key, value] of Object.entries(privateExample)) {
    const [top, leaf] = (DEMO_PRIVATE_VALUES[key] ?? "").split(".") as [string, string];
    asValues[top] = { ...(isMap(asValues[top]) ? asValues[top] : {}), [leaf]: value };
  }
  const release = () => spec(one("demo", "HelmRelease", "trustiot"));

  test("the HelmRelease layers the shape, the demo overlay and then each private value on its own, in that order", () => {
    expect(one("demo", "HelmRelease", "trustiot").metadata.namespace).toBe(APP_NAMESPACE);
    const releaseSpec = release();
    expect(releaseSpec.chartRef).toEqual({ kind: "OCIRepository", name: "trustiot" });
    expect(releaseSpec.releaseName).toBe("trustiot");
    expect(releaseSpec.valuesFrom).toEqual([
      { kind: "ConfigMap", name: "trustiot-values", valuesKey: "values-saas.yaml" },
      { kind: "ConfigMap", name: "trustiot-values", valuesKey: "values-demo.yaml" },
      ...Object.entries(DEMO_PRIVATE_VALUES).map(([key, path]) => ({ kind: "Secret", name: DEMO_VALUES_NAME, valuesKey: key, targetPath: path, literal: true })),
    ]);
    // The SaaS release's remediation and timeout, so the demo exercises the same upgrade path.
    const saas = spec(one("saas", "HelmRelease", "trustiot"));
    const { valuesFrom: _demoValues, ...demoRest } = releaseSpec;
    const { valuesFrom: _saasValues, ...saasRest } = saas;
    expect(demoRest).toEqual(saasRest);
    const values = one("demo", "ConfigMap", "trustiot-values");
    expect(values.metadata.namespace).toBe(APP_NAMESPACE);
    expect(values.metadata.labels?.["reconcile.fluxcd.io/watch"]).toBe("Enabled");
    const data = values.data as Record<string, string>;
    expect(data["values-saas.yaml"]).toBe(read(shapeValuesFile("demo")));
    expect(data["values-demo.yaml"]).toBe(overlayText);
    expect(one("demo", "Namespace", APP_NAMESPACE)).toEqual(one("saas", "Namespace", APP_NAMESPACE));
  });

  test("every Secret it reads goes verbatim into one allowed chart value: targetPath, its own key, literal", () => {
    expect(privateValuesProblems(release().valuesFrom)).toEqual([]);
  });

  test("that check refuses an entry without literal or targetPath, and an entry for any other chart value", () => {
    const entries = release().valuesFrom as YamlMap[];
    const without = (key: string, field: string) =>
      entries.map((entry) => (entry.valuesKey === key ? Object.fromEntries(Object.entries(entry).filter(([name]) => name !== field)) : entry));
    expect(privateValuesProblems(without("smtp-from", "literal"))).toEqual([expect.stringMatching(/^valuesFrom\[5\] is not literal: true/)]);
    expect(privateValuesProblems(without("app-host", "targetPath"))).toEqual([
      expect.stringMatching(/^valuesFrom\[2\] has no targetPath/),
      "no entry sets hosts.app",
    ]);
    const extra = [...entries, { kind: "Secret", name: DEMO_VALUES_NAME, valuesKey: "smtp-from", targetPath: "services.identity.image.registry", literal: true }];
    expect(privateValuesProblems(extra)).toEqual([expect.stringMatching(/^valuesFrom\[6\] sets services\.identity\.image\.registry, which is not one of the private values/)]);
  });

  test("the generator writes the values Secret with exactly the four keys and the watch label", () => {
    const lines = Object.entries(privateExample).map(([key, value]) => `${key}=${value}\n`).join("");
    const secret = parseAll(renderDemoValuesSecret(lines))[0]!;
    expect(secret.metadata).toEqual({ name: DEMO_VALUES_NAME, namespace: APP_NAMESPACE, labels: { "reconcile.fluxcd.io/watch": "Enabled" } });
    expect(secret.stringData).toEqual(privateExample);
    expect(Object.keys(secret.stringData as YamlMap).sort()).toEqual(Object.keys(DEMO_PRIVATE_VALUES).sort());
  });

  test("the committed values Secret, once there, has exactly the four keys and the watch label", () => {
    const file = `${DEMO_VALUES_NAME}.sops.yaml`;
    if (!sopsFiles("demo").includes(file)) return;
    const secret = parseAll(read(`${DEMO_DIR}/${file}`))[0]!;
    expect(secret.metadata.labels).toEqual({ "reconcile.fluxcd.io/watch": "Enabled" });
    expect(secret.data).toBeUndefined();
    expect(Object.keys(secret.stringData as YamlMap).sort()).toEqual(Object.keys(DEMO_PRIVATE_VALUES).sort());
  });

  const band = staticBandOf(DEMO_SERVICE_RANGE)!;
  const clusterIpOf = (values: YamlMap) => (isMap(values.ingress) && isMap(values.ingress.tunnel) ? values.ingress.tunnel.clusterIP : undefined);
  /** The overlay's fixed ClusterIP, as the one address it may name, only while it is in the band. */
  const allowedAddressesOf = (values: YamlMap): string[] => {
    const address = clusterIpOf(values);
    if (typeof address !== "string" || !/^\d+(?:\.\d+){3}$/.test(address)) return [];
    const value = ipv4ToNumber(address);
    return value >= ipv4ToNumber(band.first) && value <= ipv4ToNumber(band.last) ? [address] : [];
  };
  const clusterIp = clusterIpOf(overlay);
  const allowedAddresses = allowedAddressesOf(overlay);

  test("the ingress Service's fixed ClusterIP is in the static band of the demo machine's Service range, clear of kubernetes and kube-dns", () => {
    expect(band).toEqual({ first: "10.43.0.1", last: "10.43.1.0" });
    expect(typeof clusterIp).toBe("string");
    expect(allowedAddresses).toEqual([clusterIp as string]);
    // k3s gives these two to the kubernetes and kube-dns Services.
    expect(["10.43.0.1", "10.43.0.10"]).not.toContain(clusterIp);
  });

  test("the public overlay names no private host, no mail address and no IP address but the fixed ClusterIP", () => {
    expect(overlay.hosts).toBeUndefined();
    expect(isMap(overlay.smtp) ? overlay.smtp.from : undefined).toBeUndefined();
    expect(hostProblems(overlayText, allowedAddresses)).toEqual([]);
  });

  test("the host check refuses any other host name, a mail address and an IP address", () => {
    const lines = overlayText.split("\n").length;
    const problems = (text: string) => hostProblems(text, allowedAddresses);
    expect(problems(`${overlayText}# see api.example.com\n`)).toEqual([`line ${lines}, column 7: a host name that is not under .invalid or .test and not a listed public service`]);
    expect(problems(overlayText.replace("https://qdrant.invalid:6333", "https://kb.example.com:6333"))).toHaveLength(1);
    expect(problems(overlayText.replace("host: smtp.resend.com", "host: smtp.example.com"))).toHaveLength(1);
    expect(problems(`${overlayText}# no-reply@x\n`)).toEqual([`line ${lines}, column 11: "@", as in a mail address`]);
    expect(problems(`${overlayText}# 192.0.2.10\n`)).toEqual([`line ${lines}, column 3: an IP address`]);
    // Another address in the Service range is no exception either.
    expect(problems(`${overlayText}# 10.43.0.81\n`)).toEqual([`line ${lines}, column 3: an IP address`]);
    // A fixed ClusterIP outside the band is no exception at all.
    const outside = overlayText.replace(`clusterIP: ${clusterIp as string}`, "clusterIP: 10.43.2.80");
    expect(outside).not.toBe(overlayText);
    expect(hostProblems(outside, allowedAddressesOf(Bun.YAML.parse(outside) as YamlMap))).toHaveLength(1);
  });

  test(
    "with the private values, the chart at HEAD accepts it: the services that exist, Resend's relay, no Sentry, no Qdrant",
    () => {
      const result = helmTemplate([overlay], setLiteral);
      expect(result.stderr).toBe("");
      expect(result.code).toBe(0);
      const objects = parseAll(result.stdout);
      const deployments = objects.filter((m) => m.kind === "Deployment").map((m) => m.metadata.name).sort();
      expect(deployments).toEqual(["trustiot-assets", "trustiot-identity", "trustiot-otel-collector"]);
      for (const name of ["trustiot-assets", "trustiot-identity"]) {
        const pod = (spec(objects.find((m) => m.kind === "Deployment" && m.metadata.name === name)!).template as { spec: { imagePullSecrets: { name: string }[] } }).spec;
        expect(pod.imagePullSecrets).toEqual([{ name: GHCR_PULL_NAME }]);
      }
      const platform = objects.find((m) => m.kind === "ConfigMap" && m.metadata.name === "trustiot-platform")?.data as Record<string, string>;
      expect(platform.TRUSTIOT_APP_HOST).toBe(privateExample["app-host"]);
      expect(platform.TRUSTIOT_SMTP_HOST).toBe("smtp.resend.com");
      expect(platform.TRUSTIOT_SMTP_PORT).toBe("587");
      expect(platform.TRUSTIOT_SMTP_SECURITY).toBe("starttls");
      expect(platform.SENTRY_ENVIRONMENT).toBeUndefined();
      expect(new URL(platform.TRUSTIOT_QDRANT_URL ?? "").hostname.endsWith(".invalid")).toBe(true);
      expect(objects.some((m) => /qdrant/i.test(m.metadata.name))).toBe(false);
      // The overlay that Istio applies to the ingress Service it generates: the fixed ClusterIP.
      const options = objects.find((m) => m.kind === "ConfigMap" && m.metadata.name === "trustiot-ingress-options")?.data as Record<string, string>;
      expect((Bun.YAML.parse(options.service ?? "") as { spec: YamlMap }).spec).toEqual({ type: "ClusterIP", clusterIP: clusterIp });
    },
    SLOW_TEST_MS,
  );

  test(
    "the README's values that turn telemetry-query on render with the overlay: the service, its cookie route on the private app host, no Secret to add (Laysi/trustiot-v2#539)",
    () => {
      // The YAML block under 「打開 telemetry-query」 in deploy/flux/README.md, as an operator copies it.
      const readme = read("deploy/flux/README.md");
      const section = readme.slice(readme.indexOf("- **打開 telemetry-query**"));
      const block = /\n {2}```yaml\n([\s\S]*?)\n {2}```\n/.exec(section)?.[1];
      expect(block).toBeDefined();
      const values = Bun.YAML.parse((block ?? "").replaceAll(/^ {2}/gm, "")) as YamlMap;
      expect(Object.keys(values).sort()).toEqual(["ingress", "services"]);
      // The overlay sets no route yet; once it does, the README's list must keep those (helm
      // replaces a list as a whole), which this test cannot know, so it stops here.
      expect(isMap(overlay.ingress) ? overlay.ingress.routes : undefined).toBeUndefined();
      const result = helmTemplate([overlay, values], setLiteral);
      expect(result.stderr).toBe("");
      expect(result.code).toBe(0);
      const objects = parseAll(result.stdout);
      const deployment = objects.find((m) => m.kind === "Deployment" && m.metadata.name === "trustiot-telemetry-query");
      expect(deployment).toBeDefined();
      const pod = (spec(deployment!).template as { spec: { imagePullSecrets: { name: string }[] } }).spec;
      expect(pod.imagePullSecrets).toEqual([{ name: GHCR_PULL_NAME }]);
      const routes = objects.filter((m) => m.kind === "HTTPRoute").map((m) => JSON.stringify(m.spec));
      expect(
        routes.some((r) => r.includes('"value":"/api/telemetry"') && r.includes('"name":"trustiot-telemetry-query"') && r.includes(privateExample["app-host"]!)),
      ).toBe(true);
      // The passwords are generated in the cluster: the only Secret it reads is the one the chart
      // declares for the migration Job to fill.
      const secretRefs = new Set(
        [...JSON.stringify(spec(deployment!)).matchAll(/"secretKeyRef":\{"key":"[^"]+","name":"([^"]+)"\}/g)].map((m) => m[1]),
      );
      expect([...secretRefs]).toEqual(["trustiot-telemetry-query-password"]);
      expect(objects.some((m) => m.kind === "Secret" && m.metadata.name === "trustiot-telemetry-query-password")).toBe(true);
    },
    SLOW_TEST_MS,
  );

  test(
    "the private values arrive verbatim and change nothing else; through --set syntax the sender would set another value",
    () => {
      const literal = helmTemplate([overlay], setLiteral);
      const values = helmTemplate([overlay, asValues]);
      expect(literal.code).toBe(0);
      expect(values.code).toBe(0);
      const platform = parseAll(literal.stdout).find((m) => m.kind === "ConfigMap" && m.metadata.name === "trustiot-platform")?.data as Record<string, string>;
      expect(platform.TRUSTIOT_SMTP_FROM).toBe(privateExample["smtp-from"]);
      expect(platform.TRUSTIOT_APP_HOST).toBe(privateExample["app-host"]);
      // The whole render, every object and field, is what the values file gives.
      expect(literal.stdout).toBe(values.stdout);
      // The control: without literal, the comma in the sender starts a second value that moves every
      // service's image to another registry. This is what `literal: true` prevents.
      const parsed = helmTemplate([overlay], setLiteral.map((arg) => (arg === "--set-literal" ? "--set" : arg)));
      expect(parsed.code).toBe(0);
      const identity = parseAll(parsed.stdout).find((m) => m.kind === "Deployment" && m.metadata.name === "trustiot-identity")!;
      const image = (spec(identity).template as { spec: { containers: { image: string }[] } }).spec.containers[0]?.image ?? "";
      expect(image.startsWith("registry.evil.test/")).toBe(true);
      expect(literal.stdout).not.toContain("image: registry.evil.test/");
    },
    SLOW_TEST_MS,
  );

  test(
    "without the private values the chart refuses to install, so nothing is installed half-way",
    () => {
      const result = helmTemplate([overlay]);
      expect(result.code).not.toBe(0);
      expect(result.stderr).toMatch(/hosts/);
    },
    SLOW_TEST_MS,
  );
});

// The MQTT broker (Laysi/trustiot-v2#84) is off in the chart's shapes. Its server certificate now
// comes from the chart's own CronJob (Laysi/trustiot-v2#535), but turning the broker on in a
// cluster is a change of its own, which updates this test; until then every values file and value
// a cluster's release reads must leave it off.
describe.each([...CLUSTERS])("the MQTT broker stays off in the %s cluster", (cluster) => {
  test("no values file of the release sets mqttBroker, and no private value targets it", () => {
    const data = one(cluster, "ConfigMap", "trustiot-values").data as Record<string, string>;
    expect(Object.keys(data).length).toBeGreaterThan(0);
    for (const [file, text] of Object.entries(data)) {
      const values = Bun.YAML.parse(text);
      expect({ file, mqttBroker: isMap(values) ? values.mqttBroker : undefined }).toEqual({ file, mqttBroker: undefined });
    }
    const release = spec(one(cluster, "HelmRelease", "trustiot"));
    expect(release.values).toBeUndefined();
    for (const entry of (release.valuesFrom ?? []) as YamlMap[]) {
      expect(String(entry.targetPath ?? "")).not.toStartWith("mqttBroker");
    }
  });
});

interface SopsRule {
  path_regex: string;
  encrypted_regex: string;
  age: string[];
}

const sopsText = read(".sops.yaml");
const sopsConfig = Bun.YAML.parse(sopsText) as { creation_rules: SopsRule[] };

function ruleFor(cluster: ClusterName): SopsRule {
  const rule = sopsConfig.creation_rules.find((r) => new RegExp(r.path_regex).test(`${appDir(cluster)}/${GHCR_PULL_NAME}.sops.yaml`));
  if (rule === undefined) throw new Error(`.sops.yaml has no rule for ${cluster}`);
  return rule;
}

describe(".sops.yaml", () => {
  test("one rule per cluster: Secrets under its directory named *.sops.yaml, data and stringData only", () => {
    expect(sopsConfig.creation_rules.length).toBe(CLUSTERS.length);
    for (const cluster of CLUSTERS) {
      const rule = ruleFor(cluster);
      expect(rule.encrypted_regex).toBe("^(data|stringData)$");
      const path = new RegExp(String(rule.path_regex));
      const dir = appDir(cluster);
      expect(path.test(`${dir}/${GHCR_PULL_NAME}.sops.yaml`)).toBe(true);
      expect(path.test(`/home/someone/trustiot-v2/${dir}/${GHCR_PULL_NAME}.sops.yaml`)).toBe(true);
      expect(path.test(`${dir}/values-instance.yaml`)).toBe(false);
      expect(path.test(`deploy/chart/${GHCR_PULL_NAME}.sops.yaml`)).toBe(false);
      expect(path.test(`${GHCR_PULL_NAME}.sops.yaml`)).toBe(false);
      // A cluster's rule never matches another cluster's files: each one decrypts only its own.
      for (const other of CLUSTERS.filter((c) => c !== cluster)) {
        expect(path.test(`${appDir(other)}/${GHCR_PULL_NAME}.sops.yaml`)).toBe(false);
      }
    }
  });

  test("every recipient is an age public key on a line of its own with its role", () => {
    // sopsRuleTexts throws on a recipient written any other way, so no key is without a role.
    const texts = sopsRuleTexts(sopsText);
    expect(texts.map((rule) => rule.pathRegex)).toEqual(sopsConfig.creation_rules.map((rule) => rule.path_regex));
    expect(texts.map((rule) => rule.recipients.map((r) => r.key))).toEqual(sopsConfig.creation_rules.map((rule) => rule.age));
  });

  test("each rule is empty or holds its own cluster's key and every operator key, and no other cluster's key", () => {
    expect(sopsRecipientProblems(sopsRuleTexts(sopsText))).toEqual([]);
  });
});

describe("github-known-hosts", () => {
  // https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/githubs-ssh-key-fingerprints
  const PUBLISHED = new Map([
    ["ecdsa-sha2-nistp256", "SHA256:p2QAMXNIC1TJYWeIOttrVc98/R1BUFWu3/LiyKgUfQM"],
    ["ssh-ed25519", "SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU"],
    ["ssh-rsa", "SHA256:uNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s"],
  ]);

  test("holds exactly GitHub's published host keys", () => {
    const lines = read("deploy/flux/github-known-hosts")
      .split("\n")
      .filter((line) => line !== "" && !line.startsWith("#"));
    const found = new Map(
      lines.map((line) => {
        const [host, type, key] = line.split(" ");
        expect(host).toBe("github.com");
        const fingerprint = `SHA256:${new Bun.CryptoHasher("sha256").update(Buffer.from(key ?? "", "base64")).digest("base64").replace(/=+$/, "")}`;
        return [type ?? "", fingerprint];
      }),
    );
    expect(found).toEqual(PUBLISHED);
  });
});
