// Unit tests of the route-list evaluation (ingress-authz.ts, Laysi/trustiot-v2#77): no helm, no
// cluster. chart.test.ts applies it to the rendered chart.
import { describe, expect, test } from "bun:test";

import { hostnameOf, istioMatches, policyApplies, routeFor, routeListProblems, sampleRequests } from "./ingress-authz.ts";
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

// The shape the chart renders (templates/ingress.yaml): every path but the exceptions, and each
// exception on every host but its own, with or without a port.
const EXCEPTIONS: Exception[] = [
  { hostname: APP, path: "/auth/login" },
  { hostname: APP, path: "/healthz" },
  { hostname: GW, path: "/mqtt" },
];
const CHART_RULES: PolicyRule[] = [
  { to: [{ operation: { notPaths: ["/auth/login", "/healthz", "/mqtt"] } }] },
  { to: [{ operation: { paths: ["/auth/login"], notHosts: [APP, `${APP}:*`] } }] },
  { to: [{ operation: { paths: ["/healthz"], notHosts: [APP, `${APP}:*`] } }] },
  { to: [{ operation: { paths: ["/mqtt"], notHosts: [GW, `${GW}:*`] } }] },
];
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
    expect(policyApplies([{}], { host: APP, path: "/healthz" })).toBe(true);
    expect(policyApplies([], { host: APP, path: "/api" })).toBe(false);
  });

  test("an operation matches when every field it sets does", () => {
    const rules: PolicyRule[] = [{ to: [{ operation: { paths: ["/healthz"], notHosts: [APP, `${APP}:*`] } }] }];
    expect(policyApplies(rules, { host: GW, path: "/healthz" })).toBe(true);
    expect(policyApplies(rules, { host: `${GW}:443`, path: "/healthz" })).toBe(true);
    expect(policyApplies(rules, { host: APP, path: "/healthz" })).toBe(false);
    expect(policyApplies(rules, { host: `${APP}:8443`, path: "/healthz" })).toBe(false);
    expect(policyApplies(rules, { host: GW, path: "/api" })).toBe(false);
  });

  test("fields the evaluation does not know are refused, not ignored", () => {
    expect(() => policyApplies([{ from: [{ source: { principals: ["x"] } }] }], { host: APP, path: "/" })).toThrow("rule field");
    expect(() => policyApplies([{ to: [{ operation: { methods: ["GET"] } }] }], { host: APP, path: "/" })).toThrow("operation field");
  });
});

describe("Gateway API route selection", () => {
  test("Exact wins over every prefix; the longest prefix on a segment boundary wins; any port", () => {
    expect(routeFor(ROUTES, { host: APP, path: "/auth/login" })?.backend).toBe("identity");
    expect(routeFor(ROUTES, { host: APP, path: "/auth/login" })?.public).toBe(true);
    expect(routeFor(ROUTES, { host: APP, path: "/auth/other" })?.path).toBe("/auth");
    expect(routeFor(ROUTES, { host: APP, path: "/authx" })?.path).toBe("/");
    expect(routeFor(ROUTES, { host: `${APP}:8443`, path: "/healthz" })?.public).toBe(true);
    expect(routeFor(ROUTES, { host: "other.example.test", path: "/" })).toBeUndefined();
  });

  test("the hostname is the Host without a port, lower-case", () => {
    expect(hostnameOf(`${APP}:443`)).toBe(APP);
    expect(hostnameOf("App.Example.Test")).toBe(APP);
    expect(hostnameOf(APP)).toBe(APP);
  });
});

describe("routeListProblems", () => {
  test("the chart's policy checks every routed request except the listed exceptions", () => {
    expect(routeListProblems(ROUTES, CHART_RULES, EXCEPTIONS, PORTS)).toEqual([]);
  });

  test("a check that leaves out more than the exceptions is caught", () => {
    const wide: PolicyRule[] = [{ to: [{ operation: { notPaths: ["/auth*", "/healthz", "/mqtt"] } }] }, ...CHART_RULES.slice(1)];
    const problems = routeListProblems(ROUTES, wide, EXCEPTIONS, PORTS);
    expect(problems).toContain(`${APP}/auth/x: reaches identity through ${APP}/auth without the cookie check`);
    expect(problems).toContain(`${APP}:8443/auth: reaches identity through ${APP}/auth without the cookie check`);
  });

  test("an exception that leaks to another host is caught, with and without a port", () => {
    const leaky = CHART_RULES.filter((rule) => JSON.stringify(rule).includes("notPaths"));
    const problems = routeListProblems(ROUTES, leaky, EXCEPTIONS, PORTS);
    expect(problems).toContain(`${GW}/healthz: reaches certs through ${GW}/healthz without the cookie check`);
    expect(problems).toContain(`${GW}:443/auth/login: reaches certs through ${GW}/ without the cookie check`);
    expect(problems).toContain(`${APP}/mqtt: reaches assets through ${APP}/ without the cookie check`);
  });

  test("an exception that does not hold with a port is caught", () => {
    const portless: PolicyRule[] = CHART_RULES.map((rule) => JSON.parse(JSON.stringify(rule).replaceAll(/,"[^"]+:\*"/g, "")) as PolicyRule);
    const problems = routeListProblems(ROUTES, portless, EXCEPTIONS, PORTS);
    expect(problems).toContain(`${APP}:443/healthz: a listed exception, but the cookie check applies to it`);
    expect(problems).not.toContain(`${APP}/healthz: a listed exception, but the cookie check applies to it`);
  });

  test("a public route that is not a listed exception, or an exception without its route, is caught", () => {
    const extra = [...ROUTES, route(GW, "/status", "Exact", "certs", true)];
    expect(routeListProblems(extra, CHART_RULES, EXCEPTIONS, PORTS)).toContain(`${GW}/status: served by the public route ${GW}/status, which is not a listed exception`);
    // Without its public route, the exempt path falls through to the host's cookie route unchecked.
    const missing = ROUTES.filter((r) => r.path !== "/mqtt");
    expect(routeListProblems(missing, CHART_RULES, EXCEPTIONS, PORTS)).toContain(`${GW}/mqtt: reaches certs through ${GW}/ without the cookie check`);
    // Checked, but on the cookie route: the exception no longer holds.
    expect(routeListProblems(missing, [{}], EXCEPTIONS, PORTS)).toContain(`${GW}/mqtt: a listed exception, but it is routed to the cookie route ${GW}/`);
    const none = ROUTES.filter((r) => r.hostname !== GW);
    expect(routeListProblems(none, CHART_RULES, EXCEPTIONS, PORTS)).toContain(`${GW}/mqtt: a listed exception has no route`);
  });

  test("a check without rules leaves every route open; one that checks everything closes the exceptions", () => {
    expect(routeListProblems(ROUTES, [], EXCEPTIONS, PORTS).filter((p) => p.includes("without the cookie check")).length).toBeGreaterThan(10);
    expect(routeListProblems(ROUTES, [{}], EXCEPTIONS, PORTS)).toContain(`${APP}/healthz: a listed exception, but the cookie check applies to it`);
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
];
const DENY: PolicyRule[] = [{ to: [{ operation: { paths: ["/internal*"] } }] }];
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
  test("the chart's policy checks every routed request except the listed exceptions", () => {
    expect(routeListProblems(SPA_ROUTES, SPA_RULES, SPA_EXCEPTIONS, PORTS, DENY)).toEqual([]);
  });

  test("the probes cover both sides of each prefix's boundary, in another case too", () => {
    const paths = new Set(sampleRequests(SPA_ROUTES, SPA_EXCEPTIONS, PORTS).map((request) => request.path));
    for (const path of ["/", "/x", "/api", "/api/x", "/apix", "/API/x", "/auth", "/auth/", "/auth/x", "/authx", "/AUTH/x", "/internal"]) {
      expect(paths.has(path)).toBe(true);
    }
    // Gateway API and Istio compare paths with case: /API/x and /apix are the SPA's, /auth/ is not.
    for (const path of ["/API/x", "/apix", "/authx"]) {
      expect(routeFor(SPA_ROUTES, { host: APP, path })?.backend).toBe("web");
      expect(policyApplies(SPA_RULES, { host: APP, path })).toBe(false);
    }
    expect(policyApplies(SPA_RULES, { host: APP, path: "/auth/" })).toBe(true);
  });

  test("a checked request without a cookie route is refused by the identity service, so it is no problem", () => {
    // /api/x lands on the SPA's route, but it is checked and no cookie route matches it.
    expect(routeFor(SPA_ROUTES, { host: APP, path: "/api/x" })?.backend).toBe("web");
    expect(policyApplies(SPA_RULES, { host: APP, path: "/api/x" })).toBe(true);
    // A public route that hides a cookie route gets a token signed for the cookie route's service.
    const hiding = [...SPA_ROUTES, route(APP, "/api/assets/open", "Exact", "assets", true)];
    expect(routeListProblems(hiding, SPA_RULES, SPA_EXCEPTIONS, PORTS, DENY)).toContain(
      `${APP}/api/assets/open: served by the public route ${APP}/api/assets/open, which is not a listed exception`,
    );
  });

  test("a check without the cookie prefixes leaves the API open and gives its paths to the SPA", () => {
    const open = SPA_RULES.filter((_, i) => i !== 1);
    const problems = routeListProblems(SPA_ROUTES, open, SPA_EXCEPTIONS, PORTS, DENY);
    expect(problems).toContain(`${APP}/api/assets: reaches assets through ${APP}/api/assets without the cookie check`);
    expect(problems).toContain(`${APP}:443/auth/x: reaches identity through ${APP}/auth without the cookie check`);
    expect(problems).toContain(`${APP}/api/x: served by the public route ${APP}/, which is not a listed exception`);
    // Bare prefixes only: /api itself is not the SPA's either.
    const starOnly = SPA_RULES.map((rule) => JSON.parse(JSON.stringify(rule).replace('"/api",', "")) as PolicyRule);
    expect(routeListProblems(SPA_ROUTES, starOnly, SPA_EXCEPTIONS, PORTS, DENY)).toContain(
      `${APP}/api: served by the public route ${APP}/, which is not a listed exception`,
    );
  });

  test("a cookie route on hosts.app outside the prefixes, or an exemption of another host, is caught", () => {
    const outside = [...SPA_ROUTES, route(APP, "/settings", "PathPrefix", "assets")];
    expect(routeListProblems(outside, SPA_RULES, SPA_EXCEPTIONS, PORTS, DENY)).toContain(
      `${APP}/settings: reaches assets through ${APP}/settings without the cookie check`,
    );
    const both = SPA_RULES.map((rule, i) =>
      i === 0 ? { to: [{ operation: { notHosts: [APP, `${APP}:*`, GW, `${GW}:*`], notPaths: PUBLIC } }] } : rule,
    );
    expect(routeListProblems(SPA_ROUTES, both, SPA_EXCEPTIONS, PORTS, DENY)).toContain(`${GW}:8443/x: reaches certs through ${GW}/ without the cookie check`);
  });

  test("the static files hold only on their own route: an exact public route they cover is still unlisted", () => {
    const extra = [...SPA_ROUTES, route(APP, "/status", "Exact", "assets", true)];
    expect(routeListProblems(extra, SPA_RULES, SPA_EXCEPTIONS, PORTS, DENY)).toEqual([
      `${APP}/status: served by the public route ${APP}/status, which is not a listed exception`,
      `${APP}:80/status: served by the public route ${APP}/status, which is not a listed exception`,
      `${APP}:443/status: served by the public route ${APP}/status, which is not a listed exception`,
      `${APP}:8443/status: served by the public route ${APP}/status, which is not a listed exception`,
    ]);
  });

  test("checking the static files, or giving them no route, is caught; only DENY makes /internal no problem", () => {
    expect(routeListProblems(SPA_ROUTES, [{}], SPA_EXCEPTIONS, PORTS, DENY)).toContain(`${APP}/x: a listed exception, but the cookie check applies to it`);
    const none = SPA_ROUTES.filter((r) => r.backend !== "web");
    expect(routeListProblems(none, SPA_RULES, SPA_EXCEPTIONS, PORTS, DENY)).toContain(`${APP}/static/app.js: a listed exception has no route`);
    expect(routeListProblems(SPA_ROUTES, SPA_RULES, SPA_EXCEPTIONS, PORTS)).toContain(`${APP}/internal: a listed exception, but the cookie check applies to it`);
  });
});
