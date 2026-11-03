// The conditions of the ingress e2e test's SPA and cross-site cases (ingress-answers.ts,
// Laysi/trustiot-v2#943), without a cluster: each one accepts the answer a correct ingress gives
// and names what is wrong with the answer of each broken ingress the case is meant to catch. The
// answers are built the way traefik/whoami and the identity double (identity-double/server.ts)
// write them.
import { describe, expect, test } from "bun:test";
import {
  type CheckHeaders,
  CROSS_SITE_KEY,
  crossSiteRefusalProblems,
  doubleClaims,
  sameOriginPassProblems,
  staticFileProblems,
} from "./ingress-answers.ts";

const PROTECTED = "Name: protected-backend";
const SPA = "Name: spa-static";
const COOKIE = "sid=valid-0123456789abcdef";
const FORGED = "forged.4b1d";

/** A whoami answer: its name, the request line, and one line per header. */
function whoami(name: string, requestLine: string, headers: [string, string][] = []): { status: number; body: string } {
  const lines = [name, "Hostname: whoami-0", "IP: 127.0.0.1", "RemoteAddr: 10.42.0.7:41234", requestLine, "Host: app.saas.trustiot.test", "User-Agent: trustiot-ingress-e2e"];
  for (const [key, value] of headers) lines.push(`${key}: ${value}`);
  return { status: 200, body: `${lines.join("\r\n")}\r\n` };
}

/** The identity header the double returns for `path` with `received`. */
function identity(path: string, received: unknown): [string, string] {
  const claims = { sub: "user-e2e", tenant: "tenant-e2e", path, host: "app.saas.trustiot.test", received };
  return ["X-Internal-Identity", `double.${Buffer.from(JSON.stringify(claims)).toString("base64url")}`];
}

function refusal(received: unknown, extra: Record<string, unknown> = {}): { status: number; body: string } {
  return { status: 403, body: JSON.stringify({ code: "permission_denied", message_key: CROSS_SITE_KEY, received, ...extra }) };
}

const CROSS: CheckHeaders = { origin: null, "sec-fetch-site": "cross-site" };
const FOREIGN: CheckHeaders = { origin: "https://attacker.example.test", "sec-fetch-site": null };
const SAME: CheckHeaders = { origin: "https://app.saas.trustiot.test", "sec-fetch-site": "same-origin" };
const NONE: CheckHeaders = { origin: null, "sec-fetch-site": null };

describe("a cross-site request's answer", () => {
  test("passes when it is the check's refusal and the check received the headers as sent", () => {
    expect(crossSiteRefusalProblems(refusal(CROSS), CROSS, PROTECTED)).toEqual([]);
    expect(crossSiteRefusalProblems(refusal(FOREIGN), FOREIGN, PROTECTED)).toEqual([]);
  });

  test("fails when the provider does not pass the headers: the check lets the request through", () => {
    // What the double answers without Origin and Sec-Fetch-Site: an identity, so the backend answers.
    const through = whoami(PROTECTED, "POST /api/whoami HTTP/1.1", [identity("/api/whoami", NONE), ["Cookie", COOKIE]]);
    const problems = crossSiteRefusalProblems(through, CROSS, PROTECTED);
    expect(problems).toContain("expected status 403 from the cookie check, got 200");
    expect(problems).toContain(`the backend behind the cookie route answered (${PROTECTED})`);
    expect(problems).toContain(`expected the check's JSON refusal, got ${JSON.stringify(PROTECTED)}`);
    // The failure names whoami's first line only, never the cookie it echoed (CI logs are public).
    expect(problems.join("\n")).not.toContain(COOKIE);
  });

  test("fails on a 403 that is not the check's cross-site refusal", () => {
    // Istio's own refusal (a DENY policy), the session check's, and another message key.
    expect(crossSiteRefusalProblems({ status: 403, body: "RBAC: access denied" }, CROSS, PROTECTED)).toEqual([
      'expected the check\'s JSON refusal, got "RBAC: access denied"',
    ]);
    expect(crossSiteRefusalProblems({ status: 403, body: JSON.stringify({ code: "permission_denied" }) }, CROSS, PROTECTED)).toEqual([
      `expected message_key "${CROSS_SITE_KEY}", got undefined`,
      `expected the check to receive ${JSON.stringify(CROSS)}, it received nothing`,
    ]);
    expect(crossSiteRefusalProblems({ status: 401, body: JSON.stringify({ code: "unauthenticated" }) }, CROSS, PROTECTED)).toContain(
      "expected status 403 from the cookie check, got 401",
    );
  });

  test("fails when the check received other values than the client sent, or more headers", () => {
    // Refused for Origin, but Sec-Fetch-Site was dropped on the way (or the reverse).
    expect(crossSiteRefusalProblems(refusal({ origin: "https://attacker.example.test", "sec-fetch-site": null }), { ...FOREIGN, "sec-fetch-site": "cross-site" }, PROTECTED)).toHaveLength(1);
    expect(crossSiteRefusalProblems(refusal(NONE), FOREIGN, PROTECTED)).toEqual([
      `expected the check to receive ${JSON.stringify(FOREIGN)}, it received ${JSON.stringify(NONE)}`,
    ]);
    expect(crossSiteRefusalProblems(refusal({ ...CROSS, cookie: COOKIE }), CROSS, PROTECTED)).toEqual([
      `expected the check to receive ${JSON.stringify(CROSS)}, it received ${JSON.stringify(CROSS)} and the other headers ["cookie"]`,
    ]);
  });
});

describe("a same-origin request's answer", () => {
  test("passes when the backend got the double's one identity and the check received the headers as sent", () => {
    const answer = whoami(PROTECTED, "POST /api/whoami HTTP/1.1", [identity("/api/whoami", SAME)]);
    expect(sameOriginPassProblems(answer, SAME, PROTECTED, "POST /api/whoami HTTP/1.1", "/api/whoami")).toEqual([]);
    expect(doubleClaims(answer.body).received).toEqual(SAME);
  });

  test("fails when the provider does not pass the headers, even though the request got through", () => {
    const answer = whoami(PROTECTED, "POST /api/whoami HTTP/1.1", [identity("/api/whoami", NONE)]);
    expect(sameOriginPassProblems(answer, SAME, PROTECTED, "POST /api/whoami HTTP/1.1", "/api/whoami")).toEqual([
      `expected the check to receive ${JSON.stringify(SAME)}, it received ${JSON.stringify(NONE)}`,
    ]);
    // A double from before the cross-site check, which lists no headers at all.
    const old = whoami(PROTECTED, "POST /api/whoami HTTP/1.1", [identity("/api/whoami", undefined)]);
    expect(sameOriginPassProblems(old, SAME, PROTECTED, "POST /api/whoami HTTP/1.1", "/api/whoami")).toHaveLength(1);
  });

  test("fails when a same-origin request is refused, or the backend gets no, two or a foreign identity", () => {
    expect(sameOriginPassProblems(refusal(SAME), SAME, PROTECTED, "POST /api/whoami HTTP/1.1", "/api/whoami")).toContain(
      "expected exactly one x-internal-identity at the backend, got 0",
    );
    const two = whoami(PROTECTED, "POST /api/whoami HTTP/1.1", [identity("/api/whoami", SAME), ["X-Internal-Identity", FORGED]]);
    expect(sameOriginPassProblems(two, SAME, PROTECTED, "POST /api/whoami HTTP/1.1", "/api/whoami")).toEqual(["expected exactly one x-internal-identity at the backend, got 2"]);
    const forged = whoami(PROTECTED, "POST /api/whoami HTTP/1.1", [["X-Internal-Identity", FORGED]]);
    expect(sameOriginPassProblems(forged, SAME, PROTECTED, "POST /api/whoami HTTP/1.1", "/api/whoami")).toEqual(["expected the identity header to be the double's"]);
  });

  test("fails when web answers, or the check or the backend saw another path", () => {
    const web = whoami(SPA, "GET /%61pi/x HTTP/1.1");
    const problems = sameOriginPassProblems(web, NONE, PROTECTED, "GET /api/x HTTP/1.1", "/api/x");
    expect(problems).toContain(`expected the backend behind the cookie route (${PROTECTED}) to answer`);
    expect(problems).toContain('expected the backend to get "GET /api/x HTTP/1.1"');
    const otherPath = whoami(PROTECTED, "GET /api/x HTTP/1.1", [identity("/%61pi/x", NONE)]);
    expect(sameOriginPassProblems(otherPath, NONE, PROTECTED, "GET /api/x HTTP/1.1", "/api/x")).toEqual([
      'expected the check to be asked about /api/x, it was asked about "/%61pi/x"',
    ]);
  });
});

describe("the SPA's static files' answer", () => {
  const LINE = "GET /devices/4f1c2a/settings HTTP/1.1";

  test("passes when web answered without the session, an identity or the client's values", () => {
    const answer = whoami(SPA, LINE, [["X-Forwarded-Proto", "https"], ["Accept", "*/*"]]);
    expect(staticFileProblems(answer, SPA, PROTECTED, LINE, [COOKIE, FORGED])).toEqual([]);
  });

  test("fails when the route keeps the cookie", () => {
    const answer = whoami(SPA, LINE, [["Cookie", `theme=dark; ${COOKIE}`]]);
    expect(staticFileProblems(answer, SPA, PROTECTED, LINE, [COOKIE, FORGED])).toEqual([
      "expected web to get no cookie header, it got 1",
      "web echoed value #1 that the client sent (a cookie or a forged header)",
    ]);
  });

  test("fails when the cookie check ran (its identity reaches web) or a forged identity header in any spelling gets through", () => {
    const checked = whoami(SPA, LINE, [identity("/devices/4f1c2a/settings", NONE)]);
    expect(staticFileProblems(checked, SPA, PROTECTED, LINE, [COOKIE, FORGED])).toEqual(["expected web to get no x-internal-identity, it got 1"]);
    for (const name of ["X-Internal-Identity", "X_internal_identity", "X-Internal_identity", "X_internal-Identity"]) {
      const forged = whoami(SPA, LINE, [[name, FORGED]]);
      expect(staticFileProblems(forged, SPA, PROTECTED, LINE, [COOKIE, FORGED])).toEqual([
        `expected web to get no ${name.toLowerCase()}, it got 1`,
        "web echoed value #2 that the client sent (a cookie or a forged header)",
      ]);
    }
  });

  test("fails when the cookie check refused the request or the cookie route's backend answered", () => {
    const refused = { status: 401, body: JSON.stringify({ code: "unauthenticated" }) };
    expect(staticFileProblems(refused, SPA, PROTECTED, LINE, [])).toEqual([
      // A JSON body is shown by its code and message key only.
      'expected status 200 from web, got 401 JSON {"code":"unauthenticated"}',
      `expected web (${SPA}) to answer`,
      `expected web to get "${LINE}"`,
    ]);
    const protectedAnswer = whoami(PROTECTED, LINE, [identity("/devices/4f1c2a/settings", NONE)]);
    expect(staticFileProblems(protectedAnswer, SPA, PROTECTED, LINE, [])).toEqual([
      `expected web (${SPA}) to answer`,
      `the backend behind the cookie route answered (${PROTECTED})`,
      "expected web to get no x-internal-identity, it got 1",
    ]);
  });

  test("fails when web got another path than the client sent", () => {
    const answer = whoami(SPA, "GET /api/x HTTP/1.1");
    expect(staticFileProblems(answer, SPA, PROTECTED, "GET /API/x HTTP/1.1", [])).toEqual(['expected web to get "GET /API/x HTTP/1.1"']);
  });
});

// CI logs are public, and the e2e sends a valid session cookie that whoami echoes back: no failure
// message may carry it, whatever a broken ingress answers (Laysi/trustiot-v2#943).
describe("failure messages never carry the cookie the request sent", () => {
  const LINE = "GET /api/x HTTP/1.1";
  const withCookie = (name: string, status: number, extra: [string, string][] = []) => ({
    ...whoami(name, LINE, [["Cookie", `theme=dark; ${COOKIE}`], ...extra]),
    status,
  });
  // A cookie-bearing answer from each broken ingress: the wrong backend, a server error, no, two
  // or a foreign identity, an identity whose claims list the cookie, a JSON body and a one-line one.
  const answers: [string, { status: number; body: string }][] = [
    ["web with the cookie", withCookie(SPA, 200)],
    ["a 500 echoing the cookie", withCookie(PROTECTED, 500)],
    ["the backend without an identity", withCookie(PROTECTED, 200)],
    ["the backend with two identities", withCookie(PROTECTED, 200, [identity("/api/x", NONE), identity("/api/x", NONE)])],
    ["the backend with a forged identity", withCookie(PROTECTED, 200, [["X-Internal-Identity", FORGED]])],
    ["claims that list the cookie", withCookie(PROTECTED, 200, [identity("/x", { ...NONE, cookie: COOKIE })])],
    ["a refusal that lists the cookie", refusal({ origin: null, "sec-fetch-site": "cross-site", cookie: COOKIE })],
    ["a refusal whose received is the cookie", refusal(COOKIE)],
    ["a one-line answer after the cookie", { status: 502, body: `upstream error\r\nCookie: ${COOKIE}\r\n` }],
  ];

  test.each(answers)("%s", (_name, answer) => {
    const messages = [
      ...staticFileProblems(answer, SPA, PROTECTED, LINE, [COOKIE, FORGED]),
      ...sameOriginPassProblems(answer, CROSS, PROTECTED, "GET /api/y HTTP/1.1", "/api/y"),
      ...crossSiteRefusalProblems(answer, NONE, PROTECTED),
    ];
    // Each broken answer fails at least one condition, so the messages are really built.
    expect(messages.length).toBeGreaterThan(0);
    expect({ cookie: messages.join("\n").includes(COOKIE) }).toEqual({ cookie: false });
  });

  test("doubleClaims names the answer's first line only when it finds no identity of the double's", () => {
    let message = "";
    try {
      doubleClaims(withCookie(PROTECTED, 200).body);
    } catch (error) {
      message = (error as Error).message;
    }
    expect(message).toBe(`no identity header from the double in the answer starting ${JSON.stringify(PROTECTED)}`);
  });
});
