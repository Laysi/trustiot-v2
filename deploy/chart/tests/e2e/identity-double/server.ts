// Test double of the identity service for the ingress e2e test (Laysi/trustiot-v2#62).
// The real service is Laysi/trustiot-v2#61; this one only mimics its ext_authz contract:
//
//   ANY /internal/ext-authz/<original path>   the ingress gateway's ext_authz check
//       cookie sid=valid-<id>  -> 200 with x-internal-identity: double.<base64url claims>; the
//                                claims hold the Host and the path the check request carried
//                                (Laysi/trustiot-v2#77), which ingress.e2e.ts compares with the
//                                client's request
//       cookie sid=slow-<id>   -> same, after a 3 s delay (longer than the ext_authz timeout)
//       anything else          -> 401 {"code":"unauthenticated"}
//   GET /healthz, GET /readyz                  200 "ok"
//   anything else                              200 JSON echo of the request (exception routes)
//
// Like a TrustIoT service (crates/service-kit), it also listens on a health port (HEALTH_PORT,
// default 8081, the chart's healthPort) that answers GET /healthz and GET /readyz with 200 and
// everything else with 404. The chart's probes use that port.
//
// Like the identity service, it serves the token exchange on a port of its own (EXCHANGE_PORT,
// default 8082, the chart's port token-exchange; Laysi/trustiot-v2#106). It exchanges nothing:
// every request there gets the same 200 JSON echo, so mesh.e2e.ts sees whether the mesh let the
// call through and whether the caller's token arrived.
//
// The echo lists every request header (repeated headers joined by ", "), so a test can see
// whether an x-internal-identity header reached this backend. Never deploy this outside a test
// namespace: any "valid-" session is accepted.

const PORT = Number(process.env.PORT ?? "8080");
const HEALTH_PORT = Number(process.env.HEALTH_PORT ?? "8081");
const EXCHANGE_PORT = Number(process.env.EXCHANGE_PORT ?? "8082");
const CHECK_PREFIX = "/internal/ext-authz";
const SLOW_MS = 3000;

function sessionId(cookieHeader: string | null): string | undefined {
  if (!cookieHeader) return undefined;
  for (const part of cookieHeader.split(";")) {
    const [name, ...rest] = part.trim().split("=");
    if (name === "sid") return rest.join("=");
  }
  return undefined;
}

function json(status: number, body: unknown, extra: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...extra },
  });
}

async function check(req: Request, url: URL): Promise<Response> {
  const sid = sessionId(req.headers.get("cookie"));
  const valid = sid !== undefined && /^(valid|slow)-[A-Za-z0-9_-]{1,64}$/.test(sid);
  if (!valid) return json(401, { code: "unauthenticated" });
  if (sid.startsWith("slow-")) await Bun.sleep(SLOW_MS);
  const claims = {
    sub: `user-${sid}`,
    tenant: "tenant-e2e",
    path: url.pathname.slice(CHECK_PREFIX.length) || "/",
    // The original request's Host as the gateway passes it to the check, port included or not.
    host: req.headers.get("host"),
  };
  const identity = `double.${Buffer.from(JSON.stringify(claims)).toString("base64url")}`;
  return new Response(null, { status: 200, headers: { "x-internal-identity": identity } });
}

function isProbe(req: Request, url: URL): boolean {
  return req.method === "GET" && (url.pathname === "/healthz" || url.pathname === "/readyz");
}

function echo(req: Request, url: URL): Response {
  return json(200, {
    service: "identity-double",
    method: req.method,
    path: url.pathname,
    query: url.search,
    headers: Object.fromEntries(req.headers.entries()),
  });
}

const health = Bun.serve({
  port: HEALTH_PORT,
  hostname: "0.0.0.0",
  fetch(req) {
    return isProbe(req, new URL(req.url)) ? new Response("ok") : new Response("not found", { status: 404 });
  },
});

const server = Bun.serve({
  port: PORT,
  hostname: "0.0.0.0",
  async fetch(req) {
    const url = new URL(req.url);
    if (url.pathname === CHECK_PREFIX || url.pathname.startsWith(`${CHECK_PREFIX}/`)) {
      return check(req, url);
    }
    if (isProbe(req, url)) return new Response("ok");
    return echo(req, url);
  },
});

const exchange = Bun.serve({
  port: EXCHANGE_PORT,
  hostname: "0.0.0.0",
  fetch(req) {
    return echo(req, new URL(req.url));
  },
});

// The container runs this process as PID 1, which has no default SIGTERM handler.
for (const signal of ["SIGTERM", "SIGINT"] as const) {
  process.on(signal, () => {
    server.stop(true);
    exchange.stop(true);
    health.stop(true);
    process.exit(0);
  });
}

console.log(
  `identity double listening on ${server.hostname}:${server.port}, token exchange on port ${exchange.port}, health on port ${health.port}`,
);
