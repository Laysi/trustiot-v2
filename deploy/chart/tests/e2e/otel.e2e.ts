// Log pipeline e2e test (Laysi/trustiot-v2#73, decision 22): installs the chart (on-prem shape)
// with the bundled ClickHouse, the OpenTelemetry collector and crates/service-kit's
// example-service into an existing namespace, plus a stand-in SIEM, then checks that
//   1. a request through the example service lands in ClickHouse's log database as a structured
//      record carrying the request's trace id, and its span in the trace table;
//   2. both log tables have a retention TTL;
//   3. the SIEM receives the same records, authenticated with the header from the Secret;
//   4. logs and spans carrying an email, tokens, a password and a request body are redacted in
//      ClickHouse and at the SIEM.
// README.md in this directory lists the prerequisites; run from the repository root:
//
//   E2E_NAMESPACE=e2e-73 E2E_SERVICE_IMAGE=<image> bun test --timeout 900000 ./deploy/chart/tests/e2e/otel.e2e.ts
//
// Not part of `bun test deploy/chart` (the file name has no .test. suffix): it needs a cluster.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { isAbsolute, join } from "node:path";
import type { Subprocess } from "bun";

const CHART_DIR = join(import.meta.dir, "..", "..");
const REPO_ROOT = join(CHART_DIR, "..", "..");
const MIGRATIONS = join(REPO_ROOT, "services/ingest/migrations/clickhouse");

function required(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required (see deploy/chart/tests/e2e/README.md)`);
  return value;
}

const NAMESPACE = required("E2E_NAMESPACE");
const SERVICE_IMAGE = required("E2E_SERVICE_IMAGE");
const CONTEXT = process.env.E2E_KUBECONTEXT;
const RELEASE = process.env.E2E_RELEASE ?? "trustiot";
const PULL_POLICY = process.env.E2E_IMAGE_PULL_POLICY ?? "IfNotPresent";
const REPORT_DIR = process.env.E2E_REPORT_DIR;
const KEEP = process.env.E2E_KEEP === "1";

const PREFIX = RELEASE.includes("trustiot") ? RELEASE : `${RELEASE}-trustiot`;
const RUN = crypto.randomUUID().replaceAll("-", "").slice(0, 8);
// Names of their own, so the test never replaces a Secret or object it did not create.
const ADMIN_SECRET = `trustiot-e2e-ch-admin-${RUN}`;
const COLLECTOR_SECRET = `trustiot-e2e-collector-${RUN}`;
const SIEM = `trustiot-e2e-siem-${RUN}`;
const COLLECTOR_USER = `trustiot_collector_e2e_${RUN}`;
const COLLECTOR_IMAGE = (() => {
  const values = Bun.YAML.parse(readFileSync(join(CHART_DIR, "values.yaml"), "utf8")) as {
    observability: { collector: { image: { repository: string; tag: string; digest: string } } };
  };
  const { repository, tag, digest } = values.observability.collector.image;
  return `${repository}:${tag}@${digest}`;
})();

// Values that look like what must never be stored: they are sent, then looked for everywhere.
const SECRETS = {
  email: `alice.e2e.${RUN}@example.com`,
  jwt: `eyJhbGci.eyJzdWIi.c2ln${RUN}`,
  bearer: `Bearer e2eToken${RUN}abcdef`,
  password: `hunter2-${RUN}`,
  body: `{"chat":"please reset machine ${RUN}"}`,
  phone: "0912-345-678",
};

function pinnedTool(tool: string): string {
  const result = Bun.spawnSync({ cmd: ["proto", "bin", tool, "--reporter", "text"], cwd: REPO_ROOT });
  const path = result.stdout.toString().trim().split("\n").at(-1)?.trim() ?? "";
  if (result.exitCode !== 0 || !isAbsolute(path) || !existsSync(path)) {
    throw new Error(`cannot find the pinned ${tool}; run \`proto install\``);
  }
  return path;
}
const HELM = pinnedTool("helm");
const KUBECTL = pinnedTool("kubectl");

function run(cmd: string[], stdin?: string): string {
  const proc = Bun.spawnSync({
    cmd,
    cwd: CHART_DIR,
    stdin: stdin === undefined ? "ignore" : new TextEncoder().encode(stdin),
    stdout: "pipe",
    stderr: "pipe",
  });
  if (proc.exitCode !== 0) {
    throw new Error(`${cmd.slice(0, 4).join(" ")} failed (${proc.exitCode}):\n${proc.stderr.toString()}`);
  }
  return proc.stdout.toString();
}
const kubectlArgs = () => [KUBECTL, ...(CONTEXT ? ["--context", CONTEXT] : []), "--namespace", NAMESPACE];
const kubectl = (...args: string[]) => run([...kubectlArgs(), ...args]);
const kubectlIn = (stdin: string, ...args: string[]) => run([...kubectlArgs(), ...args], stdin);
const helm = (...args: string[]) =>
  run([HELM, ...(CONTEXT ? ["--kube-context", CONTEXT] : []), "--namespace", NAMESPACE, ...args]);

async function waitFor<T>(what: string, check: () => T | undefined | Promise<T | undefined>, timeoutMs = 120_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = await check();
    if (value !== undefined) return value;
    await Bun.sleep(2_000);
  }
  throw new Error(`timed out waiting for ${what}`);
}

async function freePort(): Promise<number> {
  const server = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data() {} } });
  const port = server.port;
  server.stop(true);
  return port;
}

const forwards: Subprocess[] = [];
// kubectl port-forward to a Service; returns the local base URL.
async function portForward(service: string, remotePort: number): Promise<string> {
  const port = await freePort();
  forwards.push(
    Bun.spawn([...kubectlArgs(), "port-forward", `service/${service}`, `${port}:${remotePort}`], {
      stdout: "ignore",
      stderr: "inherit",
    }),
  );
  await waitFor(`port-forward to ${service}`, async () => {
    try {
      const socket = await Bun.connect({ hostname: "127.0.0.1", port, socket: { data() {} } });
      socket.end();
      return true;
    } catch {
      return undefined;
    }
  });
  return `http://127.0.0.1:${port}`;
}

// Runs SQL in the ClickHouse pod as the bootstrap admin. The password stays in the pod: the client
// reads it from the mounted Secret file, and the SQL comes on stdin.
function clickhouse(sql: string): string {
  return kubectlIn(
    sql,
    "exec",
    "-i",
    `${PREFIX}-clickhouse-0`,
    "-c",
    "clickhouse",
    "--",
    "bash",
    "-c",
    'exec clickhouse-client --user "$CLICKHOUSE_USER" --password "$(cat /etc/trustiot/clickhouse-admin/password)" --multiquery',
  );
}
function clickhouseRows(sql: string): Record<string, unknown>[] {
  return clickhouse(`${sql} FORMAT JSONEachRow`)
    .split("\n")
    .filter((line) => line.trim())
    .map((line) => JSON.parse(line) as Record<string, unknown>);
}

function siemLogs(): string {
  return kubectl("logs", `deployment/${SIEM}`, "--tail=-1");
}

const SIEM_CONFIG = `extensions:
  bearertokenauth:
    scheme: Bearer
    token: \${env:SIEM_TOKEN}
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318
        auth:
          authenticator: bearertokenauth
exporters:
  debug:
    verbosity: detailed
service:
  extensions: [bearertokenauth]
  pipelines:
    logs: {receivers: [otlp], exporters: [debug]}
    traces: {receivers: [otlp], exporters: [debug]}
`;

// The stand-in SIEM: the same collector image, receiving OTLP/HTTP only with the bearer token and
// printing everything it gets. Restricted Pod Security like the chart's pods.
function siemManifests(): string {
  const labels = { "app.kubernetes.io/name": SIEM, "app.kubernetes.io/part-of": "trustiot-e2e" };
  const objects = [
    { apiVersion: "v1", kind: "ConfigMap", metadata: { name: SIEM, labels }, data: { "config.yaml": SIEM_CONFIG } },
    // The chart denies all incoming traffic in the namespace by default; a SIEM outside the
    // namespace would not need this. Only the collector's pods may connect (the SIEM is not in
    // the mesh, so the connection arrives from the collector's pod IP on the OTLP port).
    {
      apiVersion: "networking.k8s.io/v1",
      kind: "NetworkPolicy",
      metadata: { name: SIEM, labels },
      spec: {
        podSelector: { matchLabels: labels },
        policyTypes: ["Ingress"],
        ingress: [
          {
            from: [
              {
                podSelector: {
                  matchLabels: { "app.kubernetes.io/instance": RELEASE, "app.kubernetes.io/component": "otel-collector" },
                },
              },
            ],
            ports: [{ port: 4318, protocol: "TCP" }],
          },
        ],
      },
    },
    {
      apiVersion: "v1",
      kind: "Service",
      metadata: { name: SIEM, labels },
      spec: { type: "ClusterIP", selector: labels, ports: [{ name: "otlp-http", port: 4318, targetPort: 4318 }] },
    },
    {
      apiVersion: "apps/v1",
      kind: "Deployment",
      metadata: { name: SIEM, labels },
      spec: {
        replicas: 1,
        selector: { matchLabels: labels },
        template: {
          metadata: { labels },
          spec: {
            automountServiceAccountToken: false,
            securityContext: { runAsNonRoot: true, seccompProfile: { type: "RuntimeDefault" } },
            containers: [
              {
                name: "siem",
                image: COLLECTOR_IMAGE,
                args: ["--config=/etc/siem/config.yaml"],
                env: [{ name: "SIEM_TOKEN", valueFrom: { secretKeyRef: { name: SIEM, key: "token" } } }],
                ports: [{ containerPort: 4318 }],
                resources: { requests: { cpu: "20m", memory: "48Mi" }, limits: { memory: "256Mi" } },
                securityContext: {
                  allowPrivilegeEscalation: false,
                  readOnlyRootFilesystem: true,
                  capabilities: { drop: ["ALL"] },
                },
                volumeMounts: [{ name: "config", mountPath: "/etc/siem", readOnly: true }],
              },
            ],
            volumes: [{ name: "config", configMap: { name: SIEM } }],
          },
        },
      },
    },
  ];
  return objects.map((o) => JSON.stringify(o)).join("\n---\n");
}

function imageValues(): string[] {
  const match = /^(?<repository>[^@]+?)(?::(?<tag>[\w][\w.-]*))?(?:@(?<digest>sha256:[a-f0-9]{64}))?$/.exec(SERVICE_IMAGE);
  if (!match?.groups?.repository) throw new Error(`E2E_SERVICE_IMAGE is not an image reference: ${SERVICE_IMAGE}`);
  const { repository, tag, digest } = match.groups;
  return [
    "--set-string",
    `services.assets.image.repository=${repository}`,
    "--set-string",
    `services.assets.image.tag=${tag ?? "latest"}`,
    "--set-string",
    `services.assets.image.digest=${digest ?? ""}`,
    "--set-string",
    `services.assets.image.pullPolicy=${PULL_POLICY}`,
  ];
}

const work = mkdtempSync(join(tmpdir(), "trustiot-otel-e2e-"));
const created = { secrets: false, siem: false, release: false };
const report: Record<string, unknown> = { run: RUN, namespace: NAMESPACE, release: RELEASE, collectorImage: COLLECTOR_IMAGE };
const traceId = Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, "0")).join("");
let collectorUrl = "";
let siemToken = "";

function secretFile(name: string, value: string): string {
  const path = join(work, name);
  writeFileSync(path, value, { mode: 0o600 });
  return path;
}

function hex(bytes: number): string {
  return Array.from(crypto.getRandomValues(new Uint8Array(bytes)), (b) => b.toString(16).padStart(2, "0")).join("");
}

beforeAll(async () => {
  // Never touch an existing installation (the same check as cluster.ts, which this test does not
  // import because it requires the ingress test's variables).
  const status = Bun.spawnSync({
    cmd: [HELM, ...(CONTEXT ? ["--kube-context", CONTEXT] : []), "--namespace", NAMESPACE, "status", RELEASE],
    stdout: "ignore",
    stderr: "pipe",
  });
  if (status.exitCode === 0) {
    throw new Error(`release ${RELEASE} already exists in ${NAMESPACE}; use another E2E_RELEASE or namespace`);
  }
  if (!status.stderr.toString().includes("not found")) {
    throw new Error(`helm status ${RELEASE} failed:\n${status.stderr.toString()}`);
  }
  // Fresh passwords, written to files in a private temp dir so they never appear in argv.
  const collectorPassword = hex(24);
  siemToken = hex(24);
  created.secrets = true;
  kubectl("create", "secret", "generic", ADMIN_SECRET, `--from-file=password=${secretFile("admin", hex(24))}`);
  kubectl("create", "secret", "generic", COLLECTOR_SECRET, `--from-file=password=${secretFile("collector", collectorPassword)}`);
  kubectl(
    "create",
    "secret",
    "generic",
    SIEM,
    `--from-file=token=${secretFile("siem-token", siemToken)}`,
    `--from-file=header=${secretFile("siem-header", `Bearer ${siemToken}`)}`,
  );
  created.siem = true;
  kubectlIn(siemManifests(), "apply", "-f", "-");

  const base = [
    "--values",
    "values-onprem.yaml",
    "--values",
    "tests/values/onprem-instance.yaml",
    "--values",
    "tests/e2e/values-otel-e2e.yaml",
    "--set",
    `clickhouse.bundled.adminSecret.name=${ADMIN_SECRET}`,
    "--set",
    `observability.collector.clickhouse.passwordSecret.name=${COLLECTOR_SECRET}`,
    "--set",
    `observability.collector.clickhouse.user=${COLLECTOR_USER}`,
    ...imageValues(),
  ];
  // Step 1: ClickHouse only, so the schema and the collector's account exist before anything
  // writes (the migration Job, Laysi/trustiot-v2#60, will do this part on install).
  helm(
    "install",
    RELEASE,
    ".",
    ...base,
    "--set",
    "observability.collector.enabled=false",
    "--set",
    "services.assets.enabled=false",
    "--rollback-on-failure",
    "--wait",
    "--timeout",
    "8m",
  );
  created.release = true;
  const sql = readdirSync(MIGRATIONS)
    .filter((f) => f.endsWith(".sql"))
    .sort()
    .map((f) => readFileSync(join(MIGRATIONS, f), "utf8"))
    .join("\n");
  clickhouse(sql);
  // The two statements of 0005_logs.sql and deploy/chart/README.md.
  clickhouse(
    `CREATE USER IF NOT EXISTS ${COLLECTOR_USER} IDENTIFIED WITH sha256_password BY '${collectorPassword}';\n` +
      `GRANT trustiot_otel_collector TO ${COLLECTOR_USER};\n`,
  );
  // Step 2: the collector, forwarding logs and traces to the SIEM, and the example service.
  helm(
    "upgrade",
    RELEASE,
    ".",
    ...base,
    "--set",
    "observability.collector.forward.enabled=true",
    "--set",
    `observability.collector.forward.endpoint=http://${SIEM}.${NAMESPACE}.svc:4318`,
    "--set-json",
    'observability.collector.forward.signals=["logs","traces"]',
    "--set",
    "observability.collector.forward.authHeader.name=Authorization",
    "--set",
    `observability.collector.forward.authHeader.secret.name=${SIEM}`,
    "--set",
    "observability.collector.forward.authHeader.secret.key=header",
    "--rollback-on-failure",
    "--wait",
    "--timeout",
    "8m",
  );
  kubectl("rollout", "status", `deployment/${SIEM}`, "--timeout=180s");
  collectorUrl = await portForward(`${PREFIX}-otel-collector`, 4318);
}, 900_000);

afterAll(() => {
  for (const forward of forwards) forward.kill();
  if (REPORT_DIR) {
    mkdirSync(REPORT_DIR, { recursive: true });
    writeFileSync(join(REPORT_DIR, "otel-e2e.json"), `${JSON.stringify(report, null, 2)}\n`);
  }
  rmSync(work, { recursive: true, force: true });
  if (KEEP) return;
  const errors: string[] = [];
  const steps: [boolean, () => unknown][] = [
    [created.release, () => helm("uninstall", RELEASE, "--wait", "--timeout", "3m")],
    // The StatefulSet keeps its volume on uninstall (persistentVolumeClaimRetentionPolicy).
    [created.release, () => kubectl("delete", "pvc", `data-${PREFIX}-clickhouse-0`, "--ignore-not-found", "--wait=true")],
    [created.siem, () => kubectl("delete", "deployment,service,configmap,networkpolicy", SIEM, "--ignore-not-found", "--wait=true")],
    [created.secrets, () => kubectl("delete", "secret", ADMIN_SECRET, COLLECTOR_SECRET, SIEM, "--ignore-not-found")],
  ];
  for (const [done, cleanup] of steps) {
    if (!done) continue;
    try {
      cleanup();
    } catch (error) {
      errors.push(String(error));
    }
  }
  if (errors.length) throw new Error(`cleanup failed:\n${errors.join("\n")}`);
}, 300_000);

describe("log pipeline (decision 22)", () => {
  test("a request through the example service lands in ClickHouse as a structured record with its trace id", async () => {
    const service = await portForward(`${PREFIX}-assets`, 8080);
    // The request body carries values that must never be logged; the service logs only its length.
    const response = await fetch(`${service}/v1/echo`, {
      method: "POST",
      headers: { "content-type": "application/json", traceparent: `00-${traceId}-${hex(8)}-01` },
      body: JSON.stringify({ text: `${SECRETS.email} ${SECRETS.password} ${SECRETS.body}` }),
    });
    expect(response.status).toBe(200);
    const logs = await waitFor("the request's log record in ClickHouse", () => {
      const rows = clickhouseRows(
        `SELECT Timestamp, TraceId, SpanId, SeverityText, ServiceName, Body, LogAttributes, ReceivedAt
         FROM trustiot_logs.otel_logs WHERE TraceId = '${traceId}'`,
      );
      return rows.some((r) => r.Body === "request completed") ? rows : undefined;
    });
    const record = logs.find((r) => r.Body === "request completed") as Record<string, unknown>;
    expect(record.ServiceName).toBe("example-service");
    expect(record.SeverityText).toBe("INFO");
    expect(record.SpanId).toMatch(/^[0-9a-f]{16}$/);
    expect(record.LogAttributes).toMatchObject({
      "http.request.method": "POST",
      "http.route": "/v1/echo",
      "http.response.status_code": "200",
    });
    const spans = await waitFor("the request's span in ClickHouse", () => {
      const rows = clickhouseRows(
        `SELECT TraceId, SpanName, ServiceName, SpanAttributes FROM trustiot_logs.otel_traces WHERE TraceId = '${traceId}'`,
      );
      return rows.length ? rows : undefined;
    });
    expect(spans.map((s) => s.SpanName)).toContain("POST /v1/echo");
    // Nothing of the request body anywhere in the request's records.
    const stored = JSON.stringify([logs, spans]);
    for (const value of [SECRETS.email, SECRETS.password, SECRETS.body]) expect(stored).not.toContain(value);
    report.request = { traceId, logs, spans };
  }, 300_000);

  test("both log tables keep their rows for a limited time (TTL)", () => {
    const tables = clickhouseRows(
      "SELECT name, engine, engine_full FROM system.tables WHERE database = 'trustiot_logs' ORDER BY name",
    );
    expect(tables.map((t) => t.name)).toEqual(["otel_logs", "otel_traces"]);
    for (const table of tables) {
      expect(table.engine).toBe("MergeTree");
      expect(String(table.engine_full)).toContain("TTL ReceivedAt + toIntervalDay(30)");
    }
    report.tables = tables;
  });

  test("the SIEM receives the same records, and only with the auth header", async () => {
    const received = await waitFor("the request's records at the SIEM", () => {
      const logs = siemLogs();
      return logs.includes(`Trace ID: ${traceId}`) && logs.includes("Body: Str(request completed)") ? logs : undefined;
    });
    const excerpt = received
      .split("\n")
      .filter((line) => line.includes(traceId) || line.includes("request completed") || line.includes("http.route"))
      .slice(0, 20);
    // Without the header the SIEM refuses the export, so the records above came authenticated.
    const siem = await portForward(SIEM, 4318);
    const unauthenticated = await fetch(`${siem}/v1/logs`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ resourceLogs: [] }),
    });
    expect(unauthenticated.status).toBe(401);
    report.siem = { excerpt, unauthenticatedStatus: unauthenticated.status };
  }, 300_000);

  test("email, tokens, password and request body are redacted in ClickHouse and at the SIEM", async () => {
    const marker = `redaction-${RUN}`;
    const now = `${Date.now()}000000`;
    const s = (value: string) => ({ stringValue: value });
    const logs = {
      resourceLogs: [
        {
          resource: { attributes: [{ key: "service.name", value: s("redaction-e2e") }] },
          scopeLogs: [
            {
              scope: { name: "redaction-e2e" },
              logRecords: [
                {
                  timeUnixNano: now,
                  severityText: "WARN",
                  severityNumber: 13,
                  body: s(
                    `login failed for ${SECRETS.email} with Authorization: ${SECRETS.bearer} token ${SECRETS.jwt} password=${SECRETS.password} phone ${SECRETS.phone}`,
                  ),
                  attributes: [
                    { key: "test.marker", value: s(marker) },
                    { key: "user.email", value: s(SECRETS.email) },
                    { key: "token", value: s(`opaque-${RUN}`) },
                    { key: "http.request.body", value: s(SECRETS.body) },
                    { key: "http.request.body.size", value: { intValue: String(SECRETS.body.length) } },
                    { key: "note", value: s(`forwarded by ${SECRETS.email}`) },
                    { key: "http.route", value: s("/v1/echo") },
                  ],
                },
              ],
            },
          ],
        },
      ],
    };
    const traces = {
      resourceSpans: [
        {
          resource: { attributes: [{ key: "service.name", value: s("redaction-e2e") }] },
          scopeSpans: [
            {
              scope: { name: "redaction-e2e" },
              spans: [
                {
                  traceId: hex(16),
                  spanId: hex(8),
                  name: `GET /users/${SECRETS.email}`,
                  kind: 2,
                  startTimeUnixNano: now,
                  endTimeUnixNano: now,
                  attributes: [
                    { key: "test.marker", value: s(marker) },
                    { key: "http.request.header.authorization", value: s(SECRETS.bearer) },
                    { key: "url.full", value: s(`https://x.test/cb?token=${SECRETS.jwt}`) },
                  ],
                  events: [
                    {
                      timeUnixNano: now,
                      name: "exception",
                      attributes: [{ key: "exception.message", value: s(`bad password=${SECRETS.password}`) }],
                    },
                  ],
                  status: { code: 2, message: `failed for ${SECRETS.email}` },
                },
              ],
            },
          ],
        },
      ],
    };
    for (const [path, payload] of [
      ["/v1/logs", logs],
      ["/v1/traces", traces],
    ] as const) {
      const response = await fetch(`${collectorUrl}${path}`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(payload),
      });
      expect(response.status).toBe(200);
    }
    const logRows = await waitFor("the redacted log in ClickHouse", () => {
      const rows = clickhouseRows(
        `SELECT Body, LogAttributes FROM trustiot_logs.otel_logs WHERE LogAttributes['test.marker'] = '${marker}'`,
      );
      return rows.length ? rows : undefined;
    });
    const spanRows = await waitFor("the redacted span in ClickHouse", () => {
      const rows = clickhouseRows(
        `SELECT SpanName, SpanAttributes, StatusMessage, Events.Attributes AS EventAttributes
         FROM trustiot_logs.otel_traces WHERE SpanAttributes['test.marker'] = '${marker}'`,
      );
      return rows.length ? rows : undefined;
    });
    const log = logRows[0] as { Body: string; LogAttributes: Record<string, string> };
    expect(log.Body).toBe("login failed for **** with Authorization: **** token **** **** phone ****");
    expect(log.LogAttributes).toMatchObject({
      "user.email": "****",
      token: "****",
      "http.request.body": "****",
      "http.request.body.size": String(SECRETS.body.length),
      note: "forwarded by ****",
      "http.route": "/v1/echo",
    });
    const span = spanRows[0] as {
      SpanName: string;
      SpanAttributes: Record<string, string>;
      StatusMessage: string;
      EventAttributes: Record<string, string>[];
    };
    expect(span.SpanName).toBe("GET /users/****");
    expect(span.StatusMessage).toBe("failed for ****");
    expect(span.SpanAttributes["http.request.header.authorization"]).toBe("****");
    expect(span.EventAttributes[0]?.["exception.message"]).toBe("bad ****");

    const stored = JSON.stringify([logRows, spanRows]);
    const siem = await waitFor("the redacted records at the SIEM", () => {
      const output = siemLogs();
      return output.includes(`Str(${marker})`) && output.includes("GET /users/****") ? output : undefined;
    });
    for (const value of Object.values(SECRETS)) {
      expect(stored).not.toContain(value);
      expect(siem).not.toContain(value);
    }
    report.redaction = {
      // What was sent is described in the report, not stored: it looks like credentials.
      clickhouse: { logs: logRows, spans: spanRows },
      siemExcerpt: siem.split("\n").filter((line) => line.includes("****")).slice(0, 20),
    };
  }, 300_000);

  test("the collector runs under the restricted Pod Security Standard", () => {
    const pods = JSON.parse(
      kubectl("get", "pods", "-l", "app.kubernetes.io/component=otel-collector", "-o", "json"),
    ) as { items: { spec: { securityContext: unknown; containers: { securityContext: unknown }[] }; status: { phase: string } }[] };
    expect(pods.items.length).toBe(1);
    const pod = pods.items[0];
    expect(pod?.status.phase).toBe("Running");
    expect(pod?.spec.securityContext).toEqual({ runAsNonRoot: true, seccompProfile: { type: "RuntimeDefault" } });
    expect(pod?.spec.containers[0]?.securityContext).toEqual({
      allowPrivilegeEscalation: false,
      readOnlyRootFilesystem: true,
      capabilities: { drop: ["ALL"] },
    });
    report.collectorPod = pod?.spec.securityContext;
  });
});
