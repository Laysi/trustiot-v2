// Chart tests: helm lint, kubeconform and the chart's own rules for both deployment shapes.
// Run from the repository root: bun test deploy/chart
// Needs helm and kubeconform from .prototools; kubeconform downloads schemas on first use.
import { describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { isAbsolute, join } from "node:path";

const CHART_DIR = join(import.meta.dir, "..");
const REPO_ROOT = join(CHART_DIR, "..", "..");

// Runs the versions pinned in .prototools directly instead of through proto's shims: a shim
// can write its own notices to stdout (for example while another proto process holds a
// lock), which would corrupt rendered manifests.
function pinnedTool(tool: string): string {
  const result = Bun.spawnSync({
    cmd: ["proto", "bin", tool, "--reporter", "text"],
    cwd: REPO_ROOT,
    stdout: "pipe",
    stderr: "pipe",
  });
  const path = result.stdout.toString().trim().split("\n").at(-1)?.trim() ?? "";
  if (result.exitCode !== 0 || !isAbsolute(path) || !existsSync(path)) {
    throw new Error(`cannot find the pinned ${tool}; run \`proto install\`\n${result.stderr.toString()}`);
  }
  return path;
}

const HELM = pinnedTool("helm");
const KUBECONFORM = pinnedTool("kubeconform");
const RELEASE = "trustiot";
const NAMESPACE = "trustiot";
// Oldest version allowed by Chart.yaml kubeVersion, and the k3s version of the lab and SaaS hosts.
const KUBERNETES_VERSIONS = ["1.30.0", "1.36.0"];
// Pods carry these Kubernetes-native objects as-is; the README documents them as one field each.
const KUBERNETES_OBJECT_FIELDS = new Set([
  "ports",
  "env",
  "resources",
  "podSecurityContext",
  "securityContext",
  "probes.startup",
  "probes.liveness",
  "probes.readiness",
  "nodeSelector",
  "tolerations",
  "affinity",
  "podLabels",
  "podAnnotations",
]);
const CRD_SCHEMA_LOCATION =
  "https://raw.githubusercontent.com/datreeio/CRDs-catalog/main/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json";
const SLOW_TEST_MS = 120_000;

const SHAPES = {
  saas: ["values-saas.yaml", "tests/values/saas-instance.yaml"],
  onprem: ["values-onprem.yaml", "tests/values/onprem-instance.yaml"],
} as const;
type Shape = keyof typeof SHAPES;
const SHAPE_NAMES = Object.keys(SHAPES) as Shape[];
const SMOKE = "tests/values/smoke.yaml";
// Ingress e2e overlay: identity, certs, mqtt-bridge and assets on, plus a catch-all cookie route.
const E2E_VALUES = "tests/e2e/values-e2e.yaml";

type YamlMap = { [key: string]: unknown };
type Manifest = YamlMap & {
  apiVersion: string;
  kind: string;
  metadata: { name: string; labels?: Record<string, string>; annotations?: Record<string, string> };
};
type Container = YamlMap & {
  name: string;
  image: string;
  env?: { name: string; value?: string; valueFrom?: { secretKeyRef?: { name: string; key: string } } }[];
  envFrom?: { configMapRef?: { name: string } }[];
  ports?: { name: string; containerPort: number; hostPort?: number }[];
  volumeMounts?: YamlMap[];
  livenessProbe?: YamlMap;
  readinessProbe?: YamlMap;
  startupProbe?: YamlMap;
  lifecycle?: { preStop?: { sleep?: { seconds: number } } };
  resources?: { requests?: Record<string, string>; limits?: Record<string, string> };
  securityContext?: YamlMap & { capabilities?: { drop?: string[]; add?: string[] } };
};
type PodSpec = YamlMap & {
  serviceAccountName: string;
  automountServiceAccountToken?: boolean;
  terminationGracePeriodSeconds: number;
  securityContext?: YamlMap & { seccompProfile?: { type: string } };
  containers: Container[];
  volumes?: YamlMap[];
};

function isMap(value: unknown): value is YamlMap {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function run(cmd: string[], stdin?: string) {
  const proc = Bun.spawnSync({
    cmd,
    cwd: CHART_DIR,
    stdin: stdin === undefined ? "ignore" : new TextEncoder().encode(stdin),
    stdout: "pipe",
    stderr: "pipe",
  });
  return { code: proc.exitCode, stdout: proc.stdout.toString(), stderr: proc.stderr.toString() };
}

function valuesArgs(files: readonly string[]): string[] {
  return files.flatMap((file) => ["--values", file]);
}

function helmTemplate(files: readonly string[], args: string[] = [], release = RELEASE) {
  return run([HELM, "template", release, ".", "--namespace", NAMESPACE, ...valuesArgs(files), ...args]);
}

function parseManifests(yaml: string): Manifest[] {
  const parsed: unknown = Bun.YAML.parse(yaml);
  const docs = Array.isArray(parsed) ? parsed : [parsed];
  return docs.filter(isMap) as Manifest[];
}

function render(files: readonly string[], args: string[] = [], release = RELEASE): Manifest[] {
  const result = helmTemplate(files, args, release);
  if (result.code !== 0) throw new Error(`helm template failed:\n${result.stderr}`);
  return parseManifests(result.stdout);
}

function find(manifests: Manifest[], kind: string, name: string): Manifest {
  const found = manifests.find((m) => m.kind === kind && m.metadata.name === name);
  if (!found) throw new Error(`${kind}/${name} not rendered`);
  return found;
}

function podSpec(deployment: Manifest): PodSpec {
  const spec = deployment.spec as { template: { spec: PodSpec } };
  return spec.template.spec;
}

function platformConfig(manifests: Manifest[]): Record<string, string> {
  return find(manifests, "ConfigMap", `${RELEASE}-platform`).data as Record<string, string>;
}

function readYaml(relativePath: string): YamlMap {
  const parsed: unknown = Bun.YAML.parse(readFileSync(join(CHART_DIR, relativePath), "utf8"));
  if (!isMap(parsed)) throw new Error(`${relativePath} is not a YAML map`);
  return parsed;
}

const defaults = readYaml("values.yaml");
const chart = readYaml("Chart.yaml") as { version: string; appVersion: string };
const plannedServices = Object.keys(defaults.services as YamlMap);
const enableAllServices = plannedServices.flatMap((name) => ["--set", `services.${name}.enabled=true`]);

function kubeconform(manifests: string, kubernetesVersion: string) {
  const cache = join(tmpdir(), "trustiot-kubeconform-cache");
  mkdirSync(cache, { recursive: true });
  return run(
    [
      KUBECONFORM,
      "-strict",
      "-summary",
      "-kubernetes-version",
      kubernetesVersion,
      "-cache",
      cache,
      "-schema-location",
      "default",
      "-schema-location",
      CRD_SCHEMA_LOCATION,
      "-",
    ],
    manifests,
  );
}

describe.each(SHAPE_NAMES)("%s shape", (shape) => {
  const files = SHAPES[shape];

  test("helm lint --strict passes, with and without the smoke service", () => {
    for (const extra of [[], [SMOKE]]) {
      const result = run([HELM, "lint", ".", "--strict", ...valuesArgs([...files, ...extra])]);
      expect(result.stdout + result.stderr).toContain("0 chart(s) failed");
      expect(result.code).toBe(0);
    }
  });

  test.each(KUBERNETES_VERSIONS)(
    "rendered manifests pass kubeconform -strict for Kubernetes %s",
    (kubernetesVersion) => {
      const renders = [
        helmTemplate(files),
        helmTemplate([...files, SMOKE]),
        helmTemplate([...files, SMOKE], enableAllServices),
        // Ingress routes, public paths and the cookie check.
        helmTemplate([...files, E2E_VALUES], enableAllServices),
      ];
      for (const rendered of renders) {
        expect(rendered.stderr).toBe("");
        expect(rendered.code).toBe(0);
        const result = kubeconform(rendered.stdout, kubernetesVersion);
        expect(`${result.stdout}${result.stderr}`).toMatch(/Invalid: 0, Errors: 0, Skipped: 0/);
        expect(result.code).toBe(0);
      }
    },
    SLOW_TEST_MS,
  );

  test("services stay off until enabled: only the platform settings, the ingress, bundled ClickHouse and default deny render", () => {
    const manifests = render(files);
    expect(manifests.map((m) => `${m.kind}/${m.metadata.name}`).sort()).toEqual(
      [
        `AuthorizationPolicy/${RELEASE}-ingress-cookie-check`,
        `AuthorizationPolicy/${RELEASE}-ingress-deny-internal`,
        `ConfigMap/${RELEASE}-ingress-options`,
        `ConfigMap/${RELEASE}-platform`,
        `Gateway/${RELEASE}-ingress`,
        `NetworkPolicy/${RELEASE}-default-deny`,
        `NetworkPolicy/${RELEASE}-ingress`,
        `AuthorizationPolicy/${RELEASE}-clickhouse`,
        `NetworkPolicy/${RELEASE}-clickhouse`,
        `ConfigMap/${RELEASE}-clickhouse-config`,
        `ServiceAccount/${RELEASE}-clickhouse`,
        `Service/${RELEASE}-clickhouse`,
        `StatefulSet/${RELEASE}-clickhouse`,
      ].sort(),
    );
    // No backend, so no route and no public path: the cookie check covers every request.
    const check = find(manifests, "AuthorizationPolicy", `${RELEASE}-ingress-cookie-check`);
    expect((check.spec as { rules: unknown[] }).rules).toEqual([{}]);
  });

  test("every planned service renders a ServiceAccount, Service and Deployment", () => {
    const manifests = render(files, enableAllServices);
    for (const name of plannedServices) {
      const objectName = `${RELEASE}-${name}`;
      find(manifests, "ServiceAccount", objectName);
      find(manifests, "Service", objectName);
      const pod = podSpec(find(manifests, "Deployment", objectName));
      expect(pod.serviceAccountName).toBe(objectName);
      expect(pod.containers[0]?.image).toBe(`ghcr.io/laysi/trustiot-v2/${name}:${chart.appVersion}`);
    }
  });
});

describe("deployment shapes", () => {
  test("SaaS: behind Cloudflare Tunnel, hosted Qdrant, Sentry on", () => {
    const config = platformConfig(render(SHAPES.saas));
    expect(config.TRUSTIOT_DEPLOYMENT_MODE).toBe("saas");
    expect(config.TRUSTIOT_CLIENT_IP_SOURCE).toBe("cf-connecting-ip");
    expect(config.TRUSTIOT_QDRANT_URL).toBe("https://qdrant.saas.trustiot.test:6333");
    expect(config.SENTRY_ENVIRONMENT).toBe("test");
  });

  test("on-prem: direct ingress, bundled Qdrant, customer SMTP, Sentry off", () => {
    const config = platformConfig(render(SHAPES.onprem));
    expect(config.TRUSTIOT_DEPLOYMENT_MODE).toBe("onprem");
    expect(config.TRUSTIOT_CLIENT_IP_SOURCE).toBe("x-forwarded-for-rightmost");
    expect(config.TRUSTIOT_QDRANT_URL).toBeUndefined();
    expect(config.TRUSTIOT_SMTP_HOST).toBe("smtp.factory.test");
    expect(config.SENTRY_ENVIRONMENT).toBeUndefined();
  });

  test("gateway-facing hosts come from values (portability rule 1)", () => {
    for (const shape of SHAPE_NAMES) {
      const instance = readYaml(SHAPES[shape][1]).hosts as Record<string, string>;
      const config = platformConfig(render(SHAPES[shape]));
      expect(config.TRUSTIOT_APP_HOST).toBe(instance.app);
      expect(config.TRUSTIOT_GATEWAY_HOST).toBe(instance.gateway);
      expect(config.TRUSTIOT_OTA_HOST).toBe(instance.ota);
    }
  });

  test("external Postgres and ClickHouse are wired only in external mode, bundled ClickHouse in-cluster", () => {
    const bundled = platformConfig(render(SHAPES.onprem));
    expect(bundled.TRUSTIOT_POSTGRES_HOST).toBeUndefined();
    expect(bundled.TRUSTIOT_POSTGRES_CA_FILE).toBeUndefined();
    expect(bundled.TRUSTIOT_CLICKHOUSE_URL).toBe(`http://${RELEASE}-clickhouse.${NAMESPACE}.svc:8123`);
    const external = platformConfig(
      render(SHAPES.onprem, [
        "--set",
        "postgres.mode=external",
        "--set",
        "postgres.external.host=pg.factory.test",
        "--set",
        "postgres.external.caSecret.name=trustiot-pg-ca",
        "--set",
        "clickhouse.mode=external",
        "--set",
        "clickhouse.external.url=https://ch.factory.test:8443",
      ]),
    );
    expect(external.TRUSTIOT_POSTGRES_HOST).toBe("pg.factory.test");
    expect(external.TRUSTIOT_POSTGRES_PORT).toBe("5432");
    expect(external.TRUSTIOT_POSTGRES_SSLMODE).toBe("verify-full");
    expect(external.TRUSTIOT_POSTGRES_CA_FILE).toBe("/etc/trustiot/postgres-ca/ca.crt");
    expect(external.TRUSTIOT_CLICKHOUSE_URL).toBe("https://ch.factory.test:8443");
    // A publicly trusted ClickHouse certificate needs no CA file.
    expect(external.TRUSTIOT_CLICKHOUSE_CA_FILE).toBeUndefined();
  });
});

describe("secrets reach only the services that need them", () => {
  const secretRefs = (manifests: Manifest[], service: string) => {
    const env = podSpec(find(manifests, "Deployment", `${RELEASE}-${service}`)).containers[0]?.env ?? [];
    return Object.fromEntries(
      env.flatMap((e) => {
        const ref = e.valueFrom?.secretKeyRef;
        return ref ? [[e.name, `${ref.name}/${ref.key}`]] : [];
      }),
    );
  };

  test("SaaS: every service gets the Sentry DSN, dependency credentials go only where listed", () => {
    const manifests = render(SHAPES.saas, [
      "--set",
      "services.identity.enabled=true",
      "--set",
      "services.knowledge.enabled=true",
      "--set-json",
      'services.knowledge.credentials=["qdrant-read","object-storage"]',
      "--set",
      "services.ai-gateway.enabled=true",
      "--set-json",
      'services.ai-gateway.credentials=["ai"]',
    ]);
    expect(secretRefs(manifests, "identity")).toEqual({ SENTRY_DSN: "trustiot-sentry/dsn" });
    expect(secretRefs(manifests, "knowledge")).toEqual({
      SENTRY_DSN: "trustiot-sentry/dsn",
      TRUSTIOT_QDRANT_READ_API_KEY: "trustiot-qdrant/read-api-key",
      TRUSTIOT_S3_ACCESS_KEY_ID: "trustiot-object-storage/access-key-id",
      TRUSTIOT_S3_SECRET_ACCESS_KEY: "trustiot-object-storage/secret-access-key",
    });
    expect(secretRefs(manifests, "ai-gateway")).toEqual({
      SENTRY_DSN: "trustiot-sentry/dsn",
      TRUSTIOT_AI_CHAT_API_KEY: "trustiot-ai-chat/api-key",
      TRUSTIOT_AI_EMBEDDING_API_KEY: "trustiot-ai-embedding/api-key",
      TRUSTIOT_AI_RERANK_API_KEY: "trustiot-ai-chat/api-key",
    });
  });

  test("on-prem: no Sentry, and credentials without a configured Secret are left out", () => {
    const manifests = render(SHAPES.onprem, [
      "--set",
      "services.alerts.enabled=true",
      "--set-json",
      'services.alerts.credentials=["smtp"]',
      "--set",
      "services.knowledge.enabled=true",
      "--set-json",
      'services.knowledge.credentials=["qdrant-read","qdrant-write","object-storage"]',
    ]);
    // The customer's relay needs no login, and bundled Qdrant brings its own accounts.
    expect(secretRefs(manifests, "alerts")).toEqual({});
    expect(secretRefs(manifests, "knowledge")).toEqual({
      TRUSTIOT_S3_ACCESS_KEY_ID: "trustiot-object-storage/access-key-id",
      TRUSTIOT_S3_SECRET_ACCESS_KEY: "trustiot-object-storage/secret-access-key",
    });
  });

  test("an external database CA is mounted read-only into every service", () => {
    const manifests = render(
      [...SHAPES.saas, SMOKE],
      [
        "--set",
        "postgres.mode=external",
        "--set",
        "postgres.external.host=pg.saas.trustiot.test",
        "--set",
        "postgres.external.caSecret.name=trustiot-pg-ca",
        "--set",
        "postgres.external.caSecret.key=root.crt",
      ],
    );
    const pod = podSpec(find(manifests, "Deployment", `${RELEASE}-smoke-echo`));
    expect(pod.volumes).toContainEqual({
      name: "postgres-ca",
      secret: { secretName: "trustiot-pg-ca", items: [{ key: "root.crt", path: "ca.crt" }] },
    });
    expect(pod.containers[0]?.volumeMounts).toContainEqual({
      name: "postgres-ca",
      mountPath: "/etc/trustiot/postgres-ca",
      readOnly: true,
    });
  });
});

describe("exposure and hardening", () => {
  const renders = SHAPE_NAMES.map((shape) => render([...SHAPES[shape], SMOKE], enableAllServices));

  test("nothing is exposed outside the cluster: ClusterIP only, no host ports or host network", () => {
    for (const manifests of renders) {
      for (const service of manifests.filter((m) => m.kind === "Service")) {
        expect((service.spec as YamlMap).type).toBe("ClusterIP");
      }
      const workloads = manifests.filter((m) => m.kind === "Deployment" || m.kind === "StatefulSet");
      expect(workloads.some((m) => m.kind === "StatefulSet")).toBe(true);
      for (const deployment of workloads) {
        const pod = podSpec(deployment);
        expect(pod.hostNetwork).toBeUndefined();
        expect(pod.hostPID).toBeUndefined();
        expect(pod.hostIPC).toBeUndefined();
        for (const port of pod.containers.flatMap((c) => c.ports ?? [])) expect(port.hostPort).toBeUndefined();
      }
    }
  });

  test("pods meet the restricted Pod Security Standard and carry the service contract", () => {
    for (const manifests of renders) {
      for (const deployment of manifests.filter((m) => m.kind === "Deployment")) {
        const pod = podSpec(deployment);
        expect(pod.automountServiceAccountToken).toBe(false);
        expect(pod.securityContext?.runAsNonRoot).toBe(true);
        expect(pod.securityContext?.seccompProfile?.type).toBe("RuntimeDefault");
        // preStopSleepSeconds (5) + drainSeconds (30): the preStop sleep counts in the grace period.
        expect(pod.terminationGracePeriodSeconds).toBe(35);
        for (const container of pod.containers) {
          expect(container.securityContext?.allowPrivilegeEscalation).toBe(false);
          expect(container.securityContext?.readOnlyRootFilesystem).toBe(true);
          expect(container.securityContext?.capabilities?.drop).toEqual(["ALL"]);
          expect(container.securityContext?.privileged).toBeUndefined();
          expect(container.livenessProbe).toBeDefined();
          expect(container.readinessProbe).toBeDefined();
          expect(container.lifecycle?.preStop?.sleep?.seconds).toBe(5);
          expect(container.resources?.limits?.memory).toBe("256Mi");
          expect(container.envFrom?.[0]?.configMapRef?.name).toBe(`${RELEASE}-platform`);
        }
      }
    }
  });

  test("ConfigMaps never carry secret-looking keys", () => {
    for (const manifests of renders) {
      for (const configMap of manifests.filter((m) => m.kind === "ConfigMap")) {
        for (const key of Object.keys(configMap.data as YamlMap)) {
          expect(key).not.toMatch(/SECRET|PASSW|PASSPHRASE|PWD|TOKEN|API_?KEY|(^|_)KEY$|DSN|PRIVATE|CREDENTIAL/i);
        }
      }
    }
  });
});

describe("bundled ClickHouse (decision 14)", () => {
  const manifests = render(SHAPES.saas);
  const name = `${RELEASE}-clickhouse`;
  const statefulSet = find(manifests, "StatefulSet", name);
  const pod = podSpec(statefulSet);
  const container = pod.containers[0] as Container;

  function digestOf(image: string): string {
    const match = image.match(/@(sha256:[0-9a-f]{64})$/);
    if (!match?.[1]) throw new Error(`${image} is not pinned by digest`);
    return match[1];
  }

  test("the image is pinned to the same digest as the dev environment and the schema tests", () => {
    const chartDigest = digestOf(container.image);
    expect(container.image).toStartWith("docker.io/clickhouse/clickhouse-server:26.8@");
    const stack = readFileSync(join(REPO_ROOT, "scripts/lib/stack.ts"), "utf8");
    const devImage = stack.match(/clickhouse:\s*"([^"]+)"/)?.[1] ?? "";
    const schemaTests = readFileSync(join(REPO_ROOT, "services/ingest/tests/common/mod.rs"), "utf8");
    const testImage = schemaTests.match(/CLICKHOUSE_IMAGE: &str = "([^"]+)"/)?.[1] ?? "";
    expect(digestOf(devImage)).toBe(chartDigest);
    expect(digestOf(testImage)).toBe(chartDigest);
  });

  test("one replica with a retained data volume, reachable through a ClusterIP Service only", () => {
    const spec = statefulSet.spec as YamlMap & {
      replicas: number;
      serviceName: string;
      persistentVolumeClaimRetentionPolicy: Record<string, string>;
      volumeClaimTemplates: { metadata: { name: string; labels: Record<string, string> }; spec: YamlMap }[];
    };
    expect(spec.replicas).toBe(1);
    expect(spec.serviceName).toBe(name);
    expect(spec.persistentVolumeClaimRetentionPolicy).toEqual({ whenDeleted: "Retain", whenScaled: "Retain" });
    const claim = spec.volumeClaimTemplates[0];
    expect(claim?.metadata.name).toBe("data");
    // volumeClaimTemplates are immutable, so they must not carry the chart version.
    expect(Object.keys(claim?.metadata.labels ?? {})).not.toContain("helm.sh/chart");
    expect(Object.keys(claim?.metadata.labels ?? {})).not.toContain("app.kubernetes.io/version");
    expect(claim?.spec).toMatchObject({ accessModes: ["ReadWriteOnce"], resources: { requests: { storage: "50Gi" } } });
    const mounts = container.volumeMounts as { name: string; mountPath: string; readOnly?: boolean }[];
    expect(mounts).toContainEqual({ name: "data", mountPath: "/var/lib/clickhouse" });

    const service = find(manifests, "Service", name).spec as { type: string; ports: { port: number }[] };
    expect(service.type).toBe("ClusterIP");
    expect(service.ports.map((p) => p.port)).toEqual([8123, 9000]);
  });

  test("the pod meets the restricted Pod Security Standard and joins the ambient mesh", () => {
    const labels = (statefulSet.spec as { template: { metadata: { labels: Record<string, string> } } }).template
      .metadata.labels;
    expect(labels["istio.io/dataplane-mode"]).toBe("ambient");
    expect(pod.automountServiceAccountToken).toBe(false);
    expect(pod.serviceAccountName).toBe(name);
    expect(pod.securityContext?.runAsNonRoot).toBe(true);
    expect(pod.securityContext?.runAsUser).toBe(101);
    expect(pod.securityContext?.seccompProfile?.type).toBe("RuntimeDefault");
    expect(container.securityContext?.allowPrivilegeEscalation).toBe(false);
    expect(container.securityContext?.readOnlyRootFilesystem).toBe(true);
    expect(container.securityContext?.capabilities?.drop).toEqual(["ALL"]);
    expect(container.securityContext?.capabilities?.add).toBeUndefined();
    expect(container.resources?.limits?.memory).toBe("2Gi");
    for (const probe of [container.startupProbe, container.livenessProbe, container.readinessProbe]) {
      expect(probe).toMatchObject({ httpGet: { path: "/ping", port: "http" } });
    }
  });

  test("the bootstrap admin password comes from a mounted Secret, never the environment", () => {
    const env = Object.fromEntries((container.env ?? []).map((e) => [e.name, e]));
    expect(env.CLICKHOUSE_PASSWORD).toBeUndefined();
    expect(env.CLICKHOUSE_USER?.value).toBe("trustiot_admin");
    expect(env.CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT?.value).toBe("1");
    expect(env.CLICKHOUSE_PASSWORD_FILE?.value).toBe("/etc/trustiot/clickhouse-admin/password");
    for (const e of container.env ?? []) expect(e.valueFrom).toBeUndefined();
    const volumes = Object.fromEntries((pod.volumes ?? []).map((v) => [v.name as string, v]));
    expect(volumes["admin-password"]).toMatchObject({
      secret: { secretName: "trustiot-clickhouse-admin", items: [{ key: "password", path: "password" }] },
    });
    // The entrypoint writes the account, with its password, into users.d: memory only.
    expect(volumes["users-d"]).toMatchObject({ emptyDir: { medium: "Memory" } });
  });

  test("the server configuration holds no users, roles, profiles, quotas or row policies", () => {
    const config = find(manifests, "ConfigMap", `${RELEASE}-clickhouse-config`).data as Record<string, string>;
    expect(Object.keys(config)).toEqual(["trustiot.xml"]);
    const xml = config["trustiot.xml"] ?? "";
    expect(xml).toStartWith("<clickhouse>");
    for (const forbidden of [/<users\b/, /<profiles\b/, /<quotas\b/, /<roles\b/, /row_polic/, /user_directories/, /access_management/, /password/i]) {
      expect(xml).not.toMatch(forbidden);
    }
    // Only HTTP and the native protocol listen.
    for (const port of ["mysql_port", "postgresql_port", "interserver_http_port"]) {
      expect(xml).toContain(`<${port} remove="remove"/>`);
    }
    const mounts = container.volumeMounts as { name: string; mountPath: string; readOnly?: boolean }[];
    expect(mounts).toContainEqual({ name: "config", mountPath: "/etc/clickhouse-server/config.d", readOnly: true });
  });

  test("the pod rolls when the server configuration or the admin password revision changes", () => {
    const annotationsOf = (rendered: Manifest[]) =>
      (find(rendered, "StatefulSet", name).spec as { template: { metadata: { annotations: Record<string, string> } } })
        .template.metadata.annotations;
    const before = annotationsOf(manifests);
    expect(before["checksum/clickhouse-config"]).toMatch(/^[0-9a-f]{64}$/);
    expect(before["checksum/admin-secret"]).toMatch(/^[0-9a-f]{64}$/);
    const rotated = annotationsOf(render(SHAPES.saas, ["--set", "clickhouse.bundled.adminSecretRevision=2"]));
    expect(rotated["checksum/admin-secret"]).not.toBe(before["checksum/admin-secret"]);
    expect(rotated["checksum/clickhouse-config"]).toBe(before["checksum/clickhouse-config"]);
  });

  test("external mode renders no ClickHouse workload", () => {
    const external = render(SHAPES.saas, [
      "--set",
      "clickhouse.mode=external",
      "--set",
      "clickhouse.external.url=https://ch.saas.trustiot.test:8443",
      "--set",
      "clickhouse.bundled.adminSecret.name=",
    ]);
    expect(external.filter((m) => m.metadata.name.startsWith(`${RELEASE}-clickhouse`))).toEqual([]);
  });

  test("the ingress e2e overlay installs no ClickHouse, so its helm install --wait needs no admin Secret", () => {
    for (const shape of SHAPE_NAMES) {
      const e2e = render([...SHAPES[shape], "tests/e2e/values-e2e.yaml"], [
        "--set",
        "ingress.direct.tlsSecret=trustiot-ingress-tls",
      ]);
      expect(e2e.filter((m) => m.metadata.name.startsWith(`${RELEASE}-clickhouse`))).toEqual([]);
    }
  });

  test("a storage class and node placement can be set", () => {
    const placed = render(SHAPES.onprem, [
      "--set",
      "clickhouse.bundled.storage.storageClassName=fast-ssd",
      "--set",
      "clickhouse.bundled.nodeSelector.trustiot\\.io/role=data",
    ]);
    const spec = find(placed, "StatefulSet", name).spec as {
      volumeClaimTemplates: { spec: { storageClassName?: string } }[];
      template: { spec: PodSpec };
    };
    expect(spec.volumeClaimTemplates[0]?.spec.storageClassName).toBe("fast-ssd");
    expect(spec.template.spec.nodeSelector).toEqual({ "trustiot.io/role": "data" });
    expect(spec.template.spec.securityContext?.runAsNonRoot).toBe(true);
  });
});

type HttpRouteRule = {
  matches: { path: { type: string; value: string } }[];
  filters: {
    type: string;
    requestHeaderModifier: { set?: { name: string; value: string }[]; remove?: string[] };
  }[];
  backendRefs: { name: string; port: number }[];
};
type AuthzOperation = { paths?: string[]; notPaths?: string[]; hosts?: string[]; notHosts?: string[] };
type AuthzPolicy = {
  targetRefs: { group: string; kind: string; name: string }[];
  action: string;
  provider?: { name: string };
  rules: { to?: { operation: AuthzOperation }[] }[];
};

describe("ingress gateway (decision 27)", () => {
  const GATEWAY = `${RELEASE}-ingress`;
  // The only cookie-free paths: host, exact path and backend (decision 27).
  const PUBLIC_PATHS = {
    saas: [
      ["app.saas.trustiot.test", "/auth/login", `${RELEASE}-identity`],
      ["app.saas.trustiot.test", "/healthz", `${RELEASE}-identity`],
      ["gw.saas.trustiot.test", "/certs/renew", `${RELEASE}-certs`],
      ["gw.saas.trustiot.test", "/mqtt", `${RELEASE}-mqtt-bridge`],
    ],
    onprem: [
      ["trustiot.factory.test", "/auth/login", `${RELEASE}-identity`],
      ["trustiot.factory.test", "/healthz", `${RELEASE}-identity`],
      ["gw.trustiot.factory.test", "/certs/renew", `${RELEASE}-certs`],
      ["gw.trustiot.factory.test", "/mqtt", `${RELEASE}-mqtt-bridge`],
    ],
  } as const;
  const withBackends = (shape: Shape, args: string[] = []) => render([...SHAPES[shape], E2E_VALUES], args);

  function rules(manifests: Manifest[]) {
    return manifests
      .filter((m) => m.kind === "HTTPRoute")
      .flatMap((route) => {
        const spec = route.spec as { hostnames: string[]; parentRefs: { name: string; sectionName: string }[]; rules: HttpRouteRule[] };
        return spec.rules.map((rule) => ({ host: spec.hostnames[0] ?? "", parentRefs: spec.parentRefs, rule }));
      });
  }
  const modifier = (rule: HttpRouteRule) => {
    expect(rule.filters).toHaveLength(1);
    expect(rule.filters[0]?.type).toBe("RequestHeaderModifier");
    return rule.filters[0]?.requestHeaderModifier ?? {};
  };
  const strips = (rule: HttpRouteRule) => (modifier(rule).remove ?? []).includes("x-internal-identity");
  // Underscore spellings of the identity header, removed on every route (Laysi/trustiot-v2#255).
  const UNDERSCORE_SPELLINGS = ["x_internal_identity", "x-internal_identity", "x_internal-identity"];
  // A public path is exempt on its own host with or without a port, as the gateway routes it.
  const withPorts = (hosts: readonly string[]) => hosts.flatMap((host) => [host, `${host}:*`]);
  const policy = (manifests: Manifest[], suffix: string) =>
    find(manifests, "AuthorizationPolicy", `${RELEASE}-ingress-${suffix}`).spec as AuthzPolicy;

  test.each(SHAPE_NAMES)("%s: Istio gateway with 2 replicas, set up through infrastructure.parametersRef", (shape) => {
    const manifests = render(SHAPES[shape]);
    const gateway = find(manifests, "Gateway", GATEWAY).spec as {
      gatewayClassName: string;
      infrastructure: { parametersRef: { group: string; kind: string; name: string } };
      listeners: { name: string; port: number; protocol: string; tls?: unknown; allowedRoutes: unknown }[];
    };
    expect(gateway.gatewayClassName).toBe("istio");
    expect(gateway.infrastructure.parametersRef).toEqual({ group: "", kind: "ConfigMap", name: `${GATEWAY}-options` });
    const options = find(manifests, "ConfigMap", `${GATEWAY}-options`).data as Record<string, string>;
    const deployment = Bun.YAML.parse(options.deployment ?? "") as {
      spec: { replicas: number; template: { spec: { securityContext: unknown; containers: { name: string; resources: unknown }[] } } };
    };
    expect(deployment.spec.replicas).toBe(2);
    expect(deployment.spec.template.spec.securityContext).toEqual({ seccompProfile: { type: "RuntimeDefault" } });
    expect(deployment.spec.template.spec.containers[0]?.name).toBe("istio-proxy");
    expect(Bun.YAML.parse(options.podDisruptionBudget ?? "")).toEqual({ spec: { minAvailable: 1 } });
    const service = Bun.YAML.parse(options.service ?? "") as { spec: { type: string } };
    if (shape === "saas") {
      // Behind Cloudflare Tunnel: ClusterIP only, plain HTTP from cloudflared on the host.
      expect(service.spec.type).toBe("ClusterIP");
      expect(gateway.listeners).toEqual([
        { name: "http", port: 80, protocol: "HTTP", allowedRoutes: { namespaces: { from: "Same" } } },
      ]);
    } else {
      // On-prem: 443 in the customer network, the gateway terminates the outer TLS.
      expect(service.spec.type).toBe("LoadBalancer");
      expect(gateway.listeners).toEqual([
        {
          name: "https",
          port: 443,
          protocol: "HTTPS",
          tls: { mode: "Terminate", certificateRefs: [{ kind: "Secret", group: "", name: "trustiot-ingress-tls" }] },
          allowedRoutes: { namespaces: { from: "Same" } },
        },
      ]);
    }
  });

  test("a single replica gets no PodDisruptionBudget", () => {
    const options = find(render(SHAPES.saas, ["--set", "ingress.replicas=1"]), "ConfigMap", `${GATEWAY}-options`).data as Record<string, string>;
    expect(options.podDisruptionBudget).toBeUndefined();
  });

  test.each(SHAPE_NAMES)("%s: every route sets x-forwarded-proto https and attaches to the one listener", (shape) => {
    const rendered = rules(withBackends(shape));
    expect(rendered.length).toBeGreaterThan(0);
    for (const { rule, parentRefs } of rendered) {
      expect(modifier(rule).set).toEqual([{ name: "x-forwarded-proto", value: "https" }]);
      expect(parentRefs).toEqual([{ name: GATEWAY, sectionName: shape === "saas" ? "http" : "https" }]);
    }
  });

  test.each(SHAPE_NAMES)("%s: public paths are exact, strip the identity header and skip the cookie check only on their host", (shape) => {
    const manifests = withBackends(shape);
    const rendered = rules(manifests);
    const publicRules = rendered.filter(({ rule }) => strips(rule));
    expect(
      publicRules.map(({ host, rule }) => [host, rule.matches[0]?.path.value, rule.backendRefs[0]?.name]).sort(),
    ).toEqual(PUBLIC_PATHS[shape].map((p) => [...p]).sort());
    for (const { rule } of publicRules) {
      expect(modifier(rule).remove).toEqual(["x-internal-identity", ...UNDERSCORE_SPELLINGS]);
      expect(rule.matches).toHaveLength(1);
      expect(rule.matches[0]?.path.type).toBe("Exact");
      expect(rule.backendRefs).toEqual([{ name: rule.backendRefs[0]?.name ?? "", port: 8080 }]);
    }
    const check = policy(manifests, "cookie-check");
    expect(check.action).toBe("CUSTOM");
    expect(check.provider).toEqual({ name: "trustiot-identity" });
    expect(check.targetRefs).toEqual([{ group: "gateway.networking.k8s.io", kind: "Gateway", name: GATEWAY }]);
    const exempt = PUBLIC_PATHS[shape].map(([, path]) => path).sort();
    // Rule 1 checks every other path; one more rule per public path checks it on any other host.
    expect(check.rules[0]).toEqual({ to: [{ operation: { notPaths: exempt } }] });
    expect(check.rules.slice(1)).toEqual(
      exempt.map((path) => ({
        to: [
          {
            operation: {
              paths: [path],
              notHosts: withPorts(PUBLIC_PATHS[shape].filter(([, p]) => p === path).map(([host]) => host)),
            },
          },
        ],
      })),
    );
    // Cookie routes never strip the header (that would drop the identity service's value), only
    // its underscore spellings, and are never exempt.
    const cookieRules = rendered.filter(({ rule }) => !strips(rule));
    expect(cookieRules.length).toBeGreaterThan(0);
    for (const { rule } of cookieRules) {
      expect(modifier(rule).remove).toEqual(UNDERSCORE_SPELLINGS);
      expect(exempt).not.toContain(rule.matches[0]?.path.value);
    }
  });

  test("a public path is neither routed nor exempt while its backend is off", () => {
    const manifests = render(SHAPES.saas, ["--set", "services.identity.enabled=true"]);
    const rendered = rules(manifests);
    expect(rendered.map(({ host, rule }) => `${host}${rule.matches[0]?.path.value}`).sort()).toEqual([
      "app.saas.trustiot.test/auth/login",
      "app.saas.trustiot.test/healthz",
    ]);
    const check = policy(manifests, "cookie-check");
    expect(check.rules[0]).toEqual({ to: [{ operation: { notPaths: ["/auth/login", "/healthz"] } }] });
    expect(check.rules).toHaveLength(3);
  });

  test("the same path exempt on two hosts is checked on every other host", () => {
    const manifests = render(
      [...SHAPES.saas, E2E_VALUES],
      ["--set", "ingress.publicPaths.healthCheck=/up", "--set", "ingress.publicPaths.certRenewal=/up"],
    );
    const check = policy(manifests, "cookie-check");
    expect(check.rules).toContainEqual({
      to: [{ operation: { paths: ["/up"], notHosts: withPorts(["app.saas.trustiot.test", "gw.saas.trustiot.test"]) } }],
    });
  });

  test("/internal is denied at the gateway even when a catch-all route matches it", () => {
    const manifests = withBackends("saas");
    const deny = policy(manifests, "deny-internal");
    expect(deny.action).toBe("DENY");
    expect(deny.targetRefs).toEqual([{ group: "gateway.networking.k8s.io", kind: "Gateway", name: GATEWAY }]);
    expect(deny.rules).toEqual([{ to: [{ operation: { paths: ["/internal*"] } }] }]);
    for (const { rule } of rules(manifests)) expect(rule.matches[0]?.path.value).not.toStartWith("/internal");
  });

  test("NetworkPolicy: behind the tunnel only the host may connect; direct exposure opens 443 only", () => {
    const tunnel = find(render(SHAPES.saas), "NetworkPolicy", GATEWAY).spec as YamlMap;
    expect(tunnel).toEqual({
      podSelector: { matchLabels: { "gateway.networking.k8s.io/gateway-name": GATEWAY } },
      policyTypes: ["Ingress"],
      ingress: [{ from: [{ ipBlock: { cidr: "10.42.0.1/32" } }] }],
    });
    const direct = find(render(SHAPES.onprem), "NetworkPolicy", GATEWAY).spec as YamlMap;
    expect(direct.ingress).toEqual([
      {
        ports: [
          { port: 443, protocol: "TCP" },
          { port: 15021, protocol: "TCP" },
        ],
      },
    ]);
  });

  test("the mesh in deploy/istio decodes %2F and merges slashes, so /internal* catches every spelling", () => {
    // MERGE_SLASHES alone leaves "/%2finternal" past the DENY policy (Laysi/trustiot-v2#255).
    const istiod = Bun.YAML.parse(readFileSync(join(REPO_ROOT, "deploy/istio/istiod-values.yaml"), "utf8")) as {
      meshConfig: { pathNormalization?: { normalization?: string } };
    };
    expect(istiod.meshConfig.pathNormalization).toEqual({ normalization: "DECODE_AND_MERGE_SLASHES" });
  });

  test("the mesh extension provider in deploy/istio fails closed and matches the chart", () => {
    const istiod = Bun.YAML.parse(readFileSync(join(REPO_ROOT, "deploy/istio/istiod-values.yaml"), "utf8")) as {
      meshConfig: { extensionProviders: { name: string; envoyExtAuthzHttp: Record<string, unknown> }[] };
    };
    const ingress = defaults.ingress as { authzProvider: string };
    const provider = istiod.meshConfig.extensionProviders.find((p) => p.name === ingress.authzProvider);
    expect(provider).toBeDefined();
    const http = provider?.envoyExtAuthzHttp ?? {};
    expect(http.failOpen).toBe(false);
    expect(http.statusOnError).toBe("503");
    expect(String(http.timeout)).toMatch(/^0\.\d+s$/);
    expect(http.pathPrefix).toBe("/internal/ext-authz");
    expect(http.includeRequestHeadersInCheck).toEqual(["cookie"]);
    expect(http.headersToUpstreamOnAllow).toEqual(["x-internal-identity"]);
    // The identity service of the release "trustiot" in namespace "trustiot" (README.md).
    const identity = find(render(SHAPES.saas, ["--set", "services.identity.enabled=true"]), "Service", `${RELEASE}-identity`);
    expect(http.service).toBe(`${identity.metadata.name}.${NAMESPACE}.svc.cluster.local`);
    expect(http.port).toBe((identity.spec as { ports: { port: number }[] }).ports[0]?.port);
  });

  test("service pods join the ambient mesh, and podLabels cannot opt them out", () => {
    for (const args of [[], ["--skip-schema-validation", "--set", "services.smoke-echo.podLabels.istio\\.io/dataplane-mode=none"]]) {
      const deployment = find(render([...SHAPES.saas, SMOKE], args), "Deployment", `${RELEASE}-smoke-echo`);
      const labels = (deployment.spec as { template: { metadata: { labels: Record<string, string> } } }).template.metadata.labels;
      expect(labels["istio.io/dataplane-mode"]).toBe("ambient");
    }
  });
});

type MeshPolicy = {
  selector: { matchLabels: Record<string, string> };
  action: string;
  rules?: { from: { source: { principals: string[] } }[] }[];
};
type NetworkPolicyPeer = { podSelector?: { matchLabels: Record<string, string> }; ipBlock?: { cidr: string } };
type NetworkPolicySpec = {
  podSelector: { matchLabels?: Record<string, string> };
  policyTypes: string[];
  ingress?: { from?: NetworkPolicyPeer[]; ports?: { port: number; protocol: string }[] }[];
};

describe("service-to-service authorization (decision 27)", () => {
  const GATEWAY_SA = `${RELEASE}-ingress-istio`;
  const GATEWAY_PODS = { "gateway.networking.k8s.io/gateway-name": `${RELEASE}-ingress` };
  // istio-cni SNATs every connection from the node's network namespace to an ambient pod to this
  // address (kubelet probes, but also any hostNetwork process), and ztunnel lets it through
  // without mTLS or AuthorizationPolicy, so the NetworkPolicy may open only the probe ports.
  const HOST_SOURCE = "169.254.7.127/32";
  const isHostRule = (rule: NonNullable<NetworkPolicySpec["ingress"]>[number]) =>
    (rule.from ?? []).some((peer) => peer.ipBlock?.cidr === HOST_SOURCE);
  // The rule that admits the host source on the given probe ports.
  const hostRule = (...ports: number[]) => ({
    from: [{ ipBlock: { cidr: HOST_SOURCE } }],
    ports: ports.map((port) => ({ port, protocol: "TCP" })),
  });
  // Services probe /healthz and /readyz on their health port (serviceDefaults.healthPort and
  // serviceDefaults.probes), which serves nothing else; their API listens on http.
  const HEALTH_PORT = 8081;
  const API_PORT = 8080;
  // Test stand-ins that run traefik/whoami, which listens on http only, so their overlays probe
  // http. The identity double (identity in the e2e overlay) listens on the health port like a
  // TrustIoT service.
  const WHOAMI_STAND_INS: Record<string, string[]> = {
    [SMOKE]: ["smoke-echo"],
    [E2E_VALUES]: ["assets", "certs", "mqtt-bridge"],
  };
  const standInsOf = (overlays: readonly string[]) => new Set(overlays.flatMap((overlay) => WHOAMI_STAND_INS[overlay] ?? []));
  // Bundled ClickHouse probes /ping on its HTTP port; the native protocol (9000) stays closed.
  const CLICKHOUSE_PROBE_PORT = 8123;
  const principal = (serviceAccount: string, trustDomain = "cluster.local") =>
    `${trustDomain}/ns/${NAMESPACE}/sa/${serviceAccount}`;
  // The ingress e2e backends plus two services nobody routes to: assets calls alerts, nobody
  // calls knowledge.
  const CALLS = '{"assets":["certs","alerts"],"identity":["assets"]}';
  const meshArgs = ["--set", "services.alerts.enabled=true", "--set", "services.knowledge.enabled=true", "--set-json", `mesh.calls=${CALLS}`];
  const withCalls = (shape: Shape = "saas", args: string[] = meshArgs) => render([...SHAPES[shape], E2E_VALUES], args);
  const meshPolicy = (manifests: Manifest[], service: string) =>
    find(manifests, "AuthorizationPolicy", `${RELEASE}-${service}`).spec as MeshPolicy;
  const networkPolicy = (manifests: Manifest[], name: string) =>
    find(manifests, "NetworkPolicy", `${RELEASE}-${name}`).spec as NetworkPolicySpec;
  const podLabels = (deployment: Manifest) =>
    (deployment.spec as { template: { metadata: { labels: Record<string, string> } } }).template.metadata.labels;
  const selects = (selector: Record<string, string>, labels: Record<string, string>) =>
    Object.entries(selector).every(([key, value]) => labels[key] === value);
  const serviceOf = (objectName: string) => objectName.slice(`${RELEASE}-`.length);
  const principalsOf = (policy: MeshPolicy) => policy.rules?.flatMap((r) => r.from.flatMap((f) => f.source.principals)) ?? [];
  // The services whose pods a NetworkPolicy admits on the HBONE port, and whether the ingress
  // gateway's pods are admitted too.
  function hboneSources(manifests: Manifest[], spec: NetworkPolicySpec) {
    const hbone = (spec.ingress ?? []).filter((rule) => !isHostRule(rule));
    for (const rule of hbone) expect(rule.ports).toEqual([{ port: 15008, protocol: "TCP" }]);
    const peers = hbone.flatMap((rule) => rule.from ?? []);
    for (const peer of peers) expect(peer.ipBlock).toBeUndefined();
    const services = manifests
      .filter((m) => m.kind === "Deployment")
      .filter((d) => peers.some((peer) => peer.podSelector && selects(peer.podSelector.matchLabels, podLabels(d))))
      .map((d) => serviceOf(d.metadata.name))
      .sort();
    const gateway = peers.some((peer) => JSON.stringify(peer.podSelector?.matchLabels) === JSON.stringify(GATEWAY_PODS));
    return { services, gateway };
  }

  test("every service runs as its own ServiceAccount", () => {
    const manifests = render([...SHAPES.saas, SMOKE], enableAllServices);
    const accounts = manifests.filter((m) => m.kind === "ServiceAccount").map((m) => m.metadata.name);
    const deployments = manifests.filter((m) => m.kind === "Deployment");
    expect(deployments.length).toBe(plannedServices.length + 1);
    expect(new Set(deployments.map((d) => podSpec(d).serviceAccountName)).size).toBe(deployments.length);
    for (const deployment of deployments) {
      expect(podSpec(deployment).serviceAccountName).toBe(deployment.metadata.name);
      expect(accounts).toContain(deployment.metadata.name);
    }
  });

  test.each(SHAPE_NAMES)("%s: the namespace denies all incoming traffic by default, with or without services", (shape) => {
    for (const args of [[], enableAllServices]) {
      const deny = find(render(SHAPES[shape], args), "NetworkPolicy", `${RELEASE}-default-deny`).spec;
      expect(deny).toEqual({ podSelector: {}, policyTypes: ["Ingress"] });
    }
  });

  test("the call list and the ingress routes generate one AuthorizationPolicy per service", () => {
    const manifests = withCalls();
    const sa = (service: string) => principal(`${RELEASE}-${service}`);
    // Callers from mesh.calls first (sorted), then the ingress gateway where it routes or, for
    // identity, where it sends the cookie check (ext_authz).
    const expected: Record<string, string[]> = {
      identity: [principal(GATEWAY_SA)],
      assets: [sa("identity"), principal(GATEWAY_SA)],
      certs: [sa("assets"), principal(GATEWAY_SA)],
      "mqtt-bridge": [principal(GATEWAY_SA)],
      alerts: [sa("assets")],
      knowledge: [],
    };
    const policies = manifests.filter((m) => m.kind === "AuthorizationPolicy" && !m.metadata.name.startsWith(`${RELEASE}-ingress-`));
    expect(policies.map((m) => m.metadata.name).sort()).toEqual(Object.keys(expected).map((s) => `${RELEASE}-${s}`).sort());
    for (const [service, principals] of Object.entries(expected)) {
      const policy = meshPolicy(manifests, service);
      expect(policy.action).toBe("ALLOW");
      // The selector picks exactly this service's pods.
      for (const deployment of manifests.filter((m) => m.kind === "Deployment")) {
        expect(selects(policy.selector.matchLabels, podLabels(deployment))).toBe(serviceOf(deployment.metadata.name) === service);
      }
      if (principals.length === 0) {
        // An ALLOW policy without rules refuses every connection.
        expect(policy.rules).toBeUndefined();
      } else {
        expect(policy.rules).toEqual([{ from: [{ source: { principals } }] }]);
      }
    }
  });

  test.each(SHAPE_NAMES)("%s: each service's NetworkPolicy admits HBONE from the same callers, and kubelet probes", (shape) => {
    const manifests = withCalls(shape);
    const services = manifests.filter((m) => m.kind === "Deployment").map((d) => serviceOf(d.metadata.name));
    expect(services.length).toBe(6);
    for (const service of services) {
      const spec = networkPolicy(manifests, service);
      for (const deployment of manifests.filter((m) => m.kind === "Deployment")) {
        expect(selects(spec.podSelector.matchLabels ?? {}, podLabels(deployment))).toBe(serviceOf(deployment.metadata.name) === service);
      }
      expect(spec.policyTypes).toEqual(["Ingress"]);
      // The same callers as the AuthorizationPolicy, so either layer alone enforces the list.
      const principals = principalsOf(meshPolicy(manifests, service));
      const sources = hboneSources(manifests, spec);
      expect(sources.gateway).toBe(principals.includes(principal(GATEWAY_SA)));
      expect(sources.services).toEqual(
        principals
          .filter((p) => p !== principal(GATEWAY_SA))
          .map((p) => serviceOf(p.split("/sa/")[1] ?? ""))
          .sort(),
      );
      // Kubelet probes come from istio-cni's SNAT address, admitted on the probe port only: the
      // health port, or http for a whoami stand-in of this overlay.
      expect(spec.ingress?.at(-1)).toEqual(hostRule(standInsOf([E2E_VALUES]).has(service) ? API_PORT : HEALTH_PORT));
      // Nothing else: no rule opens an application port or admits another source.
      expect(spec.ingress?.length).toBe(principals.length > 0 ? 2 : 1);
    }
  });

  test("calls involving a disabled service are skipped", () => {
    const manifests = render(
      [...SHAPES.saas, E2E_VALUES],
      ["--set-json", 'mesh.calls={"assistant":["certs"],"certs":["knowledge","assets"]}'],
    );
    expect(principalsOf(meshPolicy(manifests, "certs"))).toEqual([principal(GATEWAY_SA)]);
    expect(principalsOf(meshPolicy(manifests, "assets"))).toEqual([principal(`${RELEASE}-certs`), principal(GATEWAY_SA)]);
    expect(manifests.some((m) => m.metadata.name === `${RELEASE}-knowledge`)).toBe(false);
  });

  test("with the default empty call list only the ingress gateway reaches its backends and the identity service", () => {
    const manifests = render(SHAPES.saas, enableAllServices);
    for (const service of plannedServices) {
      // Public paths keep identity, certs and mqtt-bridge reachable from the gateway.
      const expected = ["identity", "certs", "mqtt-bridge"].includes(service) ? [principal(GATEWAY_SA)] : [];
      expect(principalsOf(meshPolicy(manifests, service))).toEqual(expected);
      expect(networkPolicy(manifests, service).ingress?.length).toBe(expected.length > 0 ? 2 : 1);
    }
  });

  test("principals use mesh.trustDomain", () => {
    const manifests = withCalls("saas", [...meshArgs, "--set", "mesh.trustDomain=trustiot.example.com"]);
    expect(principalsOf(meshPolicy(manifests, "certs"))).toEqual([
      principal(`${RELEASE}-assets`, "trustiot.example.com"),
      principal(GATEWAY_SA, "trustiot.example.com"),
    ]);
  });

  test("no waypoint by default: ztunnel enforces every service policy at L4", () => {
    for (const shape of SHAPE_NAMES) {
      const manifests = withCalls(shape, [...meshArgs, ...enableAllServices]);
      for (const manifest of manifests) {
        expect(manifest.metadata.labels?.["istio.io/use-waypoint"]).toBeUndefined();
        if (manifest.kind === "Gateway") expect((manifest.spec as { gatewayClassName: string }).gatewayClassName).toBe("istio");
        if (manifest.kind === "AuthorizationPolicy" && !manifest.metadata.name.startsWith(`${RELEASE}-ingress-`)) {
          // Workload selector and source principals only: no targetRefs and no L7 conditions.
          const spec = manifest.spec as MeshPolicy & { targetRefs?: unknown };
          expect(spec.targetRefs).toBeUndefined();
          expect(JSON.stringify(spec.rules ?? [])).not.toMatch(/"(to|when)"/);
        }
      }
      for (const deployment of manifests.filter((m) => m.kind === "Deployment")) {
        expect(podLabels(deployment)["istio.io/use-waypoint"]).toBeUndefined();
      }
    }
  });

  test.each(SHAPE_NAMES)("%s: bundled ClickHouse admits only ingest and telemetry-query, and keeps its probes", (shape) => {
    const manifests = render(SHAPES[shape], enableAllServices);
    const statefulSet = find(manifests, "StatefulSet", `${RELEASE}-clickhouse`);
    const labels = podLabels(statefulSet);
    expect(labels["istio.io/dataplane-mode"]).toBe("ambient");
    const policy = meshPolicy(manifests, "clickhouse");
    expect(policy.action).toBe("ALLOW");
    expect(selects(policy.selector.matchLabels, labels)).toBe(true);
    for (const deployment of manifests.filter((m) => m.kind === "Deployment")) {
      expect(selects(policy.selector.matchLabels, podLabels(deployment))).toBe(false);
    }
    expect(principalsOf(policy)).toEqual([principal(`${RELEASE}-ingest`), principal(`${RELEASE}-telemetry-query`)]);
    const spec = networkPolicy(manifests, "clickhouse");
    expect(selects(spec.podSelector.matchLabels ?? {}, labels)).toBe(true);
    expect(hboneSources(manifests, spec)).toEqual({ services: ["ingest", "telemetry-query"], gateway: false });
    // The kubelet's GET /ping probes arrive from istio-cni's SNAT address, on the HTTP port only.
    expect(spec.ingress).toHaveLength(2);
    expect(spec.ingress?.at(-1)).toEqual(hostRule(CLICKHOUSE_PROBE_PORT));
  });

  test("ClickHouse refuses everyone while its callers are off, and gets no policy when external", () => {
    const off = render(SHAPES.saas);
    expect(meshPolicy(off, "clickhouse").rules).toBeUndefined();
    expect(networkPolicy(off, "clickhouse").ingress).toEqual([hostRule(CLICKHOUSE_PROBE_PORT)]);
    const external = render([...SHAPES.saas, E2E_VALUES], enableAllServices);
    expect(external.some((m) => m.metadata.name === `${RELEASE}-clickhouse`)).toBe(false);
  });

  test.each(SHAPE_NAMES)("%s: every workload's pods have their own NetworkPolicy and AuthorizationPolicy", (shape) => {
    for (const args of [[], enableAllServices]) {
      const manifests = render([...SHAPES[shape], SMOKE], args);
      const workloads = manifests.filter((m) => ["Deployment", "StatefulSet", "DaemonSet", "Job", "CronJob"].includes(m.kind));
      expect(workloads.length).toBeGreaterThan(0);
      const policies = (kind: string) =>
        manifests
          .filter((m) => m.kind === kind && m.metadata.name !== `${RELEASE}-default-deny`)
          .map((m) => {
            const spec = m.spec as { selector?: { matchLabels?: Record<string, string> }; podSelector?: { matchLabels?: Record<string, string> } };
            return (kind === "NetworkPolicy" ? spec.podSelector : spec.selector)?.matchLabels;
          })
          .filter((selector): selector is Record<string, string> => selector !== undefined && Object.keys(selector).length > 0);
      for (const workload of workloads) {
        const labels = podLabels(workload);
        expect({ workload: workload.metadata.name, networkPolicy: policies("NetworkPolicy").some((s) => selects(s, labels)) }).toEqual({
          workload: workload.metadata.name,
          networkPolicy: true,
        });
        expect({ workload: workload.metadata.name, authorizationPolicy: policies("AuthorizationPolicy").some((s) => selects(s, labels)) }).toEqual({
          workload: workload.metadata.name,
          authorizationPolicy: true,
        });
      }
    }
  });

  test("a call to ClickHouse from an unlisted service is not admitted", () => {
    // Values maps merge, so remove the default entry with null.
    const manifests = render(SHAPES.saas, [...enableAllServices, "--set", "mesh.calls.telemetry-query=null"]);
    expect(principalsOf(meshPolicy(manifests, "clickhouse"))).toEqual([principal(`${RELEASE}-ingest`)]);
    expect(hboneSources(manifests, networkPolicy(manifests, "clickhouse")).services).toEqual(["ingest"]);
  });

  test.each([
    ["an unknown caller", '{"billing":["assets"]}', /mesh\.calls: unknown caller "billing"/],
    ["a component as the caller", '{"clickhouse":["assets"]}', /mesh\.calls: unknown caller "clickhouse"/],
    ["an unknown callee", '{"assets":["billing"]}', /mesh\.calls\.assets: unknown callee "billing"/],
    ["callees that are not a list", '{"assets":"certs"}', /mesh\.calls\.assets: must be a list of services/],
  ])("the templates reject a call list with %s, without schema validation", (_name, calls, expected) => {
    const result = helmTemplate([...SHAPES.saas, E2E_VALUES], ["--skip-schema-validation", "--set-json", `mesh.calls=${calls}`]);
    expect(result.code).not.toBe(0);
    expect(result.stderr).toMatch(expected);
  });

  type Probe = { httpGet?: { port: number | string }; tcpSocket?: { port: number | string }; grpc?: { port: number }; exec?: unknown };
  // The TCP ports a workload's startup, liveness and readiness probes connect to, read from the
  // rendered pod spec with named ports resolved through the container's own ports.
  function probePorts(workload: Manifest): number[] {
    const ports = new Set<number>();
    for (const container of podSpec(workload).containers) {
      for (const probe of [container.startupProbe, container.livenessProbe, container.readinessProbe] as (Probe | undefined)[]) {
        if (!probe) continue;
        for (const port of [probe.httpGet?.port, probe.tcpSocket?.port, probe.grpc?.port]) {
          if (port === undefined) continue;
          if (typeof port === "number") {
            ports.add(port);
            continue;
          }
          const named = container.ports?.find((p) => p.name === port);
          if (!named) throw new Error(`${workload.metadata.name}: probe port ${port} is not a container port`);
          ports.add(named.containerPort);
        }
      }
    }
    return [...ports].sort((a, b) => a - b);
  }
  // Every workload with its NetworkPolicy, and the rules of that policy that admit the host source.
  function hostSourceRules(manifests: Manifest[]) {
    const workloads = manifests.filter((m) => ["Deployment", "StatefulSet"].includes(m.kind));
    expect(workloads.length).toBeGreaterThan(0);
    return workloads.map((workload) => {
      const spec = networkPolicy(manifests, serviceOf(workload.metadata.name));
      return { workload, spec, rules: (spec.ingress ?? []).filter(isHostRule) };
    });
  }
  // Overlays on top of each deployment shape, and extra helm arguments.
  const PROBE_RENDERS: [string, string[], string[]][] = [
    ["no services", [], []],
    ["the smoke service", [SMOKE], []],
    ["every service", [SMOKE], enableAllServices],
    ["the ingress e2e overlay with calls", [E2E_VALUES], meshArgs],
  ];

  test.each(SHAPE_NAMES)("%s: the host source reaches only each workload's probe ports", (shape) => {
    for (const [name, overlays, args] of PROBE_RENDERS) {
      const manifests = render([...SHAPES[shape], ...overlays], args);
      for (const { workload, spec, rules } of hostSourceRules(manifests)) {
        const label = `${name}: ${workload.metadata.name}`;
        const expected = probePorts(workload);
        // One rule, with this source alone and exactly the probe ports; none without probe ports.
        expect({ label, rules }).toEqual({ label, rules: expected.length > 0 ? [hostRule(...expected)] : [] });
        // No other rule admits an ipBlock: the HBONE rule names pods only.
        const others = (spec.ingress ?? []).filter((rule) => !isHostRule(rule));
        expect({ label, ipBlocks: others.flatMap((rule) => (rule.from ?? []).filter((peer) => peer.ipBlock)) }).toEqual({ label, ipBlocks: [] });
      }
    }
  }, SLOW_TEST_MS);

  // Laysi/trustiot-v2#286: the node reaches a TrustIoT service on its health port alone, which
  // serves /healthz and /readyz only, and never on its API port, /internal/ included.
  test.each(SHAPE_NAMES)("%s: the node source reaches every service on its health port only, never its API port", (shape) => {
    const renders: [string, string[], string[]][] = [...PROBE_RENDERS, ["every service, no stand-in", [], enableAllServices]];
    const checked = new Set<string>();
    for (const [name, overlays, args] of renders) {
      const manifests = render([...SHAPES[shape], ...overlays], args);
      const standIns = standInsOf(overlays);
      for (const { workload, spec, rules } of hostSourceRules(manifests)) {
        if (workload.kind !== "Deployment") continue;
        const service = serviceOf(workload.metadata.name);
        const label = `${name}: ${service}`;
        // Nothing but the host rule admits an ipBlock, so these are all the ports the node reaches.
        const nodePorts = (spec.ingress ?? []).filter((rule) => (rule.from ?? []).some((peer) => peer.ipBlock)).flatMap((rule) => (rule.ports ?? []).map((p) => p.port));
        if (standIns.has(service)) {
          // A whoami stand-in of a test overlay has no health port to probe.
          expect({ label, rules }).toEqual({ label, rules: [hostRule(API_PORT)] });
          continue;
        }
        expect({ label, rules }).toEqual({ label, rules: [hostRule(HEALTH_PORT)] });
        expect({ label, nodePorts }).toEqual({ label, nodePorts: [HEALTH_PORT] });
        const [container] = podSpec(workload).containers;
        expect({ label, ports: container?.ports?.map(({ name: port, containerPort }) => ({ name: port, containerPort })) }).toEqual({
          label,
          ports: [
            { name: "http", containerPort: API_PORT },
            { name: "health", containerPort: HEALTH_PORT },
          ],
        });
        for (const probe of [container?.startupProbe, container?.livenessProbe, container?.readinessProbe]) {
          expect({ label, port: (probe as Probe | undefined)?.httpGet?.port }).toEqual({ label, port: "health" });
        }
        // The Service exposes the API only: the kubelet is the health port's one client.
        const servicePorts = (find(manifests, "Service", workload.metadata.name).spec as { ports: { name: string; port: number }[] }).ports;
        expect({ label, servicePorts: servicePorts.map((p) => [p.name, p.port]) }).toEqual({ label, servicePorts: [["http", API_PORT]] });
        checked.add(service);
      }
    }
    // Every planned service was checked with the chart's defaults, the identity double too.
    expect([...checked].sort()).toEqual([...plannedServices].sort());
  }, SLOW_TEST_MS);

  test("healthPort moves the health port, and the node source and probes follow it", () => {
    const manifests = render([...SHAPES.saas], ["--set", "services.assets.enabled=true", "--set", "services.assets.healthPort=9091"]);
    const workload = find(manifests, "Deployment", `${RELEASE}-assets`);
    expect(podSpec(workload).containers[0]?.ports?.find((p) => p.name === "health")?.containerPort).toBe(9091);
    expect(networkPolicy(manifests, "assets").ingress?.filter(isHostRule)).toEqual([hostRule(9091)]);
  });

  test("no ingress route can reach the health port", () => {
    const result = helmTemplate(
      [...SHAPES.saas, SMOKE],
      ["--set-json", 'ingress.routes=[{"host":"app","path":"/","match":"PathPrefix","service":"smoke-echo","port":"health"}]'],
    );
    expect(result.code).not.toBe(0);
    expect(result.stderr).toMatch(/service "smoke-echo" has no port named "health"/);
  });

  test("probe ports follow the probe definitions: named and numeric ports, tcpSocket, grpc, and exec", () => {
    const PORTS = [
      { name: "http", containerPort: 8080 },
      { name: "grpc", containerPort: 9090 },
      { name: "admin", containerPort: 9901 },
      { name: "data", containerPort: 7000 },
    ];
    // smoke-echo with the given probes and ports; with routed=true the ingress gateway routes to
    // it, so it also has an HBONE rule.
    const smoke = (probes: object, routed = false) => {
      const args = ["--set-json", `services.smoke-echo.probes=${JSON.stringify(probes)}`, "--set-json", `services.smoke-echo.ports=${JSON.stringify(PORTS)}`];
      if (routed) args.push("--set-json", 'ingress.routes=[{"host":"app","path":"/","match":"PathPrefix","service":"smoke-echo"}]');
      const manifests = render([...SHAPES.saas, SMOKE], args);
      const workload = find(manifests, "Deployment", `${RELEASE}-smoke-echo`);
      return { manifests, spec: networkPolicy(manifests, "smoke-echo"), rendered: probePorts(workload) };
    };
    const exec = { exec: { command: ["true"] } };
    // Each probe kind and handler contributes its port; the application's other ports stay closed.
    const mixed = smoke({
      startup: { tcpSocket: { port: "admin" } },
      liveness: { grpc: { port: 9090 } },
      readiness: { httpGet: { path: "/readyz", port: 8080 } },
    });
    expect(mixed.rendered).toEqual([8080, 9090, 9901]);
    expect(mixed.spec.ingress).toEqual([hostRule(8080, 9090, 9901)]);
    // A port used by several probes is listed once; startup {} is off and an exec probe adds nothing.
    const shared = smoke({ startup: {}, liveness: { httpGet: { path: "/healthz", port: "admin" } }, readiness: { tcpSocket: { port: "admin" } } });
    expect(shared.spec.ingress).toEqual([hostRule(9901)]);
    expect(smoke({ startup: exec, liveness: { tcpSocket: { port: "grpc" } }, readiness: exec }).spec.ingress).toEqual([hostRule(9090)]);
    // Only exec probes: the host source gets no rule at all, never one without ports (all ports).
    const execOnly = smoke({ startup: {}, liveness: exec, readiness: exec });
    expect(execOnly.spec.policyTypes).toEqual(["Ingress"]);
    expect(execOnly.spec.ingress).toEqual([]);
    const routed = smoke({ startup: {}, liveness: exec, readiness: exec }, true);
    expect(routed.spec.ingress?.filter(isHostRule)).toEqual([]);
    expect(hboneSources(routed.manifests, routed.spec)).toEqual({ services: [], gateway: true });
  }, SLOW_TEST_MS);

  test("a probe on a port name the workload does not have fails the render", () => {
    const result = helmTemplate(
      [...SHAPES.saas, SMOKE],
      ["--set-json", 'services.smoke-echo.probes={"readiness":{"tcpSocket":{"port":"metrics"}}}'],
    );
    expect(result.code).not.toBe(0);
    expect(result.stderr).toMatch(/services\.smoke-echo: probes\.readiness uses port "metrics", which is not in ports/);
  });
});

describe("values validation rejects unsafe or incomplete settings", () => {
  const saas = [...SHAPES.saas, SMOKE];
  const onprem = [...SHAPES.onprem, SMOKE];
  const cases: [string, readonly string[], string[], RegExp][] = [
    ["no deployment shape", [], [], /deploymentMode/],
    ["no instance overlay", ["values-saas.yaml"], [], /\/hosts\/app/],
    ["IP address as gateway host", saas, ["--set", "hosts.gateway=203.0.113.7"], /\/hosts\/gateway/],
    ["upper-case host", saas, ["--set", "hosts.app=App.trustiot.test"], /\/hosts\/app/],
    ["on-prem behind Cloudflare Tunnel", onprem, ["--set", "ingress.exposure=tunnel"], /must be 'direct'/],
    ["misspelled top-level key", saas, ["--set", "ingres.exposure=direct"], /ingres/],
    ["unknown service field", saas, ["--set", "services.smoke-echo.hostNetwork=true"], /hostNetwork/],
    ["plain-text secret in values", saas, ["--set", "qdrant.external.apiKey=plain-text"], /apiKey/],
    [
      "credentials inside a URL",
      saas,
      ["--set", "objectStorage.endpoint=https://key:secret@objects.saas.trustiot.test"],
      /\/objectStorage\/endpoint/,
    ],
    [
      "SaaS endpoint over plain http",
      saas,
      ["--set", "ai.chat.baseUrl=http://llm.saas.trustiot.test/v1"],
      /\/ai\/chat\/baseUrl/,
    ],
    [
      "secret-like env var with a literal value",
      saas,
      ["--set", "services.smoke-echo.env[0].name=DB_PASSWORD", "--set", "services.smoke-echo.env[0].value=x"],
      /\/services\/smoke-echo\/env\/0/,
    ],
    [
      "secret-like env var read from a ConfigMap",
      saas,
      [
        "--set-json",
        'services.smoke-echo.env=[{"name":"DB_PASSWORD","valueFrom":{"configMapKeyRef":{"name":"settings","key":"db-password"}}}]',
      ],
      /\/services\/smoke-echo\/env\/0\/valueFrom/,
    ],
    [
      "env var with both a value and a source",
      saas,
      [
        "--set-json",
        'services.smoke-echo.env=[{"name":"LOG_FORMAT","value":"json","valueFrom":{"configMapKeyRef":{"name":"settings","key":"log-format"}}}]',
      ],
      /\/services\/smoke-echo\/env\/0/,
    ],
    ["Postgres without server verification", saas, ["--set", "postgres.external.sslMode=require"], /sslMode/],
    ["external Qdrant without a URL", saas, ["--set", "qdrant.external.url="], /\/qdrant\/external\/url/],
    ["Sentry on without a DSN Secret", saas, ["--set", "observability.sentry.dsnSecret.name="], /dsnSecret/],
    [
      "bundled ClickHouse without an admin password Secret",
      saas,
      ["--set", "clickhouse.bundled.adminSecret.name="],
      /\/clickhouse\/bundled\/adminSecret\/name/,
    ],
    ["ClickHouse image without a digest", saas, ["--set", "clickhouse.bundled.image.digest="], /digest/],
    [
      "bundled ClickHouse without a sized volume",
      saas,
      ["--set", "clickhouse.bundled.storage.size="],
      /\/clickhouse\/bundled\/storage\/size/,
    ],
    [
      "ClickHouse bootstrap account named default",
      saas,
      ["--set", "clickhouse.bundled.adminUser=default"],
      /\/clickhouse\/bundled\/adminUser/,
    ],
    [
      "ClickHouse pod allowed to run as root",
      saas,
      ["--set", "clickhouse.bundled.podSecurityContext.runAsNonRoot=false"],
      /runAsNonRoot/,
    ],
    [
      "ClickHouse access settings smuggled into values",
      saas,
      ["--set", "clickhouse.bundled.users.reader.password=plain-text"],
      /users/,
    ],
    [
      "SMTP sender with a header injection",
      onprem,
      ["--set-json", 'smtp.from="a@factory.test\\r\\nBcc: x@evil.test"'],
      /\/smtp\/from/,
    ],
    ["root user allowed", saas, ["--set", "serviceDefaults.podSecurityContext.runAsNonRoot=false"], /runAsNonRoot/],
    [
      "privilege escalation allowed",
      saas,
      ["--set", "services.smoke-echo.securityContext.allowPrivilegeEscalation=true"],
      /allowPrivilegeEscalation/,
    ],
    [
      "added Linux capability",
      saas,
      ["--set", "services.smoke-echo.securityContext.capabilities.add[0]=NET_ADMIN"],
      /capabilities/,
    ],
    [
      "capabilities no longer dropped",
      saas,
      ["--set-json", "services.smoke-echo.securityContext.capabilities.drop=[]"],
      /capabilities\/drop/,
    ],
    ["privileged container", saas, ["--set", "services.smoke-echo.securityContext.privileged=true"], /privileged/],
    [
      "unconfined seccomp for the pod",
      saas,
      ["--set", "serviceDefaults.podSecurityContext.seccompProfile.type=Unconfined"],
      /seccompProfile\/type/,
    ],
    [
      "unconfined seccomp for a container",
      saas,
      ["--set", "services.smoke-echo.securityContext.seccompProfile.type=Unconfined"],
      /seccompProfile\/type/,
    ],
    [
      "unconfined AppArmor",
      saas,
      ["--set", "services.smoke-echo.podSecurityContext.appArmorProfile.type=Unconfined"],
      /appArmorProfile\/type/,
    ],
    [
      "legacy AppArmor annotation",
      saas,
      [
        "--set-json",
        'services.smoke-echo.podAnnotations={"container.apparmor.security.beta.kubernetes.io/smoke-echo":"unconfined"}',
      ],
      /invalid propertyName 'container\.apparmor\.security\.beta\.kubernetes\.io\/smoke-echo'/,
    ],
    ["unmasked /proc", saas, ["--set", "services.smoke-echo.securityContext.procMount=Unmasked"], /procMount/],
    [
      "custom SELinux user",
      saas,
      ["--set", "services.smoke-echo.podSecurityContext.seLinuxOptions.user=system_u"],
      /seLinuxOptions/,
    ],
    [
      "pod sysctls",
      saas,
      ["--set-json", 'services.smoke-echo.podSecurityContext.sysctls=[{"name":"kernel.msgmax","value":"1"}]'],
      /sysctls/,
    ],
    [
      "runAsNonRoot removed from the defaults",
      saas,
      ["--set", "serviceDefaults.podSecurityContext.runAsNonRoot=null"],
      /runAsNonRoot/,
    ],
    [
      "privileged container port",
      saas,
      ["--set-json", 'services.smoke-echo.ports=[{"name":"http","containerPort":80}]'],
      /containerPort/,
    ],
    ["malformed image digest", saas, ["--set", "services.smoke-echo.image.digest=sha256:abc"], /digest/],
    [
      "pod label that would change the selector",
      saas,
      ["--set", "services.smoke-echo.podLabels.app\\.kubernetes\\.io/component=other"],
      /invalid propertyName 'app\.kubernetes\.io\/component'/,
    ],
    [
      "pod annotation that would pin the config checksum",
      saas,
      ["--set", "services.smoke-echo.podAnnotations.checksum/platform-config=stale"],
      /invalid propertyName 'checksum\/platform-config'/,
    ],
    ["no time to drain after SIGTERM", saas, ["--set", "services.smoke-echo.drainSeconds=0"], /drainSeconds/],
    [
      "credentials handed to every service",
      saas,
      ["--set-json", 'serviceDefaults.credentials=["ai"]'],
      /\/serviceDefaults\/credentials/,
    ],
    ["unknown credential", saas, ["--set-json", 'services.smoke-echo.credentials=["postgres"]'], /credentials\/0/],
    ["missing liveness probe", saas, ["--set-json", "services.smoke-echo.probes.liveness={}"], /probes.liveness is required/],
    [
      "probe on an undeclared port",
      saas,
      ["--set-json", 'services.smoke-echo.probes.readiness={"httpGet":{"path":"/readyz","port":"metrics"}}'],
      /probes.readiness uses port "metrics", which is not in ports/,
    ],
    [
      "duplicate port names",
      saas,
      [
        "--set-json",
        'services.smoke-echo.ports=[{"name":"http","containerPort":8080},{"name":"http","containerPort":9090}]',
      ],
      /port names and container ports must be unique/,
    ],
    [
      "a port named health",
      saas,
      ["--set-json", 'services.smoke-echo.ports=[{"name":"http","containerPort":8080},{"name":"health","containerPort":9000}]'],
      /services\.smoke-echo: ports must not use the name health or the health port 8081/,
    ],
    [
      "a port on the health port's number",
      saas,
      ["--set-json", 'services.smoke-echo.ports=[{"name":"http","containerPort":8081}]'],
      /services\.smoke-echo: ports must not use the name health or the health port 8081/,
    ],
    ["health port below 1024", saas, ["--set", "services.smoke-echo.healthPort=81"], /\/services\/smoke-echo\/healthPort/],
    ["health port as a string", saas, ["--set-string", "services.smoke-echo.healthPort=8081"], /\/services\/smoke-echo\/healthPort/],
    ["no health port", saas, ["--set", "serviceDefaults.healthPort=null"], /healthPort/],
    [
      "env that overrides a platform setting",
      saas,
      ["--set-json", 'services.smoke-echo.env=[{"name":"TRUSTIOT_CLIENT_IP_SOURCE","value":"x-forwarded-for"}]'],
      /\/services\/smoke-echo\/env\/0\/name/,
    ],
    [
      "env that redefines OTEL_SERVICE_NAME",
      saas,
      ["--set-json", 'services.smoke-echo.env=[{"name":"OTEL_SERVICE_NAME","value":"other"}]'],
      /\/services\/smoke-echo\/env\/0\/name/,
    ],
    [
      "env that redefines SENTRY_DSN",
      saas,
      [
        "--set-json",
        'services.smoke-echo.env=[{"name":"SENTRY_DSN","valueFrom":{"secretKeyRef":{"name":"other","key":"dsn"}}}]',
      ],
      /\/services\/smoke-echo\/env\/0\/name/,
    ],
    [
      "Secret read in serviceDefaults.env, which every service gets",
      saas,
      [
        "--set-json",
        'serviceDefaults.env=[{"name":"SHARED_TOKEN","valueFrom":{"secretKeyRef":{"name":"shared","key":"token"}}}]',
      ],
      /\/serviceDefaults\/env\/0/,
    ],
    [
      "AI provider keys for a service other than ai-gateway",
      saas,
      ["--set", "services.assistant.enabled=true", "--set-json", 'services.assistant.credentials=["ai"]'],
      /services\.assistant: only ai-gateway may receive the ai credential/,
    ],
    [
      "Qdrant keys for a service other than knowledge",
      saas,
      ["--set-json", 'services.smoke-echo.credentials=["qdrant-write"]'],
      /services\.smoke-echo: only knowledge may receive the qdrant-write credential/,
    ],
    [
      "dependency credential read through custom env",
      saas,
      [
        "--set-json",
        'services.smoke-echo.env=[{"name":"LLM_API_KEY","valueFrom":{"secretKeyRef":{"name":"trustiot-ai-chat","key":"api-key"}}}]',
      ],
      /env reads the dependency credential trustiot-ai-chat\/api-key/,
    ],
    [
      "ClickHouse bootstrap admin password read through custom env",
      saas,
      [
        "--set-json",
        'services.smoke-echo.env=[{"name":"CH_ADMIN_PASSWORD","valueFrom":{"secretKeyRef":{"name":"trustiot-clickhouse-admin","key":"password"}}}]',
      ],
      /env reads the dependency credential trustiot-clickhouse-admin\/password/,
    ],
    [
      "duplicate env names",
      saas,
      [
        "--set-json",
        'serviceDefaults.env=[{"name":"RUST_LOG","value":"info"}]',
        "--set-json",
        'services.smoke-echo.env=[{"name":"RUST_LOG","value":"debug"}]',
      ],
      /env names must be unique/,
    ],
    [
      "missing readiness probe",
      saas,
      ["--set-json", "services.smoke-echo.probes.readiness={}"],
      /probes.readiness is required/,
    ],
    // Ingress gateway (decision 27).
    ["wildcard public path", saas, ["--set", "ingress.publicPaths.healthCheck=/healthz/*"], /\/ingress\/publicPaths\/healthCheck/],
    ["public path with a dot segment", saas, ["--set", "ingress.publicPaths.login=/auth/../login"], /\/ingress\/publicPaths\/login/],
    ["public path with a trailing slash", saas, ["--set", "ingress.publicPaths.mqtt=/mqtt/"], /\/ingress\/publicPaths\/mqtt/],
    ["upper-case public path", saas, ["--set", "ingress.publicPaths.mqtt=/MQTT"], /\/ingress\/publicPaths\/mqtt/],
    ["encoded public path", saas, ["--set", "ingress.publicPaths.certRenewal=/certs%2frenew"], /\/ingress\/publicPaths\/certRenewal/],
    ["public path under /internal", saas, ["--set", "ingress.publicPaths.login=/internal/login"], /\/ingress\/publicPaths\/login/],
    ["extra public path", saas, ["--set", "ingress.publicPaths.metrics=/metrics"], /metrics/],
    [
      "cookie route under /internal",
      saas,
      ["--set-json", 'ingress.routes=[{"host":"app","path":"/internal/tenants","match":"PathPrefix","service":"smoke-echo"}]'],
      /\/ingress\/routes\/0\/path/,
    ],
    [
      "cookie route with an encoded path",
      saas,
      ["--set-json", 'ingress.routes=[{"host":"app","path":"/api%2fv1","match":"PathPrefix","service":"smoke-echo"}]'],
      /\/ingress\/routes\/0\/path/,
    ],
    [
      "cookie route with a regular expression match",
      saas,
      ["--set-json", 'ingress.routes=[{"host":"app","path":"/api","match":"RegularExpression","service":"smoke-echo"}]'],
      /\/ingress\/routes\/0\/match/,
    ],
    [
      "cookie route on an unknown host",
      saas,
      ["--set-json", 'ingress.routes=[{"host":"admin","path":"/","match":"PathPrefix","service":"smoke-echo"}]'],
      /\/ingress\/routes\/0\/host/,
    ],
    [
      "cookie route to a disabled service",
      saas,
      ["--set-json", 'ingress.routes=[{"host":"app","path":"/api","match":"PathPrefix","service":"knowledge"}]'],
      /ingress\.routes\[0\]: service "knowledge" is not enabled/,
    ],
    [
      "cookie route to an undeclared port",
      saas,
      ["--set-json", 'ingress.routes=[{"host":"app","path":"/api","match":"PathPrefix","service":"smoke-echo","port":"metrics"}]'],
      /ingress\.routes\[0\]: service "smoke-echo" has no port named "metrics"/,
    ],
    [
      "cookie route on a public path",
      [...saas, E2E_VALUES],
      ["--set-json", 'ingress.routes=[{"host":"gateway","path":"/mqtt","match":"PathPrefix","service":"assets"}]'],
      /ingress\.routes\[0\]: \/mqtt is already routed for hosts\.gateway/,
    ],
    [
      "two public paths on one host share a path",
      [...saas, E2E_VALUES],
      ["--set", "ingress.publicPaths.healthCheck=/auth/login"],
      /ingress\.publicPaths\.healthCheck: \/auth\/login is listed twice for hosts\.app/,
    ],
    ["whole network behind the tunnel", saas, ["--set-json", 'ingress.tunnel.allowedSources=["0.0.0.0/0"]'], /allowedSources/],
    ["no source behind the tunnel", saas, ["--set-json", "ingress.tunnel.allowedSources=[]"], /allowedSources/],
    ["direct exposure without a TLS certificate", onprem, ["--set", "ingress.direct.tlsSecret="], /\/ingress\/direct\/tlsSecret/],
    ["no ingress replica", saas, ["--set", "ingress.replicas=0"], /\/ingress\/replicas/],
    ["no ext_authz provider", saas, ["--set", "ingress.authzProvider="], /\/ingress\/authzProvider/],
    ["ingress without a memory limit", saas, ["--set", "ingress.resources.limits.memory=null"], /\/ingress\/resources\/limits/],
    [
      "pod label that would take a service out of the mesh",
      saas,
      ["--set", "services.smoke-echo.podLabels.istio\\.io/dataplane-mode=none"],
      /invalid propertyName 'istio\.io\/dataplane-mode'/,
    ],
    ["unknown mesh field", saas, ["--set", "mesh.waypoint=true"], /waypoint/],
    ["IP address as the trust domain", saas, ["--set", "mesh.trustDomain=10.0.0.1"], /\/mesh\/trustDomain/],
    ["callee listed twice", saas, ["--set-json", 'mesh.calls={"smoke-echo":["smoke-echo","smoke-echo"]}'], /\/mesh\/calls/],
    ["callees that are not a list", saas, ["--set-json", 'mesh.calls={"smoke-echo":"smoke-echo"}'], /\/mesh\/calls/],
  ];

  test.each(cases)("%s", (_name, files, args, expected) => {
    const result = helmTemplate(files, args);
    expect(result.code).not.toBe(0);
    expect(result.stderr).toMatch(expected);
  });
});

describe("the templates keep the secret and env boundaries without schema validation", () => {
  const files = [...SHAPES.saas, SMOKE];
  const cases: [string, string[], RegExp][] = [
    [
      "env that overrides a platform setting",
      ["--set-json", 'services.smoke-echo.env=[{"name":"TRUSTIOT_CLIENT_IP_SOURCE","value":"x-forwarded-for"}]'],
      /services\.smoke-echo: env TRUSTIOT_CLIENT_IP_SOURCE: names starting with TRUSTIOT_, SENTRY_ or OTEL_ are set by the chart/,
    ],
    [
      "env that redefines SENTRY_ENVIRONMENT in lower case",
      ["--set-json", 'services.smoke-echo.env=[{"name":"sentry_environment","value":"other"}]'],
      /names starting with TRUSTIOT_, SENTRY_ or OTEL_ are set by the chart/,
    ],
    [
      "Secret read in serviceDefaults.env",
      [
        "--set-json",
        'serviceDefaults.env=[{"name":"SHARED_TOKEN","valueFrom":{"secretKeyRef":{"name":"shared","key":"token"}}}]',
      ],
      /serviceDefaults\.env: SHARED_TOKEN must not read a Secret/,
    ],
    [
      "secret-like env var with a literal value",
      ["--set-json", 'services.smoke-echo.env=[{"name":"DB_PASSWORD","value":"x"}]'],
      /services\.smoke-echo: env DB_PASSWORD looks like a secret, so it must come from valueFrom\.secretKeyRef only/,
    ],
    [
      "secret-like env var read from a ConfigMap",
      [
        "--set-json",
        'services.smoke-echo.env=[{"name":"DB_PASSWORD","valueFrom":{"configMapKeyRef":{"name":"settings","key":"db-password"}}}]',
      ],
      /env DB_PASSWORD looks like a secret/,
    ],
    ["credentials in serviceDefaults", ["--set-json", 'serviceDefaults.credentials=["smtp"]'], /serviceDefaults\.credentials is not allowed/],
    [
      "unknown credential",
      ["--set-json", 'services.smoke-echo.credentials=["postgres"]'],
      /services\.smoke-echo: unknown credential "postgres"/,
    ],
    [
      "wildcard public path",
      ["--set", "services.identity.enabled=true", "--set", "ingress.publicPaths.healthCheck=/healthz/*"],
      /ingress\.publicPaths\.healthCheck: "\/healthz\/\*" must be one exact lower-case path/,
    ],
    [
      "public path with a dot segment, even with its backend off",
      ["--set", "ingress.publicPaths.login=/auth/../login"],
      /ingress\.publicPaths\.login: "\/auth\/\.\.\/login" must be one exact lower-case path/,
    ],
    [
      "public path under /internal",
      ["--set", "ingress.publicPaths.mqtt=/internal/mqtt"],
      /ingress\.publicPaths\.mqtt: paths under \/internal are never routed/,
    ],
    [
      "cookie route under /internal",
      ["--set-json", 'ingress.routes=[{"host":"app","path":"/internal/tenants","match":"PathPrefix","service":"smoke-echo"}]'],
      /ingress\.routes\[0\]: paths under \/internal are never routed/,
    ],
    [
      "cookie route with a dot segment",
      ["--set-json", 'ingress.routes=[{"host":"app","path":"/api/../x","match":"PathPrefix","service":"smoke-echo"}]'],
      /ingress\.routes\[0\]: "\/api\/\.\.\/x" must be '\/' or a lower-case path/,
    ],
    [
      "cookie route with a regular expression match",
      ["--set-json", 'ingress.routes=[{"host":"app","path":"/api","match":"RegularExpression","service":"smoke-echo"}]'],
      /ingress\.routes\[0\]: match must be PathPrefix or Exact/,
    ],
    [
      "cookie route on an unknown host",
      ["--set-json", 'ingress.routes=[{"host":"admin","path":"/","match":"PathPrefix","service":"smoke-echo"}]'],
      /ingress\.routes\[0\]: host must be app, gateway or ota/,
    ],
  ];

  test.each(cases)("%s", (_name, args, expected) => {
    const result = helmTemplate(files, ["--skip-schema-validation", ...args]);
    expect(result.code).not.toBe(0);
    expect(result.stderr).toMatch(expected);
  });

  test("service settings with a double underscore after TRUSTIOT stay available to custom env", () => {
    const env = podSpec(
      find(
        render(files, ["--set-json", 'services.smoke-echo.env=[{"name":"TRUSTIOT__HTTP__LISTEN_ADDR","value":"0.0.0.0:8080"}]']),
        "Deployment",
        `${RELEASE}-smoke-echo`,
      ),
    ).containers[0]?.env;
    expect(env?.map((e) => e.name)).toContain("TRUSTIOT__HTTP__LISTEN_ADDR");
  });
});

describe("service settings merge", () => {
  const files = [...SHAPES.saas, SMOKE];
  const smokeDeployment = (args: string[]) => podSpec(find(render(files, args), "Deployment", `${RELEASE}-smoke-echo`));

  test("false and 0 override the defaults", () => {
    const pod = smokeDeployment([
      "--set",
      "services.smoke-echo.preStopSleepSeconds=0",
      "--set",
      "services.smoke-echo.automountServiceAccountToken=true",
      "--set",
      "services.smoke-echo.securityContext.readOnlyRootFilesystem=false",
    ]);
    expect(pod.automountServiceAccountToken).toBe(true);
    expect(pod.containers[0]?.lifecycle).toBeUndefined();
    // Without the preStop sleep the grace period is the drain time alone.
    expect(pod.terminationGracePeriodSeconds).toBe(30);
    expect(pod.containers[0]?.securityContext?.readOnlyRootFilesystem).toBe(false);
  });

  test("maps merge key by key", () => {
    const container = smokeDeployment(["--set", "services.smoke-echo.resources.limits.memory=512Mi"]).containers[0];
    expect(container?.resources).toEqual({
      requests: { cpu: "20m", memory: "32Mi" },
      limits: { cpu: "1", memory: "512Mi" },
    });
  });

  test("a probe is replaced as a whole, and {} turns off the startup probe", () => {
    const container = smokeDeployment([
      "--set-json",
      'services.smoke-echo.probes.liveness={"exec":{"command":["/healthcheck"]},"periodSeconds":15}',
      "--set-json",
      "services.smoke-echo.probes.startup={}",
    ]).containers[0];
    expect(container?.livenessProbe).toEqual({ exec: { command: ["/healthcheck"] }, periodSeconds: 15 });
    expect(container?.startupProbe).toBeUndefined();
    expect(container?.readinessProbe).toMatchObject({ httpGet: { path: "/readyz", port: "http" } });
  });

  test("env is appended to serviceDefaults.env, after the chart's own variables", () => {
    const container = smokeDeployment([
      "--set-json",
      'serviceDefaults.env=[{"name":"RUST_LOG","value":"info"}]',
    ]).containers[0];
    expect(container?.env?.map((e) => e.name)).toEqual([
      "OTEL_SERVICE_NAME",
      "SENTRY_DSN",
      "RUST_LOG",
      "WHOAMI_PORT_NUMBER",
    ]);
  });

  test("custom env can read the service's own Secret, a ConfigMap and pod fields", () => {
    const env = [
      { name: "SESSION_KEY", valueFrom: { secretKeyRef: { name: "smoke-session", key: "key", optional: true } } },
      { name: "LOG_FORMAT", valueFrom: { configMapKeyRef: { name: "smoke-settings", key: "log-format" } } },
      { name: "POD_NAME", valueFrom: { fieldRef: { fieldPath: "metadata.name" } } },
    ];
    const container = smokeDeployment(["--set-json", `services.smoke-echo.env=${JSON.stringify(env)}`]).containers[0];
    expect(container?.env?.map((e) => e.name)).toEqual([
      "OTEL_SERVICE_NAME",
      "SENTRY_DSN",
      "SESSION_KEY",
      "LOG_FORMAT",
      "POD_NAME",
    ]);
  });

  test("an image digest pins the image; without one the tag defaults to appVersion", () => {
    expect(smokeDeployment([]).containers[0]?.image).toBe(
      "docker.io/traefik/whoami:v1.12.0@sha256:c4717a8d1f0134a7444e24f881160e033991f23027c6c5a9a3f8fd22e70d1d44",
    );
    const manifests = render(files, [
      "--set",
      "services.identity.enabled=true",
      "--set",
      "services.identity.image.digest=sha256:" + "a".repeat(64),
    ]);
    expect(podSpec(find(manifests, "Deployment", `${RELEASE}-identity`)).containers[0]?.image).toBe(
      `ghcr.io/laysi/trustiot-v2/identity:${chart.appVersion}@sha256:${"a".repeat(64)}`,
    );
  });

  test("pods roll when the platform settings change", () => {
    const checksum = (args: string[]) => {
      const deployment = find(render(files, args), "Deployment", `${RELEASE}-smoke-echo`);
      const template = (deployment.spec as { template: { metadata: { annotations: Record<string, string> } } }).template;
      return template.metadata.annotations["checksum/platform-config"];
    };
    expect(checksum([])).toMatch(/^[a-f0-9]{64}$/);
    expect(checksum([])).not.toBe(checksum(["--set", "hosts.ota=ota2.saas.trustiot.test"]));
  });

  test("object names are prefixed with the release, without repeating the chart name", () => {
    const names = (release: string) =>
      render(files, [], release)
        .filter((m) => m.kind === "Deployment")
        .map((m) => m.metadata.name);
    expect(names("trustiot-staging")).toEqual(["trustiot-staging-smoke-echo"]);
    expect(names("lab")).toEqual(["lab-trustiot-smoke-echo"]);
  });

  test("the longest release name keeps every object name distinct and within 63 characters", () => {
    // Helm caps release names at 53 characters.
    const release = `trustiot-${"x".repeat(44)}`;
    const manifests = render(
      [...files, E2E_VALUES],
      [...enableAllServices, "--set-json", 'ingress.routes=[{"host":"ota","path":"/","match":"PathPrefix","service":"gateway-ops"}]'],
      release,
    );
    const kinds = ["ServiceAccount", "Service", "Deployment", "ConfigMap", "Gateway", "HTTPRoute", "AuthorizationPolicy", "NetworkPolicy"];
    for (const kind of kinds) {
      const names = manifests.filter((m) => m.kind === kind).map((m) => m.metadata.name);
      expect(names.length).toBeGreaterThan(0);
      expect(new Set(names).size).toBe(names.length);
      for (const name of names) expect(name.length).toBeLessThanOrEqual(63);
    }
    // Istio adds "-istio" to the Gateway name for the Deployment, Service and ServiceAccount.
    for (const gateway of manifests.filter((m) => m.kind === "Gateway")) {
      expect(`${gateway.metadata.name}-istio`.length).toBeLessThanOrEqual(63);
    }
    const deployments = manifests.filter((m) => m.kind === "Deployment").map((m) => m.metadata.name);
    for (const service of [...plannedServices, "smoke-echo"]) {
      expect(deployments.some((name) => name.endsWith(`-${service}`))).toBe(true);
    }
  });

  test("podLabels and podAnnotations cannot replace the chart's selector labels or checksum", () => {
    const pod = (args: string[]) => {
      const deployment = find(render(files, args), "Deployment", `${RELEASE}-smoke-echo`);
      return (deployment.spec as { template: { metadata: { labels: Record<string, string>; annotations: Record<string, string> } } })
        .template.metadata;
    };
    const expected = pod([]);
    // Bypass the schema to prove the templates hold on their own.
    const overridden = pod([
      "--skip-schema-validation",
      "--set",
      "services.smoke-echo.podLabels.app\\.kubernetes\\.io/component=other",
      "--set",
      "services.smoke-echo.podAnnotations.checksum/platform-config=stale",
      "--set",
      "services.smoke-echo.podLabels.team=edge",
    ]);
    expect(overridden.labels["app.kubernetes.io/component"]).toBe("smoke-echo");
    expect(overridden.labels.team).toBe("edge");
    expect(overridden.annotations["checksum/platform-config"]).toBe(expected.annotations["checksum/platform-config"]);
  });
});

describe("documentation and packaging", () => {
  const readme = readFileSync(join(CHART_DIR, "README.md"), "utf8");

  // Leaf paths of values.yaml; services.<entry> collapses to services.<name>, and
  // Kubernetes-native objects under serviceDefaults count as one field.
  function documentedPaths(value: unknown, path: string): string[] {
    const prefix = ["serviceDefaults.", "clickhouse.bundled."].find((p) => path.startsWith(p));
    const relative = prefix ? path.slice(prefix.length) : "";
    if (!isMap(value) || Object.keys(value).length === 0 || KUBERNETES_OBJECT_FIELDS.has(relative)) {
      return [path.replace(/^services\.[^.]+/, "services.<name>")];
    }
    return Object.entries(value).flatMap(([key, child]) => documentedPaths(child, path ? `${path}.${key}` : key));
  }

  test("README.md lists every values field and every planned service", () => {
    const paths = new Set(documentedPaths(defaults, ""));
    const missing = [...paths].filter((path) => !readme.includes(`\`${path}\``));
    expect(missing).toEqual([]);
    const missingServices = plannedServices.filter((name) => !readme.includes(`\`${name}\``));
    expect(missingServices).toEqual([]);
  });

  test("README.md documents the optional per-service and per-port fields", () => {
    const schema = JSON.parse(readFileSync(join(CHART_DIR, "values.schema.json"), "utf8")) as {
      definitions: Record<string, { properties: Record<string, unknown> }>;
    };
    const fields = [
      ...Object.keys(schema.definitions.serviceSpec?.properties ?? {}).map((f) => `services.<name>.${f}`),
      ...Object.keys(schema.definitions.containerPort?.properties ?? {}).map((f) => `ports[].${f}`),
    ];
    expect(fields.filter((field) => !readme.includes(`\`${field}\``))).toEqual([]);
  });

  test("planned services match docs/architecture/deployment.md", () => {
    const deploymentDoc = readFileSync(join(REPO_ROOT, "docs/architecture/deployment.md"), "utf8");
    const documented = [...deploymentDoc.matchAll(/`services\/([a-z-]+)`/g)].map((m) => m[1] ?? "");
    expect(documented.length).toBeGreaterThan(0);
    expect([...plannedServices].sort()).toEqual([...new Set(documented)].sort());
  });

  test("the packaged chart ships both deployment shapes and leaves the tests out", () => {
    const out = mkdtempSync(join(tmpdir(), "trustiot-chart-"));
    try {
      const packaged = run([HELM, "package", ".", "--destination", out]);
      expect(packaged.code).toBe(0);
      const listing = run(["tar", "-tzf", join(out, `trustiot-${chart.version}.tgz`)]);
      const files = listing.stdout.split("\n").filter(Boolean);
      expect(files).toContain("trustiot/values-saas.yaml");
      expect(files).toContain("trustiot/values-onprem.yaml");
      expect(files).toContain("trustiot/values.schema.json");
      expect(files.filter((f) => f.startsWith("trustiot/tests/"))).toEqual([]);
    } finally {
      rmSync(out, { recursive: true, force: true });
    }
  });
});
