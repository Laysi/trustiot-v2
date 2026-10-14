import { describe, expect, test } from "bun:test";
import { join } from "node:path";

import { pinnedTool } from "./exec.ts";
import {
  gatewayPortProblems,
  gatewayPorts,
  gatewaysOf,
  healthEndpointsOf,
  keptObjectsOf,
  MIGRATION_WAIT_CONTAINER,
  migrationJobsOf,
  migrationOrderProblems,
  missingTlsSecrets,
  missingSecrets,
  passwordSecretProblems,
  passwordSecretsOf,
  operatorSecretsOf,
  postgresClusterProblems,
  postgresClustersOf,
  parseManifests,
  podProblems,
  restartsOf,
  readyEndpointsFor,
  workloadsOf,
} from "./manifests.ts";
import { discoverShapes, valuesArgs } from "./smoke.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");
const CHART_DIR = join(REPO_ROOT, "deploy", "chart");

const MANIFEST = `
---
# Source: trustiot/templates/services.yaml
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  selector:
    app: web
  ports:
    - name: http
      port: 80
      targetPort: app
    - name: metrics
      port: 9090
      targetPort: 9090
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    metadata:
      labels:
        app: web
        extra: label
    spec:
      containers:
        - name: web
          ports:
            - name: app
              containerPort: 8080
            - name: metrics
              containerPort: 9090
          env:
            - name: A
              valueFrom:
                secretKeyRef: { name: creds, key: password }
            - name: B
              valueFrom:
                secretKeyRef: { name: maybe, key: x, optional: true }
            - name: OWN
              valueFrom:
                secretKeyRef: { name: made-here, key: k }
          envFrom:
            - secretRef: { name: bulk }
          readinessProbe:
            httpGet: { path: /readyz, port: app }
          livenessProbe:
            httpGet: { path: /healthz, port: 8080, scheme: HTTPS }
          startupProbe:
            tcpSocket: { port: app }
      volumes:
        - name: ca
          secret:
            secretName: ca-bundle
            items:
              - { key: ca.crt, path: ca.crt }
---
apiVersion: v1
kind: Secret
metadata:
  name: made-here
---
apiVersion: batch/v1
kind: Job
metadata:
  name: migrate
spec:
  template:
    spec:
      containers:
        - name: m
          image: example
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: cfg
`;

describe("parseManifests", () => {
  test("splits documents and drops empty ones", () => {
    const objects = parseManifests("---\n# nothing\n---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: a\n---\n");
    expect(objects.map((o) => o.kind)).toEqual(["ConfigMap"]);
  });

  test("reads a single document", () => {
    expect(parseManifests("apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: a\n")).toHaveLength(1);
  });

  test("rejects a document without a kind", () => {
    expect(() => parseManifests("metadata:\n  name: a\n")).toThrow("no kind");
  });

  test("flattens a List", () => {
    const objects = parseManifests(
      "kind: List\nitems:\n  - kind: Deployment\n    metadata: { name: a }\n  - kind: Job\n    metadata: { name: b }\n",
    );
    expect(workloadsOf(objects).map((w) => `${w.kind}/${w.name}`)).toEqual(["Deployment/a", "Job/b"]);
  });
});

describe("workloadsOf", () => {
  test("lists the objects that run pods and have to settle", () => {
    expect(workloadsOf(parseManifests(MANIFEST)).map((w) => `${w.kind}/${w.name}`)).toEqual([
      "Deployment/web",
      "Job/migrate",
    ]);
  });
});

describe("password Secrets of the migration Job", () => {
  const job = (env: { name: string; value: string }[]) =>
    parseManifests(`apiVersion: batch/v1
kind: Job
metadata: { name: rel-migrate-1, labels: { app.kubernetes.io/component: migrate } }
spec:
  template:
    spec:
      containers:
        - name: migrate
          env: ${JSON.stringify(env)}
`);

  test("are read from the Job's bindings, with the keys it writes", () => {
    const objects = job([
      { name: "TRUSTIOT__PASSWORDS__CLICKHOUSE_COLLECTOR", value: "trustiot_collector=rel-otel-collector-password" },
      { name: "TRUSTIOT__PASSWORDS__POSTGRES", value: "assets_app=rel-assets-password,identity_app=rel-identity-password" },
      { name: "TRUSTIOT__WAIT__DATABASES", value: "10m" },
    ]);
    expect(Object.fromEntries(passwordSecretsOf(objects))).toEqual({
      "rel-otel-collector-password": ["trustiot_collector"],
      "rel-assets-password": ["assets_app"],
      "rel-identity-password": ["identity_app"],
    });
    expect(passwordSecretsOf(job([])).size).toBe(0);
  });

  test("include the bundled ClickHouse's generated admin password, keyed by the account the Job logs in as", () => {
    const objects = job([
      { name: "TRUSTIOT__CLICKHOUSE__USERNAME", value: "trustiot_admin" },
      { name: "TRUSTIOT__CLICKHOUSE__PASSWORD_SECRET", value: "rel-clickhouse-admin-password" },
      { name: "TRUSTIOT__PASSWORDS__CLICKHOUSE_COLLECTOR", value: "trustiot_collector=rel-otel-collector-password" },
    ]);
    expect(Object.fromEntries(passwordSecretsOf(objects))).toEqual({
      "rel-clickhouse-admin-password": ["trustiot_admin"],
      "rel-otel-collector-password": ["trustiot_collector"],
    });
    // A password in an existing Secret (clickhouse.bundled.adminSecret) or an external
    // ClickHouse's migration account is not one the release generates.
    expect(passwordSecretsOf(job([{ name: "TRUSTIOT__CLICKHOUSE__USERNAME", value: "trustiot_admin" }])).size).toBe(0);
  });

  test("hold a generated password under every key, named without its value", () => {
    const generated = Buffer.from("ab".repeat(32)).toString("base64");
    const secret = (data: Record<string, string>, annotated = true) => ({
      metadata: annotated ? { annotations: { "trustiot.io/password-generation": "1" } } : {},
      data,
    });
    expect(passwordSecretProblems("s", ["a_app"], secret({ a_app: generated }))).toEqual([]);
    expect(passwordSecretProblems("s", ["a_app"], secret({}))).toEqual(["s: key a_app is missing"]);
    const short = Buffer.from("not-generated").toString("base64");
    const problems = passwordSecretProblems("s", ["a_app"], secret({ a_app: short }, false));
    expect(problems).toEqual(["s: key a_app is not a password the Job generates", "s: no trustiot.io/password-generation annotation"]);
    expect(problems.join(" ")).not.toContain(short);
    expect(problems.join(" ")).not.toContain("not-generated");
  });
});

describe("missingSecrets", () => {
  test("returns the Secrets the pods read that the release does not create", () => {
    const missing = missingSecrets(parseManifests(MANIFEST));
    expect([...missing.keys()].sort()).toEqual(["bulk", "ca-bundle", "creds"]);
    expect([...(missing.get("creds") ?? [])]).toEqual(["password"]);
    expect([...(missing.get("ca-bundle") ?? [])]).toEqual(["ca.crt"]);
    // envFrom names no key; a placeholder keeps the Secret non-empty.
    expect([...(missing.get("bulk") ?? [])]).toEqual(["placeholder"]);
  });

  test("merges the keys of one Secret and ignores optional references", () => {
    const objects = parseManifests(`
apiVersion: apps/v1
kind: StatefulSet
metadata: { name: db }
spec:
  template:
    spec:
      initContainers:
        - name: init
          env:
            - name: A
              valueFrom: { secretKeyRef: { name: s, key: one } }
      containers:
        - name: c
          env:
            - name: B
              valueFrom: { secretKeyRef: { name: s, key: two } }
`);
    expect([...(missingSecrets(objects).get("s") ?? [])].sort()).toEqual(["one", "two"]);
  });
});

describe("CloudNativePG", () => {
  const objects = parseManifests(`
apiVersion: postgresql.cnpg.io/v1
kind: ImageCatalog
metadata:
  name: rel-postgres
  annotations: { helm.sh/resource-policy: keep }
---
apiVersion: postgresql.cnpg.io/v1
kind: Cluster
metadata:
  name: rel-postgres
  annotations: { helm.sh/resource-policy: keep }
spec: { instances: 1 }
---
apiVersion: postgresql.cnpg.io/v1
kind: Database
metadata: { name: rel-postgres-trustiot }
---
apiVersion: cluster.x-k8s.io/v1beta1
kind: Cluster
metadata: { name: not-postgres }
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: kept-volume
  annotations: { helm.sh/resource-policy: keep }
---
apiVersion: apps/v1
kind: Deployment
metadata: { name: app }
spec:
  template:
    spec:
      containers:
        - name: app
      volumes:
        - name: postgres-ca
          secret: { secretName: rel-postgres-ca, items: [{ key: ca.crt, path: ca.crt }] }
        - name: other
          secret: { secretName: other-ca }
`);

  test("finds the release's CloudNativePG Clusters only", () => {
    expect(postgresClustersOf(objects)).toEqual(["rel-postgres"]);
  });

  test("never creates a dummy for a Secret the operator makes, so the operator can make its own", () => {
    expect(operatorSecretsOf(objects)).toEqual([
      "rel-postgres-ca",
      "rel-postgres-server",
      "rel-postgres-app",
      "rel-postgres-replication",
      "rel-postgres-superuser",
    ]);
    expect([...missingSecrets(objects).keys()]).toEqual(["other-ca"]);
  });

  test("names the kept objects as kubectl prints them, Clusters first", () => {
    expect(keptObjectsOf(objects)).toEqual([
      "cluster.postgresql.cnpg.io/rel-postgres",
      "imagecatalog.postgresql.cnpg.io/rel-postgres",
      "persistentvolumeclaim/kept-volume",
    ]);
  });

  test("a Cluster is healthy only in the operator's healthy phase, with every instance ready and Ready true", () => {
    const cluster = (status: Record<string, unknown>, instances = 1) => ({ metadata: { name: "c" }, spec: { instances }, status });
    const healthy = { phase: "Cluster in healthy state", readyInstances: 1, conditions: [{ type: "Ready", status: "True" }] };
    expect(postgresClusterProblems(cluster(healthy))).toEqual([]);
    expect(postgresClusterProblems(cluster({ ...healthy, phase: "Setting up primary" }))).toEqual(['c: phase "Setting up primary"']);
    expect(postgresClusterProblems(cluster(healthy, 2))).toEqual(["c: 1 of 2 instances ready"]);
    expect(postgresClusterProblems(cluster({ ...healthy, conditions: [{ type: "Ready", status: "False" }] }))).toEqual([
      "c: condition Ready is False",
    ]);
    expect(postgresClusterProblems({ metadata: { name: "c" } })).toEqual([
      "c: phase null",
      "c: 0 of ? instances ready",
      "c: condition Ready is missing",
    ]);
  });
});

describe("healthEndpointsOf", () => {
  test("resolves probes to the Service ports that reach them", () => {
    const endpoints = healthEndpointsOf(parseManifests(MANIFEST));
    expect(endpoints.map((e) => `${e.probe} ${e.scheme} ${e.service}:${e.port}${e.path}`)).toEqual([
      "liveness https web:http/healthz",
      "readiness http web:http/readyz",
    ]);
  });

  test("ignores probes that no Service reaches and non-HTTP probes", () => {
    const objects = parseManifests(`
apiVersion: v1
kind: Service
metadata: { name: other }
spec:
  selector: { app: other }
  ports: [{ name: http, port: 80, targetPort: 8080 }]
---
apiVersion: apps/v1
kind: Deployment
metadata: { name: web }
spec:
  template:
    metadata: { labels: { app: web } }
    spec:
      containers:
        - name: web
          readinessProbe: { httpGet: { path: /readyz, port: 8080 } }
          livenessProbe: { exec: { command: [true] } }
`);
    expect(healthEndpointsOf(objects)).toEqual([]);
  });

});

describe("readyEndpointsFor", () => {
  const slice = (ports: { name?: string; port: number }[], ready: boolean[]) => ({
    ports,
    endpoints: ready.map((r, i) => ({ addresses: [`10.42.0.${i + 10}`], conditions: { ready: r } })),
  });

  test("counts the ready endpoints of the slices that serve the named port", () => {
    const list = { items: [slice([{ name: "http", port: 8080 }], [true, false, true]), slice([{ name: "native", port: 9000 }], [true])] };
    expect(readyEndpointsFor(list, "http")).toBe(2);
    expect(readyEndpointsFor(list, "native")).toBe(1);
    expect(readyEndpointsFor(list, "metrics")).toBe(0);
  });

  test("an unnamed Service port is listed with an empty name", () => {
    expect(readyEndpointsFor({ items: [slice([{ name: "", port: 8080 }], [true])] }, "8080")).toBe(1);
    expect(readyEndpointsFor({ items: [slice([{ name: "", port: 8080 }], [true])] }, "http")).toBe(0);
  });

  test("an endpoint whose ready condition is unset or null counts as ready", () => {
    const list = {
      items: [
        {
          ports: [{ name: "http", port: 8080 }],
          endpoints: [{ addresses: ["10.42.0.10"] }, { addresses: ["10.42.0.11"], conditions: {} }, { addresses: ["10.42.0.12"], conditions: { ready: null } }],
        },
      ],
    };
    expect(readyEndpointsFor(list, "http")).toBe(3);
  });

  test("a slice whose port has no number serves nothing", () => {
    const list = { items: [{ ports: [{ name: "http", port: null }], endpoints: [{ addresses: ["10.42.0.10"], conditions: { ready: true } }] }] };
    expect(readyEndpointsFor(list, "http")).toBe(0);
  });

  test("no slice, or only endpoints that are not ready, gives 0", () => {
    expect(readyEndpointsFor({}, "http")).toBe(0);
    expect(readyEndpointsFor({ items: [slice([{ name: "http", port: 8080 }], [false])] }, "http")).toBe(0);
  });
});

describe("migration order (Laysi/trustiot-v2#60)", () => {
  const job = { status: { completionTime: "2026-10-13T03:00:10Z" } };
  const waiter = (name: string, containerStatuses: unknown[]) => ({
    metadata: { name },
    spec: { initContainers: [{ name: MIGRATION_WAIT_CONTAINER }], containers: [{ name: "svc" }] },
    status: { containerStatuses },
  });
  const started = (at: string) => ({ name: "svc", state: { running: { startedAt: at } } });

  test("the chart's migration Job is found by its component label", () => {
    const objects = parseManifests(`
apiVersion: batch/v1
kind: Job
metadata:
  name: trustiot-migrate-0b4fb1a7
  labels:
    app.kubernetes.io/component: migrate
---
apiVersion: batch/v1
kind: Job
metadata:
  name: other
`);
    expect(migrationJobsOf(objects)).toEqual(["trustiot-migrate-0b4fb1a7"]);
  });

  test("services that started at or after the Job's completion pass; pods without the init container are ignored", () => {
    const pods = {
      items: [
        waiter("a", [started("2026-10-13T03:00:10Z")]),
        waiter("b", [started("2026-10-13T03:00:42Z")]),
        { metadata: { name: "clickhouse-0" }, spec: { containers: [{ name: "c" }] }, status: { containerStatuses: [started("2026-10-13T02:59:00Z")] } },
      ],
    };
    expect(migrationOrderProblems(job, pods)).toEqual([]);
  });

  test("a container that started before completion, also in an earlier run, is reported", () => {
    const pods = {
      items: [
        waiter("early", [started("2026-10-13T03:00:09Z")]),
        waiter("restarted", [
          { name: "svc", state: { running: { startedAt: "2026-10-13T03:01:00Z" } }, lastState: { terminated: { startedAt: "2026-10-13T02:59:59Z" } } },
        ]),
        waiter("pending", [{ name: "svc", state: { waiting: { reason: "PodInitializing" } } }]),
      ],
    };
    const problems = migrationOrderProblems(job, pods);
    expect(problems).toHaveLength(3);
    expect(problems[0]).toStartWith("early/svc started 2026-10-13T03:00:09.000Z, before the Job completed");
    expect(problems[1]).toStartWith("restarted/svc started 2026-10-13T02:59:59.000Z");
    expect(problems[2]).toBe("pending/svc has not started");
  });

  test("a Job without completionTime, or no waiting pod, is a failure", () => {
    expect(migrationOrderProblems({ status: {} }, { items: [] })).toEqual(["the migration Job has no completionTime: it has not completed"]);
    expect(migrationOrderProblems(job, { items: [] })).toEqual([`no pod has the ${MIGRATION_WAIT_CONTAINER} init container`]);
  });
});

describe("pods", () => {
  const pod = (name: string, status: Record<string, unknown>) => ({ metadata: { name }, status });

  test("a running, ready pod and a completed one are fine", () => {
    const list = {
      items: [
        pod("a", { phase: "Running", containerStatuses: [{ name: "c", ready: true, restartCount: 0 }] }),
        pod("job", { phase: "Succeeded" }),
      ],
    };
    expect(podProblems(list)).toEqual([]);
    expect(restartsOf(list).size).toBe(0);
  });

  test("reports crash loops, image pull errors and unready pods", () => {
    const list = {
      items: [
        pod("crash", {
          phase: "Running",
          containerStatuses: [{ name: "c", ready: false, restartCount: 4, state: { waiting: { reason: "CrashLoopBackOff" } } }],
        }),
        pod("pull", {
          phase: "Pending",
          containerStatuses: [{ name: "c", ready: false, state: { waiting: { reason: "ImagePullBackOff" } } }],
        }),
        pod("slow", { phase: "Running", containerStatuses: [{ name: "c", ready: false, restartCount: 0 }] }),
      ],
    };
    const problems = podProblems(list).map((p) => `${p.pod}: ${p.problem}`);
    expect(problems).toContain("crash: c is waiting: CrashLoopBackOff");
    expect(problems).toContain("pull: c is waiting: ImagePullBackOff");
    expect(problems).toContain("pull: phase Pending");
    expect(problems).toContain("slow: a container is not ready");
    expect(restartsOf(list).get("crash")).toBe(4);
  });
});

describe("the chart as deployed by the nightly", () => {
  test("discoverShapes finds both deployment shapes with their overlays", () => {
    const shapes = discoverShapes(CHART_DIR);
    expect(shapes.map((s) => s.name)).toEqual(["onprem", "saas"]);
    expect(shapes[0]?.valuesFiles).toEqual([
      "values-onprem.yaml",
      "tests/values/onprem-instance.yaml",
      "tests/values/smoke.yaml",
    ]);
    expect(valuesArgs(CHART_DIR, shapes[1]!).slice(0, 2)).toEqual(["-f", join(CHART_DIR, "values-saas.yaml")]);
  });

  for (const shape of ["saas", "onprem"]) {
    test(`values-${shape}.yaml renders the stand-in workload with its health endpoints`, async () => {
      const helm = await pinnedTool("helm", REPO_ROOT);
      const target = discoverShapes(CHART_DIR).find((s) => s.name === shape);
      expect(target).toBeDefined();
      if (target === undefined) return;
      const rendered = Bun.spawnSync({
        cmd: [helm, "template", "trustiot", CHART_DIR, "--namespace", `nightly-${shape}`, ...valuesArgs(CHART_DIR, target)],
        stdout: "pipe",
        stderr: "pipe",
      });
      expect(rendered.stderr.toString()).toBe("");
      const objects = parseManifests(rendered.stdout.toString());
      expect(workloadsOf(objects).map((w) => w.name)).toContain("trustiot-smoke-echo");
      // Components the chart gains later add workloads and endpoints of their own (the bundled
      // ClickHouse answers /ping); the stand-in service's pair is always there.
      const endpoints = healthEndpointsOf(objects);
      const smokeEcho = endpoints.filter((e) => e.service === "trustiot-smoke-echo");
      expect(smokeEcho.map((e) => e.path).sort()).toEqual(["/healthz", "/readyz"]);
      expect(smokeEcho.every((e) => e.port === "http")).toBe(true);
      // Every endpoint belongs to a workload of the release.
      const workloadNames = new Set(workloadsOf(objects).map((w) => w.name));
      expect(endpoints.every((e) => workloadNames.has(e.workload))).toBe(true);
      // The bundled Postgres: its Cluster is checked for health, kept on uninstall and then deleted.
      expect(postgresClustersOf(objects)).toEqual(["trustiot-postgres"]);
      // The password Secrets the migration Job fills are kept too (Laysi/trustiot-v2#60); the
      // smoke values enable the collector, whose ClickHouse password the Job generates.
      // So is the bundled ClickHouse's admin password Secret, which its init container fills and
      // the Job reads (Laysi/trustiot-v2#403).
      expect(keptObjectsOf(objects)).toEqual([
        "cluster.postgresql.cnpg.io/trustiot-postgres",
        "secret/trustiot-otel-collector-password",
        "secret/trustiot-clickhouse-admin-password",
        "imagecatalog.postgresql.cnpg.io/trustiot-postgres",
      ]);
      expect([...passwordSecretsOf(objects)]).toEqual([
        ["trustiot-clickhouse-admin-password", ["trustiot_admin"]],
        ["trustiot-otel-collector-password", ["trustiot_collector"]],
      ]);
      // The empty Secrets are the release's own: no dummy is made for them.
      expect(missingSecrets(objects).has("trustiot-otel-collector-password")).toBe(false);
      expect(missingSecrets(objects).has("trustiot-clickhouse-admin-password")).toBe(false);
    }, 60_000);
  }
});

describe("gateways", () => {
  const GATEWAYS = `
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: { name: ingress }
spec:
  gatewayClassName: istio
  listeners:
    - name: http
      port: 80
      protocol: HTTP
    - name: https
      port: 443
      protocol: HTTPS
      tls:
        mode: Terminate
        certificateRefs:
          - { kind: Secret, group: "", name: ingress-tls }
          - { name: made-here }
          - { kind: ConfigMap, name: not-a-secret }
---
apiVersion: v1
kind: Secret
metadata: { name: made-here }
---
apiVersion: example.test/v1
kind: Gateway
metadata: { name: other-kind-of-gateway }
`;

  test("gatewaysOf lists Gateway API gateways only", () => {
    expect(gatewaysOf(parseManifests(GATEWAYS))).toEqual(["ingress"]);
  });

  test("missingTlsSecrets lists the certificate Secrets the release does not create", () => {
    expect(missingTlsSecrets(parseManifests(GATEWAYS))).toEqual(["ingress-tls"]);
  });
});

describe("gateway ports", () => {
  // What Istio 1.30.5 generates for a Gateway (`kubectl get deployments,services -l
  // gateway.networking.k8s.io/gateway-name=<name> -o json`), trimmed to the fields the check reads;
  // captured on the lab cluster (docs/test-reports/2026-10-11-287-gateway-netpol-ports.md).
  function istioGenerated(gateway: string, listener: { name: string; port: number }, infrastructure: Record<string, string> = {}) {
    const labels = {
      ...infrastructure,
      "gateway.istio.io/managed": "istio.io-gateway-controller",
      "gateway.networking.k8s.io/gateway-class-name": "istio",
      "gateway.networking.k8s.io/gateway-name": gateway,
      "istio.io/dataplane-mode": "none",
    };
    const probe = { httpGet: { path: "/healthz/ready", port: 15021, scheme: "HTTP" } };
    return JSON.stringify({
      apiVersion: "v1",
      kind: "List",
      items: [
        {
          apiVersion: "apps/v1",
          kind: "Deployment",
          metadata: { name: `${gateway}-istio`, labels },
          spec: {
            template: {
              metadata: { labels: { ...labels, "sidecar.istio.io/inject": "false" } },
              spec: {
                containers: [
                  {
                    name: "istio-proxy",
                    ports: [
                      { containerPort: 15020, name: "metrics", protocol: "TCP" },
                      { containerPort: 15021, name: "status-port", protocol: "TCP" },
                      { containerPort: 15090, name: "http-envoy-prom", protocol: "TCP" },
                    ],
                    startupProbe: probe,
                    readinessProbe: probe,
                  },
                ],
              },
            },
          },
        },
        {
          apiVersion: "v1",
          kind: "Service",
          metadata: { name: `${gateway}-istio`, labels },
          spec: {
            ports: [
              { name: "status-port", port: 15021, protocol: "TCP", targetPort: 15021 },
              { name: listener.name, port: listener.port, protocol: "TCP", targetPort: listener.port },
            ],
          },
        },
      ],
    });
  }

  for (const [shape, listener] of [
    ["saas", { name: "http", port: 80 }],
    ["onprem", { name: "https", port: 443 }],
  ] as const) {
    test(`values-${shape}.yaml: the gateway's NetworkPolicy admits exactly what Istio's gateway pods use`, async () => {
      const helm = await pinnedTool("helm", REPO_ROOT);
      const target = discoverShapes(CHART_DIR).find((s) => s.name === shape);
      if (target === undefined) throw new Error(`no ${shape} shape`);
      const rendered = Bun.spawnSync({
        cmd: [helm, "template", "trustiot", CHART_DIR, "--namespace", `nightly-${shape}`, ...valuesArgs(CHART_DIR, target)],
        stdout: "pipe",
        stderr: "pipe",
      });
      expect(rendered.stderr.toString()).toBe("");
      const release = parseManifests(rendered.stdout.toString());
      const [gateway] = gatewaysOf(release);
      if (gateway === undefined) throw new Error("the release has no Gateway");
      // Istio copies the Gateway's infrastructure labels onto the pods, which the policy may select on.
      const spec = release.find((o) => o.kind === "Gateway")?.spec as { infrastructure?: { labels?: Record<string, string> } };
      const generated = parseManifests(istioGenerated(gateway, listener, spec.infrastructure?.labels));
      const ports = gatewayPorts(gateway, generated, release);
      expect(ports).toEqual({ gateway, needed: [listener.port, 15021], admitted: [listener.port, 15021] });
      expect(gatewayPortProblems(ports)).toEqual([]);
    }, 60_000);
  }

  const POLICY_HEADER = `
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: { name: deny-all }
spec: { podSelector: {}, policyTypes: [Ingress] }
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: { name: other-pods }
spec:
  podSelector: { matchLabels: { app: other } }
  ingress: [{}]
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: { name: gw }
spec:
  podSelector: { matchLabels: { "gateway.networking.k8s.io/gateway-name": gw } }
  policyTypes: [Ingress]
`;
  const generated = parseManifests(istioGenerated("gw", { name: "http", port: 80 }));
  const check = (rules: string) => gatewayPorts("gw", generated, parseManifests(`${POLICY_HEADER}  ingress:\n${rules}`));

  test("a policy that admits the listener and the status port by name or number passes; other pods' policies do not count", () => {
    const ports = check(`
    - from: [{ ipBlock: { cidr: 10.42.0.1/32 } }]
      ports: [{ port: 80 }, { port: status-port, protocol: TCP }]
`);
    expect(ports).toEqual({ gateway: "gw", needed: [80, 15021], admitted: [80, 15021] });
    expect(gatewayPortProblems(ports)).toEqual([]);
  });

  test("a rule without ports admits every port", () => {
    const ports = check(`
    - from: [{ ipBlock: { cidr: 10.42.0.1/32 } }]
`);
    expect(ports.admitted).toBe("all");
    expect(gatewayPortProblems(ports)).toEqual(["Gateway gw: a NetworkPolicy rule admits every port"]);
  });

  test("a refused status port and an admitted metrics range are both reported; UDP does not count", () => {
    const ports = check(`
    - ports: [{ port: 80 }, { port: 15020, endPort: 15021, protocol: UDP }, { port: 15090, endPort: 15091 }]
`);
    expect(ports.admitted).toEqual([80, 15090, 15091]);
    expect(gatewayPortProblems(ports)).toEqual([
      "Gateway gw: the NetworkPolicy refuses 15021, which the pods' probes or Service use",
      "Gateway gw: the NetworkPolicy admits 15090, 15091, which neither a probe nor the Service uses",
    ]);
  });

  test("a gateway without its generated Deployment or Service, or a selector it cannot evaluate, is an error", () => {
    const service = parseManifests(istioGenerated("gw", { name: "http", port: 80 })).flatMap((o) =>
      o.kind === "List" ? ((o.items as typeof generated) ?? []) : [o],
    );
    expect(() => gatewayPorts("gw", service.filter((o) => o.kind === "Service"), [])).toThrow("expected one Deployment");
    expect(() => gatewayPorts("gw", service.filter((o) => o.kind === "Deployment"), [])).toThrow("no Service");
    const expressions = parseManifests(`
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: { name: by-expression }
spec:
  podSelector: { matchExpressions: [{ key: app, operator: Exists }] }
`);
    expect(() => gatewayPorts("gw", generated, expressions)).toThrow("matchExpressions");
  });
});
