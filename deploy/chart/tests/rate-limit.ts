// The ingress rate limit (Laysi/trustiot-v2#113) as the chart renders it: Envoy's local rate
// limit filter inside the EnvoyFilter of templates/ingress.yaml. Shared by the chart tests
// (chart.test.ts, no Envoy) and the test that runs the rendered filter in the Envoy build Istio
// ships (envoy-rate-limit.e2e.ts), so both read the filter from the same place.

type YamlMap = { [key: string]: unknown };

export type RateLimitManifest = { apiVersion: string; kind: string; metadata: { name: string }; spec?: unknown };

/** The typed_config of the local rate limit filter: what the chart hands Envoy. */
export type LocalRateLimit = {
  "@type": string;
  stat_prefix: string;
  filter_enabled: { runtime_key: string; default_value: { numerator: number; denominator: string } };
  filter_enforced: { runtime_key: string; default_value: { numerator: number; denominator: string } };
  always_consume_default_token_bucket: boolean;
  max_dynamic_descriptors: number;
  token_bucket?: unknown;
  rate_limits: { actions: RateLimitAction[] }[];
  descriptors: { entries: { key: string; value?: string }[]; token_bucket: TokenBucket }[];
};

export type TokenBucket = { max_tokens: number; tokens_per_fill: number; fill_interval: string };

export type HeaderMatcher = {
  name: string;
  present_match?: boolean;
  invert_match?: boolean;
  string_match?: { safe_regex: { regex: string } };
};

export type RateLimitAction =
  | { request_headers: { header_name: string; descriptor_key: string } }
  | { header_value_match: { descriptor_key: string; descriptor_value: string; headers: HeaderMatcher[] } };

/** The Lua filter before the rate limit, which writes the client it counts. */
export type ClientLua = { "@type": string; default_source_code: { inline_string: string } };

export type RateLimitPatch = {
  targetRefs: { group: string; kind: string; name: string }[];
  configPatches: {
    applyTo: string;
    match: { context: string; listener: { portNumber: number; filterChain: { filter: { name: string; subFilter?: { name: string } } } } };
    patch: { operation: string; value: { name: string; typed_config: LocalRateLimit | ClientLua } };
  }[];
};

export const LOCAL_RATE_LIMIT_TYPE = "type.googleapis.com/envoy.extensions.filters.http.local_ratelimit.v3.LocalRateLimit";
export const LUA_TYPE = "type.googleapis.com/envoy.extensions.filters.http.lua.v3.Lua";
/** The header the Lua filter writes and the rate limit counts by (templates/_ingress.tpl). */
export const CLIENT_HEADER = "x-trustiot-rate-limit-client";

function isMap(value: unknown): value is YamlMap {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** The rate limit EnvoyFilters among rendered manifests (none when the key is empty). */
export function rateLimitFilters(manifests: readonly RateLimitManifest[]): RateLimitManifest[] {
  return manifests.filter((m) => m.kind === "EnvoyFilter");
}

/** The one rate limit EnvoyFilter's spec; throws when there is none or more than one. */
export function rateLimitPatchOf(manifests: readonly RateLimitManifest[]): RateLimitPatch {
  const filters = rateLimitFilters(manifests);
  if (filters.length !== 1) throw new Error(`expected one EnvoyFilter, found ${filters.length}`);
  const spec = filters[0]?.spec;
  if (!isMap(spec)) throw new Error("the EnvoyFilter has no spec");
  return spec as RateLimitPatch;
}

function patchCarrying(manifests: readonly RateLimitManifest[], type: string) {
  const found = rateLimitPatchOf(manifests).configPatches.filter((p) => p.patch.value.typed_config["@type"] === type);
  if (found.length !== 1) throw new Error(`expected one config patch with ${type}, found ${found.length}`);
  return found[0]?.patch.value.typed_config;
}

/** The local rate limit filter's configuration from the one rate limit EnvoyFilter. */
export function localRateLimitOf(manifests: readonly RateLimitManifest[]): LocalRateLimit {
  return patchCarrying(manifests, LOCAL_RATE_LIMIT_TYPE) as LocalRateLimit;
}

/** The Lua filter that writes the rate limit's client, from the one rate limit EnvoyFilter. */
export function clientLuaOf(manifests: readonly RateLimitManifest[]): ClientLua {
  return patchCarrying(manifests, LUA_TYPE) as ClientLua;
}

/** Both filters, in the order Envoy runs them. */
export type RateLimitFilters = { lua: ClientLua; rateLimit: LocalRateLimit };

export function rateLimitFiltersOf(manifests: readonly RateLimitManifest[]): RateLimitFilters {
  return { lua: clientLuaOf(manifests), rateLimit: localRateLimitOf(manifests) };
}

// A model of the Lua filter's client (templates/_ingress.tpl, "trustiot.ingress.rateLimitClientLua")
// for the chart tests, which run no Envoy: an IPv6 address becomes its /64, anything else stays.
// envoy-rate-limit.e2e.ts checks the Lua itself.
function prefix64(address: string): string | undefined {
  let text = address.replace(/%.*$/, "");
  const tail = /^(.*:)(\d+)\.(\d+)\.(\d+)\.(\d+)$/.exec(text);
  if (tail) {
    const [a, b, c, d] = tail.slice(2).map(Number) as [number, number, number, number];
    if ([a, b, c, d].some((n) => n > 255)) return undefined;
    text = `${tail[1]}${(a * 256 + b).toString(16)}:${(c * 256 + d).toString(16)}`;
  }
  const sides = text.split("::");
  if (sides.length > 2) return undefined;
  const groups = (side: string) => (side === "" ? [] : side.split(":"));
  const first = groups(sides[0] ?? "");
  const last = sides.length === 2 ? groups(sides[1] ?? "") : [];
  if (![...first, ...last].every((g) => /^[0-9a-f]{1,4}$/.test(g))) return undefined;
  const count = first.length + last.length;
  if (sides.length === 2 ? count > 7 : count !== 8) return undefined;
  const all = [...first, ...Array<string>(8 - count).fill("0"), ...last].map((g) => Number.parseInt(g, 16).toString(16));
  return `${all.slice(0, 4).join(":")}::/64`;
}

/** The client header the Lua filter writes for a CF-Connecting-IP value; undefined leaves it out. */
export function luaClient(value: string | undefined): string | undefined {
  if (value === undefined) return undefined;
  const trimmed = value.toLowerCase().trim();
  if (trimmed === "") return undefined;
  return trimmed.includes(":") ? (prefix64(trimmed) ?? trimmed) : trimmed;
}

/** A request as it reaches the gateway: Host, path after normalization, and its other headers. */
export type FilterRequest = { authority: string; path: string; headers?: [string, string][] };

/** The request as the rate limit sees it, after the Lua filter (as luaClient models it). */
export function afterLua(request: FilterRequest): FilterRequest {
  const headers = (request.headers ?? []).filter(([name]) => name.toLowerCase() !== CLIENT_HEADER);
  const client = luaClient(headers.find(([name]) => name.toLowerCase() === "cf-connecting-ip")?.[1]);
  return { ...request, headers: client === undefined ? headers : [...headers, [CLIENT_HEADER, client]] };
}

export type Descriptor = { key: string; value: string }[];

// Envoy matches safe_regex against the whole value (RE2 FullMatch). The chart's patterns start
// with RE2's (?i) flag where they ignore case, which JavaScript writes as a flag instead.
function fullMatch(pattern: string, value: string): boolean {
  const insensitive = pattern.startsWith("(?i)");
  return new RegExp(`^(?:${insensitive ? pattern.slice(4) : pattern})$`, insensitive ? "i" : "").test(value);
}

function headerValue(request: FilterRequest, name: string): string | undefined {
  if (name === ":authority") return request.authority;
  if (name === ":path") return request.path;
  return (request.headers ?? []).find(([n]) => n.toLowerCase() === name)?.[1];
}

function headerMatches(request: FilterRequest, matcher: HeaderMatcher): boolean {
  const value = headerValue(request, matcher.name);
  let matched: boolean;
  if (matcher.present_match !== undefined) matched = (value !== undefined) === matcher.present_match;
  else if (matcher.string_match) matched = value !== undefined && fullMatch(matcher.string_match.safe_regex.regex, value);
  else throw new Error(`unsupported header matcher ${JSON.stringify(matcher)}`);
  return matcher.invert_match ? !matched : matched;
}

/**
 * The descriptors Envoy builds for a request from the filter's rate_limits, as Envoy's rate
 * limit actions do: a descriptor exists only when every action of its entry produces an entry
 * (request_headers needs the header; header_value_match needs every matcher to match).
 * Only the two actions the chart uses are modelled.
 */
export function requestDescriptors(filter: LocalRateLimit, request: FilterRequest): Descriptor[] {
  const descriptors: Descriptor[] = [];
  for (const limit of filter.rate_limits) {
    const entries: Descriptor = [];
    for (const action of limit.actions) {
      if ("request_headers" in action) {
        const value = headerValue(request, action.request_headers.header_name);
        if (value === undefined) break;
        entries.push({ key: action.request_headers.descriptor_key, value });
      } else if ("header_value_match" in action) {
        const match = action.header_value_match;
        if (!match.headers.every((m) => headerMatches(request, m))) break;
        entries.push({ key: match.descriptor_key, value: match.descriptor_value });
      } else {
        throw new Error(`unsupported rate limit action ${JSON.stringify(action)}`);
      }
    }
    if (entries.length === limit.actions.length) descriptors.push(entries);
  }
  return descriptors;
}

/**
 * The configured descriptors (their token buckets) that a request takes a token from: the
 * same number of entries with the same keys, and the same value where the configuration
 * names one (an entry without a value matches any value and counts per value).
 */
export function matchedBuckets(filter: LocalRateLimit, request: FilterRequest): { entries: Descriptor; token_bucket: TokenBucket }[] {
  const requested = requestDescriptors(filter, afterLua(request));
  return requested.flatMap((descriptor) =>
    filter.descriptors
      .filter(
        (configured) =>
          configured.entries.length === descriptor.length &&
          configured.entries.every((entry, i) => entry.key === descriptor[i]?.key && (entry.value === undefined || entry.value === descriptor[i]?.value)),
      )
      .map((configured) => ({ entries: descriptor, token_bucket: configured.token_bucket })),
  );
}

/**
 * A static Envoy configuration that puts the two filters, in the chart's order, in front of routes
 * that answer 200 to every request ("login" for /auth/login exactly, so a test sees what the
 * router made of a path; "routed" for the rest), on one plain HTTP listener. The connection manager normalizes paths the way the mesh
 * does (deploy/istio/istiod-values.yaml, DECODE_AND_MERGE_SLASHES: RFC 3986 normalization,
 * merged slashes, %2F decoded), and like Istio's gateway it takes the TCP peer as the client
 * address and appends it to X-Forwarded-For, trusting no hop before it.
 */
export function envoyBootstrap(filters: RateLimitFilters, port: number): YamlMap {
  return {
    admin: { address: { socket_address: { address: "127.0.0.1", port_value: 9901 } } },
    static_resources: {
      listeners: [
        {
          name: "ingress",
          address: { socket_address: { address: "0.0.0.0", port_value: port } },
          filter_chains: [
            {
              filters: [
                {
                  name: "envoy.filters.network.http_connection_manager",
                  typed_config: {
                    "@type": "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager",
                    stat_prefix: "ingress",
                    use_remote_address: true,
                    xff_num_trusted_hops: 0,
                    normalize_path: true,
                    merge_slashes: true,
                    path_with_escaped_slashes_action: "UNESCAPE_AND_FORWARD",
                    route_config: {
                      name: "all",
                      virtual_hosts: [
                        {
                          name: "all",
                          domains: ["*"],
                          routes: [
                            { match: { path: "/auth/login" }, direct_response: { status: 200, body: { inline_string: "login\n" } } },
                            { match: { prefix: "/" }, direct_response: { status: 200, body: { inline_string: "routed\n" } } },
                          ],
                        },
                      ],
                    },
                    http_filters: [
                      { name: "envoy.filters.http.lua", typed_config: filters.lua },
                      { name: "envoy.filters.http.local_ratelimit", typed_config: filters.rateLimit },
                      { name: "envoy.filters.http.router", typed_config: { "@type": "type.googleapis.com/envoy.extensions.filters.http.router.v3.Router" } },
                    ],
                  },
                },
              ],
            },
          ],
        },
      ],
    },
  };
}
