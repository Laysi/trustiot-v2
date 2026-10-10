// Ingress e2e test (Laysi/trustiot-v2#62): installs the chart with test doubles into an existing
// namespace of any cluster with Istio ambient, then checks the cookie check, the public paths,
// the header rules, path-bypass attempts and an identity-service outage through the real ingress
// gateway. The /internal slash cases (Laysi/trustiot-v2#255) also need the mesh's path
// normalization from deploy/istio/istiod-values.yaml. README.md in this directory lists the
// prerequisites; run from the repository root:
//
//   E2E_NAMESPACE=e2e-62 E2E_IDENTITY_IMAGE=<image> bun test ./deploy/chart/tests/e2e/ingress.e2e.ts
//
// Not part of `bun test deploy/chart` (the file name has no .test. suffix): it needs a cluster.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Subprocess } from "bun";
import {
  CONTEXT,
  GATEWAY,
  IDENTITY_IMAGE,
  KEEP,
  KUBECTL,
  NAMESPACE,
  OBJECT_PREFIX,
  RELEASE,
  REPORT_DIR,
  freePort,
  imageValues,
  installRelease,
  kubectl,
  run,
  uninstallRelease,
  waitFor,
} from "./cluster";

const SHAPE = (process.env.E2E_SHAPE ?? "saas") as "saas" | "onprem";
if (SHAPE !== "saas" && SHAPE !== "onprem") throw new Error("E2E_SHAPE must be saas or onprem");
const PROVIDER = process.env.E2E_AUTHZ_PROVIDER ?? "trustiot-identity";
// ext_authz timeout of the mesh extension provider (deploy/istio/istiod-values.yaml).
const AUTHZ_TIMEOUT_MS = Number(process.env.E2E_AUTHZ_TIMEOUT_MS ?? "500");
// Reach the gateway's ClusterIP through `ssh -L` on this host (the cluster node), so requests come
// from the host like cloudflared's. Without it the test uses kubectl port-forward, which enters
// the gateway pod directly and so skips the ingress NetworkPolicy.
const SSH_JUMP = process.env.E2E_SSH_JUMP;

const HOSTS =
  SHAPE === "saas"
    ? { app: "app.saas.trustiot.test", gateway: "gw.saas.trustiot.test", ota: "ota.saas.trustiot.test" }
    : { app: "trustiot.factory.test", gateway: "gw.trustiot.factory.test", ota: "ota.trustiot.factory.test" };
type HostKey = keyof typeof HOSTS;
// A name of its own, so the test never replaces a certificate it did not create.
const TLS_SECRET = `trustiot-e2e-tls-${crypto.randomUUID().slice(0, 8)}`;
// Markers that whoami prints as "Name: <WHOAMI_NAME>" (tests/e2e/values-e2e.yaml).
const PROTECTED_MARKER = "Name: protected-backend";
const FORGED = `forged.${crypto.randomUUID()}`;
const VALID_COOKIE = `sid=valid-${crypto.randomUUID().replaceAll("-", "")}`;
const SLOW_COOKIE = `sid=slow-${crypto.randomUUID().replaceAll("-", "")}`;

// ---------------------------------------------------------------------------------------------
// Raw HTTP/1.1 client: fetch() would normalize the request target (dot segments, encodings),
// which is exactly what the bypass cases must send unchanged.

// ms: time to the first response byte. Envoy keeps the connection open for up to 1 s after a local
// reply (delayed close), so the time until close would overstate how fast the gateway answered.
type RawResponse = { status: number; headers: Record<string, string[]>; body: string; ms: number };

let ingress: { host: string; port: number; tls: boolean; ca?: string };

function decodeChunked(body: Buffer): Buffer {
  const parts: Buffer[] = [];
  let offset = 0;
  while (offset < body.length) {
    const lineEnd = body.indexOf("\r\n", offset);
    if (lineEnd < 0) break;
    const size = Number.parseInt(body.subarray(offset, lineEnd).toString(), 16);
    if (!Number.isFinite(size) || size === 0) break;
    parts.push(body.subarray(lineEnd + 2, lineEnd + 2 + size));
    offset = lineEnd + 2 + size + 2;
  }
  return Buffer.concat(parts);
}

type RawOptions = {
  host?: HostKey | string;
  method?: string;
  headers?: [string, string][];
  // TLS server name (on-prem shape); defaults to the Host header without its port.
  sni?: string;
};

// Sends one request. When the tunnel to the gateway (ssh or kubectl port-forward) has died,
// for example because the SSH session was dropped, reopens it and sends the request once more;
// any other failure fails the test.
async function raw(target: string, options: RawOptions = {}): Promise<RawResponse> {
  try {
    return await rawOnce(target, options);
  } catch (error) {
    if (tunnel === undefined || (tunnel.exitCode === null && tunnel.signalCode === null)) throw error;
    console.error(`tunnel to the ingress gateway exited (${tunnel.exitCode ?? tunnel.signalCode}); reopening it`);
    await openTunnel();
    return rawOnce(target, options);
  }
}

async function rawOnce(target: string, options: RawOptions): Promise<RawResponse> {
  const hostHeader = options.host === undefined ? HOSTS.app : (HOSTS[options.host as HostKey] ?? options.host);
  const lines = [
    `${options.method ?? "GET"} ${target} HTTP/1.1`,
    `host: ${hostHeader}`,
    "user-agent: trustiot-ingress-e2e",
    "connection: close",
    ...(options.headers ?? []).map(([name, value]) => `${name}: ${value}`),
  ];
  const request = `${lines.join("\r\n")}\r\n\r\n`;
  const chunks: Buffer[] = [];
  const started = performance.now();
  let firstByte: number | undefined;
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`timeout: ${target}`)), 15_000);
    const finish = () => {
      clearTimeout(timer);
      resolve();
    };
    Bun.connect({
      hostname: ingress.host,
      port: ingress.port,
      tls: ingress.tls
        ? { serverName: options.sni ?? hostHeader.split(":")[0], ca: ingress.ca }
        : undefined,
      socket: {
        open(socket) {
          socket.write(request);
        },
        data(_socket, data) {
          firstByte ??= performance.now();
          chunks.push(Buffer.from(data));
        },
        close: finish,
        end: finish,
        error(_socket, error) {
          clearTimeout(timer);
          reject(error);
        },
        connectError(_socket, error) {
          clearTimeout(timer);
          reject(error);
        },
      },
    }).catch(reject);
  });
  const ms = (firstByte ?? performance.now()) - started;
  const response = Buffer.concat(chunks);
  const split = response.indexOf("\r\n\r\n");
  if (split < 0) throw new Error(`no HTTP response for ${target}: ${JSON.stringify(response.toString())}`);
  const [statusLine = "", ...headerLines] = response.subarray(0, split).toString().split("\r\n");
  const status = Number(statusLine.split(" ")[1]);
  const headers: Record<string, string[]> = {};
  for (const line of headerLines) {
    const colon = line.indexOf(":");
    const name = line.slice(0, colon).trim().toLowerCase();
    (headers[name] ??= []).push(line.slice(colon + 1).trim());
  }
  let body = response.subarray(split + 4);
  if (headers["transfer-encoding"]?.some((v) => v.toLowerCase().includes("chunked"))) body = decodeChunked(body);
  return { status, headers, body: body.toString(), ms };
}

// whoami prints one "Name: value" line per request header value.
function whoamiHeaderCount(body: string, header: string): number {
  return body.split("\n").filter((line) => line.toLowerCase().startsWith(`${header.toLowerCase()}:`)).length;
}
function whoamiHeader(body: string, header: string): string | undefined {
  const line = body.split("\n").find((l) => l.toLowerCase().startsWith(`${header.toLowerCase()}:`));
  return line?.slice(line.indexOf(":") + 1).trim();
}

const results: Record<string, unknown>[] = [];
function record(group: string, name: string, target: string, host: string, response: RawResponse) {
  results.push({
    group,
    name,
    host,
    target,
    status: response.status,
    ms: Math.round(response.ms * 10) / 10,
    reachedProtectedBackend: response.body.includes(PROTECTED_MARKER),
    forgedHeaderSeen: response.body.includes(FORGED),
  });
}

// ---------------------------------------------------------------------------------------------
// Install and tear down.

const work = mkdtempSync(join(tmpdir(), "trustiot-ingress-e2e-"));
let tunnel: Subprocess | undefined;
// Cleanup removes only what this run created.
let releaseCreated = false;
let tlsSecretCreated = false;

async function openTunnel(): Promise<void> {
  const port = await freePort();
  const remotePort = SHAPE === "saas" ? 80 : 443;
  if (SSH_JUMP) {
    const clusterIp = kubectl("get", "service", `${GATEWAY}-istio`, "-o", "jsonpath={.spec.clusterIP}").trim();
    tunnel = Bun.spawn(
      ["ssh", "-N", "-o", "ExitOnForwardFailure=yes", "-L", `127.0.0.1:${port}:${clusterIp}:${remotePort}`, SSH_JUMP],
      { stdout: "ignore", stderr: "inherit" },
    );
  } else {
    tunnel = Bun.spawn(
      [
        KUBECTL,
        ...(CONTEXT ? ["--context", CONTEXT] : []),
        "--namespace",
        NAMESPACE,
        "port-forward",
        `service/${GATEWAY}-istio`,
        `${port}:${remotePort}`,
      ],
      { stdout: "ignore", stderr: "inherit" },
    );
  }
  ingress = { host: "127.0.0.1", port, tls: SHAPE === "onprem", ca: ingress?.ca };
  await waitFor("the tunnel to the ingress gateway", async () => {
    try {
      const socket = await Bun.connect({ hostname: "127.0.0.1", port, socket: { data() {} } });
      socket.end();
      return true;
    } catch {
      return false;
    }
  });
}

beforeAll(async () => {
  const shapeFiles =
    SHAPE === "saas"
      ? ["values-saas.yaml", "tests/values/saas-instance.yaml"]
      : ["values-onprem.yaml", "tests/values/onprem-instance.yaml"];
  const extra: string[] = ["--set", `ingress.authzProvider=${PROVIDER}`, ...imageValues("identity", IDENTITY_IMAGE)];
  if (SHAPE === "onprem") {
    // Throwaway self-signed certificate for the three hosts; the key never leaves the temp dir.
    const key = join(work, "tls.key");
    const crt = join(work, "tls.crt");
    run([
      "openssl",
      "req",
      "-x509",
      "-newkey",
      "ec",
      "-pkeyopt",
      "ec_paramgen_curve:P-256",
      "-nodes",
      "-days",
      "1",
      "-subj",
      "/CN=trustiot ingress e2e",
      "-addext",
      `subjectAltName=DNS:${HOSTS.app},DNS:${HOSTS.gateway},DNS:${HOSTS.ota}`,
      "-keyout",
      key,
      "-out",
      crt,
    ]);
    tlsSecretCreated = true;
    kubectl("create", "secret", "tls", TLS_SECRET, "--cert", crt, "--key", key);
    ingress = { host: "", port: 0, tls: true, ca: await Bun.file(crt).text() };
    rmSync(key);
    extra.push(
      "--set",
      `ingress.direct.tlsSecret=${TLS_SECRET}`,
      // Keep the gateway off the node's port 443: the test reaches it through its ClusterIP.
      "--set",
      "ingress.direct.serviceType=ClusterIP",
    );
  }
  // Only a release this run installed counts as its own, so a concurrent run's release is never
  // uninstalled (cluster.ts).
  installRelease([...shapeFiles, "tests/e2e/values-e2e.yaml"], extra);
  releaseCreated = true;
  kubectl("wait", `gateway/${GATEWAY}`, "--for=condition=Programmed", "--timeout=180s");
  kubectl("rollout", "status", `deployment/${GATEWAY}-istio`, "--timeout=180s");
  await openTunnel();
  // The gateway is ready once routes and the cookie check are in its configuration.
  await waitFor("the ingress gateway configuration", async () => {
    const response = await raw("/api/whoami", { headers: [["cookie", VALID_COOKIE]] }).catch(() => undefined);
    return response?.status === 200 && response.body.includes(PROTECTED_MARKER);
  });
}, 600_000);

afterAll(() => {
  tunnel?.kill();
  const errors: string[] = [];
  // A failed report write must not skip the cleanup below.
  if (REPORT_DIR) {
    try {
      mkdirSync(REPORT_DIR, { recursive: true });
      writeFileSync(join(REPORT_DIR, `ingress-e2e-${SHAPE}.json`), `${JSON.stringify(results, null, 2)}\n`);
    } catch (error) {
      errors.push(`report: ${String(error)}`);
    }
  }
  if (!KEEP) {
    for (const [created, cleanup] of [
      [releaseCreated, uninstallRelease],
      [tlsSecretCreated, () => kubectl("delete", "secret", TLS_SECRET, "--ignore-not-found")],
    ] as [boolean, () => void][]) {
      if (!created) continue;
      try {
        cleanup();
      } catch (error) {
        errors.push(String(error));
      }
    }
  }
  rmSync(work, { recursive: true, force: true });
  if (errors.length) throw new Error(`cleanup failed:\n${errors.join("\n")}`);
}, 300_000);

// ---------------------------------------------------------------------------------------------
// Cookie check.

// Spellings of /internal that only the mesh's path normalization (merge slashes, decode %2F)
// brings back to /internal before the DENY policy matches (Laysi/trustiot-v2#255).
const INTERNAL_SLASH_CASES = [
  "//internal",
  "//internal/tenants",
  "///internal",
  "//./internal",
  "/.//internal/tenants",
  "/x/..//internal/tenants",
  "/%2finternal",
  "/%2finternal/tenants",
  "/%2Finternal",
  "/%2f%2finternal",
  "/./%2finternal",
];

// Spellings of x-internal-identity with underscores. Envoy forwards headers with underscores
// unchanged, and CGI/WSGI-style servers read "-" and "_" alike (HTTP_X_INTERNAL_IDENTITY), so
// the ingress removes all three on every route (Laysi/trustiot-v2#255, F2).
const UNDERSCORE_IDENTITY_HEADERS = ["x_internal_identity", "x-internal_identity", "x_internal-identity"];

describe("paths that need the cookie", () => {
  test("no cookie or an unknown session: 401 from the identity service, backend not reached", async () => {
    for (const [name, headers] of [
      ["no cookie", []],
      ["unknown session", [["cookie", "sid=nope"]]],
      ["other cookies only", [["cookie", "theme=dark; lang=zh-TW"]]],
    ] as [string, [string, string][]][]) {
      const response = await raw("/api/whoami", { headers });
      record("cookie", name, "/api/whoami", HOSTS.app, response);
      expect(response.status).toBe(401);
      expect(response.body).toContain("unauthenticated");
      expect(response.body).not.toContain(PROTECTED_MARKER);
    }
  });

  test("valid cookie: the backend gets exactly one identity header, from the identity service", async () => {
    const response = await raw("/api/whoami", { headers: [["cookie", `theme=dark; ${VALID_COOKIE}`]] });
    record("cookie", "valid cookie", "/api/whoami", HOSTS.app, response);
    expect(response.status).toBe(200);
    expect(response.body).toContain(PROTECTED_MARKER);
    expect(whoamiHeaderCount(response.body, "x-internal-identity")).toBe(1);
    expect(whoamiHeader(response.body, "x-internal-identity")).toStartWith("double.");
  });

  test("valid cookie plus a forged identity header: the forged value never reaches the backend", async () => {
    const response = await raw("/api/whoami", {
      headers: [
        ["cookie", VALID_COOKIE],
        ["x-internal-identity", FORGED],
      ],
    });
    record("cookie", "valid cookie + forged header", "/api/whoami", HOSTS.app, response);
    expect(response.status).toBe(200);
    expect(whoamiHeaderCount(response.body, "x-internal-identity")).toBe(1);
    expect(response.body).not.toContain(FORGED);
  });

  test("valid cookie plus underscore spellings of the identity header: none reaches the backend", async () => {
    const response = await raw("/api/whoami", {
      headers: [["cookie", VALID_COOKIE], ...UNDERSCORE_IDENTITY_HEADERS.map((name): [string, string] => [name, FORGED])],
    });
    record("cookie", "valid cookie + underscore identity headers", "/api/whoami", HOSTS.app, response);
    expect(response.status).toBe(200);
    expect(response.body).toContain(PROTECTED_MARKER);
    expect(response.body).not.toContain(FORGED);
    for (const name of UNDERSCORE_IDENTITY_HEADERS) expect(whoamiHeaderCount(response.body, name)).toBe(0);
    expect(whoamiHeaderCount(response.body, "x-internal-identity")).toBe(1);
    expect(whoamiHeader(response.body, "x-internal-identity")).toStartWith("double.");
  });

  test("forged identity header without a cookie: 401", async () => {
    const response = await raw("/api/whoami", { headers: [["x-internal-identity", FORGED]] });
    record("cookie", "forged header, no cookie", "/api/whoami", HOSTS.app, response);
    expect(response.status).toBe(401);
    expect(response.body).not.toContain(FORGED);
  });

  test("x-forwarded-proto is https at the backend, even when the client sends http", async () => {
    for (const headers of [[], [["x-forwarded-proto", "http"]]] as [string, string][][]) {
      const response = await raw("/api/whoami", { headers: [["cookie", VALID_COOKIE], ...headers] });
      record("x-forwarded-proto", `cookie route, client sent ${headers.length ? "http" : "nothing"}`, "/api/whoami", HOSTS.app, response);
      expect(response.status).toBe(200);
      expect(whoamiHeaderCount(response.body, "x-forwarded-proto")).toBe(1);
      expect(whoamiHeader(response.body, "x-forwarded-proto")).toBe("https");
    }
  });

  test("/internal is never reachable: 401 without a cookie, 403 with one", async () => {
    for (const [cookie, expected] of [
      [undefined, 401],
      [VALID_COOKIE, 403],
    ] as [string | undefined, number][]) {
      for (const target of ["/internal/ext-authz/api/whoami", "/internal", "/internal/tenants"]) {
        const response = await raw(target, { headers: cookie ? [["cookie", cookie]] : [] });
        record("internal", cookie ? "valid cookie" : "no cookie", target, HOSTS.app, response);
        expect(response.status).toBe(expected);
        expect(response.body).not.toContain(PROTECTED_MARKER);
      }
    }
  });

  // Laysi/trustiot-v2#255 (F1 of docs/test-reports/2026-10-10-62-e2e-verify.md): without the mesh's
  // path normalization (deploy/istio/istiod-values.yaml, DECODE_AND_MERGE_SLASHES) these
  // spellings slip past the /internal* DENY and reach the catch-all backend, which may clean the
  // path up to /internal/... itself. MERGE_SLASHES alone still lets the %2f forms through.
  test.each(INTERNAL_SLASH_CASES)("/internal behind doubled or encoded slashes is denied: %s", async (target) => {
    for (const [cookie, expected] of [
      [undefined, 401],
      [VALID_COOKIE, 403],
    ] as [string | undefined, number][]) {
      const response = await raw(target, { headers: cookie ? [["cookie", cookie]] : [] });
      record("internal slashes", cookie ? "valid cookie" : "no cookie", target, HOSTS.app, response);
      expect(response.body).not.toContain(PROTECTED_MARKER);
      expect(response.status).toBe(expected);
    }
  });

  test("an unknown host gets no route", async () => {
    // TLS still uses a name of the certificate; only the Host header is unknown.
    const response = await raw("/api/whoami", {
      host: "unknown.trustiot.test",
      sni: HOSTS.app,
      headers: [["cookie", VALID_COOKIE]],
    });
    record("cookie", "unknown host", "/api/whoami", "unknown.trustiot.test", response);
    expect(response.status).toBe(404);
  });
});

// ---------------------------------------------------------------------------------------------
// Public paths (decision 27): login, health check, certificate renewal, MQTT.

describe("public paths", () => {
  const whoamiPaths: [string, HostKey, string][] = [
    ["certificate renewal", "gateway", "/certs/renew"],
    ["MQTT over WebSocket", "gateway", "/mqtt"],
  ];

  test.each(whoamiPaths)("%s: no cookie needed, the forged identity header is stripped", async (name, host, path) => {
    const response = await raw(path, {
      host,
      method: "POST",
      headers: [
        ["x-internal-identity", FORGED],
        ["x-forwarded-proto", "http"],
        ["content-length", "0"],
      ],
    });
    record("public", name, path, HOSTS[host], response);
    expect(response.status).toBe(200);
    expect(whoamiHeaderCount(response.body, "x-internal-identity")).toBe(0);
    expect(response.body).not.toContain(FORGED);
    expect(whoamiHeader(response.body, "x-forwarded-proto")).toBe("https");
  });

  test.each([
    ["login", "/auth/login"],
    ["health check", "/healthz"],
  ])("%s on the identity service: no cookie needed, the forged identity header is stripped", async (name, path) => {
    const response = await raw(path, {
      method: "POST",
      headers: [
        ["x-internal-identity", FORGED],
        ["x-forwarded-proto", "http"],
        ["content-length", "0"],
      ],
    });
    record("public", name, path, HOSTS.app, response);
    expect(response.status).toBe(200);
    const echo = JSON.parse(response.body) as { service: string; headers: Record<string, string> };
    expect(echo.service).toBe("identity-double");
    expect(echo.headers["x-internal-identity"]).toBeUndefined();
    expect(echo.headers["x-forwarded-proto"]).toBe("https");
    expect(response.body).not.toContain(FORGED);
  });

  test.each([...whoamiPaths, ["login", "app", "/auth/login"], ["health check", "app", "/healthz"]] as [string, HostKey, string][])(
    "%s: underscore spellings of the identity header are removed",
    async (name, host, path) => {
      const response = await raw(path, {
        host,
        method: "POST",
        headers: [...UNDERSCORE_IDENTITY_HEADERS.map((header): [string, string] => [header, FORGED]), ["content-length", "0"]],
      });
      record("public", `${name}, underscore identity headers`, path, HOSTS[host], response);
      expect(response.status).toBe(200);
      expect(response.body).not.toContain(FORGED);
      expect(response.body).not.toContain(PROTECTED_MARKER);
    },
  );

  // Laysi/trustiot-v2#255 (F3): a gateway, its WebSocket library or a proxy may put the port in
  // Host. The route matches the host with any port, so the exemption does too, and only on the
  // public path's own host (deploy/chart/README.md).
  test("Host with a port: a public path is public on its own host only", async () => {
    const port = SHAPE === "saas" ? 80 : 443;
    for (const [host, path, expected] of [
      [`${HOSTS.gateway}:${port}`, "/mqtt", 200],
      [`${HOSTS.gateway}:${port}`, "/certs/renew", 200],
      [`${HOSTS.gateway}:8443`, "/mqtt", 200],
      [`${HOSTS.app}:${port}`, "/healthz", 200],
      [`${HOSTS.app}:${port}`, "/auth/login", 200],
      [`${HOSTS.app}:${port}`, "/mqtt", 401],
      [`${HOSTS.app}:${port}`, "/certs/renew", 401],
      [`${HOSTS.app}:${port}`, "/api/whoami", 401],
      [`${HOSTS.gateway}:${port}`, "/auth/login", 404],
      [`${HOSTS.ota}:${port}`, "/mqtt", 404],
    ] as [string, string, number][]) {
      const response = await raw(path, {
        host,
        method: "POST",
        headers: [
          ["x-internal-identity", FORGED],
          ["content-length", "0"],
        ],
      });
      record("public, host with port", `${path} on ${host}`, path, host, response);
      expect(response.status).toBe(expected);
      expect(response.body).not.toContain(PROTECTED_MARKER);
      expect(response.body).not.toContain(FORGED);
    }
  });

  test("GET /healthz answers without a cookie", async () => {
    const response = await raw("/healthz");
    record("public", "health check GET", "/healthz", HOSTS.app, response);
    expect(response.status).toBe(200);
    expect(response.body).toBe("ok");
  });

  test("a public path on another host is not public", async () => {
    // hosts.app has a catch-all cookie route, so the request is checked and refused (401).
    // hosts.gateway and hosts.ota have no route for these paths, so the gateway answers 404
    // itself without calling the identity service or any backend.
    for (const [host, path, expected] of [
      ["app", "/mqtt", 401],
      ["app", "/certs/renew", 401],
      ["gateway", "/auth/login", 404],
      ["gateway", "/healthz", 404],
      ["ota", "/mqtt", 404],
      ["ota", "/auth/login", 404],
    ] as [HostKey, string, number][]) {
      const response = await raw(path, { host, headers: [["x-internal-identity", FORGED]] });
      record("public on another host", `${path} on hosts.${host}`, path, HOSTS[host], response);
      expect(response.status).toBe(expected);
      expect(response.body).not.toContain(PROTECTED_MARKER);
      expect(response.body).not.toContain(FORGED);
      expect(response.body).not.toContain("identity-double");
    }
  });
});

// ---------------------------------------------------------------------------------------------
// Path-bypass attempts. Each request tries to reach a cookie route without a cookie by making
// the path look like a public one (or the reverse), based on the forms in
// docs/feasibility/2026-10-10-edge-poc.md: "..", "%2e%2e", ";", "//", case, "?" and "%23".
// None may reach the protected backend, and the forged identity header must never be echoed.

const BYPASS_CASES: { name: string; target: string; host?: HostKey }[] = [
  { name: "dot-dot out of a public path", target: "/healthz/../api/whoami" },
  { name: "encoded dot-dot", target: "/healthz/%2e%2e/api/whoami" },
  { name: "encoded dot-dot, upper case hex", target: "/healthz/%2E%2E/api/whoami" },
  { name: "dot-dot with an encoded slash", target: "/healthz/..%2fapi/whoami" },
  { name: "encoded slashes around dot-dot", target: "/healthz%2f..%2fapi/whoami" },
  { name: "double leading slash", target: "//api/whoami" },
  { name: "double slash after a public path", target: "/healthz//../api/whoami" },
  { name: "upper-case public path then dot-dot", target: "/HEALTHZ/../api/whoami" },
  { name: "upper-case public path", target: "/HEALTHZ" },
  { name: "mixed-case public path", target: "/Auth/Login" },
  { name: "public path of the gateway host, dot-dot", target: "/mqtt/../api/whoami", host: "gateway" },
  { name: "public path in the query string", target: "/api/whoami?x=/healthz" },
  { name: "public path in the query after dot-dot", target: "/api/whoami?/../healthz" },
  { name: "semicolon path parameter, then dot-dot", target: "/healthz;/../api/whoami" },
  { name: "dot-dot with a semicolon", target: "/healthz/..;/api/whoami" },
  { name: "semicolon parameter on a public path", target: "/healthz;x=y" },
  { name: "single dot segment", target: "/./api/whoami" },
  { name: "encoded single dot", target: "/%2e/api/whoami" },
  { name: "encoded hash before a public path", target: "/api/whoami%23/healthz" },
  { name: "raw hash before a public path", target: "/api/whoami#/healthz" },
  { name: "trailing slash on a public path", target: "/healthz/" },
  { name: "public path as a prefix", target: "/healthz/api/whoami" },
  { name: "public path plus suffix", target: "/healthzx" },
  { name: "backslash dot-dot", target: "/healthz\\..\\api\\whoami" },
  { name: "encoded backslash dot-dot", target: "/healthz%5c..%5capi%5cwhoami" },
  { name: "absolute-form request target", target: `http://${"APP_HOST"}/healthz/../api/whoami` },
  { name: "encoded internal prefix", target: "/%69nternal/tenants" },
  { name: "internal prefix after dot-dot", target: "/healthz/../internal/tenants" },
];

describe("path-bypass attempts never reach a cookie route without a cookie", () => {
  test("the list covers at least 11 forms", () => {
    expect(BYPASS_CASES.length).toBeGreaterThanOrEqual(11);
    expect(new Set(BYPASS_CASES.map((c) => c.target)).size).toBe(BYPASS_CASES.length);
  });

  test.each(BYPASS_CASES)("$name: $target", async ({ name, target, host }) => {
    const resolved = target.replace("APP_HOST", HOSTS.app);
    const response = await raw(resolved, { host, headers: [["x-internal-identity", FORGED]] });
    record("bypass", name, resolved, HOSTS[host ?? "app"], response);
    expect(response.body).not.toContain(PROTECTED_MARKER);
    expect(response.body).not.toContain(FORGED);
    // Rejected (400 bad request, 401 no cookie, 403 denied, 404 no route) or, at most, the
    // public health check itself.
    if (response.status === 200) {
      expect(response.body).toBe("ok");
    } else {
      expect([400, 401, 403, 404]).toContain(response.status);
    }
  });
});

// ---------------------------------------------------------------------------------------------
// Identity service failures: the ingress refuses with 503 within the ext_authz timeout, and
// the public paths that do not depend on the identity service keep working.

describe("identity service failures", () => {
  test("slow identity service: 503 after the ext_authz timeout, backend not reached", async () => {
    for (let i = 0; i < 5; i++) {
      const response = await raw("/api/whoami", { headers: [["cookie", SLOW_COOKIE]] });
      record("identity slow", `slow session #${i + 1}`, "/api/whoami", HOSTS.app, response);
      expect(response.status).toBe(503);
      expect(response.body).not.toContain(PROTECTED_MARKER);
      expect(response.ms).toBeGreaterThanOrEqual(AUTHZ_TIMEOUT_MS * 0.9);
      expect(response.ms).toBeLessThan(AUTHZ_TIMEOUT_MS + 1000);
    }
  }, 60_000);

  test("identity service down: cookie routes return 503 within the timeout, public paths keep working", async () => {
    const identity = `deployment/${OBJECT_PREFIX}-identity`;
    kubectl("scale", identity, "--replicas=0");
    try {
      await waitFor("the identity pods to stop", () => {
        const pods = kubectl("get", "pods", "-l", `app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=identity`, "-o", "name").trim();
        return pods === "";
      });
      await waitFor("the gateway to see the identity service down", async () => {
        const response = await raw("/api/whoami", { headers: [["cookie", VALID_COOKIE]] });
        return response.status === 503;
      });
      for (let i = 0; i < 10; i++) {
        for (const [name, cookie] of [
          ["valid cookie", VALID_COOKIE],
          ["no cookie", undefined],
        ] as [string, string | undefined][]) {
          const response = await raw("/api/whoami", { headers: cookie ? [["cookie", cookie]] : [] });
          record("identity down", `${name} #${i + 1}`, "/api/whoami", HOSTS.app, response);
          expect(response.status).toBe(503);
          expect(response.body).not.toContain(PROTECTED_MARKER);
          expect(response.ms).toBeLessThan(AUTHZ_TIMEOUT_MS + 1000);
        }
        for (const path of ["/certs/renew", "/mqtt"]) {
          const response = await raw(path, { host: "gateway", headers: [["x-internal-identity", FORGED]] });
          record("identity down", `public ${path} #${i + 1}`, path, HOSTS.gateway, response);
          expect(response.status).toBe(200);
          expect(whoamiHeaderCount(response.body, "x-internal-identity")).toBe(0);
        }
      }
    } finally {
      kubectl("scale", identity, "--replicas=1");
      kubectl("rollout", "status", identity, "--timeout=180s");
    }
    await waitFor("the identity service to answer again", async () => {
      const response = await raw("/api/whoami", { headers: [["cookie", VALID_COOKIE]] });
      return response.status === 200;
    });
    const after = await raw("/api/whoami");
    record("identity down", "no cookie after recovery", "/api/whoami", HOSTS.app, after);
    expect(after.status).toBe(401);
  }, 400_000);
});
