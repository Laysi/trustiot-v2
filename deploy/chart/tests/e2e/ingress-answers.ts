// What the ingress e2e test (ingress.e2e.ts) reads from an answer, and the conditions it puts on
// the answers of the SPA and cross-site cases (Laysi/trustiot-v2#943). Kept apart from
// ingress.e2e.ts, which needs a cluster, so ingress-answers.test.ts can show that each condition
// fails on the answer a broken ingress would give.
//
// The backends are traefik/whoami, which prints one "Name: value" line per request header value
// (Go's canonical spelling, so names are compared without case), and the identity double
// (identity-double/server.ts), whose identity header carries base64url JSON claims.

/** An answer as the raw client returns it; only these two fields are read. */
export type Answer = { status: number; body: string };

/** How many header lines named `header` whoami printed. */
export function whoamiHeaderCount(body: string, header: string): number {
  return body.split("\n").filter((line) => line.toLowerCase().startsWith(`${header.toLowerCase()}:`)).length;
}

/** The value of the first header line named `header` that whoami printed. */
export function whoamiHeader(body: string, header: string): string | undefined {
  const line = body.split("\n").find((l) => l.toLowerCase().startsWith(`${header.toLowerCase()}:`));
  return line?.slice(line.indexOf(":") + 1).trim();
}

/**
 * The two headers that the mesh's ext_authz provider passes to the cookie check besides the
 * cookie (`includeRequestHeadersInCheck` in deploy/istio/istiod-values.yaml), as the identity
 * double received them: null when the check request had none.
 */
export type CheckHeaders = { origin: string | null; "sec-fetch-site": string | null };

/** The claims the identity double put in the identity header that whoami echoed. */
export type DoubleClaims = { host: unknown; path: unknown; received: unknown };

/**
 * What the cookie check was asked about: the identity double puts the Host, the path and the two
 * headers of the cross-site check that the check request carried into the claims of the identity
 * header it returns (identity-double/server.ts), and whoami echoes that header.
 */
export function doubleClaims(body: string): DoubleClaims {
  const header = whoamiHeader(body, "x-internal-identity");
  if (header === undefined || !header.startsWith("double.")) throw new Error(`no identity header from the double in:\n${body}`);
  const claims = JSON.parse(Buffer.from(header.slice("double.".length), "base64url").toString("utf8")) as Partial<DoubleClaims>;
  return { host: claims.host, path: claims.path, received: claims.received };
}

/** The message key of the identity service's 403 for a cross-site request (services/identity/src/csrf.rs). */
export const CROSS_SITE_KEY = "security.cross_site_request";

/** The spellings of the identity header that never reach a backend except from the cookie check. */
export const IDENTITY_HEADER_SPELLINGS = ["x-internal-identity", "x_internal_identity", "x-internal_identity", "x_internal-identity"] as const;

// The first line of a body, for a failure message: enough to tell a JSON refusal, Istio's own text
// or whoami's "Name:" line apart, without the header lines whoami echoes after it (the test's
// cookie among them), since CI logs are public.
function short(body: string): string {
  return JSON.stringify(body.split("\n", 1)[0]!.trimEnd().slice(0, 200));
}

function json(body: string): Record<string, unknown> | undefined {
  try {
    const parsed: unknown = JSON.parse(body);
    return typeof parsed === "object" && parsed !== null && !Array.isArray(parsed) ? (parsed as Record<string, unknown>) : undefined;
  } catch {
    return undefined;
  }
}

function sameHeaders(received: unknown, sent: CheckHeaders): boolean {
  if (typeof received !== "object" || received === null) return false;
  const value = received as Record<string, unknown>;
  return Object.keys(value).sort().join(",") === "origin,sec-fetch-site" && value.origin === sent.origin && value["sec-fetch-site"] === sent["sec-fetch-site"];
}

/**
 * What is wrong with the answer to a cross-site request on a cookie route, or nothing: it must be
 * the cookie check's own 403 (`permission_denied`, message key CROSS_SITE_KEY) passed through by
 * the gateway, the check must have received `sent` exactly (so the refusal came from those
 * headers), and the backend behind the route (`protectedMarker`) must not have answered.
 */
export function crossSiteRefusalProblems(answer: Answer, sent: CheckHeaders, protectedMarker: string): string[] {
  const problems: string[] = [];
  if (answer.status !== 403) problems.push(`expected status 403 from the cookie check, got ${answer.status}`);
  if (answer.body.includes(protectedMarker)) problems.push(`the backend behind the cookie route answered (${protectedMarker})`);
  const body = json(answer.body);
  if (body === undefined) {
    problems.push(`expected the check's JSON refusal, got ${short(answer.body)}`);
    return problems;
  }
  if (body.code !== "permission_denied") problems.push(`expected code "permission_denied", got ${JSON.stringify(body.code)}`);
  if (body.message_key !== CROSS_SITE_KEY) problems.push(`expected message_key "${CROSS_SITE_KEY}", got ${JSON.stringify(body.message_key)}`);
  if (!sameHeaders(body.received, sent)) {
    problems.push(`expected the check to receive ${JSON.stringify(sent)}, it received ${JSON.stringify(body.received)}`);
  }
  return problems;
}

/**
 * What is wrong with the answer to a same-origin request on a cookie route, or nothing: the
 * backend (`protectedMarker`) answered `requestLine`, it got exactly one identity header and it
 * is the double's, whose claims show that the check was asked about `path` and received `sent`
 * exactly.
 */
export function sameOriginPassProblems(answer: Answer, sent: CheckHeaders, protectedMarker: string, requestLine: string, path: string): string[] {
  const problems: string[] = [];
  if (answer.status !== 200) problems.push(`expected status 200 from the backend, got ${answer.status} ${short(answer.body)}`);
  if (!answer.body.includes(protectedMarker)) problems.push(`expected the backend behind the cookie route (${protectedMarker}) to answer`);
  if (!answer.body.includes(requestLine)) problems.push(`expected the backend to get "${requestLine}"`);
  const identities = whoamiHeaderCount(answer.body, "x-internal-identity");
  if (identities !== 1) {
    problems.push(`expected exactly one x-internal-identity at the backend, got ${identities}`);
    return problems;
  }
  let claims: DoubleClaims;
  try {
    claims = doubleClaims(answer.body);
  } catch {
    problems.push("expected the identity header to be the double's");
    return problems;
  }
  if (claims.path !== path) problems.push(`expected the check to be asked about ${path}, it was asked about ${JSON.stringify(claims.path)}`);
  if (!sameHeaders(claims.received, sent)) {
    problems.push(`expected the check to receive ${JSON.stringify(sent)}, it received ${JSON.stringify(claims.received)}`);
  }
  return problems;
}

/**
 * What is wrong with the answer of the SPA's static files (services.web), or nothing: web
 * (`spaMarker`) answered `requestLine`, not the cookie route's backend (`protectedMarker`); it got
 * no Cookie, no identity header in any spelling (so no cookie check put one there and no forged
 * one got through), and none of the `secrets` the client sent (its cookie and forged values) is
 * anywhere in what it echoed.
 */
export function staticFileProblems(answer: Answer, spaMarker: string, protectedMarker: string, requestLine: string, secrets: string[]): string[] {
  const problems: string[] = [];
  if (answer.status !== 200) problems.push(`expected status 200 from web, got ${answer.status} ${short(answer.body)}`);
  if (!answer.body.includes(spaMarker)) problems.push(`expected web (${spaMarker}) to answer`);
  if (answer.body.includes(protectedMarker)) problems.push(`the backend behind the cookie route answered (${protectedMarker})`);
  if (!answer.body.includes(requestLine)) problems.push(`expected web to get "${requestLine}"`);
  const cookies = whoamiHeaderCount(answer.body, "cookie");
  if (cookies !== 0) problems.push(`expected web to get no cookie header, it got ${cookies}`);
  for (const name of IDENTITY_HEADER_SPELLINGS) {
    const count = whoamiHeaderCount(answer.body, name);
    if (count !== 0) problems.push(`expected web to get no ${name}, it got ${count}`);
  }
  secrets.forEach((secret, index) => {
    if (answer.body.includes(secret)) problems.push(`web echoed value #${index + 1} that the client sent (a cookie or a forged header)`);
  });
  return problems;
}
