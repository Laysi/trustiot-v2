// Ingress e2e test (Laysi/trustiot-v2#62): installs the chart with test doubles into an existing
// namespace of any cluster with Istio ambient, then checks the cookie check, the public paths,
// the header rules, path-bypass attempts and an identity-service outage through the real ingress
// gateway. The /internal slash cases (Laysi/trustiot-v2#255) also need the mesh's path
// normalization from deploy/istio/istiod-values.yaml, and the cross-site cases
// (Laysi/trustiot-v2#943) its provider's includeRequestHeadersInCheck. The SPA's static files
// (services.web) are on in this test's own release only (values-e2e.yaml). README.md in this
// directory lists the prerequisites; run from the repository root:
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
  type CheckHeaders,
  crossSiteRefusalProblems,
  doubleClaims,
  sameOriginPassProblems,
  staticFileProblems,
  whoamiHeader,
  whoamiHeaderCount,
} from "./ingress-answers.ts";
import { type RawEndpoint, type RawResponse, sendRaw } from "./raw-client.ts";
import { withTunnelRecovery } from "./tunnel-recovery.ts";
import {
  CONTEXT,
  GATEWAY,
  IDENTITY_IMAGE,
  KEEP,
  KUBECTL,
  MIGRATE_IMAGE,
  NAMESPACE,
  OBJECT_PREFIX,
  RELEASE,
  REPORT_DIR,
  freePort,
  imageValues,
  installRelease,
  migrateImageValues,
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
// The SPA's static files (services.web): every path on hosts.app outside /api and /auth.
const SPA_MARKER = "Name: spa-static";
const FORGED = `forged.${crypto.randomUUID()}`;
const VALID_COOKIE = `sid=valid-${crypto.randomUUID().replaceAll("-", "")}`;
const SLOW_COOKIE = `sid=slow-${crypto.randomUUID().replaceAll("-", "")}`;

// ---------------------------------------------------------------------------------------------
// Raw HTTP/1.1 client (raw-client.ts): fetch() would normalize the request target (dot segments,
// encodings), which is exactly what the bypass cases must send unchanged.

let ingress: RawEndpoint;

type RawOptions = {
  host?: HostKey | string;
  method?: string;
  headers?: [string, string][];
  // TLS server name (on-prem shape); defaults to the Host header without its port.
  sni?: string;
};

// Sends one request. When the tunnel to the gateway (ssh or kubectl port-forward) was lost while
// the request was in flight, for example because the SSH session was dropped or kubectl printed
// "lost connection to pod", reopens it and sends the request once more (tunnel-recovery.ts); any
// other failure, an HTTP status that the test does not expect included, fails the test.
function raw(target: string, options: RawOptions = {}): Promise<RawResponse> {
  const hostHeader = options.host === undefined ? HOSTS.app : (HOSTS[options.host as HostKey] ?? options.host);
  const request = { target, method: options.method, hostHeader, headers: options.headers, sni: options.sni };
  return withTunnelRecovery(() => sendRaw(ingress, request), {
    tunnel: () => tunnel,
    reopen: openTunnel,
    log: (message) => console.error(message),
  });
}

// What the cookie check was asked about: the Host and the path in the claims of the double's
// identity header, which whoami echoes (ingress-answers.ts).
function checkedRequest(body: string): { host: unknown; path: unknown } {
  const { host, path } = doubleClaims(body);
  return { host, path };
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
  const extra: string[] = [
    "--set",
    `ingress.authzProvider=${PROVIDER}`,
    ...imageValues("identity", IDENTITY_IMAGE),
    ...migrateImageValues(MIGRATE_IMAGE),
  ];
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

// Spellings of /api/whoami that the mesh's path normalization turns into /api/whoami.
const NORMALIZED_WHOAMI_CASES = ["//api/whoami", "/api//whoami", "/./api/whoami", "/api/./whoami", "/api/x/../whoami", "/api%2fwhoami", "/api%2Fwhoami"];

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
  // spellings slip past the /internal* DENY and the cookie check and reach the SPA's static files
  // (a catch-all backend), which may clean the path up to /internal/... itself. MERGE_SLASHES
  // alone still lets the %2f forms through.
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

  // Laysi/trustiot-v2#77, from the notes of Laysi/trustiot-v2#255 (deploy/chart/README.md, "Host"
  // with a port; deploy/istio/README.md, path normalization): what the gateway hands to the
  // identity service. The Host reaches the check as the client sent it, port included, and the
  // services' audience lookup (services/identity/src/audience.rs) ignores the port.
  test("the cookie check gets the client's Host as sent, with or without a port", async () => {
    const port = SHAPE === "saas" ? 80 : 443;
    for (const host of [HOSTS.app, `${HOSTS.app}:${port}`, `${HOSTS.app}:8443`]) {
      const response = await raw("/api/whoami", { host, headers: [["cookie", VALID_COOKIE]] });
      record("checked request", `Host ${host}`, "/api/whoami", host, response);
      expect(response.status).toBe(200);
      expect(response.body).toContain(PROTECTED_MARKER);
      expect(checkedRequest(response.body)).toEqual({ host, path: "/api/whoami" });
    }
  });

  // The mesh normalizes the path before routing and authorization (DECODE_AND_MERGE_SLASHES), so
  // the cookie check and the backend both see /api/whoami, never the spelling the client sent.
  test.each(NORMALIZED_WHOAMI_CASES)("the cookie check and the backend get the normalized path: %s", async (target) => {
    const response = await raw(target, { headers: [["cookie", VALID_COOKIE]] });
    record("checked request", "normalized path", target, HOSTS.app, response);
    expect(response.status).toBe(200);
    expect(response.body).toContain(PROTECTED_MARKER);
    expect(checkedRequest(response.body)).toEqual({ host: HOSTS.app, path: "/api/whoami" });
    // whoami echoes its request line first.
    expect(response.body).toContain("GET /api/whoami HTTP/1.1");
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
  // Each with a method it admits (Laysi/trustiot-v2#859): the renewal takes POST, the root CRL GET
  // and HEAD, the MQTT WebSocket GET. whoami answers any of them with the echo.
  const whoamiPaths: [string, HostKey, string, string][] = [
    ["certificate renewal", "gateway", "/certs/renew", "POST"],
    ["root CRL", "gateway", "/certs/root-crl", "GET"],
    ["MQTT over WebSocket", "gateway", "/mqtt", "GET"],
  ];

  test.each(whoamiPaths)("%s: no cookie needed, the forged identity header is stripped", async (name, host, path, method) => {
    const response = await raw(path, {
      host,
      method,
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

  // Only login: the health check admits GET and HEAD, which the identity double answers with "ok"
  // or no body, so its headers are not visible here; its route strips the same headers as the
  // login's (chart.test.ts compares every public route's filter).
  test.each([["login", "/auth/login"]])("%s on the identity service: no cookie needed, the forged identity header is stripped", async (name, path) => {
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

  test.each([...whoamiPaths, ["login", "app", "/auth/login", "POST"], ["health check", "app", "/healthz", "GET"]] as [string, HostKey, string, string][])(
    "%s: underscore spellings of the identity header are removed",
    async (name, host, path, method) => {
      const response = await raw(path, {
        host,
        method,
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
  // public path's own host (deploy/chart/README.md). On hosts.app the gateway's public paths are
  // the SPA's static files.
  test("Host with a port: a public path is public on its own host only", async () => {
    const port = SHAPE === "saas" ? 80 : 443;
    // Each public path with a method it admits (Laysi/trustiot-v2#859); POST elsewhere.
    for (const [host, path, method, expected, marker] of [
      [`${HOSTS.gateway}:${port}`, "/mqtt", "GET", 200],
      [`${HOSTS.gateway}:${port}`, "/certs/renew", "POST", 200],
      [`${HOSTS.gateway}:${port}`, "/certs/root-crl", "GET", 200],
      [`${HOSTS.gateway}:8443`, "/mqtt", "GET", 200],
      [`${HOSTS.app}:${port}`, "/healthz", "GET", 200],
      [`${HOSTS.app}:${port}`, "/auth/login", "POST", 200],
      [`${HOSTS.app}:${port}`, "/mqtt", "POST", 200, SPA_MARKER],
      [`${HOSTS.app}:${port}`, "/certs/renew", "POST", 200, SPA_MARKER],
      [`${HOSTS.app}:${port}`, "/api/whoami", "POST", 401],
      [`${HOSTS.gateway}:${port}`, "/auth/login", "POST", 404],
      [`${HOSTS.ota}:${port}`, "/mqtt", "POST", 404],
    ] as [string, string, string, number, string?][]) {
      const response = await raw(path, {
        host,
        method,
        headers: [
          ["x-internal-identity", FORGED],
          ["content-length", "0"],
        ],
      });
      record("public, host with port", `${method} ${path} on ${host}`, path, host, response);
      expect(response.status).toBe(expected);
      if (marker !== undefined) expect(response.body).toContain(marker);
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

  // Laysi/trustiot-v2#859: a public path admits only the methods its service serves there. Any
  // other method on its own host gets the cookie check (401 without a cookie) and then the DENY
  // policy (403 with a valid one: the identity double signs for any path, so only DENY refuses
  // it), and never reaches the backend.
  test.each([
    ["certificate renewal", "gateway", "/certs/renew", ["GET", "HEAD", "PUT", "DELETE"]],
    ["root CRL", "gateway", "/certs/root-crl", ["POST", "PUT", "DELETE", "PATCH", "OPTIONS"]],
    ["MQTT over WebSocket", "gateway", "/mqtt", ["POST", "HEAD", "PUT"]],
    ["login", "app", "/auth/login", ["GET", "HEAD", "PUT"]],
    ["health check", "app", "/healthz", ["POST", "PUT", "DELETE"]],
  ] as [string, HostKey, string, string[]][])("%s: any other method is refused at the ingress, with or without a cookie", async (name, host, path, methods) => {
    for (const method of methods) {
      for (const [cookie, expected] of [
        [undefined, 401],
        [VALID_COOKIE, 403],
      ] as [string | undefined, number][]) {
        const response = await raw(path, {
          host,
          method,
          headers: [["x-internal-identity", FORGED], ...(cookie === undefined ? [] : [["cookie", cookie] as [string, string]]), ...(method === "HEAD" || method === "GET" ? [] : [["content-length", "0"] as [string, string]])],
        });
        record("public, other method", `${name}: ${method}${cookie === undefined ? "" : " with a valid cookie"}`, path, HOSTS[host], response);
        expect({ method, cookie: cookie !== undefined, status: response.status }).toEqual({ method, cookie: cookie !== undefined, status: expected });
        for (const backend of ["Name: public-", "identity-double", PROTECTED_MARKER, FORGED]) expect(response.body).not.toContain(backend);
      }
    }
  });

  test("a public path on another host is not public", async () => {
    // On hosts.app every path outside /api and /auth is the SPA's static files, so the request
    // gets them, not the gateway's backend, and without the forged header. hosts.gateway and
    // hosts.ota have no route for these paths, so the gateway answers 404 itself without calling
    // the identity service or any backend.
    for (const [host, path, expected, marker] of [
      ["app", "/mqtt", 200, SPA_MARKER],
      ["app", "/certs/renew", 200, SPA_MARKER],
      ["gateway", "/auth/login", 404],
      ["gateway", "/healthz", 404],
      ["ota", "/mqtt", 404],
      ["ota", "/auth/login", 404],
    ] as [HostKey, string, number, string?][]) {
      const response = await raw(path, { host, headers: [["x-internal-identity", FORGED]] });
      record("public on another host", `${path} on hosts.${host}`, path, HOSTS[host], response);
      expect(response.status).toBe(expected);
      if (marker !== undefined) expect(response.body).toContain(marker);
      expect(response.body).not.toContain(PROTECTED_MARKER);
      expect(response.body).not.toContain(FORGED);
      expect(response.body).not.toContain("identity-double");
    }
  });
});

// ---------------------------------------------------------------------------------------------
// The SPA's static files (services.web, Laysi/trustiot-v2#943): on hosts.app every path outside
// the cookie prefixes /api and /auth (and the denied /internal) goes to web without the cookie
// check, and web never gets the session or an identity: the route removes the Cookie and every
// spelling of x-internal-identity (templates/ingress.yaml). Each condition is in
// ingress-answers.ts, whose unit test shows it failing on a broken answer.

// The root, a deep link of the client-side router, a hashed asset, and the edges of the /api
// prefix that the gateway and Istio do not count as under it (case, no "/" after the prefix).
const SPA_PATHS = ["/", "/devices/4f1c2a/settings", "/static/B7f3kQ2x.js", "/API/x", "/apix"];

describe("the SPA's static files on hosts.app", () => {
  // Without a cookie the cookie check would answer 401: web answering proves it was not asked.
  test.each(SPA_PATHS)("%s without a cookie: web answers, the cookie check is not asked", async (target) => {
    const response = await raw(target);
    record("spa", "no cookie", target, HOSTS.app, response);
    const problems = staticFileProblems(response, SPA_MARKER, PROTECTED_MARKER, `GET ${target} HTTP/1.1`, []);
    expect({ target, problems }).toEqual({ target, problems: [] });
  });

  // A valid session and forged identity headers: the route must drop the Cookie, the forged
  // x-internal-identity and its underscore spellings before web. Web getting no identity header
  // proves nothing about the cookie check, because the public static route removes that header
  // after ext_authz either way (templates/ingress.yaml); the 200 without a cookie above is the
  // proof that no check runs.
  test.each(SPA_PATHS)("%s with a valid cookie and forged identity headers: web gets none of them", async (target) => {
    const response = await raw(target, {
      headers: [["cookie", `theme=dark; ${VALID_COOKIE}`], ["x-internal-identity", FORGED], ...UNDERSCORE_IDENTITY_HEADERS.map((name): [string, string] => [name, FORGED])],
    });
    record("spa", "valid cookie + forged identity headers", target, HOSTS.app, response);
    const problems = staticFileProblems(response, SPA_MARKER, PROTECTED_MARKER, `GET ${target} HTTP/1.1`, [VALID_COOKIE, "theme=dark", FORGED]);
    expect({ target, problems }).toEqual({ target, problems: [] });
  });

  // RFC 3986 normalization (part of DECODE_AND_MERGE_SLASHES, deploy/istio/istiod-values.yaml)
  // decodes a percent-encoded letter before routing and authorization, so /%61pi/x is /api/x: it
  // is the cookie route's and checked like it, never web's without a check. The same decoding
  // made /%69nternal/tenants meet the /internal DENY on 2026-10-10
  // (docs/test-reports/2026-10-10-62-e2e-verify.md, T5).
  test("an encoded letter of the cookie prefix (/%61pi/x) is /api/x: the cookie check applies", async () => {
    const target = "/%61pi/x";
    const anonymous = await raw(target);
    record("spa", "encoded prefix, no cookie", target, HOSTS.app, anonymous);
    expect({ status: anonymous.status, unauthenticated: anonymous.body.includes("unauthenticated"), web: anonymous.body.includes(SPA_MARKER) }).toEqual({
      status: 401,
      unauthenticated: true,
      web: false,
    });
    // Booleans, never the body itself: a failed toContain prints the whole answer into the CI log.
    expect({ backend: anonymous.body.includes(PROTECTED_MARKER) }).toEqual({ backend: false });
    const response = await raw(target, { headers: [["cookie", VALID_COOKIE]] });
    record("spa", "encoded prefix, valid cookie", target, HOSTS.app, response);
    const none: CheckHeaders = { origin: null, "sec-fetch-site": null };
    expect(sameOriginPassProblems(response, none, PROTECTED_MARKER, "GET /api/x HTTP/1.1", "/api/x")).toEqual([]);
    // The backend echoes the cookie, so the body must not reach a failure message either.
    expect({ web: response.body.includes(SPA_MARKER) }).toEqual({ web: false });
  });
});

// ---------------------------------------------------------------------------------------------
// Cross-site requests (Laysi/trustiot-v2#943, decision 27): the identity service refuses a
// cross-site request with a session cookie (services/identity/src/csrf.rs) only when the mesh's
// ext_authz provider passes Origin and Sec-Fetch-Site to the check (includeRequestHeadersInCheck
// in deploy/istio/istiod-values.yaml); without them it lets every request through. The double
// applies the same rule (identity-double/server.ts, tested in identity-double.test.ts) and lists
// the two headers it received, so each case below also shows that the gateway passed exactly what
// the client sent. A provider without the headers makes every refusal a 200 from the backend and
// every "received" null, and both fail here.

const APP_ORIGIN = `https://${HOSTS.app}`;
const FOREIGN_ORIGIN = "https://attacker.example.test";

describe("cross-site requests on a cookie route", () => {
  test.each([
    ["POST with Sec-Fetch-Site: cross-site", "POST", { origin: null, "sec-fetch-site": "cross-site" }],
    ["GET with Sec-Fetch-Site: cross-site", "GET", { origin: null, "sec-fetch-site": "cross-site" }],
    ["POST with a foreign Origin and no Sec-Fetch-Site", "POST", { origin: FOREIGN_ORIGIN, "sec-fetch-site": null }],
  ] as [string, string, CheckHeaders][])("%s and a valid cookie: 403 security.cross_site_request from the check, backend not reached", async (name, method, sent) => {
    const response = await raw("/api/whoami", { method, headers: crossSiteHeaders(sent, method) });
    record("csrf", name, "/api/whoami", HOSTS.app, response);
    expect({ name, problems: crossSiteRefusalProblems(response, sent, PROTECTED_MARKER) }).toEqual({ name, problems: [] });
  });

  test.each([
    ["POST with Sec-Fetch-Site: same-origin and the app's Origin", "POST", { origin: APP_ORIGIN, "sec-fetch-site": "same-origin" }],
    ["POST with the app's Origin and no Sec-Fetch-Site", "POST", { origin: APP_ORIGIN, "sec-fetch-site": null }],
  ] as [string, string, CheckHeaders][])("%s and a valid cookie: the backend answers, and the check received both headers as sent", async (name, method, sent) => {
    const response = await raw("/api/whoami", { method, headers: crossSiteHeaders(sent, method) });
    record("csrf", name, "/api/whoami", HOSTS.app, response);
    const problems = sameOriginPassProblems(response, sent, PROTECTED_MARKER, `${method} /api/whoami HTTP/1.1`, "/api/whoami");
    expect({ name, problems }).toEqual({ name, problems: [] });
  });
});

// The valid cookie, the cross-site headers that are set in `sent`, and an empty body for a write.
function crossSiteHeaders(sent: CheckHeaders, method: string): [string, string][] {
  const headers: [string, string][] = [["cookie", VALID_COOKIE]];
  if (sent.origin !== null) headers.push(["origin", sent.origin]);
  if (sent["sec-fetch-site"] !== null) headers.push(["sec-fetch-site", sent["sec-fetch-site"]]);
  if (method !== "GET" && method !== "HEAD") headers.push(["content-length", "0"]);
  return headers;
}

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
    // Rejected (400 bad request, 401 no cookie, 403 denied, 404 no route) or, at most, a public
    // answer: the health check itself or, on hosts.app, the SPA's static files.
    if (response.status === 200) {
      if (response.body !== "ok") expect(response.body).toContain(SPA_MARKER);
    } else {
      expect([400, 401, 403, 404]).toContain(response.status);
    }
  });
});

// ---------------------------------------------------------------------------------------------
// Rate limit (Laysi/trustiot-v2#113): the SaaS shape counts each client by CF-Connecting-IP, in
// Envoy's local rate limit filter that the chart's EnvoyFilter puts first in the gateway's HTTP
// filters. tests/e2e/values-e2e.yaml gives /api/rate-limited a rule of its own with this limit,
// refilled over an hour, so the count does not depend on how fast the test sends. On-prem renders
// no rate limit until it has a client source of its own (Laysi/trustiot-v2#446).
const RATE_LIMITED_PATH = "/api/rate-limited";
const RATE_LIMIT_REQUESTS = 5;
// Each replica counts on its own (ingress.replicas, 2 by default): through the Service the
// requests may spread over both, so the gateway may let up to this many through.
const INGRESS_REPLICAS = 2;
// Envoy's body for a request it limits (local rate limit filter, local reply).
const RATE_LIMITED_BODY = "local_rate_limited";

describe("rate limit", () => {
  test.skipIf(SHAPE !== "saas")(
    "past a rule's limit the gateway answers 429 before the cookie check, per CF-Connecting-IP, whatever X-Forwarded-For says",
    async () => {
      // A fresh /64 from the IPv6 documentation range for each client: the gateway counts an IPv6
      // client by its /64, so a random address alone could land in a /64 an earlier run used up.
      const hex4 = () => crypto.randomUUID().slice(0, 4);
      const fresh64 = () => `2001:db8:${hex4()}:${hex4()}::1`;
      const client = fresh64();
      const other = fresh64();
      const statuses: number[] = [];
      let limited: RawResponse | undefined;
      for (let i = 0; i < RATE_LIMIT_REQUESTS * INGRESS_REPLICAS + 1 && !limited; i++) {
        const response = await raw(RATE_LIMITED_PATH, {
          // A new X-Forwarded-For on every request: it must not start a new count.
          headers: [["cf-connecting-ip", client], ["x-forwarded-for", `203.0.113.${i + 1}`]],
        });
        record("rate limit", `${client} #${i + 1}`, RATE_LIMITED_PATH, HOSTS.app, response);
        statuses.push(response.status);
        if (response.status === 429) limited = response;
      }
      // Before the limit the cookie check answers (401, no cookie); the backend is never reached.
      const before = statuses.slice(0, statuses.indexOf(429) < 0 ? statuses.length : statuses.indexOf(429));
      expect(before.every((status) => status === 401)).toBe(true);
      // A request the tunnel lost and the test sent again may have been counted once more.
      expect(before.length).toBeGreaterThanOrEqual(RATE_LIMIT_REQUESTS - 1);
      expect(limited?.status).toBe(429);
      expect(limited?.body).toContain(RATE_LIMITED_BODY);
      expect(limited?.body).not.toContain(PROTECTED_MARKER);
      // Another client is not limited, even with the limited client's address in X-Forwarded-For.
      const another = await raw(RATE_LIMITED_PATH, { headers: [["cf-connecting-ip", other], ["x-forwarded-for", client]] });
      record("rate limit", `${other} after ${client}`, RATE_LIMITED_PATH, HOSTS.app, another);
      expect(another.status).toBe(401);
    },
    60_000,
  );

  test.skipIf(SHAPE !== "onprem")("on-prem has no rate limit yet: the same requests all reach the cookie check", async () => {
    for (let i = 0; i < RATE_LIMIT_REQUESTS * INGRESS_REPLICAS + 1; i++) {
      const response = await raw(RATE_LIMITED_PATH, { headers: [["cf-connecting-ip", "192.0.2.1"]] });
      record("rate limit", `on-prem #${i + 1}`, RATE_LIMITED_PATH, HOSTS.app, response);
      expect(response.status).toBe(401);
    }
  }, 60_000);
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
        // Each with a method it admits (Laysi/trustiot-v2#859): another method is checked, so it
        // would get 503 here.
        for (const [path, method] of [
          ["/certs/renew", "POST"],
          ["/certs/root-crl", "GET"],
          ["/mqtt", "GET"],
        ] as [string, string][]) {
          const response = await raw(path, {
            host: "gateway",
            method,
            headers: [["x-internal-identity", FORGED], ...(method === "POST" ? [["content-length", "0"] as [string, string]] : [])],
          });
          record("identity down", `public ${method} ${path} #${i + 1}`, path, HOSTS.gateway, response);
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
