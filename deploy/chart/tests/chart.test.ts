// Chart tests: helm lint, kubeconform and the chart's own rules for both deployment shapes.
// Run from the repository root: bun test deploy/chart
// Needs helm and kubeconform from .prototools; kubeconform downloads schemas on first use.
import { describe, expect, test } from "bun:test";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { isAbsolute, join, relative } from "node:path";
import { checkRenderedManifests } from "../../../scripts/ci/check-secret-handling.ts";
import { routeListProblems, sampleRequests } from "./ingress-authz.ts";
import type { Exception, PolicyRule, RouteRule } from "./ingress-authz.ts";
import { COMMAND_TIMEOUT_MS, SLOW_TEST_MS, throwIfTimedOut, useProcessTestTimeouts } from "./timing.ts";

// Every test starts helm, and some also kubeconform: see timing.ts for the limits.
useProcessTestTimeouts();

const CHART_DIR = join(import.meta.dir, "..");
const REPO_ROOT = join(CHART_DIR, "..", "..");

// Runs the versions pinned in .prototools directly instead of through proto's shims: a shim
// can write its own notices to stdout (for example while another proto process holds a
// lock), which would corrupt rendered manifests.
function pinnedTool(tool: string): string {
  const cmd = ["proto", "bin", tool, "--reporter", "text"];
  const result = Bun.spawnSync({ cmd, cwd: REPO_ROOT, stdout: "pipe", stderr: "pipe", timeout: COMMAND_TIMEOUT_MS });
  throwIfTimedOut(cmd, result);
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

const SHAPES = {
  saas: ["values-saas.yaml", "tests/values/saas-instance.yaml"],
  onprem: ["values-onprem.yaml", "tests/values/onprem-instance.yaml"],
} as const;
type Shape = keyof typeof SHAPES;
const SHAPE_NAMES = Object.keys(SHAPES) as Shape[];
const SMOKE = "tests/values/smoke.yaml";
// The migration Job's accounts for an external Postgres and ClickHouse, required in external mode.
const EXTERNAL_MIGRATION = [
  "--set",
  "postgres.external.migrationSecret.name=trustiot-pg-migrate",
  "--set",
  "clickhouse.external.migrationSecret.name=trustiot-ch-migrate",
];
// Ingress e2e overlay: identity, certs, mqtt-bridge, assets and web (the SPA's static files) on,
// plus a cookie route for /api.
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
  env?: { name: string; value?: string; valueFrom?: { secretKeyRef?: { name: string; key: string; optional?: boolean }; fieldRef?: { fieldPath: string } } }[];
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

function run(cmd: readonly string[], stdin?: string) {
  const proc = Bun.spawnSync({
    cmd: [...cmd],
    cwd: CHART_DIR,
    stdin: stdin === undefined ? "ignore" : new TextEncoder().encode(stdin),
    stdout: "pipe",
    stderr: "pipe",
    timeout: COMMAND_TIMEOUT_MS,
  });
  throwIfTimedOut(
    cmd,
    proc,
    cmd[0] === KUBECONFORM
      ? "kubeconform fetches schemas over the network into its cache, and a download can stall; " +
          "or the machine was so busy that it never ran (check `uptime`)"
      : undefined,
  );
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

// files/postgres/system-databases.sql split into statements: they are separated by a blank line,
// and lines that start with `--` are comments. tools/migrate/tests/system_databases.rs splits it
// the same way and runs each statement; the Cluster's postInitSQL must hold exactly these.
const SYSTEM_DATABASE_SQL = join(CHART_DIR, "files/postgres/system-databases.sql");
function systemDatabaseStatements(text: string): string[] {
  return text
    .split(/\n[ \t]*\n/)
    .map((block) => block.replace(/^--.*$/gm, "").trim())
    .filter((statement) => statement !== "");
}

function find(manifests: Manifest[], kind: string, name: string): Manifest {
  const found = manifests.find((m) => m.kind === kind && m.metadata.name === name);
  if (!found) throw new Error(`${kind}/${name} not rendered`);
  return found;
}

// Deployments of TrustIoT services, as opposed to in-cluster components such as the collector.
function isServiceDeployment(m: Manifest): boolean {
  return m.kind === "Deployment" && m.metadata.labels?.["app.kubernetes.io/component"] !== "otel-collector";
}

function podSpec(deployment: Manifest): PodSpec {
  const spec = deployment.spec as { template: { spec: PodSpec } };
  return spec.template.spec;
}

function platformConfig(manifests: Manifest[]): Record<string, string> {
  return find(manifests, "ConfigMap", `${RELEASE}-platform`).data as Record<string, string>;
}

/**
 * The password Secrets of the migration Job a render must declare: one per enabled service that
 * lists postgres under mesh.calls (its Deployment carries <prefix>__DATABASE__USERNAME, the prefix
 * being TRUSTIOT or, for the AI gateway, AI_GATEWAY) and one for the collector, when it is rendered.
 */
function jobPasswordSecretNames(manifests: Manifest[]): string[] {
  const names: string[] = [];
  for (const deployment of manifests.filter((m) => m.kind === "Deployment")) {
    const env = podSpec(deployment).containers[0]?.env ?? [];
    if (env.some((e) => /^(TRUSTIOT|AI_GATEWAY)__DATABASE__USERNAME$/.test(e.name))) names.push(`${deployment.metadata.name}-password`);
    if (deployment.metadata.name.endsWith("-otel-collector")) names.push(`${deployment.metadata.name}-password`);
  }
  return names.sort();
}

/**
 * The bundled ClickHouse's admin password Secret, when its pod generates the password: the Secret
 * its init container `admin-password` fills (Laysi/trustiot-v2#403).
 */
function adminPasswordSecretNames(manifests: Manifest[]): string[] {
  return manifests
    .filter((m) => m.kind === "StatefulSet" && m.metadata.labels?.["app.kubernetes.io/component"] === "clickhouse")
    .flatMap((m) => ((podSpec(m).initContainers ?? []) as Container[]).filter((c) => c.name === "admin-password"))
    .map((c) => c.env?.find((e) => e.name === "TRUSTIOT__ADMIN__SECRET")?.value ?? "(no TRUSTIOT__ADMIN__SECRET)");
}

/**
 * The identity service's signing keys Secret, while the service is enabled: the Secret its init
 * container `signing-keys` fills (Laysi/trustiot-v2#404).
 */
function signingKeysSecretNames(manifests: Manifest[]): string[] {
  return manifests
    .filter((m) => m.kind === "Deployment" && m.metadata.labels?.["app.kubernetes.io/component"] === "identity")
    .flatMap((m) => ((podSpec(m).initContainers ?? []) as Container[]).filter((c) => c.name === "signing-keys"))
    .map((c) => c.env?.find((e) => e.name === "TRUSTIOT__KEYS__SECRET")?.value ?? "(no TRUSTIOT__KEYS__SECRET)");
}

/**
 * The certs service's issuing CA keys Secret, while the service is enabled: the Secret its init
 * container `ca-keys` fills (Laysi/trustiot-v2#78).
 */
function caKeysSecretNames(manifests: Manifest[]): string[] {
  return manifests
    .filter((m) => m.kind === "Deployment" && m.metadata.labels?.["app.kubernetes.io/component"] === "certs")
    .flatMap((m) => ((podSpec(m).initContainers ?? []) as Container[]).filter((c) => c.name === "ca-keys"))
    .map((c) => c.env?.find((e) => e.name === "TRUSTIOT__KEYS__SECRET")?.value ?? "(no TRUSTIOT__KEYS__SECRET)");
}

/**
 * The MQTT broker's accounts Secret, while the broker is enabled: the Secret its init container
 * `accounts` fills with the cloud services' broker passwords (Laysi/trustiot-v2#84).
 */
function brokerAccountsSecretNames(manifests: Manifest[]): string[] {
  return manifests
    .filter((m) => m.kind === "StatefulSet" && m.metadata.labels?.["app.kubernetes.io/component"] === "mqtt-broker")
    .flatMap((m) => ((podSpec(m).initContainers ?? []) as Container[]).filter((c) => c.name === "accounts"))
    .map((c) => c.env?.find((e) => e.name === "TRUSTIOT__ACCOUNTS__SECRET")?.value ?? "(no TRUSTIOT__ACCOUNTS__SECRET)");
}

/**
 * The MQTT broker's server certificate Secret, while the broker is enabled: the Secret its pod
 * mounts as `server-tls` and the certificate CronJob fills (Laysi/trustiot-v2#535). Declared with
 * or without the certs service, since the broker pod cannot start without the Secret it mounts.
 */
function brokerServerTlsSecretNames(manifests: Manifest[]): string[] {
  return manifests
    .filter((m) => m.kind === "StatefulSet" && m.metadata.labels?.["app.kubernetes.io/component"] === "mqtt-broker")
    .flatMap((m) => (podSpec(m).volumes ?? []).filter((v) => v.name === "server-tls"))
    .map((v) => (v.secret as { secretName?: string } | undefined)?.secretName ?? "(no server-tls Secret)");
}

/** Every Secret a render must declare empty for tools/migrate to fill. */
function passwordSecretNames(manifests: Manifest[]): string[] {
  return [
    ...jobPasswordSecretNames(manifests),
    ...adminPasswordSecretNames(manifests),
    ...signingKeysSecretNames(manifests),
    ...caKeysSecretNames(manifests),
    ...brokerAccountsSecretNames(manifests),
    ...brokerServerTlsSecretNames(manifests),
  ].sort();
}

function readYaml(relativePath: string): YamlMap {
  const parsed: unknown = Bun.YAML.parse(readFileSync(join(CHART_DIR, relativePath), "utf8"));
  if (!isMap(parsed)) throw new Error(`${relativePath} is not a YAML map`);
  return parsed;
}

const defaults = readYaml("values.yaml");
const chart = readYaml("Chart.yaml") as { version: string; appVersion: string };
const plannedServices = Object.keys(defaults.services as YamlMap);
// The AI gateway refuses to start without a provider key and a role, so the chart refuses to render
// it without them: one provider and one role for both shapes (the SaaS overlay adds more).
const AI_GATEWAY_SETTINGS = [
  "--set",
  "ai.providers.anthropic.apiKeySecret.name=trustiot-anthropic",
  "--set-json",
  'ai.roles.test={"models":["anthropic/claude-test"]}',
];
// ingest and gateway-ops need the in-cluster MQTT broker, and the render fails without it
// (Laysi/trustiot-v2#642, Laysi/trustiot-v2#721), so every service comes with the broker.
const enableAllServices = [
  ...plannedServices.flatMap((name) => ["--set", `services.${name}.enabled=true`]),
  "--set",
  "mqttBroker.enabled=true",
  ...AI_GATEWAY_SETTINGS,
];
// Every service that renders without the broker: all but ingest and gateway-ops.
const enableAllServicesWithoutBroker = [
  ...enableAllServices,
  ...["mqttBroker.enabled=false", "services.ingest.enabled=false", "services.gateway-ops.enabled=false"].flatMap((value) => ["--set", value]),
];

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

  test("services stay off until enabled: only the platform settings, the ingress, bundled Postgres and ClickHouse, the collector, the migration Job and default deny render", () => {
    const manifests = render(files);
    // The Job's name ends in a hash of its spec (see the migration Job's tests).
    const names = manifests.map((m) => `${m.kind}/${m.metadata.name}`.replace(/-migrate-[0-9a-f]{8}$/, "-migrate-<hash>"));
    expect(names.sort()).toEqual(
      [
        `ServiceAccount/${RELEASE}-migrate`,
        `Job/${RELEASE}-migrate-<hash>`,
        `AuthorizationPolicy/${RELEASE}-migrate`,
        `NetworkPolicy/${RELEASE}-migrate`,
        // The collector's password: declared empty, filled by the Job, which alone may patch it;
        // the collector waits for the Job.
        `Secret/${RELEASE}-otel-collector-password`,
        `Role/${RELEASE}-migrate`,
        `RoleBinding/${RELEASE}-migrate`,
        `Role/${RELEASE}-migrate-status`,
        `RoleBinding/${RELEASE}-migrate-status`,
        `ImageCatalog/${RELEASE}-postgres`,
        `Cluster/${RELEASE}-postgres`,
        `Database/${RELEASE}-postgres-trustiot`,
        `NetworkPolicy/${RELEASE}-postgres`,
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
        // The bootstrap admin password: declared empty, filled by ClickHouse's init container,
        // whose Role may get and patch only it (Laysi/trustiot-v2#403).
        `Secret/${RELEASE}-clickhouse-admin-password`,
        `Role/${RELEASE}-clickhouse`,
        `RoleBinding/${RELEASE}-clickhouse`,
        `ConfigMap/${RELEASE}-otel-collector-config`,
        `ServiceAccount/${RELEASE}-otel-collector`,
        `Service/${RELEASE}-otel-collector`,
        `Deployment/${RELEASE}-otel-collector`,
        `AuthorizationPolicy/${RELEASE}-otel-collector`,
        `NetworkPolicy/${RELEASE}-otel-collector`,
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
    // smtp.credentialsSecret names a Secret: the relay needs an account, so the identity service
    // reports a missing one (its reference to the Secret is optional).
    expect(config.TRUSTIOT_SMTP_AUTH_REQUIRED).toBe("true");
  });

  test("on-prem: direct ingress, bundled Qdrant, customer SMTP, Sentry off", () => {
    const config = platformConfig(render(SHAPES.onprem));
    expect(config.TRUSTIOT_DEPLOYMENT_MODE).toBe("onprem");
    expect(config.TRUSTIOT_CLIENT_IP_SOURCE).toBe("x-forwarded-for-rightmost");
    expect(config.TRUSTIOT_QDRANT_URL).toBeUndefined();
    expect(config.TRUSTIOT_SMTP_HOST).toBe("smtp.factory.test");
    // The customer's relay takes mail without an account: no credentials Secret.
    expect(config.TRUSTIOT_SMTP_AUTH_REQUIRED).toBeUndefined();
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

  test("external Postgres and ClickHouse are wired only in external mode, bundled ones in-cluster", () => {
    const bundled = platformConfig(render(SHAPES.onprem));
    // CloudNativePG's read-write Service, whose server certificate names this host.
    expect(bundled.TRUSTIOT_POSTGRES_HOST).toBe(`${RELEASE}-postgres-rw.${NAMESPACE}.svc`);
    expect(bundled.TRUSTIOT_POSTGRES_PORT).toBe("5432");
    expect(bundled.TRUSTIOT_POSTGRES_DATABASE).toBe("trustiot");
    expect(bundled.TRUSTIOT_POSTGRES_SSLMODE).toBe("verify-full");
    // The CA file is per service: only the services that use Postgres have it.
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
        // The services alone: the migration Job would need a CA for ClickHouse.
        "--set",
        "migrate.enabled=false",
      ]),
    );
    expect(external.TRUSTIOT_POSTGRES_HOST).toBe("pg.factory.test");
    expect(external.TRUSTIOT_POSTGRES_PORT).toBe("5432");
    expect(external.TRUSTIOT_POSTGRES_SSLMODE).toBe("verify-full");
    expect(external.TRUSTIOT_POSTGRES_CA_FILE).toBeUndefined();
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
        return ref ? [[e.name, `${ref.name}/${ref.key}${ref.optional === true ? " (optional)" : ""}`]] : [];
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
    ]);
    expect(secretRefs(manifests, "identity")).toEqual({
      SENTRY_DSN: "trustiot-sentry/dsn",
      // Its own database passwords only, from its own Secret (the migration Job's tests): the
      // service role, and the platform role that reads the event log (Laysi/trustiot-v2#130).
      TRUSTIOT__DATABASE__PASSWORD: "trustiot-identity-password/identity_app",
      TRUSTIOT__DATABASE__PLATFORM_PASSWORD: "trustiot-identity-password/identity_platform",
      // The relay account for login codes: values.yaml lists `smtp` for identity. Optional, so
      // a missing Secret does not keep the pod, and the ext_authz check, from starting; the
      // service reports it (TRUSTIOT_SMTP_AUTH_REQUIRED). The only optional reference.
      TRUSTIOT_SMTP_USERNAME: "trustiot-smtp/username (optional)",
      TRUSTIOT_SMTP_PASSWORD: "trustiot-smtp/password (optional)",
    });
    expect(secretRefs(manifests, "knowledge")).toEqual({
      SENTRY_DSN: "trustiot-sentry/dsn",
      TRUSTIOT_QDRANT_READ_API_KEY: "trustiot-qdrant/read-api-key",
      TRUSTIOT_S3_ACCESS_KEY_ID: "trustiot-object-storage/access-key-id",
      TRUSTIOT_S3_SECRET_ACCESS_KEY: "trustiot-object-storage/secret-access-key",
    });
    // values.yaml lists `ai` for the AI gateway: one key per configured provider, each from the
    // Secret the overlay names.
    // It lists postgres under mesh.calls too (values.yaml): its own role's password, under its
    // own prefix.
    expect(secretRefs(manifests, "ai-gateway")).toEqual({
      SENTRY_DSN: "trustiot-sentry/dsn",
      AI_GATEWAY__DATABASE__PASSWORD: "trustiot-ai-gateway-password/ai_gateway_app",
      TRUSTIOT_AI_ANTHROPIC_API_KEY: "trustiot-anthropic/api-key",
      TRUSTIOT_AI_OPENAI_API_KEY: "trustiot-openai/api-key",
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

  test("an external Postgres CA is mounted read-only into the services that use Postgres, a ClickHouse CA into every service", () => {
    const external = [
      "--set",
      "postgres.mode=external",
      "--set",
      "postgres.external.host=pg.saas.trustiot.test",
      "--set",
      "postgres.external.caSecret.name=trustiot-pg-ca",
      "--set",
      "postgres.external.caSecret.key=root.crt",
      "--set",
      "clickhouse.mode=external",
      "--set",
      "clickhouse.external.url=https://ch.saas.trustiot.test:8443",
      "--set",
      "clickhouse.external.caSecret.name=trustiot-ch-ca",
      ...EXTERNAL_MIGRATION,
    ];
    const manifests = render([...SHAPES.saas, SMOKE], [...external, "--set", "services.identity.enabled=true"]);
    const postgresVolume = {
      name: "postgres-ca",
      secret: { secretName: "trustiot-pg-ca", items: [{ key: "root.crt", path: "ca.crt" }] },
    };
    const postgresMount = { name: "postgres-ca", mountPath: "/etc/trustiot/postgres-ca", readOnly: true };
    const clickhouseMount = { name: "clickhouse-ca", mountPath: "/etc/trustiot/clickhouse-ca", readOnly: true };
    // identity lists postgres under mesh.calls (values.yaml); the smoke service does not.
    const identity = podSpec(find(manifests, "Deployment", `${RELEASE}-identity`));
    expect(identity.volumes).toContainEqual(postgresVolume);
    expect(identity.containers[0]?.volumeMounts).toContainEqual(postgresMount);
    expect(identity.containers[0]?.env).toContainEqual({ name: "TRUSTIOT_POSTGRES_CA_FILE", value: "/etc/trustiot/postgres-ca/ca.crt" });
    const smoke = podSpec(find(manifests, "Deployment", `${RELEASE}-smoke-echo`));
    expect(JSON.stringify(smoke)).not.toContain("postgres-ca");
    expect(JSON.stringify(smoke)).not.toContain("TRUSTIOT_POSTGRES_CA_FILE");
    for (const pod of [identity, smoke]) expect(pod.containers[0]?.volumeMounts).toContainEqual(clickhouseMount);
    expect(platformConfig(manifests).TRUSTIOT_CLICKHOUSE_CA_FILE).toBe("/etc/trustiot/clickhouse-ca/ca.crt");
    // Listing postgres is what gives a service the CA: assets without it has none.
    const assets = (calls: string) =>
      podSpec(find(render([...SHAPES.saas, SMOKE], [...external, "--set", "services.assets.enabled=true", "--set-json", `mesh.calls.assets=${calls}`]), "Deployment", `${RELEASE}-assets`));
    const unlisted = assets("[]");
    expect(JSON.stringify(unlisted)).not.toContain("postgres-ca");
    expect(JSON.stringify(unlisted)).not.toContain("TRUSTIOT_POSTGRES_CA_FILE");
    const listed = assets('["postgres"]');
    expect(listed.volumes).toContainEqual(postgresVolume);
    expect(listed.containers[0]?.volumeMounts).toContainEqual(postgresMount);
    // A service the chart has no connection budget for, such as the smoke stand-in, cannot list
    // postgres in either mode: its pools would go uncounted (README.md, 「叢集內的 Postgres」).
    // The migration Job is off: the smoke service's image is not a TrustIoT release, which the
    // Job's tag rule refuses for a service that uses Postgres.
    const smokeListed = helmTemplate(
      [...SHAPES.saas, SMOKE],
      [...external, "--set-json", 'mesh.calls.smoke-echo=["postgres"]', "--set", "migrate.enabled=false"],
    );
    expect(smokeListed.code).not.toBe(0);
    expect(smokeListed.stderr).toContain("services.smoke-echo lists postgres under mesh.calls, but the chart has no connection budget for it");
  });
});

describe("internal identity (decision 27, Laysi/trustiot-v2#61; keys generated in the cluster, Laysi/trustiot-v2#404)", () => {
  const e2e = [...SHAPES.saas, E2E_VALUES];
  const SIGNING_KEYS = `${RELEASE}-identity-signing-keys`;
  const PUBLIC_KEYS = `${RELEASE}-internal-identity-keys`;
  const LEGACY_X = "jxSgv4kffdHZ1jN2sf_IVpE4VGbJdheBqhCAAqI89Q8";
  const legacy = [
    "--set",
    "internalIdentity.signingKeySecret.name=trustiot-identity-signing",
    "--set-json",
    `internalIdentity.publicKeys=[{"kid":"2026-10-a","x":"${LEGACY_X}"}]`,
  ];
  type Rule = { apiGroups?: string[]; resources?: string[]; resourceNames?: string[]; verbs?: string[] };
  const env = (manifests: Manifest[], service: string) =>
    podSpec(find(manifests, "Deployment", `${RELEASE}-${service}`)).containers[0]?.env ?? [];
  const envValue = (manifests: Manifest[], service: string, name: string) =>
    env(manifests, service).find((e) => e.name === name)?.value;
  const initsOf = (pod: PodSpec) => (pod.initContainers ?? []) as Container[];
  const ROTATION = `${RELEASE}-identity-key-rotation`;
  // The suspended CronJob an operator starts the first platform administrator's bootstrap from
  // (Laysi/trustiot-v2#582); its own tests are in the describe block below.
  const BOOTSTRAP = `${RELEASE}-platform-admin-bootstrap`;
  // The CronJob that keeps the MQTT broker's server certificate (Laysi/trustiot-v2#535), which
  // tests/values/smoke.yaml deploys with the certs service; its tests are in the broker's block.
  const BROKER_CERTIFICATE = `${RELEASE}-mqtt-broker-certificate`;
  const otherCronJobs = (manifests: Manifest[]) => manifests.filter((m) => m.kind === "CronJob" && m.metadata.name !== BOOTSTRAP && m.metadata.name !== BROKER_CERTIFICATE);
  const cronPod = (cronJob: Manifest) =>
    (cronJob.spec as { jobTemplate: { spec: { template: { spec: PodSpec } } } }).jobTemplate.spec.template.spec;
  // Every pod template the chart renders: Deployments, StatefulSets, Jobs, the rotation CronJob;
  // CloudNativePG's Cluster has none.
  const podsOf = (manifests: Manifest[]) =>
    manifests
      .filter((m) => ["Deployment", "StatefulSet", "Job", "DaemonSet", "CronJob"].includes(m.kind))
      .map((m) => ({ name: `${m.kind}/${m.metadata.name}`, pod: m.kind === "CronJob" ? cronPod(m) : podSpec(m) }));

  test("every service reads the public keys from the ConfigMap it mounts whole, never from a value", () => {
    for (const shape of SHAPE_NAMES) {
      for (const args of [["--set", "services.identity.enabled=true"], enableAllServices]) {
        const manifests = render([...SHAPES[shape], SMOKE], args);
        const config = platformConfig(manifests);
        expect(config.TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE).toBe("/etc/trustiot/internal-identity/jwks.json");
        // The JWK Set itself is never a platform setting any more: a value in the ConfigMap
        // would need a restart of every service to change.
        expect(config.TRUSTIOT_INTERNAL_IDENTITY_KEYS).toBeUndefined();
        // Declared without data: Helm never owns what the init container writes.
        const configMap = find(manifests, "ConfigMap", PUBLIC_KEYS);
        expect(Object.keys(configMap).sort()).toEqual(["apiVersion", "kind", "metadata"]);
        expect(configMap.metadata.annotations?.["helm.sh/resource-policy"]).toBe("keep");
        const services = manifests.filter(isServiceDeployment);
        expect(services.length).toBeGreaterThan(0);
        for (const deployment of services) {
          const pod = podSpec(deployment);
          expect(pod.volumes, deployment.metadata.name).toContainEqual({
            name: "internal-identity-keys",
            configMap: { name: PUBLIC_KEYS, optional: true },
          });
          // The whole ConfigMap: a subPath mount never sees an update.
          expect(pod.containers[0]?.volumeMounts, deployment.metadata.name).toContainEqual({
            name: "internal-identity-keys",
            mountPath: "/etc/trustiot/internal-identity",
            readOnly: true,
          });
          expect(JSON.stringify(pod.containers[0]?.volumeMounts)).not.toContain("subPath");
        }
      }
    }
  });

  test("without the identity service nothing publishes keys, so no service is told where they are", () => {
    // A service that verifies internal identity then refuses to start for want of the setting,
    // as it did without internalIdentity.publicKeys before Laysi/trustiot-v2#404, instead of
    // waiting unready for keys that never come; the example service in the e2e overlays runs
    // without its identity routes.
    for (const shape of SHAPE_NAMES) {
      for (const files of [[...SHAPES[shape], SMOKE], [...SHAPES[shape], "tests/e2e/values-otel-e2e.yaml"]]) {
        const manifests = render(files, ["--set", "services.assets.enabled=true"]);
        expect(platformConfig(manifests).TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE).toBeUndefined();
        expect(manifests.filter((m) => m.metadata.name === PUBLIC_KEYS)).toEqual([]);
        expect(JSON.stringify(manifests)).not.toContain("internal-identity-keys");
        expect(manifests.filter(isServiceDeployment).length).toBeGreaterThan(0);
      }
    }
    // The values are checked all the same (the render failures below run without the service).
  });

  test("only the identity service mounts its signing keys, read-only and mode 0440, in a container without a token", () => {
    const everything = [...enableAllServices, "--set", "observability.collector.enabled=true"];
    for (const shape of SHAPE_NAMES) {
      const manifests = render([...SHAPES[shape], SMOKE], everything);
      const identity = podSpec(find(manifests, "Deployment", `${RELEASE}-identity`));
      // Mode 0440: root and the pod's fsGroup read the file, no other user in the container can.
      // Optional: the Secret has no key before the init container's first run.
      expect(identity.volumes).toContainEqual({
        name: "identity-signing-keys",
        secret: {
          secretName: SIGNING_KEYS,
          defaultMode: 0o440,
          optional: true,
          items: [{ key: "signing-keys.json", path: "signing-keys.json" }],
        },
      });
      expect(identity.securityContext?.fsGroup).toBe(65532);
      expect(identity.containers[0]?.volumeMounts).toContainEqual({
        name: "identity-signing-keys",
        mountPath: "/etc/trustiot/identity-signing-keys",
        readOnly: true,
      });
      expect(envValue(manifests, "identity", "TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE")).toBe(
        "/etc/trustiot/identity-signing-keys/signing-keys.json",
      );
      expect(envValue(manifests, "identity", "TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION")).toBe("0");
      expect(envValue(manifests, "identity", "TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS")).toBe("600");
      // No token in the pod's containers: only the init containers mount the projected one.
      expect(identity.automountServiceAccountToken).toBe(false);
      for (const container of identity.containers) {
        expect(JSON.stringify(container.volumeMounts ?? [])).not.toContain("migrate-kube-api");
      }
      // No other pod template names the Secret, in a volume, an env or a projection. The
      // rotation CronJob names it only to reach it through the API (the test below).
      expect(podsOf(manifests).map(({ name }) => name)).toContain(`CronJob/${ROTATION}`);
      for (const { name, pod } of podsOf(manifests)) {
        if (name === `Deployment/${RELEASE}-identity`) continue;
        if (name === `CronJob/${ROTATION}`) {
          expect(JSON.stringify(pod.volumes)).not.toContain(SIGNING_KEYS);
          expect(JSON.stringify(pod)).not.toContain("secretKeyRef");
          expect(pod.securityContext?.fsGroup, name).not.toBe(65532);
          continue;
        }
        const text = JSON.stringify(pod);
        expect(text, name).not.toContain(SIGNING_KEYS);
        expect(text, name).not.toContain("TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE");
        expect(pod.securityContext?.fsGroup, name).not.toBe(65532);
      }
      // Nothing of the private key reaches a ConfigMap.
      for (const configMap of manifests.filter((m) => m.kind === "ConfigMap")) {
        expect(JSON.stringify(configMap)).not.toContain(SIGNING_KEYS);
      }
    }
    // OpenShift: the SCC assigns the fsGroup; the file mode stays.
    const openshift = render(
      [...SHAPES.saas, SMOKE],
      [...enableAllServices, "--set", "services.identity.podSecurityContext.fsGroup=null"],
    );
    const sccAssigned = podSpec(find(openshift, "Deployment", `${RELEASE}-identity`));
    expect(sccAssigned.securityContext?.fsGroup).toBeUndefined();
    expect(sccAssigned.securityContext?.runAsNonRoot).toBe(true);
    expect(JSON.stringify(sccAssigned.volumes)).toContain('"defaultMode":288');
    // With the identity service off: no Secret, no Role, and no pod mentions the keys.
    const off = render([...SHAPES.saas, SMOKE], ["--set", "services.assets.enabled=true"]);
    expect(off.filter((m) => m.metadata.name === SIGNING_KEYS)).toEqual([]);
    expect(JSON.stringify(off)).not.toContain(SIGNING_KEYS);
  });

  test("the identity pod's init container generates and publishes the keys with a token only it mounts, and the Role names exactly the Secret and the ConfigMap", () => {
    for (const migrate of ["true", "false"]) {
      const manifests = render(SHAPES.saas, ["--set", "services.identity.enabled=true", "--set", `migrate.enabled=${migrate}`]);
      const pod = podSpec(find(manifests, "Deployment", `${RELEASE}-identity`));
      // First, before the wait for the migration Job: it needs no database.
      const init = initsOf(pod)[0] as Container & { args: string[] };
      expect(init.name).toBe("signing-keys");
      expect(init.image).toBe(`ghcr.io/laysi/trustiot-v2/migrate:${chart.appVersion}`);
      expect(init.args).toEqual(["identity-signing-keys"]);
      expect(init.env).toEqual([
        { name: "TRUSTIOT__KEYS__API_URL", value: "https://kubernetes.default.svc" },
        { name: "TRUSTIOT__KEYS__NAMESPACE", valueFrom: { fieldRef: { fieldPath: "metadata.namespace" } } },
        { name: "TRUSTIOT__KEYS__TOKEN_FILE", value: "/var/run/secrets/trustiot/kube/token" },
        { name: "TRUSTIOT__KEYS__CA_FILE", value: "/var/run/secrets/trustiot/kube/ca.crt" },
        { name: "TRUSTIOT__KEYS__SECRET", value: SIGNING_KEYS },
        { name: "TRUSTIOT__KEYS__CONFIG_MAP", value: PUBLIC_KEYS },
        { name: "TRUSTIOT__KEYS__GENERATION", value: "1" },
        { name: "TRUSTIOT__KEYS__REVOKE_BEFORE_GENERATION", value: "0" },
        { name: "TRUSTIOT__KEYS__PROPAGATION_SECONDS", value: "600" },
      ]);
      expect(init.securityContext).toEqual(pod.containers[0]?.securityContext);
      expect(init.volumeMounts).toEqual([{ name: "migrate-kube-api", mountPath: "/var/run/secrets/trustiot/kube", readOnly: true }]);
      expect(pod.volumes).toContainEqual({
        name: "migrate-kube-api",
        projected: {
          defaultMode: 0o444,
          sources: [
            { serviceAccountToken: { path: "token", expirationSeconds: 3600 } },
            { configMap: { name: "kube-root-ca.crt", items: [{ key: "ca.crt", path: "ca.crt" }] } },
          ],
        },
      });
      const role = find(manifests, "Role", SIGNING_KEYS) as Manifest & { rules: Rule[] };
      expect(role.rules).toEqual([
        { apiGroups: [""], resources: ["secrets"], resourceNames: [SIGNING_KEYS], verbs: ["get", "patch"] },
        { apiGroups: [""], resources: ["configmaps"], resourceNames: [PUBLIC_KEYS], verbs: ["get", "patch"] },
      ]);
      const binding = find(manifests, "RoleBinding", SIGNING_KEYS) as Manifest & { roleRef: unknown; subjects: unknown[] };
      expect(binding.roleRef).toEqual({ apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: SIGNING_KEYS });
      expect(binding.subjects).toEqual([{ kind: "ServiceAccount", name: `${RELEASE}-identity`, namespace: NAMESPACE }]);
      // The ServiceAccount itself mounts no token anywhere by default.
      expect((find(manifests, "ServiceAccount", `${RELEASE}-identity`) as Manifest & { automountServiceAccountToken: boolean }).automountServiceAccountToken).toBe(false);
    }
  });

  test("a rotation or a revocation restarts the identity service only, which gets the revocation before its files do", () => {
    const base = ["--set", "services.identity.enabled=true", "--set", "services.assets.enabled=true"];
    const before = render(SHAPES.saas, base);
    const rotated = render(SHAPES.saas, [...base, "--set", "internalIdentity.keyGeneration=2"]);
    const revoked = render(SHAPES.saas, [...base, "--set", "internalIdentity.keyGeneration=3", "--set", "internalIdentity.revokeBeforeGeneration=3"]);
    const slower = render(SHAPES.saas, [...base, "--set", "internalIdentity.keyPropagationSeconds=900"]);
    const template = (manifests: Manifest[], service: string) =>
      JSON.stringify((find(manifests, "Deployment", `${RELEASE}-${service}`).spec as { template: unknown }).template);
    for (const changed of [rotated, revoked, slower]) {
      expect(template(changed, "identity")).not.toBe(template(before, "identity"));
      // The other services keep their pods: they reread the public keys file.
      expect(template(changed, "assets")).toBe(template(before, "assets"));
      expect(platformConfig(changed)).toEqual(platformConfig(before));
    }
    const generation = (manifests: Manifest[], name: string) =>
      initsOf(podSpec(find(manifests, "Deployment", `${RELEASE}-identity`)))[0]?.env?.find((e) => e.name === name)?.value;
    expect(generation(rotated, "TRUSTIOT__KEYS__GENERATION")).toBe("2");
    expect(generation(revoked, "TRUSTIOT__KEYS__GENERATION")).toBe("3");
    expect(generation(revoked, "TRUSTIOT__KEYS__REVOKE_BEFORE_GENERATION")).toBe("3");
    expect(envValue(revoked, "identity", "TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION")).toBe("3");
    expect(generation(slower, "TRUSTIOT__KEYS__PROPAGATION_SECONDS")).toBe("900");
    expect(envValue(slower, "identity", "TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS")).toBe("900");
    // Exact decimal digits, as tools/migrate compares them.
    const big = render(SHAPES.saas, [...base, "--set", "internalIdentity.keyGeneration=1000000"]);
    expect(generation(big, "TRUSTIOT__KEYS__GENERATION")).toBe("1000000");
  });

  // Laysi/trustiot-v2#429: the CronJob that rotates the signing key by age.
  test("the rotation CronJob runs the init container's command as the identity ServiceAccount, with no Role, binding or Secret mount of its own", () => {
    const everything = [...enableAllServices, "--set", "observability.collector.enabled=true"];
    const rbac = (manifests: Manifest[]) =>
      manifests
        .filter((m) => ["ServiceAccount", "Role", "ClusterRole", "RoleBinding", "ClusterRoleBinding"].includes(m.kind))
        .map((m) => JSON.stringify(m))
        .sort();
    const selector = { "app.kubernetes.io/name": "trustiot", "app.kubernetes.io/instance": RELEASE, "app.kubernetes.io/component": "identity-key-rotation" };
    for (const shape of SHAPE_NAMES) {
      const manifests = render([...SHAPES[shape], SMOKE], everything);
      const cronJob = find(manifests, "CronJob", ROTATION) as Manifest & {
        spec: {
          schedule: string;
          timeZone: string;
          concurrencyPolicy: string;
          startingDeadlineSeconds: number;
          jobTemplate: { spec: { template: { metadata: { labels: Record<string, string> } } } };
        };
      };
      expect(cronJob.spec.schedule).toBe("17 3 * * *");
      expect(cronJob.spec.timeZone).toBe("Etc/UTC");
      expect(cronJob.spec.concurrencyPolicy).toBe("Forbid");
      // Fewer than 100 missed runs fit in the deadline even at a run a minute, so the controller
      // never stops scheduling it.
      expect(cronJob.spec.startingDeadlineSeconds).toBe(3600);
      expect(cronJob.spec.jobTemplate.spec.template.metadata.labels["istio.io/dataplane-mode"]).toBe("ambient");
      const pod = cronPod(cronJob);
      expect(pod.serviceAccountName).toBe(`${RELEASE}-identity`);
      expect(pod.automountServiceAccountToken).toBe(false);
      expect(pod.initContainers).toBeUndefined();
      expect(pod.containers.map((c) => c.name)).toEqual(["key-rotation"]);
      const container = pod.containers[0] as Container & { args: string[] };
      const identity = podSpec(find(manifests, "Deployment", `${RELEASE}-identity`));
      const init = initsOf(identity)[0] as Container & { args: string[] };
      // The init container's command and settings plus the maximum key age, and its token volume.
      expect(container.image).toBe(init.image);
      expect(container.args).toEqual(["identity-signing-keys"]);
      expect(container.env).toEqual([...(init.env ?? []), { name: "TRUSTIOT__KEYS__MAX_KEY_AGE_SECONDS", value: "7776000" }]);
      expect(container.securityContext).toEqual(init.securityContext);
      expect(container.volumeMounts).toEqual(init.volumeMounts);
      expect(pod.volumes).toEqual(identity.volumes?.filter((v) => v.name === "migrate-kube-api"));
      // Nobody connects to it.
      const authorization = find(manifests, "AuthorizationPolicy", ROTATION) as Manifest & { spec: Record<string, unknown> };
      expect(authorization.spec).toEqual({ selector: { matchLabels: selector }, action: "ALLOW" });
      const network = find(manifests, "NetworkPolicy", ROTATION) as Manifest & { spec: Record<string, unknown> };
      expect(network.spec).toEqual({ podSelector: { matchLabels: selector }, policyTypes: ["Ingress"], ingress: [] });
      // The identity service checks for an overdue rotation with the same maximum age.
      expect(envValue(manifests, "identity", "TRUSTIOT_IDENTITY_MAX_KEY_AGE_SECONDS")).toBe("7776000");

      // Acceptance: no permission beyond the identity ServiceAccount's. Every ServiceAccount, Role
      // and binding renders exactly as without rotation by age, so no subject gains access.
      const off = render([...SHAPES[shape], SMOKE], [...everything, "--set", "internalIdentity.maxKeyAgeSeconds=0"]);
      expect(rbac(manifests)).toEqual(rbac(off));
      // Acceptance: maxKeyAgeSeconds 0 renders no CronJob, and the identity service checks nothing.
      expect(otherCronJobs(off)).toEqual([]);
      expect(JSON.stringify(off)).not.toContain("identity-key-rotation");
      expect(envValue(off, "identity", "TRUSTIOT_IDENTITY_MAX_KEY_AGE_SECONDS")).toBe("0");
    }
    // Nothing rotates while a person's key is moved over, or without the identity service.
    const moving = render([...SHAPES.saas, SMOKE], [...everything, ...legacy]);
    expect(otherCronJobs(moving)).toEqual([]);
    expect(envValue(moving, "identity", "TRUSTIOT_IDENTITY_MAX_KEY_AGE_SECONDS")).toBe("0");
    const without = render([...SHAPES.saas, SMOKE], ["--set", "services.assets.enabled=true"]);
    expect(without.filter((m) => m.kind === "CronJob")).toEqual([]);
  }, SLOW_TEST_MS);

  test("the maximum key age and the schedule change the CronJob and the identity service's check only", () => {
    const base = ["--set", "services.identity.enabled=true", "--set", "services.assets.enabled=true"];
    const before = render(SHAPES.saas, base);
    const often = render(SHAPES.saas, [...base, "--set", "internalIdentity.keyRotationSchedule=*/5 * * * *"]);
    const shorter = render(SHAPES.saas, [...base, "--set", "internalIdentity.keyPropagationSeconds=120", "--set", "internalIdentity.maxKeyAgeSeconds=545"]);
    const template = (manifests: Manifest[], service: string) =>
      JSON.stringify((find(manifests, "Deployment", `${RELEASE}-${service}`).spec as { template: unknown }).template);
    expect((find(often, "CronJob", ROTATION).spec as { schedule: string }).schedule).toBe("*/5 * * * *");
    // A new schedule restarts nothing.
    expect(template(often, "identity")).toBe(template(before, "identity"));
    expect(template(often, "assets")).toBe(template(before, "assets"));
    // The shortest age for a propagation time: twice it plus 305 s.
    const env = cronPod(find(shorter, "CronJob", ROTATION)).containers[0]?.env ?? [];
    expect(env.find((e) => e.name === "TRUSTIOT__KEYS__MAX_KEY_AGE_SECONDS")?.value).toBe("545");
    expect(env.find((e) => e.name === "TRUSTIOT__KEYS__PROPAGATION_SECONDS")?.value).toBe("120");
    expect(envValue(shorter, "identity", "TRUSTIOT_IDENTITY_MAX_KEY_AGE_SECONDS")).toBe("545");
    expect(template(shorter, "assets")).toBe(template(before, "assets"));
    for (const schedule of ["@daily", "@midnight", "@hourly", "0 */6 * * *", "5,35 1-3 * * *"]) {
      expect(helmTemplate(SHAPES.saas, [...base, "--set-json", `internalIdentity.keyRotationSchedule=${JSON.stringify(schedule)}`]).code, schedule).toBe(0);
    }
    // The CronJob's name leaves the Job controller its 11 characters, even for the longest release
    // name (Helm caps it at 53); its policies carry the same name.
    for (const release of ["r", `trustiot-${"x".repeat(44)}`]) {
      const long = render(SHAPES.saas, base, release);
      const cronJobs = long.filter((m) => m.kind === "CronJob" && m.metadata.name.endsWith("-identity-key-rotation"));
      expect(cronJobs).toHaveLength(1);
      const name = cronJobs[0]?.metadata.name ?? "";
      expect(name.length, release).toBeLessThanOrEqual(52);
      expect(name).toEndWith("-identity-key-rotation");
      expect(long.filter((m) => m.metadata.name === name).map((m) => m.kind).sort()).toEqual(["AuthorizationPolicy", "CronJob", "NetworkPolicy"]);
    }
  }, SLOW_TEST_MS);

  test("a signing key a person made before Laysi/trustiot-v2#404 reaches the init container only, which imports it", () => {

    const manifests = render([...SHAPES.saas, SMOKE], [...enableAllServices, ...legacy]);
    const pod = podSpec(find(manifests, "Deployment", `${RELEASE}-identity`));
    const init = initsOf(pod)[0] as Container;
    expect(init.env?.slice(-2)).toEqual([
      { name: "TRUSTIOT__KEYS__LEGACY_KEY_FILE", value: "/etc/trustiot/legacy-identity-signing-key/signing-key.pem" },
      {
        name: "TRUSTIOT__KEYS__LEGACY_PUBLIC_KEYS",
        value: JSON.stringify({ keys: [{ crv: "Ed25519", kid: "2026-10-a", kty: "OKP", x: LEGACY_X }] }),
      },
    ]);
    expect(init.volumeMounts).toContainEqual({ name: "legacy-identity-signing-key", mountPath: "/etc/trustiot/legacy-identity-signing-key", readOnly: true });
    expect(pod.volumes).toContainEqual({
      name: "legacy-identity-signing-key",
      secret: { secretName: "trustiot-identity-signing", defaultMode: 0o440, items: [{ key: "signing-key.pem", path: "signing-key.pem" }] },
    });
    // Neither the service's own container nor any other pod mounts it.
    expect(JSON.stringify(pod.containers)).not.toContain("legacy-identity-signing-key");
    for (const { name, pod: other } of podsOf(manifests)) {
      if (name === `Deployment/${RELEASE}-identity`) continue;
      expect(JSON.stringify(other), name).not.toContain("trustiot-identity-signing");
    }
    // Still the generated Secret and the ConfigMap: the import fills them.
    expect(envValue(manifests, "identity", "TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE")).toBe("/etc/trustiot/identity-signing-keys/signing-keys.json");
    expect(platformConfig(manifests).TRUSTIOT_INTERNAL_IDENTITY_KEYS).toBeUndefined();
    // Without it, the init container has no import settings.
    const fresh = render(SHAPES.saas, ["--set", "services.identity.enabled=true"]);
    expect(JSON.stringify(podSpec(find(fresh, "Deployment", `${RELEASE}-identity`)))).not.toContain("LEGACY");
  });

  test("no Role but the identity service's may write a ConfigMap or name its signing keys (the certs service's writes only its requests)", () => {
    // Every optional component on, as "with every optional component on" below does.
    const switchesOf = (node: unknown, path: string[] = []): string[] =>
      isMap(node)
        ? Object.entries(node).flatMap(([key, value]) =>
            key === "enabled" && typeof value === "boolean" ? [[...path, key].join(".")] : switchesOf(value, [...path, key]),
          )
        : [];
    const switches = switchesOf(defaults).flatMap((path) => ["--set", `${path}=true`]);
    for (const shape of SHAPE_NAMES) {
      const manifests = render([...SHAPES[shape], SMOKE], [
          ...switches,
          ...legacy,
          ...["--set", "observability.sentry.dsnSecret.name=s", "--set", "observability.sentry.environment=t"],
          ...["--set", "observability.collector.forward.endpoint=https://siem.trustiot.test:4318"],
          ...AI_GATEWAY_SETTINGS,
        ]);
      const roles = manifests.filter((m) => m.kind === "Role" || m.kind === "ClusterRole") as (Manifest & { rules: Rule[] })[];
      expect(roles.map((m) => m.metadata.name)).toContain(SIGNING_KEYS);
      for (const role of roles) {
        for (const rule of role.rules) {
          const core = (rule.apiGroups ?? []).some((g) => g === "" || g === "*");
          const resources = rule.resources ?? [];
          const writes = (rule.verbs ?? []).some((verb) => verb !== "get" && verb !== "list" && verb !== "watch");
          if (role.metadata.name === SIGNING_KEYS) continue;
          // The certs service's Role patches the one ConfigMap it fills with certificate signing
          // requests, by name ("gateway certificate authority" below).
          const certsRequests =
            role.metadata.name === `${RELEASE}-certs-ca-keys` &&
            JSON.stringify(rule.resourceNames) === JSON.stringify([`${RELEASE}-certs-ca-requests`]) &&
            resources.length === 1;
          expect({ role: role.metadata.name, writesConfigMaps: core && writes && resources.some((r) => r === "configmaps" || r === "*") && !certsRequests }).toEqual({
            role: role.metadata.name,
            writesConfigMaps: false,
          });
          expect(JSON.stringify(rule), role.metadata.name).not.toContain(SIGNING_KEYS);
        }
      }
      const bindings = manifests.filter((m) => m.kind === "RoleBinding" && (m as Manifest & { roleRef: { name: string } }).roleRef.name === SIGNING_KEYS);
      expect(bindings.map((m) => (m as Manifest & { subjects: unknown[] }).subjects)).toEqual([[{ kind: "ServiceAccount", name: `${RELEASE}-identity`, namespace: NAMESPACE }]]);
    }
  }, SLOW_TEST_MS);

  test("the identity service alone gets the cookie routes, with host names, to address its tokens", () => {
    const manifests = render(e2e, [
      "--set-json",
      'ingress.routes=[{"host":"app","path":"/api","match":"PathPrefix","service":"assets"},{"host":"gateway","path":"/api/certs","match":"Exact","service":"certs"}]',
    ]);
    // Cookie routes only: the public paths and the SPA's static files get no token.
    expect(JSON.parse(envValue(manifests, "identity", "TRUSTIOT_INGRESS_ROUTES") ?? "null")).toEqual([
      { host: "app.saas.trustiot.test", path: "/api", match: "PathPrefix", service: "assets" },
      { host: "gw.saas.trustiot.test", path: "/api/certs", match: "Exact", service: "certs" },
    ]);
    for (const service of ["assets", "certs", "mqtt-bridge", "web"]) {
      expect(envValue(manifests, service, "TRUSTIOT_INGRESS_ROUTES")).toBeUndefined();
    }
    expect(platformConfig(manifests).TRUSTIOT_INGRESS_ROUTES).toBeUndefined();
    // No cookie routes: an empty list, so the identity service starts and denies every check.
    const none = render([...SHAPES.saas, SMOKE], ["--set", "services.identity.enabled=true"]);
    expect(envValue(none, "identity", "TRUSTIOT_INGRESS_ROUTES")).toBe("[]");
  });
});

describe("gateway certificate authority (decision 12, Laysi/trustiot-v2#78)", () => {
  const KEYS = `${RELEASE}-certs-ca-keys`;
  const REQUESTS = `${RELEASE}-certs-ca-requests`;
  const CERTIFICATES = `${RELEASE}-certs-ca-certificates`;
  const withCerts = ["--set", "services.certs.enabled=true"];
  type Rule = { apiGroups?: string[]; resources?: string[]; resourceNames?: string[]; verbs?: string[] };
  const cronPod = (cronJob: Manifest) =>
    (cronJob.spec as { jobTemplate: { spec: { template: { spec: PodSpec } } } }).jobTemplate.spec.template.spec;
  // Every pod template the chart renders; CloudNativePG's Cluster has none.
  const podsOf = (manifests: Manifest[]) =>
    manifests
      .filter((m) => ["Deployment", "StatefulSet", "Job", "DaemonSet", "CronJob"].includes(m.kind))
      .map((m) => ({ name: `${m.kind}/${m.metadata.name}`, pod: m.kind === "CronJob" ? cronPod(m) : podSpec(m) }));
  // A stand-in for a certificate's DER in base64: "MII", a two-byte length, as a certificate has.
  const certificate = (fill: string) => `MII${fill.repeat(149)}`;

  test("while the certs service is off, nothing of the CA is rendered, but the values are checked", () => {
    const others = plannedServices.filter((name) => name !== "certs").flatMap((name) => ["--set", `services.${name}.enabled=true`]);
    const off = render([...SHAPES.saas, SMOKE], others);
    expect(off.filter(isServiceDeployment).length).toBeGreaterThan(0);
    expect(JSON.stringify(off)).not.toContain("certs-ca");
    const result = helmTemplate(SHAPES.saas, ["--set", "certificateAuthority.keyGeneration=2", "--set", "certificateAuthority.removeKeysBeforeGeneration=3"]);
    expect(result.code).not.toBe(0);
    expect(result.stderr).toContain("certificateAuthority.keyGeneration (2) must not be below certificateAuthority.removeKeysBeforeGeneration (3)");
  });

  test("the certs pod's init container fills the keys Secret and the requests ConfigMap with a token only it mounts, and the Role names exactly those two", () => {
    for (const migrate of ["true", "false"]) {
      const manifests = render(SHAPES.saas, [...withCerts, "--set", `migrate.enabled=${migrate}`]);
      const deployment = find(manifests, "Deployment", `${RELEASE}-certs`);
      const pod = podSpec(deployment);
      const inits = (pod.initContainers ?? []) as (Container & { args: string[] })[];
      // First, before the wait for the migration Job: it needs no database.
      expect(inits.map((c) => c.name)).toEqual(migrate === "true" ? ["ca-keys", "wait-for-migrations"] : ["ca-keys"]);
      const init = inits[0] as Container & { args: string[] };
      expect(init.image).toBe(`ghcr.io/laysi/trustiot-v2/migrate:${chart.appVersion}`);
      expect(init.args).toEqual(["certs-ca-keys"]);
      expect(init.env).toEqual([
        { name: "TRUSTIOT__KEYS__API_URL", value: "https://kubernetes.default.svc" },
        { name: "TRUSTIOT__KEYS__NAMESPACE", valueFrom: { fieldRef: { fieldPath: "metadata.namespace" } } },
        { name: "TRUSTIOT__KEYS__TOKEN_FILE", value: "/var/run/secrets/trustiot/kube/token" },
        { name: "TRUSTIOT__KEYS__CA_FILE", value: "/var/run/secrets/trustiot/kube/ca.crt" },
        { name: "TRUSTIOT__KEYS__SECRET", value: KEYS },
        { name: "TRUSTIOT__KEYS__CONFIG_MAP", value: REQUESTS },
        { name: "TRUSTIOT__KEYS__GENERATION", value: "1" },
        { name: "TRUSTIOT__KEYS__REMOVE_BEFORE_GENERATION", value: "0" },
      ]);
      expect(init.securityContext).toEqual(pod.containers[0]?.securityContext);
      expect(init.volumeMounts).toEqual([{ name: "migrate-kube-api", mountPath: "/var/run/secrets/trustiot/kube", readOnly: true }]);
      // No token in the service's own container.
      expect(pod.automountServiceAccountToken).toBe(false);
      for (const container of pod.containers) {
        expect(JSON.stringify(container.volumeMounts ?? [])).not.toContain("migrate-kube-api");
      }
      // The Secret and the requests ConfigMap are declared without data, kept by Helm and filled
      // by tools/migrate.
      for (const [kind, name] of [
        ["Secret", KEYS],
        ["ConfigMap", REQUESTS],
      ] as const) {
        const declared = find(manifests, kind, name);
        expect(Object.keys(declared).sort(), name).toEqual(kind === "Secret" ? ["apiVersion", "kind", "metadata", "type"] : ["apiVersion", "kind", "metadata"]);
        expect(declared.metadata.labels?.["trustiot.io/filled-by"]).toBe("migrate");
        expect(declared.metadata.labels?.["app.kubernetes.io/component"]).toBe("certs");
        expect(declared.metadata.annotations?.["helm.sh/resource-policy"]).toBe("keep");
      }
      const role = find(manifests, "Role", KEYS) as Manifest & { rules: Rule[] };
      expect(role.rules).toEqual([
        { apiGroups: [""], resources: ["secrets"], resourceNames: [KEYS], verbs: ["get", "patch"] },
        { apiGroups: [""], resources: ["configmaps"], resourceNames: [REQUESTS], verbs: ["get", "patch"] },
      ]);
      const binding = find(manifests, "RoleBinding", KEYS) as Manifest & { roleRef: unknown; subjects: unknown[] };
      expect(binding.roleRef).toEqual({ apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: KEYS });
      expect(binding.subjects).toEqual([{ kind: "ServiceAccount", name: `${RELEASE}-certs`, namespace: NAMESPACE }]);
    }
  });

  test("only the certs service mounts the issuing CA keys, read-only and mode 0440 in a group of its own, and no other Role or pod names them", () => {
    const everything = [...enableAllServices, "--set", "observability.collector.enabled=true"];
    for (const shape of SHAPE_NAMES) {
      const manifests = render([...SHAPES[shape], SMOKE], everything);
      const certs = podSpec(find(manifests, "Deployment", `${RELEASE}-certs`));
      expect(certs.volumes).toContainEqual({
        name: "certs-ca-keys",
        secret: { secretName: KEYS, defaultMode: 0o440, optional: true, items: [{ key: "issuing-ca-keys.json", path: "issuing-ca-keys.json" }] },
      });
      expect(certs.volumes).toContainEqual({ name: "certs-ca-certificates", configMap: { name: CERTIFICATES } });
      expect(certs.securityContext?.fsGroup).toBe(65533);
      expect(certs.containers[0]?.volumeMounts).toEqual(
        expect.arrayContaining([
          { name: "certs-ca-keys", mountPath: "/etc/trustiot/certs-ca-keys", readOnly: true },
          { name: "certs-ca-certificates", mountPath: "/etc/trustiot/certs-ca", readOnly: true },
        ]),
      );
      expect((certs.containers[0]?.env ?? []).filter((e) => e.name.startsWith("TRUSTIOT__CA__"))).toEqual([
        { name: "TRUSTIOT__CA__KEYS_FILE", value: "/etc/trustiot/certs-ca-keys/issuing-ca-keys.json" },
        { name: "TRUSTIOT__CA__ROOT_CERTIFICATE_FILE", value: "/etc/trustiot/certs-ca/root.pem" },
        { name: "TRUSTIOT__CA__CERTIFICATES_FILE", value: "/etc/trustiot/certs-ca/issuing.pem" },
      ]);
      // It uses Postgres (values.yaml mesh.calls): its own role and password.
      expect((certs.containers[0]?.env ?? []).find((e) => e.name === "TRUSTIOT__DATABASE__USERNAME")?.value).toBe("certs_app");
      // Its platform role writes the platform-wide factory registration event
      // (Laysi/trustiot-v2#79), from the same Secret.
      expect((certs.containers[0]?.env ?? []).filter((e) => e.name.startsWith("TRUSTIOT__DATABASE__PLATFORM_"))).toEqual([
        { name: "TRUSTIOT__DATABASE__PLATFORM_USERNAME", value: "certs_platform" },
        {
          name: "TRUSTIOT__DATABASE__PLATFORM_PASSWORD",
          valueFrom: { secretKeyRef: { name: `${RELEASE}-certs-password`, key: "certs_platform" } },
        },
      ]);
      for (const { name, pod } of podsOf(manifests)) {
        if (name === `Deployment/${RELEASE}-certs`) continue;
        expect(JSON.stringify(pod), name).not.toContain("certs-ca");
        expect(pod.securityContext?.fsGroup, name).not.toBe(65533);
      }
      for (const role of manifests.filter((m) => (m.kind === "Role" || m.kind === "ClusterRole") && m.metadata.name !== KEYS)) {
        expect(JSON.stringify(role), role.metadata.name).not.toContain("certs-ca");
      }
      // Nothing of the keys reaches a ConfigMap but their names.
      for (const configMap of manifests.filter((m) => m.kind === "ConfigMap")) {
        expect(JSON.stringify(configMap), configMap.metadata.name).not.toContain("issuing-ca-keys.json");
      }
    }
  });

  test("the root and issuing CA certificates come from the values as PEM, and only certificates do", () => {
    const root = certificate("A");
    const issuing = [certificate("B"), certificate("C")];
    const manifests = render(SHAPES.saas, [
      ...withCerts,
      "--set",
      `certificateAuthority.rootCertificate=${root}`,
      "--set-json",
      `certificateAuthority.issuingCertificates=${JSON.stringify(issuing)}`,
    ]);
    const data = (find(manifests, "ConfigMap", CERTIFICATES) as Manifest & { data: Record<string, string> }).data;
    const pem = (der: string) => `-----BEGIN CERTIFICATE-----\n${(der.match(/.{1,64}/g) ?? []).join("\n")}\n-----END CERTIFICATE-----\n`;
    expect(data).toEqual({ "root.pem": pem(root), "issuing.pem": issuing.map(pem).join("") });
    // Before the ceremony both files are empty: the service starts but cannot sign.
    const empty = (find(render(SHAPES.saas, withCerts), "ConfigMap", CERTIFICATES) as Manifest & { data: Record<string, string> }).data;
    expect(empty).toEqual({ "root.pem": "", "issuing.pem": "" });
  });

  test("a private key, PEM text or too many certificates in the values fail the render, with or without the schema", () => {
    // An EC private key's DER is shorter than a certificate's, so its base64 starts "MIG" or "MHc".
    const pkcs8 = `MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg${"A".repeat(40)}`;
    const sec1 = `MHcCAQEEI${"A".repeat(43)}`;
    for (const [what, args, schema, template] of [
      ["a PKCS#8 key as the root", ["--set", `certificateAuthority.rootCertificate=${pkcs8}`], /rootCertificate/, /rootCertificate must be one certificate/],
      ["a SEC 1 key among the issuing CAs", ["--set-json", `certificateAuthority.issuingCertificates=["${sec1}"]`], /issuingCertificates/, /issuingCertificates\[0\] must be one certificate/],
      ["base64 of the wrong length", ["--set", `certificateAuthority.rootCertificate=${certificate("A")}A`], null, /rootCertificate must be one certificate/],
      [
        "five issuing CAs",
        ["--set-json", `certificateAuthority.issuingCertificates=${JSON.stringify(["A", "B", "C", "D", "E"].map(certificate))}`],
        /issuingCertificates/,
        /holds more than 4 certificates/,
      ],
      ["a generation below the removal", ["--set", "certificateAuthority.keyGeneration=2", "--set", "certificateAuthority.removeKeysBeforeGeneration=3"], null, /must not be below/],
    ] as const) {
      const checked = helmTemplate(SHAPES.saas, [...withCerts, ...args]);
      expect(checked.code, what).not.toBe(0);
      if (schema) expect(checked.stderr, what).toMatch(schema);
      const unchecked = helmTemplate(SHAPES.saas, [...withCerts, ...args, "--skip-schema-validation"]);
      expect(unchecked.code, what).not.toBe(0);
      expect(unchecked.stderr, what).toMatch(template);
    }
    // PEM text has line breaks, which no value may hold (templates/values-check.yaml).
    const pemText = helmTemplate(SHAPES.saas, [
      ...withCerts,
      "--skip-schema-validation",
      "--set-json",
      `certificateAuthority.rootCertificate=${JSON.stringify(`-----BEGIN CERTIFICATE-----\n${certificate("A")}\n-----END CERTIFICATE-----`)}`,
    ]);
    expect(pemText.code).not.toBe(0);
  }, SLOW_TEST_MS);

  test("the certs container gets no token, and custom env can neither read the keys nor move the CA files", () => {
    const token = helmTemplate(SHAPES.saas, [...withCerts, "--set", "services.certs.automountServiceAccountToken=true"]);
    expect(token.code).not.toBe(0);
    expect(token.stderr).toContain("services.certs.automountServiceAccountToken must be false");
    for (const [service, env, expected] of [
      ["assets", `[{"name":"STOLEN","valueFrom":{"secretKeyRef":{"name":"${KEYS}","key":"issuing-ca-keys.json"}}}]`, `env reads Secret ${KEYS}`],
      ["certs", '[{"name":"TRUSTIOT__CA__KEYS_FILE","value":"/tmp/keys.json"}]', "env names must be unique"],
      ["certs", '[{"name":"TRUSTIOT__CA__ROOT_CERTIFICATE_FILE","value":"/tmp/root.pem"}]', "env names must be unique"],
    ] as const) {
      const result = helmTemplate(SHAPES.saas, [...withCerts, "--set", `services.${service}.enabled=true`, "--set-json", `services.${service}.env=${env}`]);
      expect(result.code, env).not.toBe(0);
      expect(result.stderr, env).toContain(expected);
    }
  }, SLOW_TEST_MS);

  test("raising keyGeneration restarts the certs pods with the new generation, and nothing else", () => {
    const before = render(SHAPES.saas, [...withCerts, "--set", "services.identity.enabled=true"]);
    const after = render(SHAPES.saas, [...withCerts, "--set", "services.identity.enabled=true", "--set", "certificateAuthority.keyGeneration=2"]);
    const template = (manifests: Manifest[], name: string) => JSON.stringify((find(manifests, "Deployment", `${RELEASE}-${name}`).spec as YamlMap).template);
    expect(template(after, "certs")).not.toBe(template(before, "certs"));
    expect(template(after, "certs")).toContain('{"name":"TRUSTIOT__KEYS__GENERATION","value":"2"}');
    expect(template(after, "identity")).toBe(template(before, "identity"));
    expect(jobPasswordSecretNames(after)).toEqual(jobPasswordSecretNames(before));
  });
});

// Checklist M3 (docs/architecture/secrets.md): values name Secrets and never hold secret
// material, and the chart reads Secrets by name and key only, never creating one itself.
describe("values reference Secrets by name only and hold no secret material", () => {
  const SECRET_LIKE = /secret|passw|passphrase|pwd|token|api_?key|(^|_)key$|dsn|private|credential/i;
  // Secret names are DNS subdomains; keys inside a Secret follow the Kubernetes data key rule.
  const SECRET_NAME = /^([a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*)?$/;
  const SECRET_DATA_KEY = /^[-._a-zA-Z0-9]+$/;
  const CREDENTIAL_NAME = /^[a-z][a-z0-9-]*$/;
  const SECRET_MATERIAL: [RegExp, string][] = [
    [/-----BEGIN [A-Z0-9 ]+-----/, "a PEM block"],
    [/\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}/, "a JWT"],
    [/\b(gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})/, "a GitHub token"],
    [/\bsk-[A-Za-z0-9_-]{16,}/, "an API key"],
    [/\bAKIA[0-9A-Z]{16}\b/, "an AWS access key"],
    [/\bAIza[0-9A-Za-z_-]{30,}/, "a Google API key"],
    [/\bxox[abprs]-[A-Za-z0-9-]{10,}/, "a Slack token"],
    [/AGE-SECRET-KEY-1/, "an age private key"],
    [/^[a-z][a-z0-9+.-]*:\/\/[^/@\s]+@/i, "a URL with credentials"],
  ];
  const VALUES_FILES = [
    "values.yaml",
    "values-saas.yaml",
    "values-onprem.yaml",
    "tests/values/saas-instance.yaml",
    "tests/values/onprem-instance.yaml",
    SMOKE,
  ];

  // Returns why a string looks like secret material: a known shape, or a run of 24+ characters
  // without separators that mixes letters and digits (a random key or token, including a
  // lowercase hex key). Names made of short words (`trustiot-clickhouse-admin`) are not.
  function secretMaterial(value: string): string | undefined {
    for (const [pattern, what] of SECRET_MATERIAL) if (pattern.test(value)) return what;
    const randomLooking =
      value.length >= 24 &&
      /^[A-Za-z0-9+/=_.-]+$/.test(value) &&
      value
        .split(/[-_.]/)
        .some((run) => run.length >= 24 && [/[a-z]/, /[A-Z]/, /[0-9]/].filter((c) => c.test(run)).length >= 2);
    return randomLooking ? "a random-looking key or token" : undefined;
  }

  type Leaf = { path: string[]; value: unknown };
  function leaves(value: unknown, path: string[] = []): Leaf[] {
    if (isMap(value)) return Object.entries(value).flatMap(([k, v]) => leaves(v, [...path, k]));
    if (Array.isArray(value)) return value.flatMap((v, i) => leaves(v, [...path, String(i)]));
    return [{ path, value }];
  }

  // Problems in one parsed values file. A field may only look like a secret when it is part of
  // a Secret reference: the `name` or a `key`/`*Key` field of a `*Secret` map, a `*Secret`
  // field that holds a Secret name itself (`ingress.direct.tlsSecret`), an entry of
  // `imagePullSecrets`, an env var's `valueFrom.secretKeyRef`, a `credentials` entry (the
  // name of a chart-defined credential, not the credential itself), or a `*SecretRevision`
  // counter that rolls pods when a Secret is rotated.
  function valuesProblems(doc: unknown): string[] {
    const problems: string[] = [];
    for (const { path, value } of leaves(doc)) {
      const where = path.join(".");
      // The identity service's public keys (decision 27) look random by design and are not
      // secret; values.schema.json allows only kid and x there, so no private key fits.
      const publicKey = path.length === 4 && path[0] === "internalIdentity" && path[1] === "publicKeys" && path[3] === "x";
      if (typeof value === "string" && !publicKey) {
        const material = secretMaterial(value);
        if (material) problems.push(`${where} holds ${material}`);
      }
      // A boolean is a switch such as automountServiceAccountToken, never secret material.
      if (typeof value === "boolean" || !path.some((segment) => SECRET_LIKE.test(segment))) continue;
      const owner = path.findLastIndex((segment) => /Secrets?$|^secretKeyRef$/.test(segment));
      const field = path.at(-1) ?? "";
      const text = typeof value === "string" ? value : "";
      if (path.at(-2) === "credentials" && path.at(-3) !== undefined && CREDENTIAL_NAME.test(text)) continue;
      if (/Secret$/.test(field) && path.findLastIndex((segment) => SECRET_LIKE.test(segment)) === path.length - 1 && SECRET_NAME.test(text)) continue;
      if (/SecretRevision$/.test(field) && /^\d{1,9}$/.test(text)) continue;
      // A counter that renews generated passwords (migrate.passwordGeneration,
      // clickhouse.bundled.adminPasswordGeneration), never a password.
      if (/^(passwordGeneration|adminPasswordGeneration)$/.test(field) && /^\d{1,9}$/.test(String(value))) continue;
      if (owner >= 0 && path.length === owner + 2 && field === "name" && SECRET_NAME.test(text)) continue;
      if (owner >= 0 && path.length === owner + 2 && /^(key|.+Key)$/.test(field) && SECRET_DATA_KEY.test(text)) continue;
      // imagePullSecrets: a list of names or of {name} objects.
      if (path[owner] === "imagePullSecrets" && SECRET_NAME.test(text)) continue;
      problems.push(`${where} looks like a secret but is not a Secret name or key reference`);
    }
    return problems;
  }

  test.each(VALUES_FILES)("%s: secret fields are Secret names and keys, never secret material", (file) => {
    expect(valuesProblems(readYaml(file))).toEqual([]);
  });

  test("the values rule catches plain-text secrets", () => {
    expect(valuesProblems({ qdrant: { external: { apiKey: "plain" } } })).toEqual([
      "qdrant.external.apiKey looks like a secret but is not a Secret name or key reference",
    ]);
    expect(valuesProblems({ smtp: { credentialsSecret: { name: "trustiot-smtp", password: "hunter2" } } })).toEqual([
      "smtp.credentialsSecret.password looks like a secret but is not a Secret name or key reference",
    ]);
    expect(valuesProblems({ ai: { providers: { anthropic: { apiKeySecret: { name: "sk-0123456789abcdefghijKLMN" } } } } })).toEqual([
      "ai.providers.anthropic.apiKeySecret.name holds an API key",
      "ai.providers.anthropic.apiKeySecret.name looks like a secret but is not a Secret name or key reference",
    ]);
    expect(valuesProblems({ hosts: { app: "-----BEGIN PRIVATE KEY-----" } })).toEqual(["hosts.app holds a PEM block"]);
    // A `*Secret` field holds a Secret name, and a `*SecretRevision` a counter, nothing else.
    expect(valuesProblems({ ingress: { direct: { tlsSecret: "trustiot-ingress-tls" } }, clickhouse: { bundled: { adminSecretRevision: "2" } } })).toEqual([]);
    expect(valuesProblems({ ingress: { direct: { tlsSecret: "BEGIN key material" } }, clickhouse: { bundled: { adminSecretRevision: "hunter2" } } })).toEqual([
      "ingress.direct.tlsSecret looks like a secret but is not a Secret name or key reference",
      "clickhouse.bundled.adminSecretRevision looks like a secret but is not a Secret name or key reference",
    ]);
    // Assembled at run time so that gitleaks does not report the example key.
    const awsKey = ["AKIA", "ABCDEFGHIJKLMNOP"].join("");
    expect(valuesProblems({ objectStorage: { region: awsKey } })).toEqual(["objectStorage.region holds an AWS access key"]);
    expect(valuesProblems({ objectStorage: { endpoint: "https://user:pass@s3.test" } })).toEqual([
      "objectStorage.endpoint holds a URL with credentials",
    ]);
    expect(valuesProblems({ extra: { note: "q8Zr4TnW2xKv9LbP0sYh7GcJ3mDfE6uA" } })).toEqual([
      "extra.note holds a random-looking key or token",
    ]);
    // Only a public key's x is exempt: the same value anywhere else is still reported.
    expect(valuesProblems({ internalIdentity: { publicKeys: [{ kid: "2026-10-a", x: "q8Zr4TnW2xKv9LbP0sYh7GcJ3mDfE6uA" }] } })).toEqual([]);
    expect(valuesProblems({ internalIdentity: { publicKeys: [{ kid: "2026-10-a", d: "q8Zr4TnW2xKv9LbP0sYh7GcJ3mDfE6uA" }] } })).toEqual([
      "internalIdentity.publicKeys.0.d holds a random-looking key or token",
    ]);
    expect(valuesProblems({ internalIdentity: { x: "q8Zr4TnW2xKv9LbP0sYh7GcJ3mDfE6uA" } })).toEqual([
      "internalIdentity.x holds a random-looking key or token",
    ]);
    // A lowercase hex key is random-looking too, also inside a longer value.
    const hexRun = "0123456789abcdef".repeat(4);
    expect(valuesProblems({ extra: { note: hexRun } })).toEqual(["extra.note holds a random-looking key or token"]);
    expect(valuesProblems({ extra: { note: `prefix-${hexRun.slice(0, 32)}` } })).toEqual(["extra.note holds a random-looking key or token"]);
    // Long names made of words are not.
    expect(valuesProblems({ extra: { note: "trustiot-clickhouse-admin-2026-10-10-backup" } })).toEqual([]);
    // References are fine, including the image digest and credential names.
    expect(
      valuesProblems({
        global: { imagePullSecrets: ["ghcr-pull", { name: "ghcr-pull-2" }] },
        qdrant: { external: { credentialsSecret: { name: "trustiot-qdrant", readApiKeyKey: "read-api-key" } } },
        services: {
          knowledge: {
            credentials: ["qdrant-read"],
            image: { digest: `sha256:${"ab".repeat(32)}` },
            env: [{ name: "DB_PASSWORD", valueFrom: { secretKeyRef: { name: "db", key: "password" } } }],
          },
        },
      }),
    ).toEqual([]);
  });

  // Renders both shapes with every service: several helm commands, so it gets the slow limit.
  test("rendered manifests carry no secret value, declare only the Secrets the migration Job fills, and read Secrets by name and key only", () => {
    const names = new Set(
      SHAPE_NAMES.flatMap((shape) =>
        SHAPES[shape].flatMap((file) =>
          leaves(readYaml(file))
            .filter(({ path }) => path.at(-1) === "name" && /Secret$/.test(path.at(-2) ?? ""))
            .map(({ value }) => String(value)),
        ),
      ),
    );
    expect(names.size).toBeGreaterThan(0);
    const allCredentials = [
      "--set-json",
      'services.knowledge.credentials=["qdrant-read","qdrant-write","object-storage","smtp"]',
      "--set-json",
      'services.ai-gateway.credentials=["ai","smtp"]',
      "--set",
      "postgres.mode=external",
      "--set",
      "postgres.external.host=pg.trustiot.test",
      "--set",
      "postgres.external.caSecret.name=trustiot-pg-ca",
      ...EXTERNAL_MIGRATION,
    ];
    names.add("trustiot-pg-ca");
    // The bundled Postgres's CA certificate, from the Secret CloudNativePG generates.
    names.add(`${RELEASE}-postgres-ca`);
    for (const shape of SHAPE_NAMES) {
      for (const args of [[...enableAllServices, ...allCredentials], enableAllServices]) {
        const manifests = render([...SHAPES[shape], SMOKE], args);
        // docs/architecture/secrets.md: the chart never renders a secret value. The only Secrets
        // it declares are the password Secrets of the services that use Postgres and of the
        // collector: no data, kept by Helm, filled by the migration Job.
        const declared = manifests.filter((m) => m.kind === "Secret");
        expect(declared.map((m) => m.metadata.name).sort()).toEqual(passwordSecretNames(manifests));
        for (const secret of declared) {
          expect(Object.keys(secret).sort(), secret.metadata.name).toEqual(["apiVersion", "kind", "metadata", "type"]);
          expect(secret.type).toBe("Opaque");
          expect(secret.metadata.labels?.["trustiot.io/filled-by"]).toBe("migrate");
          expect(secret.metadata.annotations?.["helm.sh/resource-policy"]).toBe("keep");
          names.add(secret.metadata.name);
        }
        let references = 0;
        for (const deployment of manifests.filter((m) => m.kind === "Deployment")) {
          const pod = podSpec(deployment);
          for (const volume of pod.volumes ?? []) {
            const secret = volume.secret as { secretName?: string } | undefined;
            if (!secret) continue;
            references += 1;
            expect(names).toContain(secret.secretName ?? "");
          }
          for (const container of pod.containers) {
            // Where the container mounts a Secret volume: a <name>_FILE setting may name a file
            // there by path (the broker account's password file, Laysi/trustiot-v2#634).
            const secretVolumes = new Set((pod.volumes ?? []).filter((v) => v.secret !== undefined).map((v) => v.name));
            const secretDirs = (container.volumeMounts ?? []).filter((m) => secretVolumes.has(m.name as string) && m.subPath === undefined).map((m) => `${m.mountPath}/`);
            for (const env of container.env ?? []) {
              const ref = env.valueFrom?.secretKeyRef;
              const fileInSecretMount = /_FILE$/.test(env.name) && typeof env.value === "string" && secretDirs.some((dir) => env.value?.startsWith(dir) && !env.value.slice(dir.length).includes("/"));
              if (SECRET_LIKE.test(env.name) && !fileInSecretMount) {
                // Secret-looking variables never carry a literal value.
                expect(env.value, env.name).toBeUndefined();
                expect(ref, env.name).toBeDefined();
              }
              if (!ref) continue;
              references += 1;
              // By name and key only. The SMTP account's references are also optional, and only
              // they: a missing Secret must not keep the identity service, and with it the ext_authz
              // check, from starting (the service reports it, TRUSTIOT_SMTP_AUTH_REQUIRED).
              const smtp = env.name.startsWith("TRUSTIOT_SMTP_");
              expect(Object.keys(ref).sort(), env.name).toEqual(smtp ? ["key", "name", "optional"] : ["key", "name"]);
              if (smtp) expect(ref.optional, env.name).toBe(true);
              expect(names).toContain(ref.name);
            }
          }
        }
        expect(references).toBeGreaterThan(0);
        // Nothing rendered, ConfigMaps included, carries secret material.
        // The chart's own `checksum/*` pod annotations are SHA-256 digests of rendered config,
        // which are random-looking by design.
        const material = leaves(manifests).flatMap(({ path, value }) => {
          const digest =
            path.at(-2) === "annotations" && (path.at(-1) ?? "").startsWith("checksum/") && /^[0-9a-f]{64}$/.test(String(value));
          const found = typeof value === "string" && !digest ? secretMaterial(value) : undefined;
          return found ? [`${path.join(".")}: ${found}`] : [];
        });
        expect(material).toEqual([]);
      }
    }
  }, SLOW_TEST_MS);

  // Laysi/trustiot-v2#398: scripts/ci/check-secret-handling.ts judges templates by their text,
  // which cannot show what a template computes, and a template that renders only under some
  // settings is out of reach of the renders above. So the chart is also rendered with every
  // optional component on, in each of its modes, and every Secret in the output must be one of
  // the migration Job's empty password Secrets. The switches are read from values.yaml (every
  // boolean `enabled`) and values.schema.json (every `mode` enum), so a new component is covered
  // without changing this test; the two shapes cover deploymentMode and ingress exposure.
  test("with every optional component on, in every mode, the only Secrets rendered are the migration Job's empty password Secrets", () => {
    const switches = leaves(defaults)
      .filter(({ path, value }) => path.at(-1) === "enabled" && typeof value === "boolean")
      .map(({ path }) => path.join("."));
    expect(switches).toEqual(
      expect.arrayContaining([
        "services.identity.enabled",
        "migrate.enabled",
        "observability.sentry.enabled",
        "observability.collector.enabled",
        "observability.collector.forward.enabled",
      ]),
    );
    const modes: { path: string; values: string[] }[] = [];
    const findModes = (node: unknown, path: string[]) => {
      if (!isMap(node) || !isMap(node.properties)) return;
      for (const [key, property] of Object.entries(node.properties)) {
        if (key === "mode" && isMap(property) && Array.isArray(property.enum)) {
          modes.push({ path: [...path, key].join("."), values: property.enum.map(String) });
        }
        findModes(property, [...path, key]);
      }
    };
    findModes(JSON.parse(readFileSync(join(CHART_DIR, "values.schema.json"), "utf8")), []);
    expect(modes.map((mode) => mode.path)).toEqual(expect.arrayContaining(["postgres.mode", "clickhouse.mode", "qdrant.mode"]));
    const shapeSettings = SHAPE_NAMES.map((shape) => readYaml(SHAPES[shape][0]) as { global: { deploymentMode: string }; ingress: { exposure: string } });
    expect(shapeSettings.map((values) => values.global.deploymentMode).sort()).toEqual(["onprem", "saas"]);
    expect(shapeSettings.map((values) => values.ingress.exposure).sort()).toEqual(["direct", "tunnel"]);
    // What the components need once they are on: endpoints, and the Secrets they read by name.
    const settings = [
      ...EXTERNAL_MIGRATION,
      ...["--set", "postgres.external.host=pg.trustiot.test", "--set", "postgres.external.caSecret.name=trustiot-pg-ca"],
      ...["--set", "clickhouse.external.url=https://ch.trustiot.test:8443", "--set", "clickhouse.external.caSecret.name=trustiot-ch-ca"],
      ...["--set", "qdrant.external.url=https://qdrant.trustiot.test:6333", "--set", "qdrant.external.credentialsSecret.name=trustiot-qdrant"],
      ...["--set", "objectStorage.credentialsSecret.name=trustiot-object-storage", "--set", "smtp.credentialsSecret.name=trustiot-smtp"],
      ...["anthropic", "openai"].flatMap((provider) => ["--set", `ai.providers.${provider}.apiKeySecret.name=trustiot-${provider}`]),
      ...["--set-json", 'ai.roles.test={"models":["anthropic/claude-test","openai/gpt-test"]}'],
      ...["--set", "internalIdentity.signingKeySecret.name=trustiot-identity-signing"],
      ...["--set-json", 'internalIdentity.publicKeys=[{"kid":"2026-10-a","x":"jxSgv4kffdHZ1jN2sf_IVpE4VGbJdheBqhCAAqI89Q8"}]'],
      ...["--set", "observability.sentry.environment=test", "--set", "observability.sentry.dsnSecret.name=trustiot-sentry"],
      ...["--set", "observability.collector.forward.endpoint=https://siem.trustiot.test:4318"],
      ...["--set", "observability.collector.forward.authHeader.name=Authorization"],
      ...["--set", "observability.collector.forward.authHeader.valueSecret.name=trustiot-siem"],
      ...["--set", "observability.collector.forward.caSecret.name=trustiot-siem-ca"],
      ...["--set-json", 'global.imagePullSecrets=["ghcr-pull"]'],
      ...["--set-json", 'services.knowledge.credentials=["qdrant-read","qdrant-write","object-storage","smtp"]'],
      ...["--set-json", 'services.ai-gateway.credentials=["ai","smtp"]'],
    ];
    const rounds = Math.max(...modes.map((mode) => mode.values.length));
    for (const shape of SHAPE_NAMES) {
      for (let round = 0; round < rounds; round += 1) {
        const args = [
          ...switches.flatMap((path) => ["--set", `${path}=true`]),
          ...modes.flatMap((mode) => ["--set", `${mode.path}=${mode.values[round % mode.values.length]}`]),
          ...settings,
        ];
        const where = `${shape}, ${modes.map((mode) => mode.values[round % mode.values.length]).join("/")}`;
        const result = helmTemplate([...SHAPES[shape], SMOKE], args);
        expect(result.stderr, where).toBe("");
        expect(result.code, where).toBe(0);
        // The text rule, applied to the output: every kind is written the way both this check and
        // Helm read it, and the only Secret is the Job's empty one.
        expect(checkRenderedManifests(where, result.stdout)).toEqual([]);
        const manifests = parseManifests(result.stdout);
        const objects = manifests.flatMap((m) => [m, ...((Array.isArray(m.items) ? m.items : []) as Manifest[])]);
        const secrets = objects.filter((m) => /^secret$/i.test(String(m.kind)));
        expect(secrets.length, where).toBeGreaterThan(0);
        expect(secrets.map((m) => m.metadata.name).sort(), where).toEqual(passwordSecretNames(manifests));
        for (const secret of secrets) {
          expect(Object.keys(secret).sort(), `${where}: ${secret.metadata.name}`).toEqual(["apiVersion", "kind", "metadata", "type"]);
          expect(secret.type).toBe("Opaque");
          expect(secret.metadata.labels?.["trustiot.io/filled-by"]).toBe("migrate");
          expect(secret.metadata.annotations?.["helm.sh/resource-policy"]).toBe("keep");
        }
      }
    }
  }, SLOW_TEST_MS);
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
      for (const deployment of manifests.filter(isServiceDeployment)) {
        const pod = podSpec(deployment);
        expect(pod.automountServiceAccountToken).toBe(false);
        expect(pod.securityContext?.runAsNonRoot).toBe(true);
        expect(pod.securityContext?.seccompProfile?.type).toBe("RuntimeDefault");
        // preStopSleepSeconds (5) + drainSeconds (30, 310 for the AI gateway, whose streams in
        // flight run to their end): the preStop sleep counts in the grace period.
        expect(pod.terminationGracePeriodSeconds).toBe(deployment.metadata.name === `${RELEASE}-ai-gateway` ? 315 : 35);
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
        // The public keys ConfigMap renders without data (the identity pod's init container fills it).
        for (const key of Object.keys((configMap.data ?? {}) as YamlMap)) {
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

  // The ClickHouse LTS minor that decision 14's tenant isolation analysis and the introspection
  // tests of services/ingest were checked against. Renovate proposes only LTS minors and updates
  // the digest of this one on its own (renovate.json5); a pull request to the next LTS fails here
  // on purpose until someone has repeated those checks and moved this constant.
  const CLICKHOUSE_MINOR = "26.8";

  test("the image is the checked LTS minor, pinned like the dev environment and the schema tests", () => {
    digestOf(container.image);
    expect(container.image).toStartWith(`docker.io/clickhouse/clickhouse-server:${CLICKHOUSE_MINOR}@`);
    const stack = readFileSync(join(REPO_ROOT, "scripts/lib/stack.ts"), "utf8");
    const devImage = stack.match(/clickhouse:\s*"([^"]+)"/)?.[1] ?? "";
    const schemaTests = readFileSync(join(REPO_ROOT, "services/ingest/tests/common/mod.rs"), "utf8");
    const testImage = schemaTests.match(/CLICKHOUSE_IMAGE: &str = "([^"]+)"/)?.[1] ?? "";
    // Same tag and digest in all three: Renovate reads each as one copy of the same pin and
    // updates them in one pull request.
    expect(devImage).toBe(container.image);
    expect(testImage).toBe(container.image);
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

  test("the bootstrap admin password is generated in the cluster before the server starts, and read from a file in memory", () => {
    const env = Object.fromEntries((container.env ?? []).map((e) => [e.name, e]));
    expect(env.CLICKHOUSE_PASSWORD).toBeUndefined();
    expect(env.CLICKHOUSE_USER?.value).toBe("trustiot_admin");
    expect(env.CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT?.value).toBe("1");
    expect(env.CLICKHOUSE_PASSWORD_FILE?.value).toBe("/etc/trustiot/clickhouse-admin/password");
    for (const e of container.env ?? []) expect(e.valueFrom).toBeUndefined();
    const volumes = Object.fromEntries((pod.volumes ?? []).map((v) => [v.name as string, v]));
    // Laysi/trustiot-v2#403: the init container writes the password the Secret holds into memory;
    // the server mounts it read-only and never mounts the Secret or a token.
    expect(volumes["admin-password"]).toEqual({ name: "admin-password", emptyDir: { medium: "Memory", sizeLimit: "1Mi" } });
    expect(container.volumeMounts).toContainEqual({ name: "admin-password", mountPath: "/etc/trustiot/clickhouse-admin", readOnly: true });
    expect(JSON.stringify(container)).not.toContain("migrate-kube-api");
    // The entrypoint writes the account, with its password, into users.d: memory only.
    expect(volumes["users-d"]).toMatchObject({ emptyDir: { medium: "Memory" } });
    const secret = `${RELEASE}-clickhouse-admin-password`;
    const inits = (pod.initContainers ?? []) as (Container & { args: string[]; imagePullPolicy: string })[];
    expect(inits.map((c) => c.name)).toEqual(["admin-password"]);
    const init = inits[0] as Container & { args: string[]; imagePullPolicy: string };
    expect(init.image).toBe(`ghcr.io/laysi/trustiot-v2/migrate:${chart.appVersion}`);
    expect(init.args).toEqual(["clickhouse-admin-password"]);
    expect(init.env).toEqual([
      { name: "TRUSTIOT__ADMIN__API_URL", value: "https://kubernetes.default.svc" },
      { name: "TRUSTIOT__ADMIN__NAMESPACE", valueFrom: { fieldRef: { fieldPath: "metadata.namespace" } } },
      { name: "TRUSTIOT__ADMIN__TOKEN_FILE", value: "/var/run/secrets/trustiot/kube/token" },
      { name: "TRUSTIOT__ADMIN__CA_FILE", value: "/var/run/secrets/trustiot/kube/ca.crt" },
      { name: "TRUSTIOT__ADMIN__SECRET", value: secret },
      { name: "TRUSTIOT__ADMIN__ACCOUNT", value: "trustiot_admin" },
      { name: "TRUSTIOT__ADMIN__GENERATION", value: "1" },
      { name: "TRUSTIOT__ADMIN__PASSWORD_FILE", value: "/etc/trustiot/clickhouse-admin/password" },
    ]);
    expect(init.envFrom).toBeUndefined();
    expect(init.volumeMounts).toEqual([
      { name: "admin-password", mountPath: "/etc/trustiot/clickhouse-admin" },
      { name: "migrate-kube-api", mountPath: "/var/run/secrets/trustiot/kube", readOnly: true },
    ]);
    // Restricted Pod Security, like the server.
    expect(init.securityContext).toEqual({ allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: { drop: ["ALL"] } });
    expect(init.resources?.limits?.memory).toBe("128Mi");
    // The token of ClickHouse's own ServiceAccount, which only the init container mounts.
    expect(volumes["migrate-kube-api"]).toEqual({
      name: "migrate-kube-api",
      projected: {
        defaultMode: 0o444,
        sources: [
          { serviceAccountToken: { path: "token", expirationSeconds: 3600 } },
          { configMap: { name: "kube-root-ca.crt", items: [{ key: "ca.crt", path: "ca.crt" }] } },
        ],
      },
    });
    // The migration image is arm64 only.
    expect(pod.nodeSelector).toEqual({ "kubernetes.io/arch": "arm64" });
    // The Secret is the chart's own, empty (templates/passwords.yaml), and ClickHouse's
    // ServiceAccount may get and patch it and nothing else.
    expect(find(manifests, "Secret", secret)).toMatchObject({ metadata: { labels: { "app.kubernetes.io/component": "clickhouse", "trustiot.io/filled-by": "migrate" } } });
    const role = find(manifests, "Role", name) as Manifest & { rules: unknown[] };
    expect(role.rules).toEqual([{ apiGroups: [""], resources: ["secrets"], resourceNames: [secret], verbs: ["get", "patch"] }]);
    const binding = find(manifests, "RoleBinding", name) as Manifest & { roleRef: unknown; subjects: unknown[] };
    expect(binding.roleRef).toEqual({ apiGroup: "rbac.authorization.k8s.io", kind: "Role", name });
    expect(binding.subjects).toEqual([{ kind: "ServiceAccount", name, namespace: NAMESPACE }]);
  });

  test("an installation from before Laysi/trustiot-v2#403 keeps reading its existing Secret until it moves over", () => {
    const legacy = render(SHAPES.saas, ["--set", "clickhouse.bundled.adminSecret.name=trustiot-clickhouse-admin"]);
    const legacyPod = podSpec(find(legacy, "StatefulSet", name));
    const volumes = Object.fromEntries((legacyPod.volumes ?? []).map((v) => [v.name as string, v]));
    expect(volumes["admin-password"]).toEqual({
      name: "admin-password",
      secret: { secretName: "trustiot-clickhouse-admin", defaultMode: 0o440, items: [{ key: "password", path: "password" }] },
    });
    expect(volumes["migrate-kube-api"]).toBeUndefined();
    expect(legacyPod.initContainers).toBeUndefined();
    expect(legacyPod.nodeSelector).toBeUndefined();
    // No generated Secret, and no Role for ClickHouse.
    expect(legacy.filter((m) => m.metadata.name === `${RELEASE}-clickhouse-admin-password`)).toEqual([]);
    expect(legacy.filter((m) => m.kind === "Role" && m.metadata.name === name)).toEqual([]);
    // The pod template is the one the chart rendered before Laysi/trustiot-v2#403 for the same
    // values, so the upgrade that brings this chart does not restart ClickHouse.
    const annotations = (find(legacy, "StatefulSet", name).spec as { template: { metadata: { annotations: Record<string, string> } } }).template.metadata
      .annotations;
    const before = new Bun.CryptoHasher("sha256").update(JSON.stringify(["trustiot-clickhouse-admin", "password", "1"])).digest("hex");
    expect(annotations["checksum/admin-secret"]).toBe(before);
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

  test("the pod rolls when the server configuration or the admin password generation changes", () => {
    const templateOf = (rendered: Manifest[]) =>
      (find(rendered, "StatefulSet", name).spec as { template: { metadata: { annotations: Record<string, string> }; spec: PodSpec } })
        .template;
    const annotationsOf = (rendered: Manifest[]) => templateOf(rendered).metadata.annotations;
    const before = annotationsOf(manifests);
    expect(before["checksum/clickhouse-config"]).toMatch(/^[0-9a-f]{64}$/);
    expect(before["checksum/admin-secret"]).toMatch(/^[0-9a-f]{64}$/);
    // A rotation: the new pod's init container generates the new password before the server
    // starts with it (the generation reaches it in the pod template too).
    const rotatedRender = render(SHAPES.saas, ["--set", "clickhouse.bundled.adminPasswordGeneration=2"]);
    const rotated = annotationsOf(rotatedRender);
    expect(rotated["checksum/admin-secret"]).not.toBe(before["checksum/admin-secret"]);
    expect(rotated["checksum/clickhouse-config"]).toBe(before["checksum/clickhouse-config"]);
    const generation = ((templateOf(rotatedRender).spec.initContainers ?? []) as Container[])[0]?.env?.find((e) => e.name === "TRUSTIOT__ADMIN__GENERATION");
    expect(generation?.value).toBe("2");
    // A rotation touches neither the migration Job nor the services: the Job reads the password
    // when it next runs.
    const jobs = (rendered: Manifest[]) => rendered.filter((m) => m.kind === "Job").map((m) => m.metadata.name);
    expect(jobs(rotatedRender)).toEqual(jobs(manifests));
    // adminSecretRevision is for an existing Secret only, and leaves a generated password alone.
    const revised = annotationsOf(render(SHAPES.saas, ["--set", "clickhouse.bundled.adminSecretRevision=2"]));
    expect(revised["checksum/admin-secret"]).toBe(before["checksum/admin-secret"]);
    const legacy = ["--set", "clickhouse.bundled.adminSecret.name=trustiot-clickhouse-admin"];
    const legacyBefore = annotationsOf(render(SHAPES.saas, legacy));
    const legacyRevised = annotationsOf(render(SHAPES.saas, [...legacy, "--set", "clickhouse.bundled.adminSecretRevision=2"]));
    expect(legacyRevised["checksum/admin-secret"]).not.toBe(legacyBefore["checksum/admin-secret"]);
    // A generation from a values file renders as exact digits, like migrate.passwordGeneration.
    const dir = mkdtempSync(join(tmpdir(), "trustiot-chart-"));
    try {
      const file = join(dir, "generation.yaml");
      writeFileSync(file, "clickhouse:\n  bundled:\n    adminPasswordGeneration: 20261015\n");
      const fromFile = ((templateOf(render([...SHAPES.saas, file])).spec.initContainers ?? []) as Container[])[0]?.env?.find(
        (e) => e.name === "TRUSTIOT__ADMIN__GENERATION",
      );
      expect(fromFile?.value).toBe("20261015");
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("external mode renders no ClickHouse workload", () => {
    const external = render(SHAPES.saas, [
      "--set",
      "clickhouse.mode=external",
      "--set",
      "clickhouse.external.url=https://ch.saas.trustiot.test:8443",
      "--set",
      "clickhouse.bundled.adminSecret.name=",
      "--set",
      "clickhouse.external.caSecret.name=trustiot-ch-ca",
      ...EXTERNAL_MIGRATION,
    ]);
    expect(external.filter((m) => m.metadata.name.startsWith(`${RELEASE}-clickhouse`))).toEqual([]);
  });

  test("services and the collector cannot read the admin password: no mount, no env, no envFrom, no Role", () => {
    // Laysi/trustiot-v2#403 acceptance: only ClickHouse's init container (get and patch) and the
    // migration Job (get, through the API) reach the generated Secret.
    const secret = `${RELEASE}-clickhouse-admin-password`;
    for (const shape of SHAPE_NAMES) {
      const rendered = render([...SHAPES[shape], SMOKE], enableAllServices);
      expect(passwordSecretNames(rendered)).toContain(secret);
      const workloads = rendered.filter((m) => m.kind === "Deployment" || m.kind === "StatefulSet" || m.kind === "Job");
      const readers: string[] = [];
      for (const workload of workloads) {
        const spec = podSpec(workload);
        for (const c of [...spec.containers, ...((spec.initContainers ?? []) as Container[])]) {
          const text = JSON.stringify({ env: c.env ?? [], envFrom: c.envFrom ?? [] });
          if (text.includes(`"${secret}"`)) readers.push(`${workload.kind}/${workload.metadata.name}/${c.name}`);
        }
        if (JSON.stringify(spec.volumes ?? []).includes(`"${secret}"`)) readers.push(`${workload.kind}/${workload.metadata.name} volume`);
      }
      // The init container and the Job name it (as a setting, never a secretKeyRef or a volume).
      expect(readers.sort()).toEqual([`Job/${jobsOf(rendered)[0]}/migrate`, `StatefulSet/${RELEASE}-clickhouse/admin-password`]);
      for (const workload of workloads) {
        const spec = podSpec(workload);
        for (const c of [...spec.containers, ...((spec.initContainers ?? []) as Container[])]) {
          expect(c.env?.some((e) => e.valueFrom?.secretKeyRef?.name === secret) ?? false).toBe(false);
        }
      }
      // The Roles that name it, and who they are bound to.
      type Rule = { apiGroups?: string[]; resources?: string[]; resourceNames?: string[]; verbs: string[] };
      const grants = rendered
        .filter((m) => m.kind === "Role")
        .flatMap((role) =>
          ((role as Manifest & { rules: Rule[] }).rules ?? [])
            .filter((rule) => (rule.resources ?? []).includes("secrets") && (rule.resourceNames ?? []).includes(secret))
            .map((rule) => {
              const binding = rendered.find((m) => m.kind === "RoleBinding" && (m as Manifest & { roleRef: { name: string } }).roleRef.name === role.metadata.name) as
                | (Manifest & { subjects: { name: string }[] })
                | undefined;
              return `${(binding?.subjects ?? []).map((s) => s.name).join(",")}: ${rule.verbs.join(",")}`;
            }),
        );
      expect(grants.sort()).toEqual([`${RELEASE}-clickhouse: get,patch`, `${RELEASE}-migrate: get`]);
    }
  });

  function jobsOf(rendered: Manifest[]): string[] {
    return rendered.filter((m) => m.kind === "Job").map((m) => m.metadata.name);
  }

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
    // The chart adds the architecture of the init container's migration image.
    expect(spec.template.spec.nodeSelector).toEqual({ "trustiot.io/role": "data", "kubernetes.io/arch": "arm64" });
    expect(spec.template.spec.securityContext?.runAsNonRoot).toBe(true);
  });
});

// tools/migrate/tests/mqtt_broker.rs runs the configuration, the probe and the files of the init
// containers and the sidecar in the pinned image; these tests check that the chart deploys exactly
// that, and the boundaries around it (README.md, "MQTT broker").
describe("MQTT broker (decision 6, Laysi/trustiot-v2#84)", () => {
  const NAME = `${RELEASE}-mqtt-broker`;
  const ACCOUNTS = `${RELEASE}-mqtt-broker-accounts`;
  const TLS = `${RELEASE}-mqtt-broker-tls`;
  const TLS_DIR = "/etc/trustiot/mqtt-broker-tls";
  const GATEWAY_PORT = 8883;
  const SERVICE_PORT = 1883;
  const brokerOn = ["--set", "mqttBroker.enabled=true"];
  // The broker's callers in values.yaml's mesh.calls: certs lists it since Laysi/trustiot-v2#634.
  // certs, ingest (Laysi/trustiot-v2#642) and gateway-ops (Laysi/trustiot-v2#721) get their
  // accounts from the chart.
  const callers = ["ingest", "gateway-ops", "mqtt-bridge", "certs"].flatMap((name) => ["--set", `services.${name}.enabled=true`]);
  // certs without the broker in its calls, as before Laysi/trustiot-v2#634.
  const certsOff = ["--set-json", 'mesh.calls.certs=["postgres"]'];
  const manifests = render(SHAPES.saas, [...brokerOn, ...callers]);
  const statefulSet = find(manifests, "StatefulSet", NAME);
  const pod = podSpec(statefulSet) as PodSpec & { initContainers: Container[]; shareProcessNamespace?: boolean; nodeSelector?: Record<string, string> };
  const containerOf = (spec: YamlMap, name: string) => {
    const found = [...((spec.initContainers ?? []) as Container[]), ...((spec.containers ?? []) as Container[])].find((c) => c.name === name);
    if (!found) throw new Error(`no container ${name}`);
    return found as Container & { command?: string[]; args?: string[]; volumeMounts: YamlMap[] };
  };
  const broker = containerOf(pod, "mosquitto");
  const brokerTest = readFileSync(join(REPO_ROOT, "tools/migrate/tests/mqtt_broker.rs"), "utf8");
  const rustConst = (text: string, name: string) => text.match(new RegExp(`const ${name}: &str =\\s*"([^"]+)"`))?.[1] ?? `(no ${name})`;
  const configOf = (rendered: Manifest[]) => (find(rendered, "ConfigMap", `${NAME}-config`).data as Record<string, string>)["mosquitto.conf"] ?? "";
  // The gateway listener, which the trust containers copy into the directory mosquitto.conf
  // includes once the server certificate is usable (Laysi/trustiot-v2#535).
  const listenerOf = (rendered: Manifest[]) => (find(rendered, "ConfigMap", `${NAME}-config`).data as Record<string, string>)["gateway-listener.conf"] ?? "";
  const LISTENERS_DIR = "/mosquitto/listeners";
  const CONFIG_DIR = "/etc/trustiot/mqtt-broker-config";
  const CERTIFICATE = `${NAME}-certificate`;
  const principal = (service: string) => `cluster.local/ns/${NAMESPACE}/sa/${RELEASE}-${service}`;
  type Rule = { from: { source: { principals: string[] } }[]; to?: { operation: { ports: string[] } }[] };
  type Peer = { podSelector?: { matchLabels: Record<string, string> }; ipBlock?: unknown };
  const policyOf = (rendered: Manifest[], kind: "AuthorizationPolicy" | "NetworkPolicy", name = NAME) => find(rendered, kind, name).spec as YamlMap;
  const principalsOfPolicy = (rendered: Manifest[], service: string) =>
    ((policyOf(rendered, "AuthorizationPolicy", `${RELEASE}-${service}`).rules ?? []) as Rule[]).flatMap((rule) => rule.from.flatMap((from) => from.source.principals));
  // The components whose pods the broker's NetworkPolicy admits, and whether any rule admits an address block.
  const networkSources = (rendered: Manifest[]) => {
    const rules = (policyOf(rendered, "NetworkPolicy").ingress ?? []) as { from?: Peer[]; ports?: unknown[] }[];
    const peers = rules.flatMap((rule) => rule.from ?? []);
    for (const rule of rules) expect(rule.ports).toEqual([{ port: 15008, protocol: "TCP" }]);
    return {
      components: peers.map((peer) => peer.podSelector?.matchLabels["app.kubernetes.io/component"] ?? "(not a pod)").sort(),
      ipBlocks: peers.filter((peer) => peer.ipBlock !== undefined).length,
    };
  };
  const cronPod = (cronJob: Manifest) =>
    (cronJob.spec as { jobTemplate: { spec: { template: { spec: PodSpec } } } }).jobTemplate.spec.template.spec;
  const podsOf = (rendered: Manifest[]) =>
    rendered
      .filter((m) => ["Deployment", "StatefulSet", "Job", "DaemonSet", "CronJob"].includes(m.kind))
      .map((m) => ({ name: `${m.kind}/${m.metadata.name}`, pod: m.kind === "CronJob" ? cronPod(m) : podSpec(m) }));

  test("off by default and in both deployment shapes: nothing of the broker renders", () => {
    for (const shape of SHAPE_NAMES) {
      expect((readYaml(SHAPES[shape][0]).mqttBroker as YamlMap | undefined)?.enabled, shape).toBeUndefined();
      expect((readYaml(SHAPES[shape][1]).mqttBroker as YamlMap | undefined)?.enabled, shape).toBeUndefined();
      const rendered = render(SHAPES[shape], enableAllServicesWithoutBroker);
      expect(rendered.filter((m) => m.metadata.name.includes("mqtt-broker")).map((m) => `${m.kind}/${m.metadata.name}`), shape).toEqual([]);
      // The services that list the broker in mesh.calls keep running; their calls to it are skipped.
      expect(principalsOfPolicy(rendered, "certs"), shape).not.toContain(principal("mqtt-broker"));
    }
    expect((defaults.mqttBroker as YamlMap).enabled).toBe(false);
  }, SLOW_TEST_MS);

  test("the image is Mosquitto 2.1, pinned like the broker tests and the dev environment", () => {
    expect(broker.image).toMatch(/^docker\.io\/library\/eclipse-mosquitto:2\.1\.\d+-alpine@sha256:[0-9a-f]{64}$/);
    const protocolTest = readFileSync(join(REPO_ROOT, "crates/protocol/tests/broker.rs"), "utf8");
    const stack = readFileSync(join(REPO_ROOT, "scripts/lib/stack.ts"), "utf8");
    // Same tag and digest everywhere: Renovate reads each as one copy of the same pin and updates
    // them in one pull request.
    expect({
      protocolTest: rustConst(protocolTest, "MOSQUITTO_IMAGE"),
      brokerTest: rustConst(brokerTest, "MOSQUITTO_IMAGE"),
      stack: stack.match(/mosquitto:\s*"([^"]+)"/)?.[1],
    }).toEqual({ protocolTest: broker.image, brokerTest: broker.image, stack: broker.image });
    // The image's own command, without the entrypoint script, as the broker tests run it.
    expect(broker.command).toEqual(["/usr/sbin/mosquitto", "-c", "/mosquitto/config/mosquitto.conf"]);
    expect(brokerTest).toContain(`exec ${broker.command?.join(" ")}`);
  });

  test("the configuration is the file the broker tests run, plus memory_limit at three quarters of the memory limit", () => {
    const file = readFileSync(join(CHART_DIR, "files/mosquitto/mosquitto.conf"), "utf8");
    expect(configOf(manifests)).toBe(`${file}${rustConst(brokerTest, "MEMORY_LIMIT")}\n`);
    expect(rustConst(brokerTest, "MEMORY_LIMIT")).toBe("memory_limit 201326592");
    expect(rustConst(brokerTest, "CONTAINER_MEMORY")).toBe("256m");
    expect(broker.resources?.limits?.memory).toBe("256Mi");
    const larger = render(SHAPES.saas, [...brokerOn, "--set", "mqttBroker.resources.limits.memory=1Gi"]);
    expect(configOf(larger)).toBe(`${file}memory_limit 805306368\n`);
    // The gateway listener likewise, word for word, as tools/migrate/tests/mqtt_broker.rs includes it.
    const listener = readFileSync(join(CHART_DIR, "files/mosquitto/gateway-listener.conf"), "utf8");
    expect(listenerOf(manifests)).toBe(listener);
    expect(brokerTest).toContain('include_str!("../../../deploy/chart/files/mosquitto/gateway-listener.conf")');
    for (const limit of ["300M", "1.5Gi", "0Mi"]) {
      for (const schema of [[], ["--skip-schema-validation"]]) {
        const result = helmTemplate(SHAPES.saas, [...brokerOn, ...schema, "--set", `mqttBroker.resources.limits.memory=${limit}`]);
        expect({ limit, schema, code: result.code }).toEqual({ limit, schema, code: 1 });
        if (schema.length > 0) expect(result.stderr).toContain("mqttBroker.resources.limits.memory");
      }
    }
    // A regular file from a subPath mount, read-only, as the issue requires (no symlink to follow).
    expect(broker.volumeMounts).toContainEqual({ name: "config", mountPath: "/mosquitto/config/mosquitto.conf", subPath: "mosquitto.conf", readOnly: true });
  }, SLOW_TEST_MS);

  test("decision 6: TCP_NODELAY broker-wide, and the gateways' listener puts ChaCha20-Poly1305 first (Laysi/trustiot-v2#564)", () => {
    // Split the configuration as the broker reads it at each `listener` line: mosquitto.conf, with
    // the gateway listener where it includes the listeners directory (Laysi/trustiot-v2#535). The
    // part before the first listener holds the broker-wide options. Comments and blank lines are
    // dropped.
    const main = configOf(manifests).split("\n");
    const include = main.indexOf(`include_dir ${LISTENERS_DIR}`);
    expect(include).toBeGreaterThan(0);
    // After every listener of mosquitto.conf; only memory_limit, a broker-wide option, follows.
    expect(main.slice(include + 1).filter((line) => line !== "" && !line.startsWith("#"))).toEqual([rustConst(brokerTest, "MEMORY_LIMIT")]);
    expect(main.slice(0, include).some((line) => line.startsWith(`listener ${GATEWAY_PORT}`))).toBe(false);
    const sections = new Map<string, string[]>([["global", []]]);
    let current = sections.get("global") as string[];
    for (const line of [...main.slice(0, include), ...listenerOf(manifests).split("\n")]) {
      if (line === "" || line.startsWith("#")) continue;
      if (line.startsWith("listener ")) {
        current = [];
        sections.set(line, current);
      } else current.push(line);
    }
    expect([...sections.keys()]).toEqual(["global", `listener ${SERVICE_PORT}`, "listener 0 /run/mosquitto/probe.sock", `listener ${GATEWAY_PORT}`]);
    const optionsOf = (section: string, option: string) => (sections.get(section) ?? []).filter((line) => line.split(" ")[0] === option);
    // Mosquitto picks the TLS 1.3 suite in its own order, not the client's, so ChaCha20 must lead.
    expect(optionsOf(`listener ${GATEWAY_PORT}`, "ciphers_tls1.3")).toEqual([
      "ciphers_tls1.3 TLS_CHACHA20_POLY1305_SHA256:TLS_AES_256_GCM_SHA384:TLS_AES_128_GCM_SHA256",
    ]);
    expect(optionsOf(`listener ${GATEWAY_PORT}`, "tls_version")).toEqual(["tls_version tlsv1.3"]);
    expect(optionsOf("global", "set_tcp_nodelay")).toEqual(["set_tcp_nodelay true"]);
    for (const section of sections.keys()) if (section !== "global") expect(optionsOf(section, "set_tcp_nodelay"), section).toEqual([]);
    // The services' listener stays plain MQTT inside the mesh (ztunnel's mTLS), with no TLS options.
    expect((sections.get(`listener ${SERVICE_PORT}`) ?? []).filter((line) => /^(ciphers|tls_|certfile|keyfile)/.test(line))).toEqual([]);
  });

  test("the probes run the broker tests' command on the probe socket: no TCP probe, no TLS handshake, no port for the node", () => {
    const probe = rustConst(brokerTest, "PROBE");
    for (const [kind, found] of [
      ["startup", broker.startupProbe],
      ["liveness", broker.livenessProbe],
      ["readiness", broker.readinessProbe],
    ] as const) {
      const spec = found as { exec?: { command: string[] }; httpGet?: unknown; tcpSocket?: unknown; grpc?: unknown } | undefined;
      expect({ kind, command: spec?.exec?.command.join(" ") }).toEqual({ kind, command: probe });
      expect({ kind, other: [spec?.httpGet, spec?.tcpSocket, spec?.grpc].filter((x) => x !== undefined) }).toEqual({ kind, other: [] });
    }
    expect(probe).toContain("--unix /run/mosquitto/probe.sock");
    expect(configOf(manifests)).toContain("\nlistener 0 /run/mosquitto/probe.sock\n");
    expect(broker.volumeMounts).toContainEqual({ name: "run", mountPath: "/run/mosquitto" });
    expect(pod.volumes).toContainEqual({ name: "run", emptyDir: { medium: "Memory", sizeLimit: "1Mi" } });
    expect(networkSources(manifests).ipBlocks).toBe(0);
  });

  test.each(SHAPE_NAMES)("%s: reachable inside the cluster only: a ClusterIP Service, no host port, no ingress route", (shape) => {
    const rendered = render([...SHAPES[shape], SMOKE], [...enableAllServices]);
    const service = find(rendered, "Service", NAME).spec as { type: string; ports: YamlMap[]; externalIPs?: unknown; loadBalancerIP?: unknown };
    expect(service.type).toBe("ClusterIP");
    expect(service.ports.map((p) => [p.name, p.port, p.nodePort])).toEqual([
      ["mqtt-gateways", GATEWAY_PORT, undefined],
      ["mqtt-services", SERVICE_PORT, undefined],
    ]);
    expect(service.externalIPs).toBeUndefined();
    expect(service.loadBalancerIP).toBeUndefined();
    const brokerPod = podSpec(find(rendered, "StatefulSet", NAME)) as PodSpec & { hostNetwork?: unknown; hostPID?: unknown };
    expect(brokerPod.hostNetwork).toBeUndefined();
    expect(brokerPod.hostPID).toBeUndefined();
    for (const port of brokerPod.containers.flatMap((c) => c.ports ?? [])) expect(port.hostPort).toBeUndefined();
    // No route of any kind names the broker's Service, and none can be made to.
    const routes = rendered.filter((m) => /Route$/.test(m.kind));
    expect(routes.length).toBeGreaterThan(0);
    expect(routes.filter((m) => JSON.stringify(m.spec).includes(`"${NAME}"`)).map((m) => m.metadata.name)).toEqual([]);
    const routed = helmTemplate([...SHAPES[shape], SMOKE], ["--set-json", 'ingress.routes=[{"host":"app","path":"/mqtt","match":"PathPrefix","service":"mqtt-broker"}]']);
    expect(routed.code).not.toBe(0);
    expect(routed.stderr).toContain('service "mqtt-broker" is not enabled');
  });

  test("the mesh admits mqtt-bridge on the gateway port and the account holders on the service port, nobody else", () => {
    const rendered = manifests;
    const policy = policyOf(rendered, "AuthorizationPolicy");
    expect(policy.action).toBe("ALLOW");
    expect(policy.rules as Rule[]).toEqual([
      { from: [{ source: { principals: [principal("mqtt-bridge")] } }], to: [{ operation: { ports: [String(GATEWAY_PORT)] } }] },
      {
        from: [{ source: { principals: [principal("certs"), principal("gateway-ops"), principal("ingest")] } }],
        to: [{ operation: { ports: [String(SERVICE_PORT)] } }],
      },
    ]);
    // HBONE hides the destination port from a NetworkPolicy: it admits the same callers'
    // pods, and ztunnel enforces the ports above. No address block: the probes need none.
    expect(networkSources(rendered)).toEqual({ components: ["certs", "gateway-ops", "ingest", "mqtt-bridge"], ipBlocks: 0 });
    // certs reaches the broker only while it lists it (values.yaml does, Laysi/trustiot-v2#634).
    const withoutCerts = render(SHAPES.saas, [...brokerOn, ...callers, ...certsOff]);
    expect(principalsOfPolicy(withoutCerts, "mqtt-broker")).not.toContain(principal("certs"));
    expect(networkSources(withoutCerts).components).not.toContain("certs");
    // A service that is not one of the broker's callers fails the render, so it cannot be let in
    // by accident (and gets no account).
    for (const caller of ["assets", "telemetry-query", "knowledge"]) {
      const result = helmTemplate(SHAPES.saas, [...brokerOn, "--set", `services.${caller}.enabled=true`, "--set-json", `mesh.calls.${caller}=["mqtt-broker"]`]);
      expect({ caller, code: result.code }).toEqual({ caller, code: 1 });
      expect(result.stderr).toContain(`mesh.calls.${caller}: the MQTT broker admits only`);
    }
    // Without its callers, nobody: an ALLOW policy without rules refuses every connection.
    const alone = render(SHAPES.saas, brokerOn);
    expect(policyOf(alone, "AuthorizationPolicy").rules).toBeUndefined();
    expect(policyOf(alone, "NetworkPolicy").ingress).toEqual([]);
  }, SLOW_TEST_MS);

  test("the broker reaches the certs service for the CA certificates and CRLs, only while both run", () => {
    expect(principalsOfPolicy(manifests, "certs")).toContain(principal("mqtt-broker"));
    const trustEnv = (rendered: Manifest[], name: string) => containerOf(podSpec(find(rendered, "StatefulSet", NAME)), name).env;
    for (const name of ["trust-files", "trust"]) {
      expect(trustEnv(manifests, name)).toEqual([
        { name: "TRUSTIOT__TRUST__CERTS_URL", value: `http://${RELEASE}-certs.${NAMESPACE}.svc:8080` },
        { name: "TRUSTIOT__TRUST__DIR", value: "/mosquitto/data/trust" },
        { name: "TRUSTIOT__TRUST__TLS_DIR", value: TLS_DIR },
        { name: "TRUSTIOT__TRUST__LISTENERS_DIR", value: LISTENERS_DIR },
        { name: "TRUSTIOT__TRUST__GATEWAY_LISTENER", value: `${CONFIG_DIR}/gateway-listener.conf` },
        { name: "TRUSTIOT__TRUST__INTERVAL", value: "30s" },
      ]);
    }
    // Without the certs service the broker trusts no gateway (tools/migrate's placeholder CA).
    const withoutCerts = render(SHAPES.saas, [...brokerOn, "--set", "services.mqtt-bridge.enabled=true"]);
    expect(trustEnv(withoutCerts, "trust")?.[0]).toEqual({ name: "TRUSTIOT__TRUST__CERTS_URL", value: "" });
    expect(withoutCerts.some((m) => m.metadata.name === `${RELEASE}-certs`)).toBe(false);
  });

  // Laysi/trustiot-v2#85: mqtt-bridge relays each gateway's WebSocket to the broker's gateway port,
  // the one port the mesh admits it on, and reads the source-IP mode from the platform settings.
  test.each(SHAPE_NAMES)("%s: mqtt-bridge gets the broker's gateway port, and no address without the broker", (shape) => {
    const BRIDGE = `${RELEASE}-mqtt-bridge`;
    const bridge = (rendered: Manifest[]) => {
      const container = (podSpec(find(rendered, "Deployment", BRIDGE)).containers as Container[])[0];
      if (!container) throw new Error(`no container in ${BRIDGE}`);
      return container;
    };
    const withBroker = bridge(render(SHAPES[shape], [...brokerOn, ...callers]));
    expect((withBroker.env ?? []).filter((e) => e.name.startsWith("TRUSTIOT__BROKER__"))).toEqual([
      { name: "TRUSTIOT__BROKER__ADDRESS", value: `${NAME}.${NAMESPACE}.svc:${GATEWAY_PORT}` },
    ]);
    expect(withBroker.envFrom).toEqual([{ configMapRef: { name: `${RELEASE}-platform` } }]);
    // Without the broker the bridge has nowhere to relay to and refuses to start
    // (services/mqtt-bridge/README.md, 「設定」).
    const withoutBroker = bridge(render(SHAPES[shape], ["--set", "services.mqtt-bridge.enabled=true"]));
    expect((withoutBroker.env ?? []).some((e) => e.name.startsWith("TRUSTIOT__BROKER__"))).toBe(false);
    // The address is the chart's: custom env cannot point the bridge elsewhere, with the broker or
    // without it, through the service's env or serviceDefaults.env.
    for (const broker of [[...brokerOn, ...callers], ["--set", "services.mqtt-bridge.enabled=true"]]) {
      for (const field of ["services.mqtt-bridge.env", "serviceDefaults.env"]) {
        const result = helmTemplate(SHAPES[shape], [...broker, "--set-json", `${field}=[{"name":"trustiot__broker__address","value":"elsewhere:8883"}]`]);
        expect(result.code).not.toBe(0);
        expect(result.stderr).toContain("services.mqtt-bridge: env trustiot__broker__address is set by the chart");
      }
    }
  });

  // Security review of Laysi/trustiot-v2#85: a service allowed to call mqtt-bridge could set
  // CF-Connecting-IP itself and reach the broker's gateway port through the bridge, so the ingress
  // gateway stays its only caller.
  test("no service may list mqtt-bridge in mesh.calls", () => {
    const result = helmTemplate(SHAPES.saas, [
      ...brokerOn,
      ...callers,
      "--set",
      "services.assets.enabled=true",
      "--set-json",
      'mesh.calls.assets=["postgres","mqtt-bridge"]',
    ]);
    expect(result.code).not.toBe(0);
    expect(result.stderr).toContain("mesh.calls.assets: no service may call mqtt-bridge");
  });

  // Laysi/trustiot-v2#557, Laysi/trustiot-v2#535: a certificate for the broker's name lets its
  // holder pose as the broker to every gateway, so only the broker's certificate CronJob, which
  // writes the key into the broker's Secret, reaches the port that issues it; the broker's own pod
  // reaches the API port only.
  test.each(SHAPE_NAMES)("%s: the broker's certificate CronJob alone reaches the certs service's broker-certs port, which the certs Service leaves out", (shape) => {
    const BROKER_CERTS_PORT = 8082;
    const CERTS = `${RELEASE}-certs`;
    const BROKER_CERTS = `${RELEASE}-certs-broker`;
    // Every service, one of them calling certs, and the broker with its callers.
    const rendered = render(SHAPES[shape], [...brokerOn, ...enableAllServices, "--set-json", 'mesh.calls.assets=["certs","postgres"]']);
    const deployment = find(rendered, "Deployment", CERTS);
    const certsLabels = (deployment.spec as { template: { metadata: { labels: Record<string, string> } } }).template.metadata.labels;
    const selectsCerts = (selector: Record<string, string>) => Object.entries(selector).every(([key, value]) => certsLabels[key] === value);

    // The certs service's own policy: its callers on the API port only.
    expect(policyOf(rendered, "AuthorizationPolicy", CERTS).rules as Rule[]).toEqual([
      {
        from: [{ source: { principals: [principal("assets"), principal("mqtt-broker"), principal("ingress-istio")] } }],
        to: [{ operation: { ports: ["8080"] } }],
      },
    ]);
    // The broker certificate's policy, on the certs pods: the CronJob on broker-certs only.
    const brokerPolicy = policyOf(rendered, "AuthorizationPolicy", BROKER_CERTS);
    expect(brokerPolicy.action).toBe("ALLOW");
    expect(selectsCerts((brokerPolicy.selector as { matchLabels: Record<string, string> }).matchLabels)).toBe(true);
    expect(brokerPolicy.rules as Rule[]).toEqual([
      { from: [{ source: { principals: [principal("mqtt-broker-certificate")] } }], to: [{ operation: { ports: [String(BROKER_CERTS_PORT)] } }] },
    ]);
    // That principal is the CronJob pod's ServiceAccount.
    const issuer = cronPod(find(rendered, "CronJob", CERTIFICATE));
    expect(issuer.serviceAccountName).toBe(CERTIFICATE);
    // Across every AuthorizationPolicy that selects the certs pods: a rule that admits anyone but
    // the broker names its ports, and broker-certs is not one of them. ALLOW policies add up, so
    // this is what decides who reaches the port. A policy with neither selector nor targetRefs
    // would apply to every workload of the namespace, so it counts as one on the certs pods; one
    // with targetRefs applies to its targets (the ingress gateway's) only.
    const onCerts = rendered.filter((m) => {
      if (m.kind !== "AuthorizationPolicy") return false;
      const spec = m.spec as { selector?: { matchLabels?: Record<string, string> }; targetRefs?: unknown[] };
      if (spec.targetRefs !== undefined) return false;
      return selectsCerts(spec.selector?.matchLabels ?? {});
    });
    expect(onCerts.map((m) => m.metadata.name).sort()).toEqual([CERTS, BROKER_CERTS]);
    const reachBrokerPort = onCerts.flatMap((m) =>
      (((m.spec as YamlMap).rules ?? []) as Rule[]).flatMap((rule) => {
        const ports = rule.to?.flatMap((to) => to.operation.ports);
        return ports === undefined || ports.includes(String(BROKER_CERTS_PORT)) ? rule.from.flatMap((from) => from.source.principals) : [];
      }),
    );
    expect(reachBrokerPort).toEqual([principal("mqtt-broker-certificate")]);
    // The NetworkPolicy sees HBONE only: it admits the CronJob's pods, and not the broker's.
    const network = policyOf(rendered, "NetworkPolicy", BROKER_CERTS);
    expect(network.podSelector).toEqual(policyOf(rendered, "NetworkPolicy", CERTS).podSelector);
    const issuerSelector = { "app.kubernetes.io/name": "trustiot", "app.kubernetes.io/instance": RELEASE, "app.kubernetes.io/component": "mqtt-broker-certificate" };
    expect(network.ingress).toEqual([{ from: [{ podSelector: { matchLabels: issuerSelector } }], ports: [{ port: 15008, protocol: "TCP" }] }]);
    const cronJob = find(rendered, "CronJob", CERTIFICATE).spec as { jobTemplate: { spec: { template: { metadata: { labels: Record<string, string> } } } } };
    const issuerLabels = cronJob.jobTemplate.spec.template.metadata.labels;
    expect(Object.entries(issuerSelector).every(([key, value]) => issuerLabels[key] === value)).toBe(true);
    const brokerLabels = (statefulSet.spec as { template: { metadata: { labels: Record<string, string> } } }).template.metadata.labels;
    expect(Object.entries(issuerSelector).every(([key, value]) => brokerLabels[key] === value)).toBe(false);

    // The certs Service leaves the port out; its own Service has it alone.
    const servicePorts = (name: string) =>
      (find(rendered, "Service", name).spec as { ports: { name: string; port: number; targetPort: string }[] }).ports.map((p) => [p.name, p.port, p.targetPort]);
    expect(servicePorts(CERTS)).toEqual([["http", 8080, "http"]]);
    expect(servicePorts(BROKER_CERTS)).toEqual([["broker-certs", BROKER_CERTS_PORT, "broker-certs"]]);
    const brokerService = find(rendered, "Service", BROKER_CERTS);
    expect((brokerService.spec as { type: string }).type).toBe("ClusterIP");
    expect((brokerService.spec as { selector: Record<string, string> }).selector).toEqual((find(rendered, "Service", CERTS).spec as { selector: Record<string, string> }).selector);
    expect(brokerService.metadata.labels?.["app.kubernetes.io/component"]).toBe("certs-broker");

    // The service listens where the Service points and names the broker's Service in the
    // certificate (services/certs/README.md, "broker 伺服器憑證").
    const container = podSpec(deployment).containers[0];
    expect(container?.ports?.map((p) => [p.name, p.containerPort])).toEqual([
      ["http", 8080],
      ["broker-certs", BROKER_CERTS_PORT],
      ["health", 8081],
    ]);
    const env = Object.fromEntries((container?.env ?? []).map((e) => [e.name, e.value]));
    expect(env.TRUSTIOT__BROKER__LISTEN_ADDR).toBe(`0.0.0.0:${BROKER_CERTS_PORT}`);
    expect(env.TRUSTIOT__BROKER__SERVICE_NAME).toBe(`${NAME}.${NAMESPACE}.svc`);
    // No other workload gets the broker's settings; mqtt-bridge gets only the broker's address
    // (Laysi/trustiot-v2#85).
    for (const other of rendered.filter((m) => m.kind === "Deployment" && m.metadata.name !== CERTS)) {
      const expected = other.metadata.name === `${RELEASE}-mqtt-bridge` ? ["TRUSTIOT__BROKER__ADDRESS"] : [];
      expect(podSpec(other).containers.flatMap((c) => (c.env ?? []).map((e) => e.name)).filter((name) => name.startsWith("TRUSTIOT__BROKER__"))).toEqual(expected);
    }

    // Without the broker the port's policies admit nobody, and the certificate names no Service.
    const alone = render(SHAPES[shape], enableAllServicesWithoutBroker);
    expect(policyOf(alone, "AuthorizationPolicy", BROKER_CERTS).rules).toBeUndefined();
    expect(policyOf(alone, "NetworkPolicy", BROKER_CERTS).ingress).toEqual([]);
    const aloneEnv = (podSpec(find(alone, "Deployment", CERTS)).containers[0]?.env ?? []).map((e) => e.name);
    expect(aloneEnv).toContain("TRUSTIOT__BROKER__LISTEN_ADDR");
    expect(aloneEnv).not.toContain("TRUSTIOT__BROKER__SERVICE_NAME");
    // Without the certs service, nothing of it.
    expect(render(SHAPES[shape], brokerOn).some((m) => m.metadata.name === BROKER_CERTS)).toBe(false);
  }, SLOW_TEST_MS);

  test.each(SHAPE_NAMES)("%s: the broker-certs Service stays on 8082 when the container port changes, and the policies follow the container port", (shape) => {
    const SERVICE_PORT = 8082;
    const CONTAINER_PORT = 9443;
    const CERTS = `${RELEASE}-certs`;
    const BROKER_CERTS = `${RELEASE}-certs-broker`;
    const rendered = render(SHAPES[shape], [
      ...brokerOn,
      ...enableAllServices,
      "--set-json",
      `services.certs.ports=[{"name":"http","containerPort":8080,"appProtocol":"http"},{"name":"broker-certs","containerPort":${CONTAINER_PORT},"appProtocol":"http"}]`,
    ]);
    // The broker's address does not move: the Service keeps 8082 and forwards by name.
    const service = find(rendered, "Service", BROKER_CERTS).spec as { ports: { name: string; port: number; targetPort: string }[] };
    expect(service.ports.map((p) => [p.name, p.port, p.targetPort])).toEqual([["broker-certs", SERVICE_PORT, "broker-certs"]]);
    // The service listens on the new container port, and ztunnel matches that port.
    const container = podSpec(find(rendered, "Deployment", CERTS)).containers[0];
    expect(container?.ports?.find((p) => p.name === "broker-certs")?.containerPort).toBe(CONTAINER_PORT);
    const env = Object.fromEntries((container?.env ?? []).map((e) => [e.name, e.value]));
    expect(env.TRUSTIOT__BROKER__LISTEN_ADDR).toBe(`0.0.0.0:${CONTAINER_PORT}`);
    expect(policyOf(rendered, "AuthorizationPolicy", BROKER_CERTS).rules as Rule[]).toEqual([
      { from: [{ source: { principals: [principal("mqtt-broker-certificate")] } }], to: [{ operation: { ports: [String(CONTAINER_PORT)] } }] },
    ]);
    // The CronJob's address does not move either.
    const issuerEnv = cronPod(find(rendered, "CronJob", CERTIFICATE)).containers[0]?.env ?? [];
    expect(issuerEnv.find((e) => e.name === "TRUSTIOT__CERTIFICATE__CERTS_URL")?.value).toBe(`http://${BROKER_CERTS}.${NAMESPACE}.svc:${SERVICE_PORT}`);
    // The certs service's own policy still admits its callers on the API port alone.
    const own = (policyOf(rendered, "AuthorizationPolicy", CERTS).rules as Rule[]).flatMap((rule) => rule.to?.flatMap((to) => to.operation.ports) ?? []);
    expect(own).toEqual(["8080"]);
  }, SLOW_TEST_MS);

  test("the accounts: a token only the accounts init container mounts, a Role on the accounts Secret alone, an account per holder", () => {
    expect(pod.initContainers.map((c) => c.name)).toEqual(["accounts", "trust-files"]);
    expect(pod.containers.map((c) => c.name)).toEqual(["mosquitto", "trust"]);
    const accounts = containerOf(pod, "accounts");
    expect(accounts.image).toBe(`ghcr.io/laysi/trustiot-v2/migrate:${chart.appVersion}`);
    expect(accounts.args).toEqual(["mqtt-broker-accounts"]);
    expect(accounts.env).toEqual([
      { name: "TRUSTIOT__ACCOUNTS__API_URL", value: "https://kubernetes.default.svc" },
      { name: "TRUSTIOT__ACCOUNTS__NAMESPACE", valueFrom: { fieldRef: { fieldPath: "metadata.namespace" } } },
      { name: "TRUSTIOT__ACCOUNTS__TOKEN_FILE", value: "/var/run/secrets/trustiot/kube/token" },
      { name: "TRUSTIOT__ACCOUNTS__CA_FILE", value: "/var/run/secrets/trustiot/kube/ca.crt" },
      { name: "TRUSTIOT__ACCOUNTS__SECRET", value: ACCOUNTS },
      // The enabled account holders that list the broker: mqtt-bridge carries gateways and has no
      // account.
      { name: "TRUSTIOT__ACCOUNTS__SERVICES", value: "certs,gateway-ops,ingest" },
      { name: "TRUSTIOT__ACCOUNTS__GENERATION", value: "1" },
      { name: "TRUSTIOT__ACCOUNTS__DYNSEC_FILE", value: "/mosquitto/data/dynamic-security.json" },
    ]);
    expect(accounts.volumeMounts).toEqual([
      { name: "data", mountPath: "/mosquitto/data" },
      { name: "migrate-kube-api", mountPath: "/var/run/secrets/trustiot/kube", readOnly: true },
    ]);
    for (const container of [...pod.initContainers, ...pod.containers].filter((c) => c.name !== "accounts")) {
      expect({ container: container.name, token: JSON.stringify(container.volumeMounts ?? []).includes("migrate-kube-api") }).toEqual({ container: container.name, token: false });
    }
    expect(pod.automountServiceAccountToken).toBe(false);
    expect(pod.serviceAccountName).toBe(NAME);
    expect((find(manifests, "ServiceAccount", NAME) as Manifest & { automountServiceAccountToken?: boolean }).automountServiceAccountToken).toBe(false);
    const role = find(manifests, "Role", NAME) as Manifest & { rules: unknown[] };
    expect(role.rules).toEqual([{ apiGroups: [""], resources: ["secrets"], resourceNames: [ACCOUNTS], verbs: ["get", "patch"] }]);
    const binding = find(manifests, "RoleBinding", NAME) as Manifest & { roleRef: unknown; subjects: unknown[] };
    expect(binding.roleRef).toEqual({ apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: NAME });
    expect(binding.subjects).toEqual([{ kind: "ServiceAccount", name: NAME, namespace: NAMESPACE }]);
    const declared = find(manifests, "Secret", ACCOUNTS);
    expect(Object.keys(declared).sort()).toEqual(["apiVersion", "kind", "metadata", "type"]);
    expect(declared.metadata.labels?.["trustiot.io/filled-by"]).toBe("migrate");
    expect(declared.metadata.labels?.["app.kubernetes.io/component"]).toBe("mqtt-broker");
    expect(declared.metadata.annotations?.["helm.sh/resource-policy"]).toBe("keep");
    const withoutCerts = render(SHAPES.saas, [...brokerOn, ...callers, ...certsOff]);
    expect(containerOf(podSpec(find(withoutCerts, "StatefulSet", NAME)), "accounts").env?.find((e) => e.name === "TRUSTIOT__ACCOUNTS__SERVICES")?.value).toBe(
      "gateway-ops,ingest",
    );
  });

  test("the server certificate: a Secret the chart declares empty, mounted whole and read-only by the broker pod alone, written by the certificate CronJob alone (Laysi/trustiot-v2#535)", () => {
    expect(pod.volumes).toContainEqual({ name: "server-tls", secret: { secretName: TLS, defaultMode: 0o440 } });
    const mount = { name: "server-tls", mountPath: TLS_DIR, readOnly: true };
    for (const name of ["mosquitto", "trust-files", "trust"]) expect(containerOf(pod, name).volumeMounts).toContainEqual(mount);
    expect(JSON.stringify(containerOf(pod, "accounts").volumeMounts)).not.toContain("server-tls");
    // Never through subPath, which the kubelet does not update when the Secret changes.
    for (const container of [...pod.initContainers, ...pod.containers]) {
      for (const volumeMount of container.volumeMounts ?? []) {
        if (volumeMount.name === "server-tls") expect(volumeMount.subPath).toBeUndefined();
      }
    }
    // The certificate is the gateway listener's alone; no line of mosquitto.conf but its comment on
    // the pod's mounts names the TLS directory.
    expect(listenerOf(manifests)).toContain(`\ncertfile ${TLS_DIR}/tls.crt\nkeyfile ${TLS_DIR}/tls.key\n`);
    expect(
      configOf(manifests)
        .split("\n")
        .filter((line) => !line.startsWith("#") && line.includes(TLS_DIR)),
    ).toEqual([]);
    // Declared empty, like every Secret the cluster fills (templates/passwords.yaml): no data, kept
    // on uninstall, and the CronJob's to fill.
    const everything = render([...SHAPES.saas, SMOKE], enableAllServices);
    const declared = find(everything, "Secret", TLS);
    expect(Object.keys(declared).sort()).toEqual(["apiVersion", "kind", "metadata", "type"]);
    expect((declared as Manifest & { type?: string }).type).toBe("Opaque");
    expect(declared.metadata.labels?.["trustiot.io/filled-by"]).toBe("migrate");
    expect(declared.metadata.labels?.["app.kubernetes.io/component"]).toBe("mqtt-broker");
    expect(declared.metadata.annotations?.["helm.sh/resource-policy"]).toBe("keep");
    expect(find(manifests, "Secret", TLS).metadata.name).toBe(TLS);
    // No other pod names the broker's Secrets: the CronJob's pod names the server certificate
    // Secret in its settings only (it mounts no Secret), and never the accounts Secret; certs,
    // ingest and gateway-ops mount their own key of the accounts Secret (Laysi/trustiot-v2#634,
    // Laysi/trustiot-v2#642, Laysi/trustiot-v2#721, tested below) and nothing else of the broker's.
    for (const { name, pod: found } of podsOf(everything)) {
      if (name === `StatefulSet/${NAME}`) continue;
      const other = [`Deployment/${RELEASE}-certs`, `Deployment/${RELEASE}-ingest`, `Deployment/${RELEASE}-gateway-ops`].includes(name) ? { ...found, volumes: (found.volumes ?? []).filter((v) => v.name !== "mqtt-broker-account") } : found;
      if (name === `CronJob/${CERTIFICATE}`) {
        expect(JSON.stringify(other.volumes ?? [])).not.toContain('"secret"');
        expect(JSON.stringify(other)).not.toContain(ACCOUNTS);
        expect(JSON.stringify(other)).not.toContain("secretKeyRef");
        expect(other.containers.flatMap((c) => (c.env ?? []).filter((e) => e.value === TLS).map((e) => e.name))).toEqual(["TRUSTIOT__CERTIFICATE__SECRET"]);
        continue;
      }
      expect({ name, names: JSON.stringify(other).includes(`${NAME}-`) }).toEqual({ name, names: false });
    }
    // Two Roles name the broker's Secrets, each one Secret: the broker's the accounts Secret, the
    // CronJob's the server certificate Secret. No other Role names either.
    const roles = everything.filter((m) => m.kind === "Role" || m.kind === "ClusterRole");
    const naming = (secret: string) => roles.filter((role) => JSON.stringify(role).includes(secret)).map((role) => role.metadata.name);
    expect(naming(ACCOUNTS)).toEqual([NAME]);
    expect(naming(TLS)).toEqual([CERTIFICATE]);
    for (const role of roles.filter((m) => m.metadata.name !== NAME && m.metadata.name !== CERTIFICATE)) {
      expect(JSON.stringify(role), role.metadata.name).not.toContain(NAME);
    }
  }, SLOW_TEST_MS);

  test("without a usable server certificate the broker runs without its gateway port: the trust containers copy the listener into a directory the broker includes read-only (Laysi/trustiot-v2#535)", () => {
    // Only the trust containers write the listeners directory; the broker reads it.
    expect(pod.volumes).toContainEqual({ name: "listeners", emptyDir: { medium: "Memory", sizeLimit: "1Mi" } });
    expect(broker.volumeMounts).toContainEqual({ name: "listeners", mountPath: LISTENERS_DIR, readOnly: true });
    for (const name of ["trust-files", "trust"]) {
      const mounts = containerOf(pod, name).volumeMounts;
      expect(mounts).toContainEqual({ name: "listeners", mountPath: LISTENERS_DIR });
      // The whole ConfigMap, read-only, for gateway-listener.conf.
      expect(mounts).toContainEqual({ name: "config", mountPath: CONFIG_DIR, readOnly: true });
    }
    expect(JSON.stringify(containerOf(pod, "accounts").volumeMounts)).not.toContain("listeners");
    expect(configOf(manifests)).toContain(`\ninclude_dir ${LISTENERS_DIR}\n`);
    expect(rustConst(brokerTest, "LISTENERS_DIR")).toBe(LISTENERS_DIR);
    // A new gateway listener restarts the broker like a new mosquitto.conf: the trust containers
    // copy it only when the certificate becomes usable.
    const annotations = (rendered: Manifest[]) =>
      (find(rendered, "StatefulSet", NAME).spec as { template: { metadata: { annotations: Record<string, string> } } }).template.metadata.annotations;
    expect(annotations(manifests)["checksum/mqtt-broker-config"]).toMatch(/^[0-9a-f]{64}$/);
  });

  test("custom env of a service can read neither of the broker's Secrets", () => {
    for (const [secret, key] of [
      [ACCOUNTS, "ingest"],
      [ACCOUNTS, "certs"],
      [TLS, "tls.key"],
    ]) {
      const env = `[{"name":"STOLEN","valueFrom":{"secretKeyRef":{"name":"${secret}","key":"${key}"}}}]`;
      const result = helmTemplate(SHAPES.saas, [...brokerOn, "--set", "services.ingest.enabled=true", "--set-json", `services.ingest.env=${env}`]);
      expect({ secret, code: result.code }).toEqual({ secret, code: 1 });
      expect(result.stderr).toContain(`env reads Secret ${secret}`);
    }
  }, SLOW_TEST_MS);

  // Laysi/trustiot-v2#634: the chart hands a service its broker account through
  // "trustiot.mqttBroker.serviceCredentials" (templates/_mqtt-broker.tpl): its own key of the
  // accounts Secret, mounted as a directory, and the broker's address and account in its settings.
  const ACCOUNT_DIR = "/etc/trustiot/mqtt-broker-account";
  const accountEnv = (service: string) => [
    { name: "TRUSTIOT__MQTT__HOST", value: `${NAME}.${NAMESPACE}.svc` },
    { name: "TRUSTIOT__MQTT__PORT", value: String(SERVICE_PORT) },
    { name: "TRUSTIOT__MQTT__USERNAME", value: `service:${service}` },
    { name: "TRUSTIOT__MQTT__PASSWORD_FILE", value: `${ACCOUNT_DIR}/${service}` },
  ];
  const accountVolume = (service: string) => ({ name: "mqtt-broker-account", secret: { secretName: ACCOUNTS, defaultMode: 0o440, items: [{ key: service, path: service }] } });
  const accountMount = { name: "mqtt-broker-account", mountPath: ACCOUNT_DIR, readOnly: true };
  const brokerEnvOf = (container: Container | undefined) => (container?.env ?? []).filter((e) => /^TRUSTIOT__MQTT__/i.test(e.name));
  const servicePod = (rendered: Manifest[], service: string) => podSpec(find(rendered, "Deployment", `${RELEASE}-${service}`)) as PodSpec & { initContainers?: Container[] };
  // Every pod of a render that has a volume of the accounts Secret, with the keys it selects; and
  // every mount of such a volume, by pod and container, with its subPath if any.
  const accountsUse = (rendered: Manifest[]) => {
    const keys: Record<string, string[]> = {};
    const mounts: { pod: string; container: string; subPath: unknown }[] = [];
    for (const { name, pod: spec } of podsOf(rendered)) {
      const volumes = (spec.volumes ?? []).filter((v) => JSON.stringify(v).includes(ACCOUNTS));
      if (volumes.length === 0) continue;
      keys[name] = volumes.flatMap((v) => ((v.secret as { items?: { key: string }[] } | undefined)?.items ?? [{ key: "(every key)" }]).map((item) => item.key));
      const names = new Set(volumes.map((v) => v.name));
      for (const container of [...(((spec as YamlMap).initContainers ?? []) as Container[]), ...spec.containers]) {
        for (const mount of container.volumeMounts ?? []) {
          if (names.has(mount.name as string)) mounts.push({ pod: name, container: container.name, subPath: mount.subPath });
        }
      }
    }
    return { keys, mounts };
  };

  test.each(SHAPE_NAMES)(
    "%s: certs gets its broker account: its own key alone, mounted as a directory, the broker's address and service:certs in its settings, and the mesh lets it reach the service port only (Laysi/trustiot-v2#634)",
    (shape) => {
      const rendered = render(SHAPES[shape], [...brokerOn, ...callers]);
      const certs = servicePod(rendered, "certs");
      const container = certs.containers[0];
      expect(brokerEnvOf(container)).toEqual(accountEnv("certs"));
      // The key of certs and no other, from the accounts Secret, read-only, 0440 (root and the
      // pod's fsGroup); not optional, so the pod waits for the key instead of starting without it.
      expect((certs.volumes ?? []).filter((v) => v.name === "mqtt-broker-account")).toEqual([accountVolume("certs")]);
      expect((certs.securityContext as YamlMap | undefined)?.fsGroup).toBe(65533);
      expect((container?.volumeMounts ?? []).filter((m) => m.name === "mqtt-broker-account")).toEqual([accountMount]);
      // The password file is the mounted key.
      expect(accountEnv("certs")[3]?.value).toBe(`${accountMount.mountPath}/${accountVolume("certs").secret.items[0]?.key}`);
      // Only the main container: the init containers never see the password.
      for (const init of certs.initContainers ?? []) expect(JSON.stringify(init)).not.toContain("mqtt-broker-account");
      // No other pod mounts the Secret, none through subPath, and no env of any pod reads it. A
      // Secret field of the values cannot point another pod at it either: the field tests below
      // ("... cannot name a protected Secret ...") render each field with its component on.
      const use = accountsUse(rendered);
      expect(use.keys).toEqual({
        [`Deployment/${RELEASE}-certs`]: ["certs"],
        [`Deployment/${RELEASE}-gateway-ops`]: ["gateway-ops"],
        [`Deployment/${RELEASE}-ingest`]: ["ingest"],
      });
      expect(use.mounts).toEqual([
        { pod: `Deployment/${RELEASE}-certs`, container: "certs", subPath: undefined },
        { pod: `Deployment/${RELEASE}-gateway-ops`, container: "gateway-ops", subPath: undefined },
        { pod: `Deployment/${RELEASE}-ingest`, container: "ingest", subPath: undefined },
      ]);
      for (const { name, pod: spec } of podsOf(rendered)) {
        expect({ name, reads: JSON.stringify(spec).includes(`"secretKeyRef":{"name":"${ACCOUNTS}"`) }).toEqual({ name, reads: false });
      }
      // mqtt-bridge holds no account, so it gets neither the settings nor a password.
      expect(brokerEnvOf(servicePod(rendered, "mqtt-bridge").containers[0])).toEqual([]);
      // The broker's AuthorizationPolicy lets the certs ServiceAccount reach 1883 and not 8883; its
      // NetworkPolicy admits the certs pods on 15008 (networkSources checks the port).
      const rules = policyOf(rendered, "AuthorizationPolicy").rules as Rule[];
      const principalsOn = (port: number) =>
        rules.filter((rule) => (rule.to ?? []).some((to) => to.operation.ports.includes(String(port)))).flatMap((rule) => rule.from.flatMap((from) => from.source.principals));
      expect(principalsOn(SERVICE_PORT)).toContain(principal("certs"));
      expect(principalsOn(GATEWAY_PORT)).not.toContain(principal("certs"));
      expect(rules.every((rule) => (rule.to ?? []).length > 0)).toBe(true);
      expect(networkSources(rendered).components).toContain("certs");
      // No Role but the broker's names the Secret.
      const roles = rendered.filter((m) => m.kind === "Role" || m.kind === "ClusterRole");
      expect(roles.filter((role) => JSON.stringify(role).includes(ACCOUNTS)).map((role) => role.metadata.name)).toEqual([NAME]);
    },
    SLOW_TEST_MS,
  );

  test.each(SHAPE_NAMES)("%s: without the broker, or without mqtt-broker in mesh.calls.certs, certs gets no broker settings and no Secret (Laysi/trustiot-v2#634)", (shape) => {
    for (const [label, args] of [
      ["broker off", ["--set", "services.certs.enabled=true"]],
      ["broker off, every service on", enableAllServicesWithoutBroker],
      ["certs does not list the broker", [...brokerOn, ...callers, ...certsOff]],
    ] as const) {
      const rendered = render(SHAPES[shape], [...args]);
      const certs = servicePod(rendered, "certs");
      expect({ label, env: brokerEnvOf(certs.containers[0]) }).toEqual({ label, env: [] });
      // With the broker on, ingest still gets its own key (Laysi/trustiot-v2#642); certs gets none.
      const use = Object.entries(accountsUse(rendered).keys).filter(([pod]) => label.startsWith("broker off") || pod.endsWith("-certs"));
      expect({ label, use }).toEqual({ label, use: [] });
      expect({ label, names: JSON.stringify(certs).includes(ACCOUNTS) }).toEqual({ label, names: false });
    }
  }, SLOW_TEST_MS);

  test.each(SHAPE_NAMES)("%s: custom env cannot set the broker settings of certs, with the broker or without it (Laysi/trustiot-v2#634)", (shape) => {
    // The template refuses them; the schema refuses a password-like name with a literal value too,
    // so the template's own rule is checked without the schema as well.
    for (const broker of [[...brokerOn, ...callers], ["--set", "services.certs.enabled=true"]]) {
      for (const field of ["services.certs.env", "serviceDefaults.env"]) {
        for (const name of ["TRUSTIOT__MQTT__HOST", "trustiot__mqtt__password_file", "TRUSTIOT__MQTT__USERNAME", "TRUSTIOT__MQTT__PORT"]) {
          for (const schema of [[], ["--skip-schema-validation"]]) {
            const result = helmTemplate(SHAPES[shape], [...broker, ...schema, "--set-json", `${field}=[{"name":"${name}","value":"elsewhere"}]`]);
            expect({ broker: broker.length, field, name, schema, code: result.code }).toEqual({ broker: broker.length, field, name, schema, code: 1 });
            if (schema.length > 0 || !/passw/i.test(name)) {
              expect(result.stderr).toContain(`services.certs: env ${name} is set by the chart: the service's broker account is the in-cluster broker's`);
            }
          }
        }
      }
    }
    // Its other settings stay open, the sync's interval among them (services/certs/README.md).
    const interval = render(SHAPES[shape], [...brokerOn, ...callers, "--set-json", 'services.certs.env=[{"name":"TRUSTIOT__DYNSEC__INTERVAL","value":"60s"}]']);
    expect(servicePod(interval, "certs").containers[0]?.env).toContainEqual({ name: "TRUSTIOT__DYNSEC__INTERVAL", value: "60s" });
    // A service that does not get an account keeps the name free (it reads no such setting).
    const bridge = render(SHAPES[shape], [...brokerOn, ...callers, "--set-json", 'services.mqtt-bridge.env=[{"name":"TRUSTIOT__MQTT__HOST","value":"x"}]']);
    expect(servicePod(bridge, "mqtt-bridge").containers[0]?.env).toContainEqual({ name: "TRUSTIOT__MQTT__HOST", value: "x" });
  }, SLOW_TEST_MS);

  test.each(SHAPE_NAMES)(
    "%s: ingest gets its broker account through the same helper: its own key alone, mounted as a directory, service:ingest in its settings, which custom env cannot set; no other pod has its key (Laysi/trustiot-v2#642)",
    (shape) => {
      const rendered = render(SHAPES[shape], [...brokerOn, ...callers]);
      const ingest = servicePod(rendered, "ingest");
      const container = ingest.containers[0];
      expect(brokerEnvOf(container)).toEqual(accountEnv("ingest"));
      expect((ingest.volumes ?? []).filter((v) => v.name === "mqtt-broker-account")).toEqual([accountVolume("ingest")]);
      // A group of its own reads the 0440 file: not certs' or identity's.
      expect((ingest.securityContext as YamlMap | undefined)?.fsGroup).toBe(65531);
      expect((container?.volumeMounts ?? []).filter((m) => m.name === "mqtt-broker-account")).toEqual([accountMount]);
      for (const init of ingest.initContainers ?? []) expect(JSON.stringify(init)).not.toContain("mqtt-broker-account");
      // The key ingest reaches only the ingest pod; no env of any pod reads the accounts Secret.
      const use = accountsUse(rendered);
      for (const [pod, keys] of Object.entries(use.keys)) {
        expect({ pod, ingest: keys.includes("ingest") }).toEqual({ pod, ingest: pod === `Deployment/${RELEASE}-ingest` });
      }
      for (const { name, pod: spec } of podsOf(rendered)) {
        expect({ name, reads: JSON.stringify(spec).includes(`"secretKeyRef":{"name":"${ACCOUNTS}"`) }).toEqual({ name, reads: false });
      }
      // The mesh lets ingest reach the broker's service port and not its gateway port.
      const rules = policyOf(rendered, "AuthorizationPolicy").rules as Rule[];
      const principalsOn = (port: number) =>
        rules.filter((rule) => (rule.to ?? []).some((to) => to.operation.ports.includes(String(port)))).flatMap((rule) => rule.from.flatMap((from) => from.source.principals));
      expect(principalsOn(SERVICE_PORT)).toContain(principal("ingest"));
      expect(principalsOn(GATEWAY_PORT)).not.toContain(principal("ingest"));
      // Without mqtt-broker in mesh.calls.ingest, ingest would get neither its settings nor its key
      // and never become ready, so the render fails (security review of Laysi/trustiot-v2#721, L3).
      for (const calls of ['["clickhouse","postgres"]', "[]"]) {
        const unlisted = helmTemplate(SHAPES[shape], [...brokerOn, ...callers, "--set-json", `mesh.calls.ingest=${calls}`]);
        expect({ calls, code: unlisted.code }).toEqual({ calls, code: 1 });
        expect(unlisted.stderr).toContain("services.ingest: ingest works only through the in-cluster MQTT broker, so mesh.calls.ingest must list mqtt-broker");
      }
      // Without the broker ingest has no work, so the render fails instead of deploying a pod that
      // never becomes ready (security review of Laysi/trustiot-v2#642, I-3); with the broker it
      // renders, and with ingest off the broker is not needed.
      for (const off of [["--set", "services.ingest.enabled=true"], ["--set", "services.ingest.enabled=true", "--set", "mqttBroker.enabled=false"]]) {
        const result = helmTemplate(SHAPES[shape], off);
        expect({ off, code: result.code }).toEqual({ off, code: 1 });
        expect(result.stderr).toContain("services.ingest: ingest reads the gateways' telemetry from the in-cluster MQTT broker, so it needs mqttBroker.enabled: true");
      }
      expect(helmTemplate(SHAPES[shape], [...brokerOn, "--set", "services.ingest.enabled=true"]).code).toBe(0);
      expect(helmTemplate(SHAPES[shape], ["--set", "services.ingest.enabled=false"]).code).toBe(0);
      // Its broker settings are the chart's.
      for (const name of ["TRUSTIOT__MQTT__HOST", "TRUSTIOT__MQTT__USERNAME", "trustiot__mqtt__port"]) {
        const result = helmTemplate(SHAPES[shape], [...brokerOn, ...callers, "--set-json", `services.ingest.env=[{"name":"${name}","value":"elsewhere"}]`]);
        expect({ name, code: result.code }).toEqual({ name, code: 1 });
        expect(result.stderr).toContain(`services.ingest: env ${name} is set by the chart: the service's broker account is the in-cluster broker's`);
      }
      // Its telemetry limits stay open (services/ingest/README.md).
      const limits = render(SHAPES[shape], [...brokerOn, ...callers, "--set-json", 'services.ingest.env=[{"name":"TRUSTIOT__TELEMETRY_LIMITS__BURST","value":"120"}]']);
      expect(servicePod(limits, "ingest").containers[0]?.env).toContainEqual({ name: "TRUSTIOT__TELEMETRY_LIMITS__BURST", value: "120" });
    },
    SLOW_TEST_MS,
  );

  test.each(SHAPE_NAMES)(
    "%s: gateway-ops gets its broker account through the same helper: its own key alone, mounted as a directory, service:gateway-ops in its settings, which custom env cannot set; the mesh lets it reach the broker's service port only, and no other pod has its key (Laysi/trustiot-v2#721)",
    (shape) => {
      const rendered = render(SHAPES[shape], [...brokerOn, ...callers]);
      const gatewayOps = servicePod(rendered, "gateway-ops");
      const container = gatewayOps.containers[0];
      expect(brokerEnvOf(container)).toEqual(accountEnv("gateway-ops"));
      expect(accountEnv("gateway-ops")).toEqual([
        { name: "TRUSTIOT__MQTT__HOST", value: `${NAME}.${NAMESPACE}.svc` },
        { name: "TRUSTIOT__MQTT__PORT", value: String(SERVICE_PORT) },
        { name: "TRUSTIOT__MQTT__USERNAME", value: "service:gateway-ops" },
        { name: "TRUSTIOT__MQTT__PASSWORD_FILE", value: "/etc/trustiot/mqtt-broker-account/gateway-ops" },
      ]);
      expect((gatewayOps.volumes ?? []).filter((v) => v.name === "mqtt-broker-account")).toEqual([accountVolume("gateway-ops")]);
      // A group of its own reads the 0440 file: not that of certs, identity or ingest.
      expect((gatewayOps.securityContext as YamlMap | undefined)?.fsGroup).toBe(65530);
      const everything = render(SHAPES[shape], enableAllServices);
      const groups = ["identity", "certs", "ingest"].map((service) => (servicePod(everything, service).securityContext as YamlMap | undefined)?.fsGroup);
      expect(groups).not.toContain(65530);
      expect((container?.volumeMounts ?? []).filter((m) => m.name === "mqtt-broker-account")).toEqual([accountMount]);
      for (const init of gatewayOps.initContainers ?? []) expect(JSON.stringify(init)).not.toContain("mqtt-broker-account");
      // The key gateway-ops reaches only the gateway-ops pod; no env of any pod reads the accounts Secret.
      const use = accountsUse(rendered);
      for (const [pod, keys] of Object.entries(use.keys)) {
        expect({ pod, gatewayOps: keys.includes("gateway-ops") }).toEqual({ pod, gatewayOps: pod === `Deployment/${RELEASE}-gateway-ops` });
      }
      for (const { name, pod: spec } of podsOf(rendered)) {
        expect({ name, reads: JSON.stringify(spec).includes(`"secretKeyRef":{"name":"${ACCOUNTS}"`) }).toEqual({ name, reads: false });
      }
      // The mesh lets gateway-ops reach the broker's service port and not its gateway port, and
      // lets no caller reach gateway-ops: it has no API.
      const rules = policyOf(rendered, "AuthorizationPolicy").rules as Rule[];
      const principalsOn = (port: number) =>
        rules.filter((rule) => (rule.to ?? []).some((to) => to.operation.ports.includes(String(port)))).flatMap((rule) => rule.from.flatMap((from) => from.source.principals));
      expect(principalsOn(SERVICE_PORT)).toContain(principal("gateway-ops"));
      expect(principalsOn(GATEWAY_PORT)).not.toContain(principal("gateway-ops"));
      expect(networkSources(rendered).components).toContain("gateway-ops");
      expect(principalsOfPolicy(rendered, "gateway-ops")).toEqual([]);
      // Without mqtt-broker in mesh.calls.gateway-ops it would get neither its settings nor its key
      // and never become ready, so the render fails (security review of Laysi/trustiot-v2#721, L3);
      // with gateway-ops off the list does not matter.
      for (const calls of ['["postgres"]', "[]"]) {
        const unlisted = helmTemplate(SHAPES[shape], [...brokerOn, ...callers, "--set-json", `mesh.calls.gateway-ops=${calls}`]);
        expect({ calls, code: unlisted.code }).toEqual({ calls, code: 1 });
        expect(unlisted.stderr).toContain("services.gateway-ops: gateway-ops works only through the in-cluster MQTT broker, so mesh.calls.gateway-ops must list mqtt-broker");
      }
      expect(helmTemplate(SHAPES[shape], [...brokerOn, "--set", "services.gateway-ops.enabled=false", "--set-json", 'mesh.calls.gateway-ops=["postgres"]']).code).toBe(0);
      // Without the broker no gateway gets its configuration and the pod never becomes ready, so
      // the render fails; with the broker it renders, and with gateway-ops off the broker is not
      // needed.
      for (const off of [["--set", "services.gateway-ops.enabled=true"], ["--set", "services.gateway-ops.enabled=true", "--set", "mqttBroker.enabled=false"]]) {
        const result = helmTemplate(SHAPES[shape], off);
        expect({ off, code: result.code }).toEqual({ off, code: 1 });
        expect(result.stderr).toContain("services.gateway-ops: gateway-ops delivers the gateways' configurations through the in-cluster MQTT broker, so it needs mqttBroker.enabled: true");
      }
      expect(helmTemplate(SHAPES[shape], [...brokerOn, "--set", "services.gateway-ops.enabled=true"]).code).toBe(0);
      expect(helmTemplate(SHAPES[shape], ["--set", "services.gateway-ops.enabled=false"]).code).toBe(0);
      // Its broker settings are the chart's. The schema refuses a password-like name with a
      // literal value too, so the template's own rule is checked without the schema as well.
      for (const name of ["TRUSTIOT__MQTT__HOST", "TRUSTIOT__MQTT__USERNAME", "trustiot__mqtt__port", "trustiot__mqtt__password_file"]) {
        for (const schema of [[], ["--skip-schema-validation"]]) {
          const result = helmTemplate(SHAPES[shape], [...brokerOn, ...callers, ...schema, "--set-json", `services.gateway-ops.env=[{"name":"${name}","value":"elsewhere"}]`]);
          expect({ name, schema, code: result.code }).toEqual({ name, schema, code: 1 });
          if (schema.length > 0 || !/passw/i.test(name)) {
            expect(result.stderr).toContain(`services.gateway-ops: env ${name} is set by the chart: the service's broker account is the in-cluster broker's`);
          }
        }
      }
    },
    SLOW_TEST_MS,
  );

  // The helper is the one every account holder uses: ingest joined with Laysi/trustiot-v2#642 and
  // gateway-ops with Laysi/trustiot-v2#721, each only by adding its name to
  // "trustiot.mqttBroker.credentialServices". A copy of the chart with that list rewritten shows
  // that each pod gets its own key and account and nobody else's.
  test("the helper hands every account holder its own key and nobody else's (Laysi/trustiot-v2#634)", () => {
    const dir = mkdtempSync(join(tmpdir(), "trustiot-chart-broker-"));
    try {
      const copy = join(dir, "trustiot");
      cpSync(CHART_DIR, copy, { recursive: true, filter: (source) => relative(CHART_DIR, source).split("/")[0] !== "tests" });
      const helpers = join(copy, "templates/_mqtt-broker.tpl");
      const listed = '{{- $services := list "certs" "ingest" "gateway-ops" -}}';
      const text = readFileSync(helpers, "utf8");
      expect(text.split(listed).length).toBe(2);
      const renderCopy = (services: string[], args: string[]) => {
        writeFileSync(helpers, text.replace(listed, `{{- $services := list ${services.map((service) => `"${service}"`).join(" ")} -}}`));
        return run([HELM, "template", RELEASE, copy, "--namespace", NAMESPACE, ...valuesArgs(SHAPES.saas), ...args]);
      };
      const holders = ["certs", "gateway-ops", "ingest"];
      const result = renderCopy(holders, [...brokerOn, ...callers]);
      expect(result.stderr).toBe("");
      const rendered = parseManifests(result.stdout);
      for (const service of holders) {
        const pod = servicePod(rendered, service);
        expect({ service, env: brokerEnvOf(pod.containers[0]) }).toEqual({ service, env: accountEnv(service) });
        expect({ service, volumes: (pod.volumes ?? []).filter((v) => v.name === "mqtt-broker-account") }).toEqual({ service, volumes: [accountVolume(service)] });
        expect({ service, mounts: (pod.containers[0]?.volumeMounts ?? []).filter((m) => m.name === "mqtt-broker-account") }).toEqual({ service, mounts: [accountMount] });
      }
      const use = accountsUse(rendered);
      expect(use.keys).toEqual(Object.fromEntries(holders.map((service) => [`Deployment/${RELEASE}-${service}`, [service]])));
      expect(use.mounts.every((mount) => mount.subPath === undefined)).toBe(true);
      expect(use.mounts.map((mount) => mount.pod).sort()).toEqual(holders.map((service) => `Deployment/${RELEASE}-${service}`));
      expect(brokerEnvOf(servicePod(rendered, "mqtt-bridge").containers[0])).toEqual([]);
      // A holder that does not list the broker gets nothing, even in the list (certs: ingest and
      // gateway-ops refuse to render without it).
      const unlisted = parseManifests(renderCopy(holders, [...brokerOn, ...callers, "--set-json", 'mesh.calls.certs=["postgres"]']).stdout);
      expect(Object.keys(accountsUse(unlisted).keys).sort()).toEqual([`Deployment/${RELEASE}-gateway-ops`, `Deployment/${RELEASE}-ingest`]);
      expect(brokerEnvOf(servicePod(unlisted, "certs").containers[0])).toEqual([]);
      // Only account holders can be in the list.
      for (const intruder of ["mqtt-bridge", "assets"]) {
        const refused = renderCopy([...holders, intruder], [...brokerOn, ...callers]);
        expect({ intruder, code: refused.code }).toEqual({ intruder, code: 1 });
        expect(refused.stderr).toContain(`trustiot.mqttBroker.credentialServices: ${intruder} holds no broker account`);
      }
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  }, SLOW_TEST_MS);

  // Every Secret field of the values ("trustiot.values.checkSecretRefs" in _helpers.tpl), with the
  // settings that put its component to work, as a function of the Secret's name. Each field hands
  // its Secret to a pod or a gateway, so a protected Secret there would, for example, mount the
  // dynsec administrator's password (key certs of the accounts Secret) into the collector, or send
  // it to an AI provider as an API key (security review of Laysi/trustiot-v2#634).
  const FORWARD = ["--set", "observability.collector.forward.enabled=true", "--set", "observability.collector.forward.endpoint=https://siem.trustiot.test:4318"];
  const EXTERNAL_POSTGRES = ["--set", "postgres.mode=external", "--set", "postgres.external.host=pg.trustiot.test"];
  const EXTERNAL_CLICKHOUSE = ["--set", "clickhouse.mode=external", "--set", "clickhouse.external.url=https://ch.trustiot.test:8443"];
  const PROTECTED_SECRET_FIELDS: [string, (secret: string) => string[]][] = [
    ["ingress.direct.tlsSecret", (secret) => ["--set", "ingress.exposure=direct", "--set", `ingress.direct.tlsSecret=${secret}`]],
    [
      "internalIdentity.signingKeySecret.name",
      (secret) => [
        ...["--set", "services.identity.enabled=true", "--set", `internalIdentity.signingKeySecret.name=${secret}`],
        ...["--set-json", 'internalIdentity.publicKeys=[{"kid":"2026-10-a","x":"jxSgv4kffdHZ1jN2sf_IVpE4VGbJdheBqhCAAqI89Q8"}]'],
      ],
    ],
    ["postgres.external.caSecret.name", (secret) => [...EXTERNAL_MIGRATION, ...EXTERNAL_POSTGRES, "--set", `postgres.external.caSecret.name=${secret}`]],
    ["clickhouse.bundled.adminSecret.name", (secret) => ["--set", `clickhouse.bundled.adminSecret.name=${secret}`]],
    ["clickhouse.external.caSecret.name", (secret) => [...EXTERNAL_MIGRATION, ...EXTERNAL_CLICKHOUSE, "--set", `clickhouse.external.caSecret.name=${secret}`]],
    [
      "qdrant.external.credentialsSecret.name",
      (secret) => [
        ...["--set", "qdrant.mode=external", "--set", "qdrant.external.url=https://qdrant.trustiot.test:6333", "--set", `qdrant.external.credentialsSecret.name=${secret}`],
        ...["--set", "services.knowledge.enabled=true", "--set-json", 'services.knowledge.credentials=["qdrant-read","qdrant-write"]'],
      ],
    ],
    ["objectStorage.credentialsSecret.name", (secret) => ["--set", `objectStorage.credentialsSecret.name=${secret}`, "--set-json", 'services.certs.credentials=["object-storage"]']],
    ["smtp.credentialsSecret.name", (secret) => ["--set", `smtp.credentialsSecret.name=${secret}`, "--set-json", 'services.certs.credentials=["smtp"]']],
    ...["anthropic", "openai"].map((provider): [string, (secret: string) => string[]] => [
      `ai.providers.${provider}.apiKeySecret.name`,
      (secret) => [
        ...["--set", "services.ai-gateway.enabled=true", "--set", `ai.providers.${provider}.apiKeySecret.name=${secret}`],
        ...["--set-json", `ai.roles.test={"models":["${provider}/model-test"]}`],
      ],
    ]),
    [
      "observability.sentry.dsnSecret.name",
      (secret) => ["--set", "observability.sentry.enabled=true", "--set", "observability.sentry.environment=test", "--set", `observability.sentry.dsnSecret.name=${secret}`],
    ],
    [
      "observability.collector.forward.authHeader.valueSecret.name",
      (secret) => [...FORWARD, "--set", "observability.collector.forward.authHeader.name=Authorization", "--set", `observability.collector.forward.authHeader.valueSecret.name=${secret}`],
    ],
    ["observability.collector.forward.caSecret.name", (secret) => [...FORWARD, "--set", `observability.collector.forward.caSecret.name=${secret}`]],
    // The migration Secrets are protected themselves: checked against the chart's own Secrets.
    [
      "postgres.external.migrationSecret.name",
      (secret) => [...EXTERNAL_POSTGRES, "--set", "postgres.external.caSecret.name=trustiot-pg-ca", "--set", `postgres.external.migrationSecret.name=${secret}`],
    ],
    [
      "clickhouse.external.migrationSecret.name",
      (secret) => [...EXTERNAL_CLICKHOUSE, "--set", "clickhouse.external.caSecret.name=trustiot-ch-ca", "--set", `clickhouse.external.migrationSecret.name=${secret}`],
    ],
  ];
  // Protected Secrets a field could name: the broker's accounts (the dynsec administrator's
  // password under the key certs) and the certs service's issuing CA keys.
  const PROTECTED_SECRETS = [ACCOUNTS, `${RELEASE}-certs-ca-keys`];
  const OWN_SECRET = "trustiot-own-secret";

  test("every Secret field of the values is in the list the chart checks against its protected Secrets", () => {
    // Each key that ends in Secret (a {name, key} map, or ingress.direct.tlsSecret's bare name);
    // global.imagePullSecrets is not one: only the kubelet reads those.
    const fields: string[] = [];
    const walk = (node: unknown, path: string[]) => {
      if (!isMap(node)) return;
      for (const [key, value] of Object.entries(node)) {
        if (key.endsWith("Secret")) fields.push([...path, key, ...(isMap(value) ? ["name"] : [])].join("."));
        else walk(value, [...path, key]);
      }
    };
    walk(defaults, []);
    expect(fields.sort()).toEqual(PROTECTED_SECRET_FIELDS.map(([field]) => field).sort());
    const helpers = readFileSync(join(CHART_DIR, "templates/_helpers.tpl"), "utf8");
    for (const field of fields.filter((f) => !f.includes("migrationSecret") && !f.startsWith("ai.providers."))) {
      expect({ field, checked: helpers.includes(`"field" "${field}"`) }).toEqual({ field, checked: true });
    }
  });

  test.each(PROTECTED_SECRET_FIELDS.map(([field]) => field))(
    "%s cannot name a protected Secret, so no pod but certs, ingest and gateway-ops gets a broker account's key (security review of Laysi/trustiot-v2#634)",
    (field) => {
      const settings = PROTECTED_SECRET_FIELDS.find(([name]) => name === field)?.[1];
      if (!settings) throw new Error(`no settings for ${field}`);
      for (const shape of SHAPE_NAMES) {
        // A Secret of its own works, the field is in use, and only certs, ingest and gateway-ops
        // have the accounts Secret, each its own key.
        const rendered = render(SHAPES[shape], [...brokerOn, ...callers, ...settings(OWN_SECRET)]);
        expect({ shape, field, used: JSON.stringify(rendered).includes(OWN_SECRET) }).toEqual({ shape, field, used: true });
        expect({ shape, field, use: accountsUse(rendered).keys }).toEqual({
          shape,
          field,
          use: {
            [`Deployment/${RELEASE}-certs`]: ["certs"],
            [`Deployment/${RELEASE}-gateway-ops`]: ["gateway-ops"],
            [`Deployment/${RELEASE}-ingest`]: ["ingest"],
          },
        });
        for (const secret of PROTECTED_SECRETS) {
          const refused = helmTemplate(SHAPES[shape], [...brokerOn, ...callers, ...settings(secret)]);
          expect({ shape, field, secret, code: refused.code }).toEqual({ shape, field, secret, code: 1 });
          expect(refused.stderr).toContain(`${field}: Secret ${secret} is one the chart hands only to its own consumers`);
        }
      }
    },
    SLOW_TEST_MS,
  );

  test.each(SHAPE_NAMES)("%s: an external database's CA cannot share the migration account's Secret (security review of Laysi/trustiot-v2#634)", (shape) => {
    for (const [field, args] of [
      ["postgres.external.caSecret.name", [...EXTERNAL_POSTGRES, ...EXTERNAL_MIGRATION, "--set", "postgres.external.caSecret.name=trustiot-pg-migrate"]],
      ["clickhouse.external.caSecret.name", [...EXTERNAL_CLICKHOUSE, ...EXTERNAL_MIGRATION, "--set", "clickhouse.external.caSecret.name=trustiot-ch-migrate"]],
    ] as const) {
      const refused = helmTemplate(SHAPES[shape], [...args]);
      expect({ field, code: refused.code }).toEqual({ field, code: 1 });
      expect(refused.stderr).toContain(`${field}: Secret trustiot-`);
    }
  }, SLOW_TEST_MS);

  test("the pod: restricted Pod Security Standard, one user in every container, a shared process namespace for the reload signal, the ambient mesh", () => {
    const template = (statefulSet.spec as { template: { metadata: { labels: Record<string, string> } } }).template;
    expect(template.metadata.labels["istio.io/dataplane-mode"]).toBe("ambient");
    expect(pod.securityContext).toEqual({ runAsNonRoot: true, runAsUser: 1883, runAsGroup: 1883, fsGroup: 1883, seccompProfile: { type: "RuntimeDefault" } });
    // The trust sidecar signals the broker (tools/migrate/src/mqtt_trust.rs), which it may only
    // as the same user: no container sets a user of its own.
    expect(pod.shareProcessNamespace).toBe(true);
    for (const container of [...pod.initContainers, ...pod.containers]) {
      expect({ container: container.name, securityContext: container.securityContext }).toEqual({
        container: container.name,
        securityContext: { allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: { drop: ["ALL"] } },
      });
      if (container.name !== "mosquitto") expect(container.image).toBe(`ghcr.io/laysi/trustiot-v2/migrate:${chart.appVersion}`);
      if (container.name !== "mosquitto") expect(container.ports).toBeUndefined();
    }
    expect(containerOf(pod, "trust-files").args).toEqual(["mqtt-broker-trust", "--once"]);
    expect(containerOf(pod, "trust").args).toEqual(["mqtt-broker-trust"]);
    // The init containers and the sidecar run the migration image, built for arm64 only.
    expect(pod.nodeSelector).toEqual({ "kubernetes.io/arch": "arm64" });
    const amd64 = helmTemplate(SHAPES.saas, [...brokerOn, "--set", "mqttBroker.nodeSelector.kubernetes\\.io/arch=amd64"]);
    expect(amd64.code).not.toBe(0);
    expect(amd64.stderr).toContain("mqttBroker.nodeSelector: kubernetes.io/arch must be arm64");
    const spec = statefulSet.spec as YamlMap & { replicas: number; persistentVolumeClaimRetentionPolicy: unknown; volumeClaimTemplates: { metadata: { name: string; labels: Record<string, string> }; spec: YamlMap }[] };
    expect(spec.replicas).toBe(1);
    expect(spec.persistentVolumeClaimRetentionPolicy).toEqual({ whenDeleted: "Retain", whenScaled: "Retain" });
    expect(spec.volumeClaimTemplates.map((claim) => claim.metadata.name)).toEqual(["data"]);
    expect(Object.keys(spec.volumeClaimTemplates[0]?.metadata.labels ?? {})).not.toContain("helm.sh/chart");
    expect(spec.volumeClaimTemplates[0]?.spec).toMatchObject({ accessModes: ["ReadWriteOnce"], resources: { requests: { storage: "1Gi" } } });
  });

  test("a new configuration, account holder or password generation restarts the broker, and nothing else", () => {
    const template = (rendered: Manifest[], kind: string, name: string) => JSON.stringify((find(rendered, kind, name).spec as YamlMap).template);
    const before = render(SHAPES.saas, [...brokerOn, ...callers]);
    expect(template(before, "StatefulSet", NAME)).toBe(template(manifests, "StatefulSet", NAME));
    const accountsChecksum = (rendered: Manifest[]) =>
      (find(rendered, "StatefulSet", NAME).spec as { template: { metadata: { annotations: Record<string, string> } } }).template.metadata.annotations["checksum/mqtt-broker-accounts"];
    for (const change of [
      ["--set", "mqttBroker.resources.limits.memory=512Mi"],
      ["--set", "mqttBroker.passwordGeneration=2"],
      certsOff,
    ]) {
      const after = render(SHAPES.saas, [...brokerOn, ...callers, ...change]);
      expect({ change, restarted: template(after, "StatefulSet", NAME) !== template(before, "StatefulSet", NAME) }).toEqual({ change, restarted: true });
      if (change !== certsOff) {
        // Every service keeps running, certs, ingest and gateway-ops included, which mount their
        // account's key and read the rotated password from it at their next connection
        // (Laysi/trustiot-v2#634, Laysi/trustiot-v2#642, Laysi/trustiot-v2#721): no generation in
        // their pod templates.
        for (const service of ["ingest", "gateway-ops", "mqtt-bridge", "certs"]) {
          expect({ change, service, same: template(after, "Deployment", `${RELEASE}-${service}`) === template(before, "Deployment", `${RELEASE}-${service}`) }).toEqual({ change, service, same: true });
        }
      }
    }
    // certs joining the account holders (values.yaml since Laysi/trustiot-v2#634) changes the
    // accounts checksum, so an upgrade restarts the broker, whose init container generates its
    // password; certs itself rolls to mount it.
    const withoutCerts = render(SHAPES.saas, [...brokerOn, ...callers, ...certsOff]);
    expect(accountsChecksum(before)).toMatch(/^[0-9a-f]{64}$/);
    expect(accountsChecksum(withoutCerts)).not.toBe(accountsChecksum(before));
    expect(template(withoutCerts, "Deployment", `${RELEASE}-certs`)).not.toBe(template(before, "Deployment", `${RELEASE}-certs`));
    // The certs pod template names no generation and no checksum of the accounts.
    expect(template(before, "Deployment", `${RELEASE}-certs`)).not.toContain("passwordGeneration");
    expect(Object.keys((find(before, "Deployment", `${RELEASE}-certs`).spec as { template: { metadata: { annotations: Record<string, string> } } }).template.metadata.annotations)).toEqual([
      "checksum/platform-config",
    ]);
    for (const generation of ["0x10", "1234567890", "-1"]) {
      const result = helmTemplate(SHAPES.saas, [...brokerOn, "--skip-schema-validation", "--set-string", `mqttBroker.passwordGeneration=${generation}`]);
      expect({ generation, code: result.code }).toEqual({ generation, code: 1 });
      expect(result.stderr).toContain("mqttBroker.passwordGeneration");
    }
    // A new certificate generation or schedule changes the CronJob only: the broker picks up a
    // renewed certificate through its mounted Secret, without a restart.
    for (const change of [
      ["--set", "mqttBroker.certificateGeneration=2"],
      ["--set", "mqttBroker.certificateSchedule=@hourly"],
    ]) {
      const after = render(SHAPES.saas, [...brokerOn, ...callers, ...change]);
      expect({ change, same: template(after, "StatefulSet", NAME) === template(before, "StatefulSet", NAME) }).toEqual({ change, same: true });
      const cronJobSpec = (rendered: Manifest[]) => JSON.stringify(find(rendered, "CronJob", CERTIFICATE).spec);
      expect({ change, cronJob: cronJobSpec(after) !== cronJobSpec(before) }).toEqual({ change, cronJob: true });
    }
  }, SLOW_TEST_MS);

  // Laysi/trustiot-v2#535: the server certificate is issued in the cluster. The CronJob's pod holds
  // the key in memory only, alone reaches the certs service's broker-certs port (the tests above)
  // and may get and patch the one Secret; nobody connects to it.
  test("the certificate CronJob: its own ServiceAccount and a Role on the server certificate Secret alone, a projected token, no Secret mounted, nobody connects to it", () => {
    const cronJob = find(manifests, "CronJob", CERTIFICATE) as Manifest & {
      spec: {
        schedule: string;
        timeZone: string;
        concurrencyPolicy: string;
        startingDeadlineSeconds: number;
        jobTemplate: { spec: { template: { metadata: { labels: Record<string, string> } } } };
      };
    };
    expect(cronJob.spec.schedule).toBe("*/15 * * * *");
    expect(cronJob.spec.timeZone).toBe("Etc/UTC");
    expect(cronJob.spec.concurrencyPolicy).toBe("Forbid");
    expect(cronJob.spec.startingDeadlineSeconds).toBe(3600);
    expect(cronJob.spec.jobTemplate.spec.template.metadata.labels["istio.io/dataplane-mode"]).toBe("ambient");
    const issuer = cronPod(cronJob) as PodSpec & { initContainers?: unknown; nodeSelector?: Record<string, string>; enableServiceLinks?: boolean };
    expect(issuer.serviceAccountName).toBe(CERTIFICATE);
    expect(issuer.automountServiceAccountToken).toBe(false);
    expect(issuer.enableServiceLinks).toBe(false);
    expect(issuer.initContainers).toBeUndefined();
    // The migration image is built for arm64 only.
    expect(issuer.nodeSelector).toEqual({ "kubernetes.io/arch": "arm64" });
    expect(issuer.containers.map((c) => c.name)).toEqual(["certificate"]);
    const container = issuer.containers[0] as Container & { args: string[] };
    expect(container.image).toBe(`ghcr.io/laysi/trustiot-v2/migrate:${chart.appVersion}`);
    expect(container.args).toEqual(["mqtt-broker-certificate"]);
    expect(container.env).toEqual([
      { name: "TRUSTIOT__CERTIFICATE__API_URL", value: "https://kubernetes.default.svc" },
      { name: "TRUSTIOT__CERTIFICATE__NAMESPACE", valueFrom: { fieldRef: { fieldPath: "metadata.namespace" } } },
      { name: "TRUSTIOT__CERTIFICATE__TOKEN_FILE", value: "/var/run/secrets/trustiot/kube/token" },
      { name: "TRUSTIOT__CERTIFICATE__CA_FILE", value: "/var/run/secrets/trustiot/kube/ca.crt" },
      { name: "TRUSTIOT__CERTIFICATE__SECRET", value: TLS },
      { name: "TRUSTIOT__CERTIFICATE__CERTS_URL", value: `http://${RELEASE}-certs-broker.${NAMESPACE}.svc:8082` },
      // The names the certs service puts in the certificate (its TRUSTIOT__BROKER__SERVICE_NAME).
      { name: "TRUSTIOT__CERTIFICATE__SERVICE_NAME", value: `${NAME}.${NAMESPACE}.svc` },
      { name: "TRUSTIOT__CERTIFICATE__GENERATION", value: "1" },
    ]);
    const certsEnv = podSpec(find(manifests, "Deployment", `${RELEASE}-certs`)).containers[0]?.env ?? [];
    expect(certsEnv.find((e) => e.name === "TRUSTIOT__BROKER__SERVICE_NAME")?.value).toBe(`${NAME}.${NAMESPACE}.svc`);
    expect(container.securityContext).toEqual({ allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: { drop: ["ALL"] } });
    // The projected token and the cluster's CA, nothing else.
    expect(container.volumeMounts).toEqual([{ name: "migrate-kube-api", mountPath: "/var/run/secrets/trustiot/kube", readOnly: true }]);
    expect(issuer.volumes?.map((v) => v.name)).toEqual(["migrate-kube-api"]);
    expect(JSON.stringify(issuer.volumes)).not.toContain('"secret"');
    expect((find(manifests, "ServiceAccount", CERTIFICATE) as Manifest & { automountServiceAccountToken?: boolean }).automountServiceAccountToken).toBe(false);
    const role = find(manifests, "Role", CERTIFICATE) as Manifest & { rules: unknown[] };
    expect(role.rules).toEqual([{ apiGroups: [""], resources: ["secrets"], resourceNames: [TLS], verbs: ["get", "patch"] }]);
    const binding = find(manifests, "RoleBinding", CERTIFICATE) as Manifest & { roleRef: unknown; subjects: unknown[] };
    expect(binding.roleRef).toEqual({ apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: CERTIFICATE });
    expect(binding.subjects).toEqual([{ kind: "ServiceAccount", name: CERTIFICATE, namespace: NAMESPACE }]);
    // The broker's own Role still names its accounts Secret alone: the broker pod cannot write its
    // certificate.
    expect((find(manifests, "Role", NAME) as Manifest & { rules: unknown[] }).rules).toEqual([
      { apiGroups: [""], resources: ["secrets"], resourceNames: [ACCOUNTS], verbs: ["get", "patch"] },
    ]);
    // Nobody connects to the CronJob's pod.
    const selector = { "app.kubernetes.io/name": "trustiot", "app.kubernetes.io/instance": RELEASE, "app.kubernetes.io/component": "mqtt-broker-certificate" };
    expect((find(manifests, "AuthorizationPolicy", CERTIFICATE) as Manifest & { spec: unknown }).spec).toEqual({ selector: { matchLabels: selector }, action: "ALLOW" });
    expect((find(manifests, "NetworkPolicy", CERTIFICATE) as Manifest & { spec: unknown }).spec).toEqual({
      podSelector: { matchLabels: selector },
      policyTypes: ["Ingress"],
      ingress: [],
    });
    for (const kind of ["CronJob", "ServiceAccount", "Role", "RoleBinding", "AuthorizationPolicy", "NetworkPolicy"]) {
      expect({ kind, count: manifests.filter((m) => m.kind === kind && m.metadata.name === CERTIFICATE).length }).toEqual({ kind, count: 1 });
    }
  }, SLOW_TEST_MS);

  test("the certificate CronJob renders only with the broker and the certs service; its name, values and schedule are checked", () => {
    // Without the certs service nothing issues the certificate: the Secret stays empty and the
    // broker serves the cloud services without its gateway port.
    const withoutCerts = render(SHAPES.saas, [...brokerOn, "--set", "services.mqtt-bridge.enabled=true"]);
    expect(withoutCerts.filter((m) => m.metadata.name === CERTIFICATE)).toEqual([]);
    expect(find(withoutCerts, "Secret", TLS).metadata.name).toBe(TLS);
    expect(render(SHAPES.saas, ["--set", "services.certs.enabled=true"]).filter((m) => m.metadata.name.startsWith(NAME))).toEqual([]);
    // The CronJob's name leaves the Job controller its 11 characters, even for the longest release
    // name (Helm caps it at 53); its ServiceAccount, Role and policies keep the component name,
    // and the broker-certs port's policy admits that ServiceAccount.
    for (const release of ["r", `trustiot-${"x".repeat(44)}`]) {
      const long = render(SHAPES.saas, [...brokerOn, ...callers], release);
      const cronJobs = long.filter((m) => m.kind === "CronJob" && m.metadata.name.endsWith("-mqtt-broker-certificate"));
      expect(cronJobs).toHaveLength(1);
      expect((cronJobs[0]?.metadata.name ?? "").length, release).toBeLessThanOrEqual(52);
      const account = cronPod(cronJobs[0] as Manifest).serviceAccountName;
      expect(account).toEndWith("-mqtt-broker-certificate");
      expect(
        long
          .filter((m) => m.kind !== "CronJob" && m.metadata.name === account)
          .map((m) => m.kind)
          .sort(),
      ).toEqual(["AuthorizationPolicy", "NetworkPolicy", "Role", "RoleBinding", "ServiceAccount"]);
      const brokerCerts = long.filter((m) => m.kind === "AuthorizationPolicy" && m.metadata.name.endsWith("-certs-broker"));
      expect(brokerCerts).toHaveLength(1);
      const principals = (((brokerCerts[0]?.spec as YamlMap).rules ?? []) as Rule[]).flatMap((rule) => rule.from.flatMap((from) => from.source.principals));
      expect(principals).toEqual([`cluster.local/ns/${NAMESPACE}/sa/${account}`]);
    }
    // At least every hour, so neither the first certificate after the certs service can sign nor a
    // renewal waits longer.
    // Both the schema and the template check it, so a schedule the API server would refuse fails
    // the render instead of the install.
    for (const schedule of ["@hourly", "*/5 * * * *", "7 * * * *", "0,30 * * * *", "59 * * * *", "*/1,7 * * * *"]) {
      const set = ["--set-json", `mqttBroker.certificateSchedule=${JSON.stringify(schedule)}`];
      expect((find(render(SHAPES.saas, [...brokerOn, ...callers, ...set]), "CronJob", CERTIFICATE).spec as { schedule: string }).schedule, schedule).toBe(schedule);
    }
    for (const schedule of [
      "@daily",
      "0 3 * * *",
      "*/15 * * *",
      "TZ=Asia/Taipei */15 * * * *",
      "",
      // Minutes out of range, a step of 0 or past the hour, ranges, and broken lists.
      "60 * * * *",
      "*/0 * * * *",
      "*/60 * * * *",
      "5-1 * * * *",
      "0-30 * * * *",
      "1,,2 * * * *",
      "*/ * * * *",
      "7, * * * *",
    ]) {
      for (const schema of [[], ["--skip-schema-validation"]]) {
        const result = helmTemplate(SHAPES.saas, [...brokerOn, ...callers, ...schema, "--set-json", `mqttBroker.certificateSchedule=${JSON.stringify(schedule)}`]);
        expect({ schedule, schema, code: result.code }).toEqual({ schedule, schema, code: 1 });
        expect(result.stderr).toContain("certificateSchedule");
      }
    }
    // Generations as the CronJob reads them: 1 to 9 digits, at least 1.
    const seventh = cronPod(find(render(SHAPES.saas, [...brokerOn, ...callers, "--set", "mqttBroker.certificateGeneration=7"]), "CronJob", CERTIFICATE));
    expect(seventh.containers[0]?.env?.at(-1)).toEqual({ name: "TRUSTIOT__CERTIFICATE__GENERATION", value: "7" });
    for (const generation of ["0", "0x10", "1234567890", "-1"]) {
      for (const schema of [[], ["--skip-schema-validation"]]) {
        const result = helmTemplate(SHAPES.saas, [...brokerOn, ...callers, ...schema, "--set-string", `mqttBroker.certificateGeneration=${generation}`]);
        expect({ generation, schema, code: result.code }).toEqual({ generation, schema, code: 1 });
        expect(result.stderr).toContain("certificateGeneration");
      }
    }
  }, SLOW_TEST_MS);
});

describe("bundled Postgres (CloudNativePG, Laysi/trustiot-v2#58)", () => {
  const cluster = `${RELEASE}-postgres`;
  type ClusterSpec = YamlMap & {
    instances: number;
    enablePDB: boolean;
    enableSuperuserAccess: boolean;
    imagePullPolicy: string;
    imagePullSecrets?: { name: string }[];
    imageCatalogRef: { apiGroup: string; kind: string; name: string; major: number };
    bootstrap: { initdb: YamlMap };
    postgresql: { shared_preload_libraries: string[]; parameters: Record<string, string>; pg_hba: string[] } & YamlMap;
    storage: { size: string; storageClass?: string };
    resources: { requests: Record<string, string>; limits: Record<string, string> };
    affinity: { nodeSelector?: Record<string, string>; tolerations?: YamlMap[] };
    inheritedMetadata: { labels: Record<string, string> };
  };
  type NetworkPolicyRule = { from: YamlMap[]; ports: { port: number; protocol: string }[] };
  const clusterSpec = (manifests: Manifest[]) => find(manifests, "Cluster", cluster).spec as ClusterSpec;
  const catalogImages = (manifests: Manifest[]) =>
    (find(manifests, "ImageCatalog", cluster).spec as { images: { major: number; image: string }[] }).images;
  const policyRules = (manifests: Manifest[]) => {
    const spec = find(manifests, "NetworkPolicy", `${RELEASE}-postgres`).spec as {
      podSelector: { matchLabels: Record<string, string> };
      policyTypes: string[];
      ingress: NetworkPolicyRule[];
    };
    return spec;
  };
  const manifests = render([...SHAPES.saas, SMOKE], ["--set", "services.identity.enabled=true", "--set", "services.mqtt-bridge.enabled=true"]);

  // deploy/images/postgres/Containerfile: `FROM ghcr.io/cloudnative-pg/postgresql:<major>.<minor>-...`
  // and `ARG AGE_VERSION=<version>`.
  const containerfile = readFileSync(join(REPO_ROOT, "deploy/images/postgres/Containerfile"), "utf8");
  const imageMajor = Number(containerfile.match(/^FROM ghcr\.io\/cloudnative-pg\/postgresql:(\d+)\./m)?.[1]);
  const imageAge = containerfile.match(/^ARG AGE_VERSION=(\S+)$/m)?.[1];

  test("the image catalog, the Cluster and the AGE extension match the image's PostgreSQL major and AGE version", () => {
    expect(imageMajor).toBeGreaterThanOrEqual(18);
    expect(imageAge).toMatch(/^\d+\.\d+\.\d+$/);
    expect(catalogImages(manifests)).toEqual([{ major: imageMajor, image: "localhost/trustiot-e2e/postgres:nightly" }]);
    const spec = clusterSpec(manifests);
    expect(spec.imageCatalogRef).toEqual({ apiGroup: "postgresql.cnpg.io", kind: "ImageCatalog", name: cluster, major: imageMajor });
    const database = find(manifests, "Database", `${cluster}-trustiot`).spec as YamlMap;
    expect(database).toEqual({
      cluster: { name: cluster },
      name: "trustiot",
      owner: "trustiot_owner",
      ensure: "present",
      databaseReclaimPolicy: "retain",
      extensions: [{ name: "age", version: imageAge }],
    });
    // AGE's parser hook is loaded in every session, so no role needs LOAD.
    expect(spec.postgresql.shared_preload_libraries).toEqual(["age"]);
  });

  test("the image is the released one by default, pinned by digest when set", () => {
    const released = render(SHAPES.onprem);
    expect(catalogImages(released)[0]?.image).toBe(`ghcr.io/laysi/trustiot-v2/postgres:${chart.appVersion}`);
    expect(clusterSpec(released).imagePullPolicy).toBe("IfNotPresent");
    const digest = `sha256:${"ab".repeat(32)}`;
    const pinned = render(SHAPES.onprem, ["--set", `postgres.bundled.image.digest=${digest}`, "--set", "postgres.bundled.image.tag=1.2.3"]);
    expect(catalogImages(pinned)[0]?.image).toBe(`ghcr.io/laysi/trustiot-v2/postgres:1.2.3@${digest}`);
    expect(clusterSpec(manifests).imagePullPolicy).toBe("Never");
  });

  test("one instance without superuser access, an ordinary owner, the built-in locale and the data kept on uninstall", () => {
    const spec = clusterSpec(manifests);
    expect(spec.instances).toBe(1);
    expect(spec.enablePDB).toBe(false);
    // No superuser password and no <cluster>-superuser Secret.
    expect(spec.enableSuperuserAccess).toBe(false);
    expect(spec.superuserSecret).toBeUndefined();
    // initdb takes the owner's password from a Secret CloudNativePG generates. The chart adds two
    // things that run as superuser: the system-database statements (next test,
    // Laysi/trustiot-v2#400), and CREATEROLE for the owner, which the migration Job needs to
    // create the service roles (Laysi/trustiot-v2#60). Nothing else runs as superuser.
    expect(spec.bootstrap.initdb).toEqual({
      database: "trustiot",
      owner: "trustiot_owner",
      encoding: "UTF8",
      localeProvider: "builtin",
      builtinLocale: "C.UTF-8",
      postInitSQL: systemDatabaseStatements(readFileSync(SYSTEM_DATABASE_SQL, "utf8")),
      postInitApplicationSQL: ["ALTER ROLE trustiot_owner CREATEROLE"],
    });
    expect(spec.managed).toBeUndefined();
    for (const kind of ["Cluster", "ImageCatalog"]) {
      expect(find(manifests, kind, cluster).metadata.annotations?.["helm.sh/resource-policy"]).toBe("keep");
    }
    expect(spec.storage).toEqual({ size: "20Gi" });
  });

  test("the bootstrap keeps TrustIoT's roles out of the postgres and template1 databases with the statements of the one SQL file", () => {
    // One set of statements (Laysi/trustiot-v2#400): templates/postgres.yaml carries them, since
    // templates never read chart files (scripts/ci/check-secret-handling.ts), and this fails when
    // they differ from the file that tools/migrate/tests/system_databases.rs and the runbook run.
    const statements = systemDatabaseStatements(readFileSync(SYSTEM_DATABASE_SQL, "utf8"));
    for (const shape of SHAPE_NAMES) {
      const initdb = clusterSpec(render(SHAPES[shape])).bootstrap.initdb as { postInitSQL?: string[]; postInitTemplateSQL?: string[] };
      expect(initdb.postInitSQL).toEqual(statements);
      // Database privileges live in the shared catalog, so nothing has to run inside template1.
      expect(initdb.postInitTemplateSQL).toBeUndefined();
    }
    expect(statements.length).toBeGreaterThan(0);
    for (const shape of SHAPE_NAMES) {
      const initdb = clusterSpec(render(SHAPES[shape])).bootstrap.initdb as Record<string, unknown>;
      for (const key of ["postInitSQL", "postInitApplicationSQL", "postInitTemplateSQL"]) {
        // CloudNativePG passes these to its initdb Job as container arguments, where Kubernetes
        // turns $$ into $ and expands $(NAME): a `DO $$ ... $$` body reached PostgreSQL as
        // `DO $ ... $` on the lab and failed the bootstrap. Dollar quotes need a named tag.
        for (const statement of (initdb[key] as string[] | undefined) ?? []) {
          expect(statement).not.toContain("$$");
          expect(statement).not.toContain("$(");
        }
      }
    }
    for (const statement of statements) {
      expect(statement).toEndWith(";");
      expect(statement).not.toMatch(/^--/m);
    }
    const plain = statements.map((s) => s.replace(/\s+/g, " "));
    expect(plain).toContain("REVOKE CONNECT, TEMPORARY ON DATABASE postgres FROM PUBLIC;");
    expect(plain).toContain("REVOKE CONNECT, TEMPORARY ON DATABASE template1 FROM PUBLIC;");
    // Only CloudNativePG's own roles that connect to `postgres` get CONNECT back, by name.
    expect(plain.filter((s) => s.startsWith("GRANT"))).toEqual(["GRANT CONNECT ON DATABASE postgres TO streaming_replica, cnpg_metrics_exporter;"]);
  });

  test("the server refuses connections without TLS, and only sizing parameters come from values", () => {
    const spec = clusterSpec(manifests);
    expect(spec.postgresql.pg_hba).toEqual(["hostnossl all all all reject"]);
    expect(spec.postgresql.parameters).toEqual({
      shared_buffers: "128MB",
      effective_cache_size: "384MB",
      work_mem: "4MB",
      maintenance_work_mem: "64MB",
      max_connections: "240",
    });
    // The string form takes the same 10–1000 as the number (values.schema.json).
    const most = clusterSpec(render(SHAPES.saas, ["--set-string", "postgres.bundled.parameters.max_connections=1000"]));
    expect(most.postgresql.parameters.max_connections).toBe("1000");
    // CloudNativePG manages TLS and replication itself; the chart sets no certificate of its own.
    expect(spec.certificates).toBeUndefined();
    expect(spec.resources).toEqual({ requests: { cpu: "100m", memory: "256Mi" }, limits: { cpu: "2", memory: "1Gi" } });
    expect(spec.affinity.nodeSelector).toEqual({ "kubernetes.io/arch": "arm64" });
  });

  test("the Postgres pods stay outside the mesh and admit only the services that use Postgres, the migration Job and the operator", () => {
    expect(clusterSpec(manifests).inheritedMetadata.labels).toEqual({ "istio.io/dataplane-mode": "none" });
    const policy = policyRules(manifests);
    expect(policy.podSelector.matchLabels).toEqual({ "cnpg.io/cluster": cluster, "cnpg.io/podRole": "instance" });
    expect(policy.policyTypes).toEqual(["Ingress"]);
    // identity lists postgres under mesh.calls; mqtt-bridge does not.
    const pods = (component: string) => ({
      podSelector: { matchLabels: { "app.kubernetes.io/name": "trustiot", "app.kubernetes.io/instance": RELEASE, "app.kubernetes.io/component": component } },
    });
    expect(policy.ingress).toEqual([
      {
        from: [pods("identity"), pods("migrate")],
        ports: [{ port: 5432, protocol: "TCP" }],
      },
      {
        from: [
          {
            namespaceSelector: { matchLabels: { "kubernetes.io/metadata.name": "cnpg-system" } },
            podSelector: { matchLabels: { "app.kubernetes.io/name": "cloudnative-pg" } },
          },
        ],
        ports: [{ port: 8000, protocol: "TCP" }],
      },
    ]);
    // A call to postgres adds no mesh policy: Postgres is outside the mesh.
    expect(manifests.filter((m) => m.kind === "AuthorizationPolicy" && m.metadata.name.includes("postgres"))).toEqual([]);
    // Without callers and the Job only the operator's rule is left, and a customer's operator can
    // be matched.
    const none = policyRules(
      render(SHAPES.onprem, [
        "--set-json",
        "mesh.calls.identity=[]",
        "--set",
        "migrate.enabled=false",
        "--set",
        "postgres.bundled.operator.namespace=operators",
        "--set-json",
        'postgres.bundled.operator.podLabels={"app.kubernetes.io/name":"cloudnative-pg","olm.owner":"cnpg"}',
      ]),
    );
    expect(none.ingress).toEqual([
      {
        from: [
          {
            namespaceSelector: { matchLabels: { "kubernetes.io/metadata.name": "operators" } },
            podSelector: { matchLabels: { "app.kubernetes.io/name": "cloudnative-pg", "olm.owner": "cnpg" } },
          },
        ],
        ports: [{ port: 8000, protocol: "TCP" }],
      },
    ]);
  });

  test("services that use Postgres get only the CA certificate of CloudNativePG's CA Secret; no other generated Secret is read", () => {
    const identity = podSpec(find(manifests, "Deployment", `${RELEASE}-identity`));
    expect(identity.volumes).toContainEqual({
      name: "postgres-ca",
      secret: { secretName: `${cluster}-ca`, items: [{ key: "ca.crt", path: "ca.crt" }] },
    });
    expect(identity.containers[0]?.volumeMounts).toContainEqual({ name: "postgres-ca", mountPath: "/etc/trustiot/postgres-ca", readOnly: true });
    expect(identity.containers[0]?.env).toContainEqual({ name: "TRUSTIOT_POSTGRES_CA_FILE", value: "/etc/trustiot/postgres-ca/ca.crt" });
    for (const name of ["mqtt-bridge", "smoke-echo"]) {
      const pod = JSON.stringify(podSpec(find(manifests, "Deployment", `${RELEASE}-${name}`)));
      expect(pod).not.toContain("postgres-ca");
      expect(pod).not.toContain("TRUSTIOT_POSTGRES_CA_FILE");
    }
    // The CA's private key and the server key stay with the operator; the owner's password
    // reaches the migration Job only, and the first platform administrator's bootstrap CronJob,
    // which runs as the migration role (Laysi/trustiot-v2#582).
    const rendered = JSON.stringify(manifests);
    expect(rendered).not.toContain("ca.key");
    for (const suffix of ["server", "replication", "superuser"]) expect(rendered).not.toContain(`${cluster}-${suffix}`);
    const readers = manifests.filter((m) => JSON.stringify(m).includes(`${cluster}-app`));
    expect(readers.map((m) => `${m.kind}/${m.metadata.name}`).sort()).toEqual([
      `CronJob/${RELEASE}-platform-admin-bootstrap`,
      expect.stringMatching(new RegExp(`^Job/${RELEASE}-migrate-[0-9a-f]{8}$`)),
    ]);
    for (const reader of readers) {
      const spec = reader.spec as { template?: { spec: PodSpec }; jobTemplate?: { spec: { template: { spec: PodSpec } } } };
      const container = (spec.template ?? spec.jobTemplate?.spec.template)?.spec.containers[0];
      expect(container?.env?.filter((e) => e.valueFrom?.secretKeyRef?.name === `${cluster}-app`)).toEqual([
        { name: "TRUSTIOT__POSTGRES__USERNAME", valueFrom: { secretKeyRef: { name: `${cluster}-app`, key: "username" } } },
        { name: "TRUSTIOT__POSTGRES__PASSWORD", valueFrom: { secretKeyRef: { name: `${cluster}-app`, key: "password" } } },
      ]);
    }
  });

  test("the Cluster name stays within CloudNativePG's 50 characters for the longest release name", () => {
    const release = `r${"x".repeat(52)}`;
    const long = render(SHAPES.saas, ["--set", "services.identity.enabled=true"], release);
    const clusters = long.filter((m) => m.kind === "Cluster");
    expect(clusters).toHaveLength(1);
    const name = clusters[0]?.metadata.name ?? "";
    expect(name.length).toBeLessThanOrEqual(50);
    expect(name).toEndWith("-postgres");
    expect(find(long, "ImageCatalog", name)).toBeDefined();
    expect((find(long, "Database", `${name}-trustiot`).spec as { cluster: { name: string } }).cluster.name).toBe(name);
    const named = (kind: string, suffix: string) => {
      const found = long.find((m) => m.kind === kind && m.metadata.name.endsWith(suffix));
      if (!found) throw new Error(`no ${kind} named *${suffix}`);
      return found;
    };
    const config = named("ConfigMap", "-platform").data as Record<string, string>;
    expect(config.TRUSTIOT_POSTGRES_HOST).toBe(`${name}-rw.${NAMESPACE}.svc`);
    const identity = podSpec(named("Deployment", "-identity"));
    expect(identity.volumes).toContainEqual({
      name: "postgres-ca",
      secret: { secretName: `${name}-ca`, items: [{ key: "ca.crt", path: "ca.crt" }] },
    });
  });

  test("long release names that start alike get different Clusters; a short one keeps its plain name", () => {
    const clusterName = (release: string) => {
      const clusters = render(SHAPES.saas, [], release).filter((m) => m.kind === "Cluster");
      expect(clusters).toHaveLength(1);
      return clusters[0]?.metadata.name ?? "";
    };
    // Both releases share their first 50 characters, more than any shortened prefix keeps.
    const common = `trustiot-${"y".repeat(41)}`;
    const [a, b] = [clusterName(`${common}-a`), clusterName(`${common}-b`)];
    expect(a).not.toBe(b);
    for (const name of [a, b]) {
      expect(name.length).toBeLessThanOrEqual(50);
      expect(name).toMatch(/^trustiot-y+-[0-9a-f]{8}-postgres$/);
    }
    // Names that fit stay readable and stable: the default release's Cluster is trustiot-postgres.
    expect(clusterName(RELEASE)).toBe(`${RELEASE.includes("trustiot") ? RELEASE : `${RELEASE}-trustiot`}-postgres`);
    const longest = `trustiot-${"z".repeat(32)}`;
    expect(clusterName(longest)).toBe(`${longest}-postgres`);
    expect(`${longest}-postgres`).toHaveLength(50);
  });

  test("placement, storage class and pull Secrets can be set", () => {
    const placed = clusterSpec(
      render(SHAPES.onprem, [
        "--set",
        "postgres.bundled.storage.storageClassName=fast-ssd",
        "--set",
        "postgres.bundled.nodeSelector.trustiot\\.io/role=data",
        "--set-json",
        'postgres.bundled.tolerations=[{"key":"dedicated","operator":"Equal","value":"data","effect":"NoSchedule"}]',
        "--set-json",
        'global.imagePullSecrets=["ghcr-pull"]',
      ]),
    );
    expect(placed.storage).toEqual({ size: "20Gi", storageClass: "fast-ssd" });
    expect(placed.affinity.nodeSelector).toEqual({ "kubernetes.io/arch": "arm64", "trustiot.io/role": "data" });
    expect(placed.affinity.tolerations).toEqual([{ key: "dedicated", operator: "Equal", value: "data", effect: "NoSchedule" }]);
    expect(placed.imagePullSecrets).toEqual([{ name: "ghcr-pull" }]);
  });

  test("the Postgres e2e overlay installs the bundled databases, the migration Job, the collector and identity as the only Postgres client", () => {
    for (const shape of SHAPE_NAMES) {
      const e2e = render([...SHAPES[shape], "tests/e2e/values-postgres-e2e.yaml"]);
      expect(clusterSpec(e2e).storage).toEqual({ size: "1Gi" });
      // One install, as a real one (Laysi/trustiot-v2#60): the Job generates the passwords of
      // identity's role and of the collector's ClickHouse account.
      expect(e2e.filter((m) => m.kind === "StatefulSet" || m.kind === "Deployment").map((m) => m.metadata.name).sort()).toEqual(
        [`${RELEASE}-clickhouse`, `${RELEASE}-identity`, `${RELEASE}-otel-collector`].sort(),
      );
      expect(e2e.filter((m) => m.kind === "Job").map((m) => m.metadata.labels?.["app.kubernetes.io/component"])).toEqual(["migrate"]);
      // And ClickHouse's init container generates its admin password (Laysi/trustiot-v2#403), the
      // identity pod's its signing keys (Laysi/trustiot-v2#404).
      expect(passwordSecretNames(e2e)).toEqual([
        `${RELEASE}-clickhouse-admin-password`,
        `${RELEASE}-identity-password`,
        `${RELEASE}-identity-signing-keys`,
        `${RELEASE}-otel-collector-password`,
      ]);
      const from = (policyRules(e2e).ingress[0]?.from ?? []) as { podSelector?: { matchLabels: Record<string, string> } }[];
      expect(from.map((f) => f.podSelector?.matchLabels["app.kubernetes.io/component"]).sort()).toEqual(["identity", "migrate"]);
      const identity = podSpec(find(e2e, "Deployment", `${RELEASE}-identity`));
      expect(identity.containers[0]?.env).toContainEqual({ name: "TRUSTIOT_POSTGRES_CA_FILE", value: "/etc/trustiot/postgres-ca/ca.crt" });
      expect(identity.containers[0]?.env).toContainEqual({ name: "TRUSTIOT__DATABASE__USERNAME", value: "identity_app" });
    }
  });

  test("external mode and the e2e overlays render no Postgres objects", () => {
    const external = render(SHAPES.saas, [
      "--set",
      "postgres.mode=external",
      "--set",
      "postgres.external.host=pg.saas.trustiot.test",
      "--set",
      "postgres.external.caSecret.name=trustiot-pg-ca",
      ...EXTERNAL_MIGRATION,
    ]);
    const e2e = SHAPE_NAMES.flatMap((shape) => [
      render([...SHAPES[shape], E2E_VALUES], ["--set", "ingress.direct.tlsSecret=trustiot-ingress-tls"]),
      render([...SHAPES[shape], "tests/e2e/values-otel-e2e.yaml"]),
    ]);
    for (const rendered of [external, ...e2e]) {
      expect(rendered.filter((m) => m.apiVersion.startsWith("postgresql.cnpg.io/"))).toEqual([]);
      expect(rendered.filter((m) => m.metadata.name === `${RELEASE}-postgres`)).toEqual([]);
    }
  });
});

type CollectorConfig = {
  receivers: { otlp: { protocols: Record<string, { endpoint: string }> } };
  processors: {
    redaction: {
      allow_all_keys: boolean;
      redact_all_types: boolean;
      summary: string;
      blocked_key_patterns: string[];
      ignored_key_patterns: string[];
      blocked_values: string[];
    };
    "transform/redact": { error_mode: string; trace_statements: { context: string; statements: string[] }[] };
  } & YamlMap;
  exporters: Record<string, YamlMap>;
  service: { pipelines: Record<string, { receivers: string[]; processors: string[]; exporters: string[] }> };
};

// Columns the ClickHouse exporter of the pinned collector inserts
// (exporter/clickhouseexporter/internal/sqltemplates/{logs,traces}_insert.sql of
// opentelemetry-collector-contrib v0.161.0, plus the optional EventName of logs). This is the
// guard on collector upgrades: a Renovate pull request that changes the collector's tag fails
// the tests below until someone has compared the new exporter's columns with 0006_logs.sql (and
// added a migration if they changed) and moved this version. A new digest of the same version
// passes on its own (renovate.json5).
const EXPORTER_VERSION = "0.161.0";
const EXPORTER_COLUMNS = {
  otel_logs: [
    "Timestamp", "TraceId", "SpanId", "TraceFlags", "SeverityText", "SeverityNumber", "ServiceName", "Body",
    "ResourceSchemaUrl", "ResourceAttributes", "ScopeSchemaUrl", "ScopeName", "ScopeVersion", "ScopeAttributes",
    "LogAttributes", "EventName",
  ],
  otel_traces: [
    "Timestamp", "TraceId", "SpanId", "ParentSpanId", "TraceState", "SpanName", "SpanKind", "ServiceName",
    "ResourceAttributes", "ScopeName", "ScopeVersion", "SpanAttributes", "Duration", "StatusCode", "StatusMessage",
    "Events", "Links",
  ],
};

describe("Postgres connection budget (Laysi/trustiot-v2#575)", () => {
  // The chart's budgets and reserved connections, read from the template the way the services'
  // Rust tests read them (crates/testkit, `chart`): the JSON body of each helper.
  const template = readFileSync(join(CHART_DIR, "templates/_postgres.tpl"), "utf8");
  function helperJson(name: string): Record<string, unknown> {
    const body = template.split(`{{- define "${name}" -}}`)[1]?.split("{{- end -}}")[0];
    if (body === undefined) throw new Error(`no helper ${name} in templates/_postgres.tpl`);
    return JSON.parse(body) as Record<string, unknown>;
  }
  type Budget = { maxConnections: { min: number; max: number }; otherPools: number };
  const budgets = helperJson("trustiot.postgres.connectionBudgets") as Record<string, Budget>;
  const reserved = Object.values(helperJson("trustiot.postgres.reservedConnections") as Record<string, number>).reduce((a, b) => a + b, 0);
  const services = defaults.services as Record<string, { database?: { maxConnections?: number } }>;
  const calls = (defaults.mesh as { calls: Record<string, string[]> }).calls;
  const postgresServices = Object.keys(calls).filter((name) => calls[name]?.includes("postgres")).sort();
  // The gateway does not render without its providers and roles (AI_GATEWAY_SETTINGS), ingest and
  // gateway-ops not without the MQTT broker.
  const enable = (names: readonly string[]) => [
    ...names.flatMap((name) => ["--set", `services.${name}.enabled=true`]),
    ...(names.includes("ai-gateway") ? AI_GATEWAY_SETTINGS : []),
    ...(names.includes("ingest") || names.includes("gateway-ops") ? ["--set", "mqttBroker.enabled=true"] : []),
  ];
  // What the defaults need with every Postgres service on at one replica, computed here from the
  // budgets and values.yaml, independently of the template's arithmetic.
  const perReplica = (name: string) => (services[name]?.database?.maxConnections ?? NaN) + (budgets[name]?.otherPools ?? NaN);
  const needed = postgresServices.reduce((sum, name) => sum + perReplica(name) * 2, 0) + reserved;
  const limit = Number((defaults.postgres as { bundled: { parameters: { max_connections: number } } }).bundled.parameters.max_connections);

  test("every service that uses Postgres by default has a budget and a pool size, and the numbers are the ones README.md states", () => {
    expect(postgresServices).toEqual(["ai-gateway", "assets", "assistant", "certs", "gateway-ops", "identity", "ingest", "telemetry-query"]);
    expect(Object.keys(budgets).sort()).toEqual(postgresServices);
    expect(Object.fromEntries(postgresServices.map((name) => [name, perReplica(name)]))).toEqual({
      "ai-gateway": 4,
      assets: 14,
      assistant: 10,
      certs: 9,
      "gateway-ops": 13,
      identity: 31,
      ingest: 14,
      "telemetry-query": 12,
    });
    expect(reserved).toBe(18);
    // 8 to spare (README.md, 「連線預算」): the next service or larger pool that uses Postgres
    // raises the limit. Laysi/trustiot-v2#721 raised it from 200 for gateway-ops,
    // Laysi/trustiot-v2#645 from 220 for the assistant's API and purge pools and gateway-ops' larger
    // pool from Laysi/trustiot-v2#112.
    expect(needed).toBe(232);
    expect(limit).toBe(240);
    const readme = readFileSync(join(CHART_DIR, "README.md"), "utf8");
    for (const name of postgresServices) {
      expect(readme).toContain(`| \`${name}\` | \`${services[name]?.database?.maxConnections}\` | ${budgets[name]?.otherPools} | ${perReplica(name)} |`);
    }
    expect(readme).toContain(`| 合計 | | | 107 | 214 |`);
  });

  test.each(SHAPE_NAMES)("%s: the defaults, every Postgres service and every planned service pass the check", (shape) => {
    for (const args of [[], enable(postgresServices), enableAllServices]) {
      const result = helmTemplate([...SHAPES[shape], SMOKE], args);
      expect(result.stderr).toBe("");
      expect(result.code).toBe(0);
    }
  });

  test("the check passes at exactly what the services and the reserved connections need, and fails one below with the breakdown", () => {
    const at = (n: number) => helmTemplate(SHAPES.saas, [...enable(postgresServices), "--set", `postgres.bundled.parameters.max_connections=${n}`]);
    const exact = at(needed);
    expect(exact.stderr).toBe("");
    expect(exact.code).toBe(0);
    const below = at(needed - 1);
    expect(below.code).not.toBe(0);
    expect(below.stderr).toContain(
      `postgres.bundled.parameters.max_connections is ${needed - 1}, but ${needed} connections are needed: the services can open ${needed - reserved} while all of them roll at once ` +
        "(per replica x pods: ai-gateway 4 x 2, assets 14 x 2, assistant 10 x 2, certs 9 x 2, gateway-ops 13 x 2, identity 31 x 2, ingest 14 x 2, telemetry-query 12 x 2), " +
        `and ${reserved} stay reserved for superusers, CloudNativePG, the migration Job and the platform administrator bootstrap.`,
    );
    // The string form of max_connections counts the same.
    const asString = helmTemplate(SHAPES.saas, [...enable(postgresServices), "--set-string", `postgres.bundled.parameters.max_connections=${needed - 1}`]);
    expect(asString.code).not.toBe(0);
    expect(asString.stderr).toContain(`but ${needed} connections are needed`);
  });

  test.each([
    ["two more identity replicas", ["--set", "services.identity.replicas=3"], 31, 4],
    ["a larger assets pool", ["--set", "services.assets.database.maxConnections=30"], 34, 2],
  ] as const)("the defaults with %s fail: every replica plus the one a rolling update adds counts", (_what, args, perReplicaAfter, pods) => {
    const result = helmTemplate(SHAPES.saas, [...enable(postgresServices), ...args]);
    expect(result.code).not.toBe(0);
    expect(result.stderr).toContain("postgres.bundled.parameters.max_connections is 240, but");
    expect(result.stderr).toMatch(new RegExp(`(identity|assets) ${perReplicaAfter} x ${pods}`));
    // The same values fit once max_connections is raised by what they add.
    const raised = helmTemplate(SHAPES.saas, [...enable(postgresServices), ...args, "--set", "postgres.bundled.parameters.max_connections=1000"]);
    expect(raised.stderr).toBe("");
    expect(raised.code).toBe(0);
  });

  test("the Deployments roll with the surge the check counts", () => {
    const manifests = render(SHAPES.saas, enable(postgresServices));
    for (const name of postgresServices) {
      const strategy = (find(manifests, "Deployment", `${RELEASE}-${name}`).spec as { strategy: { rollingUpdate: { maxSurge: number; maxUnavailable: number } } }).strategy;
      expect(strategy.rollingUpdate).toEqual({ maxSurge: 1, maxUnavailable: 0 });
    }
  });

  test("each service gets its pool size from database.maxConnections, within the range it accepts, and custom env cannot change it", () => {
    const env = (args: string[], name: string) =>
      (podSpec(find(render(SHAPES.saas, [...enable(postgresServices), ...args]), "Deployment", `${RELEASE}-${name}`)).containers[0]?.env ?? []).filter(
        (e) => e.name === "TRUSTIOT__DATABASE__MAX_CONNECTIONS",
      );
    expect(env([], "certs")).toEqual([{ name: "TRUSTIOT__DATABASE__MAX_CONNECTIONS", value: "5" }]);
    expect(env(["--set", "services.certs.database.maxConnections=2"], "certs")).toEqual([{ name: "TRUSTIOT__DATABASE__MAX_CONNECTIONS", value: "2" }]);
    // identity keeps 3 connections open, so it refuses fewer (services/identity/src/config.rs);
    // the schema's floor is 2 for every service, so the template refuses it, with and without
    // schema validation.
    for (const schema of [[], ["--skip-schema-validation"]]) {
      const result = helmTemplate(SHAPES.saas, [...enable(postgresServices), "--set", "services.identity.database.maxConnections=2", ...schema]);
      expect(result.code).not.toBe(0);
      expect(result.stderr).toContain("services.identity.database.maxConnections must be a whole number from 3 to 100");
    }
    const unchecked = helmTemplate(SHAPES.saas, [...enable(postgresServices), "--set", "services.assets.database.maxConnections=101", "--skip-schema-validation"]);
    expect(unchecked.stderr).toContain("services.assets.database.maxConnections must be a whole number from 2 to 100");
    const missing = helmTemplate(SHAPES.saas, [...enable(postgresServices), "--set", "services.assets.database=null"]);
    expect(missing.code).not.toBe(0);
    expect(missing.stderr).toContain("services.assets.database.maxConnections is required");
    const custom = helmTemplate(SHAPES.saas, [
      ...enable(postgresServices),
      "--set-json",
      'services.assets.env=[{"name":"trustiot__database__max_connections","value":"90"}]',
    ]);
    expect(custom.code).not.toBe(0);
    expect(custom.stderr).toContain("services.assets: env trustiot__database__max_connections is set by the chart from services.assets.database.maxConnections");
    // The gateway's settings carry its own prefix, and the name is the chart's there too.
    const gatewayEnv = (args: string[]) =>
      (podSpec(find(render(SHAPES.saas, [...enable(["ai-gateway"]), ...args]), "Deployment", `${RELEASE}-ai-gateway`)).containers[0]?.env ?? []).filter(
        (e) => /DATABASE__MAX_CONNECTIONS$/.test(e.name),
      );
    expect(gatewayEnv([])).toEqual([{ name: "AI_GATEWAY__DATABASE__MAX_CONNECTIONS", value: "4" }]);
    expect(gatewayEnv(["--set", "services.ai-gateway.database.maxConnections=32"])).toEqual([{ name: "AI_GATEWAY__DATABASE__MAX_CONNECTIONS", value: "32" }]);
    const gatewayAbove = helmTemplate(SHAPES.saas, [...enable(["ai-gateway"]), "--set", "services.ai-gateway.database.maxConnections=33", "--skip-schema-validation"]);
    expect(gatewayAbove.code).not.toBe(0);
    expect(gatewayAbove.stderr).toContain("services.ai-gateway.database.maxConnections must be a whole number from 2 to 32");
    const gatewayCustom = helmTemplate(SHAPES.saas, [
      ...enable(postgresServices),
      "--set-json",
      'services.ai-gateway.env=[{"name":"ai_gateway__database__max_connections","value":"30"}]',
    ]);
    expect(gatewayCustom.code).not.toBe(0);
    expect(gatewayCustom.stderr).toContain("services.ai-gateway: env ai_gateway__database__max_connections is set by the chart from services.ai-gateway.database.maxConnections");
    // Nor can serviceDefaults set a pool size for every service (values.schema.json).
    const everyService = helmTemplate(SHAPES.saas, ["--set", "serviceDefaults.database.maxConnections=10"]);
    expect(everyService.code).not.toBe(0);
    expect(everyService.stderr).toMatch(/serviceDefaults/);
  });

  test("external mode checks no total, since the customer sizes that server, but still the range of each pool", () => {
    const external = [
      "--set",
      "postgres.mode=external",
      "--set",
      "postgres.external.host=pg.saas.trustiot.test",
      "--set",
      "postgres.external.caSecret.name=trustiot-pg-ca",
      ...EXTERNAL_MIGRATION,
      ...enable(postgresServices),
    ];
    const many = helmTemplate(SHAPES.saas, [...external, "--set", "services.identity.replicas=10", "--set", "postgres.bundled.parameters.max_connections=10"]);
    expect(many.stderr).toBe("");
    expect(many.code).toBe(0);
    const tooSmall = helmTemplate(SHAPES.saas, [...external, "--set", "services.identity.database.maxConnections=2"]);
    expect(tooSmall.code).not.toBe(0);
    expect(tooSmall.stderr).toContain("services.identity.database.maxConnections must be a whole number from 3 to 100");
  });
});

describe("migration Job (Laysi/trustiot-v2#60)", () => {
  type JobSpec = YamlMap & {
    parallelism: number;
    completions: number;
    podReplacementPolicy: string;
    backoffLimit: number;
    activeDeadlineSeconds: number;
    template: { metadata: { labels: Record<string, string>; annotations: Record<string, string> }; spec: PodSpec & { restartPolicy: string } };
  };
  type Env = NonNullable<Container["env"]>;
  const IDENTITY_SECRET = `${RELEASE}-identity-password`;
  const COLLECTOR_SECRET = `${RELEASE}-otel-collector-password`;
  const ADMIN_SECRET = `${RELEASE}-clickhouse-admin-password`;
  const SIGNING_KEYS_SECRET = `${RELEASE}-identity-signing-keys`;
  const jobsOf = (manifests: Manifest[]) => manifests.filter((m) => m.kind === "Job");
  const jobOf = (manifests: Manifest[]) => {
    const jobs = jobsOf(manifests);
    expect(jobs).toHaveLength(1);
    return jobs[0] as Manifest;
  };
  const jobSpec = (manifests: Manifest[]) => jobOf(manifests).spec as JobSpec;
  const jobEnv = (manifests: Manifest[]): Env => jobSpec(manifests).template.spec.containers[0]?.env ?? [];
  const initContainers = (deployment: Manifest) => (podSpec(deployment).initContainers ?? []) as Container[];
  const envValue = (env: Env, name: string) => env.find((e) => e.name === name);
  const withIdentity = ["--set", "services.identity.enabled=true", "--set", "services.mqtt-bridge.enabled=true"];
  const manifests = render(SHAPES.saas, withIdentity);

  test("one Job, named after a hash of its spec, runs the migrate image once, one pod at a time, with a token only its container mounts", () => {
    const job = jobOf(manifests);
    expect(job.metadata.name).toMatch(new RegExp(`^${RELEASE}-migrate-[0-9a-f]{8}$`));
    const spec = jobSpec(manifests);
    expect({ parallelism: spec.parallelism, completions: spec.completions, podReplacementPolicy: spec.podReplacementPolicy }).toEqual({
      parallelism: 1,
      completions: 1,
      podReplacementPolicy: "Failed",
    });
    expect(spec.backoffLimit).toBe(3);
    expect(spec.activeDeadlineSeconds).toBe(1800);
    // No ttlSecondsAfterFinished: services that start later still find the Job complete.
    expect(spec.ttlSecondsAfterFinished).toBeUndefined();
    const pod = spec.template.spec;
    expect(pod.restartPolicy).toBe("Never");
    expect(pod.serviceAccountName).toBe(`${RELEASE}-migrate`);
    expect(pod.automountServiceAccountToken).toBe(false);
    expect(find(manifests, "ServiceAccount", `${RELEASE}-migrate`).automountServiceAccountToken).toBe(false);
    expect(pod.containers).toHaveLength(1);
    expect((pod as { initContainers?: unknown }).initContainers).toBeUndefined();
    const container = pod.containers[0] as Container & { args: string[] };
    expect(container.image).toBe(`ghcr.io/laysi/trustiot-v2/migrate:${chart.appVersion}`);
    expect(container.args).toEqual(["run"]);
    expect(container.envFrom).toEqual([{ configMapRef: { name: `${RELEASE}-platform` } }]);
    // In the mesh, so the bundled ClickHouse admits it by its ServiceAccount.
    expect(spec.template.metadata.labels["istio.io/dataplane-mode"]).toBe("ambient");
    expect(spec.template.metadata.labels["app.kubernetes.io/component"]).toBe("migrate");
    // Not a Helm hook: the Job is an ordinary object of the release (README.md, "migration Job").
    expect(job.metadata.annotations?.["helm.sh/hook"]).toBeUndefined();
  });

  test("the Job logs in with the owner's generated Secret and the ClickHouse admin, and is told which password Secrets to fill, by name only", () => {
    const env = jobEnv(manifests);
    const secretRefs = env.filter((e) => e.valueFrom?.secretKeyRef).map((e) => `${e.name}=${e.valueFrom?.secretKeyRef?.name}/${e.valueFrom?.secretKeyRef?.key}`);
    // The Job reads no password it fills from an env Secret reference: it reads and writes them
    // through the API, so they never sit in its environment before it decides on them. The
    // bundled ClickHouse's generated admin password too: the Job reads it through the API when it
    // is about to use ClickHouse, and again while ClickHouse refuses it (Laysi/trustiot-v2#403).
    expect(secretRefs).toEqual([
      `TRUSTIOT__POSTGRES__USERNAME=${RELEASE}-postgres-app/username`,
      `TRUSTIOT__POSTGRES__PASSWORD=${RELEASE}-postgres-app/password`,
    ]);
    expect(envValue(env, "TRUSTIOT__CLICKHOUSE__USERNAME")?.value).toBe("trustiot_admin");
    expect(envValue(env, "TRUSTIOT__CLICKHOUSE__PASSWORD_SECRET")?.value).toBe(`${RELEASE}-clickhouse-admin-password`);
    expect(envValue(env, "TRUSTIOT__CLICKHOUSE__PASSWORD")).toBeUndefined();
    expect(envValue(env, "TRUSTIOT_POSTGRES_CA_FILE")?.value).toBe("/etc/trustiot/postgres-ca/ca.crt");
    expect(env.filter((e) => e.name.startsWith("TRUSTIOT__PASSWORDS__"))).toEqual([
      { name: "TRUSTIOT__PASSWORDS__API_URL", value: "https://kubernetes.default.svc" },
      { name: "TRUSTIOT__PASSWORDS__NAMESPACE", valueFrom: { fieldRef: { fieldPath: "metadata.namespace" } } },
      { name: "TRUSTIOT__PASSWORDS__TOKEN_FILE", value: "/var/run/secrets/trustiot/kube/token" },
      { name: "TRUSTIOT__PASSWORDS__CA_FILE", value: "/var/run/secrets/trustiot/kube/ca.crt" },
      { name: "TRUSTIOT__PASSWORDS__GENERATION", value: "1" },
      { name: "TRUSTIOT__PASSWORDS__CLICKHOUSE_COLLECTOR", value: `trustiot_collector=${COLLECTOR_SECRET}` },
      // Only identity uses Postgres here (mqtt-bridge does not).
      { name: "TRUSTIOT__PASSWORDS__POSTGRES", value: `identity_app=${IDENTITY_SECRET},identity_platform=${IDENTITY_SECRET}` },
    ]);
    expect(envValue(env, "TRUSTIOT__WAIT__DATABASES")?.value).toBe("10m");
    expect(envValue(env, "TRUSTIOT__WAIT__LOCK")?.value).toBe("10m");
    const pod = jobSpec(manifests).template.spec;
    expect(pod.volumes).toEqual([
      { name: "postgres-ca", secret: { secretName: `${RELEASE}-postgres-ca`, items: [{ key: "ca.crt", path: "ca.crt" }] } },
      {
        name: "migrate-kube-api",
        projected: {
          defaultMode: 0o444,
          sources: [
            { serviceAccountToken: { path: "token", expirationSeconds: 3600 } },
            { configMap: { name: "kube-root-ca.crt", items: [{ key: "ca.crt", path: "ca.crt" }] } },
          ],
        },
      },
    ]);
    expect(pod.containers[0]?.volumeMounts).toEqual([
      { name: "postgres-ca", mountPath: "/etc/trustiot/postgres-ca", readOnly: true },
      { name: "migrate-kube-api", mountPath: "/var/run/secrets/trustiot/kube", readOnly: true },
    ]);
    // Nothing to fill (no service uses Postgres, no collector) but the generated ClickHouse admin
    // password to read: the token and the API settings, and a Role that only reads that Secret.
    const adminOnly = render(SHAPES.saas, ["--set", "observability.collector.enabled=false"]);
    const adminEnv = jobEnv(adminOnly);
    expect(adminEnv.filter((e) => e.name.startsWith("TRUSTIOT__PASSWORDS__")).map((e) => e.name)).toEqual([
      "TRUSTIOT__PASSWORDS__API_URL",
      "TRUSTIOT__PASSWORDS__NAMESPACE",
      "TRUSTIOT__PASSWORDS__TOKEN_FILE",
      "TRUSTIOT__PASSWORDS__CA_FILE",
      "TRUSTIOT__PASSWORDS__GENERATION",
    ]);
    expect(JSON.stringify(jobSpec(adminOnly).template.spec)).toContain("migrate-kube-api");
    const adminRole = find(adminOnly, "Role", `${RELEASE}-migrate`) as Manifest & { rules: unknown[] };
    expect(adminRole.rules).toEqual([{ apiGroups: [""], resources: ["secrets"], resourceNames: [`${RELEASE}-clickhouse-admin-password`], verbs: ["get"] }]);
    // Nothing at all to reach through the API (an existing admin Secret): no token at all.
    const nothing = render(SHAPES.saas, ["--set", "observability.collector.enabled=false", "--set", "clickhouse.bundled.adminSecret.name=trustiot-clickhouse-admin"]);
    const bare = jobSpec(nothing).template.spec;
    expect(JSON.stringify(bare)).not.toContain("migrate-kube-api");
    expect(JSON.stringify(bare)).not.toContain("TRUSTIOT__PASSWORDS__");
    expect(nothing.filter((m) => m.kind === "Secret" || m.kind === "Role")).toEqual([]);
    // There the Job reads the existing Secret from its environment, as before Laysi/trustiot-v2#403.
    expect(envValue(jobEnv(nothing), "TRUSTIOT__CLICKHOUSE__PASSWORD")?.valueFrom).toEqual({
      secretKeyRef: { name: "trustiot-clickhouse-admin", key: "password" },
    });
    expect(envValue(jobEnv(nothing), "TRUSTIOT__CLICKHOUSE__PASSWORD_SECRET")).toBeUndefined();
  });

  test("the password Secrets: one per consumer, without data, kept by Helm, labelled as filled by tools/migrate", () => {
    const secrets = manifests.filter((m) => m.kind === "Secret");
    expect(secrets.map((m) => m.metadata.name)).toEqual([IDENTITY_SECRET, COLLECTOR_SECRET, ADMIN_SECRET, SIGNING_KEYS_SECRET]);
    // Exactly this shape, every key and value: scripts/ci/check-secret-handling.ts lets the
    // template through only because nothing in it is a value (Laysi/trustiot-v2#60 round 4, R1).
    // In particular the chart never renders the annotations tools/migrate writes (the generation
    // and the Secret's uid). The last two are the bundled ClickHouse's admin password, which its
    // pod's init container fills (Laysi/trustiot-v2#403), and the identity service's signing
    // keys, which the identity pod's init container fills (Laysi/trustiot-v2#404).
    const components = ["identity", "otel-collector", "clickhouse", "identity"];
    const names = [IDENTITY_SECRET, COLLECTOR_SECRET, ADMIN_SECRET, SIGNING_KEYS_SECRET];
    for (const [i, secret] of secrets.entries()) {
      const component = components[i] ?? "";
      expect(secret).toEqual({
        apiVersion: "v1",
        kind: "Secret",
        metadata: {
          name: names[i] ?? "",
          labels: {
            "helm.sh/chart": `trustiot-${chart.version}`,
            "app.kubernetes.io/name": "trustiot",
            "app.kubernetes.io/instance": RELEASE,
            "app.kubernetes.io/version": chart.appVersion,
            "app.kubernetes.io/managed-by": "Helm",
            "app.kubernetes.io/part-of": "trustiot",
            "app.kubernetes.io/component": component,
            "trustiot.io/filled-by": "migrate",
          },
          annotations: {
            "helm.sh/resource-policy": "keep",
            "trustiot.io/description": "Generated in the cluster by tools/migrate; nobody sets it (docs/architecture/secrets.md).",
          },
        },
        type: "Opaque",
      });
    }
    // Each consumer's own component, so a node's kubelet reads only the passwords of its pods.
    // The public keys ConfigMap has the same metadata, without type or data.
    expect(find(manifests, "ConfigMap", `${RELEASE}-internal-identity-keys`)).toEqual({
      apiVersion: "v1",
      kind: "ConfigMap",
      metadata: {
        name: `${RELEASE}-internal-identity-keys`,
        labels: (secrets[3] as Manifest).metadata.labels,
        annotations: {
          "helm.sh/resource-policy": "keep",
          "trustiot.io/description":
            "Public keys of the internal identity service, published in the cluster by tools/migrate; nobody sets them (docs/architecture/secrets.md).",
        },
      },
    });
    expect(secrets.map((m) => m.metadata.labels?.["app.kubernetes.io/component"])).toEqual(components);
    // Declared with the Job turned off too: whatever replaces the Job fills them, and the
    // consumers read them the same way. ClickHouse's init container fills its own regardless.
    const off = render(SHAPES.saas, [...withIdentity, "--set", "migrate.enabled=false"]);
    expect(off.filter((m) => m.kind === "Secret").map((m) => m.metadata.name)).toEqual(names);
  });

  test("every label and annotation the chart and Helm put on the password Secrets is one the Job keeps when it binds them", () => {
    // tools/migrate/src/passwords.rs strips every other key in the patch that binds a Secret
    // (Laysi/trustiot-v2#60 round 5, N3); a chart key missing there would be removed on the first
    // fill and put back by every upgrade (round 6, L1).
    const source = readFileSync(join(REPO_ROOT, "tools/migrate/src/passwords.rs"), "utf8");
    const constants = new Map([...source.matchAll(/pub const ([A-Z_]+): &str = "([^"]+)";/g)].map((m) => [m[1]!, m[2]!]));
    // The broker's certificate CronJob binds its Secret the same way and records its own
    // generation (Laysi/trustiot-v2#535); passwords.rs names that constant by its path.
    const certificate = readFileSync(join(REPO_ROOT, "tools/migrate/src/mqtt_certificate.rs"), "utf8");
    for (const m of certificate.matchAll(/pub const ([A-Z_]+): &str = "([^"]+)";/g)) constants.set(`crate::mqtt_certificate::${m[1]!}`, m[2]!);
    const allowlist = (name: string): Set<string> => {
      const body = new RegExp(`const ${name}: &\\[&str\\] = &\\[([^\\]]*)\\];`).exec(source)?.[1];
      if (body === undefined) throw new Error(`${name} not found in passwords.rs`);
      const items = body
        .split(",")
        .map((item) => item.trim())
        .filter((item) => item !== "");
      return new Set(
        items.map((item) => {
          const literal = /^"([^"]+)"$/.exec(item)?.[1];
          const value = literal ?? constants.get(item);
          if (value === undefined) throw new Error(`${name}: cannot read ${item}`);
          return value;
        }),
      );
    };
    const labels = allowlist("KEPT_LABELS");
    const annotations = allowlist("KEPT_ANNOTATIONS");
    // Helm adds these to every object it installs (ownership, which an adoption checks).
    for (const key of ["meta.helm.sh/release-name", "meta.helm.sh/release-namespace"]) expect(annotations).toContain(key);
    const configurations = [
      manifests,
      render(SHAPES.onprem, withIdentity),
      render(SHAPES.saas, [...enableAllServices, "--set-json", 'mesh.calls.telemetry-query=["postgres"]']),
      render(SHAPES.saas, [...withIdentity, "--set", "migrate.enabled=false"]),
    ];
    let checked = 0;
    for (const rendered of configurations) {
      // The public keys ConfigMap too: the identity pod's init container binds it the same way.
      for (const secret of rendered.filter((m) => (m.kind === "Secret" || m.kind === "ConfigMap") && m.metadata.labels?.["trustiot.io/filled-by"] === "migrate")) {
        for (const key of Object.keys(secret.metadata.labels ?? {})) expect(labels, `${secret.metadata.name} label ${key}`).toContain(key);
        for (const key of Object.keys(secret.metadata.annotations ?? {})) {
          expect(annotations, `${secret.metadata.name} annotation ${key}`).toContain(key);
        }
        checked++;
      }
    }
    // Both kinds: a service's and the collector's, in every configuration.
    expect(checked).toBeGreaterThanOrEqual(8);
  });

  test("the Job's ServiceAccount may get and patch exactly its password Secrets and get the ClickHouse admin Secret, and no other Role but ClickHouse's touches Secrets", () => {
    const everything = render(SHAPES.saas, [...enableAllServices, "--set-json", 'mesh.calls.telemetry-query=["postgres"]', "--set", "mqttBroker.enabled=true"]);
    expect(everything.some((m) => m.kind === "Role" && m.metadata.name === `${RELEASE}-mqtt-broker`)).toBe(true);
    expect(everything.some((m) => m.kind === "Role" && m.metadata.name === `${RELEASE}-mqtt-broker-certificate`)).toBe(true);
    for (const rendered of [manifests, everything]) {
      const fills = jobPasswordSecretNames(rendered);
      const role = find(rendered, "Role", `${RELEASE}-migrate`) as Manifest & {
        rules: { apiGroups: string[]; resources: string[]; resourceNames?: string[]; verbs: string[] }[];
      };
      expect(role.rules.map((rule) => ({ ...rule, resourceNames: [...(rule.resourceNames ?? [])].sort() }))).toEqual([
        { apiGroups: [""], resources: ["secrets"], resourceNames: fills, verbs: ["get", "patch"] },
        { apiGroups: [""], resources: ["secrets"], resourceNames: [ADMIN_SECRET], verbs: ["get"] },
      ]);
      const binding = find(rendered, "RoleBinding", `${RELEASE}-migrate`) as Manifest & { roleRef: unknown; subjects: unknown[] };
      expect(binding.roleRef).toEqual({ apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: `${RELEASE}-migrate` });
      expect(binding.subjects).toEqual([{ kind: "ServiceAccount", name: `${RELEASE}-migrate`, namespace: NAMESPACE }]);
      // Every Role of the chart: only the Job's, ClickHouse's (which names only the admin
      // Secret, "bundled ClickHouse" above), the identity service's (which names only its
      // signing keys, "internal identity" above), the certs service's (which names only its
      // issuing CA keys, "gateway certificate authority" below), the MQTT broker's (which names
      // only its accounts Secret, "MQTT broker" below) and the broker's certificate CronJob's
      // (which names only the server certificate Secret, Laysi/trustiot-v2#535) may name secrets
      // (or a wildcard), and no Role anywhere may create, list, watch, update or delete them.
      const allowed = [
        `${RELEASE}-migrate`,
        `${RELEASE}-clickhouse`,
        `${RELEASE}-identity-signing-keys`,
        `${RELEASE}-certs-ca-keys`,
        `${RELEASE}-mqtt-broker`,
        `${RELEASE}-mqtt-broker-certificate`,
      ];
      for (const other of rendered.filter((m) => m.kind === "Role" || m.kind === "ClusterRole")) {
        const rules = (other as Manifest & { rules: { apiGroups?: string[]; resources?: string[]; verbs?: string[] }[] }).rules;
        const touches = rules.some((rule) => (rule.resources ?? []).some((r) => r === "secrets" || r === "*") && (rule.apiGroups ?? []).some((g) => g === "" || g === "*"));
        expect({ role: other.metadata.name, touches }).toEqual({ role: other.metadata.name, touches: allowed.includes(other.metadata.name) && other.kind === "Role" });
        for (const rule of rules.filter((r) => (r.resources ?? []).includes("secrets"))) {
          expect((rule.verbs ?? []).filter((verb) => verb !== "get" && verb !== "patch"), other.metadata.name).toEqual([]);
        }
      }
    }
    expect(passwordSecretNames(everything)).toContain(`${RELEASE}-telemetry-query-password`);
  });

  test("telemetry-query's ClickHouse account: the Job generates its password into the service's Secret, and only the service reads it", () => {
    const secret = `${RELEASE}-telemetry-query-password`;
    // Disabled (the default): no binding, so the Job keeps the account from logging in.
    expect(jobEnv(manifests).map((e) => e.name)).not.toContain("TRUSTIOT__PASSWORDS__CLICKHOUSE_TELEMETRY_QUERY");
    for (const shape of SHAPE_NAMES) {
      const rendered = render(SHAPES[shape], [...withIdentity, "--set", "services.telemetry-query.enabled=true"]);
      const env = jobEnv(rendered);
      expect(env.find((e) => e.name === "TRUSTIOT__PASSWORDS__CLICKHOUSE_TELEMETRY_QUERY"), shape).toEqual({
        name: "TRUSTIOT__PASSWORDS__CLICKHOUSE_TELEMETRY_QUERY",
        value: `trustiot_telemetry_query=${secret}`,
      });
      expect(env.find((e) => e.name === "TRUSTIOT__PASSWORDS__CLICKHOUSE_COLLECTOR")?.value, shape).not.toContain(secret);
      expect(env.find((e) => e.name === "TRUSTIOT__PASSWORDS__POSTGRES")?.value, shape).toContain(
        `telemetry_query_app=${secret},telemetry_query_platform=${secret}`,
      );
      const tq = podSpec(find(rendered, "Deployment", `${RELEASE}-telemetry-query`)).containers[0] as Container;
      expect(tq.env?.filter((e) => e.name.startsWith("TRUSTIOT__CLICKHOUSE__")), shape).toEqual([
        { name: "TRUSTIOT__CLICKHOUSE__USERNAME", value: "trustiot_telemetry_query" },
        { name: "TRUSTIOT__CLICKHOUSE__PASSWORD", valueFrom: { secretKeyRef: { name: secret, key: "trustiot_telemetry_query" } } },
      ]);
      // No other workload reads the account's password.
      for (const other of rendered.filter((m) => m.kind === "Deployment" && m.metadata.name !== `${RELEASE}-telemetry-query`)) {
        expect(JSON.stringify(other), other.metadata.name).not.toContain("trustiot_telemetry_query");
      }
    }
    // Custom env cannot replace the account or read the Secret from another service.
    for (const name of ["TRUSTIOT__CLICKHOUSE__USERNAME", "TRUSTIOT__CLICKHOUSE__PASSWORD"]) {
      const result = helmTemplate(SHAPES.saas, [
        ...withIdentity, "--set", "services.telemetry-query.enabled=true",
        "--set-json", `services.telemetry-query.env=[{"name":"${name}","valueFrom":{"secretKeyRef":{"name":"elsewhere","key":"k"}}}]`,
      ]);
      expect(result.code, name).not.toBe(0);
      expect(result.stderr, name).toContain(`env ${name} is set by the chart`);
    }
    const stolen = helmTemplate(SHAPES.saas, [
      ...withIdentity, "--set", "services.telemetry-query.enabled=true",
      "--set-json", `services.mqtt-bridge.env=[{"name":"STOLEN","valueFrom":{"secretKeyRef":{"name":"${secret}","key":"trustiot_telemetry_query"}}}]`,
    ]);
    expect(stolen.code).not.toBe(0);
    expect(stolen.stderr).toContain(`env reads Secret ${secret}`);
  });

  test("ingest's ClickHouse account and Postgres roles: the Job generates their passwords into the service's one Secret, and only the service reads them (Laysi/trustiot-v2#642)", () => {
    const secret = `${RELEASE}-ingest-password`;
    const account = "trustiot_ingest_writer";
    // Disabled (the default): no binding, so the Job keeps an existing account from logging in.
    expect(jobEnv(manifests).map((e) => e.name)).not.toContain("TRUSTIOT__PASSWORDS__CLICKHOUSE_INGEST");
    for (const shape of SHAPE_NAMES) {
      const rendered = render(SHAPES[shape], ["--set", "services.ingest.enabled=true", "--set", "mqttBroker.enabled=true"]);
      const env = jobEnv(rendered);
      expect(env.find((e) => e.name === "TRUSTIOT__PASSWORDS__CLICKHOUSE_INGEST"), shape).toEqual({
        name: "TRUSTIOT__PASSWORDS__CLICKHOUSE_INGEST",
        value: `${account}=${secret}`,
      });
      expect(env.find((e) => e.name === "TRUSTIOT__PASSWORDS__CLICKHOUSE_COLLECTOR")?.value, shape).not.toContain(secret);
      expect(env.find((e) => e.name === "TRUSTIOT__PASSWORDS__POSTGRES")?.value, shape).toContain(`ingest_app=${secret},ingest_platform=${secret}`);
      // The one Secret, declared empty for the Job to fill, which the Job's Role names.
      const declared = rendered.filter((m) => m.kind === "Secret" && m.metadata.name === secret);
      expect(declared.length, shape).toBe(1);
      expect((declared[0] as YamlMap).data, shape).toBeUndefined();
      const ingest = podSpec(find(rendered, "Deployment", `${RELEASE}-ingest`)).containers[0] as Container;
      expect(ingest.env?.filter((e) => /^TRUSTIOT__(CLICKHOUSE|DATABASE)__/.test(e.name)), shape).toEqual([
        { name: "TRUSTIOT__DATABASE__USERNAME", value: "ingest_app" },
        { name: "TRUSTIOT__DATABASE__PASSWORD", valueFrom: { secretKeyRef: { name: secret, key: "ingest_app" } } },
        { name: "TRUSTIOT__DATABASE__MAX_CONNECTIONS", value: "10" },
        { name: "TRUSTIOT__DATABASE__PLATFORM_USERNAME", value: "ingest_platform" },
        { name: "TRUSTIOT__DATABASE__PLATFORM_PASSWORD", valueFrom: { secretKeyRef: { name: secret, key: "ingest_platform" } } },
        { name: "TRUSTIOT__CLICKHOUSE__USERNAME", value: account },
        { name: "TRUSTIOT__CLICKHOUSE__PASSWORD", valueFrom: { secretKeyRef: { name: secret, key: account } } },
      ]);
      // No other workload reads the account's password or the Secret.
      for (const other of rendered.filter((m) => ["Deployment", "StatefulSet", "DaemonSet", "CronJob"].includes(m.kind) && m.metadata.name !== `${RELEASE}-ingest`)) {
        expect(JSON.stringify(other), other.metadata.name).not.toContain(account);
        expect(JSON.stringify(other), other.metadata.name).not.toContain(secret);
      }
    }
    // Custom env cannot replace the account or read the Secret from another service.
    for (const name of ["TRUSTIOT__CLICKHOUSE__USERNAME", "TRUSTIOT__CLICKHOUSE__PASSWORD"]) {
      const result = helmTemplate(SHAPES.saas, [
        "--set", "services.ingest.enabled=true", "--set", "mqttBroker.enabled=true",
        "--set-json", `services.ingest.env=[{"name":"${name}","valueFrom":{"secretKeyRef":{"name":"elsewhere","key":"k"}}}]`,
      ]);
      expect(result.code, name).not.toBe(0);
      expect(result.stderr, name).toContain(`env ${name} is set by the chart`);
    }
    const stolen = helmTemplate(SHAPES.saas, [
      ...withIdentity, "--set", "services.ingest.enabled=true", "--set", "mqttBroker.enabled=true",
      "--set-json", `services.mqtt-bridge.env=[{"name":"STOLEN","valueFrom":{"secretKeyRef":{"name":"${secret}","key":"${account}"}}}]`,
    ]);
    expect(stolen.code).not.toBe(0);
    expect(stolen.stderr).toContain(`env reads Secret ${secret}`);
  });

  test("gateway-ops' Postgres roles: the Job generates their passwords into the service's one Secret, and only the service reads them (Laysi/trustiot-v2#721)", () => {
    const secret = `${RELEASE}-gateway-ops-password`;
    // Disabled (the default): no binding, so the Job keeps the roles from logging in.
    expect(jobEnv(manifests).find((e) => e.name === "TRUSTIOT__PASSWORDS__POSTGRES")?.value ?? "").not.toContain("gateway_ops");
    expect(manifests.filter((m) => m.kind === "Secret" && m.metadata.name === secret)).toEqual([]);
    for (const shape of SHAPE_NAMES) {
      const rendered = render(SHAPES[shape], ["--set", "services.gateway-ops.enabled=true", "--set", "mqttBroker.enabled=true"]);
      const env = jobEnv(rendered);
      expect(env.find((e) => e.name === "TRUSTIOT__PASSWORDS__POSTGRES")?.value, shape).toContain(`gateway_ops_app=${secret},gateway_ops_platform=${secret}`);
      // No ClickHouse account: gateway-ops does not use ClickHouse.
      expect(env.filter((e) => /^TRUSTIOT__PASSWORDS__CLICKHOUSE_/.test(e.name) && String(e.value).includes(secret)), shape).toEqual([]);
      // The one Secret, declared empty for the Job to fill.
      const declared = rendered.filter((m) => m.kind === "Secret" && m.metadata.name === secret);
      expect(declared.length, shape).toBe(1);
      expect((declared[0] as YamlMap).data, shape).toBeUndefined();
      const gatewayOps = podSpec(find(rendered, "Deployment", `${RELEASE}-gateway-ops`));
      const container = gatewayOps.containers[0] as Container;
      expect(container.env?.filter((e) => /^TRUSTIOT__(CLICKHOUSE|DATABASE)__/.test(e.name)), shape).toEqual([
        { name: "TRUSTIOT__DATABASE__USERNAME", value: "gateway_ops_app" },
        { name: "TRUSTIOT__DATABASE__PASSWORD", valueFrom: { secretKeyRef: { name: secret, key: "gateway_ops_app" } } },
        { name: "TRUSTIOT__DATABASE__MAX_CONNECTIONS", value: "9" },
        { name: "TRUSTIOT__DATABASE__PLATFORM_USERNAME", value: "gateway_ops_platform" },
        { name: "TRUSTIOT__DATABASE__PLATFORM_PASSWORD", valueFrom: { secretKeyRef: { name: secret, key: "gateway_ops_platform" } } },
      ]);
      // The Postgres CA and nothing of ClickHouse.
      expect(container.env?.map((e) => e.name), shape).toContain("TRUSTIOT_POSTGRES_CA_FILE");
      expect(JSON.stringify(gatewayOps), shape).not.toContain("clickhouse-password");
      // No other workload reads the Secret.
      for (const other of rendered.filter((m) => ["Deployment", "StatefulSet", "DaemonSet", "CronJob"].includes(m.kind) && m.metadata.name !== `${RELEASE}-gateway-ops`)) {
        expect(JSON.stringify(other), other.metadata.name).not.toContain(secret);
      }
      // The bundled Postgres admits it on 5432.
      const admitted = JSON.stringify(find(rendered, "NetworkPolicy", `${RELEASE}-postgres`).spec);
      expect(admitted, shape).toContain('"app.kubernetes.io/component":"gateway-ops"');
    }
    // Custom env cannot replace a role or read the Secret from another service.
    for (const name of ["TRUSTIOT__DATABASE__USERNAME", "TRUSTIOT__DATABASE__PLATFORM_PASSWORD"]) {
      const result = helmTemplate(SHAPES.saas, [
        "--set", "services.gateway-ops.enabled=true", "--set", "mqttBroker.enabled=true",
        "--set-json", `services.gateway-ops.env=[{"name":"${name}","valueFrom":{"secretKeyRef":{"name":"elsewhere","key":"k"}}}]`,
      ]);
      expect(result.code, name).not.toBe(0);
      expect(result.stderr, name).toContain(`env ${name} is set by the chart`);
    }
    const stolen = helmTemplate(SHAPES.saas, [
      ...withIdentity, "--set", "services.gateway-ops.enabled=true", "--set", "mqttBroker.enabled=true",
      "--set-json", `services.mqtt-bridge.env=[{"name":"STOLEN","valueFrom":{"secretKeyRef":{"name":"${secret}","key":"gateway_ops_platform"}}}]`,
    ]);
    expect(stolen.code).not.toBe(0);
    expect(stolen.stderr).toContain(`env reads Secret ${secret}`);
  });

  test("external Postgres and ClickHouse: the Job uses their migration Secrets and CAs, which no service reads", () => {
    const external = render(SHAPES.onprem, [
      ...withIdentity,
      "--set", "postgres.mode=external",
      "--set", "postgres.external.host=db.factory.test",
      "--set", "postgres.external.caSecret.name=factory-postgres-ca",
      "--set", "postgres.external.migrationSecret.name=factory-postgres-migrate",
      "--set", "clickhouse.mode=external",
      "--set", "clickhouse.external.url=https://ch.factory.test:8443",
      "--set", "clickhouse.external.caSecret.name=factory-clickhouse-ca",
      "--set", "clickhouse.external.migrationSecret.name=factory-clickhouse-migrate",
    ]);
    const env = jobEnv(external);
    const refs = env.filter((e) => e.valueFrom?.secretKeyRef).map((e) => `${e.name}=${e.valueFrom?.secretKeyRef?.name}/${e.valueFrom?.secretKeyRef?.key}`);
    expect(refs).toEqual([
      "TRUSTIOT__POSTGRES__USERNAME=factory-postgres-migrate/username",
      "TRUSTIOT__POSTGRES__PASSWORD=factory-postgres-migrate/password",
      "TRUSTIOT__CLICKHOUSE__USERNAME=factory-clickhouse-migrate/username",
      "TRUSTIOT__CLICKHOUSE__PASSWORD=factory-clickhouse-migrate/password",
    ]);
    const volumes = jobSpec(external).template.spec.volumes ?? [];
    expect(volumes.map((v) => JSON.stringify(v))).toContain(
      JSON.stringify({ name: "clickhouse-ca", secret: { secretName: "factory-clickhouse-ca", items: [{ key: "ca.crt", path: "ca.crt" }] } }),
    );
    for (const deployment of external.filter((m) => m.kind === "Deployment")) {
      expect(JSON.stringify(deployment)).not.toContain("-migrate/");
      expect(JSON.stringify(deployment)).not.toContain("factory-postgres-migrate");
      expect(JSON.stringify(deployment)).not.toContain("factory-clickhouse-migrate");
    }
    // Custom env cannot read either migration Secret, in any key.
    for (const [secret, key] of [
      ["factory-postgres-migrate", "password"],
      ["factory-clickhouse-migrate", "notes"],
    ]) {
      const result = helmTemplate(SHAPES.onprem, [
        ...withIdentity,
        "--set", "postgres.mode=external",
        "--set", "postgres.external.host=db.factory.test",
        "--set", "postgres.external.caSecret.name=factory-postgres-ca",
        "--set", "postgres.external.migrationSecret.name=factory-postgres-migrate",
        "--set", "clickhouse.mode=external",
        "--set", "clickhouse.external.url=https://ch.factory.test:8443",
        "--set", "clickhouse.external.caSecret.name=factory-clickhouse-ca",
        "--set", "clickhouse.external.migrationSecret.name=factory-clickhouse-migrate",
        "--set-json",
        `services.mqtt-bridge.env=[{"name":"STOLEN","valueFrom":{"secretKeyRef":{"name":"${secret}","key":"${key}"}}}]`,
      ]);
      expect(result.code, secret).not.toBe(0);
      expect(result.stderr).toContain(`env reads Secret ${secret}`);
    }
  });

  test("each service that uses Postgres gets its own role and its own password Secret, which nobody else may read", () => {
    const identity = podSpec(find(manifests, "Deployment", `${RELEASE}-identity`)).containers[0] as Container;
    expect(identity.env?.filter((e) => e.name.startsWith("TRUSTIOT__DATABASE__"))).toEqual([
      { name: "TRUSTIOT__DATABASE__USERNAME", value: "identity_app" },
      { name: "TRUSTIOT__DATABASE__PASSWORD", valueFrom: { secretKeyRef: { name: IDENTITY_SECRET, key: "identity_app" } } },
      // Its main pool, from services.identity.database.maxConnections (Laysi/trustiot-v2#575).
      { name: "TRUSTIOT__DATABASE__MAX_CONNECTIONS", value: "10" },
      // identity keeps the event log, which reads events as its platform role (Laysi/trustiot-v2#130).
      { name: "TRUSTIOT__DATABASE__PLATFORM_USERNAME", value: "identity_platform" },
      { name: "TRUSTIOT__DATABASE__PLATFORM_PASSWORD", valueFrom: { secretKeyRef: { name: IDENTITY_SECRET, key: "identity_platform" } } },
    ]);
    const bridge = JSON.stringify(find(manifests, "Deployment", `${RELEASE}-mqtt-bridge`));
    expect(bridge).not.toContain("TRUSTIOT__DATABASE__");
    expect(bridge).not.toContain(IDENTITY_SECRET);
    // A service name with "-" makes a role with "_"; telemetry-query's platform role logs in too
    // (it consumes the identity service's events).
    const telemetry = render(SHAPES.saas, ["--set", "services.telemetry-query.enabled=true"]);
    const tq = podSpec(find(telemetry, "Deployment", `${RELEASE}-telemetry-query`)).containers[0] as Container;
    expect(tq.env?.filter((e) => e.name.startsWith("TRUSTIOT__DATABASE__"))).toEqual([
      { name: "TRUSTIOT__DATABASE__USERNAME", value: "telemetry_query_app" },
      {
        name: "TRUSTIOT__DATABASE__PASSWORD",
        valueFrom: { secretKeyRef: { name: `${RELEASE}-telemetry-query-password`, key: "telemetry_query_app" } },
      },
      { name: "TRUSTIOT__DATABASE__MAX_CONNECTIONS", value: "10" },
      { name: "TRUSTIOT__DATABASE__PLATFORM_USERNAME", value: "telemetry_query_platform" },
      {
        name: "TRUSTIOT__DATABASE__PLATFORM_PASSWORD",
        valueFrom: { secretKeyRef: { name: `${RELEASE}-telemetry-query-password`, key: "telemetry_query_platform" } },
      },
    ]);
    // Custom env cannot read another consumer's password Secret, in any key, nor replace the
    // service's own login.
    for (const [env, expected] of [
      [`[{"name":"STOLEN","valueFrom":{"secretKeyRef":{"name":"${IDENTITY_SECRET}","key":"identity_app"}}}]`, `env reads Secret ${IDENTITY_SECRET}`],
      [`[{"name":"STOLEN","valueFrom":{"secretKeyRef":{"name":"${COLLECTOR_SECRET}","key":"other"}}}]`, `env reads Secret ${COLLECTOR_SECRET}`],
    ] as const) {
      const result = helmTemplate(SHAPES.saas, [...withIdentity, "--set-json", `services.mqtt-bridge.env=${env}`]);
      expect(result.code, env).not.toBe(0);
      expect(result.stderr, env).toContain(expected);
    }
    for (const name of ["TRUSTIOT__DATABASE__USERNAME", "TRUSTIOT__DATABASE__PASSWORD"]) {
      const result = helmTemplate(SHAPES.saas, [
        ...withIdentity,
        "--set-json",
        `services.identity.env=[{"name":"${name}","valueFrom":{"secretKeyRef":{"name":"elsewhere","key":"k"}}}]`,
      ]);
      expect(result.code, name).not.toBe(0);
      expect(result.stderr, name).toContain(`services.identity: env ${name} is set by the chart`);
    }
  });

  test("a service whose platform role logs in (assets, identity) gets both passwords from its one Secret, and the Job fills both keys", () => {
    // Laysi/trustiot-v2#65: an outbox consumer reads its producers' events as its platform role,
    // so the chart binds <schema>_platform too ("trustiot.migrate.platformLogins"), in the same
    // Secret; tools/migrate/tests/registry.rs keeps that list in line with the registry.
    const ASSETS_SECRET = `${RELEASE}-assets-password`;
    const both = render(SHAPES.saas, [...withIdentity, "--set", "services.assets.enabled=true"]);
    expect(envValue(jobEnv(both), "TRUSTIOT__PASSWORDS__POSTGRES")?.value).toBe(
      `assets_app=${ASSETS_SECRET},assets_platform=${ASSETS_SECRET},identity_app=${IDENTITY_SECRET},identity_platform=${IDENTITY_SECRET}`,
    );
    // One Secret per consumer, however many keys it holds, and the Job's Role names each once.
    // (ClickHouse's admin Secret is the init container's, and the Job's Role only reads it.)
    // (So is the identity service's signing keys Secret, which the Job never touches.)
    const secrets = both
      .filter((m) => m.kind === "Secret" && m.metadata.name !== ADMIN_SECRET && m.metadata.name !== SIGNING_KEYS_SECRET)
      .map((m) => m.metadata.name);
    expect(secrets).toEqual([ASSETS_SECRET, IDENTITY_SECRET, COLLECTOR_SECRET]);
    expect(passwordSecretNames(both)).toEqual([...secrets, ADMIN_SECRET, SIGNING_KEYS_SECRET].sort());
    const role = find(both, "Role", `${RELEASE}-migrate`) as Manifest & { rules: { resourceNames: string[] }[] };
    expect(role.rules[0]?.resourceNames).toEqual(secrets);
    const assets = podSpec(find(both, "Deployment", `${RELEASE}-assets`)).containers[0] as Container;
    expect(assets.env?.filter((e) => e.name.startsWith("TRUSTIOT__DATABASE__"))).toEqual([
      { name: "TRUSTIOT__DATABASE__USERNAME", value: "assets_app" },
      { name: "TRUSTIOT__DATABASE__PASSWORD", valueFrom: { secretKeyRef: { name: ASSETS_SECRET, key: "assets_app" } } },
      { name: "TRUSTIOT__DATABASE__MAX_CONNECTIONS", value: "10" },
      { name: "TRUSTIOT__DATABASE__PLATFORM_USERNAME", value: "assets_platform" },
      { name: "TRUSTIOT__DATABASE__PLATFORM_PASSWORD", valueFrom: { secretKeyRef: { name: ASSETS_SECRET, key: "assets_platform" } } },
    ]);
    // identity's platform role logs in too, with its own Secret (Laysi/trustiot-v2#130).
    const identity = podSpec(find(both, "Deployment", `${RELEASE}-identity`)).containers[0] as Container;
    expect(identity.env?.filter((e) => e.name.startsWith("TRUSTIOT__DATABASE__PLATFORM_"))).toEqual([
      { name: "TRUSTIOT__DATABASE__PLATFORM_USERNAME", value: "identity_platform" },
      { name: "TRUSTIOT__DATABASE__PLATFORM_PASSWORD", valueFrom: { secretKeyRef: { name: IDENTITY_SECRET, key: "identity_platform" } } },
    ]);
    expect(JSON.stringify(identity)).not.toContain(ASSETS_SECRET);
    // The service's platform login is the chart's too, and no other service may read its Secret.
    for (const name of ["TRUSTIOT__DATABASE__PLATFORM_USERNAME", "TRUSTIOT__DATABASE__PLATFORM_PASSWORD"]) {
      const result = helmTemplate(SHAPES.saas, [
        ...withIdentity,
        "--set",
        "services.assets.enabled=true",
        "--set-json",
        `services.assets.env=[{"name":"${name}","valueFrom":{"secretKeyRef":{"name":"elsewhere","key":"k"}}}]`,
      ]);
      expect(result.code, name).not.toBe(0);
      expect(result.stderr, name).toContain(`services.assets: env ${name} is set by the chart`);
    }
    const stolen = helmTemplate(SHAPES.saas, [
      ...withIdentity,
      "--set",
      "services.assets.enabled=true",
      "--set-json",
      `services.identity.env=[{"name":"STOLEN","valueFrom":{"secretKeyRef":{"name":"${ASSETS_SECRET}","key":"assets_platform"}}}]`,
    ]);
    expect(stolen.code).not.toBe(0);
    expect(stolen.stderr).toContain(`env reads Secret ${ASSETS_SECRET}`);
  });

  test("every service pod waits in an init container for this Job, with a token only that container mounts", () => {
    const job = jobOf(manifests).metadata.name;
    const deployments = manifests.filter(isServiceDeployment);
    expect(deployments.length).toBe(2);
    for (const deployment of deployments) {
      const pod = podSpec(deployment);
      expect(pod.automountServiceAccountToken).toBe(false);
      // The identity pod first fills its signing keys ("internal identity" above), which needs no
      // database.
      const inits = initContainers(deployment);
      const identity = deployment.metadata.name === `${RELEASE}-identity`;
      expect(inits.map((c) => c.name)).toEqual(identity ? ["signing-keys", "wait-for-migrations"] : ["wait-for-migrations"]);
      const init = inits.at(-1) as Container & { args: string[] };
      expect(init.image).toBe(`ghcr.io/laysi/trustiot-v2/migrate:${chart.appVersion}`);
      expect(init.args).toEqual(["wait-for-job"]);
      expect(init.env).toEqual([
        { name: "TRUSTIOT__KUBE__JOB", value: job },
        { name: "TRUSTIOT__KUBE__NAMESPACE", valueFrom: { fieldRef: { fieldPath: "metadata.namespace" } } },
        { name: "TRUSTIOT__KUBE__API_URL", value: "https://kubernetes.default.svc" },
        { name: "TRUSTIOT__KUBE__TOKEN_FILE", value: "/var/run/secrets/trustiot/kube/token" },
        { name: "TRUSTIOT__KUBE__CA_FILE", value: "/var/run/secrets/trustiot/kube/ca.crt" },
      ]);
      expect(init.securityContext).toEqual(pod.containers[0]?.securityContext);
      expect(init.volumeMounts).toEqual([{ name: "migrate-kube-api", mountPath: "/var/run/secrets/trustiot/kube", readOnly: true }]);
      expect(pod.volumes).toContainEqual({
        name: "migrate-kube-api",
        projected: {
          defaultMode: 0o444,
          sources: [
            { serviceAccountToken: { path: "token", expirationSeconds: 3600 } },
            { configMap: { name: "kube-root-ca.crt", items: [{ key: "ca.crt", path: "ca.crt" }] } },
          ],
        },
      });
      for (const container of pod.containers) {
        expect(JSON.stringify(container.volumeMounts ?? [])).not.toContain("migrate-kube-api");
      }
    }
  });

  test("the ServiceAccounts of the services and the collector may read this one Job and nothing else", () => {
    const job = jobOf(manifests).metadata.name;
    const role = find(manifests, "Role", `${RELEASE}-migrate-status`) as Manifest & { rules: unknown[] };
    expect(role.rules).toEqual([{ apiGroups: ["batch"], resources: ["jobs"], resourceNames: [job], verbs: ["get"] }]);
    const binding = find(manifests, "RoleBinding", `${RELEASE}-migrate-status`) as Manifest & { roleRef: unknown; subjects: unknown[] };
    expect(binding.roleRef).toEqual({ apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: `${RELEASE}-migrate-status` });
    expect(binding.subjects).toEqual(
      ["identity", "mqtt-bridge", "otel-collector"].map((name) => ({ kind: "ServiceAccount", name: `${RELEASE}-${name}`, namespace: NAMESPACE })),
    );
    expect(manifests.filter((m) => m.kind === "ClusterRole" || m.kind === "ClusterRoleBinding")).toEqual([]);
    // Without services and without the collector there is nobody to bind and nothing for the Job
    // to fill: it only reads ClickHouse's admin Secret, and ClickHouse fills that one itself.
    const alone = render(SHAPES.saas, ["--set", "observability.collector.enabled=false"]);
    expect(alone.filter((m) => m.kind === "Role" || m.kind === "RoleBinding").map((m) => `${m.kind}/${m.metadata.name}`)).toEqual([
      `Role/${RELEASE}-clickhouse`,
      `Role/${RELEASE}-migrate`,
      `RoleBinding/${RELEASE}-clickhouse`,
      `RoleBinding/${RELEASE}-migrate`,
    ]);
    expect((find(alone, "Role", `${RELEASE}-migrate`) as Manifest & { rules: unknown[] }).rules).toEqual([
      { apiGroups: [""], resources: ["secrets"], resourceNames: [ADMIN_SECRET], verbs: ["get"] },
    ]);
    // With ClickHouse external there is nothing to read either.
    expect(
      render(SHAPES.saas, [
        "--set",
        "observability.collector.enabled=false",
        "--set",
        "clickhouse.mode=external",
        "--set",
        "clickhouse.external.url=https://clickhouse.example.com:8443",
        "--set",
        "clickhouse.external.caSecret.name=clickhouse-ca",
        "--set",
        "clickhouse.external.migrationSecret.name=clickhouse-migration",
      ]).filter((m) => m.kind === "Role" || m.kind === "RoleBinding"),
    ).toEqual([]);
  });

  test("nobody connects to the Job's pod, and no NetworkPolicy of the chart restricts egress (the init container reaches the API server)", () => {
    const labels = jobSpec(manifests).template.metadata.labels;
    const policy = find(manifests, "AuthorizationPolicy", `${RELEASE}-migrate`).spec as YamlMap & { rules?: unknown };
    expect(policy.action).toBe("ALLOW");
    expect(policy.rules).toBeUndefined();
    const network = find(manifests, "NetworkPolicy", `${RELEASE}-migrate`).spec as { podSelector: { matchLabels: Record<string, string> }; ingress: unknown[] };
    expect(Object.entries(network.podSelector.matchLabels).every(([key, value]) => labels[key] === value)).toBe(true);
    expect(network.ingress).toEqual([]);
    for (const shape of SHAPE_NAMES) {
      for (const m of render([...SHAPES[shape], SMOKE], enableAllServices).filter((o) => o.kind === "NetworkPolicy")) {
        expect({ name: m.metadata.name, types: (m.spec as { policyTypes: string[] }).policyTypes }).toEqual({ name: m.metadata.name, types: ["Ingress"] });
      }
    }
  });

  test("the Job's name changes with its image, settings and platform settings, and only then", () => {
    const name = (args: string[] = []) => jobOf(render(SHAPES.saas, [...withIdentity, ...args])).metadata.name;
    const base = name();
    expect(name()).toBe(base);
    // A service's own settings, or the image of a service that does not use Postgres, do not run
    // the Job again.
    expect(name(["--set", "services.identity.replicas=2"])).toBe(base);
    expect(name(["--set", `services.mqtt-bridge.image.digest=sha256:${"2".repeat(64)}`])).toBe(base);
    for (const change of [
      ["--set", "migrate.revision=2"],
      // A new image of a service that uses Postgres, a digest-only hotfix too: the service waits
      // for a run of the Job instead of starting on the old run's result.
      ["--set", `services.identity.image.digest=sha256:${"1".repeat(64)}`],
      // A new release: the services that use Postgres move with the Job (the tag rule).
      ["--set", "migrate.image.tag=9.9.9", "--set", "services.identity.image.tag=9.9.9"],
      ["--set", "migrate.wait.lock=5m"],
      ["--set", "hosts.app=other.saas.trustiot.test"],
      // A rotation: the Job generates new passwords, and the services restart with them.
      ["--set", "migrate.passwordGeneration=2"],
      ["--set", "observability.collector.clickhouse.user=trustiot_collector_b"],
    ]) {
      expect({ change, name: name(change) }).not.toEqual({ change, name: base });
    }
  });

  test("a service that uses Postgres must have the migration Job's tag", () => {
    // Only a change of the Job's own spec runs it again, so a service image of another release
    // would start on a schema the Job never applied, without waiting.
    for (const [what, args] of [
      ["only the service moves", ["--set", "services.identity.image.tag=9.9.9"]],
      ["only the default service tag moves", ["--set", "serviceDefaults.image.tag=9.9.9"]],
      ["only the Job moves", ["--set", "migrate.image.tag=9.9.9"]],
    ] as const) {
      const result = helmTemplate(SHAPES.saas, [...withIdentity, ...args]);
      expect(result.code, what).not.toBe(0);
      expect(result.stderr, what).toContain("services.identity: image tag");
      expect(result.stderr, what).toContain("Set migrate.image.tag");
    }
    // The same release renders, whichever field carries it.
    for (const args of [
      ["--set", "migrate.image.tag=9.9.9", "--set", "services.identity.image.tag=9.9.9"],
      ["--set", "migrate.image.tag=9.9.9", "--set", "serviceDefaults.image.tag=9.9.9"],
    ]) {
      const identity = find(render(SHAPES.saas, [...withIdentity, ...args]), "Deployment", `${RELEASE}-identity`);
      expect(podSpec(identity).containers[0]?.image).toBe("ghcr.io/laysi/trustiot-v2/identity:9.9.9");
      expect(initContainers(identity)[0]?.image).toBe("ghcr.io/laysi/trustiot-v2/migrate:9.9.9");
    }
    // A service that does not use Postgres, or a chart without the Job, is not held to it.
    expect(helmTemplate(SHAPES.saas, [...withIdentity, "--set", "services.mqtt-bridge.image.tag=9.9.9"]).code).toBe(0);
    expect(
      helmTemplate(SHAPES.saas, [...withIdentity, "--set", "services.identity.image.tag=9.9.9", "--set", "migrate.enabled=false"]).code,
    ).toBe(0);
  });

  test("passwordGeneration and revision from a values file render as exact decimals, and a value the Job would refuse fails the render", () => {
    // A values file gives YAML integers to the template as float64, which toString renders as
    // "1e+06" from 1000000 on; the Job refuses that (Laysi/trustiot-v2#60 round 5, N1).
    const dir = mkdtempSync(join(tmpdir(), "trustiot-chart-generation-"));
    try {
      const file = (name: string, yaml: string) => {
        const path = join(dir, name);
        writeFileSync(path, yaml);
        return path;
      };
      const rendered = (yaml: string) => {
        const job = jobOf(render([...SHAPES.saas, file("values.yaml", yaml)], withIdentity));
        const env = (job.spec as { template: { spec: PodSpec } }).template.spec.containers[0]?.env ?? [];
        const annotations = (job.spec as { template: { metadata: { annotations: Record<string, string> } } }).template.metadata.annotations;
        return {
          generation: env.find((e) => e.name === "TRUSTIOT__PASSWORDS__GENERATION")?.value,
          revision: annotations["trustiot.io/migrate-revision"],
        };
      };
      for (const [yaml, generation, revision] of [
        ["migrate:\n  passwordGeneration: 20261014\n  revision: 20261014\n", "20261014", "20261014"],
        ["migrate:\n  passwordGeneration: 1000000\n  revision: 1000000\n", "1000000", "1000000"],
        ["migrate:\n  passwordGeneration: 999999999\n", "999999999", "1"],
        ['migrate:\n  passwordGeneration: "20261014"\n  revision: "r-2"\n', "20261014", "r-2"],
        ["migrate:\n  passwordGeneration: 0\n", "0", "1"],
      ] as const) {
        expect({ yaml, ...rendered(yaml) }).toEqual({ yaml, generation, revision });
      }
      // --set gives a string, which renders as it is.
      expect(envValue(jobEnv(render(SHAPES.saas, [...withIdentity, "--set", "migrate.passwordGeneration=1000000"])), "TRUSTIOT__PASSWORDS__GENERATION")?.value).toBe(
        "1000000",
      );
      // Ten digits: the schema refuses the string, the template refuses the number it would
      // otherwise pass on.
      for (const yaml of ["migrate:\n  passwordGeneration: 1000000000\n", 'migrate:\n  passwordGeneration: "1000000000"\n']) {
        const result = helmTemplate([...SHAPES.saas, file("bad.yaml", yaml)], withIdentity);
        expect(result.code, yaml).not.toBe(0);
        expect(result.stderr, yaml).toContain("passwordGeneration");
      }
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("the longest release name keeps the Job's name within 63 characters", () => {
    const release = "a".repeat(53);
    const long = render(SHAPES.saas, withIdentity, release);
    const job = jobOf(long).metadata.name;
    expect(job.length).toBeLessThanOrEqual(63);
    expect(job).toMatch(/-migrate-[0-9a-f]{8}$/);
  });

  test("migrate.enabled false renders no Job, no init container and no Job-related access", () => {
    const off = render(SHAPES.saas, [...withIdentity, "--set", "migrate.enabled=false"]);
    expect(jobsOf(off)).toEqual([]);
    expect(off.filter((m) => m.metadata.name.includes("-migrate"))).toEqual([]);
    // The identity pod still fills its signing keys, which needs no Job.
    for (const deployment of off.filter(isServiceDeployment)) {
      const names = ((podSpec(deployment).initContainers ?? []) as Container[]).map((c) => c.name);
      expect(names).toEqual(deployment.metadata.name === `${RELEASE}-identity` ? ["signing-keys"] : []);
    }
    expect(JSON.stringify(find(off, "NetworkPolicy", `${RELEASE}-postgres`))).not.toContain('"migrate"');
  });

  test.each([
    ["a password generation that is not a number", ["--set", "migrate.passwordGeneration=v2"], /passwordGeneration/],
    ["the removed service roles Secret", ["--set", "postgres.serviceRolesSecret.name=x"], /serviceRolesSecret/],
    ["the removed collector password Secret", ["--set", "observability.collector.clickhouse.passwordSecret.name=x"], /passwordSecret/],
    [
      "a collector on another architecture while the Job runs",
      ["--set", "observability.collector.nodeSelector.kubernetes\\.io/arch=amd64"],
      /kubernetes\.io\/arch must be arm64 while migrate\.enabled is true/,
    ],
    [
      "external Postgres without its migration Secret",
      ["--set", "postgres.mode=external", "--set", "postgres.external.host=db.factory.test", "--set", "postgres.external.caSecret.name=ca"],
      /postgres\.external\.migrationSecret\.name is required/,
    ],
    [
      "external ClickHouse without its migration Secret",
      ["--set", "clickhouse.mode=external", "--set", "clickhouse.external.url=https://ch.factory.test:8443", "--set", "clickhouse.external.caSecret.name=ca"],
      /clickhouse\.external\.migrationSecret\.name is required/,
    ],
    [
      "external ClickHouse without a CA",
      ["--set", "clickhouse.mode=external", "--set", "clickhouse.external.url=https://ch.factory.test:8443", "--set", "clickhouse.external.migrationSecret.name=m"],
      /clickhouse\.external\.caSecret\.name is required while migrate\.enabled/,
    ],
    ["a service named migrate", ["--skip-schema-validation", "--set", "services.migrate.enabled=false"], /services\.migrate: the name belongs to the migration Job/],
    ["a wait longer than an hour", ["--set", "migrate.wait.lock=2h"], /wait/],
  ])("the chart refuses %s", (_name, args, expected) => {
    const result = helmTemplate(SHAPES.saas, args);
    expect(result.code).not.toBe(0);
    expect(result.stderr).toMatch(expected);
  });
});

describe("first platform administrator (Laysi/trustiot-v2#582)", () => {
  const BOOTSTRAP = `${RELEASE}-platform-admin-bootstrap`;
  const withIdentity = ["--set", "services.identity.enabled=true"];
  type CronJobSpec = {
    schedule: string;
    timeZone: string;
    suspend: boolean;
    concurrencyPolicy: string;
    successfulJobsHistoryLimit: number;
    failedJobsHistoryLimit: number;
    jobTemplate: {
      metadata: { labels: Record<string, string> };
      spec: {
        parallelism: number;
        completions: number;
        podReplacementPolicy: string;
        backoffLimit: number;
        activeDeadlineSeconds: number;
        ttlSecondsAfterFinished: number;
        template: { metadata: { labels: Record<string, string> }; spec: PodSpec & { restartPolicy: string; initContainers?: unknown } };
      };
    };
  };
  type BootstrapContainer = Container & { args: string[]; stdin: boolean; stdinOnce: boolean; tty: boolean };
  type Peer = { podSelector?: { matchLabels: Record<string, string> } };
  const cronJobSpec = (manifests: Manifest[]) => find(manifests, "CronJob", BOOTSTRAP).spec as CronJobSpec;
  const migrationJob = (manifests: Manifest[]) => {
    const jobs = manifests.filter((m) => m.kind === "Job");
    expect(jobs).toHaveLength(1);
    return jobs[0]?.spec as { template: { metadata: { labels: Record<string, string> }; spec: PodSpec } };
  };
  const selects = (selector: Record<string, string>, labels: Record<string, string>) =>
    Object.entries(selector).every(([key, value]) => labels[key] === value);

  test.each(SHAPE_NAMES)("%s: a suspended CronJob that never runs on its own, one attempt, reading the address on stdin", (shape) => {
    const manifests = render([...SHAPES[shape], SMOKE], withIdentity);
    const spec = cronJobSpec(manifests);
    // Only `kubectl create job --from=cronjob/...` starts it: suspended, and February 31st never comes.
    expect(spec.suspend).toBe(true);
    expect(spec.schedule).toBe("0 0 31 2 *");
    expect(spec.timeZone).toBe("Etc/UTC");
    expect(spec.concurrencyPolicy).toBe("Forbid");
    // The Job an operator creates from it is the CronJob's; the controller prunes finished ones
    // beyond these limits even while suspended, so 0 would delete a successful run before its exit
    // code is read. At 1 only ttlSecondsAfterFinished removes it.
    expect(spec.successfulJobsHistoryLimit).toBe(1);
    expect(spec.failedJobsHistoryLimit).toBe(1);
    const job = spec.jobTemplate.spec;
    // A second pod would wait for an address nobody writes again.
    expect({ parallelism: job.parallelism, completions: job.completions, backoffLimit: job.backoffLimit, podReplacementPolicy: job.podReplacementPolicy }).toEqual({
      parallelism: 1,
      completions: 1,
      backoffLimit: 0,
      podReplacementPolicy: "Failed",
    });
    expect(job.activeDeadlineSeconds).toBe(900);
    expect(job.ttlSecondsAfterFinished).toBe(86400);
    const pod = job.template.spec;
    expect(pod.restartPolicy).toBe("Never");
    expect(pod.initContainers).toBeUndefined();
    expect(pod.containers).toHaveLength(1);
    const container = pod.containers[0] as BootstrapContainer;
    expect(container.name).toBe("bootstrap-platform-admin");
    expect(container.args).toEqual(["bootstrap-platform-admin"]);
    // The address arrives on stdin, from the first `kubectl attach -i`, and nowhere else.
    expect({ stdin: container.stdin, stdinOnce: container.stdinOnce, tty: container.tty }).toEqual({ stdin: true, stdinOnce: true, tty: false });
    expect(JSON.stringify(spec)).not.toContain("@");
    // The migration Job's image, ServiceAccount (no token), security settings and Postgres CA.
    const migration = migrationJob(manifests);
    const migrate = migration.template.spec.containers[0] as Container;
    expect(container.image).toBe(migrate.image);
    expect(pod.serviceAccountName).toBe(`${RELEASE}-migrate`);
    expect(pod.serviceAccountName).toBe(migration.template.spec.serviceAccountName);
    expect(pod.automountServiceAccountToken).toBe(false);
    expect(pod.securityContext).toEqual(migration.template.spec.securityContext);
    expect(container.securityContext).toEqual(migrate.securityContext);
    expect(container.envFrom).toEqual(migrate.envFrom);
    expect(pod.volumes).toEqual(migration.template.spec.volumes?.filter((v) => v.name === "postgres-ca"));
    expect(container.volumeMounts).toEqual(migrate.volumeMounts?.filter((v) => v.name === "postgres-ca"));
  });

  test.each(SHAPE_NAMES)("%s: the pod holds the migration role and nothing else of the migration Job: no Kubernetes API token, no password Secret to fill, no ClickHouse", (shape) => {
    const manifests = render([...SHAPES[shape], SMOKE], withIdentity);
    const container = cronJobSpec(manifests).jobTemplate.spec.template.spec.containers[0] as BootstrapContainer;
    const migrate = migrationJob(manifests).template.spec.containers[0] as Container;
    const env = container.env ?? [];
    // Exactly the migration Job's Postgres settings (CA file and the migration role's Secret), in
    // its order, plus the wait for the database.
    const postgres = (migrate.env ?? []).filter((e) => e.name.startsWith("TRUSTIOT__POSTGRES__") || e.name === "TRUSTIOT_POSTGRES_CA_FILE");
    expect(postgres.length).toBeGreaterThan(0);
    expect(env).toEqual([...postgres, { name: "TRUSTIOT__WAIT__DATABASES", value: "10m" }]);
    const text = JSON.stringify(cronJobSpec(manifests));
    for (const absent of ["TRUSTIOT__PASSWORDS__", "TRUSTIOT__CLICKHOUSE__", "migrate-kube-api", "serviceAccountToken"]) {
      expect(text, absent).not.toContain(absent);
    }
    // The wait follows the migration Job's.
    const slower = render([...SHAPES[shape], SMOKE], [...withIdentity, "--set", "migrate.wait.databases=20m"]);
    expect((cronJobSpec(slower).jobTemplate.spec.template.spec.containers[0]?.env ?? []).at(-1)).toEqual({ name: "TRUSTIOT__WAIT__DATABASES", value: "20m" });
  });

  test.each(SHAPE_NAMES)("%s: every NetworkPolicy and AuthorizationPolicy treats its pod as it treats the migration Job's", (shape) => {
    const manifests = render([...SHAPES[shape], SMOKE], [...enableAllServices, "--set", "observability.collector.enabled=true"]);
    const job = migrationJob(manifests).template.metadata.labels;
    const pod = cronJobSpec(manifests).jobTemplate.spec.template.metadata.labels;
    expect(pod["app.kubernetes.io/component"]).toBe("migrate");
    expect(pod["trustiot.io/task"]).toBe("platform-admin-bootstrap");
    expect(pod["istio.io/dataplane-mode"]).toBe("ambient");
    // Every selector in the policies: the ones that pick the pods a policy applies to and the
    // peers a NetworkPolicy admits.
    const selectors: Record<string, string>[] = [];
    for (const m of manifests.filter((m) => m.kind === "NetworkPolicy" || m.kind === "AuthorizationPolicy")) {
      const s = m.spec as {
        selector?: { matchLabels?: Record<string, string> };
        podSelector?: { matchLabels?: Record<string, string> };
        ingress?: { from?: Peer[] }[];
        egress?: { to?: Peer[] }[];
      };
      for (const selector of [s.selector?.matchLabels, s.podSelector?.matchLabels]) if (selector) selectors.push(selector);
      for (const peer of [...(s.ingress ?? []).flatMap((r) => r.from ?? []), ...(s.egress ?? []).flatMap((r) => r.to ?? [])]) {
        if (peer.podSelector) selectors.push(peer.podSelector.matchLabels);
      }
    }
    // Some policy names the migration Job's pods (the Postgres NetworkPolicy admits them), so the
    // comparison below is not vacuous.
    expect(selectors.some((selector) => selects(selector, job) && "app.kubernetes.io/component" in selector)).toBe(true);
    for (const selector of selectors) {
      expect({ selector, bootstrap: selects(selector, pod) }).toEqual({ selector, bootstrap: selects(selector, job) });
    }
  }, SLOW_TEST_MS);

  test("rendered only while the migration Job and the identity service are, with a name the Job controller can extend", () => {
    for (const shape of SHAPE_NAMES) {
      // The identity service is off in both shapes' defaults.
      expect(render(SHAPES[shape]).some((m) => m.metadata.name === BOOTSTRAP), shape).toBe(false);
      expect(render(SHAPES[shape], withIdentity).filter((m) => m.metadata.name === BOOTSTRAP).map((m) => m.kind), shape).toEqual(["CronJob"]);
    }
    const withoutMigrate = render(SHAPES.saas, [...withIdentity, "--set", "migrate.enabled=false"]);
    expect(withoutMigrate.some((m) => m.metadata.name.endsWith("-platform-admin-bootstrap"))).toBe(false);
    // The CronJob controller appends 11 characters to name each Job; Helm caps a release name at 53.
    for (const release of ["r", `trustiot-${"x".repeat(44)}`]) {
      const names = render(SHAPES.saas, withIdentity, release)
        .filter((m) => m.kind === "CronJob" && m.metadata.name.endsWith("-platform-admin-bootstrap"))
        .map((m) => m.metadata.name);
      expect(names).toHaveLength(1);
      expect((names[0] ?? "").length, release).toBeLessThanOrEqual(52);
    }
  }, SLOW_TEST_MS);

  test("the chart refuses a service named after the CronJob", () => {
    const result = helmTemplate(SHAPES.saas, ["--skip-schema-validation", "--set", "services.platform-admin-bootstrap.enabled=false"]);
    expect(result.code).not.toBe(0);
    expect(result.stderr).toMatch(/services\.platform-admin-bootstrap: the name belongs to the CronJob that creates the first platform administrator/);
  });
});

describe("OpenTelemetry collector (decision 22)", () => {
  const name = `${RELEASE}-otel-collector`;
  const manifests = render([...SHAPES.saas, SMOKE]);
  const deployment = find(manifests, "Deployment", name);
  const pod = podSpec(deployment);
  const container = pod.containers[0] as Container;

  function configOf(rendered: Manifest[]): CollectorConfig {
    const data = find(rendered, "ConfigMap", `${RELEASE}-otel-collector-config`).data as Record<string, string>;
    return Bun.YAML.parse(data["config.yaml"] ?? "") as CollectorConfig;
  }
  const config = configOf(manifests);
  const forwardArgs = (endpoint: string, ...extra: string[]) => [
    "--set",
    "observability.collector.forward.enabled=true",
    "--set",
    `observability.collector.forward.endpoint=${endpoint}`,
    ...extra,
  ];

  // The redaction processor's rules, as JavaScript regular expressions. Go RE2 and JavaScript
  // agree on the syntax used here; a leading (?i) becomes the i flag.
  function goRegex(pattern: string): RegExp {
    return pattern.startsWith("(?i)") ? new RegExp(pattern.slice(4), "i") : new RegExp(pattern);
  }
  const rules = config.processors.redaction;
  const masksKey = (key: string) =>
    !rules.ignored_key_patterns.some((p) => goRegex(p).test(key)) &&
    rules.blocked_key_patterns.some((p) => goRegex(p).test(key));
  const maskValue = (value: string) =>
    rules.blocked_values.reduce((text, p) => text.replace(new RegExp(goRegex(p), `${goRegex(p).flags}g`), "****"), value);

  // Field names from service-kit's own redaction tests (crates/service-kit/src/telemetry/redact.rs).
  function serviceKitNames(testName: string): string[] {
    const source = readFileSync(join(REPO_ROOT, "crates/service-kit/src/telemetry/redact.rs"), "utf8");
    const body = source.split(`fn ${testName}()`)[1]?.split("] {")[0] ?? "";
    const names = [...body.matchAll(/"([^"]+)"/g)].map((m) => m[1] ?? "");
    if (names.length < 10) throw new Error(`no names found in ${testName}`);
    return names;
  }

  test("the image is the contrib distribution of the checked version, pinned by digest", () => {
    // The digest lives in values.yaml only, so a new digest of the same version needs no edit here.
    expect(container.image).toMatch(
      new RegExp(`^docker\\.io/otel/opentelemetry-collector-contrib:${EXPORTER_VERSION.replaceAll(".", "\\.")}@sha256:[0-9a-f]{64}$`),
    );
    expect(container.args).toEqual(["--config=/etc/otelcol/config.yaml"]);
  });

  test("the pod meets the restricted Pod Security Standard, joins the ambient mesh and is reachable on OTLP/HTTP only", () => {
    const labels = (deployment.spec as { template: { metadata: { labels: Record<string, string> } } }).template.metadata
      .labels;
    expect(labels["istio.io/dataplane-mode"]).toBe("ambient");
    expect(pod.automountServiceAccountToken).toBe(false);
    expect(pod.serviceAccountName).toBe(name);
    expect(pod.securityContext).toEqual({ runAsNonRoot: true, seccompProfile: { type: "RuntimeDefault" } });
    expect(container.securityContext).toEqual({
      allowPrivilegeEscalation: false,
      readOnlyRootFilesystem: true,
      capabilities: { drop: ["ALL"] },
    });
    expect(container.resources?.limits?.memory).toBe("512Mi");
    for (const probe of [container.startupProbe, container.livenessProbe, container.readinessProbe]) {
      expect(probe).toMatchObject({ httpGet: { path: "/", port: "health" } });
    }
    const service = find(manifests, "Service", name).spec as { type: string; ports: { name: string; port: number }[] };
    expect(service.type).toBe("ClusterIP");
    expect(service.ports.map((p) => `${p.name}:${p.port}`)).toEqual(["otlp-http:4318"]);
    expect(Object.keys(config.receivers.otlp.protocols)).toEqual(["http"]);
  });

  test("every service exports to the collector, and turning the collector off removes it everywhere", () => {
    const smoke = podSpec(find(manifests, "Deployment", `${RELEASE}-smoke-echo`)).containers[0];
    const endpoint = smoke?.env?.find((e) => e.name === "TRUSTIOT__TELEMETRY__OTLP_ENDPOINT");
    expect(endpoint).toEqual({
      name: "TRUSTIOT__TELEMETRY__OTLP_ENDPOINT",
      value: `http://${name}.${NAMESPACE}.svc:4318`,
    });
    const off = render([...SHAPES.saas, SMOKE], ["--set", "observability.collector.enabled=false"]);
    expect(off.filter((m) => m.metadata.name.startsWith(name))).toEqual([]);
    const offEnv = podSpec(find(off, "Deployment", `${RELEASE}-smoke-echo`)).containers[0]?.env ?? [];
    expect(offEnv.map((e) => e.name)).not.toContain("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT");
    // Without the chart's collector, a service may point at another one.
    const own = render(
      [...SHAPES.saas, SMOKE],
      [
        "--set",
        "observability.collector.enabled=false",
        "--set-json",
        'services.smoke-echo.env=[{"name":"TRUSTIOT__TELEMETRY__OTLP_ENDPOINT","value":"http://otel.example.test:4318"}]',
      ],
    );
    expect(podSpec(find(own, "Deployment", `${RELEASE}-smoke-echo`)).containers[0]?.env).toContainEqual({
      name: "TRUSTIOT__TELEMETRY__OTLP_ENDPOINT",
      value: "http://otel.example.test:4318",
    });
  });

  test("the collector waits for the migration Job, which generates its password, with a token only the init container mounts", () => {
    const job = (manifests.find((m) => m.kind === "Job") as Manifest).metadata.name;
    const inits = (pod.initContainers ?? []) as (Container & { args: string[] })[];
    expect(inits.map((c) => c.name)).toEqual(["wait-for-migrations"]);
    expect(inits[0]?.args).toEqual(["wait-for-job"]);
    expect(inits[0]?.env?.[0]).toEqual({ name: "TRUSTIOT__KUBE__JOB", value: job });
    expect(inits[0]?.securityContext).toEqual(container.securityContext);
    expect(inits[0]?.volumeMounts).toEqual([{ name: "migrate-kube-api", mountPath: "/var/run/secrets/trustiot/kube", readOnly: true }]);
    expect(JSON.stringify(container.volumeMounts ?? [])).not.toContain("migrate-kube-api");
    expect(pod.automountServiceAccountToken).toBe(false);
    // The init container runs the arm64-only migration image.
    expect(pod.nodeSelector).toEqual({ "kubernetes.io/arch": "arm64" });
    const placed = podSpec(
      find(render([...SHAPES.saas, SMOKE], ["--set", "observability.collector.nodeSelector.pool=logs"]), "Deployment", name),
    );
    expect(placed.nodeSelector).toEqual({ "kubernetes.io/arch": "arm64", pool: "logs" });
    // Without the Job nothing is waited for, and the collector keeps its own placement.
    const off = podSpec(find(render([...SHAPES.saas, SMOKE], ["--set", "migrate.enabled=false"]), "Deployment", name));
    expect(off.initContainers).toBeUndefined();
    expect(off.nodeSelector).toBeUndefined();
    expect(JSON.stringify(off)).not.toContain("migrate-kube-api");
  });

  test("secrets reach the collector only through secretKeyRef, never its configuration", () => {
    const env = Object.fromEntries((container.env ?? []).map((e) => [e.name, e]));
    expect(env.CLICKHOUSE_USER).toEqual({ name: "CLICKHOUSE_USER", value: "trustiot_collector" });
    // The password the migration Job generates into the collector's own Secret.
    expect(env.CLICKHOUSE_PASSWORD?.valueFrom).toEqual({
      secretKeyRef: { name: `${RELEASE}-otel-collector-password`, key: "trustiot_collector" },
    });
    expect(env.FORWARD_AUTH_HEADER).toBeUndefined();
    const clickhouse = config.exporters.clickhouse as YamlMap;
    expect(clickhouse.username).toBe("${env:CLICKHOUSE_USER}");
    expect(clickhouse.password).toBe("${env:CLICKHOUSE_PASSWORD}");
  });

  test("logs and traces are redacted before any exporter; metrics are dropped", () => {
    const { pipelines } = config.service;
    expect(Object.keys(pipelines).sort()).toEqual(["logs", "metrics", "traces"]);
    expect(pipelines.logs).toEqual({ receivers: ["otlp"], processors: ["memory_limiter", "redaction"], exporters: ["clickhouse"] });
    expect(pipelines.traces).toEqual({
      receivers: ["otlp"],
      processors: ["memory_limiter", "redaction", "transform/redact"],
      exporters: ["clickhouse"],
    });
    expect(pipelines.metrics).toEqual({ receivers: ["otlp"], processors: ["memory_limiter"], exporters: ["nop"] });
    expect(rules.allow_all_keys).toBe(true);
    expect(rules.redact_all_types).toBe(true);
    // The audit attributes would list which keys were masked.
    expect(rules.summary).toBe("silent");
    // Span names and status messages get the value patterns too.
    const groups = config.processors["transform/redact"].trace_statements;
    expect(groups.map((g) => g.context)).toEqual(["span"]);
    const statements = groups[0]?.statements ?? [];
    expect(statements).toHaveLength(2 * rules.blocked_values.length);
    expect(statements[0]).toStartWith("replace_pattern(name, ");
    expect(statements.at(-1)).toStartWith("replace_pattern(status.message, ");
  });

  test("key rules mask every name service-kit masks and keep every name it keeps", () => {
    const sensitive = serviceKitNames("credentials_personal_data_and_payloads_are_sensitive");
    const kept = serviceKitNames("operational_fields_are_kept");
    expect(sensitive.filter((key) => !masksKey(key))).toEqual([]);
    expect(kept.filter(masksKey)).toEqual([]);
    for (const key of ["requestBody", "userEmail", "chatContent", "accessJWT", "PROMPT", "http.request.body.content"]) {
      expect(masksKey(key)).toBe(true);
    }
    for (const key of ["contentType", "bodySize", "http.response.body.size", "content-length", "test.marker"]) {
      expect(masksKey(key)).toBe(false);
    }
  });

  test("value rules mask credentials and personal data inside text and keep operational values", () => {
    const masked: [string, string][] = [
      ["login failed for alice.wang@example.com", "login failed for ****"],
      ["Authorization: Bearer abcDEF123456ghiJKL", "Authorization: ****"],
      ["token eyJhbGci.eyJzdWIi.c2lnbmF0", "token ****"],
      ["password=hunter2 and api_key: sk-abc", "**** and ****"],
      ["postgres://svc:pa55word@db:5432/x", "****db:5432/x"],
      ["call 0912-345-678 or +886 912 345 678", "call **** or ****"],
      ["holder A123456789", "holder ****"],
      ["key sk-test0123456789abcdefghij", "key ****"],
      ["-----BEGIN EC PRIVATE KEY-----", "****"],
    ];
    for (const [input, expected] of masked) expect(maskValue(input)).toBe(expected);
    for (const kept of [
      "request completed",
      "/v1/devices/{id}",
      "4bf92f3577b34da6a3ce929d0e0e4736",
      "2026-10-10T08:00:00.123456Z",
      "3f2a1c4e-9b7d-4e60-8a5b-0c1d2e3f4a5b",
      "duration 3.2 ms, 1234567 rows",
      "https://app.saas.trustiot.test/api/assets",
      "10.42.0.1:8080",
    ]) {
      expect(maskValue(kept)).toBe(kept);
    }
  });

  test("ClickHouse gets inserts only, into tables the migrations own", () => {
    const clickhouse = config.exporters.clickhouse as YamlMap;
    expect(clickhouse).toMatchObject({
      endpoint: `tcp://${RELEASE}-clickhouse.${NAMESPACE}.svc:9000?dial_timeout=10s`,
      database: "trustiot_logs",
      create_schema: false,
      logs_table_name: "otel_logs",
      traces_table_name: "otel_traces",
    });
    expect(container.image).toContain(`:${EXPORTER_VERSION}@`);
    const sql = readFileSync(join(REPO_ROOT, "services/ingest/migrations/clickhouse/0006_logs.sql"), "utf8");
    for (const [table, columns] of Object.entries(EXPORTER_COLUMNS)) {
      const ddl = sql.split(`CREATE TABLE IF NOT EXISTS trustiot_logs.${table}\n`)[1]?.split("\nENGINE")[0] ?? "";
      const defined = [...ddl.matchAll(/^ {4}([A-Za-z]+) /gm)].map((m) => m[1]);
      expect(columns.filter((column) => !defined.includes(column))).toEqual([]);
      expect(sql).toContain(`GRANT INSERT, SHOW COLUMNS ON trustiot_logs.${table} TO trustiot_otel_collector;`);
    }
  });

  test("external ClickHouse is reached over its HTTPS URL with its CA", () => {
    const external = render(SHAPES.saas, [
      "--set",
      "clickhouse.mode=external",
      "--set",
      "clickhouse.external.url=https://ch.saas.trustiot.test:8443",
      "--set",
      "clickhouse.external.caSecret.name=ch-ca",
      ...EXTERNAL_MIGRATION,
    ]);
    expect(configOf(external).exporters.clickhouse).toMatchObject({
      endpoint: "https://ch.saas.trustiot.test:8443",
      tls: { ca_file: "/etc/trustiot/clickhouse-ca/ca.crt" },
    });
    const externalPod = podSpec(find(external, "Deployment", name));
    expect(externalPod.volumes).toContainEqual({
      name: "clickhouse-ca",
      secret: { secretName: "ch-ca", items: [{ key: "ca.crt", path: "ca.crt" }] },
    });
  });

  test("forwarding is off by default and copies only the chosen signals to the SIEM", () => {
    for (const shape of SHAPE_NAMES) {
      expect(Object.keys(configOf(render(SHAPES[shape])).exporters).sort()).toEqual(["clickhouse", "nop"]);
    }
    const logsOnly = configOf(render(SHAPES.onprem, forwardArgs("https://siem.factory.test:4318")));
    expect(logsOnly.exporters["otlp_http/siem"]).toMatchObject({ endpoint: "https://siem.factory.test:4318" });
    expect(logsOnly.service.pipelines.logs?.exporters).toEqual(["clickhouse", "otlp_http/siem"]);
    expect(logsOnly.service.pipelines.traces?.exporters).toEqual(["clickhouse"]);
    expect(logsOnly.service.pipelines.metrics?.exporters).toEqual(["nop"]);

    const both = configOf(
      render(
        SHAPES.onprem,
        forwardArgs("http://siem.factory.test:4317", "--set", "observability.collector.forward.protocol=grpc", "--set-json", 'observability.collector.forward.signals=["logs","traces"]'),
      ),
    );
    // The gRPC exporter would try TLS on an http:// endpoint.
    expect(both.exporters["otlp_grpc/siem"]).toMatchObject({ endpoint: "http://siem.factory.test:4317", tls: { insecure: true } });
    expect(both.service.pipelines.traces?.exporters).toEqual(["clickhouse", "otlp_grpc/siem"]);
  });

  test("the SIEM's auth header and CA come from Secrets", () => {
    const rendered = render(
      SHAPES.onprem,
      forwardArgs(
        "https://siem.factory.test",
        "--set",
        "observability.collector.forward.authHeader.name=Authorization",
        "--set",
        "observability.collector.forward.authHeader.valueSecret.name=siem-auth",
        "--set",
        "observability.collector.forward.caSecret.name=siem-ca",
      ),
    );
    expect(configOf(rendered).exporters["otlp_http/siem"]).toMatchObject({
      headers: { Authorization: "${env:FORWARD_AUTH_HEADER}" },
      tls: { ca_file: "/etc/trustiot/forward-ca/ca.crt" },
    });
    const forwardPod = podSpec(find(rendered, "Deployment", name));
    const env = forwardPod.containers[0]?.env ?? [];
    expect(env.find((e) => e.name === "FORWARD_AUTH_HEADER")?.valueFrom).toEqual({
      secretKeyRef: { name: "siem-auth", key: "value" },
    });
    expect(forwardPod.volumes).toContainEqual({
      name: "forward-ca",
      secret: { secretName: "siem-ca", items: [{ key: "ca.crt", path: "ca.crt" }] },
    });
  });

  test("in the mesh, every enabled service and nothing else reaches the collector", () => {
    const rendered = render([...SHAPES.saas, SMOKE], enableAllServices);
    const policy = find(rendered, "AuthorizationPolicy", name).spec as {
      selector: { matchLabels: Record<string, string> };
      rules: { from: { source: { principals: string[] } }[] }[];
    };
    expect(policy.selector.matchLabels["app.kubernetes.io/component"]).toBe("otel-collector");
    const services = [...plannedServices, "smoke-echo"].sort();
    expect(policy.rules[0]?.from[0]?.source.principals).toEqual(
      services.map((s) => `cluster.local/ns/${NAMESPACE}/sa/${RELEASE}-${s}`),
    );
    const network = find(rendered, "NetworkPolicy", name).spec as {
      ingress: { from: { podSelector?: { matchLabels: Record<string, string> }; ipBlock?: unknown }[]; ports?: unknown }[];
    };
    const sources = network.ingress[0]?.from.map((peer) => peer.podSelector?.matchLabels["app.kubernetes.io/component"]);
    expect(sources?.sort()).toEqual(services);
    expect(network.ingress[0]?.ports).toEqual([{ port: 15008, protocol: "TCP" }]);
    // The kubelet's probes reach only the health_check port; OTLP (4318) comes through the mesh.
    expect(network.ingress.at(-1)).toEqual({
      from: [{ ipBlock: { cidr: "169.254.7.127/32" } }],
      ports: [{ port: 13133, protocol: "TCP" }],
    });
  });

  test("the pod rolls when the configuration changes", () => {
    const checksum = (rendered: Manifest[]) =>
      (find(rendered, "Deployment", name).spec as { template: { metadata: { annotations: Record<string, string> } } })
        .template.metadata.annotations["checksum/config"];
    expect(checksum(manifests)).toMatch(/^[0-9a-f]{64}$/);
    expect(checksum(render([...SHAPES.onprem, SMOKE], forwardArgs("https://siem.factory.test")))).not.toBe(
      checksum(render([...SHAPES.onprem, SMOKE])),
    );
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
  // hosts.app of the shape's instance overlay, where the SPA's static files are served.
  const appHost = (shape: Shape) => PUBLIC_PATHS[shape][0][0];
  // The SPA's static files off: the e2e overlay and enableAllServices turn them on.
  const WEB_OFF = ["--set", "services.web.enabled=false"];
  // The paths of hosts.app that keep the cookie check while the SPA is on (templates/_ingress.tpl).
  const COOKIE_PREFIXES = ["/api", "/auth"];
  const underCookiePrefix = (path: string) => COOKIE_PREFIXES.some((prefix) => path === prefix || path.startsWith(`${prefix}/`));

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

  // ingress.tunnel.clusterIP (Laysi/trustiot-v2#478): Istio applies the service overlay to the
  // Service it generates, so the address cloudflared forwards to survives a recreated Service.
  const serviceOverlay = (manifests: Manifest[]) =>
    (find(manifests, "ConfigMap", `${GATEWAY}-options`).data as Record<string, string>).service ?? "";
  // The overlay text before the field existed: unset, the chart renders exactly this.
  const TUNNEL_OVERLAY = "spec:\n  # Reached only by cloudflared on the host; nothing listens on the host or the network.\n  type: ClusterIP\n";
  const DIRECT_OVERLAY = "spec:\n  type: LoadBalancer\n";

  test("tunnel: ingress.tunnel.clusterIP pins the Service's ClusterIP in the service overlay", () => {
    const overlay = serviceOverlay(render(SHAPES.saas, ["--set", "ingress.tunnel.clusterIP=10.43.0.240"]));
    expect(Bun.YAML.parse(overlay)).toEqual({ spec: { type: "ClusterIP", clusterIP: "10.43.0.240" } });
    expect(overlay).toBe(
      `${TUNNEL_OVERLAY}  # Fixed, so a recreated Service keeps the address cloudflared forwards to (ingress.tunnel.clusterIP).\n  clusterIP: 10.43.0.240\n`,
    );
  });

  test.each(SHAPE_NAMES)("%s: without ingress.tunnel.clusterIP the service overlay is the text it was before the field", (shape) => {
    expect(serviceOverlay(render(SHAPES[shape]))).toBe(shape === "saas" ? TUNNEL_OVERLAY : DIRECT_OVERLAY);
  });

  // Every overlay the chart tests and the nightly install, rendered with the field left out and
  // with it set to "" (its default in values.yaml): the whole output is byte for byte the same.
  test.each(
    SHAPE_NAMES.flatMap((shape) =>
      [[], [SMOKE], [E2E_VALUES], [E2E_VALUES, "tests/e2e/values-mesh-e2e.yaml"]].map((extra) => [`${shape}${extra.map((f) => ` + ${f}`).join("")}`, [...SHAPES[shape], ...extra]] as const),
    ),
  )("%s: an empty ingress.tunnel.clusterIP renders byte for byte what an absent one does", (_name, files) => {
    const absent = helmTemplate(files, ["--set", "ingress.tunnel.clusterIP=null"]);
    const empty = helmTemplate(files, ["--set-string", "ingress.tunnel.clusterIP="]);
    const plain = helmTemplate(files);
    expect(absent.code).toBe(0);
    expect(empty.code).toBe(0);
    expect(empty.stdout).toBe(absent.stdout);
    expect(plain.stdout).toBe(absent.stdout);
    expect(absent.stdout).not.toContain("clusterIP:");
  }, SLOW_TEST_MS);

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
    // Without the SPA's static files, as every install before Laysi/trustiot-v2#123 renders.
    const manifests = withBackends(shape, WEB_OFF);
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

  test.each(SHAPE_NAMES)("%s: with the SPA, every other path on hosts.app is its static files, without the cookie check, the Cookie or the identity header", (shape) => {
    const manifests = withBackends(shape);
    const app = appHost(shape);
    const rendered = rules(manifests);
    const statics = rendered.filter(({ rule }) => (modifier(rule).remove ?? []).includes("cookie"));
    expect(statics.map(({ host, rule }) => ({ host, matches: rule.matches, backendRefs: rule.backendRefs }))).toEqual([
      { host: app, matches: [{ path: { type: "PathPrefix", value: "/" } }], backendRefs: [{ name: `${RELEASE}-web`, port: 8080 }] },
    ]);
    // The web service holds neither the session cookie nor an identity, in any spelling.
    expect(modifier(statics[0]?.rule as HttpRouteRule).remove).toEqual(["cookie", "x-internal-identity", ...UNDERSCORE_SPELLINGS]);
    // The public paths keep their exact routes and lose only the identity header; no cookie
    // route of hosts.app lies outside the cookie prefixes.
    const others = rendered.filter((r) => !statics.includes(r));
    expect(
      others.filter(({ rule }) => strips(rule)).map(({ host, rule }) => [host, rule.matches[0]?.path.value, rule.backendRefs[0]?.name]).sort(),
    ).toEqual(PUBLIC_PATHS[shape].map((p) => [...p]).sort());
    const appCookieRoutes = others.filter(({ host, rule }) => host === app && !strips(rule));
    expect(appCookieRoutes.length).toBeGreaterThan(0);
    for (const { rule } of appCookieRoutes) expect(underCookiePrefix(rule.matches[0]?.path.value ?? "")).toBe(true);
    // Rule 1 checks every other host as without the SPA, rule 2 the cookie prefixes and /internal
    // on every host, and one more rule per public path checks it on any other host; a public path
    // outside the prefixes reaches the static files on hosts.app, so it is not checked there.
    const exempt = PUBLIC_PATHS[shape].map(([, path]) => path).sort();
    const check = policy(manifests, "cookie-check");
    expect(check.rules).toEqual([
      { to: [{ operation: { notHosts: withPorts([app]), notPaths: exempt } }] },
      { to: [{ operation: { paths: ["/api", "/api/*", "/auth", "/auth/*", "/internal*"], notPaths: exempt } }] },
      ...exempt.map((path) => {
        const hosts = PUBLIC_PATHS[shape].filter(([, p]) => p === path).map(([host]) => host as string);
        return { to: [{ operation: { paths: [path], notHosts: withPorts([...new Set(underCookiePrefix(path) ? hosts : [...hosts, app])].sort()) } }] };
      }),
    ]);
  });

  test("with the SPA, a cookie route on hosts.app outside /api and /auth, a cookie route to web, or a host named like hosts.app fails to render", () => {
    const withRoutes = (routes: object[], args: string[] = []) =>
      helmTemplate([...SHAPES.saas, E2E_VALUES], [...args, "--set-json", `ingress.routes=${JSON.stringify(routes)}`]);
    const outside =
      "while services.web is enabled, every path on hosts.app outside /api and /auth is the SPA's static files, so a cookie route there must be one of them or below one";
    for (const [path, match] of [["/", "PathPrefix"], ["/apix", "PathPrefix"], ["/authz", "Exact"], ["/settings", "PathPrefix"], ["/healthz/details", "Exact"]]) {
      const refused = withRoutes([{ host: "app", path: "/api", match: "PathPrefix", service: "assets" }, { host: "app", path, match, service: "assets" }]);
      expect(refused.code).not.toBe(0);
      expect(refused.stderr).toContain(`ingress.routes[1]: ${outside}`);
      // Without the SPA, the same route is the host's own.
      expect(withRoutes([{ host: "app", path, match, service: "assets" }], WEB_OFF).code).toBe(0);
    }
    // The prefixes themselves, paths below them, and other hosts' paths render.
    const allowed = withRoutes([
      { host: "app", path: "/api", match: "Exact", service: "assets" },
      { host: "app", path: "/api/assets", match: "PathPrefix", service: "assets" },
      { host: "app", path: "/auth", match: "PathPrefix", service: "identity" },
      { host: "app", path: "/auth/logout", match: "Exact", service: "identity" },
      { host: "gateway", path: "/", match: "PathPrefix", service: "certs" },
      { host: "ota", path: "/", match: "PathPrefix", service: "certs" },
    ]);
    expect(allowed.stderr).toBe("");
    expect(allowed.code).toBe(0);
    const toWeb = withRoutes([{ host: "app", path: "/api/web", match: "PathPrefix", service: "web" }]);
    expect(toWeb.code).not.toBe(0);
    expect(toWeb.stderr).toContain(
      "ingress.routes[0]: web serves only the SPA's static files, which need neither the session cookie nor an internal identity, so it gets no cookie route",
    );
    // The exemption of hosts.app would cover every route of a host with the same name.
    for (const host of ["gateway", "ota"]) {
      const same = helmTemplate([...SHAPES.saas, E2E_VALUES], ["--set", `hosts.${host}=app.saas.trustiot.test`]);
      expect(same.code).not.toBe(0);
      expect(same.stderr).toContain(
        `hosts.${host}: must differ from hosts.app while services.web is enabled, because every path on hosts.app outside /api and /auth skips the cookie check`,
      );
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

  // The route-list proof of the MS0 criterion "with the identity service down the ingress refuses
  // everything but the listed exceptions" (Laysi/trustiot-v2#77, docs/roadmap/ms0-acceptance.md):
  // ext_authz fails closed (deploy/istio, checked below), so a request is refused while the
  // identity service is down exactly when the cookie check applies to it. ingress-authz.ts
  // evaluates the rendered HTTPRoutes and the check the way the gateway does.
  const ROUTE_LIST = [
    { host: "app", path: "/", match: "PathPrefix", service: "assets" },
    { host: "app", path: "/api/identity", match: "PathPrefix", service: "identity" },
    { host: "app", path: "/api/assets", match: "PathPrefix", service: "assets" },
    // Prefixes and paths that overlap the exceptions, on their own host and on the others.
    { host: "app", path: "/auth", match: "PathPrefix", service: "identity" },
    { host: "app", path: "/healthz/details", match: "Exact", service: "identity" },
    { host: "app", path: "/mqtt", match: "PathPrefix", service: "assistant" },
    { host: "gateway", path: "/", match: "PathPrefix", service: "gateway-ops" },
    { host: "gateway", path: "/certs", match: "PathPrefix", service: "certs" },
    { host: "gateway", path: "/healthz", match: "Exact", service: "certs" },
    { host: "gateway", path: "/auth/login", match: "Exact", service: "certs" },
    { host: "ota", path: "/", match: "PathPrefix", service: "gateway-ops" },
    { host: "ota", path: "/mqtt", match: "Exact", service: "mqtt-bridge" },
  ];
  // The same overlaps with the SPA's static files on: cookie routes on hosts.app only under the
  // cookie prefixes, and the public paths' backends also routed on the other hosts.
  const ROUTE_LIST_SPA = [
    { host: "app", path: "/api", match: "PathPrefix", service: "assets" },
    { host: "app", path: "/api/identity", match: "PathPrefix", service: "identity" },
    { host: "app", path: "/api/mqtt", match: "Exact", service: "mqtt-bridge" },
    { host: "app", path: "/auth", match: "PathPrefix", service: "identity" },
    { host: "app", path: "/auth/login/details", match: "Exact", service: "identity" },
    { host: "gateway", path: "/", match: "PathPrefix", service: "gateway-ops" },
    { host: "gateway", path: "/api", match: "PathPrefix", service: "certs" },
    { host: "gateway", path: "/certs", match: "PathPrefix", service: "certs" },
    { host: "gateway", path: "/healthz", match: "Exact", service: "certs" },
    { host: "gateway", path: "/auth/login", match: "Exact", service: "certs" },
    { host: "ota", path: "/", match: "PathPrefix", service: "gateway-ops" },
    { host: "ota", path: "/mqtt", match: "Exact", service: "mqtt-bridge" },
    { host: "ota", path: "/static", match: "PathPrefix", service: "assets" },
  ];
  // What the nightly smoke adds for its API check (scripts/nightly/api.ts, API_ROUTES).
  const SMOKE_API = [
    "--set", "services.identity.enabled=true",
    "--set", "services.assets.enabled=true",
    "--set", "services.certs.enabled=true",
    "--set-json", `ingress.routes=${JSON.stringify([
      { host: "app", path: "/api/identity", match: "PathPrefix", service: "identity" },
      { host: "app", path: "/api/assets", match: "PathPrefix", service: "assets" },
      { host: "app", path: "/api/certs", match: "PathPrefix", service: "certs" },
    ])}`,
  ];
  const ROUTE_LIST_CASES = SHAPE_NAMES.flatMap((shape) => [
    { name: `${shape}: every service, routes that overlap the exceptions`, shape, files: SHAPES[shape], args: [...enableAllServices, ...WEB_OFF, "--set-json", `ingress.routes=${JSON.stringify(ROUTE_LIST)}`] },
    { name: `${shape}: every service and the SPA, routes that overlap the exceptions`, shape, files: SHAPES[shape], args: [...enableAllServices, "--set-json", `ingress.routes=${JSON.stringify(ROUTE_LIST_SPA)}`] },
    { name: `${shape}: the nightly smoke with its API routes`, shape, files: [...SHAPES[shape], SMOKE], args: SMOKE_API },
    { name: `${shape}: the nightly smoke with its API routes and the SPA`, shape, files: [...SHAPES[shape], SMOKE], args: [...SMOKE_API, "--set", "services.web.enabled=true"] },
    { name: `${shape}: the ingress e2e overlay`, shape, files: [...SHAPES[shape], E2E_VALUES], args: [] },
    { name: `${shape}: the ingress e2e overlay without the SPA`, shape, files: [...SHAPES[shape], E2E_VALUES], args: WEB_OFF },
  ]).concat([{ name: "saas: the service-to-service e2e overlay", shape: "saas", files: [...SHAPES.saas, E2E_VALUES, "tests/e2e/values-mesh-e2e.yaml"], args: [] }]);

  test.each(ROUTE_LIST_CASES.map((c) => [c.name, c] as const))(
    "route list, %s: every routed request is behind the cookie check except the listed exceptions",
    (_name, { shape, files, args }) => {
      const manifests = render(files, args);
      // The evaluation reads one hostname per HTTPRoute and one path match per rule; a second one,
      // or a header or method match, would be left out of the proof without failing it.
      for (const route of manifests.filter((m) => m.kind === "HTTPRoute")) expect((route.spec as { hostnames: string[] }).hostnames).toHaveLength(1);
      for (const { rule } of rules(manifests)) expect(rule.matches.map((match) => Object.keys(match))).toEqual([["path"]]);
      const routes: RouteRule[] = rules(manifests).map(({ host, rule }) => ({
        hostname: host,
        path: rule.matches[0]?.path.value ?? "",
        match: rule.matches[0]?.path.type as RouteRule["match"],
        public: strips(rule),
        backend: rule.backendRefs[0]?.name ?? "",
      }));
      // Decision 27's exceptions whose backend this render runs: the others are neither routed nor
      // exempt. With the SPA, every path of hosts.app outside the cookie prefixes is one more.
      const running = new Set(manifests.filter((m) => m.kind === "Deployment").map((m) => m.metadata.name));
      const exceptions: Exception[] = PUBLIC_PATHS[shape].filter(([, , backend]) => running.has(backend)).map(([hostname, path]) => ({ hostname, path }));
      if (running.has(`${RELEASE}-web`)) exceptions.push({ hostname: appHost(shape), outside: COOKIE_PREFIXES });
      expect(exceptions.length).toBeGreaterThan(0);
      const check = policy(manifests, "cookie-check");
      expect(check.action).toBe("CUSTOM");
      const deny = policy(manifests, "deny-internal");
      expect(deny.action).toBe("DENY");
      // The proof lets the identity service refuse a checked request no cookie route matches; it
      // matches the cookie routes it is given, and those are exactly the rendered ones.
      if (running.has(`${RELEASE}-identity`)) {
        const given = podSpec(find(manifests, "Deployment", `${RELEASE}-identity`)).containers[0]?.env?.find((e) => e.name === "TRUSTIOT_INGRESS_ROUTES")?.value;
        expect((JSON.parse(given ?? "null") as { host: string; path: string; match: string }[]).map((r) => `${r.host} ${r.match} ${r.path}`).sort()).toEqual(
          routes.filter((r) => !r.public).map((r) => `${r.hostname} ${r.match} ${r.path}`).sort(),
        );
      }
      const ports = [shape === "saas" ? 80 : 443, 8443];
      expect(routeListProblems(routes, check.rules as PolicyRule[], exceptions, ports, deny.rules as PolicyRule[])).toEqual([]);
      // Not a vacuous pass: the render has cookie routes, and each exception has its route.
      expect(routes.filter((r) => !r.public).length).toBeGreaterThan(0);
      expect(routes.filter((r) => r.public).length).toBe(exceptions.length);
      expect(sampleRequests(routes, exceptions, ports).length).toBeGreaterThan(routes.length);
    },
  );

  // Istio's status port on every gateway pod. Istio fixes it: its kube-gateway injection template
  // probes it (startup and readiness, GET /healthz/ready) and lists it in the gateway's Service as
  // "status-port". Recorded for Istio 1.30.5 in docs/test-reports/2026-10-11-287-gateway-netpol-ports.md;
  // the nightly smoke test compares the live gateway's probe and Service ports with the policy.
  const ISTIO_STATUS_PORT = 15021;

  test("NetworkPolicy: behind the tunnel only the host may connect, to 80 and 15021; direct exposure opens 443 and 15021", () => {
    const tunnel = find(render(SHAPES.saas), "NetworkPolicy", GATEWAY).spec as YamlMap;
    expect(tunnel).toEqual({
      podSelector: { matchLabels: { "gateway.networking.k8s.io/gateway-name": GATEWAY } },
      policyTypes: ["Ingress"],
      ingress: [
        {
          from: [{ ipBlock: { cidr: "10.42.0.1/32" } }],
          ports: [
            { port: 80, protocol: "TCP" },
            { port: 15021, protocol: "TCP" },
          ],
        },
      ],
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

  test.each(SHAPE_NAMES)("%s: every rule of the gateway's NetworkPolicy admits only the Gateway's listener and Istio's status port", (shape) => {
    // Two sources behind the tunnel, so a rule per source would also be caught.
    const args = shape === "saas" ? ["--set-json", 'ingress.tunnel.allowedSources=["10.42.0.1/32","10.42.1.1/32"]'] : [];
    const manifests = render(SHAPES[shape], args);
    const gateway = find(manifests, "Gateway", GATEWAY).spec as {
      listeners: { port: number }[];
      infrastructure: { labels: Record<string, string> };
    };
    const expected = [...gateway.listeners.map((l) => l.port), ISTIO_STATUS_PORT].map((port) => ({ port, protocol: "TCP" }));
    const rules = (find(manifests, "NetworkPolicy", GATEWAY).spec as NetworkPolicySpec).ingress ?? [];
    expect(rules.length).toBeGreaterThan(0);
    // A rule without ports admits every port of the pod, Envoy's metrics included.
    for (const rule of rules) expect(rule.ports).toEqual(expected);
    if (shape === "saas") {
      expect(rules.flatMap((rule) => rule.from ?? [])).toEqual([
        { ipBlock: { cidr: "10.42.0.1/32" } },
        { ipBlock: { cidr: "10.42.1.1/32" } },
      ]);
    }
    // The kubelet's probes come from the host's address, an allowed source, only while the pod
    // stays out of the ambient mesh (Istio labels it istio.io/dataplane-mode: none). In the mesh,
    // they would come from the node source 169.254.7.127, which no rule admits.
    expect(Object.keys(gateway.infrastructure.labels)).not.toContain("istio.io/dataplane-mode");
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
    // The cookie, and the headers of the identity service's cross-site request check (CSRF).
    expect(http.includeRequestHeadersInCheck).toEqual(["cookie", "origin", "sec-fetch-site"]);
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
  rules?: { from: { source: { principals: string[] } }[]; to?: { operation: { ports: string[] } }[] }[];
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
  // The identity service's token exchange listens on a port of its own (Laysi/trustiot-v2#106).
  const EXCHANGE_PORT = 8082;
  // The certs service issues the MQTT broker's server certificate on a port of its own
  // (Laysi/trustiot-v2#557).
  const BROKER_CERTS_PORT = 8082;
  // The AI gateway serves its admin API on a port of its own (Laysi/trustiot-v2#607).
  const AI_GATEWAY_ADMIN_PORT = 8082;
  // An identity (or certs) AuthorizationPolicy rule: these principals, on this port only.
  const identityRule = (port: number, principals: string[]) => ({ from: [{ source: { principals } }], to: [{ operation: { ports: [String(port)] } }] });
  // Test stand-ins that run traefik/whoami, which listens on http only, so their overlays probe
  // http. The identity double (identity in the e2e overlay) listens on the health port like a
  // TrustIoT service.
  const WHOAMI_STAND_INS: Record<string, string[]> = {
    [SMOKE]: ["smoke-echo"],
    [E2E_VALUES]: ["assets", "certs", "mqtt-bridge", "web"],
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
  const podLabels = (workload: Manifest) =>
    workload.kind === "CronJob"
      ? (workload.spec as { jobTemplate: { spec: { template: { metadata: { labels: Record<string, string> } } } } }).jobTemplate.spec.template.metadata.labels
      : (workload.spec as { template: { metadata: { labels: Record<string, string> } } }).template.metadata.labels;
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
    // Deployments by their name, the migration Job (whose name ends in a hash) as "migrate".
    const services = manifests
      .filter((m) => m.kind === "Deployment" || m.kind === "Job")
      .filter((d) => peers.some((peer) => peer.podSelector && selects(peer.podSelector.matchLabels, podLabels(d))))
      .map((d) => (d.kind === "Job" ? "migrate" : serviceOf(d.metadata.name)))
      .sort();
    const gateway = peers.some((peer) => JSON.stringify(peer.podSelector?.matchLabels) === JSON.stringify(GATEWAY_PODS));
    return { services, gateway };
  }

  test("every service runs as its own ServiceAccount", () => {
    const manifests = render([...SHAPES.saas, SMOKE], enableAllServices);
    const accounts = manifests.filter((m) => m.kind === "ServiceAccount").map((m) => m.metadata.name);
    const deployments = manifests.filter((m) => m.kind === "Deployment");
    // The planned services, the smoke service and the OpenTelemetry collector.
    expect(deployments.length).toBe(plannedServices.length + 2);
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

  test("the call list and the ingress routes generate one AuthorizationPolicy per service, and one each for the token exchange and the broker certificate", () => {
    const manifests = withCalls();
    const sa = (service: string) => principal(`${RELEASE}-${service}`);
    // Callers from mesh.calls first (sorted), then the ingress gateway where it routes or, for
    // identity, where it sends the cookie check (ext_authz), on its API port only. The services
    // that call another one on behalf of users reach the identity service through the token
    // exchange's own policy, on the exchange's port only (Laysi/trustiot-v2#106): assets (certs,
    // alerts) and identity itself (assets).
    const expected: Record<string, string[]> = {
      identity: [principal(GATEWAY_SA)],
      "identity-exchange": [sa("assets"), sa("identity")],
      assets: [sa("identity"), principal(GATEWAY_SA)],
      certs: [sa("assets"), principal(GATEWAY_SA)],
      // The MQTT broker's server certificate: the broker alone, which this overlay does not
      // deploy (Laysi/trustiot-v2#557).
      "certs-broker": [],
      "mqtt-bridge": [principal(GATEWAY_SA)],
      // The SPA's static files: the gateway only, like a public path.
      web: [principal(GATEWAY_SA)],
      alerts: [sa("assets")],
      knowledge: [],
    };
    // The rotation CronJob's policy admits nobody (the internal identity tests check it).
    const policies = manifests.filter(
      (m) => m.kind === "AuthorizationPolicy" && !m.metadata.name.startsWith(`${RELEASE}-ingress-`) && m.metadata.name !== `${RELEASE}-identity-key-rotation`,
    );
    expect(policies.map((m) => m.metadata.name).sort()).toEqual(Object.keys(expected).map((s) => `${RELEASE}-${s}`).sort());
    for (const [service, principals] of Object.entries(expected)) {
      const policy = meshPolicy(manifests, service);
      expect(policy.action).toBe("ALLOW");
      // The selector picks exactly this service's pods; the token exchange's, the identity
      // service's, and the broker certificate's, the certs service's.
      const workload = service === "identity-exchange" ? "identity" : service === "certs-broker" ? "certs" : service;
      for (const deployment of manifests.filter((m) => m.kind === "Deployment")) {
        expect(selects(policy.selector.matchLabels, podLabels(deployment))).toBe(serviceOf(deployment.metadata.name) === workload);
      }
      if (principals.length === 0) {
        // An ALLOW policy without rules refuses every connection.
        expect(policy.rules).toBeUndefined();
      } else if (service === "identity" || service === "certs") {
        expect(policy.rules).toEqual([identityRule(API_PORT, principals)]);
      } else if (service === "identity-exchange") {
        expect(policy.rules).toEqual([identityRule(EXCHANGE_PORT, principals)]);
      } else {
        expect(policy.rules).toEqual([{ from: [{ source: { principals } }] }]);
      }
    }
  });

  test.each(SHAPE_NAMES)("%s: each service's NetworkPolicy admits HBONE from the same callers, and kubelet probes", (shape) => {
    const manifests = withCalls(shape);
    const services = manifests.filter((m) => m.kind === "Deployment").map((d) => serviceOf(d.metadata.name));
    expect(services.length).toBe(7);
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

  describe("calls on behalf of users (decision 27, Laysi/trustiot-v2#106)", () => {
    const envOf = (manifests: Manifest[], workload: string) =>
      podSpec(find(manifests, "Deployment", `${RELEASE}-${workload}`)).containers[0]?.env ?? [];
    const onBehalfCalls = (manifests: Manifest[]) => {
      const value = envOf(manifests, "identity").find((e) => e.name === "TRUSTIOT_ON_BEHALF_CALLS")?.value;
      if (value === undefined) throw new Error("the identity service has no TRUSTIOT_ON_BEHALF_CALLS");
      return JSON.parse(value) as Record<string, string[]>;
    };

    test("the identity service exchanges along mesh.calls between enabled services, and every such caller reaches it", () => {
      const calls = {
        // Two services and Postgres: only the services are calls on behalf of users.
        assets: ["certs", "alerts", "postgres"],
        // ai-gateway verifies no internal identity (decision 28): the mesh alone authorizes calls
        // to it, and it holds no user's token to exchange. It keeps `postgres`, without which the
        // render fails.
        assistant: ["ai-gateway", "knowledge"],
        "ai-gateway": ["knowledge", "postgres"],
        // In-cluster components are no service APIs (ingest must list the broker, Laysi/trustiot-v2#721).
        ingest: ["clickhouse", "mqtt-broker"],
        // Disabled services on either side are skipped, as in the mesh plan.
        "gateway-ops": ["certs"],
        certs: ["field-context"],
      };
      const enable = [...["alerts", "knowledge", "assistant", "ai-gateway", "ingest"].flatMap((s) => ["--set", `services.${s}.enabled=true`]), "--set", "mqttBroker.enabled=true", ...AI_GATEWAY_SETTINGS];
      for (const shape of SHAPE_NAMES) {
        const manifests = render([...SHAPES[shape], E2E_VALUES], [...enable, "--set-json", `mesh.calls=${JSON.stringify(calls)}`]);
        expect(onBehalfCalls(manifests)).toEqual({ assets: ["alerts", "certs"], assistant: ["knowledge"] });
        // Both layers of the token exchange admit exactly these callers, on the exchange's port
        // only; the identity service's own policies admit the ingress gateway's cookie check, on
        // the API port only. The NetworkPolicies see HBONE only, so they split the callers, not
        // the ports.
        expect(meshPolicy(manifests, "identity-exchange").rules).toEqual([
          identityRule(EXCHANGE_PORT, [principal(`${RELEASE}-assets`), principal(`${RELEASE}-assistant`)]),
        ]);
        expect(meshPolicy(manifests, "identity").rules).toEqual([identityRule(API_PORT, [principal(GATEWAY_SA)])]);
        const exchangeNetwork = networkPolicy(manifests, "identity-exchange");
        expect(exchangeNetwork.podSelector.matchLabels).toEqual(networkPolicy(manifests, "identity").podSelector.matchLabels);
        expect(hboneSources(manifests, exchangeNetwork)).toEqual({ services: ["assets", "assistant"], gateway: false });
        // HBONE only: the kubelet's probes are the identity service's own NetworkPolicy's.
        expect(exchangeNetwork.ingress).toHaveLength(1);
        expect(hboneSources(manifests, networkPolicy(manifests, "identity"))).toEqual({ services: [], gateway: true });
        // The call list is the identity service's alone: other services and the shared platform
        // settings never see it.
        for (const deployment of manifests.filter(isServiceDeployment)) {
          if (deployment.metadata.name === `${RELEASE}-identity`) continue;
          expect(envOf(manifests, serviceOf(deployment.metadata.name)).map((e) => e.name)).not.toContain("TRUSTIOT_ON_BEHALF_CALLS");
        }
        expect(platformConfig(manifests).TRUSTIOT_ON_BEHALF_CALLS).toBeUndefined();
      }
    });

    test("a caller listed in mesh.calls for the identity service itself reaches both ports, each through its own policies", () => {
      const manifests = render([...SHAPES.saas, E2E_VALUES], ["--set-json", 'mesh.calls={"assets":["identity","certs"]}']);
      expect(onBehalfCalls(manifests)).toEqual({ assets: ["certs", "identity"] });
      expect(meshPolicy(manifests, "identity").rules).toEqual([identityRule(API_PORT, [principal(`${RELEASE}-assets`), principal(GATEWAY_SA)])]);
      expect(meshPolicy(manifests, "identity-exchange").rules).toEqual([identityRule(EXCHANGE_PORT, [principal(`${RELEASE}-assets`)])]);
      expect(hboneSources(manifests, networkPolicy(manifests, "identity"))).toEqual({ services: ["assets"], gateway: true });
      expect(hboneSources(manifests, networkPolicy(manifests, "identity-exchange"))).toEqual({ services: ["assets"], gateway: false });
    });

    // M2 of the 2026-10-21 security review: the cookie check (ext_authz) and the token exchange
    // never share a grant. Each has its own Service, AuthorizationPolicy and NetworkPolicy.
    test.each(SHAPE_NAMES)("%s: the token exchange has its own Service, and the identity service's Service leaves its port out", (shape) => {
      for (const args of [enableAllServices, [...enableAllServices, "--set-json", 'mesh.calls={"assets":["certs"]}']]) {
        const manifests = render(SHAPES[shape], args);
        const ports = (name: string) =>
          (find(manifests, "Service", `${RELEASE}-${name}`).spec as { ports: { name: string; port: number; targetPort: string }[] }).ports.map((p) => [p.name, p.port, p.targetPort]);
        expect(ports("identity")).toEqual([["http", API_PORT, "http"]]);
        expect(ports("identity-exchange")).toEqual([["token-exchange", EXCHANGE_PORT, "token-exchange"]]);
        const exchange = find(manifests, "Service", `${RELEASE}-identity-exchange`);
        expect((exchange.spec as { selector: Record<string, string> }).selector).toEqual(
          (find(manifests, "Service", `${RELEASE}-identity`).spec as { selector: Record<string, string> }).selector,
        );
        expect((exchange.spec as { type: string }).type).toBe("ClusterIP");
        // Its own component label, so the identity service still has one Service of its component.
        expect(exchange.metadata.labels?.["app.kubernetes.io/component"]).toBe("identity-exchange");
        const container = podSpec(find(manifests, "Deployment", `${RELEASE}-identity`)).containers[0];
        expect(container?.ports?.find((p) => p.name === "token-exchange")?.containerPort).toBe(EXCHANGE_PORT);
      }
    });

    test.each(SHAPE_NAMES)("%s: without calls on behalf of users the token exchange's policies admit nobody", (shape) => {
      // The assistant's tool calls are the default values' only ones; lists replace, maps merge.
      const manifests = render(SHAPES[shape], [...enableAllServices, "--set-json", "mesh.calls.assistant=[]"]);
      expect(meshPolicy(manifests, "identity-exchange").rules).toBeUndefined();
      expect(networkPolicy(manifests, "identity-exchange").ingress).toEqual([]);
      // The identity service's own policy keeps the gateway on its API port.
      expect(meshPolicy(manifests, "identity").rules).toEqual([identityRule(API_PORT, [principal(GATEWAY_SA)])]);
    });

    test("without the identity service there is no token exchange", () => {
      const manifests = render(SHAPES.saas, []);
      expect(manifests.some((m) => m.metadata.name === `${RELEASE}-identity-exchange`)).toBe(false);
    });

    // The assistant's tools call telemetry-query and assets as the asking user
    // (Laysi/trustiot-v2#592); ai-gateway takes no internal identity, so it is not on the list.
    test.each(SHAPE_NAMES)("%s: the default values list only the assistant's tool calls on behalf of users", (shape) => {
      expect(onBehalfCalls(render(SHAPES[shape], enableAllServices))).toEqual({ assistant: ["assets", "telemetry-query"] });
      // Without the assistant, or without the services it calls, nothing is left.
      expect(onBehalfCalls(render(SHAPES[shape], ["--set", "services.identity.enabled=true"]))).toEqual({});
      const withoutTools = ["telemetry-query", "assets"].flatMap((s) => ["--set", `services.${s}.enabled=false`]);
      expect(onBehalfCalls(render(SHAPES[shape], [...enableAllServices, ...withoutTools]))).toEqual({});
    });

    // README.md, "呼叫矩陣": the callers of every workload with the default values and every
    // service enabled, so the documented matrix is the rendered one. A change to the default
    // mesh.calls, public paths or ingress.routes updates the table in the same change.
    test.each(SHAPE_NAMES)("%s: README.md's call matrix is what the default values render", (shape) => {
      const readme = readFileSync(join(CHART_DIR, "README.md"), "utf8");
      const section = readme.split("#### 呼叫矩陣")[1]?.split(/\n#{2,4} /)[0] ?? "";
      const EVERY_SERVICE = "每個啟用的服務";
      const NOT_A_SERVICE = "—";
      const names = (cell: string) =>
        cell === "（無）" || cell === NOT_A_SERVICE
          ? []
          : cell === EVERY_SERVICE
            ? [...plannedServices]
            : cell.split("、").map((name) => /^`([a-z-]+)`$/.exec(name)?.[1] ?? `unparsed ${name}`);
      const rows = section
        .split("\n")
        .filter((line) => line.startsWith("| `"))
        .map((line) => line.split("|").slice(1, -1).map((cell) => cell.trim()));
      expect(rows.length).toBeGreaterThan(0);
      const documented = Object.fromEntries(rows.map(([callee = "", callers = "", onBehalf = ""]) => [callee.replaceAll("`", ""), { callers: names(callers).sort(), onBehalf: names(onBehalf).sort(), service: onBehalf !== NOT_A_SERVICE }]));

      const manifests = render(SHAPES[shape], enableAllServices);
      const exchanges = onBehalfCalls(manifests);
      const rendered = Object.fromEntries(
        manifests
          .filter((m) => m.kind === "AuthorizationPolicy" && (m.spec as MeshPolicy).action === "ALLOW")
          .map((m) => {
            const callee = serviceOf(m.metadata.name);
            const callers = principalsOf(m.spec as MeshPolicy).map((p) => serviceOf(p.split("/sa/")[1] ?? "")).sort();
            const onBehalf = Object.entries(exchanges).filter(([, callees]) => callees.includes(callee)).map(([caller]) => caller).sort();
            return [callee, { callers, onBehalf, service: plannedServices.includes(callee) }];
          }),
      );
      expect(documented).toEqual(rendered);
    });
  });

  test("with the default call list only the ingress gateway, the assistant and the MQTT broker reach other services", () => {
    const manifests = render(SHAPES.saas, enableAllServices);
    for (const service of plannedServices) {
      // Public paths keep identity, certs and mqtt-bridge reachable from the gateway, and the
      // SPA's static files keep web. The assistant calls the AI gateway and its tools' services
      // (Laysi/trustiot-v2#592). The broker fetches the CA certificates and CRLs from certs (every
      // service includes ingest, which renders only with the broker, Laysi/trustiot-v2#642).
      const expected =
        service === "certs"
          ? [principal(`${RELEASE}-mqtt-broker`), principal(GATEWAY_SA)]
          : ["identity", "mqtt-bridge", "web"].includes(service)
            ? [principal(GATEWAY_SA)]
            : ["ai-gateway", "assets", "telemetry-query"].includes(service)
              ? [principal(`${RELEASE}-assistant`)]
              : [];
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
          // Workload selector, source principals and, for the identity service's port split,
          // destination ports only, all L4: no targetRefs and no L7 conditions.
          const spec = manifest.spec as MeshPolicy & { targetRefs?: unknown };
          expect(spec.targetRefs).toBeUndefined();
          for (const rule of spec.rules ?? []) {
            expect(JSON.stringify(rule.from)).not.toMatch(/"(to|when)"/);
            expect(Object.keys(rule).sort()).toEqual(rule.to ? ["from", "to"] : ["from"]);
            for (const to of rule.to ?? []) expect(Object.keys(to)).toEqual(["operation"]);
            for (const to of rule.to ?? []) expect(Object.keys(to.operation)).toEqual(["ports"]);
          }
        }
      }
      for (const deployment of manifests.filter((m) => m.kind === "Deployment")) {
        expect(podLabels(deployment)["istio.io/use-waypoint"]).toBeUndefined();
      }
    }
  });

  test.each(SHAPE_NAMES)("%s: bundled ClickHouse admits only ingest, telemetry-query and the collector, and keeps its probes", (shape) => {
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
    // mesh.calls first, then the collector and the migration Job, which the chart adds itself.
    expect(principalsOf(policy)).toEqual([
      principal(`${RELEASE}-ingest`),
      principal(`${RELEASE}-telemetry-query`),
      principal(`${RELEASE}-otel-collector`),
      principal(`${RELEASE}-migrate`),
    ]);
    const spec = networkPolicy(manifests, "clickhouse");
    expect(selects(spec.podSelector.matchLabels ?? {}, labels)).toBe(true);
    expect(hboneSources(manifests, spec)).toEqual({ services: ["ingest", "migrate", "otel-collector", "telemetry-query"], gateway: false });
    // The kubelet's GET /ping probes arrive from istio-cni's SNAT address, on the HTTP port only.
    expect(spec.ingress).toHaveLength(2);
    expect(spec.ingress?.at(-1)).toEqual(hostRule(CLICKHOUSE_PROBE_PORT));
  });

  test("ClickHouse refuses everyone while its callers are off, and gets no policy when external", () => {
    const collectorOnly = render(SHAPES.saas);
    expect(principalsOf(meshPolicy(collectorOnly, "clickhouse"))).toEqual([principal(`${RELEASE}-otel-collector`), principal(`${RELEASE}-migrate`)]);
    const off = render(SHAPES.saas, ["--set", "observability.collector.enabled=false", "--set", "migrate.enabled=false"]);
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
    expect(principalsOf(meshPolicy(manifests, "clickhouse"))).toEqual([
      principal(`${RELEASE}-ingest`),
      principal(`${RELEASE}-otel-collector`),
      principal(`${RELEASE}-migrate`),
    ]);
    expect(hboneSources(manifests, networkPolicy(manifests, "clickhouse")).services).toEqual(["ingest", "migrate", "otel-collector"]);
  });

  // Decision 28: the AI gateway verifies no internal identity and spends the provider accounts, so
  // the mesh alone keeps every service but the AI assistant, the knowledge base and alerts away
  // from it (Laysi/trustiot-v2#536).
  describe("only the assistant, the knowledge base and alerts may call ai-gateway (decision 28)", () => {
    const CALLERS = ["alerts", "assistant", "knowledge"];
    const withGateway = (services: string[]) => [...services.flatMap((s) => ["--set", `services.${s}.enabled=true`]), ...AI_GATEWAY_SETTINGS];

    test.each(SHAPE_NAMES)("%s: both layers admit exactly the listed callers among the three, and nothing else", (shape) => {
      const enable = withGateway([...CALLERS, "ai-gateway", "assets"]);
      const all = Object.fromEntries(CALLERS.map((caller) => [caller, ["ai-gateway"]]));
      const manifests = render([...SHAPES[shape], E2E_VALUES], [...enable, "--set-json", `mesh.calls=${JSON.stringify(all)}`]);
      expect(principalsOf(meshPolicy(manifests, "ai-gateway")).sort()).toEqual(CALLERS.map((c) => principal(`${RELEASE}-${c}`)).sort());
      expect(hboneSources(manifests, networkPolicy(manifests, "ai-gateway"))).toEqual({ services: CALLERS, gateway: false });
      // One caller listed: only that one.
      const one = render([...SHAPES[shape], E2E_VALUES], [...enable, "--set-json", 'mesh.calls={"assistant":["ai-gateway"]}']);
      expect(principalsOf(meshPolicy(one, "ai-gateway"))).toEqual([principal(`${RELEASE}-assistant`)]);
      expect(hboneSources(one, networkPolicy(one, "ai-gateway"))).toEqual({ services: ["assistant"], gateway: false });
      // None listed: no service, and never the ingress gateway (no cookie route may lead to it).
      // Maps merge, so the default values' assistant entry goes with an empty list.
      const none = render([...SHAPES[shape], E2E_VALUES], [...enable, "--set-json", 'mesh.calls={"assistant":[]}']);
      expect(principalsOf(meshPolicy(none, "ai-gateway"))).toEqual([]);
      expect(hboneSources(none, networkPolicy(none, "ai-gateway"))).toEqual({ services: [], gateway: false });
    });

    // The ingress gateway is the other way in: "trustiot.ingress.plan" refuses every cookie route
    // to ai-gateway, on any host and with either match, so the mesh never admits the gateway there.
    test.each(["app", "gateway", "ota"])("no ingress route on hosts.%s may lead to ai-gateway, with or without schema validation", (host) => {
      for (const [path, match] of [["/api/ai", "PathPrefix"], ["/", "PathPrefix"], ["/v1/messages", "Exact"]]) {
        for (const skip of [[], ["--skip-schema-validation"]]) {
          const routes = JSON.stringify([{ host, path, match, service: "ai-gateway" }]);
          // web off, so its SPA rule on hosts.app does not refuse "/" first.
          const args = [...skip, ...withGateway(["ai-gateway"]), "--set", "services.web.enabled=false", "--set-json", `ingress.routes=${routes}`];
          const result = helmTemplate([...SHAPES.saas, E2E_VALUES], args);
          expect(result.code).not.toBe(0);
          expect(result.stderr).toMatch(/ingress\.routes\[0\]: ai-gateway takes no internal identity tokens/);
        }
      }
    });

    const others = plannedServices.filter((service) => !CALLERS.includes(service));
    test.each(others)("%s may not list ai-gateway in mesh.calls, enabled or not, with or without schema validation", (caller) => {
      for (const args of [withGateway([]), withGateway([caller]), ["--skip-schema-validation", ...withGateway([caller])]]) {
        const result = helmTemplate([...SHAPES.saas, E2E_VALUES], [...args, "--set-json", `mesh.calls={"${caller}":["ai-gateway"]}`]);
        expect(result.code).not.toBe(0);
        expect(result.stderr).toMatch(`mesh.calls.${caller}: only alerts, assistant, knowledge may call ai-gateway (decision 28)`);
      }
    });
  });

  // The AI assistant (decision 31, Laysi/trustiot-v2#592): the mesh lets it reach the AI gateway,
  // its tools' services and the identity service's token exchange, and the chart points it only at
  // services the mesh lets it call (README.md, 「AI 助理」).
  describe("the assistant (decision 31, Laysi/trustiot-v2#592)", () => {
    const ASSISTANT = principal(`${RELEASE}-assistant`);
    const ADDRESS_VARIABLES = ["TRUSTIOT__AI_GATEWAY__URL", "TRUSTIOT__ASSETS__URL", "TRUSTIOT__IDENTITY__EXCHANGE_URL", "TRUSTIOT__TELEMETRY_QUERY__URL"];
    type ServiceSpec = { selector: Record<string, string>; ports: { name: string; port: number; targetPort: string | number }[] };
    const assistantEnv = (manifests: Manifest[]) => podSpec(find(manifests, "Deployment", `${RELEASE}-assistant`)).containers[0]?.env ?? [];
    const onBehalfOf = (manifests: Manifest[]) =>
      JSON.parse((podSpec(find(manifests, "Deployment", `${RELEASE}-identity`)).containers[0]?.env ?? []).find((e) => e.name === "TRUSTIOT_ON_BEHALF_CALLS")?.value ?? "null") as Record<string, string[]>;
    // Every URL in the assistant's env, followed through the Service it names to the one workload
    // and container port behind it, with the AuthorizationPolicies that admit the assistant on
    // that port. Read from the rendered objects only, so a new address is checked without a list.
    function addressesOf(manifests: Manifest[]) {
      return assistantEnv(manifests)
        .filter((e) => /^[a-z][a-z0-9+.-]*:\/\//.test(e.value ?? ""))
        .map((e) => {
          const url = new URL(e.value ?? "");
          expect(url.protocol, e.name).toBe("http:");
          // A Service of this release and namespace: its short name or <name>.<namespace>.svc.
          const [name = "", ...domain] = url.hostname.split(".");
          if (domain.length > 0) expect(domain.join("."), e.name).toBe(`${NAMESPACE}.svc`);
          const service = find(manifests, "Service", name).spec as ServiceSpec;
          const servicePort = service.ports.find((p) => p.port === Number(url.port));
          if (servicePort === undefined) throw new Error(`${e.name}: Service ${name} has no port ${url.port}`);
          const workloads = manifests.filter((m) => ["Deployment", "StatefulSet", "DaemonSet"].includes(m.kind) && selects(service.selector, podLabels(m)));
          expect(workloads.map((w) => w.metadata.name), e.name).toHaveLength(1);
          const workload = workloads[0] as Manifest;
          const containerPort =
            typeof servicePort.targetPort === "number"
              ? servicePort.targetPort
              : podSpec(workload)
                  .containers.flatMap((c) => c.ports ?? [])
                  .find((p) => p.name === servicePort.targetPort)?.containerPort;
          if (containerPort === undefined) throw new Error(`${e.name}: ${workload.metadata.name} has no port ${servicePort.targetPort}`);
          const admittedBy = manifests
            .filter((m) => m.kind === "AuthorizationPolicy")
            .filter((m) => {
              const spec = m.spec as MeshPolicy;
              const selector = spec.selector?.matchLabels ?? {};
              return (
                Object.keys(selector).length > 0 &&
                selects(selector, podLabels(workload)) &&
                (spec.rules ?? []).some(
                  (rule) =>
                    (rule.from ?? []).some((f) => (f.source?.principals ?? []).includes(ASSISTANT)) &&
                    (rule.to === undefined || rule.to.some((t) => t.operation.ports.includes(String(containerPort)))),
                )
              );
            })
            .map((m) => serviceOf(m.metadata.name))
            .sort();
          return { variable: e.name, service: serviceOf(name), workload: serviceOf(workload.metadata.name), admittedBy };
        });
    }

    // Acceptance criterion 3: every service the chart gives the assistant an address of is in
    // mesh.calls.assistant, the mesh admits the assistant on the very port of that address, and the
    // token exchange grants every tool's service. [case, extra args, mesh.calls.assistant, the
    // addresses the case leads to].
    const CASES: [string, string[], string[], Record<string, string>][] = [
      [
        "the default values",
        [],
        ["ai-gateway", "telemetry-query", "assets", "postgres"],
        {
          TRUSTIOT__AI_GATEWAY__URL: "ai-gateway",
          TRUSTIOT__ASSETS__URL: "assets",
          TRUSTIOT__IDENTITY__EXCHANGE_URL: "identity-exchange",
          TRUSTIOT__TELEMETRY_QUERY__URL: "telemetry-query",
        },
      ],
      [
        "a list without assets",
        ["--set-json", 'mesh.calls.assistant=["ai-gateway","telemetry-query","postgres"]'],
        ["ai-gateway", "telemetry-query", "postgres"],
        { TRUSTIOT__AI_GATEWAY__URL: "ai-gateway", TRUSTIOT__IDENTITY__EXCHANGE_URL: "identity-exchange", TRUSTIOT__TELEMETRY_QUERY__URL: "telemetry-query" },
      ],
      [
        "telemetry-query disabled",
        ["--set", "services.telemetry-query.enabled=false"],
        ["ai-gateway", "telemetry-query", "assets", "postgres"],
        { TRUSTIOT__AI_GATEWAY__URL: "ai-gateway", TRUSTIOT__ASSETS__URL: "assets", TRUSTIOT__IDENTITY__EXCHANGE_URL: "identity-exchange" },
      ],
      // No call on behalf of users, so no token exchange either.
      ["the AI gateway alone", ["--set-json", 'mesh.calls.assistant=["ai-gateway"]'], ["ai-gateway"], { TRUSTIOT__AI_GATEWAY__URL: "ai-gateway" }],
      ["the AI gateway disabled", ["--set", "services.ai-gateway.enabled=false"], ["ai-gateway", "telemetry-query", "assets", "postgres"], {
        TRUSTIOT__ASSETS__URL: "assets",
        TRUSTIOT__IDENTITY__EXCHANGE_URL: "identity-exchange",
        TRUSTIOT__TELEMETRY_QUERY__URL: "telemetry-query",
      }],
      ["an empty list", ["--set-json", "mesh.calls.assistant=[]"], [], {}],
    ];
    test.each(SHAPE_NAMES)("%s: every address the chart gives the assistant leads to a service in mesh.calls.assistant that admits it there", (shape) => {
      for (const [name, args, listed, expected] of CASES) {
        const manifests = render(SHAPES[shape], [...enableAllServices, ...args]);
        const addresses = addressesOf(manifests);
        expect(Object.fromEntries(addresses.filter((a) => ADDRESS_VARIABLES.includes(a.variable)).map((a) => [a.variable, a.service])), name).toEqual(expected);
        const exchanges = onBehalfOf(manifests).assistant ?? [];
        for (const address of addresses) {
          const where = `${name}: ${address.variable}`;
          if (address.service === "identity-exchange") {
            // The token exchange's port only, never the identity service's API port; and only
            // while the assistant calls a service on behalf of users.
            expect(address.workload, where).toBe("identity");
            expect(address.admittedBy, where).toEqual(["identity-exchange"]);
            expect(exchanges.length, where).toBeGreaterThan(0);
          } else if (plannedServices.includes(address.workload)) {
            expect(listed, where).toContain(address.workload);
            expect(address.admittedBy, where).toEqual([address.workload]);
            // A tool's service is an audience the token exchange grants the assistant.
            if (address.workload !== "ai-gateway") expect(exchanges, where).toContain(address.workload);
          } else {
            // A component the chart lets every service reach (the OpenTelemetry collector).
            expect(address.service, where).toBe("otel-collector");
            expect(address.admittedBy, where).toEqual(["otel-collector"]);
          }
        }
        // The other way: every service on the token exchange's list for the assistant has its
        // address, so the list and the addresses never drift apart.
        for (const callee of exchanges) expect(addresses.map((a) => a.workload), `${name}: ${callee}`).toContain(callee);
      }
    }, SLOW_TEST_MS);

    // Acceptance criterion 1: with the default values the assistant reaches the AI gateway,
    // telemetry-query, assets and the identity service's token exchange port, plus the collector
    // every service sends its telemetry to, and Postgres; nothing else, in either layer.
    test.each(SHAPE_NAMES)("%s: the default values let the assistant reach only ai-gateway, telemetry-query, assets and the token exchange", (shape) => {
      const manifests = render(SHAPES[shape], enableAllServices);
      // The mesh's ALLOW policies; the ingress gateway's own policies admit no service account.
      const admitting = manifests
        .filter((m) => m.kind === "AuthorizationPolicy" && !m.metadata.name.startsWith(`${RELEASE}-ingress-`))
        .filter((m) => (m.spec as MeshPolicy).action === "ALLOW" && principalsOf(m.spec as MeshPolicy).includes(ASSISTANT))
        .map((m) => serviceOf(m.metadata.name))
        .sort();
      expect(admitting).toEqual(["ai-gateway", "assets", "identity-exchange", "otel-collector", "telemetry-query"]);
      // The token exchange's port only; the identity service's own policy keeps the assistant out.
      expect(meshPolicy(manifests, "identity-exchange").rules).toEqual([identityRule(EXCHANGE_PORT, [ASSISTANT])]);
      expect(principalsOf(meshPolicy(manifests, "identity"))).not.toContain(ASSISTANT);
      // NetworkPolicies admit the assistant's pods to the same workloads, on HBONE, and to the
      // bundled Postgres (not in the mesh) on its own port.
      const assistantPods = podLabels(find(manifests, "Deployment", `${RELEASE}-assistant`));
      const reached = manifests
        .filter((m) => m.kind === "NetworkPolicy")
        .flatMap((m) =>
          ((m.spec as NetworkPolicySpec).ingress ?? [])
            .filter((rule) => (rule.from ?? []).some((peer) => peer.podSelector !== undefined && selects(peer.podSelector.matchLabels, assistantPods)))
            .map((rule) => `${serviceOf(m.metadata.name)}:${(rule.ports ?? []).map((p) => p.port).join(",")}`),
        )
        .sort();
      const postgres = manifests.some((m) => m.kind === "NetworkPolicy" && m.metadata.name === `${RELEASE}-postgres`) ? ["postgres:5432"] : [];
      expect(reached).toEqual([...["ai-gateway", "assets", "identity-exchange", "otel-collector", "telemetry-query"].map((s) => `${s}:15008`), ...postgres].sort());
      // Its database login is assistant_app, with the password from its own Secret, and its pool is
      // the size the connection budget counts (Laysi/trustiot-v2#575); its scheduled purge logs in as
      // assistant_platform, with the password from the same Secret (Laysi/trustiot-v2#645).
      expect(assistantEnv(manifests).filter((e) => e.name.startsWith("TRUSTIOT__DATABASE__"))).toEqual([
        { name: "TRUSTIOT__DATABASE__USERNAME", value: "assistant_app" },
        { name: "TRUSTIOT__DATABASE__PASSWORD", valueFrom: { secretKeyRef: { name: `${RELEASE}-assistant-password`, key: "assistant_app" } } },
        { name: "TRUSTIOT__DATABASE__MAX_CONNECTIONS", value: "8" },
        { name: "TRUSTIOT__DATABASE__PLATFORM_USERNAME", value: "assistant_platform" },
        {
          name: "TRUSTIOT__DATABASE__PLATFORM_PASSWORD",
          valueFrom: { secretKeyRef: { name: `${RELEASE}-assistant-password`, key: "assistant_platform" } },
        },
      ]);
    });

    // The addresses are the chart's: custom env could otherwise point the assistant at a service
    // mesh.calls does not list, whether the chart sets that address this time or not.
    test.each(ADDRESS_VARIABLES)("services.assistant.env and serviceDefaults.env cannot set %s, with or without schema validation", (variable) => {
      for (const lists of [[], ["--set-json", "mesh.calls.assistant=[]"]]) {
        for (const field of ["services.assistant.env", "serviceDefaults.env"]) {
          for (const skip of [[], ["--skip-schema-validation"]]) {
            const result = helmTemplate(SHAPES.saas, [...skip, ...enableAllServices, ...lists, "--set-json", `${field}=[{"name":"${variable.toLowerCase()}","value":"http://elsewhere:8080"}]`]);
            expect(result.code, `${field} ${lists.join(" ")}`).not.toBe(0);
            expect(result.stderr).toContain(`services.assistant: env ${variable.toLowerCase()} is set by the chart from mesh.calls.assistant`);
          }
        }
      }
    }, SLOW_TEST_MS);

    test("other services do not get the assistant's addresses, and a callee without an http port fails the render", () => {
      const manifests = render(SHAPES.saas, enableAllServices);
      for (const deployment of manifests.filter(isServiceDeployment)) {
        if (deployment.metadata.name === `${RELEASE}-assistant`) continue;
        const names = podSpec(deployment).containers.flatMap((c) => (c.env ?? []).map((e) => e.name));
        expect(names.filter((n) => ADDRESS_VARIABLES.includes(n)), deployment.metadata.name).toEqual([]);
      }
      const result = helmTemplate(SHAPES.saas, [...enableAllServices, "--set-json", 'services.assets.ports=[{"name":"api","containerPort":8080}]']);
      expect(result.code).not.toBe(0);
      expect(result.stderr).toContain("services.assets.ports: the assistant calls it on the port named http (mesh.calls.assistant), and there is none");
    });
  });

  test.each([
    ["an unknown caller", '{"billing":["assets"]}', /mesh\.calls: unknown caller "billing"/],
    ["a component as the caller", '{"clickhouse":["assets"]}', /mesh\.calls: unknown caller "clickhouse"/],
    ["the collector as the caller", '{"otel-collector":["assets"]}', /mesh\.calls: unknown caller "otel-collector"/],
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
        // Services only: the collector's health_check port is covered by its own tests.
        if (!isServiceDeployment(workload)) continue;
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
        // The identity service also serves its token exchange on a port of its own, which has a
        // Service of its own (Laysi/trustiot-v2#106); the certs service likewise the MQTT broker's
        // server certificate (Laysi/trustiot-v2#557), and the AI gateway its admin API
        // (Laysi/trustiot-v2#607).
        const servedPorts: [string, number][] =
          service === "identity"
            ? [["http", API_PORT], ["token-exchange", EXCHANGE_PORT]]
            : service === "certs"
              ? [["http", API_PORT], ["broker-certs", BROKER_CERTS_PORT]]
              : service === "ai-gateway"
                ? [["http", API_PORT], ["admin", AI_GATEWAY_ADMIN_PORT]]
                : [["http", API_PORT]];
        expect({ label, ports: container?.ports?.map(({ name: port, containerPort }) => ({ name: port, containerPort })) }).toEqual({
          label,
          ports: [...servedPorts.map(([port, containerPort]) => ({ name: port, containerPort })), { name: "health", containerPort: HEALTH_PORT }],
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

  // The node source reaches a pod's probe ports without mTLS or AuthorizationPolicy, so a probe on
  // a port that only its own callers may reach would open it to every process on the node: the
  // identity service's token exchange, the certs service's broker certificate issuance and the AI
  // gateway's admin API. By name or by number, in any probe kind and handler.
  test.each([
    ["identity", "readiness by name", { httpGet: { path: "/readyz", port: "token-exchange" } }, [], /services\.identity: probes\.readiness uses port "token-exchange"/],
    ["identity", "liveness by number", { tcpSocket: { port: 8082 } }, [], /services\.identity: probes\.liveness uses port 8082, the port token-exchange/],
    ["certs", "startup by name", { tcpSocket: { port: "broker-certs" } }, [], /services\.certs: probes\.startup uses port "broker-certs"/],
    ["certs", "readiness by number", { httpGet: { path: "/readyz", port: 8082 } }, [], /services\.certs: probes\.readiness uses port 8082, the port broker-certs/],
    ["ai-gateway", "readiness by name", { httpGet: { path: "/readyz", port: "admin" } }, [], /services\.ai-gateway: probes\.readiness uses port "admin"/],
    ["ai-gateway", "liveness by number", { grpc: { port: 8082 } }, [], /services\.ai-gateway: probes\.liveness uses port 8082, the port admin/],
    [
      "ai-gateway",
      "liveness by the number of a moved admin port",
      { tcpSocket: { port: 9182 } },
      ["--set-json", 'services.ai-gateway.ports=[{"name":"http","containerPort":8080},{"name":"admin","containerPort":9182}]'],
      /services\.ai-gateway: probes\.liveness uses port 9182, the port admin/,
    ],
  ] as [string, string, object, string[], RegExp][])("a %s probe on its port with its own Service and policies fails the render (%s)", (service, label, probe, extra, expected) => {
    const kind = label.split(" ")[0];
    const result = helmTemplate([...SHAPES.saas], [...enableAllServices, ...extra, "--set-json", `services.${service}.probes.${kind}=${JSON.stringify(probe)}`]);
    expect(result.code).not.toBe(0);
    expect(result.stderr).toMatch(expected);
    expect(result.stderr).toMatch(/has a Service and policies of its own: the node reaches a pod's probe ports without mTLS or AuthorizationPolicy/);
  });
});

describe("AI gateway settings (decisions 15 and 28, Laysi/trustiot-v2#536)", () => {
  const CONFIG = `${RELEASE}-ai-gateway`;
  const ON = ["--set", "services.ai-gateway.enabled=true"];
  const gatewayConfig = (manifests: Manifest[]) => (find(manifests, "ConfigMap", CONFIG) as Manifest & { data: Record<string, string> }).data;
  const gatewayContainer = (manifests: Manifest[]) => podSpec(find(manifests, "Deployment", `${RELEASE}-ai-gateway`)).containers[0] as Container;

  test("SaaS: the providers, roles and limits reach the gateway as AI_GATEWAY__ variables in its own ConfigMap, without a key", () => {
    const manifests = render([...SHAPES.saas, SMOKE], ON);
    // tests/values/saas-instance.yaml, and the chart's request timeout and drain.
    expect(gatewayConfig(manifests)).toEqual({
      AI_GATEWAY__PROVIDERS__ANTHROPIC__PROTOCOL: "anthropic-messages",
      AI_GATEWAY__PROVIDERS__ANTHROPIC__KEY_ENV: "TRUSTIOT_AI_ANTHROPIC_API_KEY",
      AI_GATEWAY__PROVIDERS__ANTHROPIC__BASE_URL: "https://ai-gateway.saas.trustiot.test/v1/example/anthropic",
      AI_GATEWAY__PROVIDERS__OPENAI__PROTOCOL: "openai-decisions",
      AI_GATEWAY__PROVIDERS__OPENAI__KEY_ENV: "TRUSTIOT_AI_OPENAI_API_KEY",
      AI_GATEWAY__ROLES__MAIN__MODELS: "anthropic/claude-opus-5-5,anthropic/claude-sonnet-5-5",
      AI_GATEWAY__ROLES__MAIN__QUEUE_WAIT: "5s",
      AI_GATEWAY__ROLES__WORKER__MODELS: "anthropic/claude-haiku-5-5",
      AI_GATEWAY__ROLES__WORKER__MAX_CONCURRENT: "64",
      AI_GATEWAY__ROLES__ROUTER__MODELS: "openai/gpt-6-luna",
      AI_GATEWAY__ROLES__ROUTER__TIMEOUTS__RESPONSE: "30s",
      AI_GATEWAY__HTTP__REQUEST_TIMEOUT: "10m",
      // drainSeconds 310 in values.yaml, less 10 s for the kit's drain delay and its last 3 s.
      AI_GATEWAY__SHUTDOWN__DRAIN_TIMEOUT: "300s",
    });
    const configMap = find(manifests, "ConfigMap", CONFIG);
    expect(configMap.metadata.labels?.["app.kubernetes.io/component"]).toBe("ai-gateway");
    const container = gatewayContainer(manifests);
    expect(container.envFrom).toEqual([{ configMapRef: { name: `${RELEASE}-platform` } }, { configMapRef: { name: CONFIG } }]);
    // The keys come from their Secrets only, in the variables KEY_ENV names; telemetry goes to the
    // collector under the gateway's own prefix.
    const env = container.env ?? [];
    for (const name of ["TRUSTIOT_AI_ANTHROPIC_API_KEY", "TRUSTIOT_AI_OPENAI_API_KEY"]) {
      const entry = env.find((e) => e.name === name);
      expect(entry?.value).toBeUndefined();
      expect(entry?.valueFrom?.secretKeyRef?.key).toBe("api-key");
    }
    expect(env.map((e) => e.name)).toContain("AI_GATEWAY__TELEMETRY__OTLP_ENDPOINT");
    expect(env.map((e) => e.name)).not.toContain("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT");
    // Only the gateway reads the ConfigMap; the AI settings are gone from the platform settings.
    for (const deployment of manifests.filter(isServiceDeployment)) {
      if (deployment.metadata.name === `${RELEASE}-ai-gateway`) continue;
      for (const c of podSpec(deployment).containers) {
        expect(c.envFrom ?? []).not.toContainEqual({ configMapRef: { name: CONFIG } });
        expect((c.env ?? []).map((e) => e.name).filter((n) => n.startsWith("TRUSTIOT_AI_"))).toEqual([]);
      }
    }
    expect(Object.keys(platformConfig(manifests)).filter((name) => name.includes("_AI_"))).toEqual([]);
    // Off: no ConfigMap, and the render does not need any AI setting.
    const off = render([...SHAPES.onprem, SMOKE]);
    expect(off.filter((m) => m.metadata.name === CONFIG)).toEqual([]);
  });

  test("on-prem: a provider without a Secret is left out, and its own API is the default endpoint", () => {
    const manifests = render([...SHAPES.onprem, SMOKE], [...ON, ...AI_GATEWAY_SETTINGS]);
    expect(gatewayConfig(manifests)).toEqual({
      AI_GATEWAY__PROVIDERS__ANTHROPIC__PROTOCOL: "anthropic-messages",
      AI_GATEWAY__PROVIDERS__ANTHROPIC__KEY_ENV: "TRUSTIOT_AI_ANTHROPIC_API_KEY",
      AI_GATEWAY__ROLES__TEST__MODELS: "anthropic/claude-test",
      AI_GATEWAY__HTTP__REQUEST_TIMEOUT: "10m",
      AI_GATEWAY__SHUTDOWN__DRAIN_TIMEOUT: "300s",
    });
    // The catalog leaves the unconfigured provider's key out: no reference to a Secret without a name.
    expect((gatewayContainer(manifests).env ?? []).map((e) => e.name).filter((n) => n.startsWith("TRUSTIOT_AI_"))).toEqual(["TRUSTIOT_AI_ANTHROPIC_API_KEY"]);
  });

  test("every limit of a role reaches its variable, and the pods roll when the settings change", () => {
    const role = {
      models: ["anthropic/claude-a", "openai/gpt-b"],
      maxConcurrent: 10000,
      queueWait: "1500ms",
      timeouts: { connect: "5s", firstToken: "2m", streamIdle: "90s", streamTotal: "24h", response: "8m" },
      maxResponseBytes: 268435456,
      maxEventBytes: 1024,
    };
    const args = [...ON, "--set-json", `ai.roles.full=${JSON.stringify(role)}`, "--set-json", "ai.requestTimeout=\"1h\""];
    const manifests = render([...SHAPES.saas, SMOKE], args);
    const data = gatewayConfig(manifests);
    expect(Object.fromEntries(Object.entries(data).filter(([name]) => name.startsWith("AI_GATEWAY__ROLES__FULL__")))).toEqual({
      AI_GATEWAY__ROLES__FULL__MODELS: "anthropic/claude-a,openai/gpt-b",
      AI_GATEWAY__ROLES__FULL__MAX_CONCURRENT: "10000",
      AI_GATEWAY__ROLES__FULL__QUEUE_WAIT: "1500ms",
      AI_GATEWAY__ROLES__FULL__TIMEOUTS__CONNECT: "5s",
      AI_GATEWAY__ROLES__FULL__TIMEOUTS__FIRST_TOKEN: "2m",
      AI_GATEWAY__ROLES__FULL__TIMEOUTS__STREAM_IDLE: "90s",
      AI_GATEWAY__ROLES__FULL__TIMEOUTS__STREAM_TOTAL: "24h",
      AI_GATEWAY__ROLES__FULL__TIMEOUTS__RESPONSE: "8m",
      AI_GATEWAY__ROLES__FULL__MAX_RESPONSE_BYTES: "268435456",
      AI_GATEWAY__ROLES__FULL__MAX_EVENT_BYTES: "1024",
    });
    expect(data.AI_GATEWAY__HTTP__REQUEST_TIMEOUT).toBe("1h");
    const checksum = (m: Manifest[]) =>
      (find(m, "Deployment", `${RELEASE}-ai-gateway`).spec as { template: { metadata: { annotations: Record<string, string> } } }).template.metadata.annotations[
        "checksum/ai-gateway-config"
      ];
    const before = checksum(render([...SHAPES.saas, SMOKE], ON));
    expect(before).toMatch(/^[0-9a-f]{64}$/);
    expect(checksum(manifests)).not.toBe(before);
    expect(checksum(render([...SHAPES.saas, SMOKE], ON))).toBe(before);
  });

  // The gateway refuses to start when queueWait plus the longer of timeouts.response and
  // timeouts.firstToken exceeds http.request_timeout less a tenth of it, at most 2 s
  // (services/ai-gateway/src/gateway.rs). The chart refuses the same values, to the millisecond.
  // On-prem has no roles of its own, so the role under test is the only one.
  const oneRole = (role: object, requestTimeout: string) => [
    ...ON,
    "--set",
    "ai.providers.anthropic.apiKeySecret.name=trustiot-anthropic",
    "--set-json",
    `ai.roles.edge=${JSON.stringify(role)}`,
    "--set-json",
    `ai.requestTimeout=${JSON.stringify(requestTimeout)}`,
    "--skip-schema-validation",
  ];
  test.each([
    ["10m", "298s", true],
    ["10m", "299s", false],
    ["1s", "100ms", false],
  ])("requestTimeout %s with queueWait %s on the default limits: renders %p", (requestTimeout, queueWait, renders) => {
    const result = helmTemplate([...SHAPES.onprem, SMOKE], oneRole({ models: ["anthropic/claude-a"], queueWait }, requestTimeout));
    if (renders) expect(result.stderr).toBe("");
    else expect(result.stderr).toMatch(/ai\.roles\.edge: queueWait plus the longer of timeouts\.response/);
    expect(result.code === 0).toBe(renders);
  });

  test("a short request timeout fits a role with short limits: the margin is a tenth, rounded up to the millisecond", () => {
    // 1s keeps 100 ms: 900 ms for 400 ms in the queue and 500 ms to the first token.
    const role = { models: ["anthropic/claude-a"], queueWait: "400ms", timeouts: { firstToken: "500ms", response: "500ms" } };
    const fits = render([...SHAPES.onprem, SMOKE], oneRole(role, "1s"));
    expect(gatewayConfig(fits).AI_GATEWAY__ROLES__EDGE__QUEUE_WAIT).toBe("400ms");
    const over = helmTemplate([...SHAPES.onprem, SMOKE], oneRole({ ...role, queueWait: "401ms" }, "1s"));
    expect(over.stderr).toMatch(/is 901 ms, more than the 900 ms ai\.requestTimeout leaves/);
  });

  // The schema takes every request timeout the template takes, in each unit, up to one hour.
  test.each([
    ["1500ms", true],
    ["3600000ms", true],
    ["3600s", true],
    ["60m", true],
    ["1h", true],
    ["3600001ms", false],
    ["0ms", false],
    ["61m", false],
  ])("the schema validates ai.requestTimeout %s: renders %p", (requestTimeout, renders) => {
    const role = { models: ["anthropic/claude-a"], timeouts: { firstToken: "100ms", response: "100ms" } };
    const args = oneRole(role, requestTimeout).filter((arg) => arg !== "--skip-schema-validation");
    const result = helmTemplate([...SHAPES.onprem, SMOKE], args);
    if (renders) expect(result.stderr).toBe("");
    else expect(result.stderr).toMatch(/\/ai\/requestTimeout/);
    expect(result.code === 0).toBe(renders);
  });

  // What the gateway would refuse at startup fails the render even without schema validation, so
  // a values mistake never ships as a crash-looping pod.
  const cases: [string, readonly string[], string[], RegExp][] = [
    ["no role", SHAPES.onprem, ["--set", "ai.providers.anthropic.apiKeySecret.name=trustiot-anthropic"], /ai\.roles: the AI gateway needs at least one model role/],
    [
      "a model of a provider without a key",
      SHAPES.onprem,
      [...AI_GATEWAY_SETTINGS, "--set-json", 'ai.roles.other={"models":["openai/gpt-x"]}'],
      /ai\.roles\.other\.models: openai\/gpt-x needs ai\.providers\.openai\.apiKeySecret\.name/,
    ],
    ["credentials without ai", SHAPES.saas, ["--set-json", "services.ai-gateway.credentials=[]"], /services\.ai-gateway\.credentials must list ai/],
    ["a provider the chart does not know", SHAPES.saas, ["--set", "ai.providers.gemini.apiKeySecret.name=trustiot-gemini"], /ai\.providers\.gemini: unknown provider; the chart knows anthropic, openai/],
    ["a base URL with user info", SHAPES.saas, ["--set", "ai.providers.openai.baseUrl=https://user@llm.saas.trustiot.test/v1"], /ai\.providers\.openai\.baseUrl must be an https URL/],
    ["a base URL over plain http", SHAPES.saas, ["--set", "ai.providers.openai.baseUrl=http://llm.saas.trustiot.test/v1"], /ai\.providers\.openai\.baseUrl must be an https URL/],
    ["a base URL with a fragment", SHAPES.saas, ["--set", "ai.providers.openai.baseUrl=https://llm.saas.trustiot.test/v1#x"], /ai\.providers\.openai\.baseUrl must be an https URL/],
    ["a base URL with a port the URL parser refuses", SHAPES.saas, ["--set", "ai.providers.openai.baseUrl=https://llm.saas.trustiot.test:65536/v1"], /ai\.providers\.openai\.baseUrl must be an https URL/],
    ["a role name in upper case", SHAPES.saas, ["--set-json", 'ai.roles.Main={"models":["anthropic/claude-x"]}'], /ai\.roles\.Main: a role name is 1 to 32 lower-case letters/],
    ["a role without models", SHAPES.saas, ["--set-json", 'ai.roles.empty={"models":[]}'], /ai\.roles\.empty\.models must list 1 to 8 models/],
    [
      "a role with nine models",
      SHAPES.saas,
      ["--set-json", `ai.roles.many={"models":${JSON.stringify(Array.from({ length: 9 }, (_, i) => `anthropic/m${i}`))}}`],
      /ai\.roles\.many\.models must list 1 to 8 models/,
    ],
    ["a model listed twice", SHAPES.saas, ["--set-json", 'ai.roles.twice={"models":["anthropic/a","anthropic/a"]}'], /ai\.roles\.twice\.models lists a model twice/],
    ["a model with a comma", SHAPES.saas, ["--set-json", 'ai.roles.comma={"models":["anthropic/a,b"]}'], /ai\.roles\.comma\.models: each model is <provider>\/<model id>/],
    ["a model with a space", SHAPES.saas, ["--set-json", 'ai.roles.space={"models":["anthropic/a b"]}'], /ai\.roles\.space\.models: each model is/],
    ["a model without a provider", SHAPES.saas, ["--set-json", 'ai.roles.bare={"models":["claude-x"]}'], /ai\.roles\.bare\.models: each model is/],
    ["an unknown role setting", SHAPES.saas, ["--set-json", 'ai.roles.main.temperature=1'], /ai\.roles\.main\.temperature: unknown setting/],
    ["an unknown time limit", SHAPES.saas, ["--set-json", 'ai.roles.main.timeouts={"total":"1m"}'], /ai\.roles\.main\.timeouts\.total: unknown time limit/],
    ["a time limit of zero", SHAPES.saas, ["--set-json", 'ai.roles.main.timeouts={"connect":"0s"}'], /ai\.roles\.main\.timeouts\.connect must be more than 0 and at most 1h/],
    ["a response limit over an hour", SHAPES.saas, ["--set-json", 'ai.roles.main.timeouts={"response":"61m"}'], /ai\.roles\.main\.timeouts\.response must be more than 0 and at most 1h/],
    ["a stream over a day", SHAPES.saas, ["--set-json", 'ai.roles.main.timeouts={"streamTotal":"25h"}'], /ai\.roles\.main\.timeouts\.streamTotal must be more than 0 and at most 24h/],
    ["a duration with two units, which the chart does not take", SHAPES.saas, ["--set-json", 'ai.roles.main.queueWait="1m30s"'], /ai\.roles\.main\.queueWait must be a duration such as 30s/],
    ["a duration as a number", SHAPES.saas, ["--set-json", "ai.roles.main.queueWait=5"], /ai\.roles\.main\.queueWait must be a duration/],
    ["a queue wait over an hour", SHAPES.saas, ["--set-json", 'ai.roles.main.queueWait="2h"', "--set-json", 'ai.requestTimeout="1h"'], /ai\.roles\.main\.queueWait must be at most 1h/],
    ["no slot per model", SHAPES.saas, ["--set-json", "ai.roles.main.maxConcurrent=0"], /ai\.roles\.main\.maxConcurrent must be a whole number from 1 to 10000/],
    ["more slots than the gateway allows", SHAPES.saas, ["--set-json", "ai.roles.main.maxConcurrent=10001"], /ai\.roles\.main\.maxConcurrent must be a whole number from 1 to 10000/],
    ["a response cap under 1 KiB", SHAPES.saas, ["--set-json", "ai.roles.main.maxResponseBytes=1023"], /ai\.roles\.main\.maxResponseBytes must be a whole number from 1024 to 268435456/],
    [
      "an event cap over the default response cap",
      SHAPES.saas,
      ["--set-json", "ai.roles.main.maxEventBytes=33554433"],
      /ai\.roles\.main\.maxEventBytes \(33554433\) must not exceed maxResponseBytes \(33554432; 32 MiB unless set\)/,
    ],
    ["a request timeout over an hour", SHAPES.saas, ["--set-json", 'ai.requestTimeout="2h"'], /ai\.requestTimeout must be more than 0 and at most 1h/],
    ["a request timeout of zero", SHAPES.saas, ["--set-json", 'ai.requestTimeout="0s"'], /ai\.requestTimeout must be more than 0 and at most 1h/],
    ["a drain too short for the kit's own drain", SHAPES.saas, ["--set", "services.ai-gateway.drainSeconds=10"], /services\.ai-gateway\.drainSeconds must be 11 to 3605/],
    [
      "custom env that sets a role",
      SHAPES.saas,
      ["--set-json", 'services.ai-gateway.env=[{"name":"AI_GATEWAY__ROLES__MAIN__MODELS","value":"openai/x"}]'],
      /services\.ai-gateway: env AI_GATEWAY__ROLES__MAIN__MODELS is set by the chart from values\.ai/,
    ],
    [
      "custom env that adds a provider",
      SHAPES.saas,
      ["--set-json", 'services.ai-gateway.env=[{"name":"ai_gateway__providers__evil__base_url","value":"https://evil.test"}]'],
      /services\.ai-gateway: env ai_gateway__providers__evil__base_url is set by the chart/,
    ],
    [
      "custom env that overrides a setting of the ConfigMap",
      SHAPES.saas,
      ["--set-json", 'services.ai-gateway.env=[{"name":"AI_GATEWAY__HTTP__REQUEST_TIMEOUT","value":"1h"}]'],
      /services\.ai-gateway: env AI_GATEWAY__HTTP__REQUEST_TIMEOUT is set by the chart/,
    ],
    [
      "custom env that names a settings file",
      SHAPES.saas,
      ["--set-json", 'services.ai-gateway.env=[{"name":"AI_GATEWAY_CONFIG_FILE","value":"/tmp/x.toml"}]'],
      /services\.ai-gateway: env AI_GATEWAY_CONFIG_FILE is set by the chart/,
    ],
    [
      "custom env that replaces a provider key",
      SHAPES.saas,
      ["--set-json", 'services.ai-gateway.env=[{"name":"TRUSTIOT_AI_OPENAI_API_KEY","value":"x"}]'],
      /services\.ai-gateway: env TRUSTIOT_AI_OPENAI_API_KEY: names starting with TRUSTIOT_, SENTRY_ or OTEL_ are set by the chart/,
    ],
    [
      "custom env that moves the gateway's database",
      SHAPES.saas,
      ["--set-json", 'services.ai-gateway.env=[{"name":"AI_GATEWAY_POSTGRES_HOST","value":"elsewhere.test"}]'],
      /services\.ai-gateway: env AI_GATEWAY_POSTGRES_HOST is set by the chart/,
    ],
    [
      "custom env that replaces the gateway's database login",
      SHAPES.saas,
      ["--set-json", 'services.ai-gateway.env=[{"name":"AI_GATEWAY__DATABASE__USERNAME","value":"postgres"}]'],
      /services\.ai-gateway: env AI_GATEWAY__DATABASE__USERNAME is set by the chart: where Postgres is, the service's login roles/,
    ],
    [
      "custom env that replaces the gateway's database password",
      SHAPES.saas,
      ["--set-json", 'services.ai-gateway.env=[{"name":"ai_gateway__database__password","valueFrom":{"secretKeyRef":{"name":"elsewhere","key":"k"}}}]'],
      /services\.ai-gateway: env ai_gateway__database__password is set by the chart/,
    ],
    [
      "custom env that redirects the gateway's telemetry",
      SHAPES.saas,
      ["--set-json", 'services.ai-gateway.env=[{"name":"AI_GATEWAY__TELEMETRY__OTLP_ENDPOINT","value":"http://elsewhere:4318"}]'],
      /services\.ai-gateway: env AI_GATEWAY__TELEMETRY__OTLP_ENDPOINT is set by the chart/,
    ],
    // The admin API's address is the chart's (Laysi/trustiot-v2#607): 0.0.0.0 would let every
    // caller of the gateway reach it by the pod's address before its policies exist, and a moved
    // port would leave the policies on the old one. Refused with and without the identity service.
    [
      "custom env that opens the admin API",
      SHAPES.saas,
      ["--set-json", 'services.ai-gateway.env=[{"name":"AI_GATEWAY__ADMIN__LISTEN_ADDR","value":"0.0.0.0:8082"}]'],
      /services\.ai-gateway: env AI_GATEWAY__ADMIN__LISTEN_ADDR is set by the chart: the admin API changes every tenant's models/,
    ],
    [
      "custom env for every service that opens the gateway's admin API, in lower case",
      SHAPES.saas,
      ["--set", "services.identity.enabled=true", "--set-json", 'serviceDefaults.env=[{"name":"ai_gateway__admin__listen_addr","value":"0.0.0.0:8082"}]'],
      /services\.ai-gateway: env ai_gateway__admin__listen_addr is set by the chart: the admin API changes every tenant's models/,
    ],
    [
      "custom env that sets any other admin API setting",
      SHAPES.onprem,
      [...AI_GATEWAY_SETTINGS, "--set-json", 'services.ai-gateway.env=[{"name":"AI_GATEWAY__ADMIN__ANYTHING","value":"x"}]'],
      /services\.ai-gateway: env AI_GATEWAY__ADMIN__ANYTHING is set by the chart/,
    ],
    [
      "a gateway without the admin port while the identity service calls it",
      SHAPES.saas,
      ["--set", "services.identity.enabled=true", "--set-json", 'services.ai-gateway.ports=[{"name":"http","containerPort":8080}]'],
      /services\.ai-gateway\.ports: the admin API needs the port named admin .* because identity calls it \(Laysi\/trustiot-v2#607\)/,
    ],
    [
      "a gateway with the admin port only",
      SHAPES.saas,
      ["--set-json", 'services.ai-gateway.ports=[{"name":"admin","containerPort":8082}]'],
      /services\.ai-gateway\.ports: the API port \(http\) is missing; admin serves the admin API only/,
    ],
    [
      "a service named after the admin API's Service and policies",
      SHAPES.saas,
      ["--set", "services.ai-gateway-admin.enabled=true"],
      /services\.ai-gateway-admin: the name belongs to the AI gateway's admin API/,
    ],
    [
      "an ingress route to the gateway's admin port",
      SHAPES.saas,
      ["--set", "services.identity.enabled=true", "--set-json", 'ingress.routes=[{"host":"app","path":"/api/ai","match":"PathPrefix","service":"ai-gateway","port":"admin"}]'],
      /ingress\.routes\[0\]: ai-gateway takes no internal identity tokens .* its admin port is the identity service's alone/,
    ],
    [
      "an ingress route to the gateway's API port",
      SHAPES.saas,
      ["--set-json", 'ingress.routes=[{"host":"app","path":"/api/ai","match":"PathPrefix","service":"ai-gateway","port":"http"}]'],
      /ingress\.routes\[0\]: ai-gateway takes no internal identity tokens/,
    ],
    // Without its database the gateway refuses to start (services/ai-gateway/README.md, 「資料庫」).
    [
      "a gateway without postgres under mesh.calls",
      SHAPES.saas,
      ["--set-json", "mesh.calls.ai-gateway=[]"],
      /mesh\.calls\.ai-gateway must list postgres: the gateway keeps the model roles' settings there/,
    ],
    [
      "an on-premises gateway without postgres under mesh.calls",
      SHAPES.onprem,
      [...AI_GATEWAY_SETTINGS, "--set-json", "mesh.calls.ai-gateway=[]"],
      /mesh\.calls\.ai-gateway must list postgres/,
    ],
  ];

  test.each(cases)("the templates refuse %s, without schema validation", (_name, files, args, expected) => {
    const result = helmTemplate([...files, SMOKE], ["--skip-schema-validation", ...ON, ...args]);
    expect(result.code).not.toBe(0);
    expect(result.stderr).toMatch(expected);
  });

  test("custom env may still tune what the chart leaves to the gateway's defaults", () => {
    const manifests = render([...SHAPES.saas, SMOKE], [
      ...ON,
      "--set-json",
      'services.ai-gateway.env=[{"name":"AI_GATEWAY__MAX_REQUEST_BYTES","value":"1048576"}]',
    ]);
    expect(gatewayContainer(manifests).env).toContainEqual({ name: "AI_GATEWAY__MAX_REQUEST_BYTES", value: "1048576" });
  });

  // values.yaml lists postgres for the gateway under mesh.calls (Laysi/trustiot-v2#605): it reads
  // the platform's Postgres settings under its own prefix, logs in as its own role with the
  // password the migration Job generates, and gets the server's CA like every Postgres client.
  test.each([
    ["bundled", [] as string[], "trustiot-postgres-rw.trustiot.svc", "5432", "trustiot"],
    [
      "external",
      [
        "--set",
        "postgres.mode=external",
        "--set",
        "postgres.external.host=pg.saas.trustiot.test",
        "--set",
        "postgres.external.port=6432",
        "--set",
        "postgres.external.database=gateway_db",
        "--set",
        "postgres.external.caSecret.name=trustiot-pg-ca",
        ...EXTERNAL_MIGRATION,
      ],
      "pg.saas.trustiot.test",
      "6432",
      "gateway_db",
    ],
  ])("%s Postgres: the gateway gets its settings, role and password under its own prefix", (_mode, args, host, port, database) => {
    const manifests = render([...SHAPES.saas, SMOKE], [...ON, ...args]);
    const env = gatewayContainer(manifests).env ?? [];
    const named = (name: string) => env.find((e) => e.name === name);
    const platform = find(manifests, "ConfigMap", `${RELEASE}-platform`).data as Record<string, string>;
    expect(platform.TRUSTIOT_POSTGRES_HOST).toBe(host);
    for (const [setting, value] of [
      ["HOST", host],
      ["PORT", port],
      ["DATABASE", database],
      ["SSLMODE", "verify-full"],
    ]) {
      expect(named(`AI_GATEWAY_POSTGRES_${setting}`), setting).toEqual({ name: `AI_GATEWAY_POSTGRES_${setting}`, value });
      // The same place the platform ConfigMap names for every other service.
      expect(platform[`TRUSTIOT_POSTGRES_${setting}`], setting).toBe(value);
    }
    expect(named("AI_GATEWAY_POSTGRES_CA_FILE")).toEqual({ name: "AI_GATEWAY_POSTGRES_CA_FILE", value: "/etc/trustiot/postgres-ca/ca.crt" });
    expect(gatewayContainer(manifests).volumeMounts).toContainEqual({ name: "postgres-ca", mountPath: "/etc/trustiot/postgres-ca", readOnly: true });
    expect(named("AI_GATEWAY__DATABASE__USERNAME")).toEqual({ name: "AI_GATEWAY__DATABASE__USERNAME", value: "ai_gateway_app" });
    expect(named("AI_GATEWAY__DATABASE__PASSWORD")).toEqual({
      name: "AI_GATEWAY__DATABASE__PASSWORD",
      valueFrom: { secretKeyRef: { name: `${RELEASE}-ai-gateway-password`, key: "ai_gateway_app" } },
    });
    // No platform role, and nothing under the names the gateway does not read.
    expect(env.filter((e) => /^(TRUSTIOT__DATABASE__|TRUSTIOT_POSTGRES_|AI_GATEWAY__DATABASE__PLATFORM_)/.test(e.name))).toEqual([]);
    // The migration Job generates the password into the gateway's own Secret.
    expect(find(manifests, "Secret", `${RELEASE}-ai-gateway-password`)).toBeDefined();
    const jobs = JSON.stringify(manifests.filter((m) => m.kind === "Job"));
    expect(jobs).toContain(`ai_gateway_app=${RELEASE}-ai-gateway-password`);
  });

  // Laysi/trustiot-v2#607: the admin API changes which models every tenant's assistant uses, so
  // only the identity service, which checks the platform administrator, reaches its port; the
  // services that forward model calls reach the API port only (templates/ai-gateway-admin.yaml,
  // the same split as templates/certs-broker.yaml).
  describe("admin API (Laysi/trustiot-v2#607)", () => {
    const GATEWAY = `${RELEASE}-ai-gateway`;
    const ADMIN = `${RELEASE}-ai-gateway-admin`;
    const ADMIN_SERVICE_PORT = 8082;
    const ADMIN_URL = `http://${ADMIN}.${NAMESPACE}.svc:${ADMIN_SERVICE_PORT}`;
    const principal = (service: string) => `cluster.local/ns/${NAMESPACE}/sa/${RELEASE}-${service}`;
    const podSelector = (service: string) => ({ "app.kubernetes.io/name": "trustiot", "app.kubernetes.io/instance": RELEASE, "app.kubernetes.io/component": service });
    const policy = (manifests: Manifest[], kind: "AuthorizationPolicy" | "NetworkPolicy", name: string) => find(manifests, kind, name).spec as MeshPolicy & NetworkPolicySpec;
    const servicePorts = (manifests: Manifest[], name: string) =>
      (find(manifests, "Service", name).spec as { ports: { name: string; port: number; targetPort: string }[] }).ports.map((p) => [p.name, p.port, p.targetPort]);
    const envOf = (manifests: Manifest[], name: string) => podSpec(find(manifests, "Deployment", name)).containers.flatMap((c) => c.env ?? []);
    // Every service, and every one that may forward model calls through the gateway does.
    const FORWARDERS = ["alerts", "assistant", "knowledge"];
    const allForwarding = [
      ...enableAllServices,
      "--set-json",
      'mesh.calls.assistant=["ai-gateway","postgres"]',
      "--set-json",
      'mesh.calls.alerts=["ai-gateway"]',
      "--set-json",
      'mesh.calls.knowledge=["ai-gateway"]',
    ];

    test.each(SHAPE_NAMES)("%s: the identity service alone reaches the admin port, which the gateway's Service leaves out; its callers reach the API port only", (shape) => {
      const manifests = render(SHAPES[shape], allForwarding);
      const gatewayLabels = (find(manifests, "Deployment", GATEWAY).spec as { template: { metadata: { labels: Record<string, string> } } }).template.metadata.labels;
      const onGateway = (selector: Record<string, string>) => Object.entries(selector).every(([key, value]) => gatewayLabels[key] === value);

      // The gateway's own policy: the forwarding services on the API port only.
      expect(policy(manifests, "AuthorizationPolicy", GATEWAY).rules).toEqual([
        { from: [{ source: { principals: FORWARDERS.map(principal) } }], to: [{ operation: { ports: ["8080"] } }] },
      ]);
      // The admin API's policy, on the gateway's pods: the identity service on the admin port only.
      const admin = policy(manifests, "AuthorizationPolicy", ADMIN);
      expect(admin.action).toBe("ALLOW");
      expect(onGateway(admin.selector.matchLabels)).toBe(true);
      expect(admin.rules).toEqual([{ from: [{ source: { principals: [principal("identity")] } }], to: [{ operation: { ports: [String(ADMIN_SERVICE_PORT)] } }] }]);
      // Across every AuthorizationPolicy on the gateway's pods (one with neither selector nor
      // targetRefs applies to every workload, so it counts): ALLOW policies add up, so a rule
      // without ports, or with the admin port, decides who reaches the admin API, and only the
      // identity service's does; the identity service does not reach the API port.
      const onGatewayPolicies = manifests.filter((m) => {
        if (m.kind !== "AuthorizationPolicy") return false;
        const spec = m.spec as { selector?: { matchLabels?: Record<string, string> }; targetRefs?: unknown[] };
        if (spec.targetRefs !== undefined) return false;
        return onGateway(spec.selector?.matchLabels ?? {});
      });
      expect(onGatewayPolicies.map((m) => m.metadata.name).sort()).toEqual([GATEWAY, ADMIN]);
      const reaching = (port: number) =>
        onGatewayPolicies
          .flatMap((m) => (m.spec as MeshPolicy).rules ?? [])
          .flatMap((rule) => {
            const ports = rule.to?.flatMap((to) => to.operation.ports);
            return ports === undefined || ports.includes(String(port)) ? rule.from.flatMap((from) => from.source.principals) : [];
          })
          .sort();
      expect(reaching(ADMIN_SERVICE_PORT)).toEqual([principal("identity")]);
      expect(reaching(8080)).toEqual(FORWARDERS.map(principal));
      // The NetworkPolicy sees HBONE only: it admits the identity service's pods.
      const network = policy(manifests, "NetworkPolicy", ADMIN);
      expect(network.podSelector).toEqual(policy(manifests, "NetworkPolicy", GATEWAY).podSelector);
      expect(network.policyTypes).toEqual(["Ingress"]);
      expect(network.ingress).toEqual([{ from: [{ podSelector: { matchLabels: podSelector("identity") } }], ports: [{ port: 15008, protocol: "TCP" }] }]);
      const identityLabels = (find(manifests, "Deployment", `${RELEASE}-identity`).spec as { template: { metadata: { labels: Record<string, string> } } }).template.metadata.labels;
      expect(Object.entries(podSelector("identity")).every(([key, value]) => identityLabels[key] === value)).toBe(true);

      // The gateway's Service leaves the port out; its own Service has it alone, on the gateway's pods.
      expect(servicePorts(manifests, GATEWAY)).toEqual([["http", 8080, "http"]]);
      expect(servicePorts(manifests, ADMIN)).toEqual([["admin", ADMIN_SERVICE_PORT, "admin"]]);
      const adminService = find(manifests, "Service", ADMIN);
      expect((adminService.spec as { type: string }).type).toBe("ClusterIP");
      expect((adminService.spec as { selector: Record<string, string> }).selector).toEqual((find(manifests, "Service", GATEWAY).spec as { selector: Record<string, string> }).selector);
      expect(adminService.metadata.labels?.["app.kubernetes.io/component"]).toBe("ai-gateway-admin");

      // The gateway listens on the pod's address, where the Service points.
      const container = podSpec(find(manifests, "Deployment", GATEWAY)).containers[0];
      expect(container?.ports?.map((p) => [p.name, p.containerPort])).toEqual([
        ["http", 8080],
        ["admin", ADMIN_SERVICE_PORT],
        ["health", 8081],
      ]);
      expect(container?.env).toContainEqual({ name: "AI_GATEWAY__ADMIN__LISTEN_ADDR", value: `0.0.0.0:${ADMIN_SERVICE_PORT}` });
      // Only the identity service gets the admin API's address, and only the gateway its listen
      // address; neither is a call on behalf of users.
      for (const deployment of manifests.filter((m) => m.kind === "Deployment")) {
        const names = podSpec(deployment).containers.flatMap((c) => (c.env ?? []).map((e) => e.name));
        expect(names.includes("TRUSTIOT_AI_GATEWAY_ADMIN_URL"), deployment.metadata.name).toBe(deployment.metadata.name === `${RELEASE}-identity`);
        expect(names.some((n) => n.startsWith("AI_GATEWAY__ADMIN__")), deployment.metadata.name).toBe(deployment.metadata.name === GATEWAY);
      }
      const identityEnv = envOf(manifests, `${RELEASE}-identity`);
      expect(identityEnv).toContainEqual({ name: "TRUSTIOT_AI_GATEWAY_ADMIN_URL", value: ADMIN_URL });
      const onBehalf = JSON.parse(identityEnv.find((e) => e.name === "TRUSTIOT_ON_BEHALF_CALLS")?.value ?? "null") as Record<string, string[]>;
      expect(Object.keys(onBehalf)).not.toContain("identity");
      expect(Object.values(onBehalf).flat()).not.toContain("ai-gateway");
    }, SLOW_TEST_MS);

    test.each(SHAPE_NAMES)("%s: the admin Service stays on 8082 when the container port changes, and the policy and listen address follow the container port", (shape) => {
      const CONTAINER_PORT = 9082;
      const manifests = render(SHAPES[shape], [
        ...enableAllServices,
        "--set-json",
        `services.ai-gateway.ports=[{"name":"http","containerPort":8080,"appProtocol":"http"},{"name":"admin","containerPort":${CONTAINER_PORT},"appProtocol":"http"}]`,
      ]);
      expect(servicePorts(manifests, ADMIN)).toEqual([["admin", ADMIN_SERVICE_PORT, "admin"]]);
      expect(envOf(manifests, GATEWAY)).toContainEqual({ name: "AI_GATEWAY__ADMIN__LISTEN_ADDR", value: `0.0.0.0:${CONTAINER_PORT}` });
      expect(policy(manifests, "AuthorizationPolicy", ADMIN).rules).toEqual([
        { from: [{ source: { principals: [principal("identity")] } }], to: [{ operation: { ports: [String(CONTAINER_PORT)] } }] },
      ]);
      expect(envOf(manifests, `${RELEASE}-identity`)).toContainEqual({ name: "TRUSTIOT_AI_GATEWAY_ADMIN_URL", value: ADMIN_URL });
      // The gateway's own policy still admits its callers on the API port alone.
      expect((policy(manifests, "AuthorizationPolicy", GATEWAY).rules ?? []).flatMap((rule) => rule.to?.flatMap((to) => to.operation.ports) ?? [])).toEqual(["8080"]);
    }, SLOW_TEST_MS);

    test.each(SHAPE_NAMES)("%s: without the identity service the admin API stays on loopback, with no Service or policies of its own", (shape) => {
      const manifests = render(SHAPES[shape], [...enableAllServices, "--set", "services.identity.enabled=false"]);
      expect(manifests.filter((m) => m.metadata.name === ADMIN)).toEqual([]);
      const env = envOf(manifests, GATEWAY);
      expect(env.filter((e) => e.name.startsWith("AI_GATEWAY__ADMIN__"))).toEqual([]);
      // The gateway's own Service and policy still leave the admin port out, so nothing reaches
      // it even if it listened beyond loopback.
      expect(servicePorts(manifests, GATEWAY)).toEqual([["http", 8080, "http"]]);
      expect((policy(manifests, "AuthorizationPolicy", GATEWAY).rules ?? []).flatMap((rule) => rule.to?.flatMap((to) => to.operation.ports) ?? [])).toEqual(["8080"]);
      // Without the gateway, nothing of it, and the identity service gets no address.
      const withoutGateway = render(SHAPES[shape], ["--set", "services.identity.enabled=true"]);
      expect(withoutGateway.filter((m) => m.metadata.name === ADMIN)).toEqual([]);
      expect(envOf(withoutGateway, `${RELEASE}-identity`).map((e) => e.name)).not.toContain("TRUSTIOT_AI_GATEWAY_ADMIN_URL");
    }, SLOW_TEST_MS);
  });
});

// Values of ingress.tunnel.clusterIP that are not one plain IPv4 address (Laysi/trustiot-v2#478):
// Kubernetes would refuse each, or take "None" for a headless Service.
const INVALID_CLUSTER_IPS: [string, string][] = [
  ["a CIDR", "10.43.0.240/32"],
  ["written with leading zeros", "10.43.0.024"],
  ["the headless value None", "None"],
  ["an IPv6 address", "fd00:10:43::f0"],
  ["an octet above 255", "10.43.0.256"],
  ["three octets", "10.43.240"],
  ["padded with a space", "10.43.0.240 "],
  ["a host name", "ingress.trustiot.test"],
];

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
      ["--set", "objectStorage.endpoint=http://objects.saas.trustiot.test"],
      /\/objectStorage\/endpoint/,
    ],
    [
      "AI provider endpoint over plain http, in either shape",
      onprem,
      ["--set", "ai.providers.anthropic.baseUrl=http://llm.factory.test/anthropic"],
      /\/ai\/providers\/anthropic\/baseUrl/,
    ],
    [
      "AI provider endpoint with a query, where a key could hide",
      saas,
      ["--set", "ai.providers.openai.baseUrl=https://llm.saas.trustiot.test/v1?key=x"],
      /\/ai\/providers\/openai\/baseUrl/,
    ],
    [
      "AI provider the chart does not know",
      saas,
      ["--set", "ai.providers.gemini.apiKeySecret.name=trustiot-gemini"],
      /gemini/,
    ],
    [
      "AI model of an unknown provider or with a comma",
      saas,
      ["--set-json", 'ai.roles.main.models=["gemini/x","anthropic/a,b"]'],
      /\/ai\/roles\/main\/models\/0|\/ai\/roles\/main\/models\/1/,
    ],
    [
      "AI role name the gateway refuses",
      saas,
      ["--set-json", 'ai.roles.Main={"models":["anthropic/claude-x"]}'],
      /Main/,
    ],
    [
      "AI request timeout over an hour",
      saas,
      ["--set", "ai.requestTimeout=2h"],
      /\/ai\/requestTimeout/,
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
    [
      "a private key (d) among the internal identity public keys",
      saas,
      [
        "--set-json",
        'internalIdentity.publicKeys=[{"kid":"k","x":"jxSgv4kffdHZ1jN2sf_IVpE4VGbJdheBqhCAAqI89Q8","d":"private"}]',
      ],
      /\/internalIdentity\/publicKeys\/0/,
    ],
    [
      "an internal identity public key that is not 32 bytes",
      saas,
      ["--set-json", 'internalIdentity.publicKeys=[{"kid":"k","x":"jxSgv4kffdHZ1jN2sf_IVpE4VGbJdheBqhCAAqI89Q"}]'],
      /\/internalIdentity\/publicKeys\/0\/x/,
    ],
    [
      "an internal identity public key in padded base64",
      saas,
      ["--set-json", 'internalIdentity.publicKeys=[{"kid":"k","x":"jxSgv4kffdHZ1jN2sf/IVpE4VGbJdheBqhCAAqI89Q8="}]'],
      /\/internalIdentity\/publicKeys\/0\/x/,
    ],
    [
      "more than eight internal identity public keys",
      saas,
      [
        "--set-json",
        `internalIdentity.publicKeys=${JSON.stringify(
          Array.from({ length: 9 }, (_, i) => ({ kid: `k${i}`, x: "jxSgv4kffdHZ1jN2sf_IVpE4VGbJdheBqhCAAqI89Q8" })),
        )}`,
      ],
      /\/internalIdentity\/publicKeys/,
    ],
    [
      "the internal identity signing key as a plain value",
      saas,
      ["--set", "internalIdentity.signingKeySecret.value=plain-text"],
      /signingKeySecret/,
    ],
    ["external Qdrant without a URL", saas, ["--set", "qdrant.external.url="], /\/qdrant\/external\/url/],
    ["Sentry on without a DSN Secret", saas, ["--set", "observability.sentry.dsnSecret.name="], /dsnSecret/],
    [
      "a ClickHouse admin password generation that is not a whole number",
      saas,
      ["--set-string", "clickhouse.bundled.adminPasswordGeneration=v2"],
      /\/clickhouse\/bundled\/adminPasswordGeneration/,
    ],
    [
      "a ClickHouse admin password generation of ten digits",
      saas,
      ["--set-string", "clickhouse.bundled.adminPasswordGeneration=1234567890"],
      /\/clickhouse\/bundled\/adminPasswordGeneration/,
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
    // The identity service cannot send login codes without a sender (Laysi/trustiot-v2#103).
    ["SMTP relay without a sender", onprem, ["--set", "smtp.from="], /\/smtp\/from/],
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
        'services.smoke-echo.env=[{"name":"LLM_API_KEY","valueFrom":{"secretKeyRef":{"name":"trustiot-anthropic","key":"api-key"}}}]',
      ],
      /env reads the dependency credential trustiot-anthropic\/api-key/,
    ],
    [
      "ClickHouse bootstrap admin password, in an existing Secret, read through custom env",
      saas,
      [
        "--set",
        "clickhouse.bundled.adminSecret.name=trustiot-clickhouse-admin",
        "--set-json",
        'services.smoke-echo.env=[{"name":"CH_ADMIN_PASSWORD","valueFrom":{"secretKeyRef":{"name":"trustiot-clickhouse-admin","key":"password"}}}]',
      ],
      /env reads the dependency credential trustiot-clickhouse-admin\/password/,
    ],
    [
      "generated ClickHouse bootstrap admin password read through custom env",
      saas,
      [
        "--set-json",
        'services.smoke-echo.env=[{"name":"CH_ADMIN","valueFrom":{"secretKeyRef":{"name":"trustiot-clickhouse-admin-password","key":"trustiot_admin"}}}]',
      ],
      /env reads Secret trustiot-clickhouse-admin-password, whose keys reach services only through the chart/,
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
      "an internal identity key id listed twice",
      saas,
      [
        "--set",
        "services.identity.enabled=true",
        "--set",
        "internalIdentity.signingKeySecret.name=trustiot-identity-signing",
        "--set-json",
        'internalIdentity.publicKeys=[{"kid":"k","x":"jxSgv4kffdHZ1jN2sf_IVpE4VGbJdheBqhCAAqI89Q8"},{"kid":"k","x":"jxSgv4kffdHZ1jN2sf_IVpE4VGbJdheBqhCAAqI89Q8"}]',
      ],
      /internalIdentity\.publicKeys\[1\]: key id "k" is listed twice/,
    ],
    [
      "env that reads a signing key a person made before Laysi/trustiot-v2#404",
      saas,
      [
        "--set",
        "internalIdentity.signingKeySecret.name=trustiot-identity-signing",
        "--set-json",
        'internalIdentity.publicKeys=[{"kid":"k","x":"jxSgv4kffdHZ1jN2sf_IVpE4VGbJdheBqhCAAqI89Q8"}]',
        "--set-json",
        'services.smoke-echo.env=[{"name":"SIGNING_KEY","valueFrom":{"secretKeyRef":{"name":"trustiot-identity-signing","key":"signing-key.pem"}}}]',
      ],
      /services\.smoke-echo: env reads the dependency credential trustiot-identity-signing\/signing-key\.pem/,
    ],
    [
      "env that reads the generated internal identity signing keys",
      saas,
      [
        "--set",
        "services.identity.enabled=true",
        "--set-json",
        'services.smoke-echo.env=[{"name":"SIGNING_KEYS","valueFrom":{"secretKeyRef":{"name":"trustiot-identity-signing-keys","key":"signing-keys.json"}}}]',
      ],
      /services\.smoke-echo: env reads Secret trustiot-identity-signing-keys, whose keys reach services only through the chart/,
    ],
    [
      "env that reads the generated internal identity signing keys while the identity service is off",
      saas,
      [
        "--set-json",
        'services.smoke-echo.env=[{"name":"SIGNING_KEYS","valueFrom":{"secretKeyRef":{"name":"trustiot-identity-signing-keys","key":"signing-keys.json"}}}]',
      ],
      /services\.smoke-echo: env reads Secret trustiot-identity-signing-keys/,
    ],
    ["an internal identity key generation of 0", saas, ["--set-string", "internalIdentity.keyGeneration=0"], /internalIdentity\.keyGeneration must be at least 1/],
    ["an internal identity key generation of 0 as a number", saas, ["--set", "internalIdentity.keyGeneration=0"], /keyGeneration/],
    [
      "an internal identity key generation below the revocation",
      saas,
      ["--set", "internalIdentity.keyGeneration=2", "--set", "internalIdentity.revokeBeforeGeneration=3"],
      /internalIdentity\.keyGeneration \(2\) must not be below internalIdentity\.revokeBeforeGeneration \(3\)/,
    ],
    ["an internal identity key generation that is not a number", saas, ["--set", "internalIdentity.keyGeneration=v2"], /keyGeneration/],
    ["a negative revocation generation", saas, ["--set", "internalIdentity.revokeBeforeGeneration=-1"], /revokeBeforeGeneration/],
    ["a key propagation time below two minutes", saas, ["--set", "internalIdentity.keyPropagationSeconds=60"], /keyPropagationSeconds/],
    ["a key propagation time above a day", saas, ["--set", "internalIdentity.keyPropagationSeconds=86401"], /keyPropagationSeconds/],
    ["a negative maximum key age", saas, ["--set", "internalIdentity.maxKeyAgeSeconds=-1"], /maxKeyAgeSeconds/],
    ["a maximum key age above 365 days", saas, ["--set", "internalIdentity.maxKeyAgeSeconds=31536001"], /maxKeyAgeSeconds/],
    ["a maximum key age as a duration", saas, ["--set-string", "internalIdentity.maxKeyAgeSeconds=90d"], /maxKeyAgeSeconds/],
    [
      "a maximum key age too short for the old key to retire before the next rotation",
      saas,
      ["--set", "internalIdentity.maxKeyAgeSeconds=1504"],
      /internalIdentity\.maxKeyAgeSeconds \(1504\) must be 0 or at least 1505/,
    ],
    ["a rotation schedule with a time zone", saas, ["--set-string", "internalIdentity.keyRotationSchedule=TZ=Asia/Taipei 0 3 * * *"], /keyRotationSchedule/],
    ["a rotation schedule of four fields", saas, ["--set-string", "internalIdentity.keyRotationSchedule=0 3 * *"], /keyRotationSchedule/],
    ["an empty rotation schedule", saas, ["--set-string", "internalIdentity.keyRotationSchedule="], /keyRotationSchedule/],
    // Sparser than daily: the key would outlive maxKeyAgeSeconds by up to the gap between runs.
    ["a weekly rotation schedule", saas, ["--set-string", "internalIdentity.keyRotationSchedule=@weekly"], /keyRotationSchedule/],
    ["a monthly rotation schedule", saas, ["--set-string", "internalIdentity.keyRotationSchedule=@monthly"], /keyRotationSchedule/],
    ["a rotation schedule on some days of the month", saas, ["--set-string", "internalIdentity.keyRotationSchedule=0 0 1\\,15 * *"], /keyRotationSchedule/],
    ["a rotation schedule in one month", saas, ["--set-string", "internalIdentity.keyRotationSchedule=0 3 * 1 *"], /keyRotationSchedule/],
    ["a rotation schedule on weekdays", saas, ["--set-string", "internalIdentity.keyRotationSchedule=30 2 * * MON-FRI"], /keyRotationSchedule/],
    [
      "a token in the identity service's container",
      saas,
      ["--set", "services.identity.enabled=true", "--set", "services.identity.automountServiceAccountToken=true"],
      /services\.identity\.automountServiceAccountToken must be false/,
    ],
    [
      "public keys of a person's signing key without its Secret",
      saas,
      ["--set-json", 'internalIdentity.publicKeys=[{"kid":"k","x":"jxSgv4kffdHZ1jN2sf_IVpE4VGbJdheBqhCAAqI89Q8"}]'],
      /internalIdentity\.publicKeys is only for moving over a signing key a person made/,
    ],
    [
      "a person's signing key without its public key",
      saas,
      ["--set", "internalIdentity.signingKeySecret.name=trustiot-identity-signing"],
      /internalIdentity\.signingKeySecret needs internalIdentity\.publicKeys/,
    ],
    [
      "cookie route to ai-gateway",
      saas,
      [
        "--set",
        "services.ai-gateway.enabled=true",
        "--set-json",
        'ingress.routes=[{"host":"app","path":"/api/ai","match":"PathPrefix","service":"ai-gateway"}]',
      ],
      /ingress\.routes\[0\]: ai-gateway takes no internal identity tokens/,
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
    // ingress.tunnel.clusterIP (Laysi/trustiot-v2#478): one plain IPv4 address, tunnel only.
    ...INVALID_CLUSTER_IPS.map(([name, value]): [string, readonly string[], string[], RegExp] => [
      `a fixed ingress ClusterIP that is ${name}`,
      saas,
      ["--set-string", `ingress.tunnel.clusterIP=${value}`],
      /at '\/ingress\/tunnel\/clusterIP': .* does not match pattern/,
    ]),
    ["a fixed ingress ClusterIP that is false", saas, ["--set", "ingress.tunnel.clusterIP=false"], /at '\/ingress\/tunnel\/clusterIP': got boolean, want string/],
    [
      "a fixed ingress ClusterIP with direct exposure",
      onprem,
      ["--set", "ingress.tunnel.clusterIP=10.43.0.240"],
      /ingress\.tunnel\.clusterIP: set only with ingress\.exposure tunnel, where cloudflared forwards to the ingress Service's ClusterIP; exposure "direct" never uses it/,
    ],
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
    [
      "collector using the default ClickHouse account",
      saas,
      ["--set", "observability.collector.clickhouse.user=default"],
      /\/observability\/collector\/clickhouse\/user/,
    ],
    [
      "collector image without a digest",
      saas,
      ["--set", "observability.collector.image.digest="],
      /\/observability\/collector\/image\/digest/,
    ],
    [
      "forwarding without an endpoint",
      onprem,
      ["--set", "observability.collector.forward.enabled=true"],
      /\/observability\/collector\/forward\/endpoint/,
    ],
    [
      "SIEM credentials inside the forward URL",
      onprem,
      [
        "--set",
        "observability.collector.forward.enabled=true",
        "--set",
        "observability.collector.forward.endpoint=https://u:p@siem.factory.test",
      ],
      /\/observability\/collector\/forward\/endpoint/,
    ],
    [
      "SaaS forwarding over plain http",
      saas,
      [
        "--set",
        "observability.collector.forward.enabled=true",
        "--set",
        "observability.collector.forward.endpoint=http://siem.saas.trustiot.test:4318",
      ],
      /\/observability\/collector\/forward\/endpoint/,
    ],
    [
      "forwarding metrics",
      onprem,
      ["--set-json", 'observability.collector.forward.signals=["logs","metrics"]'],
      /\/observability\/collector\/forward\/signals/,
    ],
    [
      "SIEM auth header without its Secret",
      onprem,
      ["--set", "observability.collector.forward.authHeader.name=Authorization"],
      /\/observability\/collector\/forward\/authHeader\/valueSecret\/name/,
    ],
    ["plain-text SIEM token in values", onprem, ["--set", "observability.collector.forward.token=plain-text"], /token/],
    ["unknown mesh field", saas, ["--set", "mesh.waypoint=true"], /waypoint/],
    ["IP address as the trust domain", saas, ["--set", "mesh.trustDomain=10.0.0.1"], /\/mesh\/trustDomain/],
    ["callee listed twice", saas, ["--set-json", 'mesh.calls={"smoke-echo":["smoke-echo","smoke-echo"]}'], /\/mesh\/calls/],
    ["callees that are not a list", saas, ["--set-json", 'mesh.calls={"smoke-echo":"smoke-echo"}'], /\/mesh\/calls/],
    ["bundled Postgres without a volume size", saas, ["--set", "postgres.bundled.storage.size="], /\/postgres\/bundled\/storage\/size/],
    ["a PostgreSQL setting other than sizing", onprem, ["--set", "postgres.bundled.parameters.ssl=off"], /\/postgres\/bundled\/parameters/],
    [
      "a PostgreSQL authentication setting",
      saas,
      ["--set", "postgres.bundled.parameters.password_encryption=md5"],
      /\/postgres\/bundled\/parameters/,
    ],
    [
      "a PostgreSQL memory setting without a unit",
      onprem,
      ["--set-string", "postgres.bundled.parameters.shared_buffers=128"],
      /\/postgres\/bundled\/parameters\/shared_buffers/,
    ],
    ["too many Postgres connections", saas, ["--set", "postgres.bundled.parameters.max_connections=5000"], /\/postgres\/bundled\/parameters\/max_connections/],
    [
      "too many Postgres connections as a string",
      saas,
      ["--set-string", "postgres.bundled.parameters.max_connections=1001"],
      /\/postgres\/bundled\/parameters\/max_connections/,
    ],
    [
      "too few Postgres connections as a string",
      saas,
      ["--set-string", "postgres.bundled.parameters.max_connections=9"],
      /\/postgres\/bundled\/parameters\/max_connections/,
    ],
    ["Postgres without a memory limit", saas, ["--set", "postgres.bundled.resources.limits.memory=null"], /\/postgres\/bundled\/resources\/limits/],
    [
      "the CloudNativePG operator without pod labels",
      onprem,
      // Maps merge, so remove the default label with null.
      ["--set", "postgres.bundled.operator.podLabels.app\\.kubernetes\\.io/name=null"],
      /\/postgres\/bundled\/operator\/podLabels/,
    ],
    ["an unknown bundled Postgres field", saas, ["--set", "postgres.bundled.superuser=true"], /superuser/],
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
    // Laysi/trustiot-v2#398: a value printed into a line cannot start another document, such as
    // a Secret with data, because no value may hold a line break (YAML also breaks at CR, NEL,
    // LS and PS), in a string, a map key or a list item.
    [
      "a value with a line break and a document separator that adds a Secret",
      ["--set-json", 'observability.sentry.environment="test\\n---\\napiVersion: v1\\nkind: Secret\\nmetadata:\\n  name: leak\\ndata:\\n  v: eA=="'],
      /observability\.sentry\.environment: holds a line break; values are single lines/,
    ],
    ["a value with a carriage return", ["--set-json", 'hosts.app="app.saas.trustiot.test\\r---"'], /hosts\.app: holds a line break/],
    ["a map key with a line separator", ["--set-json", 'serviceDefaults.podAnnotations={"a\\u2028kind: Secret":"x"}'], /serviceDefaults\.podAnnotations: a key holds a line break/],
    ["a list item with a next-line character", ["--set-json", 'global.imagePullSecrets=["pull\\u0085---"]'], /global\.imagePullSecrets\[0\]: holds a line break/],
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
    ...INVALID_CLUSTER_IPS.map(([name, value]): [string, string[], RegExp] => [
      `a fixed ingress ClusterIP that is ${name}`,
      ["--set-string", `ingress.tunnel.clusterIP=${value}`],
      new RegExp(`ingress\\.tunnel\\.clusterIP: "${value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}" must be one IPv4 address in dotted-quad form without leading zeros`),
    ]),
    // Not a string: refused, never taken for unset (false and 0 would be with Sprig's `default`).
    ...([["a number", "7", "int64 7"], ["zero", "0", "int64 0"], ["false", "false", "bool false"], ["a list", "{10.43.0.240}", "slice \\[10\\.43\\.0\\.240\\]"]] as const).map(
      ([name, value, got]): [string, string[], RegExp] => [
        `a fixed ingress ClusterIP that is ${name}`,
        ["--set", `ingress.tunnel.clusterIP=${value}`],
        new RegExp(`ingress\\.tunnel\\.clusterIP: must be a string holding one IPv4 address or empty, got ${got}`),
      ],
    ),
    [
      "a fixed ingress ClusterIP with direct exposure",
      ["--set", "ingress.exposure=direct", "--set", "ingress.direct.tlsSecret=trustiot-ingress-tls", "--set", "ingress.tunnel.clusterIP=10.43.0.240"],
      /ingress\.tunnel\.clusterIP: set only with ingress\.exposure tunnel, where cloudflared forwards to the ingress Service's ClusterIP; exposure "direct" never uses it/,
    ],
    [
      "a fixed ingress ClusterIP with no exposure",
      ["--set", "ingress.exposure=", "--set", "ingress.tunnel.clusterIP=10.43.0.240"],
      /ingress\.tunnel\.clusterIP: set only with ingress\.exposure tunnel.*exposure "" never uses it/,
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
    [
      "env that points a service past the collector",
      ["--set-json", 'services.smoke-echo.env=[{"name":"TRUSTIOT__TELEMETRY__OTLP_ENDPOINT","value":"http://elsewhere:4318"}]'],
      /services\.smoke-echo: env TRUSTIOT__TELEMETRY__OTLP_ENDPOINT is set by the chart/,
    ],
    [
      "env that reads the collector's ClickHouse password",
      [
        "--set-json",
        'services.smoke-echo.env=[{"name":"CH_PASSWORD","valueFrom":{"secretKeyRef":{"name":"trustiot-otel-collector-password","key":"trustiot_collector"}}}]',
      ],
      /env reads Secret trustiot-otel-collector-password/,
    ],
    [
      "forwarding without an endpoint",
      ["--set", "observability.collector.forward.enabled=true"],
      /observability\.collector\.forward\.endpoint is required/,
    ],
    [
      "SIEM CA with a plain-text endpoint",
      [
        "--set",
        "observability.collector.forward.enabled=true",
        "--set",
        "observability.collector.forward.endpoint=http://siem.saas.trustiot.test:4318",
        "--set",
        "observability.collector.forward.caSecret.name=siem-ca",
      ],
      /observability\.collector\.forward\.caSecret needs an https:\/\/ endpoint/,
    ],
    [
      "SIEM auth header without its Secret",
      [
        "--set",
        "observability.collector.forward.enabled=true",
        "--set",
        "observability.collector.forward.endpoint=https://siem.saas.trustiot.test",
        "--set",
        "observability.collector.forward.authHeader.name=Authorization",
      ],
      /observability\.collector\.forward\.authHeader\.valueSecret\.name is required/,
    ],
    // CloudNativePG's Secrets of the bundled Postgres: the owner's password, and the CA's private
    // key next to the certificate the chart mounts.
    ...["app/password", "ca/ca.key", "ca/ca.crt", "server/tls.key", "superuser/password"].map(
      (ref): [string, string[], RegExp] => {
        const [suffix, key] = ref.split("/");
        const secret = `${RELEASE}-postgres-${suffix}`;
        return [
          `env reading the bundled Postgres's Secret ${secret}/${key}`,
          ["--set-json", `services.smoke-echo.env=[{"name":"DB_SETTING","valueFrom":{"secretKeyRef":{"name":"${secret}","key":"${key}"}}}]`],
          new RegExp(`services\\.smoke-echo: env reads Secret ${secret}, whose keys reach services only through the chart`),
        ];
      },
    ),
    [
      "a service named after the bundled Postgres",
      ["--set", "services.postgres.enabled=true"],
      /services\.postgres: the name belongs to an in-cluster component/,
    ],
    // The internal identity keys (Laysi/trustiot-v2#404): a generation the init container would
    // refuse, one below the revocation, and a token in the container that holds the private keys.
    [
      "an internal identity key generation the init container would refuse",
      ["--set-string", "internalIdentity.keyGeneration=v2"],
      /internalIdentity\.keyGeneration: "v2" must render as 1 to 9 decimal digits/,
    ],
    [
      "an internal identity key generation below the revocation",
      ["--set", "internalIdentity.keyGeneration=1", "--set", "internalIdentity.revokeBeforeGeneration=2"],
      /internalIdentity\.keyGeneration \(1\) must not be below internalIdentity\.revokeBeforeGeneration \(2\)/,
    ],
    ["a key propagation time below two minutes", ["--set", "internalIdentity.keyPropagationSeconds=5"], /keyPropagationSeconds must be between 120 and 86400/],
    [
      "a maximum key age too short for the old key to retire before the next rotation",
      ["--set", "internalIdentity.keyPropagationSeconds=120", "--set", "internalIdentity.maxKeyAgeSeconds=544"],
      /internalIdentity\.maxKeyAgeSeconds \(544\) must be 0 or at least 545/,
    ],
    [
      "a service named after the rotation CronJob",
      ["--set", "services.identity-key-rotation.enabled=true"],
      /services\.identity-key-rotation: the name belongs to the CronJob/,
    ],
    [
      "a service named after the token exchange's Service and policies",
      ["--set", "services.identity-exchange.enabled=true"],
      /services\.identity-exchange: the name belongs to the identity service's token exchange/,
    ],
    [
      "calls on behalf of users while the identity service has no token-exchange port",
      [
        "--set",
        "services.identity.enabled=true",
        "--set-json",
        'services.identity.ports=[{"name":"http","containerPort":8080}]',
        "--set",
        "services.assets.enabled=true",
        "--set",
        "services.certs.enabled=true",
        "--set-json",
        'mesh.calls={"assets":["certs"]}',
      ],
      /services\.identity\.ports: the token exchange needs the port named token-exchange .* because assets call other services on behalf of users/,
    ],
    // The MQTT broker's server certificate (Laysi/trustiot-v2#557, Laysi/trustiot-v2#535): its port
    // has its own Service and policies, for the broker's certificate CronJob alone, which has its
    // own ServiceAccount, Role and policies.
    [
      "a service named after the broker certificate's Service and policies",
      ["--set", "services.certs-broker.enabled=true"],
      /services\.certs-broker: the name belongs to the certs service's MQTT broker certificate issuance/,
    ],
    [
      "a service named after the broker's certificate CronJob",
      ["--set", "services.mqtt-broker-certificate.enabled=true"],
      /services\.mqtt-broker-certificate: the name belongs to the CronJob that issues the MQTT broker's server certificate/,
    ],
    [
      "the MQTT broker while the certs service has no broker-certs port",
      ["--set", "services.certs.enabled=true", "--set-json", 'services.certs.ports=[{"name":"http","containerPort":8080}]', "--set", "mqttBroker.enabled=true"],
      /services\.certs\.ports: the MQTT broker's server certificate needs the port named broker-certs/,
    ],
    [
      "a certs service with the broker-certs port only",
      ["--set", "services.certs.enabled=true", "--set-json", 'services.certs.ports=[{"name":"broker-certs","containerPort":8082}]'],
      // The ingress routes to the API port by name, so their check may fail the render first.
      /services\.certs\.ports: the API port \(http\) is missing|service "certs" has no port named "http"/,
    ],
    [
      "an ingress route to the certs service's broker-certs port",
      [
        "--set",
        "services.certs.enabled=true",
        "--set-json",
        'ingress.routes=[{"host":"app","path":"/api/certs","match":"PathPrefix","service":"certs","port":"broker-certs"}]',
      ],
      /ingress\.routes\[0\]: port "broker-certs" of service "certs" has a Service and policies of its own/,
    ],
    [
      "an ingress route to the identity service's token-exchange port",
      [
        "--set",
        "services.identity.enabled=true",
        "--set-json",
        'ingress.routes=[{"host":"app","path":"/api/identity","match":"PathPrefix","service":"identity","port":"token-exchange"}]',
      ],
      /ingress\.routes\[0\]: port "token-exchange" of service "identity" has a Service and policies of its own/,
    ],
    [
      "an identity service with the token-exchange port only",
      ["--set", "services.identity.enabled=true", "--set-json", 'services.identity.ports=[{"name":"token-exchange","containerPort":8082}]'],
      // The ingress routes to the API port by name, so their check may fail the render first.
      /services\.identity\.ports: the API port \(http\) is missing|service "identity" has no port named "http"/,
    ],
    [
      "a token in the identity service's container",
      ["--set", "services.identity.enabled=true", "--set", "services.identity.automountServiceAccountToken=true"],
      /services\.identity\.automountServiceAccountToken must be false/,
    ],
    [
      "a token in every service's container",
      ["--set", "services.identity.enabled=true", "--set", "serviceDefaults.automountServiceAccountToken=true"],
      /services\.identity\.automountServiceAccountToken must be false/,
    ],
    [
      "env that reads the generated internal identity signing keys",
      ["--set-json", 'services.smoke-echo.env=[{"name":"KEYS","valueFrom":{"secretKeyRef":{"name":"trustiot-identity-signing-keys","key":"signing-keys.json"}}}]'],
      /services\.smoke-echo: env reads Secret trustiot-identity-signing-keys/,
    ],
    [
      "a ClickHouse admin password generation the init container would refuse",
      ["--set-string", "clickhouse.bundled.adminPasswordGeneration=v2"],
      /clickhouse\.bundled\.adminPasswordGeneration: "v2" must render as 1 to 9 decimal digits/,
    ],
    [
      "env that reads the generated ClickHouse admin password",
      [
        "--set-json",
        'services.smoke-echo.env=[{"name":"CH_ADMIN","valueFrom":{"secretKeyRef":{"name":"trustiot-clickhouse-admin-password","key":"trustiot_admin"}}}]',
      ],
      /env reads Secret trustiot-clickhouse-admin-password/,
    ],
    [
      "a ClickHouse node selector for another architecture while the admin password is generated",
      ["--set", "clickhouse.bundled.nodeSelector.kubernetes\\.io/arch=amd64"],
      /clickhouse\.bundled\.nodeSelector: kubernetes\.io\/arch must be arm64 while the chart generates the admin password/,
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
      "TRUSTIOT__TELEMETRY__OTLP_ENDPOINT",
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
      "TRUSTIOT__TELEMETRY__OTLP_ENDPOINT",
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
    // smoke.yaml tags every TrustIoT image `nightly`; back to the defaults for this check.
    const manifests = render(files, [
      "--set",
      "serviceDefaults.image.tag=",
      "--set",
      "migrate.image.tag=",
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
        .filter(isServiceDeployment)
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
    const prefix = ["serviceDefaults.", "clickhouse.bundled.", "postgres.bundled.", "observability.collector.", "migrate.", "mqttBroker."].find((p) =>
      path.startsWith(p),
    );
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

describe("e2e overlays", () => {
  // Every install of the cluster tests (tests/e2e/*.e2e.ts), rendered with the values files and
  // the arguments that decide what runs and which Secrets it reads; images and per-run names are
  // left out or fixed. `migrate` says whether the install runs the migration Job, and
  // `filledByTest` lists the password Secrets the test fills itself in its place.
  type E2eInstall = { name: string; files: string[]; args: string[]; migrate: boolean; filledByTest: string[] };
  const OTEL_VALUES = "tests/e2e/values-otel-e2e.yaml";
  const otelArgs = ["--set", "observability.collector.clickhouse.user=trustiot_collector_e2e"];
  const INSTALLS: E2eInstall[] = [
    ...SHAPE_NAMES.map((shape) => ({
      name: `ingress.e2e.ts, ${shape}`,
      files: [...SHAPES[shape], E2E_VALUES],
      args: [
        "--set",
        "ingress.authzProvider=trustiot-identity",
        ...(shape === "onprem" ? ["--set", "ingress.direct.tlsSecret=trustiot-e2e-tls", "--set", "ingress.direct.serviceType=ClusterIP"] : []),
      ],
      migrate: false,
      filledByTest: [],
    })),
    {
      name: "mesh.e2e.ts",
      files: [...SHAPES.saas, E2E_VALUES, "tests/e2e/values-mesh-e2e.yaml"],
      args: [],
      migrate: false,
      // The AI gateway lists postgres, which the chart requires; the test writes its password
      // into the Secret once Helm has declared it (Laysi/trustiot-v2#607).
      filledByTest: [`${RELEASE}-ai-gateway-password`],
    },
    {
      name: "otel.e2e.ts, step 1 (ClickHouse only)",
      files: [...SHAPES.onprem, OTEL_VALUES],
      args: [...otelArgs, "--set", "observability.collector.enabled=false", "--set", "services.assets.enabled=false"],
      migrate: false,
      filledByTest: [],
    },
    {
      name: "otel.e2e.ts, step 2 (the collector and the example service)",
      files: [...SHAPES.onprem, OTEL_VALUES],
      args: [
        ...otelArgs,
        "--set",
        "observability.collector.forward.enabled=true",
        "--set",
        "observability.collector.forward.endpoint=http://trustiot-e2e-siem.trustiot.svc:4318",
        "--set-json",
        'observability.collector.forward.signals=["logs","traces"]',
        "--set",
        "observability.collector.forward.authHeader.name=Authorization",
        "--set",
        "observability.collector.forward.authHeader.valueSecret.name=trustiot-e2e-siem",
        "--set",
        "observability.collector.forward.authHeader.valueSecret.key=header",
      ],
      migrate: false,
      // The test writes the collector's password into it once Helm has declared it.
      filledByTest: [`${RELEASE}-otel-collector-password`],
    },
    ...[[], ["--set", "clickhouse.bundled.adminSecret.name=trustiot-e2e-ch-admin"]].map((extra) => ({
      name: `clickhouse-admin.e2e.ts, ${extra.length === 0 ? "generated" : "an existing Secret"}`,
      files: [...SHAPES.onprem, OTEL_VALUES],
      args: ["--set", "observability.collector.enabled=false", "--set", "services.assets.enabled=false", ...extra],
      migrate: false,
      filledByTest: [],
    })),
    ...SHAPE_NAMES.map((shape) => ({
      name: `postgres.e2e.ts, ${shape}`,
      files: [...SHAPES[shape], "tests/e2e/values-postgres-e2e.yaml"],
      args: [],
      migrate: true,
      filledByTest: [],
    })),
    ...SHAPE_NAMES.flatMap((shape) =>
      [[], ["--set", "internalIdentity.keyGeneration=3", "--set", "internalIdentity.revokeBeforeGeneration=3"]].map((extra) => ({
        name: `identity-keys.e2e.ts, ${shape}, ${extra.length === 0 ? "install" : "revocation"}`,
        files: [...SHAPES[shape], "tests/e2e/values-identity-keys-e2e.yaml"],
        args: extra,
        migrate: true,
        filledByTest: [],
      })),
    ),
  ];

  function workloadPodSpec(m: Manifest): PodSpec | undefined {
    const spec = m.spec as { template?: { spec?: PodSpec }; jobTemplate?: { spec?: { template?: { spec?: PodSpec } } } } | undefined;
    if (["Deployment", "StatefulSet", "DaemonSet", "Job"].includes(m.kind)) return spec?.template?.spec;
    if (m.kind === "CronJob") return spec?.jobTemplate?.spec?.template?.spec;
    if (m.kind === "Pod") return m.spec as PodSpec;
    return undefined;
  }

  const isMigrationJob = (m: Manifest) => m.kind === "Job" && m.metadata.labels?.["app.kubernetes.io/component"] === "migrate";

  // "<kind>/<name> reads <Secret>" for every Secret a workload's pods need: environment
  // variables, envFrom and volumes, projected ones included, in every container. An optional
  // reference is left out: it never holds a pod back.
  function secretReads(manifests: Manifest[]): { read: string; secret: string }[] {
    const reads = new Map<string, string>();
    for (const m of manifests) {
      const pod = workloadPodSpec(m);
      if (pod === undefined) continue;
      const read = (secret: string | undefined) => {
        if (secret) reads.set(`${m.kind}/${m.metadata.name} reads ${secret}`, secret);
      };
      for (const container of [...pod.containers, ...((pod.initContainers ?? []) as Container[])]) {
        for (const env of container.env ?? []) {
          const ref = env.valueFrom?.secretKeyRef;
          if (ref?.optional !== true) read(ref?.name);
        }
        for (const from of (container.envFrom ?? []) as { secretRef?: { name?: string; optional?: boolean } }[]) {
          if (from.secretRef?.optional !== true) read(from.secretRef?.name);
        }
      }
      for (const volume of pod.volumes ?? []) {
        // An optional Secret volume never holds a pod back: the identity service mounts its
        // signing keys so, and its own init container fills them (Laysi/trustiot-v2#404).
        const secret = volume.secret as { secretName?: string; optional?: boolean } | undefined;
        if (secret?.optional !== true) read(secret?.secretName);
        const projected = volume.projected as { sources?: { secret?: { name?: string } }[] } | undefined;
        for (const source of projected?.sources ?? []) read(source.secret?.name);
      }
    }
    return [...reads].map(([read, secret]) => ({ read, secret })).sort((a, b) => a.read.localeCompare(b.read));
  }

  // The reads of password Secrets that nothing fills. Without the migration Job, a Secret the Job
  // would fill (label trustiot.io/filled-by: migrate) stays empty, so a pod that reads a key of it
  // never starts and `helm install --wait` runs into its timeout. PR Laysi/trustiot-v2#411's smoke
  // (2026-10-14) failed so: values.yaml gave assets postgres, and the ingress overlay's whoami
  // stand-in for assets kept it.
  function unfilledReads(manifests: Manifest[], filledByTest: readonly string[]): string[] {
    if (manifests.some(isMigrationJob)) return [];
    const unfilled = new Set(
      manifests
        .filter((m) => m.kind === "Secret" && m.metadata.labels?.["trustiot.io/filled-by"] === "migrate")
        .map((m) => m.metadata.name)
        .filter((name) => !filledByTest.includes(name)),
    );
    return secretReads(manifests)
      .filter(({ secret }) => unfilled.has(secret))
      .map(({ read }) => read);
  }

  // The dependency Secrets the instance overlays name (the SMTP relay's account, the Sentry DSN,
  // object storage, Qdrant, the AI keys): a real cluster gets them from outside the chart
  // (Laysi/trustiot-v2#72), and no cluster test creates them. A pod that needs one never starts,
  // and `helm install --wait` runs into its timeout: PR Laysi/trustiot-v2#525's smoke (2026-10-22)
  // failed so in the ingress e2e, when values.yaml gave identity the smtp credential, which
  // saas-instance.yaml names trustiot-smtp for, with a required reference.
  function namedDependencySecrets(files: readonly string[]): Set<string> {
    const names = new Set<string>();
    const walk = (node: unknown, key: string): void => {
      if (Array.isArray(node)) {
        for (const item of node) walk(item, key);
        return;
      }
      if (!isMap(node)) return;
      if (key.endsWith("Secret") && typeof node.name === "string" && node.name !== "") names.add(node.name);
      for (const [child, value] of Object.entries(node)) walk(value, child);
    };
    for (const file of files.filter((f) => f.startsWith("tests/values/"))) walk(readYaml(file), "");
    return names;
  }

  function dependencyReads(manifests: Manifest[], files: readonly string[]): string[] {
    const named = namedDependencySecrets(files);
    return secretReads(manifests)
      .filter(({ secret }) => named.has(secret))
      .map(({ read }) => read);
  }

  for (const install of INSTALLS) {
    test(`${install.name}: no pod needs a dependency Secret that the instance overlay names`, () => {
      expect(dependencyReads(render(install.files, install.args), install.files)).toEqual([]);
    });

    test(`${install.name}: no pod reads a password Secret that nothing fills`, () => {
      const manifests = render(install.files, install.args);
      expect(manifests.filter(isMigrationJob).length, "migration Jobs").toBe(install.migrate ? 1 : 0);
      expect(manifests.filter((m) => workloadPodSpec(m) !== undefined).length).toBeGreaterThan(0);
      expect(unfilledReads(manifests, install.filledByTest)).toEqual([]);
    });
  }

  test("the check catches a stand-in that keeps postgres under mesh.calls without the migration Job", () => {
    // values.yaml's calls of assets, which the ingress overlay's whoami stand-in must not keep.
    const ingress = INSTALLS[0];
    if (ingress === undefined) throw new Error("no ingress install");
    const manifests = render(ingress.files, [...ingress.args, "--set-json", 'mesh.calls.assets=["postgres"]']);
    expect(unfilledReads(manifests, [])).toEqual([`Deployment/${RELEASE}-assets reads ${RELEASE}-assets-password`]);
    // A Secret the test fills itself is no finding.
    expect(unfilledReads(manifests, [`${RELEASE}-assets-password`])).toEqual([]);
  });

  test("the check catches a required reference to a dependency Secret, and passes identity's optional one to the relay account", () => {
    const ingress = INSTALLS[0];
    if (ingress === undefined) throw new Error("no ingress install");
    expect(namedDependencySecrets(ingress.files)).toContain("trustiot-smtp");
    expect(namedDependencySecrets(ingress.files)).toContain("trustiot-sentry");
    // The identity stand-in has the relay account's variables, from an optional reference.
    const manifests = render(ingress.files, ingress.args);
    const identity = podSpec(find(manifests, "Deployment", `${RELEASE}-identity`)).containers[0]?.env ?? [];
    expect(identity.filter((e) => e.valueFrom?.secretKeyRef?.name === "trustiot-smtp").map((e) => e.valueFrom?.secretKeyRef?.optional)).toEqual([true, true]);
    // Sentry's DSN is a required reference: with Sentry on, every service would wait for it.
    const sentry = dependencyReads(render(ingress.files, [...ingress.args, "--set", "observability.sentry.enabled=true"]), ingress.files);
    expect(sentry).toContain(`Deployment/${RELEASE}-identity reads trustiot-sentry`);
  });
});
