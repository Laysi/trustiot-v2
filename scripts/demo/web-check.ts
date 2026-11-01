/**
 * The demo environment's check of the SPA's routing and of the cross-site refusal through the
 * real ingress (Laysi/trustiot-v2#961, docs/runbooks/demo-environment.md 「打開 SPA（services.web）」):
 * Cloudflare, the Access application, the tunnel, the ingress gateway, istiod's ext_authz
 * provider and the identity service's cookie check, all as the demo environment runs them.
 *
 *   bun scripts/demo/web-check.ts --expected-origin-file <file>
 *     [--session-file <file>] [--access-headers-file <file>]
 *   <the istio ConfigMap's mesh config> | bun scripts/demo/web-check.ts istiod
 *
 * `istiod` (read only, before the change merges) compares istiod's live mesh config with
 * deploy/istio/istiod-values.yaml: the path normalization and the ext_authz provider the cookie
 * check uses, both header lists included. Without `origin` and `sec-fetch-site` in
 * includeRequestHeadersInCheck the cookie check lets every cross-site request through, silently.
 *
 * It reads everything it needs from files, never from the command line:
 * - --expected-origin-file: one line, `https://` and the app host name in lower case, written by
 *   the person who runs the check. The script talks to this origin only.
 * - --session-file: the production workstation tool's session file (`origin=` and `cookie=`, by
 *   default $XDG_RUNTIME_DIR/trustiot-workstation/session), a platform administrator's session.
 *   Its origin must be the expected one, or the script refuses to send the cookie anywhere.
 * - --access-headers-file: the workstation's Cloudflare Access service token
 *   (`cf-access-client-id:` and `cf-access-client-secret:`, by default
 *   ~/.config/trustiot-workstation/access-headers).
 * Both secret files must be regular files of the user who runs it, readable by nobody else
 * (0600), as the workstation tool requires.
 *
 * Every request carries the service token, so it passes Access; the checks (CHECKS below) need no
 * write that changes anything: the "writes" go to a GET-only route of the assets service, which
 * answers 405 once a request gets past the cookie check, and the login path gets a request
 * without a body, which the identity service refuses before it reads anything. The assets service
 * does not look at Origin or Sec-Fetch-Site itself, so a 403 security.cross_site_request there
 * can only come from the cookie check, which sees the two headers only when istiod passes them
 * (includeRequestHeadersInCheck in deploy/istio/istiod-values.yaml).
 *
 * The output names each check, its path (never the host), the status, the M1 error code and
 * message key, and fixed words; never a URL, a header value, the cookie or a body. It exits 0
 * when every check passes, 1 when any fails, and 2 when it refuses its input (before any
 * request).
 */

import { lstatSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { ROOT, shownPath } from "./lib.ts";

/** The identity service's session cookie (services/identity, `__Host-` so it is bound to the host). */
export const SESSION_COOKIE = "__Host-trustiot-session";
/** The message key of the cookie check's 403 for a cross-site request (services/identity/src/csrf.rs). */
export const CROSS_SITE_REQUEST = "security.cross_site_request";
/** Another site's origin for the cross-site cases (RFC 2606, never resolves to anything real). */
export const FOREIGN_ORIGIN = "https://attacker.example.com";
/**
 * The assets service's device pool (services/assets/src/gateways/mod.rs): platform
 * administrators only, GET only, so a POST that gets past the cookie check answers 405 and
 * changes nothing. `limit=1` keeps the answer small; the script never prints it.
 */
export const POOL_PATH = "/api/assets/v1/platform/gateways?limit=1";
/** The identity service's login path, the one cookie-free path under /auth (POST only). */
export const LOGIN_PATH = "/auth/login";
/** A deep link of the SPA's client-side router: no file has this name, so web answers index.html. */
export const DEEP_LINK = "/devices/00000000-0000-4000-8000-000000000961/settings";
/** How long one request may take before the check gives up on it. */
export const REQUEST_TIMEOUT_MS = 30_000;
/** The most of a body the script reads; the SPA's largest asset is far below it. */
export const MAX_BODY_BYTES = 16 * 1024 * 1024;

/** Thrown for input the script refuses; the message never quotes a file's content. */
export class InputError extends Error {}

/** One answer, as the checks see it. */
export interface Answer {
  status: number;
  headers: Headers;
  body: string;
}

/** Sends one request to the origin; `path` starts with "/". */
export type Send = (method: "GET" | "POST", path: string, headers: [string, string][]) => Promise<Answer>;

/** One check's outcome: what it was, whether it passed, and what was seen (safe to print). */
export interface Outcome {
  name: string;
  pass: boolean;
  seen: string;
  expected: string;
}

// ---------------------------------------------------------------------------------------------
// Input

/**
 * The expected origin: exactly `https://<host>` with a lower-case DNS name of at least two labels,
 * no port, path, query, user or trailing slash. Anything else is refused without quoting it.
 */
export function parseExpectedOrigin(text: string): string {
  const lines = text.split("\n").map((line) => line.replace(/\r$/, "")).filter((line) => line.trim() !== "");
  if (lines.length !== 1) throw new InputError("the expected origin file must hold exactly one line");
  const origin = lines[0] ?? "";
  const match = /^https:\/\/((?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z](?:[a-z0-9-]{0,61}[a-z0-9])?)$/.exec(origin);
  if (match === null || (match[1] ?? "").length > 253) {
    throw new InputError("the expected origin must be https:// and a lower-case host name, without a port, a path or a trailing slash");
  }
  return origin;
}

/** The workstation tool's session file: one `origin=` and one `cookie=` line. */
export function parseSession(text: string): { origin: string; cookie: string } {
  const origins: string[] = [];
  const cookies: string[] = [];
  for (const line of text.split("\n").map((l) => l.replace(/\r$/, ""))) {
    if (line.startsWith("origin=")) origins.push(line.slice("origin=".length));
    else if (line.startsWith("cookie=")) cookies.push(line.slice("cookie=".length));
    else if (line.trim() !== "") throw new InputError("the session file has a line that is neither origin= nor cookie=");
  }
  if (origins.length !== 1 || cookies.length !== 1) throw new InputError("the session file must have one origin= and one cookie= line");
  const cookie = cookies[0] ?? "";
  // A cookie value (RFC 6265 cookie-octet): nothing that could end the header or add another cookie.
  if (!/^[\x21\x23-\x2B\x2D-\x3A\x3C-\x5B\x5D-\x7E]+$/.test(cookie)) throw new InputError("the session file's cookie is not a cookie value");
  return { origin: origins[0] ?? "", cookie };
}

/** The Access headers file: `cf-access-client-id:` and `cf-access-client-secret:` once each. */
export function parseAccessHeaders(text: string): [string, string][] {
  const found = new Map<string, string>();
  const lines = text.split("\n");
  for (const [index, raw] of lines.entries()) {
    const line = raw.replace(/\r$/, "");
    if (line.trim() === "" || line.trimStart().startsWith("#")) continue;
    const colon = line.indexOf(":");
    const name = colon < 0 ? "" : line.slice(0, colon).trim().toLowerCase();
    const value = colon < 0 ? "" : line.slice(colon + 1).trim();
    if (name !== "cf-access-client-id" && name !== "cf-access-client-secret") {
      throw new InputError(`the Access headers file, line ${index + 1}: only cf-access-client-id and cf-access-client-secret are accepted`);
    }
    if (found.has(name)) throw new InputError(`the Access headers file, line ${index + 1}: ${name} appears twice`);
    if (!/^[\x21-\x7E]+$/.test(value)) throw new InputError(`the Access headers file, line ${index + 1}: the value is empty or has characters a header cannot carry`);
    found.set(name, value);
  }
  const id = found.get("cf-access-client-id");
  const secret = found.get("cf-access-client-secret");
  if (id === undefined || secret === undefined) throw new InputError("the Access headers file needs both cf-access-client-id and cf-access-client-secret");
  return [
    ["cf-access-client-id", id],
    ["cf-access-client-secret", secret],
  ];
}

/**
 * The permission bits a secret file must not have: owner execute and every group and other bit.
 * So 0600 and narrower (0400, 0200) pass, and 0700, 0640 or 0604 are refused. The workstation tool
 * applies the same rule to the session file it writes (SECRET_FORBIDDEN in
 * factory/workstation/src/files.rs).
 */
export const SECRET_FORBIDDEN_BITS = 0o177;

/** Refuses a secret file that is not a regular file of this user, or whose mode is wider than 0600. */
export function checkPrivateFile(path: string, what: string): void {
  let stat: ReturnType<typeof lstatSync>;
  try {
    stat = lstatSync(path);
  } catch {
    throw new InputError(`${what} ${shownPath(path)} cannot be read`);
  }
  if (!stat.isFile()) throw new InputError(`${what} ${shownPath(path)} is not a regular file (a symbolic link is refused too)`);
  const uid = process.getuid?.();
  if (uid !== undefined && stat.uid !== uid) throw new InputError(`${what} ${shownPath(path)} belongs to another user`);
  if ((stat.mode & SECRET_FORBIDDEN_BITS) !== 0) {
    throw new InputError(`${what} ${shownPath(path)} has mode ${(stat.mode & 0o777).toString(8)}; it must be 0600 or narrower (chmod 600 <file>)`);
  }
}

/** What the checks need: the origin, the session cookie and the Access headers. */
export interface Inputs {
  origin: string;
  cookie: string;
  accessHeaders: [string, string][];
}

export interface InputPaths {
  expectedOrigin: string;
  session: string;
  accessHeaders: string;
}

function readText(path: string, what: string): string {
  try {
    return readFileSync(path, "utf8");
  } catch {
    throw new InputError(`${what} ${shownPath(path)} cannot be read`);
  }
}

/** Reads and checks the three files; the session must belong to the expected origin. */
export function loadInputs(paths: InputPaths): Inputs {
  const origin = parseExpectedOrigin(readText(paths.expectedOrigin, "the expected origin file"));
  checkPrivateFile(paths.session, "the session file");
  checkPrivateFile(paths.accessHeaders, "the Access headers file");
  const session = parseSession(readText(paths.session, "the session file"));
  if (session.origin !== origin) {
    throw new InputError("refused: the session belongs to another platform than the expected origin (log in with the workstation tool to the demo app first)");
  }
  const accessHeaders = parseAccessHeaders(readText(paths.accessHeaders, "the Access headers file"));
  return { origin, cookie: session.cookie, accessHeaders };
}

/** Parses the command line: three file options, no values echoed back. */
export function parseArgs(argv: readonly string[], env: Record<string, string | undefined>): InputPaths {
  const names = ["expected-origin-file", "session-file", "access-headers-file"] as const;
  const values: Partial<Record<(typeof names)[number], string>> = {};
  for (let index = 0; index < argv.length; index += 2) {
    const name = (argv[index] ?? "").replace(/^--/, "") as (typeof names)[number];
    if (!(argv[index] ?? "").startsWith("--") || !names.includes(name)) {
      throw new InputError(`unknown argument at position ${index + 1}; options: --${names.join(", --")} (each takes a file)`);
    }
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) throw new InputError(`--${name} needs a file`);
    if (values[name] !== undefined) throw new InputError(`--${name} is given twice`);
    values[name] = value;
  }
  if (values["expected-origin-file"] === undefined) throw new InputError("--expected-origin-file is required");
  const home = env.HOME || homedir();
  const runtime = env.XDG_RUNTIME_DIR;
  const session = values["session-file"] ?? (runtime ? join(runtime, "trustiot-workstation", "session") : undefined);
  if (session === undefined) throw new InputError("XDG_RUNTIME_DIR is not set; give --session-file");
  return {
    expectedOrigin: values["expected-origin-file"],
    session,
    accessHeaders: values["access-headers-file"] ?? join(home, ".config", "trustiot-workstation", "access-headers"),
  };
}

// ---------------------------------------------------------------------------------------------
// Sending

/**
 * A Send to `baseUrl` (the origin itself, or a test server) with the Access headers on every
 * request. No redirect is followed and no proxy is used. A failure to connect or a timeout becomes
 * an answer with status 0 and a fixed description, never the error's text (which can name the URL).
 */
export function makeSend(
  baseUrl: string,
  accessHeaders: [string, string][],
  timeoutMs = REQUEST_TIMEOUT_MS,
  maxBodyBytes = MAX_BODY_BYTES,
): Send {
  return async (method, path, headers) => {
    const all = new Headers();
    // A fixed agent: Cloudflare's Browser Integrity Check refuses requests without one.
    all.set("user-agent", "trustiot-demo-web-check");
    for (const [name, value] of accessHeaders) all.set(name, value);
    for (const [name, value] of headers) all.set(name, value);
    try {
      const response = await fetch(`${baseUrl}${path}`, {
        method,
        headers: all,
        redirect: "manual",
        signal: AbortSignal.timeout(timeoutMs),
        ...(method === "POST" ? { body: "" } : {}),
      });
      const body = new TextDecoder().decode(await readLimited(response.body, maxBodyBytes));
      return { status: response.status, headers: response.headers, body };
    } catch (error) {
      const name = error instanceof Error && /^[A-Za-z]+$/.test(error.name) ? error.name : "Error";
      const timedOut = name === "TimeoutError" || name === "AbortError";
      return { status: 0, headers: new Headers(), body: timedOut ? `no answer within ${timeoutMs / 1000} s` : `the request failed (${name})` };
    }
  };
}

/**
 * At most `limit` bytes of a body. Reading stops and the stream is cancelled as soon as the limit
 * is reached, so an endless or huge answer never sits in memory whole.
 */
export async function readLimited(stream: ReadableStream<Uint8Array> | null, limit: number): Promise<Uint8Array> {
  if (stream === null) return new Uint8Array();
  const reader = stream.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (size < limit) {
      const { done, value } = await reader.read();
      if (done) break;
      const part = value.length > limit - size ? value.subarray(0, limit - size) : value;
      chunks.push(part);
      size += part.length;
    }
  } finally {
    // Ends the transfer when the limit stopped the loop; a no-op on a finished stream.
    await reader.cancel().catch(() => {});
  }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.length;
  }
  return bytes;
}

// ---------------------------------------------------------------------------------------------
// Reading answers

/** The `code` and `message_key` of an M1 ErrorBody, when the body is one. */
export function errorFields(body: string): { code?: string; key?: string } {
  try {
    const value = JSON.parse(body) as unknown;
    if (value === null || typeof value !== "object" || Array.isArray(value)) return {};
    const record = value as Record<string, unknown>;
    const pick = (field: string): string | undefined => {
      const v = record[field];
      return typeof v === "string" && /^[a-z0-9_.]{1,80}$/.test(v) ? v : undefined;
    };
    return { code: pick("code"), key: pick("message_key") };
  } catch {
    return {};
  }
}

/** The media type of an answer, without parameters, when it is a plain one. */
function mediaType(answer: Answer): string | undefined {
  const type = (answer.headers.get("content-type") ?? "").split(";")[0]?.trim().toLowerCase() ?? "";
  return /^[a-z0-9.+-]+\/[a-z0-9.+-]+$/.test(type) ? type : undefined;
}

/**
 * Whether the web service sent the answer: only it sets the SPA's Content-Security-Policy and
 * X-Frame-Options (services/web/src/headers.rs); the API services and the ingress do not.
 */
export function fromWeb(answer: Answer): boolean {
  return (answer.headers.get("content-security-policy") ?? "").startsWith("default-src 'none'") && answer.headers.get("x-frame-options") === "DENY";
}

/** Whether the answer is the SPA's index.html from web. */
function isIndex(answer: Answer): boolean {
  return answer.status === 200 && fromWeb(answer) && mediaType(answer) === "text/html" && answer.body.includes('<div id="root"></div>');
}

/** What an answer was, in words that are safe to print: no URL, header value or body. */
export function describe(answer: Answer): string {
  if (answer.status === 0) return answer.body;
  const location = answer.headers.get("location") ?? "";
  let accessLogin = false;
  try {
    accessLogin = location !== "" && new URL(location).hostname.endsWith(".cloudflareaccess.com");
  } catch {
    accessLogin = false;
  }
  if (answer.status >= 300 && answer.status < 400 && accessLogin) return `${answer.status} to the Cloudflare Access login (the service token did not pass)`;
  if (answer.status >= 300 && answer.status < 400) return `${answer.status} a redirect elsewhere`;
  if ((answer.headers.get("cf-mitigated") ?? "").toLowerCase() === "challenge") return `${answer.status} a Cloudflare challenge`;
  const { code, key } = errorFields(answer.body);
  if (code !== undefined) return `${answer.status} ${code}${key === undefined ? "" : ` ${key}`}`;
  if (fromWeb(answer)) return `${answer.status} from web (${mediaType(answer) ?? "no content-type"})`;
  if ((answer.headers.get("server") ?? "").toLowerCase() === "cloudflare") return `${answer.status} from Cloudflare (${mediaType(answer) ?? "no content-type"})`;
  return `${answer.status} ${mediaType(answer) ?? "no content-type"}`;
}

// ---------------------------------------------------------------------------------------------
// The checks

/** One /static/ asset that index.html loads, or undefined. */
export function staticAsset(index: string): string | undefined {
  return /(?:src|href)="(\/static\/[A-Za-z0-9_-]{1,64}\.(?:js|css))"/.exec(index)?.[1];
}

/**
 * Runs every check once, in order, and returns their outcomes. `origin` is the app's origin as a
 * browser of the app would send it; `cookie` the platform administrator's session.
 */
export async function runChecks(send: Send, origin: string, cookie: string): Promise<Outcome[]> {
  const outcomes: Outcome[] = [];
  const record = (name: string, answer: Answer, pass: boolean, expected: string): void => {
    outcomes.push({ name, pass, seen: describe(answer), expected });
  };
  const session: [string, string] = ["cookie", `${SESSION_COOKIE}=${cookie}`];
  const crossSiteRefusal = (answer: Answer): boolean => {
    const { code, key } = errorFields(answer.body);
    return answer.status === 403 && code === "permission_denied" && key === CROSS_SITE_REQUEST;
  };

  // --- Routing on the app host (AC 2): web answers everything but /api and /auth.
  const root = await send("GET", "/", []);
  record("routing: GET / without a cookie", root, isIndex(root), "200 index.html from web");

  const deep = await send("GET", DEEP_LINK, []);
  const sameIndex = isIndex(deep) && isIndex(root) && deep.body === root.body;
  record(`routing: GET ${DEEP_LINK} (a deep link) without a cookie`, deep, sameIndex, "200 from web, the same index.html as /");

  const withCookie = await send("GET", "/", [session]);
  record("routing: GET / with the session cookie", withCookie, isIndex(withCookie) && withCookie.body === root.body, "200 from web, the same index.html");

  const asset = isIndex(root) ? staticAsset(root.body) : undefined;
  if (asset === undefined) {
    outcomes.push({ name: "routing: GET /static/* (an asset index.html loads)", pass: false, seen: "index.html from web names no /static/ asset", expected: "200 from web, immutable" });
  } else {
    const file = await send("GET", asset, []);
    const type = mediaType(file);
    const pass = file.status === 200 && fromWeb(file) && (type === "text/javascript" || type === "text/css") && (file.headers.get("cache-control") ?? "").includes("immutable");
    record("routing: GET /static/* (an asset index.html loads) without a cookie", file, pass, "200 from web, text/javascript or text/css, immutable");
  }

  const api = await send("GET", POOL_PATH, []);
  const apiFields = errorFields(api.body);
  record(`routing: GET ${POOL_PATH} without a cookie`, api, api.status === 401 && apiFields.code === "unauthenticated" && !fromWeb(api), "401 unauthenticated from the cookie check");

  const authGet = await send("GET", LOGIN_PATH, []);
  const authGetFields = errorFields(authGet.body);
  record(`routing: GET ${LOGIN_PATH} without a cookie (only POST is cookie-free)`, authGet, authGet.status === 401 && authGetFields.code === "unauthenticated" && !fromWeb(authGet), "401 unauthenticated from the cookie check");

  const login = await send("POST", LOGIN_PATH, []);
  const loginFields = errorFields(login.body);
  record(`routing: POST ${LOGIN_PATH} without a cookie or a body`, login, login.status === 400 && loginFields.code === "invalid_request" && !fromWeb(login), "400 invalid_request from the identity service's login (it reads nothing)");

  // --- Cross-site requests with the session cookie (AC 3), on the GET-only device pool.
  const control = await send("GET", POOL_PATH, [session, ["sec-fetch-site", "same-origin"]]);
  let listed = false;
  try {
    listed = Array.isArray((JSON.parse(control.body) as { gateways?: unknown }).gateways);
  } catch {
    listed = false;
  }
  record(
    `cross-site: GET ${POOL_PATH}, the cookie, Sec-Fetch-Site: same-origin`,
    control,
    control.status === 200 && listed,
    "200 with the device pool (a platform administrator's session; without it the refusals below prove nothing)",
  );

  const crossGet = await send("GET", POOL_PATH, [session, ["sec-fetch-site", "cross-site"]]);
  record(`cross-site: GET ${POOL_PATH}, the cookie, Sec-Fetch-Site: cross-site`, crossGet, crossSiteRefusal(crossGet), `403 permission_denied ${CROSS_SITE_REQUEST}`);

  const crossPost = await send("POST", POOL_PATH, [session, ["sec-fetch-site", "cross-site"]]);
  record(`cross-site: POST ${POOL_PATH}, the cookie, Sec-Fetch-Site: cross-site`, crossPost, crossSiteRefusal(crossPost), `403 permission_denied ${CROSS_SITE_REQUEST}`);

  const foreign = await send("POST", POOL_PATH, [session, ["origin", FOREIGN_ORIGIN]]);
  record(`cross-site: POST ${POOL_PATH}, the cookie, a foreign Origin, no Sec-Fetch-Site`, foreign, crossSiteRefusal(foreign), `403 permission_denied ${CROSS_SITE_REQUEST}`);

  const ownBoth = await send("POST", POOL_PATH, [session, ["origin", origin], ["sec-fetch-site", "same-origin"]]);
  record(
    `cross-site: POST ${POOL_PATH}, the cookie, the app's Origin, Sec-Fetch-Site: same-origin`,
    ownBoth,
    ownBoth.status === 405 && !fromWeb(ownBoth),
    "405 from the assets service (past the cookie check; GET only there, nothing changes)",
  );

  const ownOrigin = await send("POST", POOL_PATH, [session, ["origin", origin]]);
  record(
    `cross-site: POST ${POOL_PATH}, the cookie, the app's Origin, no Sec-Fetch-Site`,
    ownOrigin,
    ownOrigin.status === 405 && !fromWeb(ownOrigin),
    "405 from the assets service (past the cookie check; GET only there, nothing changes)",
  );

  return outcomes;
}

/** The lines to print: one per check, then the summary. */
export function report(outcomes: Outcome[]): string[] {
  const lines = outcomes.map((o) => (o.pass ? `PASS ${o.name}: ${o.seen}` : `FAIL ${o.name}: ${o.seen}; expected ${o.expected}`));
  const failed = outcomes.filter((o) => !o.pass).length;
  lines.push(failed === 0 ? `PASS all ${outcomes.length} checks` : `FAIL ${failed} of ${outcomes.length} checks`);
  return lines;
}

// ---------------------------------------------------------------------------------------------
// istiod

/** The ext_authz provider the chart's cookie check names (deploy/istio/istiod-values.yaml). */
export const PROVIDER = "trustiot-identity";
/** The provider's settings, in the order they are printed; the two header lists compare as sets. */
const PROVIDER_SETTINGS = ["service", "port", "pathPrefix", "failOpen", "timeout", "statusOnError"] as const;
const PROVIDER_LISTS = ["includeRequestHeadersInCheck", "headersToUpstreamOnAllow"] as const;

type Mesh = { pathNormalization?: unknown; extensionProviders?: unknown };

/** A value as it may be printed: plain scalars only, anything else as a placeholder. */
function shown(value: unknown): string {
  if (value === undefined) return "(not set)";
  const text = typeof value === "string" || typeof value === "number" || typeof value === "boolean" ? String(value) : "";
  return /^[A-Za-z0-9._:/-]{1,120}$/.test(text) ? text : "(a value that is not a plain scalar)";
}

/** The provider's envoyExtAuthzHttp settings, or undefined when the mesh has not exactly one. */
function providerOf(mesh: Mesh): Record<string, unknown> | undefined {
  const providers = Array.isArray(mesh.extensionProviders) ? (mesh.extensionProviders as unknown[]) : [];
  const named = providers.filter((p) => p !== null && typeof p === "object" && (p as { name?: unknown }).name === PROVIDER);
  if (named.length !== 1) return undefined;
  const http = (named[0] as { envoyExtAuthzHttp?: unknown }).envoyExtAuthzHttp;
  return http !== null && typeof http === "object" && !Array.isArray(http) ? (http as Record<string, unknown>) : undefined;
}

const listOf = (value: unknown): string[] | undefined =>
  Array.isArray(value) && value.every((v) => typeof v === "string") ? [...(value as string[])].map((v) => v.toLowerCase()).sort() : undefined;

/**
 * Compares the live mesh config (the istio ConfigMap's `mesh` in istio-system) with the repo's
 * deploy/istio/istiod-values.yaml (`meshConfig`): the path normalization and the ext_authz
 * provider's service, port, path, failure behaviour and both header lists. It prints setting
 * names and their values only (cluster service names, ports, header names).
 */
export function compareIstiod(liveMesh: string, repoValues: string): { pass: boolean; lines: string[] } {
  let live: Mesh;
  try {
    live = (Bun.YAML.parse(liveMesh) ?? {}) as Mesh;
  } catch {
    return { pass: false, lines: ["FAIL the live mesh config is not YAML"] };
  }
  const repo = ((Bun.YAML.parse(repoValues) as { meshConfig?: Mesh } | null)?.meshConfig ?? {}) as Mesh;
  const lines: string[] = [];
  const differences: string[] = [];
  const normalization = (m: Mesh) => (m.pathNormalization as { normalization?: unknown } | undefined)?.normalization;
  lines.push(`pathNormalization.normalization: ${shown(normalization(live))} (repo: ${shown(normalization(repo))})`);
  if (normalization(live) !== normalization(repo)) differences.push("pathNormalization.normalization");

  const liveProvider = providerOf(live);
  const repoProvider = providerOf(repo);
  if (repoProvider === undefined) return { pass: false, lines: [...lines, `FAIL deploy/istio/istiod-values.yaml has no single provider ${PROVIDER}`] };
  if (liveProvider === undefined) {
    lines.push(`extensionProviders: no single envoyExtAuthzHttp provider named ${PROVIDER}`);
    differences.push(`extensionProviders.${PROVIDER}`);
  } else {
    for (const key of PROVIDER_SETTINGS) {
      const same = liveProvider[key] === repoProvider[key];
      lines.push(`${PROVIDER}.${key}: ${shown(liveProvider[key])}${same ? "" : ` (repo: ${shown(repoProvider[key])})`}`);
      if (!same) differences.push(`${PROVIDER}.${key}`);
    }
    for (const key of PROVIDER_LISTS) {
      const l = listOf(liveProvider[key]);
      const r = listOf(repoProvider[key]);
      const same = l !== undefined && r !== undefined && l.join(",") === r.join(",");
      const text = (v: string[] | undefined) => (v === undefined ? "(not a list of names)" : v.map(shown).join(", "));
      lines.push(`${PROVIDER}.${key}: ${text(l)}${same ? "" : ` (repo: ${text(r)})`}`);
      if (!same) differences.push(`${PROVIDER}.${key}`);
    }
    const known = new Set<string>([...PROVIDER_SETTINGS, ...PROVIDER_LISTS]);
    const extra = [...new Set([...Object.keys(liveProvider), ...Object.keys(repoProvider)])].filter((k) => !known.has(k)).sort();
    for (const key of extra) {
      if (JSON.stringify(liveProvider[key]) !== JSON.stringify(repoProvider[key])) differences.push(`${PROVIDER}.${key}`);
    }
  }
  lines.push(differences.length === 0 ? "PASS istiod's mesh config matches deploy/istio/istiod-values.yaml" : `FAIL istiod differs from deploy/istio/istiod-values.yaml in: ${differences.join(", ")}`);
  return { pass: differences.length === 0, lines };
}

export interface RunIo {
  out: (line: string) => void;
  err: (line: string) => void;
  /** Where requests go instead of the origin: only for tests, never a command-line option. */
  baseUrl?: string;
  /** Standard input (the `istiod` mode's mesh config). */
  stdin?: () => Promise<string>;
  /** deploy/istio/istiod-values.yaml's path; tests point it elsewhere. */
  istiodValues?: string;
}

/**
 * The whole run: 0 when every check passes, 1 when one fails, 2 when the input is refused.
 * `istiod` as the only argument compares the mesh config on stdin with the repo instead.
 */
export async function run(argv: readonly string[], env: Record<string, string | undefined>, io: RunIo): Promise<number> {
  if (argv[0] === "istiod") {
    if (argv.length !== 1 || io.stdin === undefined) {
      io.err("refused: istiod takes no option; give the istio ConfigMap's mesh config on stdin");
      return 2;
    }
    const result = compareIstiod(await io.stdin(), readFileSync(io.istiodValues ?? join(ROOT, "deploy", "istio", "istiod-values.yaml"), "utf8"));
    for (const line of result.lines) io.out(line);
    return result.pass ? 0 : 1;
  }
  let inputs: Inputs;
  try {
    inputs = loadInputs(parseArgs(argv, env));
  } catch (error) {
    if (error instanceof InputError) {
      io.err(`refused: ${error.message.replace(/^refused: /, "")}`);
      return 2;
    }
    throw error;
  }
  const send = makeSend(io.baseUrl ?? inputs.origin, inputs.accessHeaders);
  const outcomes = await runChecks(send, inputs.origin, inputs.cookie);
  for (const line of report(outcomes)) io.out(line);
  return outcomes.every((o) => o.pass) ? 0 : 1;
}

if (import.meta.main) {
  const code = await run(process.argv.slice(2), process.env, { out: (line) => console.log(line), err: (line) => console.error(line), stdin: () => Bun.stdin.text() });
  process.exit(code);
}
