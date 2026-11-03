// The identity double's cross-site check (identity-double/server.ts, Laysi/trustiot-v2#943),
// without a cluster: the double runs as a Bun child process on free local ports, and the test
// sends its ext_authz endpoint the check requests the mesh's provider would send. The ingress e2e
// test relies on two things shown here: the double refuses exactly the requests the identity
// service's rule refuses for the cases the e2e sends, and what it reports it received is the two
// headers of the cross-site check and nothing else (CI logs are public).
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { join } from "node:path";
import type { Subprocess } from "bun";
import { useProcessTestTimeouts } from "../timing.ts";
import { CROSS_SITE_KEY, type CheckHeaders } from "./ingress-answers.ts";
import { freePort } from "./ports.ts";
import { type RawResponse, sendRaw } from "./raw-client.ts";

useProcessTestTimeouts();

const SERVER = join(import.meta.dir, "identity-double", "server.ts");
const APP = "app.saas.trustiot.test";
const CHECK = "/internal/ext-authz/api/whoami";
const SESSION = crypto.randomUUID().replaceAll("-", "");
const COOKIE = `sid=valid-${SESSION}`;
// Values the double must never repeat: the cookie's session, a token and an unrelated header.
const AUTHORIZATION = `Bearer secret-${crypto.randomUUID()}`;
const OTHER = `other-${crypto.randomUUID()}`;
const SECRETS = [SESSION, AUTHORIZATION, OTHER];
// How long the double may take to listen: a Bun start on a loaded machine, far above an idle one.
const START_LIMIT_MS = 60_000;

let double: Subprocess | undefined;
let port = 0;

beforeAll(async () => {
  port = await freePort();
  const health = await freePort();
  const exchange = await freePort();
  double = Bun.spawn([process.execPath, "run", SERVER], {
    env: { ...process.env, PORT: String(port), HEALTH_PORT: String(health), EXCHANGE_PORT: String(exchange) },
    stdout: "ignore",
    stderr: "inherit",
  });
  const started = Date.now();
  for (;;) {
    if (double.exitCode !== null) throw new Error(`the identity double exited with ${double.exitCode} before it listened`);
    const ready = await sendRaw({ host: "127.0.0.1", port: health, tls: false }, { target: "/readyz", hostHeader: "localhost" }).then(
      (response) => response.status === 200,
      () => false,
    );
    if (ready) break;
    if (Date.now() - started > START_LIMIT_MS) throw new Error(`timed out after ${START_LIMIT_MS / 1000} s waiting for the identity double to answer /readyz`);
    await Bun.sleep(100);
  }
});

afterAll(() => {
  double?.kill();
});

type Case = { name: string; method: string; sent: CheckHeaders; host?: string };

function checkRequest(c: Case, cookie: string | undefined): Promise<RawResponse> {
  const headers: [string, string][] = [
    ...(cookie === undefined ? [] : [["cookie", cookie] as [string, string]]),
    ["authorization", AUTHORIZATION],
    ["x-other", OTHER],
  ];
  if (c.sent.origin !== null) headers.push(["origin", c.sent.origin]);
  if (c.sent["sec-fetch-site"] !== null) headers.push(["sec-fetch-site", c.sent["sec-fetch-site"]]);
  return sendRaw({ host: "127.0.0.1", port, tls: false }, { target: CHECK, method: c.method, hostHeader: c.host ?? APP, headers, body: c.method === "GET" || c.method === "HEAD" ? undefined : "" });
}

// What a client could read from the double's answer: its headers and body, as text.
function everything(response: RawResponse): string {
  return `${JSON.stringify(response.headers)}\n${response.body}`;
}

function claimsOf(response: RawResponse): Record<string, unknown> {
  const header = response.headers["x-internal-identity"] ?? [];
  if (header.length !== 1 || !header[0]!.startsWith("double.")) throw new Error(`expected one identity header from the double, got ${JSON.stringify(header)}`);
  return JSON.parse(Buffer.from(header[0]!.slice("double.".length), "base64url").toString("utf8")) as Record<string, unknown>;
}

const none: CheckHeaders = { origin: null, "sec-fetch-site": null };
const ORIGIN = `https://${APP}`;

// Cross-site for services/identity/src/csrf.rs too (its tests list the same forms).
const CROSS_SITE: Case[] = [
  { name: "POST, Sec-Fetch-Site cross-site", method: "POST", sent: { ...none, "sec-fetch-site": "cross-site" } },
  { name: "POST, Sec-Fetch-Site same-site", method: "POST", sent: { ...none, "sec-fetch-site": "same-site" } },
  { name: "POST, Sec-Fetch-Site none", method: "POST", sent: { ...none, "sec-fetch-site": "none" } },
  { name: "GET, Sec-Fetch-Site cross-site", method: "GET", sent: { ...none, "sec-fetch-site": "cross-site" } },
  { name: "DELETE, Sec-Fetch-Site same-origin twice", method: "DELETE", sent: { ...none, "sec-fetch-site": "same-origin, same-origin" } },
  { name: "POST, a foreign Origin", method: "POST", sent: { ...none, origin: "https://attacker.example.test" } },
  { name: "PUT, Origin null", method: "PUT", sent: { ...none, origin: "null" } },
  { name: "POST, the app over http", method: "POST", sent: { ...none, origin: `http://${APP}` } },
  { name: "POST, the app on another port", method: "POST", sent: { ...none, origin: `${ORIGIN}:8443` } },
  { name: "PATCH, the app's Origin twice", method: "PATCH", sent: { ...none, origin: `${ORIGIN}, ${ORIGIN}` } },
  { name: "OPTIONS, a foreign Origin", method: "OPTIONS", sent: { ...none, origin: "https://attacker.example.test" } },
];

// Allowed by services/identity/src/csrf.rs too.
const SAME_ORIGIN: Case[] = [
  { name: "POST, Sec-Fetch-Site same-origin and the app's Origin", method: "POST", sent: { origin: ORIGIN, "sec-fetch-site": "same-origin" } },
  { name: "POST, Sec-Fetch-Site same-origin and a foreign Origin", method: "POST", sent: { origin: "https://attacker.example.test", "sec-fetch-site": "same-origin" } },
  { name: "POST, the app's Origin without Sec-Fetch-Site", method: "POST", sent: { ...none, origin: ORIGIN } },
  { name: "POST, the app's Origin with the port in Host", method: "POST", sent: { ...none, origin: `${ORIGIN}:8443` }, host: `${APP}:8443` },
  { name: "GET, Sec-Fetch-Site none", method: "GET", sent: { ...none, "sec-fetch-site": "none" } },
  { name: "GET, a foreign Origin without Sec-Fetch-Site", method: "GET", sent: { ...none, origin: "https://attacker.example.test" } },
  { name: "POST, neither header", method: "POST", sent: none },
];

describe("the identity double's cross-site check", () => {
  test.each(CROSS_SITE)("$name: 403 security.cross_site_request, before the cookie, listing the two headers it received", async (c) => {
    for (const cookie of [COOKIE, undefined]) {
      const response = await checkRequest(c, cookie);
      expect({ status: response.status, cookie: cookie !== undefined }).toEqual({ status: 403, cookie: cookie !== undefined });
      expect(JSON.parse(response.body)).toEqual({ code: "permission_denied", message_key: CROSS_SITE_KEY, received: c.sent });
      expect(response.headers["cache-control"]).toEqual(["no-store"]);
      expect(response.headers["x-internal-identity"]).toBeUndefined();
      for (const secret of SECRETS) expect(everything(response)).not.toContain(secret);
    }
  });

  test.each(SAME_ORIGIN)("$name: allowed, and the claims list the two headers it received", async (c) => {
    const response = await checkRequest(c, COOKIE);
    expect(response.status).toBe(200);
    const claims = claimsOf(response);
    expect(claims.received).toEqual(c.sent);
    expect(claims.path).toBe("/api/whoami");
    expect(claims.host).toBe(c.host ?? APP);
    for (const secret of SECRETS) expect(everything(response)).not.toContain(secret);
    // Decoded too: the claims carry no header besides the two, so not the cookie, the token or x-other.
    const decoded = JSON.stringify(claims);
    for (const secret of SECRETS) expect(decoded).not.toContain(secret);
    // Without a cookie the request is refused for the session, not for where it comes from.
    const anonymous = await checkRequest(c, undefined);
    expect(anonymous.status).toBe(401);
    expect(JSON.parse(anonymous.body)).toEqual({ code: "unauthenticated" });
  });
});
