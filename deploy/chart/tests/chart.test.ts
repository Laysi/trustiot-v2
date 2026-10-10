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

  test("services stay off until enabled: only the platform ConfigMap renders", () => {
    const manifests = render(files);
    expect(manifests.map((m) => `${m.kind}/${m.metadata.name}`)).toEqual([`ConfigMap/${RELEASE}-platform`]);
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

  test("external Postgres and ClickHouse are wired only in external mode", () => {
    const bundled = platformConfig(render(SHAPES.onprem));
    expect(bundled.TRUSTIOT_POSTGRES_HOST).toBeUndefined();
    expect(bundled.TRUSTIOT_POSTGRES_CA_FILE).toBeUndefined();
    expect(bundled.TRUSTIOT_CLICKHOUSE_URL).toBeUndefined();
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
      for (const deployment of manifests.filter((m) => m.kind === "Deployment")) {
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
    const manifests = render(files, enableAllServices, release);
    for (const kind of ["ServiceAccount", "Service", "Deployment", "ConfigMap"]) {
      const names = manifests.filter((m) => m.kind === kind).map((m) => m.metadata.name);
      expect(new Set(names).size).toBe(names.length);
      for (const name of names) expect(name.length).toBeLessThanOrEqual(63);
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
    const relative = path.startsWith("serviceDefaults.") ? path.slice("serviceDefaults.".length) : "";
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
