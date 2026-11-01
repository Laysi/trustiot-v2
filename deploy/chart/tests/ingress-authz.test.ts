// Unit tests of the route-list evaluation (ingress-authz.ts, Laysi/trustiot-v2#77): no helm, no
// cluster. chart.test.ts applies it to the rendered chart.
import { describe, expect, test } from "bun:test";

import { PROBE_METHODS, hostnameOf, istioMatches, policyApplies, routeFor, routeListProblems, sampleRequests } from "./ingress-authz.ts";
import type { Exception, PolicyRule, RouteRule } from "./ingress-authz.ts";
import { useProcessTestTimeouts } from "./timing.ts";

useProcessTestTimeouts();

const APP = "app.example.test";
const GW = "gw.example.test";
const route = (hostname: string, path: string, match: RouteRule["match"], backend: string, isPublic = false): RouteRule => ({
  hostname,
  path,
  match,
  backend,
  public: isPublic,
});
const get = (host: string, path: string) => ({ host, path, method: "GET" });

// The shape the chart renders (templates/ingress.yaml): every path but the exceptions, each
// exception on every host but its own, with or without a port, and each exception on its own host
// with a method it does not list (Laysi/trustiot-v2#859).
const EXCEPTIONS: Exception[] = [
  { hostname: APP, path: "/auth/login", methods: ["POST"] },
  { hostname: APP, path: "/healthz", methods: ["GET", "HEAD"] },
  { hostname: GW, path: "/mqtt", methods: ["GET"] },
];
// Each exception on its own host with another method: the cookie check's last rules, and the DENY
// policy's rules.
const METHOD_RULES: PolicyRule[] = [
  { to: [{ operation: { hosts: [APP, `${APP}:*`], paths: ["/auth/login"], notMethods: ["POST"] } }] },
  { to: [{ operation: { hosts: [APP, `${APP}:*`], paths: ["/healthz"], notMethods: ["GET", "HEAD"] } }] },
  { to: [{ operation: { hosts: [GW, `${GW}:*`], paths: ["/mqtt"], notMethods: ["GET"] } }] },
];
const CHART_RULES: PolicyRule[] = [
  { to: [{ operation: { notPaths: ["/auth/login", "/healthz", "/mqtt"] } }] },
  { to: [{ operation: { paths: ["/auth/login"], notHosts: [APP, `${APP}:*`] } }] },
  { to: [{ operation: { paths: ["/healthz"], notHosts: [APP, `${APP}:*`] } }] },
  { to: [{ operation: { paths: ["/mqtt"], notHosts: [GW, `${GW}:*`] } }] },
  ...METHOD_RULES,
];
const DENY_INTERNAL: PolicyRule[] = [{ to: [{ operation: { paths: ["/internal*"] } }] }];
const CHART_DENY: PolicyRule[] = [...DENY_INTERNAL, ...METHOD_RULES];
const ROUTES: RouteRule[] = [
  route(APP, "/auth/login", "Exact", "identity", true),
  route(APP, "/healthz", "Exact", "identity", true),
  route(GW, "/mqtt", "Exact", "mqtt-bridge", true),
  route(APP, "/", "PathPrefix", "assets"),
  route(APP, "/auth", "PathPrefix", "identity"),
  route(GW, "/", "PathPrefix", "certs"),
  route(GW, "/healthz", "Exact", "certs"),
];
const PORTS = [80, 443, 8443];

describe("Istio's matching", () => {
  test("exact, any, prefix and suffix values; hosts without case", () => {
    expect(istioMatches("/healthz", "/healthz")).toBe(true);
    expect(istioMatches("/healthz", "/healthz/")).toBe(false);
    expect(istioMatches("*", "/anything")).toBe(true);
    expect(istioMatches("/internal*", "/internalx")).toBe(true);
    expect(istioMatches("/internal*", "/api/internal")).toBe(false);
    expect(istioMatches("*.example.test", "app.example.test")).toBe(true);
    expect(istioMatches(`${APP}:*`, `${APP}:443`)).toBe(true);
    expect(istioMatches(`${APP}:*`, APP)).toBe(false);
    expect(istioMatches(APP, "APP.Example.TEST", true)).toBe(true);
    expect(istioMatches(APP, "APP.Example.TEST")).toBe(false);
  });

  test("a rule without operations matches everything; a policy without rules matches nothing", () => {
    expect(policyApplies([{}], get(APP, "/healthz"))).toBe(true);
    expect(policyApplies([], get(APP, "/api"))).toBe(false);
  });

  test("an operation matches when every field it sets does", () => {
    const rules: PolicyRule[] = [{ to: [{ operation: { paths: ["/healthz"], notHosts: [APP, `${APP}:*`] } }] }];
    expect(policyApplies(rules, get(GW, "/healthz"))).toBe(true);
    expect(policyApplies(rules, get(`${GW}:443`, "/healthz"))).toBe(true);
    expect(policyApplies(rules, get(APP, "/healthz"))).toBe(false);
    expect(policyApplies(rules, get(`${APP}:8443`, "/healthz"))).toBe(false);
    expect(policyApplies(rules, get(GW, "/api"))).toBe(false);
  });

  test("paths and methods in one operation: both must match; methods compare with case", () => {
    const only: PolicyRule[] = [{ to: [{ operation: { paths: ["/certs/root-crl"], methods: ["POST", "PUT"] } }] }];
    expect(policyApplies(only, { host: GW, path: "/certs/root-crl", method: "POST" })).toBe(true);
    expect(policyApplies(only, { host: GW, path: "/certs/root-crl", method: "GET" })).toBe(false);
    expect(policyApplies(only, { host: GW, path: "/certs/renew", method: "POST" })).toBe(false);
    expect(policyApplies(only, { host: GW, path: "/certs/root-crl", method: "post" })).toBe(false);
    const other: PolicyRule[] = [{ to: [{ operation: { hosts: [GW, `${GW}:*`], paths: ["/certs/root-crl"], notMethods: ["GET", "HEAD"] } }] }];
    for (const method of ["GET", "HEAD"]) expect(policyApplies(other, { host: GW, path: "/certs/root-crl", method })).toBe(false);
    for (const method of ["POST", "PUT", "DELETE", "PATCH", "OPTIONS", "get", "head"]) {
      expect(policyApplies(other, { host: GW, path: "/certs/root-crl", method })).toBe(true);
      expect(policyApplies(other, { host: `${GW}:443`, path: "/certs/root-crl", method })).toBe(true);
      // Another host, or another path, is not this rule's.
      expect(policyApplies(other, { host: APP, path: "/certs/root-crl", method })).toBe(false);
      expect(policyApplies(other, { host: GW, path: "/certs/root-crl/x", method })).toBe(false);
    }
  });

  test("fields the evaluation does not know are refused, not ignored", () => {
    expect(() => policyApplies([{ from: [{ source: { principals: ["x"] } }] }], get(APP, "/"))).toThrow("rule field");
    expect(() => policyApplies([{ to: [{ operation: { ports: ["80"] } }] }], get(APP, "/"))).toThrow("operation field");
  });
});

describe("Gateway API route selection", () => {
  test("Exact wins over every prefix; the longest prefix on a segment boundary wins; any port", () => {
    expect(routeFor(ROUTES, get(APP, "/auth/login"))?.backend).toBe("identity");
    expect(routeFor(ROUTES, get(APP, "/auth/login"))?.public).toBe(true);
    expect(routeFor(ROUTES, get(APP, "/auth/other"))?.path).toBe("/auth");
    expect(routeFor(ROUTES, get(APP, "/authx"))?.path).toBe("/");
    expect(routeFor(ROUTES, get(`${APP}:8443`, "/healthz"))?.public).toBe(true);
    expect(routeFor(ROUTES, get("other.example.test", "/"))).toBeUndefined();
  });

  test("the hostname is the Host without a port, lower-case", () => {
    expect(hostnameOf(`${APP}:443`)).toBe(APP);
    expect(hostnameOf("App.Example.Test")).toBe(APP);
    expect(hostnameOf(APP)).toBe(APP);
  });
});

describe("routeListProblems", () => {
  test("the chart's policies check every routed request except the listed exceptions", () => {
    expect(routeListProblems(ROUTES, CHART_RULES, EXCEPTIONS, PORTS, CHART_DENY)).toEqual([]);
  });

  test("every path is probed with the common methods, a lower-case one and the exceptions' own", () => {
    const methods = new Set(sampleRequests(ROUTES, EXCEPTIONS, PORTS).map((request) => request.method));
    expect([...methods].sort()).toEqual([...PROBE_METHODS].sort());
    expect(methods.has("get")).toBe(true);
    const extra = [...EXCEPTIONS, { hostname: GW, path: "/x", methods: ["PROPFIND"] }];
    expect(sampleRequests(ROUTES, extra, PORTS).some((request) => request.method === "PROPFIND")).toBe(true);
  });

  test("a check that leaves out more than the exceptions is caught", () => {
    const wide: PolicyRule[] = [{ to: [{ operation: { notPaths: ["/auth*", "/healthz", "/mqtt"] } }] }, ...CHART_RULES.slice(1)];
    const problems = routeListProblems(ROUTES, wide, EXCEPTIONS, PORTS, CHART_DENY);
    expect(problems).toContain(`GET ${APP}/auth/x: reaches identity through ${APP}/auth without the cookie check`);
    expect(problems).toContain(`POST ${APP}:8443/auth: reaches identity through ${APP}/auth without the cookie check`);
  });

  test("an exception that leaks to another host is caught, with and without a port", () => {
    const leaky = CHART_RULES.filter((rule) => JSON.stringify(rule).includes("notPaths") || JSON.stringify(rule).includes("notMethods"));
    const problems = routeListProblems(ROUTES, leaky, EXCEPTIONS, PORTS, CHART_DENY);
    expect(problems).toContain(`GET ${GW}/healthz: reaches certs through ${GW}/healthz without the cookie check`);
    expect(problems).toContain(`POST ${GW}:443/auth/login: reaches certs through ${GW}/ without the cookie check`);
    expect(problems).toContain(`GET ${APP}/mqtt: reaches assets through ${APP}/ without the cookie check`);
  });

  test("an exception that does not hold with a port is caught", () => {
    const portless: PolicyRule[] = CHART_RULES.map((rule) => JSON.parse(JSON.stringify(rule).replaceAll(/,"[^"]+:\*"/g, "")) as PolicyRule);
    const problems = routeListProblems(ROUTES, portless, EXCEPTIONS, PORTS, CHART_DENY);
    expect(problems).toContain(`GET ${APP}:443/healthz: a listed exception, but the cookie check applies to it`);
    expect(problems).not.toContain(`GET ${APP}/healthz: a listed exception, but the cookie check applies to it`);
  });

  test("a public route that is not a listed exception, or an exception without its route, is caught", () => {
    const extra = [...ROUTES, route(GW, "/status", "Exact", "certs", true)];
    expect(routeListProblems(extra, CHART_RULES, EXCEPTIONS, PORTS, CHART_DENY)).toContain(
      `GET ${GW}/status: served by the public route ${GW}/status, which is not a listed exception`,
    );
    // Without its public route, the exempt path falls through to the host's cookie route unchecked.
    const missing = ROUTES.filter((r) => r.path !== "/mqtt");
    expect(routeListProblems(missing, CHART_RULES, EXCEPTIONS, PORTS, CHART_DENY)).toContain(`GET ${GW}/mqtt: reaches certs through ${GW}/ without the cookie check`);
    // Checked, but on the cookie route: the exception no longer holds.
    expect(routeListProblems(missing, [{}], EXCEPTIONS, PORTS, CHART_DENY)).toContain(`GET ${GW}/mqtt: a listed exception, but it is routed to the cookie route ${GW}/`);
    const none = ROUTES.filter((r) => r.hostname !== GW);
    expect(routeListProblems(none, CHART_RULES, EXCEPTIONS, PORTS, CHART_DENY)).toContain(`GET ${GW}/mqtt: a listed exception has no route`);
  });

  test("a check without rules leaves every route open; one that checks everything closes the exceptions", () => {
    expect(routeListProblems(ROUTES, [], EXCEPTIONS, PORTS, CHART_DENY).filter((p) => p.includes("without the cookie check")).length).toBeGreaterThan(10);
    expect(routeListProblems(ROUTES, [{}], EXCEPTIONS, PORTS, CHART_DENY)).toContain(`HEAD ${APP}/healthz: a listed exception, but the cookie check applies to it`);
  });
});

// Laysi/trustiot-v2#859: an exception holds only for its methods. Any other method on its own host
// and path must be checked (never exempt) and refused by a DENY policy, so it reaches neither the
// exception's backend nor a cookie route that covers the path (here GW's "/" to certs).
describe("routeListProblems: an exception's methods", () => {
  test("without the DENY rules, another method is caught even though the cookie check applies", () => {
    const problems = routeListProblems(ROUTES, CHART_RULES, EXCEPTIONS, PORTS, DENY_INTERNAL);
    expect(problems).toContain(`POST ${GW}/mqtt: not a method of the listed exception (GET), but no DENY policy refuses it`);
    expect(problems).toContain(`GET ${APP}:443/auth/login: not a method of the listed exception (POST), but no DENY policy refuses it`);
    expect(problems).toContain(`get ${APP}/healthz: not a method of the listed exception (GET, HEAD), but no DENY policy refuses it`);
    expect(problems.every((problem) => problem.includes("no DENY policy refuses it"))).toBe(true);
  });

  test("without the cookie check's method rules, another method skips the check and is caught", () => {
    const pathOnly = CHART_RULES.filter((rule) => !METHOD_RULES.includes(rule));
    const problems = routeListProblems(ROUTES, pathOnly, EXCEPTIONS, PORTS, CHART_DENY);
    expect(problems).toContain(`POST ${GW}/mqtt: not a method of the listed exception (GET), but it skips the cookie check`);
    expect(problems).toContain(`PUT ${APP}:8443/healthz: not a method of the listed exception (GET, HEAD), but it skips the cookie check`);
    expect(problems).not.toContain(`GET ${GW}/mqtt: not a method of the listed exception (GET), but it skips the cookie check`);
  });

  test("a DENY rule that refuses a listed method breaks the exception, and is caught", () => {
    const wrong: PolicyRule[] = [...DENY_INTERNAL, { to: [{ operation: { hosts: [GW, `${GW}:*`], paths: ["/mqtt"], methods: ["GET"] } }] }];
    expect(routeListProblems(ROUTES, CHART_RULES, EXCEPTIONS, PORTS, wrong)).toContain(`GET ${GW}/mqtt: a listed exception, but a DENY policy refuses it`);
  });

  test("a method rule without the exception's host checks or refuses the path on every host", () => {
    // Harmless where the path is a cookie route's anyway, but it would check the SPA's static files.
    const hostless = METHOD_RULES.map((rule) => JSON.parse(JSON.stringify(rule).replace(/"hosts":\[[^\]]*\],/, "")) as PolicyRule);
    const problems = routeListProblems(SPA_ROUTES, [...SPA_RULES.slice(0, 5), ...hostless], SPA_EXCEPTIONS, PORTS, [...DENY_INTERNAL, ...METHOD_RULES]);
    expect(problems).toContain(`POST ${APP}/mqtt: a listed exception, but the cookie check applies to it`);
    // The same rule in the DENY policy refuses the static files' path, which is caught on its own.
    const refusing = routeListProblems(SPA_ROUTES, SPA_RULES, SPA_EXCEPTIONS, PORTS, [...DENY_INTERNAL, ...hostless]);
    expect(refusing).toContain(`POST ${APP}/mqtt: a listed exception, but a DENY policy refuses it`);
    expect(refusing.filter((problem) => problem.includes("/internal"))).toEqual([]);
  });

  test("the listed methods pass on the exception's host only; every other method there is refused", () => {
    for (const exception of EXCEPTIONS) {
      if (!("path" in exception)) continue;
      for (const method of PROBE_METHODS) {
        const request = { host: exception.hostname, path: exception.path, method };
        const listed = exception.methods.includes(method);
        expect({ method, path: exception.path, checked: policyApplies(CHART_RULES, request), denied: policyApplies(CHART_DENY, request) }).toEqual({
          method,
          path: exception.path,
          checked: !listed,
          denied: !listed,
        });
      }
    }
  });
});

// The shape the chart renders while services.web serves the SPA's static files on hosts.app
// (Laysi/trustiot-v2#123): hosts.app is checked on the cookie prefixes and /internal only, every
// other host as before, and a public path outside the prefixes is not checked on hosts.app either.
const COOKIE_PREFIXES = ["/api", "/auth"];
const PUBLIC = ["/auth/login", "/healthz", "/mqtt"];
const SPA_EXCEPTIONS: Exception[] = [...EXCEPTIONS, { hostname: APP, outside: COOKIE_PREFIXES }];
const SPA_RULES: PolicyRule[] = [
  { to: [{ operation: { notHosts: [APP, `${APP}:*`], notPaths: PUBLIC } }] },
  { to: [{ operation: { paths: ["/api", "/api/*", "/auth", "/auth/*", "/internal*"], notPaths: PUBLIC } }] },
  { to: [{ operation: { paths: ["/auth/login"], notHosts: [APP, `${APP}:*`] } }] },
  { to: [{ operation: { paths: ["/healthz"], notHosts: [APP, `${APP}:*`] } }] },
  { to: [{ operation: { paths: ["/mqtt"], notHosts: [APP, `${APP}:*`, GW, `${GW}:*`] } }] },
  ...METHOD_RULES,
];
const SPA_ROUTES: RouteRule[] = [
  route(APP, "/auth/login", "Exact", "identity", true),
  route(APP, "/healthz", "Exact", "identity", true),
  route(GW, "/mqtt", "Exact", "mqtt-bridge", true),
  route(APP, "/", "PathPrefix", "web", true),
  route(APP, "/api/assets", "PathPrefix", "assets"),
  route(APP, "/auth", "PathPrefix", "identity"),
  route(GW, "/", "PathPrefix", "certs"),
];

describe("routeListProblems with the SPA's static files", () => {
  test("the chart's policies check every routed request except the listed exceptions", () => {
    expect(routeListProblems(SPA_ROUTES, SPA_RULES, SPA_EXCEPTIONS, PORTS, CHART_DENY)).toEqual([]);
  });

  test("another method on a public path of hosts.app is refused there, not handed to the static files", () => {
    // /healthz is outside the cookie prefixes, so on hosts.app any other path there is the SPA's;
    // POST /healthz is not: it is checked and denied.
    expect(policyApplies(SPA_RULES, { host: APP, path: "/healthz", method: "POST" })).toBe(true);
    expect(policyApplies(CHART_DENY, { host: APP, path: "/healthz", method: "POST" })).toBe(true);
    expect(policyApplies(SPA_RULES, { host: APP, path: "/healthzx", method: "POST" })).toBe(false);
    // The gateway's public paths on hosts.app are the static files with every method.
    expect(policyApplies(SPA_RULES, { host: APP, path: "/mqtt", method: "POST" })).toBe(false);
    expect(policyApplies(CHART_DENY, { host: APP, path: "/mqtt", method: "POST" })).toBe(false);
  });

  test("the probes cover both sides of each prefix's boundary, in another case too", () => {
    const paths = new Set(sampleRequests(SPA_ROUTES, SPA_EXCEPTIONS, PORTS).map((request) => request.path));
    for (const path of ["/", "/x", "/api", "/api/x", "/apix", "/API/x", "/auth", "/auth/", "/auth/x", "/authx", "/AUTH/x", "/internal"]) {
      expect(paths.has(path)).toBe(true);
    }
    // Gateway API and Istio compare paths with case: /API/x and /apix are the SPA's, /auth/ is not.
    for (const path of ["/API/x", "/apix", "/authx"]) {
      expect(routeFor(SPA_ROUTES, get(APP, path))?.backend).toBe("web");
      expect(policyApplies(SPA_RULES, get(APP, path))).toBe(false);
    }
    expect(policyApplies(SPA_RULES, get(APP, "/auth/"))).toBe(true);
  });

  test("a checked request without a cookie route is refused by the identity service, so it is no problem", () => {
    // /api/x lands on the SPA's route, but it is checked and no cookie route matches it.
    expect(routeFor(SPA_ROUTES, get(APP, "/api/x"))?.backend).toBe("web");
    expect(policyApplies(SPA_RULES, get(APP, "/api/x"))).toBe(true);
    // A public route that hides a cookie route gets a token signed for the cookie route's service.
    const hiding = [...SPA_ROUTES, route(APP, "/api/assets/open", "Exact", "assets", true)];
    expect(routeListProblems(hiding, SPA_RULES, SPA_EXCEPTIONS, PORTS, CHART_DENY)).toContain(
      `GET ${APP}/api/assets/open: served by the public route ${APP}/api/assets/open, which is not a listed exception`,
    );
  });

  test("a check without the cookie prefixes leaves the API open and gives its paths to the SPA", () => {
    const open = SPA_RULES.filter((_, i) => i !== 1);
    const problems = routeListProblems(SPA_ROUTES, open, SPA_EXCEPTIONS, PORTS, CHART_DENY);
    expect(problems).toContain(`GET ${APP}/api/assets: reaches assets through ${APP}/api/assets without the cookie check`);
    expect(problems).toContain(`GET ${APP}:443/auth/x: reaches identity through ${APP}/auth without the cookie check`);
    expect(problems).toContain(`GET ${APP}/api/x: served by the public route ${APP}/, which is not a listed exception`);
    // Bare prefixes only: /api itself is not the SPA's either.
    const starOnly = SPA_RULES.map((rule) => JSON.parse(JSON.stringify(rule).replace('"/api",', "")) as PolicyRule);
    expect(routeListProblems(SPA_ROUTES, starOnly, SPA_EXCEPTIONS, PORTS, CHART_DENY)).toContain(
      `GET ${APP}/api: served by the public route ${APP}/, which is not a listed exception`,
    );
  });

  test("a cookie route on hosts.app outside the prefixes, or an exemption of another host, is caught", () => {
    const outside = [...SPA_ROUTES, route(APP, "/settings", "PathPrefix", "assets")];
    expect(routeListProblems(outside, SPA_RULES, SPA_EXCEPTIONS, PORTS, CHART_DENY)).toContain(
      `GET ${APP}/settings: reaches assets through ${APP}/settings without the cookie check`,
    );
    const both = SPA_RULES.map((rule, i) =>
      i === 0 ? { to: [{ operation: { notHosts: [APP, `${APP}:*`, GW, `${GW}:*`], notPaths: PUBLIC } }] } : rule,
    );
    expect(routeListProblems(SPA_ROUTES, both, SPA_EXCEPTIONS, PORTS, CHART_DENY)).toContain(`GET ${GW}:8443/x: reaches certs through ${GW}/ without the cookie check`);
  });

  test("the static files hold only on their own route: an exact public route they cover is still unlisted", () => {
    const extra = [...SPA_ROUTES, route(APP, "/status", "Exact", "assets", true)];
    expect(routeListProblems(extra, SPA_RULES, SPA_EXCEPTIONS, PORTS, CHART_DENY)).toEqual(
      [APP, `${APP}:80`, `${APP}:443`, `${APP}:8443`].flatMap((host) =>
        PROBE_METHODS.map((method) => `${method} ${host}/status: served by the public route ${APP}/status, which is not a listed exception`),
      ),
    );
  });

  test("checking the static files, or giving them no route, is caught; only DENY makes /internal no problem", () => {
    expect(routeListProblems(SPA_ROUTES, [{}], SPA_EXCEPTIONS, PORTS, CHART_DENY)).toContain(`GET ${APP}/x: a listed exception, but the cookie check applies to it`);
    const none = SPA_ROUTES.filter((r) => r.backend !== "web");
    expect(routeListProblems(none, SPA_RULES, SPA_EXCEPTIONS, PORTS, CHART_DENY)).toContain(`GET ${APP}/static/app.js: a listed exception has no route`);
    expect(routeListProblems(SPA_ROUTES, SPA_RULES, SPA_EXCEPTIONS, PORTS, METHOD_RULES)).toContain(
      `GET ${APP}/internal: a listed exception, but the cookie check applies to it`,
    );
  });
});
