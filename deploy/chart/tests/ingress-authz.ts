// The route-list proof of decision 27 (Laysi/trustiot-v2#77): with the identity service down the
// ingress refuses every request except the listed exceptions, because every route is behind the
// cookie check. The chart renders the routes (Gateway API HTTPRoutes) and the check (an Istio
// CUSTOM AuthorizationPolicy on the Gateway) separately, so this module evaluates both the way the
// gateway does and lists every request that a route serves without the check. chart.test.ts runs
// it on rendered manifests; ingress-authz.test.ts tests the evaluation itself, with no helm.
//
// Semantics followed:
// - HTTPRoute (Gateway API, as Istio implements it): a route's hostname matches the Host with or
//   without a port; an Exact match wins over every PathPrefix match, and among PathPrefix matches
//   the longest one that ends at a segment boundary wins.
// - AuthorizationPolicy (Istio): the policy applies when any rule matches; a rule without `to`
//   matches every request, and a rule matches when any of its operations does; an operation
//   matches when every field it sets does (`hosts`/`paths` must match one value, `notHosts`/
//   `notPaths` must match none). A value is exact, `*`, a prefix (`abc*`) or a suffix (`*abc`);
//   hosts compare without case. `from` and `when` are not used by the chart, and are refused
//   rather than guessed.
// - Paths are those the gateway compares after the mesh's path normalization
//   (deploy/istio/README.md), so the requests here are already normalized.
// - A request a DENY policy refuses (/internal) never reaches a route, whatever the check says.
// - A checked request on a route the identity service has no cookie route for gets no token:
//   the identity service refuses it (403, services/identity/src/audience.rs, which matches the
//   cookie routes the way routeFor does), so it never reaches that route's backend.

export interface RouteRule {
  hostname: string;
  path: string;
  match: "Exact" | "PathPrefix";
  /** A public path of decision 27: the route strips the identity header. */
  public: boolean;
  /** The backend Service, for messages. */
  backend: string;
}

export interface Operation {
  hosts?: string[];
  notHosts?: string[];
  paths?: string[];
  notPaths?: string[];
  [field: string]: unknown;
}

export interface PolicyRule {
  to?: { operation: Operation }[];
  [field: string]: unknown;
}

export interface Request {
  /** The Host header, with or without a port. */
  host: string;
  path: string;
}

/**
 * An exception that decision 27 lists: no cookie on this host and exact path, or (the SPA's
 * static files) on every path of the host outside the given prefixes, each prefix itself and
 * every path below it.
 */
export type Exception = { hostname: string; path: string } | { hostname: string; outside: string[] };

/** Istio's string match: exact, `*`, prefix `abc*` or suffix `*abc`. */
export function istioMatches(pattern: string, value: string, ignoreCase = false): boolean {
  const [p, v] = ignoreCase ? [pattern.toLowerCase(), value.toLowerCase()] : [pattern, value];
  if (p === "*") return true;
  if (p.endsWith("*")) return v.startsWith(p.slice(0, -1));
  if (p.startsWith("*")) return v.endsWith(p.slice(1));
  return p === v;
}

const OPERATION_FIELDS = new Set(["hosts", "notHosts", "paths", "notPaths"]);

function operationMatches(operation: Operation, request: Request): boolean {
  for (const field of Object.keys(operation)) {
    if (!OPERATION_FIELDS.has(field)) throw new Error(`an operation field this evaluation does not know: ${field}`);
  }
  const host = (patterns: string[]) => patterns.some((pattern) => istioMatches(pattern, request.host, true));
  const path = (patterns: string[]) => patterns.some((pattern) => istioMatches(pattern, request.path));
  if (operation.hosts !== undefined && !host(operation.hosts)) return false;
  if (operation.notHosts !== undefined && host(operation.notHosts)) return false;
  if (operation.paths !== undefined && !path(operation.paths)) return false;
  if (operation.notPaths !== undefined && path(operation.notPaths)) return false;
  return true;
}

/** Whether the policy with these rules applies to the request (for CUSTOM: the request is checked). */
export function policyApplies(rules: PolicyRule[], request: Request): boolean {
  return rules.some((rule) => {
    for (const field of Object.keys(rule)) {
      if (field !== "to") throw new Error(`a rule field this evaluation does not know: ${field}`);
    }
    if (rule.to === undefined || rule.to.length === 0) return true;
    return rule.to.some(({ operation }) => operationMatches(operation, request));
  });
}

/** The Host without its port, lower-case: what a route's hostname is compared with. */
export function hostnameOf(host: string): string {
  const colon = host.lastIndexOf(":");
  return (colon >= 0 && /^\d+$/.test(host.slice(colon + 1)) ? host.slice(0, colon) : host).toLowerCase();
}

function prefixMatches(prefix: string, path: string): boolean {
  if (prefix === "/") return true;
  return path === prefix || path.startsWith(`${prefix}/`);
}

/** The route the gateway picks for a request, or undefined when it answers 404 itself. */
export function routeFor(routes: RouteRule[], request: Request): RouteRule | undefined {
  const candidates = routes.filter((route) => route.hostname.toLowerCase() === hostnameOf(request.host));
  const exact = candidates.find((route) => route.match === "Exact" && route.path === request.path);
  if (exact !== undefined) return exact;
  return candidates
    .filter((route) => route.match === "PathPrefix" && prefixMatches(route.path, request.path))
    .sort((a, b) => b.path.length - a.path.length)[0];
}

/**
 * The requests that reach each route: its own path, one below a prefix, and every exception path;
 * for an exception outside prefixes, paths on both sides of each prefix's boundary.
 */
export function sampleRequests(routes: RouteRule[], exceptions: Exception[], ports: number[]): Request[] {
  const paths = new Set<string>();
  for (const exception of exceptions) {
    if ("path" in exception) {
      paths.add(exception.path);
      continue;
    }
    for (const path of ["/", "/x", "/static/app.js", "/internal", "/internal/x"]) paths.add(path);
    for (const prefix of exception.outside) {
      for (const path of [prefix, `${prefix}/`, `${prefix}/x`, `${prefix}x`, `${prefix.toUpperCase()}/x`]) paths.add(path);
    }
  }
  for (const route of routes) {
    paths.add(route.path);
    if (route.match === "PathPrefix") paths.add(route.path === "/" ? "/x" : `${route.path}/x`);
  }
  const hostnames = new Set([...routes.map((route) => route.hostname), ...exceptions.map((exception) => exception.hostname)]);
  const requests: Request[] = [];
  for (const hostname of hostnames) {
    for (const host of [hostname, ...ports.map((port) => `${hostname}:${port}`)]) {
      for (const path of paths) requests.push({ host, path });
    }
  }
  return requests;
}

/** The exception a request falls under: one with its exact path, or one outside prefixes. */
function exceptionFor(exceptions: Exception[], request: Request): Exception | undefined {
  const onHost = exceptions.filter((exception) => exception.hostname.toLowerCase() === hostnameOf(request.host));
  return (
    onHost.find((exception) => "path" in exception && exception.path === request.path) ??
    onHost.find((exception) => "outside" in exception && !exception.outside.some((prefix) => prefixMatches(prefix, request.path)))
  );
}

/**
 * Every way the routes and the cookie check disagree with decision 27, as messages; empty when
 * every request a route serves is checked except exactly the listed exceptions, which are not.
 * An exception outside prefixes holds only on a PathPrefix public route (the SPA's): an exact
 * public route it covers is still a public route nobody listed. denyRules are the rules of the
 * gateway's DENY policies.
 */
export function routeListProblems(
  routes: RouteRule[],
  checkRules: PolicyRule[],
  exceptions: Exception[],
  ports: number[],
  denyRules: PolicyRule[] = [],
): string[] {
  const problems: string[] = [];
  const cookieRoutes = routes.filter((route) => !route.public);
  for (const request of sampleRequests(routes, exceptions, ports)) {
    if (policyApplies(denyRules, request)) continue;
    const route = routeFor(routes, request);
    const checked = policyApplies(checkRules, request);
    const exception = exceptionFor(exceptions, request);
    const where = `${request.host}${request.path}`;
    if (route === undefined) {
      // The gateway answers 404 without a backend; an exception must have its route, though.
      if (exception !== undefined) problems.push(`${where}: a listed exception has no route`);
      continue;
    }
    if (route.public) {
      const listed = exception !== undefined && ("path" in exception || route.match === "PathPrefix");
      if (!listed) {
        // Checked, and no cookie route for the identity service to sign a token for: refused.
        if (checked && routeFor(cookieRoutes, request) === undefined) continue;
        problems.push(`${where}: served by the public route ${route.hostname}${route.path}, which is not a listed exception`);
      } else if (checked) problems.push(`${where}: a listed exception, but the cookie check applies to it`);
    } else if (!checked) {
      problems.push(`${where}: reaches ${route.backend} through ${route.hostname}${route.path} without the cookie check`);
    } else if (exception !== undefined) {
      problems.push(`${where}: a listed exception, but it is routed to the cookie route ${route.hostname}${route.path}`);
    }
  }
  return problems;
}
