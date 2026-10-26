import { describe as group, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { apiValues, DASHBOARD_CHECK_SHAPES } from "./api.ts";
import {
  CHECK_ROUTE,
  checkBodyOf,
  checkIsolation,
  CLUSTER_NAME,
  containerPortOf,
  DASHBOARD_CHECK,
  describe,
  httpRequest,
  isolationPlan,
  type IsolationTargets,
  OUTSIDE,
  parseHttpAnswer,
  parseProbeOutput,
  type PlannedProbe,
  type Probe,
  PROBE_CONTAINER_LIMIT_MS,
  PROBE_IMAGE,
  PROBE_LIMIT_S,
  PROBE_SCRIPT,
  probeArguments,
  probeCommand,
  probeInPod,
  probeProblem,
  type ProbeResult,
  readyPodOf,
  validateProbes,
} from "./dashboard-check.ts";
import type { ExecOptions } from "./exec.ts";
import { RunLog } from "./exec.ts";
import type { K8sObject } from "./manifests.ts";
import { POSTGRES_IMAGE } from "./platform.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");
const b64 = (text: string) => Buffer.from(text, "latin1").toString("base64");
const answered = (text: string): ProbeResult => ({ kind: "answered", bytes: text });
const http = (status: number, body = "", head = "") => `HTTP/1.1 ${status} X\r\ncontent-length: ${body.length}\r\n${head}\r\n${body}`;

const TARGETS: IsolationTargets = {
  namespace: "trustiot-saas",
  checker: { service: "trustiot-dashboard-check", clusterIp: "10.43.20.5", port: 8080, podIp: "10.42.0.31", healthPort: 8081 },
  collector: { clusterIp: "10.43.20.9", port: 4318 },
  apiServer: { clusterIp: "10.43.0.1", port: 443, endpoints: [{ host: "172.18.0.2", port: 6443 }] },
  checkBody: '{"page":"<p>x</p>"}',
};

group("probes", () => {
  test("refuses what could not travel as an argument or be read back", () => {
    const tcp = (overrides: Partial<Extract<Probe, { kind: "tcp" }>>): Probe => ({ kind: "tcp", id: "a", host: "10.0.0.1", port: 80, send: "x", ...overrides });
    expect(() => validateProbes([])).toThrow("no probe");
    expect(() => validateProbes([tcp({ id: "A" })])).toThrow("not a probe id");
    expect(() => validateProbes([tcp({ id: "done" })])).toThrow("not a probe id");
    expect(() => validateProbes([tcp({}), tcp({})])).toThrow("two probes are named a");
    expect(() => validateProbes([tcp({ host: "10.0.0.1; reboot" })])).toThrow("is not a host");
    expect(() => validateProbes([tcp({ host: "-x" })])).toThrow("is not a host");
    expect(() => validateProbes([tcp({ port: 0 })])).toThrow("is not a port");
    expect(() => validateProbes([tcp({ port: 65_536 })])).toThrow("is not a port");
    expect(() => validateProbes([tcp({ send: "a\0b" })])).toThrow("NUL");
    expect(() => validateProbes([{ kind: "dns", id: "d", name: "kubernetes.default" }])).toThrow("final dot");
    expect(() => validateProbes([tcp({ host: "trustiot-dashboard-check.trustiot-saas.svc.cluster.local" }), { kind: "dns", id: "d", name: CLUSTER_NAME }])).not.toThrow();
  });

  test("pass everything as arguments after the fixed script, five per probe", () => {
    const send = httpRequest("POST", "h", "/p", { contentType: "application/json", text: '{"a":"$(id) `id` \\" 中"}' });
    expect(probeArguments([{ kind: "dns", id: "d", name: CLUSTER_NAME }, { kind: "tcp", id: "t", host: "10.0.0.1", port: 8080, send }], 7)).toEqual([
      "7",
      "d", "dns", CLUSTER_NAME, "0", "",
      "t", "tcp", "10.0.0.1", "8080", send,
    ]);
    expect(probeArguments([{ kind: "dns", id: "d", name: CLUSTER_NAME }])[0]).toBe(String(PROBE_LIMIT_S));
    // The script never holds a probe's data, and it starts only on the line kubectl sends.
    expect(PROBE_SCRIPT.startsWith("set -u\nIFS= read -r -t ")).toBe(true);
    expect(PROBE_SCRIPT).toContain('[ "$start" = go ] || exit 5');
    expect(PROBE_SCRIPT).not.toContain(CLUSTER_NAME);
  });

  test("an HTTP request asks the server to close and counts its body in bytes", () => {
    const request = httpRequest("POST", "trustiot-dashboard-check", CHECK_ROUTE, { contentType: "application/json", text: '{"t":"中文"}' });
    expect(request).toBe(
      `POST ${CHECK_ROUTE} HTTP/1.1\r\nHost: trustiot-dashboard-check\r\nConnection: close\r\nAccept: */*\r\nContent-Type: application/json\r\nContent-Length: 14\r\n\r\n{"t":"中文"}`,
    );
    expect(httpRequest("GET", "1.1.1.1", "/")).toBe("GET / HTTP/1.1\r\nHost: 1.1.1.1\r\nConnection: close\r\nAccept: */*\r\n\r\n");
  });

  test("an answer is read as bytes: status, body as UTF-8, and chunks joined", () => {
    const utf8 = Buffer.from('{"t":"中文"}', "utf8").toString("latin1");
    expect(parseHttpAnswer(http(200, utf8))).toEqual({ status: 200, body: '{"t":"中文"}' });
    expect(parseHttpAnswer("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n")).toEqual({ status: 200, body: "abcde" });
    expect(parseHttpAnswer("HTTP/1.0 400 Bad Request\r\n\r\nClient sent an HTTP request to an HTTPS server.\n")?.status).toBe(400);
    expect(parseHttpAnswer("HTTP/1.1 301")).toEqual({ status: 301, body: "" });
    expect(parseHttpAnswer("\u0015\u0003\u0001\u0000\u0002\u0002\n")).toBeUndefined();
  });
});

group("the probe container's output", () => {
  const probes: Probe[] = [
    { kind: "dns", id: "dns", name: CLUSTER_NAME },
    { kind: "tcp", id: "api", host: "10.43.0.1", port: 443, send: "x" },
  ];

  test("gives each probe's result, ignoring lines that are not the script's", () => {
    const output = ["Defaulting debug container name", `tq-probe api answered ${b64("HTTP/1.1 400 x\r\n\r\n")}`, "tq-probe dns resolved 10.43.0.1", "tq-probe done", ""].join("\n");
    expect(parseProbeOutput(output, probes)).toEqual(
      new Map<string, ProbeResult>([
        ["api", { kind: "answered", bytes: "HTTP/1.1 400 x\r\n\r\n" }],
        ["dns", { kind: "resolved", address: "10.43.0.1" }],
      ]),
    );
    expect(parseProbeOutput("tq-probe dns unresolved\r\ntq-probe api silent 124\r\ntq-probe done\r\n", probes)).toEqual(
      new Map<string, ProbeResult>([
        ["dns", { kind: "unresolved" }],
        ["api", { kind: "silent", exit: 124 }],
      ]),
    );
  });

  test.each([
    ["a probe missing", "tq-probe dns unresolved\ntq-probe done\n", "without the result of api"],
    ["the closing line missing (the container was cut short)", "tq-probe dns unresolved\ntq-probe api silent 0\n", "without its closing line"],
    ["a probe twice", "tq-probe dns unresolved\ntq-probe dns unresolved\ntq-probe api silent 0\ntq-probe done\n", "printed twice"],
    ["an unknown probe", "tq-probe other silent 0\n", "names no probe"],
    ["a result of the wrong kind", "tq-probe dns answered QQ==\n", "(dns) printed answered"],
    ["an address that is not IPv4", "tq-probe dns resolved ::1\n", "printed"],
    ["an answer that is not base64", "tq-probe api answered <html>\n", "printed"],
    ["a silent probe without its status", "tq-probe api silent\n", "printed"],
  ])("fails on %s", (_label, output, message) => {
    expect(() => parseProbeOutput(output, probes)).toThrow(message);
  });
});

group("the isolation plan", () => {
  const plan = isolationPlan(TARGETS);
  const of = (from: PlannedProbe["from"]) => plan.filter((entry) => entry.from === from);

  test("probes every way out from the dashboard check's pod, and expects no answer", () => {
    const checker = of("checker");
    expect(checker.map((entry) => entry.probe.id)).toEqual(["loopback", "dns", "api-service", "api-endpoint-0", "collector", "outside"]);
    expect(checker.filter((entry) => entry.probe.id !== "loopback").every((entry) => !entry.expect.open)).toBe(true);
    const target = (id: string) => checker.find((entry) => entry.probe.id === id)!.probe;
    expect(target("dns")).toEqual({ kind: "dns", id: "dns", name: CLUSTER_NAME });
    expect(target("api-service")).toMatchObject({ host: "10.43.0.1", port: 443 });
    expect(target("api-endpoint-0")).toMatchObject({ host: "172.18.0.2", port: 6443 });
    expect(target("collector")).toMatchObject({ host: "10.43.20.9", port: 4318 });
    expect(target("outside")).toMatchObject({ host: OUTSIDE.host, port: OUTSIDE.port });
    // By address: a name would fail at the lookup, which the DNS probe covers, and prove nothing more.
    for (const entry of checker) if (entry.probe.kind === "tcp") expect(entry.probe.host).toMatch(/^\d+\.\d+\.\d+\.\d+$/);
    // The probe works in that pod: its own health port on loopback answers 200.
    expect(checker[0]).toMatchObject({ probe: { host: "127.0.0.1", port: 8081 }, expect: { open: true, status: 200 } });
  });

  test("runs the same probes from the identity pod as controls that must answer, and expects silence from the dashboard check there", () => {
    const control = of("control");
    const egress = of("checker").filter((entry) => entry.probe.id !== "loopback");
    for (const entry of egress) {
      expect(control.find((candidate) => candidate.probe.id === entry.probe.id)).toMatchObject({ probe: entry.probe, expect: { open: true } });
    }
    expect(control.find((entry) => entry.probe.id === "dns")?.expect).toEqual({ open: true, resolved: true });
    expect(control.filter((entry) => !entry.expect.open).map((entry) => entry.probe)).toMatchObject([
      { id: "checker-service", host: "10.43.20.5", port: 8080 },
      { id: "checker-pod", host: "10.42.0.31", port: 8080 },
    ]);
  });

  test("sends the assistant's check to the Service by name, and expects 200 with the verdict passed", () => {
    expect(of("assistant")).toEqual([
      expect.objectContaining({
        probe: {
          kind: "tcp",
          id: "check",
          host: "trustiot-dashboard-check.trustiot-saas.svc.cluster.local",
          port: 8080,
          send: httpRequest("POST", "trustiot-dashboard-check.trustiot-saas.svc.cluster.local", CHECK_ROUTE, { contentType: "application/json", text: TARGETS.checkBody }),
        },
        expect: { open: true, status: 200, verdict: "passed" },
      }),
    ]);
  });

  test("leaves the collector out when the release has none", () => {
    const { collector: _collector, ...without } = TARGETS;
    expect(isolationPlan(without).some((entry) => entry.probe.id === "collector")).toBe(false);
  });

  test("judges each result against what it expects", () => {
    const find = (from: PlannedProbe["from"], id: string) => plan.find((entry) => entry.from === from && entry.probe.id === id)!;
    const ok = (entry: PlannedProbe, result: ProbeResult) => expect(probeProblem(entry, result)).toBeUndefined();
    const bad = (entry: PlannedProbe, result: ProbeResult | undefined, message: string) => expect(probeProblem(entry, result)).toContain(message);
    // The dashboard check's pod: silence and no name pass; any byte or address fails.
    ok(find("checker", "dns"), { kind: "unresolved" });
    bad(find("checker", "dns"), { kind: "resolved", address: "10.43.0.1" }, "from the dashboard check's pod, DNS kubernetes.default.svc.cluster.local.: resolved to 10.43.0.1, expected no answer");
    for (const exit of [0, 3, 124]) ok(find("checker", "outside"), { kind: "silent", exit });
    bad(find("checker", "collector"), answered(http(404)), "answered HTTP 404, expected no answer");
    bad(find("checker", "api-service"), answered("\u0015\u0003"), "answered 2 byte(s), expected no answer");
    ok(find("checker", "loopback"), answered(http(200)));
    bad(find("checker", "loopback"), answered(http(503)), "expected HTTP 200");
    bad(find("checker", "loopback"), { kind: "silent", exit: 3 }, "no answer (not connected), expected an answer");
    // The controls must answer, with any status, and the name must resolve.
    ok(find("control", "api-service"), answered("HTTP/1.0 400 Bad Request\r\n\r\n"));
    ok(find("control", "outside"), answered(http(301)));
    bad(find("control", "outside"), { kind: "silent", exit: 124 }, "from the identity service's pod, 1.1.1.1:80 outside the cluster: no answer (out of time), expected an answer");
    bad(find("control", "dns"), { kind: "unresolved" }, "expected the name to resolve");
    ok(find("control", "checker-service"), { kind: "silent", exit: 0 });
    bad(find("control", "checker-pod"), answered(http(400)), "expected no answer");
    // The assistant: 200 and the verdict passed, nothing less.
    const check = find("assistant", "check");
    ok(check, answered(http(200, '{"verdict":"passed","report":{"marks":[]}}')));
    bad(check, answered(http(200, '{"verdict":"failed","problems":[]}')), "expected the verdict passed");
    bad(check, answered(http(200, "not json")), "expected the verdict passed");
    bad(check, answered(http(403, "RBAC: access denied")), "expected HTTP 200");
    bad(check, { kind: "silent", exit: 0 }, "from the assistant's pod");
    bad(check, undefined, "no result");
  });

  test("describes a result without the answer itself", () => {
    expect(describe(answered(http(200, "secret-looking body")))).toBe("answered HTTP 200");
    expect(describe({ kind: "silent", exit: 7 })).toBe("no answer (exit 7)");
    expect(describe({ kind: "silent", exit: 0 })).toBe("no answer (closed without a byte)");
  });
});

group("checkIsolation", () => {
  const pods = { checker: "trustiot-dashboard-check-a", control: "trustiot-identity-b", assistant: "trustiot-assistant-c" };
  /** What a cluster that keeps the chart's promises answers. */
  function isolated(pod: string, probe: Probe): ProbeResult {
    if (pod === pods.checker) return probe.id === "loopback" ? answered(http(200)) : probe.kind === "dns" ? { kind: "unresolved" } : { kind: "silent", exit: 0 };
    if (pod === pods.control) {
      if (probe.kind === "dns") return { kind: "resolved", address: "10.43.0.1" };
      return probe.id.startsWith("checker") ? { kind: "silent", exit: 124 } : answered(http(404));
    }
    return answered(http(200, '{"verdict":"passed","report":{"marks":[{"reference":"k|a","value":1}]}}'));
  }

  test("passes when the pod reaches nothing, the controls answer and the assistant's check passes, and says so", async () => {
    const ran: string[] = [];
    const detail = await checkIsolation(
      {
        probe: async (pod, probes) => {
          ran.push(`${pod}: ${probes.map((probe) => probe.id).join(" ")}`);
          return new Map(probes.map((probe) => [probe.id, isolated(pod, probe)]));
        },
        log: () => {},
      },
      pods,
      TARGETS,
    );
    expect(ran).toEqual([
      "trustiot-dashboard-check-a: loopback dns api-service api-endpoint-0 collector outside",
      "trustiot-identity-b: dns api-service api-endpoint-0 collector outside checker-service checker-pod",
      "trustiot-assistant-c: check",
    ]);
    expect(detail).toContain("from the dashboard check's pod trustiot-dashboard-check-a: its own /readyz on 127.0.0.1:8081: answered HTTP 200, DNS kubernetes.default.svc.cluster.local.: did not resolve");
    expect(detail).toContain("from the identity service's pod trustiot-identity-b: DNS kubernetes.default.svc.cluster.local.: resolved to 10.43.0.1");
    expect(detail).toContain("the dashboard check's pod 10.42.0.31:8080: no answer (out of time)");
    expect(detail).toContain("with the verdict passed");
  });

  test.each([
    ["DNS works in the dashboard check's pod (DNS capture back on)", (pod: string, probe: Probe): ProbeResult | undefined =>
      pod === pods.checker && probe.kind === "dns" ? { kind: "resolved", address: "10.43.0.1" } : undefined, "resolved to 10.43.0.1, expected no answer"],
    ["the collector answers the dashboard check's pod", (pod: string, probe: Probe) =>
      pod === pods.checker && probe.id === "collector" ? answered(http(404)) : undefined, "the OpenTelemetry collector 10.43.20.9:4318: answered HTTP 404, expected no answer"],
    ["another service reaches the dashboard check", (pod: string, probe: Probe) =>
      pod === pods.control && probe.id === "checker-service" ? answered(http(400)) : undefined, "the dashboard check's Service 10.43.20.5:8080: answered HTTP 400, expected no answer"],
    ["the runner has no way out, so silence proves nothing", (pod: string, probe: Probe): ProbeResult | undefined =>
      pod === pods.control && probe.id === "outside" ? { kind: "silent", exit: 124 } : undefined, "1.1.1.1:80 outside the cluster: no answer (out of time), expected an answer"],
    ["the mesh refuses the assistant", (pod: string) => (pod === pods.assistant ? { kind: "silent", exit: 0 } : undefined) as ProbeResult | undefined, "from the assistant's pod"],
  ])("fails when %s, naming every probe that differs", async (_label, change, message) => {
    const deps = {
      probe: async (pod: string, probes: Probe[]) => new Map(probes.map((probe) => [probe.id, change(pod, probe) ?? isolated(pod, probe)])),
      log: () => {},
    };
    await expect(checkIsolation(deps, pods, TARGETS)).rejects.toThrow(message);
  });
});

group("running the probes in a pod", () => {
  const ctx = { kubeconfig: "/tmp/kubeconfig", kubectl: "/bin/kubectl", log: new RunLog() };
  const probes: Probe[] = [{ kind: "tcp", id: "check", host: "trustiot-dashboard-check.ns.svc.cluster.local", port: 8080, send: httpRequest("POST", "h", CHECK_ROUTE, { contentType: "application/json", text: '{"page":"<p>中</p>"}' }) }];

  test("an attached, restricted ephemeral container of the probe image that starts on the line kubectl sends", () => {
    const cmd = probeCommand("/bin/kubectl", "ns", "trustiot-assistant-c", "tq-probe-3", probes);
    expect(cmd.slice(0, cmd.indexOf("--"))).toEqual([
      "/bin/kubectl", "debug", "--namespace", "ns", "pod/trustiot-assistant-c",
      "--container=tq-probe-3", `--image=${POSTGRES_IMAGE}`, "--image-pull-policy=Never", "--profile=restricted",
      "--stdin", "--quiet", "--pod-running-timeout=180s",
    ]);
    expect(cmd.slice(cmd.indexOf("--") + 1)).toEqual(["bash", "-c", PROBE_SCRIPT, "tq-probe", ...probeArguments(probes)]);
    expect(PROBE_IMAGE).toBe(POSTGRES_IMAGE);
    expect(() => probeCommand("/bin/kubectl", "ns", "pod; rm", "tq-probe-1", probes)).toThrow("not a container or pod name");
  });

  test("hands kubectl the start line, parses what the container printed and keeps the requests out of the log", async () => {
    const calls: { cmd: string[]; options: ExecOptions }[] = [];
    const lines: string[] = [];
    const log = { line: (text: string) => lines.push(text), block: (text: string) => lines.push(text) } as unknown as RunLog;
    const results = await probeInPod({ ...ctx, log }, "ns", "trustiot-assistant-c", "tq-probe-3", probes, {
      exec: async (cmd, options) => {
        calls.push({ cmd, options });
        return { code: 0, stdout: `tq-probe check answered ${b64(http(200, '{"verdict":"passed"}'))}\ntq-probe done\n`, stderr: "", timedOut: false };
      },
    });
    expect(results.get("check")?.kind).toBe("answered");
    expect(calls[0]?.options).toEqual({ env: { KUBECONFIG: "/tmp/kubeconfig" }, stdin: "go\n", timeoutMs: PROBE_CONTAINER_LIMIT_MS });
    expect(lines.join("\n")).not.toContain("<p>");
    expect(lines.join("\n")).toContain("pod/trustiot-assistant-c --container=tq-probe-3");
  });

  test("fails with kubectl's reason when the container printed no result", async () => {
    const run = (result: { code: number | null; stdout: string; stderr: string; timedOut: boolean }) =>
      probeInPod({ ...ctx, log: { line: () => {}, block: () => {} } as unknown as RunLog }, "ns", "pod-a", "tq-probe-1", probes, { exec: async () => result });
    await expect(run({ code: 1, stdout: "", stderr: 'error: ephemeral container "tq-probe-1" ErrImageNeverPull', timedOut: false })).rejects.toThrow(
      'probe container tq-probe-1 in pod pod-a: kubectl debug exited with 1; the probe container ended without the result of check:\nerror: ephemeral container "tq-probe-1" ErrImageNeverPull',
    );
    await expect(run({ code: null, stdout: "tq-probe check silent 0\n", stderr: "", timedOut: true })).rejects.toThrow("timed out after");
  });

  test("the bound covers kubectl's wait for the container and every probe's own limit", () => {
    expect(PROBE_CONTAINER_LIMIT_MS).toBeGreaterThan((180 + PROBE_LIMIT_S) * 1000);
  });
});

group("what the check reads from the release", () => {
  const pod = (name: string, component: string, ready: boolean, extra: Record<string, unknown> = {}) => ({
    metadata: { name, labels: { "app.kubernetes.io/component": component }, ...extra },
    status: { phase: "Running", podIP: `10.42.0.${name.length}`, conditions: [{ type: "Ready", status: ready ? "True" : "False" }] },
  });

  test("the first ready pod of a component, never one that is not ready or is being deleted", () => {
    const list = {
      items: [
        pod("identity-b", "identity", true),
        pod("identity-a", "identity", false),
        pod("identity-0", "identity", true, { deletionTimestamp: "2026-10-26T00:00:00Z" }),
        pod("identity-c", "identity", true),
        pod("assistant-a", "assistant", true),
      ],
    };
    expect(readyPodOf(list, "identity").name).toBe("identity-b");
    expect(readyPodOf(list, "assistant")).toEqual({ name: "assistant-a", ip: "10.42.0.11" });
    expect(() => readyPodOf(list, DASHBOARD_CHECK)).toThrow("no pod of dashboard-check is running and ready");
  });

  test("the health port is the Deployment's container port", () => {
    const deployment: K8sObject = {
      kind: "Deployment",
      metadata: { labels: { "app.kubernetes.io/component": DASHBOARD_CHECK } },
      spec: { template: { spec: { containers: [{ ports: [{ name: "http", containerPort: 8080 }, { name: "health", containerPort: 8081 }] }] } } },
    };
    expect(containerPortOf([deployment], DASHBOARD_CHECK, "health")).toBe(8081);
    expect(() => containerPortOf([deployment], DASHBOARD_CHECK, "metrics")).toThrow("one container port named metrics, found 0");
    expect(() => containerPortOf([], DASHBOARD_CHECK, "health")).toThrow("one Deployment of dashboard-check, found 0");
  });

  test("the assistant's check is the sandbox's correct fixture page with the shared request", () => {
    const body = JSON.parse(checkBodyOf(REPO_ROOT)) as Record<string, unknown>;
    const fixtures = join(REPO_ROOT, "frontend", "sandbox", "fixtures");
    expect(body.page).toBe(readFileSync(join(fixtures, "pages", "correct.html"), "utf8"));
    const { page: _page, ...request } = body;
    expect(request).toEqual(JSON.parse(readFileSync(join(fixtures, "request.json"), "utf8")));
  });

  test("the route, ports and paths are the service's own", () => {
    expect(readFileSync(join(REPO_ROOT, "crates", "contract", "src", "api", "dashboard", "check.rs"), "utf8")).toContain(`pub const CHECK_ROUTE: &str = "${CHECK_ROUTE}";`);
    const values = Bun.YAML.parse(readFileSync(join(REPO_ROOT, "deploy", "chart", "values.yaml"), "utf8")) as { mesh: { calls: Record<string, string[]> } };
    // The one caller the plan's assistant probe stands for.
    expect(Object.entries(values.mesh.calls).filter(([, callees]) => callees.includes(DASHBOARD_CHECK)).map(([caller]) => caller)).toEqual(["assistant"]);
  });

  test("the smoke enables the dashboard check on the SaaS shape only, from its nightly image", () => {
    expect([...DASHBOARD_CHECK_SHAPES]).toEqual(["saas"]);
    const services = (shape: string) => (apiValues(shape, []) as { services: Record<string, { enabled?: boolean; image?: unknown }> }).services;
    expect(services("saas")[DASHBOARD_CHECK]).toEqual({ enabled: true, image: { repository: "localhost/trustiot-e2e/dashboard-check", tag: "nightly", pullPolicy: "IfNotPresent" } });
    expect(services("onprem")[DASHBOARD_CHECK]).toBeUndefined();
  });
});

// The script itself, with the runner's bash, getent, timeout, head and base64 (GNU coreutils), as in
// the probe image. Skipped where they are missing (a macOS workstation); with CI set it must run.
const tools = ["bash", "getent", "timeout", "base64", "head"].every((tool) => Bun.which(tool) !== null);
group.skipIf(!tools && process.env.CI === undefined)("the probe script", () => {
  test("answers, silence and names, run at once and read back", async () => {
    const answering = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data(socket) { socket.end("HTTP/1.1 204 No Content\r\n\r\n"); } } });
    // Reads the request, then closes without a byte: the write succeeds and nothing comes back.
    const closing = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data(socket) { socket.end(); } } });
    const unused = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data() {} } });
    const refusedPort = unused.port;
    unused.stop(true);
    try {
      const probes: Probe[] = [
        { kind: "tcp", id: "answering", host: "127.0.0.1", port: answering.port, send: httpRequest("POST", "h", "/", { contentType: "text/plain", text: 'a "$(id)" `id` 中' }) },
        { kind: "tcp", id: "closing", host: "127.0.0.1", port: closing.port, send: "x" },
        { kind: "tcp", id: "refused", host: "127.0.0.1", port: refusedPort, send: "x" },
        { kind: "dns", id: "invalid", name: "tq-probe.invalid." },
      ];
      const proc = Bun.spawn(["bash", "-c", PROBE_SCRIPT, "tq-probe", ...probeArguments(probes, 10)], { stdin: new Blob(["go\n"]), stdout: "pipe", stderr: "pipe" });
      const output = await new Response(proc.stdout).text();
      expect(await proc.exited).toBe(0);
      const results = parseProbeOutput(output, probes);
      expect(results.get("answering")).toEqual({ kind: "answered", bytes: "HTTP/1.1 204 No Content\r\n\r\n" });
      expect(results.get("closing")).toEqual({ kind: "silent", exit: 0 });
      expect(results.get("refused")).toEqual({ kind: "silent", exit: 3 });
      expect(results.get("invalid")).toEqual({ kind: "unresolved" });
      // Without the start line it probes nothing.
      const idle = Bun.spawn(["bash", "-c", PROBE_SCRIPT, "tq-probe", ...probeArguments(probes, 10)], { stdin: new Blob(["stop\n"]), stdout: "pipe", stderr: "pipe" });
      expect(await new Response(idle.stdout).text()).toBe("");
      expect(await idle.exited).toBe(5);
    } finally {
      answering.stop(true);
      closing.stop(true);
    }
  });
});
