// Tests of the SaaS GitOps tree (deploy/flux/README.md). Run from the repository root:
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
  CHART_SOURCE_FILE,
  CLUSTER_DIR,
  HELM_CHART_LAYER,
  GHCR_PULL_NAME,
  SHAPE_VALUES_FILE,
  exactPattern,
  parseChartSource,
  parseShapeValues,
} from "../../../scripts/release/flux.ts";
import { CHART_REGISTRY, GITHUB_OIDC_ISSUER, signingIdentity } from "../../../scripts/release/lib.ts";
import { FLUX_COMPONENTS, FLUX_SYSTEM_DIR, controllerImages } from "../../../scripts/flux/lib.ts";

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
const built = run([KUBECTL, "kustomize", CLUSTER_DIR]);
const manifests = built.code === 0 ? parseAll(built.stdout) : [];
const components = read(`${FLUX_SYSTEM_DIR}/gotk-components.yaml`);

function only(kind: string): Manifest[] {
  return manifests.filter((m) => m.kind === kind);
}

function one(kind: string, name: string): Manifest {
  const found = manifests.find((m) => m.kind === kind && m.metadata.name === name);
  if (found === undefined) throw new Error(`${kind}/${name} is not in the build`);
  return found;
}

function spec(manifest: Manifest): YamlMap {
  return isMap(manifest.spec) ? manifest.spec : {};
}

describe("kustomize build", () => {
  test("the SaaS tree builds", () => {
    expect(built.stderr).toBe("");
    expect(built.code).toBe(0);
  });

  test("it holds Flux, its sync, and the TrustIoT release, and nothing else", () => {
    const flux = new Set(parseAll(components).map((m) => `${m.kind}/${m.metadata.name}`));
    const rest = manifests.map((m) => `${m.kind}/${m.metadata.name}`).filter((id) => !flux.has(id)).sort();
    expect(rest).toEqual([
      "ConfigMap/trustiot-values",
      "GitRepository/flux-system",
      "HelmRelease/trustiot",
      "Kustomization/flux-system",
      "Namespace/trustiot",
      "OCIRepository/trustiot",
    ]);
  });

  test(
    // The CRDs themselves come verbatim from the pinned flux CLI (checked below), and kubeconform
    // has no schema for CustomResourceDefinition, so they are left out here.
    "every other resource passes kubeconform -strict (Flux kinds against the pinned CRDs)",
    () => {
      const resources = manifests.filter((m) => m.kind !== "CustomResourceDefinition");
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

  test("every controller image is pinned by digest at the exported tag", () => {
    const expected = controllerImages(components).map((image) => `${image.name}:${image.tag}`);
    const deployed = only("Deployment").flatMap((d) => {
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

  test("the cluster reads main of this repository over SSH, only deploy/flux/, and decrypts SOPS", () => {
    const git = spec(one("GitRepository", "flux-system"));
    expect(git.url).toBe(`ssh://git@github.com/${REPOSITORY}.git`);
    expect(git.ref).toEqual({ branch: "main" });
    expect(git.secretRef).toEqual({ name: "flux-system" });
    expect(String(git.ignore).trim().split("\n")).toEqual(["/*", "!/deploy/", "/deploy/*", "!/deploy/flux/"]);
    const sync = spec(one("Kustomization", "flux-system"));
    expect(sync.path).toBe(`./${CLUSTER_DIR}`);
    expect(sync.prune).toBe(true);
    expect(sync.decryption).toEqual({ provider: "sops", secretRef: { name: "sops-age" } });
  });

  test("no notification or image automation controllers: the cluster only pulls", () => {
    const deployments = only("Deployment").map((d) => d.metadata.name).sort();
    expect(deployments).toEqual([...FLUX_COMPONENTS].sort());
  });
});

describe("the TrustIoT release", () => {
  const chartSourceText = read(CHART_SOURCE_FILE);

  test("chart.yaml is exactly what release.yml writes for its version (scripts/release/flux.ts)", () => {
    expect(() => parseChartSource(chartSourceText, REPOSITORY, CHART_REPOSITORY)).not.toThrow();
  });

  test("the chart is pulled by digest with the pull Secret and only with release.yml's signature for that tag", () => {
    const { version, digest } = parseChartSource(chartSourceText, REPOSITORY, CHART_REPOSITORY);
    const source = spec(one("OCIRepository", "trustiot"));
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
    expect(only("HelmRepository")).toEqual([]);
    expect(only("Bucket")).toEqual([]);
    expect(only("HelmChart")).toEqual([]);
    expect(only("OCIRepository").map((m) => m.metadata.name)).toEqual(["trustiot"]);
    expect(only("GitRepository").map((m) => m.metadata.name)).toEqual(["flux-system"]);
  });

  test("the values copy comes from the deployed release", () => {
    const { version } = parseChartSource(chartSourceText, REPOSITORY, CHART_REPOSITORY);
    expect(parseShapeValues(read(SHAPE_VALUES_FILE)).version).toBe(version);
  });

  test("the HelmRelease installs that chart into its namespace with the shape and instance values, in that order", () => {
    const release = one("HelmRelease", "trustiot");
    expect(release.metadata.namespace).toBe(APP_NAMESPACE);
    const releaseSpec = spec(release);
    expect(releaseSpec.chartRef).toEqual({ kind: "OCIRepository", name: "trustiot" });
    expect(releaseSpec.releaseName).toBe("trustiot");
    expect(releaseSpec.valuesFrom).toEqual([
      { kind: "ConfigMap", name: "trustiot-values", valuesKey: "values-saas.yaml" },
      { kind: "ConfigMap", name: "trustiot-values", valuesKey: "values-instance.yaml" },
    ]);
    const values = one("ConfigMap", "trustiot-values");
    expect(values.metadata.namespace).toBe(APP_NAMESPACE);
    expect(values.metadata.labels?.["reconcile.fluxcd.io/watch"]).toBe("Enabled");
    const data = values.data as Record<string, string>;
    expect(data["values-saas.yaml"]).toBe(read(SHAPE_VALUES_FILE));
    expect(data["values-instance.yaml"]).toBe(read(`${APP_DIR}/values-instance.yaml`));
    const namespace = one("Namespace", APP_NAMESPACE);
    expect(namespace.metadata.labels?.["pod-security.kubernetes.io/enforce"]).toBe("restricted");
  });

  test("every SOPS file in the release directory is applied, and nothing applied is missing", () => {
    const kustomization = Bun.YAML.parse(read(`${APP_DIR}/kustomization.yaml`)) as { resources: string[] };
    const sopsFiles = readdirSync(join(REPO_ROOT, APP_DIR)).filter((file) => file.endsWith(".sops.yaml"));
    for (const file of sopsFiles) expect(kustomization.resources).toContain(file);
    for (const resource of kustomization.resources) expect(existsSync(join(REPO_ROOT, APP_DIR, resource))).toBe(true);
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
      const dir = mkdtempSync(join(tmpdir(), "trustiot-flux-values-"));
      try {
        const merged = join(dir, "values.yaml");
        writeFileSync(merged, JSON.stringify(filled));
        const result = run([
          pinnedTool("helm"),
          "template",
          "trustiot",
          "deploy/chart",
          "--namespace",
          APP_NAMESPACE,
          "--values",
          "deploy/chart/values-saas.yaml",
          "--values",
          merged,
          "--set",
          "services.identity.enabled=true",
        ]);
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
      } finally {
        rmSync(dir, { recursive: true, force: true });
      }
    },
    SLOW_TEST_MS,
  );
});

describe(".sops.yaml", () => {
  const config = Bun.YAML.parse(read(".sops.yaml")) as { creation_rules: YamlMap[] };

  test("one rule: Secrets under deploy/flux/clusters/saas named *.sops.yaml, data and stringData only", () => {
    expect(config.creation_rules.length).toBe(1);
    const rule = config.creation_rules[0]!;
    expect(rule.encrypted_regex).toBe("^(data|stringData)$");
    const path = new RegExp(String(rule.path_regex));
    expect(path.test(`${APP_DIR}/${GHCR_PULL_NAME}.sops.yaml`)).toBe(true);
    expect(path.test(`/home/someone/trustiot-v2/${APP_DIR}/${GHCR_PULL_NAME}.sops.yaml`)).toBe(true);
    expect(path.test(`${APP_DIR}/values-instance.yaml`)).toBe(false);
    expect(path.test(`deploy/chart/${GHCR_PULL_NAME}.sops.yaml`)).toBe(false);
    expect(path.test(`${GHCR_PULL_NAME}.sops.yaml`)).toBe(false);
  });

  test("every recipient is an age public key, listed once", () => {
    const recipients = config.creation_rules[0]!.age;
    expect(Array.isArray(recipients)).toBe(true);
    const keys = recipients as unknown[];
    for (const key of keys) expect(String(key)).toMatch(/^age1[02-9ac-hj-np-z]{58}$/);
    expect(new Set(keys).size).toBe(keys.length);
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
