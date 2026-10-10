import { describe, expect, test } from "bun:test";
import { join } from "node:path";

import { pinnedTool } from "./exec.ts";
import {
  gatewaysOf,
  healthEndpointsOf,
  missingTlsSecrets,
  missingSecrets,
  parseManifests,
  podProblems,
  restartsOf,
  serviceProxyPath,
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

  test("builds the API server proxy path", () => {
    const readiness = healthEndpointsOf(parseManifests(MANIFEST)).find((e) => e.probe === "readiness");
    expect(readiness).toBeDefined();
    if (readiness === undefined) return;
    expect(serviceProxyPath("ns", readiness)).toBe("/api/v1/namespaces/ns/services/web:http/proxy/readyz");
    expect(serviceProxyPath("ns", { ...readiness, scheme: "https", path: "healthz" })).toBe(
      "/api/v1/namespaces/ns/services/https:web:http/proxy/healthz",
    );
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
    test(`values-${shape}.yaml renders a workload, a health endpoint and nothing that needs a missing Secret to be real`, async () => {
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
      const endpoints = healthEndpointsOf(objects);
      expect(endpoints.map((e) => e.path).sort()).toEqual(["/healthz", "/readyz"]);
      expect(endpoints.every((e) => e.service === "trustiot-smoke-echo" && e.port === "http")).toBe(true);
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
