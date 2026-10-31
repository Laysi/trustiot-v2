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
  type ContainerState,
  DASHBOARD_CHECK,
  describe,
  DNS_QUERY_ID,
  DNS_REFUSAL_RCODES,
  dnsHeaderOf,
  dnsQuery,
  dnsRefusalProblem,
  httpRequest,
  isTlsAlert,
  isolationPlan,
  type IsolationTargets,
  OUTSIDE,
  OUTSIDE_NAME,
  PAYLOAD_LIMIT_BYTES,
  POD_PIDS_LIMIT,
  parseHttpAnswer,
  parseProbeOutput,
  type PlannedProbe,
  podStateCommand,
  type Probe,
  PROBE_CONTAINER_LIMIT_MS,
  PROBE_IMAGE,
  PROBE_LIMIT_S,
  PROBE_SCRIPT,
  PROBE_STATE_LIMIT_MS,
  probeArguments,
  probeCommand,
  probeInPod,
  probeProblem,
  type ProbeResult,
  readyPodOf,
  validateProbes,
  ZTUNNEL_PORTS,
} from "./dashboard-check.ts";
import type { ExecOptions, ExecResult } from "./exec.ts";
import { exec, pinnedTool, RunLog } from "./exec.ts";
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
  clusterDns: { clusterIp: "10.43.0.10", udpPort: 53, tcpPort: 53 },
  checkBody: '{"page":"<p>x</p>"}',
};
/** A TLS fatal alert record (decode_error), as a TLS server refuses bytes that are not TLS. */
const TLS_ALERT = "\u0015\u0003\u0003\u0000\u0002\u0002\u0032";
/**
 * A DNS response to one question as a UDP datagram (Latin-1): its header, the question, and an
 * address record per answer. `flags` 0x8180 is a response, recursion desired and available, rcode 0;
 * 0x8182 is SERVFAIL. `authorities` is only counted in the header, which is all the checks read.
 */
function dnsResponse({ id = DNS_QUERY_ID, flags = 0x8180, answers = 1, authorities = 0 } = {}): string {
  const header = [id >> 8, id & 0xff, flags >> 8, flags & 0xff, 0, 1, answers >> 8, answers & 0xff, authorities >> 8, authorities & 0xff, 0, 0];
  const question = [7, ...Buffer.from("example"), 3, ...Buffer.from("com"), 0, 0, 1, 0, 1];
  const record = [0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 93, 184, 215, 14];
  return String.fromCharCode(...header, ...question, ...Array.from({ length: answers }, () => record).flat());
}

/** What ztunnel's DNS proxy answers when the query it forwards gets nowhere: SERVFAIL, no record. */
const SERVFAIL = dnsResponse({ flags: 0x8182, answers: 0 });

/** A DNS message as it travels over TCP, after its two-byte length. */
const tcpFramed = (message: string) => String.fromCharCode(message.length >> 8, message.length & 0xff) + message;

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
    const bytes = (payload: Uint8Array): Probe => ({ kind: "bytes", id: "b", transport: "udp", host: "127.0.0.1", port: 15_053, payload });
    expect(() => validateProbes([bytes(new Uint8Array())])).toThrow("sends 0 bytes");
    expect(() => validateProbes([bytes(new Uint8Array(PAYLOAD_LIMIT_BYTES + 1))])).toThrow(`not 1 to ${PAYLOAD_LIMIT_BYTES}`);
    expect(() => validateProbes([bytes(Uint8Array.of(0, 1, 0))])).not.toThrow();
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
    // Bytes travel base64-encoded, NUL included, and name their transport.
    expect(probeArguments([
      { kind: "bytes", id: "u", transport: "udp", host: "10.42.0.31", port: 15_053, payload: Uint8Array.of(0, 0xff, 0x41) },
      { kind: "bytes", id: "t", transport: "tcp", host: "127.0.0.1", port: 15_053, payload: Uint8Array.of(0x41) },
    ], 7)).toEqual(["7", "u", "udp-bytes", "10.42.0.31", "15053", "AP9B", "t", "tcp-bytes", "127.0.0.1", "15053", "QQ=="]);
    // The script never holds a probe's data, and it starts only on the line kubectl sends.
    expect(PROBE_SCRIPT.startsWith("set -u\nIFS= read -r -t ")).toBe(true);
    expect(PROBE_SCRIPT).toContain('[ "$start" = go ] || exit 5');
    expect(PROBE_SCRIPT).not.toContain(CLUSTER_NAME);
  });

  test("a DNS query asks for the A record of the name, with the length in front over TCP", () => {
    const udp = dnsQuery(OUTSIDE_NAME, "udp");
    expect([...udp]).toEqual([
      DNS_QUERY_ID >> 8, DNS_QUERY_ID & 0xff, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0,
      7, ...Buffer.from("example"), 3, ...Buffer.from("com"), 0,
      0, 1, 0, 1,
    ]);
    expect([...dnsQuery(OUTSIDE_NAME, "tcp")]).toEqual([0, udp.length, ...udp]);
    expect(dnsQuery(CLUSTER_NAME, "udp").length).toBe(12 + CLUSTER_NAME.length + 1 + 4);
    expect(() => dnsQuery("example.com", "udp")).toThrow("final dot");
    expect(() => dnsQuery("1.1.1.1.", "udp")).toThrow("final dot");
    expect(() => dnsQuery(`${"a".repeat(64)}.com.`, "udp")).toThrow("longer than 63");
  });

  test("a DNS answer is read from the header of one datagram", () => {
    expect(dnsHeaderOf(dnsResponse())).toEqual({ id: DNS_QUERY_ID, response: true, rcode: 0, questions: 1, answers: 1, authorities: 0, additionals: 0 });
    expect(dnsHeaderOf(dnsResponse({ flags: 0x8183, answers: 0 }))).toEqual({ id: DNS_QUERY_ID, response: true, rcode: 3, questions: 1, answers: 0, authorities: 0, additionals: 0 });
    expect(dnsHeaderOf(dnsResponse({ id: 0x1234, answers: 2, authorities: 1 }))).toMatchObject({ id: 0x1234, answers: 2, authorities: 1 });
    expect(dnsHeaderOf(`${dnsResponse().slice(0, 10)}\u0001\u0002`)).toMatchObject({ additionals: 0x0102 });
    // The query itself is not a response.
    expect(dnsHeaderOf(Buffer.from(dnsQuery(OUTSIDE_NAME, "udp")).toString("latin1"))).toMatchObject({ response: false, answers: 0 });
    // Shorter than a header: nothing to read.
    expect(dnsHeaderOf(dnsResponse().slice(0, 11))).toBeUndefined();
    expect(dnsHeaderOf("")).toBeUndefined();
  });

  test("a DNS refusal is the response to the query with SERVFAIL or REFUSED and not a single record", () => {
    expect(DNS_REFUSAL_RCODES).toEqual([2, 5]);
    expect(SERVFAIL.length).toBe(29);
    expect(dnsRefusalProblem(SERVFAIL, "udp")).toBeUndefined();
    expect(dnsRefusalProblem(dnsResponse({ flags: 0x8185, answers: 0 }), "udp")).toBeUndefined();
    // Over TCP the message follows its length.
    expect(dnsRefusalProblem(tcpFramed(SERVFAIL), "tcp")).toBeUndefined();
    expect(dnsRefusalProblem(SERVFAIL, "tcp")).toBe("answered 29 byte(s), not the header of a response to the query");
    // An answer that got anywhere: a record, a name that does not exist, an empty NOERROR.
    expect(dnsRefusalProblem(dnsResponse(), "udp")).toBe("answered a DNS response with rcode 0 and 1 record(s)");
    expect(dnsRefusalProblem(tcpFramed(dnsResponse()), "tcp")).toBe("answered a DNS response with rcode 0 and 1 record(s)");
    expect(dnsRefusalProblem(dnsResponse({ flags: 0x8183, answers: 0 }), "udp")).toBe("answered a DNS response with rcode 3 and 0 record(s)");
    expect(dnsRefusalProblem(dnsResponse({ answers: 0 }), "udp")).toBe("answered a DNS response with rcode 0 and 0 record(s)");
    expect(dnsRefusalProblem(dnsResponse({ flags: 0x8182, answers: 0, authorities: 1 }), "udp")).toBe("answered a DNS response with rcode 2 and 1 record(s)");
    // Not the response to this query, or too short to tell.
    expect(dnsRefusalProblem(dnsResponse({ id: 0x1234, flags: 0x8182, answers: 0 }), "udp")).toContain("not the header of a response to the query");
    expect(dnsRefusalProblem(Buffer.from(dnsQuery(OUTSIDE_NAME, "udp")).toString("latin1"), "udp")).toContain("not the header of a response to the query");
    expect(dnsRefusalProblem(tcpFramed(SERVFAIL).slice(0, 13), "tcp")).toBe("answered 13 byte(s), not the header of a response to the query");
    expect(dnsRefusalProblem(TLS_ALERT, "tcp")).toBe("answered 7 byte(s), not the header of a response to the query");
  });

  test("only complete TLS alert records are a TLS refusal", () => {
    expect(isTlsAlert(TLS_ALERT)).toBe(true);
    expect(isTlsAlert(TLS_ALERT + TLS_ALERT)).toBe(true);
    expect(isTlsAlert("")).toBe(false);
    expect(isTlsAlert(TLS_ALERT.slice(0, 6))).toBe(false);
    expect(isTlsAlert(TLS_ALERT + "H")).toBe(false);
    // A handshake record, or an HTTP answer, is not a refusal.
    expect(isTlsAlert("\u0016\u0003\u0003\u0000\u0002\u0002\u0032")).toBe(false);
    expect(isTlsAlert("HTTP/1.1 200 OK\r\n\r\n")).toBe(false);
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
    expect(checker.map((entry) => entry.probe.id)).toEqual([
      "loopback", "dns", "api-service", "api-endpoint-0", "collector", "outside",
      "ztunnel-dns-udp-loopback", "ztunnel-dns-tcp-loopback", "ztunnel-dns-udp-pod", "ztunnel-dns-tcp-pod",
      "ztunnel-outbound-loopback", "ztunnel-inbound-loopback", "ztunnel-hbone-loopback",
      "ztunnel-outbound-pod", "ztunnel-inbound-pod", "ztunnel-hbone-pod",
    ]);
    expect(checker.filter((entry) => entry.probe.id !== "loopback").every((entry) => !entry.expect.open)).toBe(true);
    const target = (id: string) => checker.find((entry) => entry.probe.id === id)!.probe;
    expect(target("dns")).toEqual({ kind: "dns", id: "dns", name: CLUSTER_NAME });
    expect(target("api-service")).toMatchObject({ host: "10.43.0.1", port: 443 });
    expect(target("api-endpoint-0")).toMatchObject({ host: "172.18.0.2", port: 6443 });
    expect(target("collector")).toMatchObject({ host: "10.43.20.9", port: 4318 });
    expect(target("outside")).toMatchObject({ host: OUTSIDE.host, port: OUTSIDE.port });
    // ztunnel's DNS proxy port, on loopback and on the pod's address, over UDP and TCP, asked for
    // a name outside the cluster.
    expect(target("ztunnel-dns-udp-loopback")).toEqual({ kind: "bytes", id: "ztunnel-dns-udp-loopback", transport: "udp", host: "127.0.0.1", port: 15_053, payload: dnsQuery(OUTSIDE_NAME, "udp") });
    expect(target("ztunnel-dns-tcp-loopback")).toEqual({ kind: "bytes", id: "ztunnel-dns-tcp-loopback", transport: "tcp", host: "127.0.0.1", port: 15_053, payload: dnsQuery(OUTSIDE_NAME, "tcp") });
    expect(target("ztunnel-dns-udp-pod")).toMatchObject({ transport: "udp", host: "10.42.0.31", port: 15_053 });
    expect(target("ztunnel-dns-tcp-pod")).toMatchObject({ transport: "tcp", host: "10.42.0.31", port: 15_053 });
    // Its outbound, inbound and HBONE ports on loopback and on the pod's address, each asked for
    // the outside address.
    for (const [where, host] of [["loopback", "127.0.0.1"], ["pod", "10.42.0.31"]] as const) {
      expect(target(`ztunnel-outbound-${where}`)).toEqual({ kind: "tcp", id: `ztunnel-outbound-${where}`, host, port: 15_001, send: httpRequest("GET", OUTSIDE.host, "/") });
      expect(target(`ztunnel-inbound-${where}`)).toEqual({ kind: "tcp", id: `ztunnel-inbound-${where}`, host, port: 15_006, send: httpRequest("GET", OUTSIDE.host, "/") });
      expect(target(`ztunnel-hbone-${where}`)).toEqual({ kind: "tcp", id: `ztunnel-hbone-${where}`, host, port: 15_008, send: "CONNECT 1.1.1.1:80 HTTP/1.1\r\nHost: 1.1.1.1:80\r\n\r\n" });
    }
    expect(ZTUNNEL_PORTS).toEqual({ outbound: 15_001, inbound: 15_006, hbone: 15_008, dns: 15_053 });
    // Only the HBONE port may refuse with a TLS alert, and only the DNS proxy port with a DNS error
    // without a record.
    expect(checker.filter((entry) => !entry.expect.open && "tlsRefusal" in entry.expect).map((entry) => entry.probe.id)).toEqual(["ztunnel-hbone-loopback", "ztunnel-hbone-pod"]);
    expect(checker.filter((entry) => !entry.expect.open && "dnsRefusal" in entry.expect).map((entry) => entry.probe.id)).toEqual([
      "ztunnel-dns-udp-loopback", "ztunnel-dns-tcp-loopback", "ztunnel-dns-udp-pod", "ztunnel-dns-tcp-pod",
    ]);
    // By address: a name would fail at the lookup, which the DNS probe covers, and prove nothing more.
    for (const entry of checker) if (entry.probe.kind !== "dns") expect(entry.probe.host).toMatch(/^\d+\.\d+\.\d+\.\d+$/);
    // The probe works in that pod: its own health port on loopback answers 200.
    expect(checker[0]).toMatchObject({ probe: { host: "127.0.0.1", port: 8081 }, expect: { open: true, status: 200 } });
  });

  test("runs the same probes from the identity pod as controls that must answer, and expects silence from the dashboard check there", () => {
    const control = of("control");
    // ztunnel's ports are the pod's own; the bytes probes have their own control below.
    const egress = of("checker").filter((entry) => entry.probe.id !== "loopback" && !entry.probe.id.startsWith("ztunnel-"));
    for (const entry of egress) {
      expect(control.find((candidate) => candidate.probe.id === entry.probe.id)).toMatchObject({ probe: entry.probe, expect: { open: true } });
    }
    expect(control.find((entry) => entry.probe.id === "dns")?.expect).toEqual({ open: true, resolved: true });
    // A DNS query sent as bytes to the cluster DNS answers there, over UDP and TCP: the probes that
    // send bytes work, so their silence in the dashboard check's pod means something. The name
    // ztunnel's DNS proxy is asked for resolves through the cluster DNS, and through the proxy on
    // this pod's own loopback, so the proxy's error or silence is a refusal, not a cluster DNS
    // without an upstream or a proxy that does not forward: by the pod's resolver, and by queries
    // sent as bytes whose answer holds a record (over UDP, where one read is the whole message).
    expect(control.filter((entry) => entry.probe.kind === "bytes").map(({ probe, expect }) => ({ probe, expect }))).toEqual([
      { probe: { kind: "bytes", id: "dns-udp", transport: "udp", host: "10.43.0.10", port: 53, payload: dnsQuery(CLUSTER_NAME, "udp") }, expect: { open: true, dnsAnswer: true } },
      { probe: { kind: "bytes", id: "dns-tcp", transport: "tcp", host: "10.43.0.10", port: 53, payload: dnsQuery(CLUSTER_NAME, "tcp") }, expect: { open: true } },
      { probe: { kind: "bytes", id: "outside-name-udp", transport: "udp", host: "10.43.0.10", port: 53, payload: dnsQuery(OUTSIDE_NAME, "udp") }, expect: { open: true, dnsAnswer: true } },
      { probe: { kind: "bytes", id: "ztunnel-dns-udp-loopback", transport: "udp", host: "127.0.0.1", port: 15_053, payload: dnsQuery(OUTSIDE_NAME, "udp") }, expect: { open: true, dnsAnswer: true } },
    ]);
    // The same probe as the dashboard check's, with the same label.
    const proxy = (from: PlannedProbe["from"]) => plan.find((entry) => entry.from === from && entry.probe.id === "ztunnel-dns-udp-loopback")!;
    expect(proxy("control").probe).toEqual(proxy("checker").probe);
    expect(proxy("control").label).toBe(proxy("checker").label);
    expect(control.find((entry) => entry.probe.id === "outside-name")).toMatchObject({ probe: { kind: "dns", name: OUTSIDE_NAME }, expect: { open: true, resolved: true } });
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
    // ztunnel's ports: silence passes; an answer from outside fails; the HBONE port alone may refuse
    // with nothing but a TLS alert, and the DNS proxy port alone with a DNS error without a record.
    ok(find("checker", "ztunnel-dns-udp-pod"), { kind: "silent", exit: 0 });
    ok(find("checker", "ztunnel-dns-udp-loopback"), answered(SERVFAIL));
    ok(find("checker", "ztunnel-dns-tcp-loopback"), answered(tcpFramed(SERVFAIL)));
    ok(find("checker", "ztunnel-dns-tcp-pod"), answered(tcpFramed(dnsResponse({ flags: 0x8185, answers: 0 }))));
    bad(find("checker", "ztunnel-dns-udp-loopback"), answered(dnsResponse()), "DNS example.com. over UDP to ztunnel's DNS proxy port 127.0.0.1:15053: answered a DNS response with rcode 0 and 1 record(s), expected no answer or a DNS error (SERVFAIL or REFUSED) without a record");
    bad(find("checker", "ztunnel-dns-tcp-loopback"), answered(tcpFramed(dnsResponse({ flags: 0x8183, answers: 0 }))), "over TCP to ztunnel's DNS proxy port 127.0.0.1:15053: answered a DNS response with rcode 3 and 0 record(s)");
    bad(find("checker", "ztunnel-dns-udp-loopback"), answered("tq\u0081\u0083"), "DNS example.com. over UDP to ztunnel's DNS proxy port 127.0.0.1:15053: answered 4 byte(s), not the header of a response to the query, expected no answer");
    bad(find("checker", "ztunnel-dns-tcp-pod"), answered(TLS_ALERT), "expected no answer");
    // A refusal counts only on the DNS proxy port.
    bad(find("checker", "ztunnel-outbound-loopback"), answered(SERVFAIL), "answered 29 byte(s), expected no answer");
    bad(find("checker", "ztunnel-outbound-loopback"), answered(http(301)), "1.1.1.1:80 through ztunnel's outbound port 127.0.0.1:15001: answered HTTP 301, expected no answer");
    bad(find("checker", "ztunnel-outbound-pod"), answered(http(301)), "1.1.1.1:80 through ztunnel's outbound port 10.42.0.31:15001: answered HTTP 301, expected no answer");
    ok(find("checker", "ztunnel-inbound-loopback"), { kind: "silent", exit: 124 });
    bad(find("checker", "ztunnel-inbound-pod"), answered("x"), "ztunnel's inbound port 10.42.0.31:15006: answered 1 byte(s), expected no answer");
    for (const id of ["ztunnel-hbone-loopback", "ztunnel-hbone-pod"]) {
      ok(find("checker", id), answered(TLS_ALERT));
      ok(find("checker", id), { kind: "silent", exit: 0 });
      bad(find("checker", id), answered("HTTP/1.1 200 Connection established\r\n\r\n"), "expected no answer");
      bad(find("checker", id), answered(`${TLS_ALERT}HTTP/1.1 200 OK\r\n\r\n`), "expected no answer");
    }
    bad(find("checker", "loopback"), answered(http(503)), "expected HTTP 200");
    bad(find("checker", "loopback"), { kind: "silent", exit: 3 }, "no answer (not connected), expected an answer");
    // The controls must answer, with any status, and the name must resolve.
    ok(find("control", "api-service"), answered("HTTP/1.0 400 Bad Request\r\n\r\n"));
    ok(find("control", "outside"), answered(http(301)));
    bad(find("control", "outside"), { kind: "silent", exit: 124 }, "from the identity service's pod, 1.1.1.1:80 outside the cluster: no answer (out of time), expected an answer");
    bad(find("control", "dns"), { kind: "unresolved" }, "expected the name to resolve");
    // Over UDP the answer must be the response to the query, without an error, with a record.
    for (const id of ["dns-udp", "outside-name-udp", "ztunnel-dns-udp-loopback"]) {
      ok(find("control", id), answered(dnsResponse()));
      ok(find("control", id), answered(dnsResponse({ answers: 3 })));
      bad(find("control", id), answered("\u0074\u0071\u0081\u0080"), "answered 4 byte(s), expected a DNS answer");
      bad(find("control", id), answered(dnsResponse({ flags: 0x8183, answers: 0 })), "rcode 3 and 0 answer record(s), expected the response to query 29809 with rcode 0 and at least one record");
      bad(find("control", id), answered(dnsResponse({ flags: 0x8182 })), "rcode 2");
      bad(find("control", id), answered(dnsResponse({ answers: 0 })), "0 answer record(s)");
      bad(find("control", id), answered(dnsResponse({ id: 0x1234 })), "id 4660");
      bad(find("control", id), answered(dnsResponse({ flags: 0x0100 })), "not a response");
      bad(find("control", id), { kind: "silent", exit: 124 }, "no answer (out of time), expected a DNS answer");
    }
    bad(find("control", "outside-name-udp"), answered(dnsResponse({ flags: 0x8182 })), "DNS example.com. over UDP to the cluster DNS 10.43.0.10:53: a DNS message");
    // Over TCP one read may hold part of the message: any byte is an answer.
    ok(find("control", "dns-tcp"), answered("\u0000\u0030"));
    bad(find("control", "dns-tcp"), { kind: "silent", exit: 124 }, "DNS kubernetes.default.svc.cluster.local. over TCP to the cluster DNS 10.43.0.10:53: no answer (out of time), expected an answer");
    ok(find("control", "outside-name"), { kind: "resolved", address: "93.184.215.14" });
    bad(find("control", "outside-name"), { kind: "unresolved" }, "from the identity service's pod, DNS example.com. through the pod's resolver: did not resolve, expected the name to resolve");
    // A plan that expects a DNS answer from anything but a UDP query is a mistake of the plan.
    expect(probeProblem({ ...find("control", "dns-tcp"), expect: { open: true, dnsAnswer: true } }, answered(dnsResponse()))).toContain("only a DNS query sent over UDP can expect a DNS answer");
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
    expect(describe({ kind: "silent", exit: 4 })).toBe("no answer (write failed)");
    expect(describe(answered(TLS_ALERT))).toBe("answered only a TLS alert (7 bytes)");
    // The answer to a DNS query sent as bytes: its rcode and how many records, not the records.
    const udp = isolationPlan(TARGETS).find((entry) => entry.probe.id === "ztunnel-dns-udp-loopback")!.probe;
    const tcp = isolationPlan(TARGETS).find((entry) => entry.probe.id === "ztunnel-dns-tcp-loopback")!.probe;
    expect(describe(answered(SERVFAIL), udp)).toBe("answered a DNS response with rcode 2 and 0 record(s)");
    expect(describe(answered(tcpFramed(dnsResponse({ answers: 2 }))), tcp)).toBe("answered a DNS response with rcode 0 and 2 record(s)");
    expect(describe(answered(SERVFAIL))).toBe("answered 29 byte(s)");
    expect(describe(answered(dnsResponse({ id: 0x1234 })), udp)).toBe("answered 45 byte(s)");
  });
});

group("checkIsolation", () => {
  const pods = { checker: "trustiot-dashboard-check-a", control: "trustiot-identity-b", assistant: "trustiot-assistant-c" };
  /** What a cluster that keeps the chart's promises answers. */
  function isolated(pod: string, probe: Probe): ProbeResult {
    if (pod === pods.checker) {
      if (probe.id.startsWith("ztunnel-hbone-")) return answered(TLS_ALERT);
      // ztunnel's DNS proxy listens on the pod's loopback only, and what it forwards gets nowhere.
      if (probe.kind === "bytes" && probe.host === "127.0.0.1") return answered(probe.transport === "tcp" ? tcpFramed(SERVFAIL) : SERVFAIL);
      return probe.id === "loopback" ? answered(http(200)) : probe.kind === "dns" ? { kind: "unresolved" } : { kind: "silent", exit: 0 };
    }
    if (pod === pods.control) {
      if (probe.kind === "dns") return { kind: "resolved", address: "10.43.0.1" };
      if (probe.kind === "bytes") return probe.transport === "udp" ? answered(dnsResponse()) : answered("\u0000\u0030");
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
      "trustiot-dashboard-check-a: loopback dns api-service api-endpoint-0 collector outside ztunnel-dns-udp-loopback ztunnel-dns-tcp-loopback ztunnel-dns-udp-pod ztunnel-dns-tcp-pod ztunnel-outbound-loopback ztunnel-inbound-loopback ztunnel-hbone-loopback ztunnel-outbound-pod ztunnel-inbound-pod ztunnel-hbone-pod",
      "trustiot-identity-b: dns api-service api-endpoint-0 collector outside dns-udp dns-tcp outside-name outside-name-udp ztunnel-dns-udp-loopback checker-service checker-pod",
      "trustiot-assistant-c: check",
    ]);
    expect(detail).toContain("from the dashboard check's pod trustiot-dashboard-check-a: its own /readyz on 127.0.0.1:8081: answered HTTP 200, DNS kubernetes.default.svc.cluster.local.: did not resolve");
    expect(detail).toContain("from the identity service's pod trustiot-identity-b: DNS kubernetes.default.svc.cluster.local.: resolved to 10.43.0.1");
    expect(detail).toContain("the dashboard check's pod 10.42.0.31:8080: no answer (out of time)");
    expect(detail).toContain("1.1.1.1:80 through ztunnel's HBONE port 127.0.0.1:15008: answered only a TLS alert (7 bytes)");
    expect(detail).toContain("1.1.1.1:80 through ztunnel's HBONE port 10.42.0.31:15008: answered only a TLS alert (7 bytes)");
    expect(detail).toContain("DNS example.com. through the pod's resolver: resolved to 10.43.0.1");
    expect(detail).toContain("DNS example.com. over UDP to ztunnel's DNS proxy port 127.0.0.1:15053: answered a DNS response with rcode 2 and 0 record(s), DNS example.com. over TCP to ztunnel's DNS proxy port 127.0.0.1:15053: answered a DNS response with rcode 2 and 0 record(s)");
    expect(detail).toContain("DNS example.com. over UDP to ztunnel's DNS proxy port 127.0.0.1:15053: answered a DNS response with rcode 0 and 1 record(s), the dashboard check's Service");
    expect(detail).toContain("with the verdict passed");
  });

  test.each([
    ["DNS works in the dashboard check's pod (DNS capture back on)", (pod: string, probe: Probe): ProbeResult | undefined =>
      pod === pods.checker && probe.kind === "dns" ? { kind: "resolved", address: "10.43.0.1" } : undefined, "resolved to 10.43.0.1, expected no answer"],
    ["the collector answers the dashboard check's pod", (pod: string, probe: Probe) =>
      pod === pods.checker && probe.id === "collector" ? answered(http(404)) : undefined, "the OpenTelemetry collector 10.43.20.9:4318: answered HTTP 404, expected no answer"],
    ["ztunnel's DNS proxy answers a name outside the cluster in the dashboard check's pod", (pod: string, probe: Probe) =>
      pod === pods.checker && probe.id === "ztunnel-dns-tcp-pod" ? answered(tcpFramed(dnsResponse())) : undefined, "ztunnel's DNS proxy port 10.42.0.31:15053: answered a DNS response with rcode 0 and 1 record(s), expected no answer or a DNS error (SERVFAIL or REFUSED) without a record"],
    ["ztunnel's DNS proxy does not resolve the name outside for the identity pod, so its error proves nothing", (pod: string, probe: Probe): ProbeResult | undefined =>
      pod === pods.control && probe.id === "ztunnel-dns-udp-loopback" ? answered(SERVFAIL) : undefined, "from the identity service's pod, DNS example.com. over UDP to ztunnel's DNS proxy port 127.0.0.1:15053: a DNS message with id 29809, a response, rcode 2 and 0 answer record(s)"],
    ["the cluster DNS does not answer the bytes control, so silence proves nothing", (pod: string, probe: Probe): ProbeResult | undefined =>
      pod === pods.control && probe.id === "dns-udp" ? { kind: "silent", exit: 124 } : undefined, "over UDP to the cluster DNS 10.43.0.10:53: no answer (out of time), expected a DNS answer"],
    ["ztunnel's outbound port on the pod's address leads outside", (pod: string, probe: Probe) =>
      pod === pods.checker && probe.id === "ztunnel-outbound-pod" ? answered(http(301)) : undefined, "ztunnel's outbound port 10.42.0.31:15001: answered HTTP 301, expected no answer"],
    ["the cluster DNS cannot resolve the name outside, so the proxy's silence proves nothing", (pod: string, probe: Probe): ProbeResult | undefined =>
      pod === pods.control && probe.id === "outside-name-udp" ? answered(dnsResponse({ flags: 0x8182, answers: 0 })) : undefined, "DNS example.com. over UDP to the cluster DNS 10.43.0.10:53: a DNS message with id 29809, a response, rcode 2 and 0 answer record(s)"],
    ["the name outside does not resolve from the identity pod", (pod: string, probe: Probe): ProbeResult | undefined =>
      pod === pods.control && probe.id === "outside-name" ? { kind: "unresolved" } : undefined, "DNS example.com. through the pod's resolver: did not resolve, expected the name to resolve"],
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
      "--stdin", "--quiet",
    ]);
    expect(cmd.slice(cmd.indexOf("--") + 1)).toEqual(["bash", "-c", PROBE_SCRIPT, "tq-probe", ...probeArguments(probes)]);
    expect(PROBE_IMAGE).toBe(POSTGRES_IMAGE);
    expect(() => probeCommand("/bin/kubectl", "ns", "pod; rm", "tq-probe-1", probes)).toThrow("not a container or pod name");
    expect(podStateCommand("/bin/kubectl", "ns", "pod-a")).toEqual(["/bin/kubectl", "get", "pod", "pod-a", "--namespace", "ns", "--output", "json"]);
  });

  test("hands kubectl the start line with its input kept open, parses what the container printed and keeps the requests out of the log", async () => {
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
    expect(calls).toHaveLength(1);
    expect(calls[0]?.options).toEqual({ env: { KUBECONFIG: "/tmp/kubeconfig" }, stdin: "go\n", keepStdinOpen: true, timeoutMs: PROBE_CONTAINER_LIMIT_MS });
    expect(lines.join("\n")).not.toContain("<p>");
    expect(lines.join("\n")).toContain("pod/trustiot-assistant-c --container=tq-probe-3");
  });

  /** An environment value in the pod's JSON, which must not reach the log. */
  const ENV_VALUE = "env-value-kept-out-of-the-log";
  /** A fake kubectl: `debug` returns `debug`; `get pod` returns `get`, or the pod with `state` for tq-probe-1. */
  const kubectl = (debug: ExecResult, state: ContainerState | undefined, get?: ExecResult) => {
    const calls: { cmd: string[]; options: ExecOptions }[] = [];
    const exec = async (cmd: string[], options: ExecOptions): Promise<ExecResult> => {
      calls.push({ cmd, options });
      if (cmd[1] === "debug") return debug;
      if (cmd[1] !== "get") throw new Error(`unexpected kubectl ${cmd.join(" ")}`);
      const statuses = [{ name: "tq-probe-0", state: { running: {} } }, ...(state === undefined ? [] : [{ name: "tq-probe-1", state }])];
      const pod = { spec: { containers: [{ env: [{ name: "VALUE", value: ENV_VALUE }] }] }, status: { ephemeralContainerStatuses: statuses } };
      return get ?? { code: 0, stdout: JSON.stringify(pod), stderr: "", timedOut: false };
    };
    return { calls, exec };
  };

  test.each([
    [
      "kubectl ends without the output of a container that has ended",
      { code: 0, stdout: "", stderr: "", timedOut: false },
      { terminated: { exitCode: 0, reason: "Completed" } },
      undefined,
      "probe container tq-probe-1 in pod pod-a: kubectl debug exited with 0; the probe container ended without the result of check; the container has ended with 0 (Completed)",
    ],
    [
      "the container cannot start",
      { code: 1, stdout: "", stderr: 'error: ephemeral container "tq-probe-1" ErrImageNeverPull', timedOut: false },
      { waiting: { reason: "ErrImageNeverPull", message: "image not present" } },
      undefined,
      'probe container tq-probe-1 in pod pod-a: kubectl debug exited with 1; the probe container ended without the result of check; the container is waiting (ErrImageNeverPull: image not present):\nerror: ephemeral container "tq-probe-1" ErrImageNeverPull',
    ],
    [
      "kubectl runs out of time while the container runs",
      { code: null, stdout: "tq-probe check silent 0\n", stderr: "", timedOut: true },
      { running: { startedAt: "2026-10-31T00:00:00Z" } },
      undefined,
      `kubectl debug timed out after ${PROBE_CONTAINER_LIMIT_MS / 1000} s; the probe container ended without its closing line; the container is running`,
    ],
    [
      "the kubelet reports nothing of the container",
      { code: 0, stdout: "", stderr: "", timedOut: false },
      undefined,
      undefined,
      "the container has no state from the kubelet",
    ],
    [
      "the pod cannot be read",
      { code: 0, stdout: "", stderr: "", timedOut: false },
      undefined,
      { code: 1, stdout: "", stderr: "error: not found", timedOut: false },
      "the probe container ended without the result of check; its state is unknown (kubectl get pod exited with 1)",
    ],
  ] as const)("fails with kubectl's reason and the container's state when %s", async (_label, debug, state, get, message) => {
    const fake = kubectl(debug, state, get);
    const lines: string[] = [];
    const log = { line: (text: string) => lines.push(text), block: (text: string) => lines.push(text) } as unknown as RunLog;
    const failure = probeInPod({ ...ctx, log }, "ns", "pod-a", "tq-probe-1", probes, { exec: fake.exec });
    await expect(failure).rejects.toThrow(message);
    await failure.catch((error: Error) => expect(error.message).not.toContain(ENV_VALUE));
    expect(fake.calls.map((call) => call.cmd)).toEqual([probeCommand("/bin/kubectl", "ns", "pod-a", "tq-probe-1", probes), podStateCommand("/bin/kubectl", "ns", "pod-a")]);
    expect(fake.calls[1]?.options).toEqual({ env: { KUBECONFIG: "/tmp/kubeconfig" }, timeoutMs: PROBE_STATE_LIMIT_MS });
    expect(lines.join("\n")).not.toContain(ENV_VALUE);
  });

  test("the bound covers kubectl's wait for the container and every probe's own limit", () => {
    expect(PROBE_CONTAINER_LIMIT_MS).toBeGreaterThan((180 + PROBE_LIMIT_S) * 1000);
  });

  test("passes only options the pinned kubectl knows", async () => {
    // kubectl refuses the whole command for an option it does not know, which without this test
    // only the nightly's cluster would show; read them from each command's `--help` and the global ones.
    const kubectl = await pinnedTool("kubectl", REPO_ROOT);
    const optionsOf = async (args: string[]) => {
      const result = await exec([kubectl, ...args], { timeoutMs: 30_000 });
      if (result.code !== 0) throw new Error(`kubectl ${args.join(" ")} exited with ${result.code}: ${result.stderr}`);
      return [...result.stdout.matchAll(/^\s+(?:-\w, )?(--[a-z][a-z0-9-]*)=/gm)].map((match) => match[1]!);
    };
    const global = await optionsOf(["options"]);
    expect(global).toContain("--namespace");
    for (const cmd of [probeCommand(kubectl, "ns", "pod-a", "tq-probe-1", probes), podStateCommand(kubectl, "ns", "pod-a")]) {
      const own = await optionsOf([cmd[1]!, "--help"]);
      expect(own.length).toBeGreaterThan(0);
      const known = new Set([...global, ...own]);
      const end = cmd.indexOf("--");
      const used = cmd.slice(2, end === -1 ? undefined : end).filter((arg) => arg.startsWith("--")).map((arg) => arg.split("=")[0]!);
      expect({ command: cmd[1], unknown: used.filter((option) => !known.has(option)) }).toEqual({ command: cmd[1], unknown: [] });
    }
  }, 60_000);
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

  test("the nightly cluster runs every pod under the process limit the chart asks of the nodes", () => {
    const readme = readFileSync(join(REPO_ROOT, "deploy", "chart", "README.md"), "utf8");
    expect(readme).toContain(`kubelet-arg: ["pod-max-pids=${POD_PIDS_LIMIT}"]`);
    expect(readme).toContain(`\`podPidsLimit\`，值是 ${POD_PIDS_LIMIT}`);
    const run = readFileSync(join(REPO_ROOT, "scripts", "nightly", "run.ts"), "utf8");
    expect(run).toContain('"--k3s-arg", `--kubelet-arg=pod-max-pids=${POD_PIDS_LIMIT}@server:0`,');
    expect(run).toContain('import { POD_PIDS_LIMIT } from "./dashboard-check.ts";');
  });

  test("the SaaS host installer writes the process limit the chart asks of the nodes into k3s's config", async () => {
    // Loaded by path at run time: deploy/host is its own project (it builds standalone
    // executables), and its key file is a text import that only its own type declarations cover.
    const desired = (await import(join(REPO_ROOT, "deploy", "host", "src", "desired.ts"))) as { K3S_CONFIG?: unknown };
    if (typeof desired.K3S_CONFIG !== "string") throw new Error("deploy/host/src/desired.ts exports no K3S_CONFIG string");
    const config = Bun.YAML.parse(desired.K3S_CONFIG) as Record<string, unknown>;
    expect(config["kubelet-arg"]).toEqual([`pod-max-pids=${POD_PIDS_LIMIT}`]);
    expect(readFileSync(join(REPO_ROOT, "deploy", "host", "README.md"), "utf8")).toContain(`\`pod-max-pids=${POD_PIDS_LIMIT}\``);
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

// The script itself, with the runner's bash, getent, timeout, head, dd and base64 (GNU coreutils), as
// in the probe image. Skipped where one of them is missing, as where GNU coreutils or getent are
// not installed; with CI set it must run.
const tools = ["bash", "getent", "timeout", "base64", "head", "dd"].every((tool) => Bun.which(tool) !== null);
group.skipIf(!tools && process.env.CI === undefined)("the probe script", () => {
  test("answers, silence and names, run at once and read back", async () => {
    const answering = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data(socket) { socket.end("HTTP/1.1 204 No Content\r\n\r\n"); } } });
    // Reads the request, then closes without a byte: the write succeeds and nothing comes back.
    const closing = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data(socket) { socket.end(); } } });
    const unused = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data() {} } });
    const refusedPort = unused.port;
    unused.stop(true);
    // Echo the bytes back, as a DNS server answers with the query's id first: over UDP one
    // datagram; over TCP without closing, as a DNS server keeps the connection open.
    const datagrams = await Bun.udpSocket({ hostname: "127.0.0.1", socket: { data(socket, data, port, address) { socket.send(data, port, address); } } });
    const stream = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data(socket, data) { socket.write(data); } } });
    const query = Uint8Array.of(0x74, 0x71, 0, 0x01, 0xff);
    try {
      const probes: Probe[] = [
        { kind: "bytes", id: "udp", transport: "udp", host: "127.0.0.1", port: datagrams.port, payload: query },
        { kind: "bytes", id: "tcp-open", transport: "tcp", host: "127.0.0.1", port: stream.port, payload: query },
        { kind: "bytes", id: "tcp-refused", transport: "tcp", host: "127.0.0.1", port: refusedPort, payload: query },
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
      // The bytes arrive whole, NUL included, and the first read comes back.
      expect(results.get("udp")).toEqual({ kind: "answered", bytes: Buffer.from(query).toString("latin1") });
      expect(results.get("tcp-open")).toEqual({ kind: "answered", bytes: Buffer.from(query).toString("latin1") });
      expect(results.get("tcp-refused")).toEqual({ kind: "silent", exit: 3 });
      // Without the start line it probes nothing.
      const idle = Bun.spawn(["bash", "-c", PROBE_SCRIPT, "tq-probe", ...probeArguments(probes, 10)], { stdin: new Blob(["stop\n"]), stdout: "pipe", stderr: "pipe" });
      expect(await new Response(idle.stdout).text()).toBe("");
      expect(await idle.exited).toBe(5);
    } finally {
      answering.stop(true);
      closing.stop(true);
      datagrams.close();
      stream.stop(true);
    }
  });

  test("keeps what the peer answered when it closes before the request is written", async () => {
    // Answers as soon as the connection opens and closes it on the request's first bytes, without
    // reading the rest: the request still being written then meets a closed socket (a reset, then
    // EPIPE and SIGPIPE), yet the answer has arrived. Before the probe read while it wrote, it
    // reported this as silent. A close on open instead could reach the probe before its connect
    // returned and fail the connection itself (exit 3).
    const answer = "HTTP/1.1 403 Forbidden\r\n\r\n";
    const early = Bun.listen({
      hostname: "127.0.0.1",
      port: 0,
      socket: {
        open(socket) {
          socket.write(answer);
        },
        data(socket) {
          socket.terminate();
        },
      },
    });
    // The same without an answer: nothing comes back, and the write may fail.
    const mute = Bun.listen({
      hostname: "127.0.0.1",
      port: 0,
      socket: {
        open() {},
        data(socket) {
          socket.terminate();
        },
      },
    });
    try {
      // Many lines, so the shell writes them one by one long after the peer has closed. The
      // `tcp-bytes` payload exceeds what probeArguments allows, which is why the arguments are
      // written here; the script itself has no limit.
      const lines = "x".repeat(63).concat("\n").repeat(1024);
      const payload = Buffer.from(lines).toString("base64");
      const args = [
        ...["10", "tcp", "tcp", "127.0.0.1", String(early.port), lines, "bytes", "tcp-bytes", "127.0.0.1", String(early.port), payload],
        ...["mute", "tcp", "127.0.0.1", String(mute.port), lines, "mute-bytes", "tcp-bytes", "127.0.0.1", String(mute.port), payload],
      ];
      const proc = Bun.spawn(["bash", "-c", PROBE_SCRIPT, "tq-probe", ...args], { stdin: new Blob(["go\n"]), stdout: "pipe", stderr: "pipe" });
      const output = await new Response(proc.stdout).text();
      expect(await proc.exited).toBe(0);
      const probes: Probe[] = [
        { kind: "tcp", id: "tcp", host: "127.0.0.1", port: early.port, send: "x" },
        { kind: "bytes", id: "bytes", transport: "tcp", host: "127.0.0.1", port: early.port, payload: Uint8Array.of(1) },
        { kind: "tcp", id: "mute", host: "127.0.0.1", port: mute.port, send: "x" },
        { kind: "bytes", id: "mute-bytes", transport: "tcp", host: "127.0.0.1", port: mute.port, payload: Uint8Array.of(1) },
      ];
      const results = parseProbeOutput(output, probes);
      expect(results.get("tcp")).toEqual({ kind: "answered", bytes: answer });
      expect(results.get("bytes")).toEqual({ kind: "answered", bytes: answer });
      // Nothing comes back from the mute peer. Whether a write fails too depends on when its close
      // arrives: the kernel may have taken the whole request by then (exit 0), or not (exit 4).
      for (const id of ["mute", "mute-bytes"]) {
        const result = results.get(id);
        expect(result?.kind).toBe("silent");
        expect([0, 4]).toContain(result?.kind === "silent" ? result.exit : -1);
      }
    } finally {
      early.stop(true);
      mute.stop(true);
    }
  });
});
