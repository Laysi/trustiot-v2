// The demo environment's SPA and cross-site check (web-check.ts, Laysi/trustiot-v2#961) against a
// fake ingress on loopback: every check passes against the routing and the cookie check the chart
// and istiod set up, and each way the demo could be wrong (web off, istiod without the headers,
// Access refusing the token, the login path re-routed, an expired session ...) fails the check
// that covers it. The output never carries the cookie, the token, the host or a body.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  CROSS_SITE_REQUEST,
  DEEP_LINK,
  FOREIGN_ORIGIN,
  InputError,
  LOGIN_PATH,
  POOL_PATH,
  SESSION_COOKIE,
  checkPrivateFile,
  compareIstiod,
  describe as describeAnswer,
  makeSend,
  parseAccessHeaders,
  parseArgs,
  parseExpectedOrigin,
  parseSession,
  readLimited,
  report,
  run,
  runChecks,
  staticAsset,
  type Outcome,
} from "./web-check.ts";

const ORIGIN = "https://app.example.test";
const COOKIE = "s3ss10n-c00k1e-value";
const CLIENT_ID = "client-id-0123.access";
// Made at run time: scripts/tests/policy.test.ts refuses a literal assigned to a secret-looking name.
const CLIENT_SECRET = `client-secret-${crypto.randomUUID()}`;
const ACCESS: [string, string][] = [
  ["cf-access-client-id", CLIENT_ID],
  ["cf-access-client-secret", CLIENT_SECRET],
];
const ASSET = "/static/B1Kiyeui.js";
const INDEX = `<!doctype html><html><head><script type="module" src="${ASSET}"></script></head><body><div id="root"></div></body></html>`;
const WEB_HEADERS = {
  "content-security-policy": "default-src 'none'; script-src 'self'; frame-ancestors 'none'",
  "x-frame-options": "DENY",
};
// A marker the pool answer carries: the output must never show it.
const POOL_MARKER = "tg-hardware-id-000961";

/** What the fake ingress gets wrong; every field off is the demo as it should be. */
interface Faults {
  /** services.web off: every app path goes through the cookie check (the routing before #961). */
  webOff?: boolean;
  /** /api goes to web instead of the cookie check. */
  apiToWeb?: boolean;
  /** istiod's provider passes neither Origin nor Sec-Fetch-Site to the cookie check. */
  noCheckHeaders?: boolean;
  /** istiod passes Sec-Fetch-Site only. */
  noOrigin?: boolean;
  /** Access does not accept the service token: every request goes to its login. */
  accessRefuses?: boolean;
  /** POST /auth/login goes through the cookie check instead of straight to the login. */
  loginChecked?: boolean;
  /** A deep link answers 404 instead of index.html, or another HTML page of web. */
  deepLink404?: boolean;
  deepLinkOther?: boolean;
  /** /static/ assets without the immutable cache policy. */
  assetCached?: boolean;
  /** The device pool answers 200 without a list. */
  poolNotList?: boolean;
  /** index.html names no /static/ asset. */
  noAsset?: boolean;
  /** The session is not a platform administrator's (or has expired: `expired`). */
  notAdmin?: boolean;
  expired?: boolean;
  /** The cookie check refuses same-origin writes as cross-site too. */
  refusesAll?: boolean;
  /** Cloudflare challenges every request. */
  challenge?: boolean;
}

/** Every request the fake ingress got, to check what the script sent. */
const seen: Request[] = [];
let faults: Faults = {};

const json = (status: number, body: unknown, headers: Record<string, string> = {}): Response =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", ...headers } });
const error = (status: number, code: string, key?: string): Response => json(status, { code, ...(key === undefined ? {} : { message_key: key }) });

/** The identity service's cross-site rule (services/identity/src/csrf.rs), with what istiod passes. */
function crossSite(request: Request): boolean {
  const method = request.method;
  const safe = method === "GET" || method === "HEAD";
  const site = faults.noCheckHeaders ? null : request.headers.get("sec-fetch-site");
  const origin = faults.noCheckHeaders || faults.noOrigin ? null : request.headers.get("origin");
  if (site !== null) return !(site === "same-origin" || (site === "none" && safe));
  if (safe) return false;
  return origin !== null && origin !== ORIGIN;
}

function web(path: string, method: string): Response {
  if (method !== "GET") return new Response("method not allowed\n", { status: 405, headers: { ...WEB_HEADERS, "content-type": "text/plain; charset=utf-8" } });
  if (path.startsWith("/static/")) {
    if (path !== ASSET) return new Response("not found\n", { status: 404, headers: { ...WEB_HEADERS, "content-type": "text/plain" } });
    const cache = faults.assetCached ? "no-cache" : "public, max-age=31536000, immutable";
    return new Response("export {};", { headers: { ...WEB_HEADERS, "content-type": "text/javascript; charset=utf-8", "cache-control": cache } });
  }
  if (faults.deepLink404 && path !== "/") return new Response("not found\n", { status: 404, headers: { ...WEB_HEADERS, "content-type": "text/plain" } });
  const other = faults.deepLinkOther && path !== "/";
  const body = faults.noAsset ? INDEX.replace(ASSET, "/main.js") : other ? INDEX.replace("<head>", "<head><title>other</title>") : INDEX;
  return new Response(body, { headers: { ...WEB_HEADERS, "content-type": "text/html; charset=utf-8", "cache-control": "no-cache" } });
}

/** The ingress, the cookie check and the backends, as the chart routes the app host with web on. */
function ingress(request: Request): Response {
  seen.push(request);
  const url = new URL(request.url);
  const path = url.pathname;
  if (faults.accessRefuses || request.headers.get("cf-access-client-id") !== CLIENT_ID || request.headers.get("cf-access-client-secret") !== CLIENT_SECRET) {
    return new Response(null, { status: 302, headers: { location: `https://team-x.cloudflareaccess.com/cdn-cgi/access/login/app?redirect_url=${encodeURIComponent(path)}` } });
  }
  if (faults.challenge) return new Response("<html>challenge</html>", { status: 403, headers: { "cf-mitigated": "challenge", server: "cloudflare", "content-type": "text/html" } });
  const cookiePath = path === "/api" || path.startsWith("/api/") || path === "/auth" || path.startsWith("/auth/");
  const loginExempt = path === LOGIN_PATH && request.method === "POST" && !faults.loginChecked;
  if (loginExempt) {
    // The identity service's login: the cross-site check, then the Content-Type (login.rs).
    if (crossSite(request)) return error(403, "permission_denied", CROSS_SITE_REQUEST);
    if (!(request.headers.get("content-type") ?? "").startsWith("application/json")) return error(400, "invalid_request");
    return json(202, {});
  }
  if (faults.apiToWeb && path.startsWith("/api/")) return web(path, request.method);
  if (!cookiePath && !faults.webOff) return web(path, request.method);
  // The cookie check: cross-site first, then the session (services/identity/src/ext_authz.rs).
  if (faults.refusesAll && request.method !== "GET") return error(403, "permission_denied", CROSS_SITE_REQUEST);
  if (crossSite(request)) return error(403, "permission_denied", CROSS_SITE_REQUEST);
  if (request.headers.get("cookie") !== `${SESSION_COOKIE}=${COOKIE}` || faults.expired) return error(401, "unauthenticated");
  // The backends behind the cookie routes.
  if (`${path}${url.search}` === POOL_PATH || path === "/api/assets/v1/platform/gateways") {
    if (request.method !== "GET") return new Response(null, { status: 405, headers: { allow: "GET,HEAD" } });
    if (faults.notAdmin) return error(403, "permission_denied");
    return json(200, faults.poolNotList ? { hardware_id: POOL_MARKER } : { gateways: [{ hardware_id: POOL_MARKER }], next_after: null });
  }
  return new Response("not found", { status: 404 });
}

let server: ReturnType<typeof Bun.serve>;
let base = "";
let dir = "";

beforeAll(() => {
  server = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: ingress });
  base = `http://127.0.0.1:${server.port}`;
  dir = mkdtempSync(join(tmpdir(), "web-check-"));
});

afterAll(() => {
  server.stop(true);
  rmSync(dir, { recursive: true, force: true });
});

async function check(f: Faults = {}): Promise<Outcome[]> {
  faults = f;
  seen.length = 0;
  return runChecks(makeSend(base, ACCESS), ORIGIN, COOKIE);
}

const failed = (outcomes: Outcome[]): string[] => outcomes.filter((o) => !o.pass).map((o) => o.name);
const SECRETS = [COOKIE, CLIENT_ID, CLIENT_SECRET, "app.example.test", "127.0.0.1", POOL_MARKER, "team-x", "<div", "export {}"];
const leaks = (lines: string[]): string[] => lines.filter((line) => SECRETS.some((s) => line.includes(s)));

describe("web-check against a fake ingress", () => {
  test("every check passes when web answers the app host, /api and /auth keep the cookie check, and the check sees both headers", async () => {
    const outcomes = await check();
    expect(failed(outcomes)).toEqual([]);
    expect(outcomes).toHaveLength(13);
    const lines = report(outcomes);
    expect(lines.at(-1)).toBe("PASS all 13 checks");
    expect(leaks(lines)).toEqual([]);
    // What was seen, for the test report: statuses, codes and keys only.
    expect(lines).toContain(`PASS cross-site: POST ${POOL_PATH}, the cookie, a foreign Origin, no Sec-Fetch-Site: 403 permission_denied ${CROSS_SITE_REQUEST}`);
    expect(lines).toContain(`PASS routing: POST ${LOGIN_PATH} without a cookie or a body: 400 invalid_request`);
    expect(lines).toContain("PASS routing: GET / without a cookie: 200 from web (text/html)");
  });

  test("every request carries the service token, the cookie only where a check needs it, and the headers as named", async () => {
    await check();
    expect(seen).toHaveLength(13);
    for (const request of seen) {
      expect(request.headers.get("cf-access-client-id")).toBe(CLIENT_ID);
      expect(request.headers.get("cf-access-client-secret")).toBe(CLIENT_SECRET);
      expect(request.headers.get("user-agent")).toBe("trustiot-demo-web-check");
    }
    const withCookie = seen.filter((r) => r.headers.get("cookie") !== null);
    expect(withCookie).toHaveLength(7);
    for (const request of withCookie) expect(request.headers.get("cookie")).toBe(`${SESSION_COOKIE}=${COOKIE}`);
    const origins = seen.map((r) => r.headers.get("origin")).filter((o) => o !== null);
    expect(origins).toEqual([FOREIGN_ORIGIN, ORIGIN, ORIGIN]);
    const sites = seen.map((r) => r.headers.get("sec-fetch-site")).filter((s) => s !== null);
    expect(sites).toEqual(["same-origin", "cross-site", "cross-site", "same-origin"]);
    // The writes go to the GET-only pool and the login only, never with a body.
    const posts = seen.filter((r) => r.method === "POST");
    expect(posts.map((r) => new URL(r.url).pathname).sort()).toEqual([LOGIN_PATH, ...Array(4).fill("/api/assets/v1/platform/gateways")].sort());
    for (const post of posts) expect(await post.text()).toBe("");
    expect(seen.map((r) => new URL(r.url).pathname)).toContain(DEEP_LINK);
  });

  test("with web off (the routing before #961) the SPA checks fail and the rest pass", async () => {
    expect(failed(await check({ webOff: true }))).toEqual([
      "routing: GET / without a cookie",
      `routing: GET ${DEEP_LINK} (a deep link) without a cookie`,
      "routing: GET / with the session cookie",
      "routing: GET /static/* (an asset index.html loads)",
    ]);
  });

  test("/api answered by web fails every /api check: web's 405 is not the assets service's", async () => {
    expect(failed(await check({ apiToWeb: true }))).toEqual([
      `routing: GET ${POOL_PATH} without a cookie`,
      `cross-site: GET ${POOL_PATH}, the cookie, Sec-Fetch-Site: same-origin`,
      `cross-site: GET ${POOL_PATH}, the cookie, Sec-Fetch-Site: cross-site`,
      `cross-site: POST ${POOL_PATH}, the cookie, Sec-Fetch-Site: cross-site`,
      `cross-site: POST ${POOL_PATH}, the cookie, a foreign Origin, no Sec-Fetch-Site`,
      `cross-site: POST ${POOL_PATH}, the cookie, the app's Origin, Sec-Fetch-Site: same-origin`,
      `cross-site: POST ${POOL_PATH}, the cookie, the app's Origin, no Sec-Fetch-Site`,
    ]);
  });

  test("istiod without Origin and Sec-Fetch-Site in the check: every refusal fails, the controls pass", async () => {
    expect(failed(await check({ noCheckHeaders: true }))).toEqual([
      `cross-site: GET ${POOL_PATH}, the cookie, Sec-Fetch-Site: cross-site`,
      `cross-site: POST ${POOL_PATH}, the cookie, Sec-Fetch-Site: cross-site`,
      `cross-site: POST ${POOL_PATH}, the cookie, a foreign Origin, no Sec-Fetch-Site`,
    ]);
  });

  test("istiod passing Sec-Fetch-Site but not Origin: only the foreign Origin case fails", async () => {
    expect(failed(await check({ noOrigin: true }))).toEqual([`cross-site: POST ${POOL_PATH}, the cookie, a foreign Origin, no Sec-Fetch-Site`]);
  });

  test("a check that refuses same-origin writes too fails the same-origin controls", async () => {
    expect(failed(await check({ refusesAll: true }))).toEqual([
      `cross-site: POST ${POOL_PATH}, the cookie, the app's Origin, Sec-Fetch-Site: same-origin`,
      `cross-site: POST ${POOL_PATH}, the cookie, the app's Origin, no Sec-Fetch-Site`,
    ]);
  });

  test("Access refusing the service token fails every check and says so, without the login URL", async () => {
    const outcomes = await check({ accessRefuses: true });
    expect(outcomes.every((o) => !o.pass)).toBe(true);
    const lines = report(outcomes);
    expect(lines[0]).toBe("FAIL routing: GET / without a cookie: 302 to the Cloudflare Access login (the service token did not pass); expected 200 index.html from web");
    expect(leaks(lines)).toEqual([]);
  });

  test("a Cloudflare challenge is named as one", async () => {
    const lines = report(await check({ challenge: true }));
    expect(lines[0]).toContain(": 403 a Cloudflare challenge;");
    expect(lines.at(-1)).toBe("FAIL 13 of 13 checks");
  });

  test("the login path going through the cookie check fails the POST /auth/login check", async () => {
    expect(failed(await check({ loginChecked: true }))).toEqual([`routing: POST ${LOGIN_PATH} without a cookie or a body`]);
  });

  test("a deep link that web does not answer with index.html fails", async () => {
    expect(failed(await check({ deepLink404: true }))).toEqual([`routing: GET ${DEEP_LINK} (a deep link) without a cookie`]);
  });

  test("a deep link that web answers with another page than / fails", async () => {
    expect(failed(await check({ deepLinkOther: true }))).toEqual([`routing: GET ${DEEP_LINK} (a deep link) without a cookie`]);
  });

  test("an asset without the immutable cache policy fails", async () => {
    expect(failed(await check({ assetCached: true }))).toEqual(["routing: GET /static/* (an asset index.html loads) without a cookie"]);
  });

  test("a device pool answer that is not a list fails the control", async () => {
    expect(failed(await check({ poolNotList: true }))).toEqual([`cross-site: GET ${POOL_PATH}, the cookie, Sec-Fetch-Site: same-origin`]);
  });

  test("an answer that is not the platform's names neither its location nor its body", () => {
    const answer = (status: number, headers: Record<string, string>, body = "") => ({ status, headers: new Headers(headers), body });
    expect(describeAnswer(answer(302, { location: "https://elsewhere.example.test/x?token=1" }))).toBe("302 a redirect elsewhere");
    expect(describeAnswer(answer(302, { location: "https://team-x.cloudflareaccess.com/login" }))).toBe("302 to the Cloudflare Access login (the service token did not pass)");
    expect(describeAnswer(answer(403, { server: "cloudflare", "content-type": "text/html; charset=UTF-8" }, "<html>error 1020</html>"))).toBe("403 from Cloudflare (text/html)");
    expect(describeAnswer(answer(500, { "content-type": "text/plain" }, "secret detail"))).toBe("500 text/plain");
    expect(describeAnswer(answer(401, { "content-type": "application/json" }, '{"code":"unauthenticated","message_key":"x.y","detail":"a b"}'))).toBe("401 unauthenticated x.y");
    // A code that is not a plain key is left out rather than printed.
    expect(describeAnswer(answer(400, { "content-type": "application/json" }, '{"code":"<script>"}'))).toBe("400 application/json");
  });

  test("an index.html without a /static/ asset fails the asset check", async () => {
    expect(failed(await check({ noAsset: true }))).toEqual(["routing: GET /static/* (an asset index.html loads)"]);
  });

  test("a session that is not a platform administrator's fails the control, so the refusals prove nothing alone", async () => {
    expect(failed(await check({ notAdmin: true }))).toEqual([`cross-site: GET ${POOL_PATH}, the cookie, Sec-Fetch-Site: same-origin`]);
  });

  test("an expired session fails the control and the same-origin writes", async () => {
    const outcomes = await check({ expired: true });
    expect(failed(outcomes)).toEqual([
      `cross-site: GET ${POOL_PATH}, the cookie, Sec-Fetch-Site: same-origin`,
      `cross-site: POST ${POOL_PATH}, the cookie, the app's Origin, Sec-Fetch-Site: same-origin`,
      `cross-site: POST ${POOL_PATH}, the cookie, the app's Origin, no Sec-Fetch-Site`,
    ]);
    expect(report(outcomes)[7]).toBe(
      `FAIL cross-site: GET ${POOL_PATH}, the cookie, Sec-Fetch-Site: same-origin: 401 unauthenticated; expected 200 with the device pool (a platform administrator's session; without it the refusals below prove nothing)`,
    );
  });

  test("an unreachable origin fails every check with a fixed text, never the URL", async () => {
    const closed = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response("") });
    const url = `http://127.0.0.1:${closed.port}`;
    closed.stop(true);
    const outcomes = await runChecks(makeSend(url, ACCESS), ORIGIN, COOKIE);
    expect(outcomes.every((o) => !o.pass)).toBe(true);
    const lines = report(outcomes);
    expect(lines[0]).toMatch(/^FAIL routing: GET \/ without a cookie: the request failed \([A-Za-z]+\); expected/);
    expect(leaks(lines)).toEqual([]);
  });

  test("a request that takes too long fails with the time it waited", async () => {
    const slow = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Promise<Response>(() => {}) });
    try {
      const answer = await makeSend(`http://127.0.0.1:${slow.port}`, ACCESS, 200)("GET", "/", []);
      expect(describeAnswer(answer)).toBe("no answer within 0.2 s");
    } finally {
      slow.stop(true);
    }
  });

  test("an endless body is read only up to the limit and the transfer is cancelled", async () => {
    let sent = 0;
    let cancelled = false;
    const chunk = new Uint8Array(4096).fill(0x61);
    const endless = Bun.serve({
      hostname: "127.0.0.1",
      port: 0,
      fetch: () =>
        new Response(
          new ReadableStream<Uint8Array>({
            pull(controller) {
              sent += chunk.length;
              controller.enqueue(chunk);
            },
            cancel() {
              cancelled = true;
            },
          }),
        ),
    });
    try {
      const answer = await makeSend(`http://127.0.0.1:${endless.port}`, ACCESS, 30_000, 10_000)("GET", "/", []);
      expect(answer.status).toBe(200);
      expect(answer.body).toBe("a".repeat(10_000));
      // The server stops once the client goes away; wait for it rather than for a fixed time.
      const deadline = Date.now() + 30_000;
      while (!cancelled && Date.now() < deadline) await Bun.sleep(10);
      expect(cancelled, "the server's stream was not cancelled after the limit").toBe(true);
      // Only what the transport buffered ahead was sent, nowhere near an endless body.
      expect(sent).toBeLessThan(64 * 1024 * 1024);
    } finally {
      endless.stop(true);
    }
  });

  test("a body is cut at the limit across chunks and kept whole below it", async () => {
    const stream = (parts: string[]) =>
      new ReadableStream<Uint8Array>({
        start(controller) {
          for (const part of parts) controller.enqueue(new TextEncoder().encode(part));
          controller.close();
        },
      });
    const text = async (parts: string[], limit: number) => new TextDecoder().decode(await readLimited(stream(parts), limit));
    expect(await text(["abc", "def", "ghi"], 5)).toBe("abcde");
    expect(await text(["abc", "def"], 6)).toBe("abcdef");
    expect(await text(["abc", "def"], 100)).toBe("abcdef");
    expect(await text([], 100)).toBe("");
    expect((await readLimited(null, 100)).length).toBe(0);
  });
});

describe("web-check input", () => {
  test("the expected origin is https:// and a lower-case host name, nothing else", () => {
    expect(parseExpectedOrigin(`${ORIGIN}\n`)).toBe(ORIGIN);
    for (const bad of [
      "http://app.example.test",
      "https://App.example.test",
      "https://app.example.test/",
      "https://app.example.test:443",
      "https://app.example.test/path",
      "https://user@app.example.test",
      "https://192.0.2.10",
      "https://localhost",
      "app.example.test",
      `${ORIGIN}\n${ORIGIN}`,
      "",
    ]) {
      expect(() => parseExpectedOrigin(bad)).toThrow(InputError);
    }
    // The refusal never quotes the value.
    try {
      parseExpectedOrigin("https://secret-host.example.test:8443");
    } catch (error) {
      expect((error as Error).message).not.toContain("secret-host");
    }
  });

  test("the session file has one origin and one cookie value", () => {
    expect(parseSession(`origin=${ORIGIN}\ncookie=${COOKIE}\n`)).toEqual({ origin: ORIGIN, cookie: COOKIE });
    for (const bad of [`cookie=${COOKIE}\n`, `origin=${ORIGIN}\n`, `origin=${ORIGIN}\ncookie=${COOKIE}\ncookie=${COOKIE}\n`, `origin=${ORIGIN}\ncookie=a; b=c\n`, `origin=${ORIGIN}\ncookie=\n`, `x=y\norigin=${ORIGIN}\ncookie=${COOKIE}\n`]) {
      expect(() => parseSession(bad)).toThrow(InputError);
    }
  });

  test("the Access headers file has exactly the two service token headers, and errors name lines only", () => {
    expect(parseAccessHeaders(`# token\nCF-Access-Client-Id: ${CLIENT_ID}\n\ncf-access-client-secret: ${CLIENT_SECRET}\n`)).toEqual(ACCESS);
    for (const bad of [
      `cf-access-client-id: ${CLIENT_ID}\n`,
      `cf-access-client-id: ${CLIENT_ID}\ncf-access-client-id: ${CLIENT_ID}\ncf-access-client-secret: ${CLIENT_SECRET}\n`,
      `cf-access-client-id: ${CLIENT_ID}\ncf-access-client-secret: ${CLIENT_SECRET}\nauthorization: Bearer x\n`,
      `cf-access-client-id: ${CLIENT_ID}\ncf-access-client-secret: a b\n`,
    ]) {
      let message = "";
      try {
        parseAccessHeaders(bad);
      } catch (error) {
        expect(error).toBeInstanceOf(InputError);
        message = (error as Error).message;
      }
      expect(message).not.toBe("");
      expect(message).not.toContain(CLIENT_ID);
      expect(message).not.toContain(CLIENT_SECRET);
      expect(message).not.toContain("Bearer");
    }
  });

  test("secret files must be this user's regular files of mode 0600 or narrower", () => {
    const file = join(dir, "private");
    writeFileSync(file, "x", { mode: 0o600 });
    for (const mode of [0o600, 0o400]) {
      chmodSync(file, mode);
      expect(() => checkPrivateFile(file, "the file")).not.toThrow();
    }
    // Owner execute is refused as well as every group and other bit.
    for (const mode of [0o700, 0o500, 0o640, 0o610, 0o604, 0o602, 0o601, 0o666]) {
      chmodSync(file, mode);
      expect(() => checkPrivateFile(file, "the file"), mode.toString(8)).toThrow(/must be 0600 or narrower/);
    }
    chmodSync(file, 0o600);
    const link = join(dir, "link");
    symlinkSync(file, link);
    expect(() => checkPrivateFile(link, "the file")).toThrow(/not a regular file/);
    expect(() => checkPrivateFile(join(dir, "missing"), "the file")).toThrow(/cannot be read/);
  });

  test("the command line takes three file options and echoes no value", () => {
    const env = { HOME: "/h", XDG_RUNTIME_DIR: "/run/user/1000" };
    expect(parseArgs(["--expected-origin-file", "/o"], env)).toEqual({
      expectedOrigin: "/o",
      session: "/run/user/1000/trustiot-workstation/session",
      accessHeaders: "/h/.config/trustiot-workstation/access-headers",
    });
    expect(parseArgs(["--expected-origin-file", "/o", "--session-file", "/s", "--access-headers-file", "/h"], env)).toEqual({ expectedOrigin: "/o", session: "/s", accessHeaders: "/h" });
    expect(() => parseArgs([], env)).toThrow("--expected-origin-file is required");
    expect(() => parseArgs(["--expected-origin-file", "/o"], { HOME: "/h" })).toThrow("give --session-file");
    let message = "";
    try {
      parseArgs([ORIGIN], env);
    } catch (error) {
      message = (error as Error).message;
    }
    expect(message).toContain("unknown argument at position 1");
    expect(message).not.toContain("app.example.test");
  });

  test("staticAsset finds a hashed asset of index.html and nothing outside /static/", () => {
    expect(staticAsset(INDEX)).toBe(ASSET);
    expect(staticAsset('<link rel="stylesheet" href="/static/DDEsU6_R.css">')).toBe("/static/DDEsU6_R.css");
    expect(staticAsset('<script src="https://elsewhere.example.com/static/x.js"></script>')).toBeUndefined();
    expect(staticAsset('<script src="/static/../x.js"></script>')).toBeUndefined();
  });
});

describe("web-check run", () => {
  const files = (session: string, origin = `${ORIGIN}\n`, headers = `cf-access-client-id: ${CLIENT_ID}\ncf-access-client-secret: ${CLIENT_SECRET}\n`) => {
    const o = join(dir, "origin");
    const s = join(dir, "session");
    const h = join(dir, "headers");
    for (const [path, text] of [
      [o, origin],
      [s, session],
      [h, headers],
    ] as const) {
      rmSync(path, { force: true });
      writeFileSync(path, text, { mode: 0o600 });
    }
    return ["--expected-origin-file", o, "--session-file", s, "--access-headers-file", h];
  };

  async function runWith(argv: string[], f: Faults = {}): Promise<{ code: number; out: string[]; err: string[] }> {
    faults = f;
    seen.length = 0;
    const out: string[] = [];
    const err: string[] = [];
    const code = await run(argv, { HOME: dir }, { out: (l) => out.push(l), err: (l) => err.push(l), baseUrl: base });
    return { code, out, err };
  }

  test("exits 0 when every check passes and prints only safe lines", async () => {
    const result = await runWith(files(`origin=${ORIGIN}\ncookie=${COOKIE}\n`));
    expect(result.code).toBe(0);
    expect(result.err).toEqual([]);
    expect(result.out.at(-1)).toBe("PASS all 13 checks");
    expect(leaks([...result.out, ...result.err])).toEqual([]);
  });

  test("exits 1 when a check fails", async () => {
    const result = await runWith(files(`origin=${ORIGIN}\ncookie=${COOKIE}\n`), { noCheckHeaders: true });
    expect(result.code).toBe(1);
    expect(result.out.at(-1)).toBe("FAIL 3 of 13 checks");
    expect(leaks(result.out)).toEqual([]);
  });

  test("refuses, before any request, a session of another origin", async () => {
    const result = await runWith(files(`origin=https://other.example.test\ncookie=${COOKIE}\n`));
    expect(result.code).toBe(2);
    expect(seen).toHaveLength(0);
    expect(result.err).toEqual(["refused: the session belongs to another platform than the expected origin (log in with the workstation tool to the demo app first)"]);
    expect(result.out).toEqual([]);
  });

  test("refuses, before any request, an expected origin that is not one", async () => {
    const result = await runWith(files(`origin=${ORIGIN}\ncookie=${COOKIE}\n`, "http://app.example.test\n"));
    expect(result.code).toBe(2);
    expect(seen).toHaveLength(0);
    expect(leaks(result.err)).toEqual([]);
  });

  test("refuses, before any request, a session file others can read", async () => {
    const argv = files(`origin=${ORIGIN}\ncookie=${COOKIE}\n`);
    chmodSync(join(dir, "session"), 0o644);
    const result = await runWith(argv);
    expect(result.code).toBe(2);
    expect(seen).toHaveLength(0);
    expect(result.err[0]).toContain("has mode 644; it must be 0600");
  });
});

// The istio ConfigMap's mesh config as k3s's helm-controller renders it from
// deploy/istio/values-k3s.yaml and istiod-values.yaml (keys sorted, as istiod writes them).
const LIVE_MESH = `defaultConfig:
  discoveryAddress: istiod.istio-system.svc:15012
extensionProviders:
- envoyExtAuthzHttp:
    failOpen: false
    headersToUpstreamOnAllow:
    - x-internal-identity
    includeRequestHeadersInCheck:
    - cookie
    - origin
    - sec-fetch-site
    - trustiot-expected-user
    - trustiot-expected-tenant
    pathPrefix: /internal/ext-authz
    port: 8080
    service: trustiot-identity.trustiot.svc.cluster.local
    statusOnError: "503"
    timeout: 0.5s
  name: trustiot-identity
pathNormalization:
  normalization: DECODE_AND_MERGE_SLASHES
rootNamespace: istio-system
trustDomain: cluster.local
`;

describe("web-check istiod", () => {
  const repo = Bun.file(join(import.meta.dir, "..", "..", "deploy", "istio", "istiod-values.yaml"));

  test("the mesh that istiod-values.yaml renders matches, and the lines name settings and header names only", async () => {
    const result = compareIstiod(LIVE_MESH, await repo.text());
    expect(result.pass).toBe(true);
    expect(result.lines).toEqual([
      "pathNormalization.normalization: DECODE_AND_MERGE_SLASHES (repo: DECODE_AND_MERGE_SLASHES)",
      "trustiot-identity.service: trustiot-identity.trustiot.svc.cluster.local",
      "trustiot-identity.port: 8080",
      "trustiot-identity.pathPrefix: /internal/ext-authz",
      "trustiot-identity.failOpen: false",
      "trustiot-identity.timeout: 0.5s",
      "trustiot-identity.statusOnError: 503",
      "trustiot-identity.includeRequestHeadersInCheck: cookie, origin, sec-fetch-site, trustiot-expected-tenant, trustiot-expected-user",
      "trustiot-identity.headersToUpstreamOnAllow: x-internal-identity",
      "PASS istiod's mesh config matches deploy/istio/istiod-values.yaml",
    ]);
  });

  test("the header lists compare as sets of names", async () => {
    const reordered = LIVE_MESH.replace(
      "    - cookie\n    - origin\n    - sec-fetch-site\n    - trustiot-expected-user\n    - trustiot-expected-tenant\n",
      "    - Trustiot-Expected-Tenant\n    - Sec-Fetch-Site\n    - cookie\n    - trustiot-expected-user\n    - origin\n",
    );
    expect(reordered).not.toBe(LIVE_MESH);
    expect(compareIstiod(reordered, await repo.text()).pass).toBe(true);
  });

  test("an istiod without origin and sec-fetch-site (the setting before Laysi/trustiot-v2#123) fails and says where", async () => {
    const old = LIVE_MESH.replace("    - origin\n    - sec-fetch-site\n", "");
    expect(old).not.toBe(LIVE_MESH);
    const result = compareIstiod(old, await repo.text());
    expect(result.pass).toBe(false);
    expect(result.lines).toContain(
      "trustiot-identity.includeRequestHeadersInCheck: cookie, trustiot-expected-tenant, trustiot-expected-user (repo: cookie, origin, sec-fetch-site, trustiot-expected-tenant, trustiot-expected-user)",
    );
    expect(result.lines.at(-1)).toBe("FAIL istiod differs from deploy/istio/istiod-values.yaml in: trustiot-identity.includeRequestHeadersInCheck");
  });

  test("a repo with more headers than the live istiod (a change such as Laysi/trustiot-v2#962) fails until istiod is aligned", async () => {
    const more = (await repo.text()).replace("          - sec-fetch-site\n", "          - sec-fetch-site\n          - x-test-extra-header\n");
    expect(more).toContain("x-test-extra-header");
    expect(compareIstiod(LIVE_MESH, more).lines.at(-1)).toBe("FAIL istiod differs from deploy/istio/istiod-values.yaml in: trustiot-identity.includeRequestHeadersInCheck");
  });

  test("every other difference fails: fail-open, another service, no normalization, no provider, an extra setting", async () => {
    const text = await repo.text();
    const cases: [string, string][] = [
      [LIVE_MESH.replace("failOpen: false", "failOpen: true"), "trustiot-identity.failOpen"],
      [LIVE_MESH.replace("service: trustiot-identity.trustiot", "service: poc-identity.poc"), "trustiot-identity.service"],
      [LIVE_MESH.replace("  normalization: DECODE_AND_MERGE_SLASHES\n", "  normalization: BASE\n"), "pathNormalization.normalization"],
      [LIVE_MESH.replace("name: trustiot-identity", "name: e2e-identity"), "extensionProviders.trustiot-identity"],
      [LIVE_MESH.replace("    timeout: 0.5s\n", "    timeout: 0.5s\n    withRequestBody:\n      maxRequestBytes: 10\n"), "trustiot-identity.withRequestBody"],
    ];
    for (const [mesh, setting] of cases) {
      expect(mesh).not.toBe(LIVE_MESH);
      const result = compareIstiod(mesh, text);
      expect({ setting, pass: result.pass, last: result.lines.at(-1) }).toEqual({ setting, pass: false, last: `FAIL istiod differs from deploy/istio/istiod-values.yaml in: ${setting}` });
    }
    expect(compareIstiod("a: [", text)).toEqual({ pass: false, lines: ["FAIL the live mesh config is not YAML"] });
  });

  test("run istiod reads stdin and exits 0 when the same, 1 when not, 2 with an option", async () => {
    const lines: string[] = [];
    const io = (mesh: string) => ({ out: (l: string) => lines.push(l), err: (l: string) => lines.push(l), stdin: async () => mesh });
    expect(await run(["istiod"], {}, io(LIVE_MESH))).toBe(0);
    expect(await run(["istiod"], {}, io(LIVE_MESH.replace("    - origin\n", "")))).toBe(1);
    expect(await run(["istiod", "--expected-origin-file", "x"], {}, io(LIVE_MESH))).toBe(2);
  });
});
