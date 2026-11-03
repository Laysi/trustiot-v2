// Tests of the GitOps trees in deploy/flux/clusters: the SaaS cluster and the demo environment
// (deploy/flux/README.md). Run from the repository root:
//   bun test deploy/flux
// Needs kubectl (kustomize), kubeconform, helm and flux from .prototools; kubeconform downloads
// the Kubernetes schemas on first use. The Flux resources are validated against the CRDs in
// gotk-components.yaml, so the check follows the pinned Flux version without network access.
import { describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { isIPv6 } from "node:net";
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

/** Digit groups joined by dots, found anywhere: also glued to letters (`a192.0.2.10`), which HOST_LIKE reads as one word. */
const DOTTED_NUMBER = /\d+(?:\.\d+)+/g;
/** Keys whose value may be a three-part version number. */
const VERSION_KEYS = ["version", "tag", "appVersion"];
/** A whole version value: 1.2.3, v1.2.3, 0.0.0-rc.11 or 1.2.3+build.4. */
const VERSION_VALUE = /^v?\d+\.\d+\.\d+(?:[-+][0-9a-z.-]+)?$/i;
/** A `key: value` line with one word as the value: group 1 runs up to the value (after any quote), group 3 is the key, group 5 the value. */
const KEY_VALUE_LINE = /^(\s*(?:-\s+)?(["']?)([A-Za-z0-9_-]+)\2\s*:\s*(["']?))([^"'\s#]+)\4\s*(?:#.*)?$/;
/**
 * Keys whose value names a host. There, and in a URL's authority (after `//` or `@`) or before a
 * port (`:6333`), a two-part number such as 127.1 is an abbreviated IPv4 address, which URL parsers
 * and inet_aton read as one; elsewhere a two-part number (a ratio, a version) is allowed.
 */
const ADDRESS_KEY = /(?:host|address|addr|ip|endpoint|server|url)(?:e?s)?$/i;

/** The value of a `key: value` piece: its [start, end) span and its key. */
type KeyedValue = { start: number; end: number; key: string };
/**
 * The decimal integers counted as IPv4 addresses (`3325256714` is 198.51.100.10): from 2^24, which is
 * 1.0.0.0, to 2^32 - 1. Below 2^24 is 0.0.0.0/8, "this network" (RFC 1122), never a host's address,
 * and the ports, sizes and counts a values file holds stay far below it; from 2^24 up, any integer
 * names a host. A Unix time in seconds is in the range too and is refused: this file has no reason
 * to hold one.
 */
const INTEGER_ADDRESSES = { first: 2 ** 24, last: 2 ** 32 - 1 };
/** The one value that may hold an allowed address. */
const CLUSTER_IP_PATH = "ingress.tunnel.clusterIP";
/** A line that is only the fixed ClusterIP's key and value, the one place an allowed address may be written. */
const CLUSTER_IP_LINE = /^\s+clusterIP:\s*(["']?)[\d.]+\1\s*$/;
/** A block mapping key at the start of a line: group 1 is the indentation, group 2 the key. */
const BLOCK_KEY = /^( *)([A-Za-z0-9_-]+):(?:\s|$)/;
/** Runs of hex digits, colons and dots, where any IPv6 address the text writes lies. */
const IPV6_RUN = /[0-9a-f:.]+/gi;

/** Whether a decimal integer, or a number YAML decoded (`0xC633640A`), is in the IPv4 address range. */
function isIntegerAddress(value: number): boolean {
  return Number.isInteger(value) && value >= INTEGER_ADDRESSES.first && value <= INTEGER_ADDRESSES.last;
}

/**
 * Whether a run of hex digits, colons and dots holds an IPv6 address. Hex letters or digits glued to
 * it become part of its first or last group (`ab2001:db8::7`), so a group longer than four is also
 * tried with only its last (first) one to four characters; a last group holding an embedded IPv4
 * address (`::ffff:192.0.2.10`) is taken whole. Whole groups are never dropped, so a fingerprint of
 * colon-separated bytes, which has more than eight groups, is no address. The run `::` alone (as in
 * a Rust path such as `Key::new`) is the unspecified address and is refused too. A single colon
 * before or after the address (`fd00::1: the gateway`, `proxy:2001:db8::7`) is also tried without.
 */
function holdsIpv6(run: string): boolean {
  const trimmed = run.replace(/^\.+|\.+$/g, "");
  const variants = new Set([trimmed, trimmed.replace(/^:(?!:)/, ""), trimmed.replace(/(?<!:):$/, ""), trimmed.replace(/^:(?!:)/, "").replace(/(?<!:):$/, "")]);
  return [...variants].some(holdsIpv6Groups);
}

/** holdsIpv6 for one candidate, trimming only its first and last groups. */
function holdsIpv6Groups(candidate: string): boolean {
  const groups = candidate.split(":");
  if (groups.length < 3) return false;
  const first = groups[0] as string;
  const last = groups.at(-1) as string;
  const firsts = first.length > 4 ? [1, 2, 3, 4].map((n) => first.slice(-n)) : [first];
  const lasts = last.length > 4 && !last.includes(".") ? [1, 2, 3, 4].map((n) => last.slice(0, n)) : [last];
  const middle = groups.slice(1, -1);
  return firsts.some((f) => lasts.some((l) => isIPv6([f, ...middle, l].join(":"))));
}

type Finding = { column: number; problem: string };
const HOST_PROBLEM = "a host name that is not under .invalid or .test and not a listed public service";

/**
 * What one line of the text, or one string YAML decoded, must not hold, with 1-based columns.
 * `allowedAddresses` are the IP addresses this piece may hold (callers pass them only for the value
 * of ingress.tunnel.clusterIP). `keyed` is the span of the piece that is the whole value of a key,
 * and that key: in a version key's value a three-part number is a version, in a host key's value a
 * two-part number is an address.
 */
function pieceFindings(text: string, allowedAddresses: readonly string[], keyed?: KeyedValue): Finding[] {
  const inKeyedValue = (start: number, end: number) => keyed !== undefined && start >= keyed.start && end <= keyed.end;
  const keyedValue = keyed === undefined ? "" : text.slice(keyed.start, keyed.end);
  const findings: Finding[] = [];
  const at = text.indexOf("@");
  if (at >= 0) findings.push({ column: at + 1, problem: '"@", as in a mail address' });
  for (const match of text.matchAll(HOST_LIKE)) {
    const word = match[0].toLowerCase();
    // No top-level domain is all digits: dotted numbers are checked below.
    if (!/[a-z]/.test(word.split(".").at(-1) ?? "")) continue;
    if (word.endsWith(".invalid") || word.endsWith(".test") || PUBLIC_SERVICES.includes(word) || FILE_NAME.test(word) || isChartValuesPath(word)) continue;
    findings.push({ column: (match.index ?? 0) + 1, problem: HOST_PROBLEM });
  }
  for (const match of text.matchAll(DOTTED_NUMBER)) {
    const start = match.index ?? 0;
    const parts = match[0].split(".").length;
    if (parts >= 4) {
      // The allowed address only as a word of its own, not glued to letters (`a10.43.0.80`).
      const glued = /[a-z0-9_]/i.test(text[start - 1] ?? "") || /[a-z0-9_]/i.test(text[start + match[0].length] ?? "");
      if (parts === 4 && !glued && allowedAddresses.includes(match[0])) continue;
      findings.push({ column: start + 1, problem: "an IP address" });
    } else if (parts === 3) {
      // A version only right after a `v` that ends no other word (v1.2.3, not dev1.2.3 or x_v1.2.3), or as the
      // whole value of a version key. A network prefix such as 198.51.100 anywhere else is refused.
      const vPrefix = text[start - 1] === "v" && !/[a-z0-9_]/i.test(text[start - 2] ?? "");
      const inVersionValue =
        keyed !== undefined && VERSION_KEYS.includes(keyed.key) && VERSION_VALUE.test(keyedValue) && inKeyedValue(start, start + match[0].length);
      if (vPrefix || inVersionValue) continue;
      findings.push({ column: start + 1, problem: "a three-part number outside a version, as in a network prefix" });
    } else {
      const end = start + match[0].length;
      const inUrl = /(?:\/\/|@)$/.test(text.slice(0, start)) || /^:\d/.test(text.slice(end));
      const inHostValue = keyed !== undefined && ADDRESS_KEY.test(keyed.key) && inKeyedValue(start, end);
      if (inUrl || inHostValue) findings.push({ column: start + 1, problem: "an abbreviated IP address, a two-part number as a host" });
    }
  }
  for (const match of text.matchAll(IPV6_RUN)) {
    if (holdsIpv6(match[0])) findings.push({ column: (match.index ?? 0) + 1, problem: "an IPv6 address" });
  }
  // Digits inside a long hex string (a key id, a digest, a commit) are part of a hash, not an
  // integer: about one random SHA-256 in four holds 8 to 10 digits in a row that are in range.
  // Sixteen hex characters are more than any glued integer address (`a3325256714`) needs.
  const hashes = [...text.matchAll(/[0-9a-f]{16,}/gi)].map((m) => [m.index ?? 0, (m.index ?? 0) + m[0].length] as const);
  for (const match of text.matchAll(/\d+/g)) {
    const start = match.index ?? 0;
    if (hashes.some(([from, to]) => start >= from && start + match[0].length <= to)) continue;
    if (isIntegerAddress(Number(match[0]))) findings.push({ column: (match.index ?? 0) + 1, problem: "a decimal integer in the IPv4 address range" });
  }
  return findings.sort((a, b) => a.column - b.column);
}

/**
 * The same checks on every string (keys too) and number YAML decodes from the text: escapes such as
 * `\x2e` or `\u002e` in a double-quoted string, and integers written as `0x...` or `0o...`, show what
 * they hold only once decoded. A problem names the path to the string. Only keys that make a path
 * of the chart's values (public, such as ingress.tunnel) are named; any other key, which could be a
 * private name itself, is named by its position (`<key 3>`). An alias is checked at each path it
 * appears under; one that refers to a list or map it is inside (`&x [*x]`) ends the walk there.
 */
function decodedProblems(
  value: unknown,
  allowedAddresses: readonly string[],
  path = "",
  chartPath: string | undefined = "",
  parentKey?: string,
  ancestors = new Set<object>(),
): string[] {
  const where = path === "" ? "the top level" : path;
  if (typeof value === "string") {
    const keyed = parentKey === undefined ? undefined : { start: 0, end: value.length, key: parentKey };
    // An allowed address only as the whole value of ingress.tunnel.clusterIP.
    const allowed = chartPath === CLUSTER_IP_PATH && allowedAddresses.includes(value) ? allowedAddresses : [];
    return pieceFindings(value, allowed, keyed).map((f) => `the string YAML decodes at ${where}: ${f.problem}`);
  }
  if (typeof value === "number") {
    if (isIntegerAddress(value)) return [`the number YAML decodes at ${where}: a number in the IPv4 address range`];
    // A host key's value 10.1, unquoted, is a number, which a template prints back as the host 10.1.
    const asHost = parentKey !== undefined && ADDRESS_KEY.test(parentKey) && /^\d+\.\d+$/.test(String(value));
    return asHost ? [`the number YAML decodes at ${where}: an abbreviated IP address, a two-part number as a host`] : [];
  }
  if (typeof value !== "object" || value === null || ancestors.has(value)) return [];
  ancestors.add(value);
  try {
    return decodedCollectionProblems(value, allowedAddresses, path, chartPath, parentKey, ancestors);
  } finally {
    ancestors.delete(value);
  }
}

/**
 * decodedProblems for a list or map, each of whose items is walked with `ancestors`. A list's items
 * are values of the list's key (`hosts: [...]`), so they keep it as their key.
 */
function decodedCollectionProblems(
  value: object,
  allowedAddresses: readonly string[],
  path: string,
  chartPath: string | undefined,
  parentKey: string | undefined,
  ancestors: Set<object>,
): string[] {
  if (Array.isArray(value)) return value.flatMap((item, index) => decodedProblems(item, allowedAddresses, `${path}[${index}]`, undefined, parentKey, ancestors));
  if (!isMap(value)) return [];
  return Object.entries(value).flatMap(([key, item], index) => {
    const keyFindings = pieceFindings(key, []);
    const keyChartPath = chartPath === undefined ? undefined : chartPath === "" ? key : `${chartPath}.${key}`;
    const named = keyFindings.length === 0 && keyChartPath !== undefined && /^[\w-]+$/.test(key) && isChartValuesPath(keyChartPath);
    const label = named ? key : `<key ${index + 1}>`;
    const keyPath = path === "" ? label : `${path}.${label}`;
    return [
      ...keyFindings.map((f) => `the key YAML decodes at ${keyPath}: ${f.problem}`),
      ...decodedProblems(item, allowedAddresses, keyPath, named ? keyChartPath : undefined, key, ancestors),
    ];
  });
}

/**
 * Where a line opens a YAML tag (`!!binary`, `!<tag:yaml.org,2002:binary>`, `!local`); undefined
 * when it opens none. A tag changes what the value means (base64 for !!binary) in a way the decoded
 * check does not follow, and a values file has no use for one. A tag can only start a node: at the
 * start of the line (after the indentation, a document start `---` and any `- ` or `? `), after a
 * mapping's `: `, or inside a flow collection after `[`, `{` or `,`, each also after an anchor
 * (`&name `), the node's other property. A `!` anywhere else, in a quoted string, in a comment or in
 * the lines of a block scalar (`inBlockScalar`) is text. `state` carries an open flow collection or
 * quoted string from one line to the next.
 */
function yamlTagIndex(line: string, inBlockScalar: boolean, state: { flowDepth: number; quote?: string }): number | undefined {
  if (inBlockScalar) return undefined;
  for (let i = 0; i < line.length; i++) {
    const c = line[i] as string;
    if (state.quote !== undefined) {
      if (c === "\\" && state.quote === '"') i++;
      else if (c === state.quote) state.quote = undefined;
      continue;
    }
    const text = line.slice(0, i);
    if (c === "#" && (i === 0 || /\s$/.test(text))) return undefined;
    // A node's start, also right after its anchor.
    const before = text.replace(/&[^\s[\]{},]+\s+$/, "");
    const nodeStart =
      /^\s*(?:---\s+)?(?:[-?]\s+)*$/.test(before) || /:\s+$/.test(before) || (state.flowDepth > 0 && /(?:^|[[{,])\s*$/.test(before));
    if (c === "]" || c === "}") state.flowDepth = Math.max(0, state.flowDepth - 1);
    if (!nodeStart) continue;
    if (c === "[" || c === "{") state.flowDepth++;
    else if (c === '"' || c === "'") state.quote = c;
    else if (c === "!") return i;
  }
  return undefined;
}

/**
 * A line that opens a block scalar (`key: |`, `- >-`, `- key: |`): the lines indented deeper after it
 * are its text. Only a plain or quoted key of letters, digits and `_./-` counts, never a comment
 * (`# note: |`): a line this misses is checked as ordinary content, which can only refuse more.
 */
const BLOCK_SCALAR_START = /^ *(?:(?:- +)+(?:(["']?)[\w./-]+\1: +)?|(["']?)[\w./-]+\2: +)[|>][-+0-9]*(?: +#.*)? *$/;

/**
 * Host names, mail addresses and IP addresses in a public file of the demo environment (empty when
 * none). In values, URLs and comments alike, every host-like word must be under .invalid or .test
 * (RFC 6761), a listed public service, a file name or a path of the chart's values; there must be no
 * `@` and no IP address but one of `allowedAddresses` as the value of ingress.tunnel.clusterIP: no
 * dotted IPv4 address (also glued to letters, or with more than four parts), no three-part number
 * outside a version (a network prefix), no two-part number as a host (127.1), no IPv6 address, no decimal integer in the IPv4 range and no
 * YAML tag (`!!binary`, `!<...>`). The text is checked line by line as written, where only the first
 * line that is nothing but `clusterIP: <address>` under the block keys `ingress:` and `tunnel:` may
 * hold an allowed address; once that finds nothing, every string and number YAML decodes from it is
 * checked again, where only the whole string at ingress.tunnel.clusterIP may be one, which catches what only escapes or `0x` integers reveal and an
 * allowed address under another key (strings YAML leaves as written pass again, so reporting both
 * would only repeat the first problems). Problems name the line and column, or the YAML path, only,
 * so that the CI log does not repeat what should not have been written.
 */
function hostProblems(text: string, allowedAddresses: readonly string[] = []): string[] {
  let clusterIpLineSeen = false;
  // The block mapping keys above the current line, by indentation, to tell where clusterIP is.
  const keys: { indent: number; key: string }[] = [];
  // The indentation of the line that opened the block scalar the current line is in.
  let blockScalarIndent: number | undefined;
  const tagState: { flowDepth: number; quote?: string } = { flowDepth: 0 };
  const problems = text.split("\n").flatMap((line, index) => {
    const lineIndent = (/^ */.exec(line) as RegExpExecArray)[0].length;
    if (blockScalarIndent !== undefined && line.trim() !== "" && lineIndent <= blockScalarIndent) blockScalarIndent = undefined;
    const inBlockScalar = blockScalarIndent !== undefined;
    if (!inBlockScalar && BLOCK_SCALAR_START.test(line)) blockScalarIndent = lineIndent;
    const block = inBlockScalar ? null : BLOCK_KEY.exec(line);
    if (block !== null) {
      const indent = (block[1] as string).length;
      while (keys.length > 0 && (keys.at(-1) as { indent: number }).indent >= indent) keys.pop();
      keys.push({ indent, key: block[2] as string });
    }
    const atClusterIp = block !== null && keys.map((k) => k.key).join(".") === CLUSTER_IP_PATH;
    const keyValue = KEY_VALUE_LINE.exec(line);
    const start = (keyValue?.[1] ?? "").length;
    const keyed = keyValue === null ? undefined : { start, end: start + (keyValue[5] as string).length, key: keyValue[3] as string };
    const clusterIpLine = !clusterIpLineSeen && atClusterIp && CLUSTER_IP_LINE.test(line);
    if (clusterIpLine) clusterIpLineSeen = true;
    const findings = pieceFindings(line, clusterIpLine ? allowedAddresses : [], keyed);
    const tag = yamlTagIndex(line, inBlockScalar, tagState);
    if (tag !== undefined) findings.push({ column: tag + 1, problem: "a YAML tag, whose value this check cannot read" });
    return findings.sort((a, b) => a.column - b.column).map((f) => `line ${index + 1}, column ${f.column}: ${f.problem}`);
  });
  if (problems.length > 0) return problems;
  let decoded: unknown;
  try {
    decoded = Bun.YAML.parse(text);
  } catch {
    return ["the text is not valid YAML, so the strings it decodes to could not be checked"];
  }
  return decodedProblems(decoded, allowedAddresses);
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

  // The last released chart version that lacks what a service needs on the demo cluster
  // (deploy/flux/README.md, 「日常變更」). The schema of such a version accepts any service name, so
  // the overlay renders there, but the release has no image for it: the pod cannot pull, the Helm
  // upgrade times out and rolls back. certs also needs Laysi/trustiot-v2#609's connection budget.
  // The gateway's data path (Laysi/trustiot-v2#795) needs a chart later than 0.0.0-rc.11: that
  // version has no ingest (Laysi/trustiot-v2#747) or gateway-ops (Laysi/trustiot-v2#752) image, and
  // its certs neither holds a broker account (Laysi/trustiot-v2#737) nor keeps the gateways' broker
  // clients (Laysi/trustiot-v2#727), so a gateway could not get past the broker. mqtt-bridge and the
  // broker are in rc.11, but are of no use without them; mqttBroker is a chart value, not a service.
  // The dashboard check's image came with Laysi/trustiot-v2#884 on 2026-10-31, after rc.12
  // (Laysi/trustiot-v2#881).
  // web (the SPA's static files, Laysi/trustiot-v2#961) has its image since 0.0.0-rc.11, the first
  // version whose identity service also refuses cross-site requests (Laysi/trustiot-v2#500).
  const LACKING_UP_TO: Record<string, string> = {
    certs: "0.0.0-rc.10",
    web: "0.0.0-rc.10",
    "mqtt-bridge": "0.0.0-rc.11",
    ingest: "0.0.0-rc.11",
    "gateway-ops": "0.0.0-rc.11",
    mqttBroker: "0.0.0-rc.11",
    "dashboard-check": "0.0.0-rc.12",
  };
  /** Whether the overlay switches a service, or the broker (mqttBroker), on. */
  const switchedOn = (values: YamlMap, name: string): boolean => {
    const entry = name === "mqttBroker" ? values.mqttBroker : isMap(values.services) ? values.services[name] : undefined;
    return isMap(entry) && entry.enabled === true;
  };
  /** Services the overlay switches on while the pinned chart version still lacks them. */
  const tooEarly = (values: YamlMap, tag: string): string[] =>
    Object.entries(LACKING_UP_TO)
      .filter(([name, last]) => switchedOn(values, name) && Bun.semver.order(tag, last) <= 0)
      .map(([name, last]) => `${name} is on, but the chart ${tag} is not later than ${last}`);

  test("a service is switched on only once the pinned chart ships it (Laysi/trustiot-v2#619, Laysi/trustiot-v2#795)", () => {
    const ref = spec(one("demo", "OCIRepository", "trustiot")).ref;
    const tag = isMap(ref) && typeof ref.tag === "string" ? ref.tag : "";
    expect(tag).not.toBe("");
    expect(tooEarly(overlay, tag)).toEqual([]);
    // The check itself: on, at or before the last version without it, is refused; off, or later, is not.
    const certsOn = { services: { certs: { enabled: true } } };
    expect(tooEarly(certsOn, "0.0.0-rc.10")).toEqual(["certs is on, but the chart 0.0.0-rc.10 is not later than 0.0.0-rc.10"]);
    expect(tooEarly(certsOn, "0.0.0-rc.9")).toHaveLength(1);
    expect(tooEarly(certsOn, "0.0.0-rc.11")).toEqual([]);
    expect(tooEarly({ services: { certs: { enabled: false } } }, "0.0.0-rc.10")).toEqual([]);
    expect(tooEarly({ services: { web: { enabled: true } } }, "0.0.0-rc.10")).toEqual(["web is on, but the chart 0.0.0-rc.10 is not later than 0.0.0-rc.10"]);
    expect(tooEarly({ services: { web: { enabled: true } } }, "0.0.0-rc.11")).toEqual([]);
    // The data path, each switch on its own, and the broker by its own value.
    for (const name of ["mqtt-bridge", "ingest", "gateway-ops"]) {
      const on = { services: { [name]: { enabled: true } } };
      expect(tooEarly(on, "0.0.0-rc.11")).toEqual([`${name} is on, but the chart 0.0.0-rc.11 is not later than 0.0.0-rc.11`]);
      expect(tooEarly(on, "0.0.0-rc.12")).toEqual([]);
    }
    expect(tooEarly({ mqttBroker: { enabled: true } }, "0.0.0-rc.11")).toEqual(["mqttBroker is on, but the chart 0.0.0-rc.11 is not later than 0.0.0-rc.11"]);
    expect(tooEarly({ mqttBroker: { enabled: false } }, "0.0.0-rc.11")).toEqual([]);
    expect(tooEarly({ mqttBroker: { enabled: true } }, "0.0.0-rc.12")).toEqual([]);
    // The dashboard check, which rc.12 does not ship.
    const checkOn = { services: { "dashboard-check": { enabled: true } } };
    expect(tooEarly(checkOn, "0.0.0-rc.12")).toEqual(["dashboard-check is on, but the chart 0.0.0-rc.12 is not later than 0.0.0-rc.12"]);
    expect(tooEarly(checkOn, "0.0.0-rc.13")).toEqual([]);
    // The overlay itself on the deployed rc.11: every part of the data path is refused (web is not).
    expect(tooEarly(overlay, "0.0.0-rc.11").sort()).toEqual(
      ["gateway-ops", "ingest", "mqtt-bridge", "mqttBroker"].map((name) => `${name} is on, but the chart 0.0.0-rc.11 is not later than 0.0.0-rc.11`),
    );
  });

  // The dashboard check answers only the assistant (deploy/chart/README.md, 「儀表板檢查」): on its
  // own it serves no one, and the isolation test after switching it on needs the assistant to send
  // the sample page (deploy/flux/README.md, 「打開儀表板檢查」, Laysi/trustiot-v2#881).
  const withoutCaller = (values: YamlMap): string[] =>
    switchedOn(values, "dashboard-check") && !switchedOn(values, "assistant") ? ["dashboard-check is on, but its one caller, assistant, is not"] : [];

  test("the dashboard check is switched on only together with or after the assistant (Laysi/trustiot-v2#881)", () => {
    expect(withoutCaller(overlay)).toEqual([]);
    expect(withoutCaller({ services: { "dashboard-check": { enabled: true } } })).toEqual(["dashboard-check is on, but its one caller, assistant, is not"]);
    expect(withoutCaller({ services: { "dashboard-check": { enabled: true }, assistant: { enabled: false } } })).toHaveLength(1);
    expect(withoutCaller({ services: { "dashboard-check": { enabled: true }, assistant: { enabled: true } } })).toEqual([]);
    expect(withoutCaller({ services: { assistant: { enabled: true } } })).toEqual([]);
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

  // Laysi/trustiot-v2#490: the other forms an address can take. Each is added to the overlay as a
  // comment or a new top-level key, so that it is the only problem.
  describe("the host check also refuses an address written in another form", () => {
    const lines = overlayText.split("\n").length;
    const problems = (text: string) => hostProblems(text, allowedAddresses);
    const added = (text: string) => problems(`${overlayText}${text}\n`);
    /** How a problem names the key added at the top level: by its position after the overlay's own keys. */
    const extra = `<key ${Object.keys(overlay).length + 1}>`;

    test("a three-part network prefix, but not a version", () => {
      expect(added("# 198.51.100")).toEqual([`line ${lines}, column 3: a three-part number outside a version, as in a network prefix`]);
      expect(added("# dev198.51.100")).toHaveLength(1);
      expect(added("# x_v198.51.100")).toHaveLength(1);
      expect(added("# tag: 198.51.100")).toHaveLength(1);
      expect(added("note: 0.0.0-rc.11")).toHaveLength(1);
      // Only the value is a version, not the comment after it.
      expect(added("tag: 198.51.100 # 203.0.113")).toEqual([`line ${lines}, column 19: a three-part number outside a version, as in a network prefix`]);
      // Versions: right after a lone v, or the whole value of a version key, as written and decoded.
      expect(added("# chart v1.2.3")).toEqual([]);
      expect(added("version: 1.2.3\ntag: \"0.0.0-rc.11\"\nimage:\n  appVersion: '1.2.3+build.4'")).toEqual([]);
    });

    test("an IPv6 address, also glued to hex letters, but not a time", () => {
      expect(added("# 2001:db8::7")).toEqual([`line ${lines}, column 3: an IPv6 address`]);
      expect(added("# [2001:db8:0:0:0:0:0:7]:8080")).toEqual([`line ${lines}, column 4: an IPv6 address`]);
      expect(added("# ab2001:db8::7")).toEqual([`line ${lines}, column 3: an IPv6 address`]);
      // A colon right before or after it, as in "address: what it is".
      expect(added("# fd00::1: the lab gateway")).toEqual([`line ${lines}, column 3: an IPv6 address`]);
      expect(added("# via proxy:2001:db8::7")).toEqual([`line ${lines}, column 12: an IPv6 address`]);
      expect(added("# ::ffff:192.0.2.10")).toEqual([`line ${lines}, column 3: an IPv6 address`, `line ${lines}, column 10: an IP address`]);
      // The overlay's own certificate fingerprint, colon-separated bytes, passes in the test above.
      expect(added("# at 18:33:16")).toEqual([]);
    });

    test("an IPv4 address written as one decimal integer, but not a port or a size", () => {
      expect(added("# 3325256714")).toEqual([`line ${lines}, column 3: a decimal integer in the IPv4 address range`]);
      expect(added("port: 16777216")).toEqual([`line ${lines}, column 7: a decimal integer in the IPv4 address range`]);
      expect(added("port: 16777215\nsize: 4294967296")).toEqual([]);
      expect(added("# a3325256714")).toEqual([`line ${lines}, column 4: a decimal integer in the IPv4 address range`]);
      // Digits inside a long hex string, such as a key id, are part of the hash.
      expect(added(`# key id ${"ab".repeat(8)}3325256714${"cd".repeat(8)}`)).toEqual([]);
    });

    test("an abbreviated IPv4 address as a host, but not a two-part number elsewhere", () => {
      const abbreviated = "an abbreviated IP address, a two-part number as a host";
      expect(added("# https://10.1/")).toEqual([`line ${lines}, column 11: ${abbreviated}`]);
      expect(added("# user@10.1")).toContain(`line ${lines}, column 8: ${abbreviated}`);
      expect(added("# at 10.1:6333")).toEqual([`line ${lines}, column 6: ${abbreviated}`]);
      expect(added("extra:\n  host: 10.1")).toEqual([`line ${lines + 1}, column 9: ${abbreviated}`]);
      expect(added(String.raw`extra:` + `\n  serverUrl: "https://10\\x2e1"`)).toEqual([`the string YAML decodes at ${`<key ${Object.keys(overlay).length + 1}>`}.<key 1>: ${abbreviated}`]);
      // A list of hosts: each item is a value of the host key.
      const key = `<key ${Object.keys(overlay).length + 1}>`;
      expect(added(`host: ["10.1"]`)).toEqual([`the string YAML decodes at ${key}[0]: ${abbreviated}`]);
      expect(added(`hosts:\n  - 10.1`)).toEqual([`the number YAML decodes at hosts[0]: ${abbreviated}`]);
      expect(added(`servers:\n  - [a, "10.1"]`)).toEqual([`the string YAML decodes at ${key}[0][1]: ${abbreviated}`]);
      expect(added("# Kubernetes 1.31, half is 0.5\nratio: 0.5\nratios: [0.5, \"1.5\"]")).toEqual([]);
    });

    test("an IPv4 address glued to letters", () => {
      expect(added("# a192.0.2.10")).toEqual([`line ${lines}, column 4: an IP address`]);
      expect(added("# id198.51.100.7x")).toContain(`line ${lines}, column 5: an IP address`);
      // The fixed ClusterIP is allowed only as itself, not as part of a longer number or word.
      expect(added(`# ${clusterIp as string}.1`)).toEqual([`line ${lines}, column 3: an IP address`]);
      expect(added(`# a${clusterIp as string}`)).toEqual([`line ${lines}, column 4: an IP address`]);
      expect(added(`# ${clusterIp as string}_b`)).toEqual([`line ${lines}, column 3: an IP address`]);
      // Nor anywhere but as the value of ingress.tunnel.clusterIP: in a comment, a second clusterIP
      // line, or a string that YAML decodes to it under another key.
      expect(added(`# at ${clusterIp as string}`)).toEqual([`line ${lines}, column 6: an IP address`]);
      expect(added(`other:\n  clusterIP: ${clusterIp as string}`)).toEqual([`line ${lines + 1}, column 14: an IP address`]);
      const escaped = (clusterIp as string).replaceAll(".", String.raw`\x2e`);
      expect(added(`other: "${escaped}"`)).toEqual([`the string YAML decodes at <key ${Object.keys(overlay).length + 1}>: an IP address`]);
      expect(added(`other:\n  tunnel:\n    clusterIP: ${clusterIp as string}`)).toEqual([`line ${lines + 2}, column 16: an IP address`]);
      // Nor through an alias of the mapping that holds it, under another key.
      const anchored = overlayText.replace("\n  tunnel:\n", "\n  tunnel: &t\n");
      expect(anchored).not.toBe(overlayText);
      expect(problems(`${anchored}copy: *t\n`)).toEqual([`the string YAML decodes at ${extra}.<key 1>: an IP address`]);
      // And only as the whole value there.
      const inValue = overlayText.replace(`clusterIP: ${clusterIp as string}`, `clusterIP: "${escaped}${String.raw`\x20x`}"`);
      expect(inValue).not.toBe(overlayText);
      expect(problems(inValue)).toEqual(["the string YAML decodes at ingress.tunnel.clusterIP: an IP address"]);
    });

    test("an address, host name or mail address that only YAML escapes or integer forms reveal, in values and keys", () => {
      expect(added(String.raw`extra: "192\u002e0\u002e2\u002e10"`)).toEqual([`the string YAML decodes at ${extra}: an IP address`]);
      expect(added(String.raw`extra: "api\x2eexample\x2ecom"`)).toEqual([`the string YAML decodes at ${extra}: ${HOST_PROBLEM}`]);
      expect(added(String.raw`extra: "no-reply\x40mail\x2einvalid"`)).toEqual([`the string YAML decodes at ${extra}: "@", as in a mail address`]);
      expect(added(String.raw`extra: ["2001\x3adb8\x3a\x3a7"]`)).toEqual([`the string YAML decodes at ${extra}[0]: an IPv6 address`]);
      expect(added(String.raw`extra: "198\x2e51\x2e100"`)).toEqual([`the string YAML decodes at ${extra}: a three-part number outside a version, as in a network prefix`]);
      expect(added("port: 0xC633640A")).toEqual([`the number YAML decodes at ${extra}: a number in the IPv4 address range`]);
      // A key that holds a problem is named by its position, so that the log does not repeat it.
      expect(added(String.raw`"kb\x2eexample\x2ecom": 1`)).toEqual([`the key YAML decodes at ${extra}: ${HOST_PROBLEM}`]);
      // Keys that make a path of the chart's values are named; any other key is not, even above
      // the problem (it could be a private name itself).
      expect(problems(overlayText.replace("host: smtp.resend.com", String.raw`host: "smtp\x2eexample\x2ecom"`))).toEqual([`the string YAML decodes at smtp.host: ${HOST_PROBLEM}`]);
      expect(added(String.raw`lab-gw:` + `\n  note: "api\\x2eexample\\x2ecom"`)).toEqual([`the string YAML decodes at ${extra}.<key 1>: ${HOST_PROBLEM}`]);
      // A tag such as !!binary hides its value from the decoded check, so it is refused as written.
      expect(added("extra: !!binary MTkyLjAuMi4xMA==")).toEqual([`line ${lines}, column 8: a YAML tag, whose value this check cannot read`]);
      expect(added("extra: [a, !!str b]")).toEqual([`line ${lines}, column 12: a YAML tag, whose value this check cannot read`]);
      // A verbatim tag (yaml.org in it is a host name too).
      expect(added("extra: !<tag:yaml.org,2002:binary> MTkyLjAuMi4xMA==")).toContain(`line ${lines}, column 8: a YAML tag, whose value this check cannot read`);
      expect(added("extra: !local x")).toHaveLength(1);
      // `!` in a comment or inside a quoted string or a word is no tag.
      expect(added(`# never write !!binary here\nextra: "a !!b" # c !!d\nnote: wow!!`)).toEqual([]);
      // Nor in a plain value after its start, or in a block scalar's lines.
      expect(added("note: this is !important, [!really]\nblock: |\n  !important\n  key: !x\nafter: 1")).toEqual([]);
      expect(added("block: |\n  text\nafter: !!str x")).toEqual([`line ${lines + 2}, column 8: a YAML tag, whose value this check cannot read`]);
      // After a document start, and in a flow collection or after a quoted string that spans lines.
      expect(added("--- !!binary MTkyLjAuMi4xMA==")).toEqual([`line ${lines}, column 5: a YAML tag, whose value this check cannot read`]);
      expect(added("list: [a,\n  b, !!str c]")).toEqual([`line ${lines + 1}, column 6: a YAML tag, whose value this check cannot read`]);
      expect(added('note: "a\n  b"\nafter: !!str c')).toEqual([`line ${lines + 2}, column 8: a YAML tag, whose value this check cannot read`]);
      expect(added('note: "a\n  b, !c"')).toEqual([]);
      // After an anchor, the node's other property.
      expect(added("extra: &hidden !!binary MTkyLjAuMi4xMA==")).toEqual([`line ${lines}, column 16: a YAML tag, whose value this check cannot read`]);
      expect(added("list:\n  - &a !x y\n  - [&b !y z]")).toHaveLength(2);
      // A comment that looks like a block scalar's opener opens none.
      expect(added("outer:\n  # note: |\n    extra: !!str x")).toEqual([`line ${lines + 2}, column 12: a YAML tag, whose value this check cannot read`]);
      expect(added("list:\n  - |\n    !text\n  - note: >-\n      !text")).toEqual([]);
      expect(added("list:\n  - !!str x\n  - {a: 1, b: !!str y}")).toEqual([
        `line ${lines + 1}, column 5: a YAML tag, whose value this check cannot read`,
        `line ${lines + 2}, column 15: a YAML tag, whose value this check cannot read`,
      ]);
      // An alias that refers to its own node ends the walk; what the node holds is still checked.
      expect(added("extra: &x [*x]\nloop: &y {a: *y}")).toEqual([]);
      expect(added(String.raw`extra: &x [*x, "api\x2eexample\x2ecom"]`)).toEqual([`the string YAML decodes at ${extra}[1]: ${HOST_PROBLEM}`]);
      // Escapes that decode to nothing private pass.
      expect(added(String.raw`extra: "qdrant\x2einvalid"`)).toEqual([]);
      // Text that is not YAML is refused: its decoded strings could not be checked.
      expect(added('extra: "unterminated')).toEqual(["the text is not valid YAML, so the strings it decodes to could not be checked"]);
    });
  });

  test(
    "with the private values, the chart at HEAD accepts it: the services that exist, their cookie routes, Resend's relay, no Sentry, no Qdrant",
    () => {
      const result = helmTemplate([overlay], setLiteral);
      expect(result.stderr).toBe("");
      expect(result.code).toBe(0);
      const objects = parseAll(result.stdout);
      const deployments = objects.filter((m) => m.kind === "Deployment").map((m) => m.metadata.name).sort();
      expect(deployments).toEqual([
        "trustiot-assets",
        "trustiot-certs",
        "trustiot-gateway-ops",
        "trustiot-identity",
        "trustiot-ingest",
        "trustiot-mqtt-bridge",
        "trustiot-otel-collector",
        "trustiot-web",
      ]);
      for (const name of ["trustiot-assets", "trustiot-certs", "trustiot-gateway-ops", "trustiot-identity", "trustiot-ingest", "trustiot-mqtt-bridge", "trustiot-web"]) {
        const pod = (spec(objects.find((m) => m.kind === "Deployment" && m.metadata.name === name)!).template as { spec: { imagePullSecrets: { name: string }[] } }).spec;
        expect(pod.imagePullSecrets).toEqual([{ name: GHCR_PULL_NAME }]);
      }
      // The cookie routes on the private app host (Laysi/trustiot-v2#619): the identity service's own
      // API, which the workstation tool's logout and the platform administrator's check use, and the
      // certs service's platform-admin API; the assets API for the gateway's binding, model and device,
      // and the gateway-ops list of gateways (Laysi/trustiot-v2#793). Each goes to its own service and
      // nowhere else.
      const routes = objects.filter((m) => m.kind === "HTTPRoute").map((m) => JSON.stringify(m.spec));
      for (const [path, backend] of [
        ["/api/identity", "trustiot-identity"],
        ["/api/certs", "trustiot-certs"],
        ["/api/assets", "trustiot-assets"],
        ["/api/gateway-ops", "trustiot-gateway-ops"],
      ] as const) {
        const matching = routes.filter((r) => r.includes(`"value":"${path}"`));
        expect({ path, routes: matching.length }).toEqual({ path, routes: 1 });
        expect(matching[0]).toContain(`"name":"${backend}"`);
        expect(matching[0]).toContain(privateExample["app-host"]!);
        expect(matching[0]).not.toContain(privateExample["gateway-host"]!);
        expect(matching[0]).not.toContain(privateExample["ota-host"]!);
      }
      // The SPA's static files (Laysi/trustiot-v2#961): the app host has one rule to web, PathPrefix
      // /, which strips the Cookie and the identity header, and no other host routes to web.
      const appRoute = objects.find((m) => m.kind === "HTTPRoute" && m.metadata.name === "trustiot-ingress-app");
      expect(appRoute).toBeDefined();
      expect(spec(appRoute!).hostnames).toEqual([privateExample["app-host"]]);
      const toWeb = (spec(appRoute!).rules as YamlMap[]).filter((rule) => (rule.backendRefs as YamlMap[]).some((b) => b.name === "trustiot-web"));
      expect(toWeb).toHaveLength(1);
      const spa = toWeb[0]!;
      expect((spa.matches as { path: YamlMap }[]).map((m) => m.path)).toEqual([{ type: "PathPrefix", value: "/" }]);
      expect((spa.backendRefs as YamlMap[]).map((b) => b.name)).toEqual(["trustiot-web"]);
      const removed = (spa.filters as { requestHeaderModifier?: { remove?: string[] } }[]).flatMap((f) => f.requestHeaderModifier?.remove ?? []);
      expect(removed).toEqual(expect.arrayContaining(["cookie", "x-internal-identity"]));
      expect(routes.filter((r) => r.includes('"name":"trustiot-web"'))).toHaveLength(1);
      // The gateway's data path (Laysi/trustiot-v2#795): the gateway host has exactly the chart's
      // cookie-free gateway paths (the root CRL since Laysi/trustiot-v2#831; the demo's tunnel keeps
      // it behind Cloudflare Access until Laysi/trustiot-v2#563 opens it), each an Exact match to its
      // own service, and the app and ota hosts have none of them. No route names the broker: gateways reach it only through mqtt-bridge.
      const gatewayRoutes = objects.filter((m) => m.kind === "HTTPRoute" && JSON.stringify(m.spec).includes(privateExample["gateway-host"]!));
      expect(gatewayRoutes.map((m) => m.metadata.name)).toEqual(["trustiot-ingress-gateway"]);
      const gatewayRules = (spec(gatewayRoutes[0]!).rules as YamlMap[]).map((rule) => ({
        matches: (rule.matches as { path: YamlMap }[]).map((m) => m.path),
        backends: (rule.backendRefs as YamlMap[]).map((b) => b.name),
      }));
      expect(gatewayRules).toEqual([
        { matches: [{ type: "Exact", value: "/certs/renew" }], backends: ["trustiot-certs"] },
        { matches: [{ type: "Exact", value: "/certs/root-crl" }], backends: ["trustiot-certs"] },
        { matches: [{ type: "Exact", value: "/mqtt" }], backends: ["trustiot-mqtt-bridge"] },
      ]);
      expect(routes.filter((r) => r.includes('"value":"/mqtt"') && !r.includes(privateExample["gateway-host"]!))).toEqual([]);
      expect(routes.filter((r) => r.includes("trustiot-mqtt-broker"))).toEqual([]);
      // The broker, its server certificate's CronJob, and the address the bridge relays to.
      expect(objects.some((m) => m.kind === "StatefulSet" && m.metadata.name === "trustiot-mqtt-broker")).toBe(true);
      expect(objects.some((m) => m.kind === "CronJob" && m.metadata.name === "trustiot-mqtt-broker-certificate")).toBe(true);
      const bridge = JSON.stringify(spec(objects.find((m) => m.kind === "Deployment" && m.metadata.name === "trustiot-mqtt-bridge")!));
      expect(bridge).toContain('{"name":"TRUSTIOT__BROKER__ADDRESS","value":"trustiot-mqtt-broker.trustiot.svc:8883"}');
      const platform =objects.find((m) => m.kind === "ConfigMap" && m.metadata.name === "trustiot-platform")?.data as Record<string, string>;
      expect(platform.TRUSTIOT_APP_HOST).toBe(privateExample["app-host"]);
      // mqtt-bridge records the gateway's address from Cloudflare's header (tunnel exposure).
      expect(platform.TRUSTIOT_CLIENT_IP_SOURCE).toBe("cf-connecting-ip");
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
    "with every Postgres service of the slice on, the overlay passes the chart's connection check, which still refuses two identity replicas more (Laysi/trustiot-v2#575)",
    () => {
      // The demo slice turns certs, ingest, gateway-ops and telemetry-query on next to identity and
      // assets, and Flux rolls every service at once: the chart refuses values whose pools could then
      // exceed Postgres' max_connections (deploy/chart/README.md, 「連線預算」).
      const allOn: YamlMap = { services: { certs: { enabled: true }, "telemetry-query": { enabled: true } } };
      const result = helmTemplate([overlay, allOn], setLiteral);
      expect(result.stderr).toBe("");
      expect(result.code).toBe(0);
      const objects = parseAll(result.stdout);
      const postgresClients = objects
        .filter((m) => m.kind === "Deployment" && JSON.stringify(m).includes("TRUSTIOT__DATABASE__MAX_CONNECTIONS"))
        .map((m) => m.metadata.name)
        .sort();
      expect(postgresClients).toEqual([
        "trustiot-assets",
        "trustiot-certs",
        "trustiot-gateway-ops",
        "trustiot-identity",
        "trustiot-ingest",
        "trustiot-telemetry-query",
      ]);
      // The control: three identity replicas are 33 x 4 instead of 33 x 2, past the default 240
      // (Laysi/trustiot-v2#721 raised it from 200 and Laysi/trustiot-v2#645 from 220).
      const more = helmTemplate([overlay, allOn], [...setLiteral, "--set", "services.identity.replicas=3"]);
      expect(more.code).not.toBe(0);
      expect(more.stderr).toContain("postgres.bundled.parameters.max_connections is 240, but 274 connections are needed");
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
      // Helm replaces a list as a whole, so the README's list keeps every route the overlay has,
      // in the overlay's order, and adds its own after them.
      const overlayRoutes = isMap(overlay.ingress) ? overlay.ingress.routes : undefined;
      expect(Array.isArray(overlayRoutes)).toBe(true);
      const readmeRoutes = isMap(values.ingress) ? values.ingress.routes : undefined;
      expect(Array.isArray(readmeRoutes)).toBe(true);
      expect((readmeRoutes as unknown[]).slice(0, (overlayRoutes as unknown[]).length)).toEqual(overlayRoutes as unknown[]);
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

// The MQTT broker (Laysi/trustiot-v2#84) is off in the chart's shapes. Turning it on in a cluster is
// a change of its own, which updates this table: the demo cluster turned it on for the gateway's
// data path (Laysi/trustiot-v2#795), in its public overlay only; the SaaS cluster still leaves it
// off. No other values file of a release sets it (the shape copied from the chart never does), and
// no private value targets it: what the encrypted values hold cannot be reviewed in a PR.
const BROKER_SET_BY: Record<ClusterName, Record<string, unknown>> = {
  saas: {},
  demo: { "values-demo.yaml": { enabled: true } },
};

describe.each([...CLUSTERS])("the MQTT broker in the %s cluster", (cluster) => {
  test("only the values files in the table set mqttBroker, and no private value targets it", () => {
    const data = one(cluster, "ConfigMap", "trustiot-values").data as Record<string, string>;
    expect(Object.keys(data).length).toBeGreaterThan(0);
    const setBy: Record<string, unknown> = {};
    for (const [file, text] of Object.entries(data)) {
      const values = Bun.YAML.parse(text);
      if (isMap(values) && values.mqttBroker !== undefined) setBy[file] = values.mqttBroker;
    }
    expect(setBy).toEqual(BROKER_SET_BY[cluster]);
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
