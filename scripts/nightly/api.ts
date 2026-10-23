// The smoke test's API check through the ingress (Laysi/trustiot-v2#65): the release runs the
// identity and assets services built from the commit, and the check calls them the way the SPA
// will, through the ingress gateway with a session cookie. A platform administrator creates a
// tenant (POST /api/identity/v1/tenants, 201); the identity service writes its
// identity.tenant_created event in the same transaction, the assets service consumes it, and a
// member of the new tenant then reads the tenant from the assets service (GET
// /api/assets/v1/tenant, 200 with the same ID). Without a cookie the ingress answers 401, and the
// administrator, who has no tenant, gets 403 from the assets service.
//
// The telemetry-query service (Laysi/trustiot-v2#539) consumes the same event and creates the
// tenant's read-only ClickHouse account. The member then reads the latest values of a device
// that does not exist through the ingress (GET /api/telemetry/v1/devices/<id>/latest): 404
// telemetry.account_not_ready until the account is ready, then 200 with no points; without a
// cookie 401, and the administrator gets 403. The account itself is looked up in ClickHouse's
// system.users and system.role_grants, as the bundled ClickHouse's admin inside its pod: the
// password stays in the pod, and only the account's name and role reach the log.
//
// Then the edges of the MS0 completion criteria (Laysi/trustiot-v2#77), with the real services:
// a stolen identity header, a genuine token the identity service signed for the tenant's member
// (taken from its ext_authz answer through a port-forward that bypasses the ingress), which the
// assets service accepts when it reaches it directly (checked through another such port-forward),
// never gets through the ingress: without a cookie the gateway answers 401, and with the
// administrator's cookie the assets service sees only the administrator's token (403). The
// signing keys are generated in the cluster (Laysi/trustiot-v2#404) and never leave it: the check
// reads no private key, it replays what an attacker who stole a token would hold. A Host
// with a port, and spellings of the path that the mesh normalizes, still reach the assets service
// with a token addressed to it (200 with the tenant), so the identity service resolves the
// audience from the Host and path the gateway hands it, not from what the client wrote.
//
// The k3d cluster has no mail relay for the email login (Laysi/trustiot-v2#103), so the check
// makes its two sessions the way the service's own tests do: rows in identity.users,
// identity.memberships and identity.sessions,
// written as the database superuser over the Postgres pod's local socket. Only the SHA-256 of a
// session secret reaches the database and the SQL; the secret itself lives in this process and is
// never logged. The cluster and its sessions are thrown away after the run.
import type { Subprocess } from "bun";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { ConnectionError, sendRaw } from "../../deploy/chart/tests/e2e/raw-client.ts";
import type { RawEndpoint, RawRequest, RawResponse } from "../../deploy/chart/tests/e2e/raw-client.ts";
import { freePort } from "../../deploy/chart/tests/e2e/ports.ts";
import { withTunnelRecovery } from "../../deploy/chart/tests/e2e/tunnel-recovery.ts";
import { run } from "./exec.ts";
import type { RunLog } from "./exec.ts";
import { clickhousePodOf, ingressListenerOf, platformSettingsOf, postgresClustersOf, servicePortOf } from "./manifests.ts";
import type { K8sObject } from "./manifests.ts";
import { smokeProviderNameOf } from "./platform.ts";
import { serviceImageValues } from "./service-image.ts";

/** The identity service's session cookie (services/identity/src/session.rs, SESSION_COOKIE). */
export const SESSION_COOKIE = "__Host-trustiot-session";
/** The identity service's tenant API (services/identity/src/tenants.rs, TENANTS_PATH). */
export const TENANTS_PATH = "/api/identity/v1/tenants";
/** The assets service's tenant API (services/assets/src/tenants.rs, TENANT_PATH). */
export const ASSETS_TENANT_PATH = "/api/assets/v1/tenant";
/** The telemetry-query service's latest values of a device (services/telemetry-query/src/history.rs, LATEST_PATH). */
export const TELEMETRY_LATEST_PATH = "/api/telemetry/v1/devices/{device_id}/latest";
/** Where the identity service answers the ingress's cookie check (services/identity/README.md). */
export const EXT_AUTHZ_PREFIX = "/internal/ext-authz";

/** How long the ingress may take to route the cookie check, and the assets service to consume the event. */
export const ROUTE_LIMIT_MS = 180_000;
export const EVENT_LIMIT_MS = 180_000;
const POLL_MS = 2_000;

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** One entry of the chart's ingress.routes. */
export interface IngressRoute {
  host: string;
  path: string;
  match?: string;
  service: string;
  [key: string]: unknown;
}

/**
 * The cookie routes the check needs: the identity service's tenant API, the assets service's API
 * and the telemetry-query service's history API (Laysi/trustiot-v2#539). The same routes a
 * deployment sets to serve the SPA (deploy/flux/README.md, 「展示叢集」).
 */
export const API_ROUTES: readonly IngressRoute[] = [
  { host: "app", path: "/api/identity", match: "PathPrefix", service: "identity" },
  { host: "app", path: "/api/assets", match: "PathPrefix", service: "assets" },
  { host: "app", path: "/api/telemetry", match: "PathPrefix", service: "telemetry-query" },
];

/**
 * The ingress.routes that the chart's values.yaml and then each of `valuesFiles` (relative to the
 * chart, in `helm -f` order) set: helm replaces a list as a whole, so the last file that sets it wins.
 */
export function routesOfValues(chartDir: string, valuesFiles: string[]): IngressRoute[] {
  let routes: IngressRoute[] = [];
  for (const file of ["values.yaml", ...valuesFiles]) {
    const values = Bun.YAML.parse(readFileSync(join(chartDir, file), "utf8")) as { ingress?: { routes?: unknown } } | null;
    const set = values?.ingress?.routes;
    if (set !== undefined && set !== null) {
      if (!Array.isArray(set)) throw new Error(`${file}: ingress.routes is not a list`);
      routes = set as IngressRoute[];
    }
  }
  return routes;
}

/**
 * The AI gateway's settings the smoke adds (Laysi/trustiot-v2#536): one provider and one role, so
 * the gateway starts on either shape (the SaaS instance values add their own). No key reaches CI:
 * the provider's Secret is one of the dummies smoke.ts makes for every Secret the pods read, which
 * the gateway only checks for printable ASCII at startup. It calls no provider before a request,
 * so its readiness proves that the image and the chart's settings start it, not that a key works.
 */
export const AI_GATEWAY_SMOKE = {
  providers: { anthropic: { apiKeySecret: { name: "trustiot-anthropic" } } },
  roles: { smoke: { models: ["anthropic/claude-smoke"] } },
} as const;

/**
 * The values the smoke adds for the check (after tests/values/smoke.yaml): the identity and assets
 * services from the images the nightly built, the cookie routes to their APIs after the routes the
 * shape's values already set (`routes`, from routesOfValues; helm would otherwise drop them), and
 * the smoke namespace's ext_authz provider (platform.ts). No key: the identity pod's init
 * container generates it in the cluster (Laysi/trustiot-v2#404). Also the AI gateway from its
 * image, with no caller and no route (AI_GATEWAY_SMOKE), and the telemetry-query service from its
 * image (Laysi/trustiot-v2#539), whose Postgres roles and ClickHouse account the migration Job
 * sets up with generated passwords: it needs no value besides its image and its route.
 */
export function apiValues(shape: string, routes: readonly IngressRoute[]): Record<string, unknown> {
  const image = (name: string) => {
    const { digest: _digest, ...values } = serviceImageValues(name);
    return values;
  };
  for (const route of API_ROUTES) {
    const taken = routes.find((existing) => existing.host === route.host && existing.path === route.path);
    if (taken !== undefined) {
      throw new Error(`the shape's values already route ${route.host} ${route.path} to ${taken.service}; the API check needs it for ${route.service}`);
    }
  }
  return {
    services: {
      identity: { enabled: true, image: image("identity") },
      assets: { enabled: true, image: image("assets") },
      "ai-gateway": { enabled: true, image: image("ai-gateway") },
      "telemetry-query": { enabled: true, image: image("telemetry-query") },
    },
    ai: AI_GATEWAY_SMOKE,
    ingress: {
      authzProvider: smokeProviderNameOf(shape),
      routes: [...routes, ...API_ROUTES],
    },
  };
}

/** The identity header of decision 27, which only the ingress may set (services read it, crates/service-kit). */
export const IDENTITY_HEADER = "x-internal-identity";

/**
 * A genuine internal identity token of `session`'s user for the assets service: what the identity
 * service hands the ingress for the member's GET of the tenant, asked for at its ext_authz endpoint
 * past the ingress (`sendToIdentity`). It stands for a token an attacker stole: signed with the
 * deployment's own key, valid for a minute. Throws, without the token, unless the answer is 200
 * with exactly one identity header.
 */
export async function stealIdentityToken(
  sendToIdentity: (request: RawRequest) => Promise<RawResponse>,
  appHost: string,
  session: TestSession,
): Promise<string> {
  const answer = await sendToIdentity({
    target: `${EXT_AUTHZ_PREFIX}${ASSETS_TENANT_PATH}`,
    hostHeader: appHost,
    headers: [["cookie", `${SESSION_COOKIE}=${session.cookie}`]],
  });
  const tokens = answer.headers[IDENTITY_HEADER] ?? [];
  if (answer.status !== 200 || tokens.length !== 1 || tokens[0]!.split(".").length !== 3) {
    throw new Error(
      `the identity service's ext_authz check for the member answered ${answer.status} with ${tokens.length} identity header(s); ` +
        "expected 200 with one token, or the stolen-header checks below prove nothing",
    );
  }
  return tokens[0]!;
}

/** A session the check makes: the cookie value it sends, and the hash the identity service looks up. */
export interface TestSession {
  userId: string;
  sessionId: string;
  /** The 256-bit secret in base64url without padding, as the cookie carries it. Never logged. */
  cookie: string;
  /** SHA-256 of the secret, hex: what identity.sessions.token_hash holds. */
  tokenHash: string;
}

export function newSession(): TestSession {
  const secret = crypto.getRandomValues(new Uint8Array(32));
  return {
    userId: crypto.randomUUID(),
    sessionId: crypto.randomUUID(),
    cookie: Buffer.from(secret).toString("base64url"),
    tokenHash: new Bun.CryptoHasher("sha256").update(secret).digest("hex"),
  };
}

function uuid(value: string): string {
  if (!UUID.test(value)) throw new Error(`not a UUID: ${JSON.stringify(value)}`);
  return value;
}

function sha256Hex(value: string): string {
  if (!/^[0-9a-f]{64}$/.test(value)) throw new Error("not a SHA-256 in hex");
  return value;
}

/** SQL for a platform administrator without a tenant and a session of theirs, valid for an hour. */
export function adminSessionSql(session: TestSession): string {
  return [
    "BEGIN;",
    `INSERT INTO identity.users (id, platform_admin) VALUES ('${uuid(session.userId)}', true);`,
    "INSERT INTO identity.sessions (id, token_hash, user_id, expires_at)",
    `  VALUES ('${uuid(session.sessionId)}', '\\x${sha256Hex(session.tokenHash)}', '${uuid(session.userId)}', now() + interval '1 hour');`,
    "COMMIT;",
    "",
  ].join("\n");
}

/** SQL for a viewer of `tenantId` and a session of theirs acting in that tenant, valid for an hour. */
export function memberSessionSql(session: TestSession, tenantId: string): string {
  return [
    "BEGIN;",
    `INSERT INTO identity.users (id) VALUES ('${uuid(session.userId)}');`,
    `INSERT INTO identity.memberships (tenant_id, user_id, tenant_role) VALUES ('${uuid(tenantId)}', '${uuid(session.userId)}', 'viewer');`,
    "INSERT INTO identity.sessions (id, token_hash, user_id, active_tenant_id, expires_at)",
    `  VALUES ('${uuid(session.sessionId)}', '\\x${sha256Hex(session.tokenHash)}', '${uuid(session.userId)}', '${uuid(tenantId)}', now() + interval '1 hour');`,
    "COMMIT;",
    "",
  ].join("\n");
}

/** What the check needs from the cluster; tests replace it, so no cluster is needed. */
export interface ApiCheckDeps {
  /** Runs SQL as the database superuser. */
  psql: (sql: string) => Promise<void>;
  /** Sends one request to the ingress gateway. */
  send: (request: RawRequest) => Promise<RawResponse>;
  log: (line: string) => void;
  sleep?: (ms: number) => Promise<void>;
  now?: () => number;
  limits?: { routeMs?: number; eventMs?: number; pollMs?: number };
}

/** The string field `field` of an ErrorBody, or undefined when the body is not one. */
function errorField(body: string, field: "code" | "message_key"): string | undefined {
  try {
    const parsed: unknown = JSON.parse(body);
    const value = typeof parsed === "object" && parsed !== null ? (parsed as Record<string, unknown>)[field] : undefined;
    return typeof value === "string" ? value : undefined;
  } catch {
    return undefined;
  }
}

/** The `code` of an ErrorBody, or undefined when the body is not one. */
export function errorCode(body: string): string | undefined {
  return errorField(body, "code");
}

/** The `message_key` of an ErrorBody, or undefined when the body is not one. */
export function errorMessageKey(body: string): string | undefined {
  return errorField(body, "message_key");
}

/** What the telemetry-query service answers until the tenant's account is ready (history.rs, ACCOUNT_NOT_READY). */
export const ACCOUNT_NOT_READY = "telemetry.account_not_ready";

function summary(response: RawResponse): string {
  return `${response.status} ${response.body.slice(0, 200).replaceAll("\n", " ")}`;
}

/** What checkTenantApi made, for the checks that follow it. */
export interface TenantApiResult {
  /** The report line. */
  detail: string;
  tenantId: string;
  /** The platform administrator without a tenant. */
  admin: TestSession;
  /** The viewer acting in the new tenant. */
  member: TestSession;
}

/**
 * Sends `request` through `deps.send` until `done` accepts the answer, `pending` rejects it (fails
 * at once), or `limitMs` passes (fails naming `what` and the last answer). A lost connection
 * counts as pending.
 */
async function pollUntil(
  deps: Pick<ApiCheckDeps, "send" | "sleep" | "now" | "limits">,
  what: string,
  limitMs: number,
  request: RawRequest,
  done: (r: RawResponse) => boolean,
  pending: (r: RawResponse) => boolean,
): Promise<{ response: RawResponse; waitedMs: number }> {
  const sleep = deps.sleep ?? Bun.sleep;
  const now = deps.now ?? Date.now;
  const pollMs = deps.limits?.pollMs ?? POLL_MS;
  const started = now();
  let last = "no response yet";
  for (;;) {
    try {
      const response = await deps.send(request);
      if (done(response)) return { response, waitedMs: now() - started };
      if (!pending(response)) throw new Error(`${what}: unexpected answer ${summary(response)}`);
      last = summary(response);
    } catch (error) {
      if (!(error instanceof ConnectionError)) throw error;
      last = error.message;
    }
    if (now() - started >= limitMs) throw new Error(`timed out after ${Math.round(limitMs / 1000)} s waiting for ${what}; last answer: ${last}`);
    await sleep(pollMs);
  }
}

/**
 * The check itself. `appHost` is hosts.app, where the cookie routes are; `name` is the tenant's
 * name. Returns the report line and what it made; throws with what was awaited or what differed.
 */
export async function checkTenantApi(deps: ApiCheckDeps, appHost: string, name: string): Promise<TenantApiResult> {
  const routeMs = deps.limits?.routeMs ?? ROUTE_LIMIT_MS;
  const eventMs = deps.limits?.eventMs ?? EVENT_LIMIT_MS;
  const get = (session?: TestSession): RawRequest => ({
    target: ASSETS_TENANT_PATH,
    hostHeader: appHost,
    headers: session === undefined ? [] : [["cookie", `${SESSION_COOKIE}=${session.cookie}`]],
  });
  const poll = (what: string, limitMs: number, request: RawRequest, done: (r: RawResponse) => boolean, pending: (r: RawResponse) => boolean) =>
    pollUntil(deps, what, limitMs, request, done, pending);

  const admin = newSession();
  await deps.psql(adminSessionSql(admin));
  deps.log(`made a platform administrator ${admin.userId} with a session`);

  // 1. The gateway has the routes and the cookie check: the administrator's cookie passes the
  //    identity service, the token reaches the assets service, which answers 403 (no tenant).
  //    Before that the gateway answers 404 (no route yet), 503 (ext_authz not reachable yet) or
  //    403 from Istio's own policy, none of which carries the ErrorBody code.
  const routed = await poll(
    "the ingress to pass the administrator's cookie to the assets service (403 permission_denied)",
    routeMs,
    get(admin),
    (r) => r.status === 403 && errorCode(r.body) === "permission_denied",
    () => true,
  );
  deps.log(`the ingress routes the cookie check after ${Math.round(routed.waitedMs / 1000)} s`);

  // 2. No cookie: the identity service rejects the request at the gateway.
  const anonymous = await deps.send(get());
  if (anonymous.status !== 401 || errorCode(anonymous.body) !== "unauthenticated") {
    throw new Error(`without a cookie the ingress answered ${summary(anonymous)}, expected 401 unauthenticated`);
  }

  // 3. The administrator creates a tenant; sent once, since a retry could create a second one.
  const created = await deps.send({
    target: TENANTS_PATH,
    method: "POST",
    hostHeader: appHost,
    headers: [
      ["cookie", `${SESSION_COOKIE}=${admin.cookie}`],
      ["content-type", "application/json"],
    ],
    body: JSON.stringify({ name }),
  });
  if (created.status !== 201) throw new Error(`creating a tenant answered ${summary(created)}, expected 201`);
  const tenant = JSON.parse(created.body) as { tenant_id?: unknown; name?: unknown };
  if (typeof tenant.tenant_id !== "string" || !UUID.test(tenant.tenant_id) || tenant.name !== name) {
    throw new Error(`creating a tenant answered 201 with ${created.body.slice(0, 200)}`);
  }
  const tenantId = tenant.tenant_id;
  deps.log(`created tenant ${tenantId} through the ingress`);

  // 4. A member of the new tenant reads it from the assets service once it consumed the event:
  //    404 until then (or 503 while a dependency is briefly unavailable), then 200 with that ID.
  const member = newSession();
  await deps.psql(memberSessionSql(member, tenantId));
  const served = await poll(
    `the assets service to serve tenant ${tenantId} to its member (200)`,
    eventMs,
    get(member),
    (r) => r.status === 200,
    (r) => (r.status === 404 && errorCode(r.body) === "not_found") || r.status === 503,
  );
  const body = JSON.parse(served.response.body) as { tenant_id?: unknown };
  if (body.tenant_id !== tenantId) throw new Error(`the assets service answered 200 with ${served.response.body.slice(0, 200)}, not tenant ${tenantId}`);

  const detail =
    `POST ${TENANTS_PATH} as a platform administrator: 201, tenant ${tenantId}; ` +
    `GET ${ASSETS_TENANT_PATH} as its member: 200 with that tenant after ${Math.round(served.waitedMs / 1000)} s; ` +
    "without a cookie: 401 unauthenticated; the administrator without a tenant: 403 permission_denied";
  return { detail, tenantId, admin, member };
}

/** TELEMETRY_LATEST_PATH for `device`. */
export function telemetryLatestPath(device: string): string {
  return TELEMETRY_LATEST_PATH.replace("{device_id}", uuid(device));
}

/**
 * The telemetry-query service's history API through the ingress (Laysi/trustiot-v2#539), after
 * checkTenantApi made the tenant and its member: the latest values of a device that does not exist
 * (a random ID). The administrator without a tenant gets 403 from the service once the route and
 * the cookie check reach it, a request without a cookie gets 401 at the gateway, and the member
 * gets 404 telemetry.account_not_ready (or 503 while a dependency is briefly unavailable) until
 * the service consumed the tenant's identity.tenant_created event and created and checked its
 * ClickHouse account, then 200 with the device and no points. The 200 runs a real query as the
 * tenant's account. Returns the report line; throws with what was awaited or what differed.
 */
export async function checkTelemetryApi(deps: Omit<ApiCheckDeps, "psql">, appHost: string, made: TenantApiResult): Promise<string> {
  const routeMs = deps.limits?.routeMs ?? ROUTE_LIMIT_MS;
  const eventMs = deps.limits?.eventMs ?? EVENT_LIMIT_MS;
  const device = crypto.randomUUID();
  const target = telemetryLatestPath(device);
  const get = (session?: TestSession): RawRequest => ({
    target,
    hostHeader: appHost,
    headers: session === undefined ? [] : [["cookie", `${SESSION_COOKIE}=${session.cookie}`]],
  });

  // 1. The route and the cookie check reach the service: the administrator has no tenant (403).
  //    Before that the gateway may answer 404, 503 or Istio's own 403, none with the ErrorBody code.
  //    A success for a user without a tenant is a broken check, so it fails at once.
  const routed = await pollUntil(
    deps,
    "the ingress to pass the administrator's cookie to the telemetry-query service (403 permission_denied)",
    routeMs,
    get(made.admin),
    (r) => r.status === 403 && errorCode(r.body) === "permission_denied",
    (r) => r.status < 200 || r.status >= 300,
  );
  deps.log(`the ingress routes ${TELEMETRY_LATEST_PATH} after ${Math.round(routed.waitedMs / 1000)} s`);

  // 2. No cookie: the identity service rejects the request at the gateway.
  const anonymous = await deps.send(get());
  if (anonymous.status !== 401 || errorCode(anonymous.body) !== "unauthenticated") {
    throw new Error(`${target} without a cookie answered ${summary(anonymous)}, expected 401 unauthenticated`);
  }

  // 3. The member, once the tenant's ClickHouse account is ready.
  const served = await pollUntil(
    deps,
    `the telemetry-query service to answer tenant ${made.tenantId}'s member with its ClickHouse account (200)`,
    eventMs,
    get(made.member),
    (r) => r.status === 200,
    (r) => (r.status === 404 && errorCode(r.body) === "not_found" && errorMessageKey(r.body) === ACCOUNT_NOT_READY) || r.status === 503,
  );
  let body: { device_id?: unknown; earliest?: unknown; points?: unknown };
  try {
    body = JSON.parse(served.response.body) as typeof body;
  } catch {
    throw new Error(`${target} answered 200 with ${served.response.body.slice(0, 200)}, which is not JSON`);
  }
  if (body.device_id !== device || typeof body.earliest !== "string" || !Array.isArray(body.points) || body.points.length !== 0) {
    throw new Error(`${target} answered 200 with ${served.response.body.slice(0, 200)}, expected device ${device} with no points`);
  }
  return (
    `GET ${TELEMETRY_LATEST_PATH} of a device that does not exist as the tenant's member: 200 with no points after ${Math.round(served.waitedMs / 1000)} s; ` +
    "without a cookie: 401 unauthenticated; the administrator without a tenant: 403 permission_denied"
  );
}

/** Prefix of every tenant's ClickHouse account (services/telemetry-query/src/accounts.rs, ACCOUNT_PREFIX). */
export const TENANT_ACCOUNT_PREFIX = "trustiot_tenant_account_";
/** The one role of a tenant's account (services/ingest/migrations/clickhouse/0004_access.sql). */
export const TENANT_READER_ROLE = "trustiot_tenant_reader";

/** The ClickHouse account of `tenantId`: the prefix and the ID's 32 hex digits (accounts.rs, account_name). */
export function tenantAccountName(tenantId: string): string {
  return `${TENANT_ACCOUNT_PREFIX}${uuid(tenantId).replaceAll("-", "")}`;
}

/**
 * The account as the admin sees it: its name and default roles, joined into plain text so the
 * answer does not depend on how TSV writes an array. No password or hash column.
 */
export const ACCOUNT_USER_SQL =
  "SELECT name, arrayStringConcat(default_roles_list, ',') FROM system.users WHERE name = {user:String} FORMAT TSV";
/**
 * The roles granted to the account, and whether it may pass them on (0 or 1, whatever the column's
 * type). system.role_grants names a user grantee in `user_name` and a role grantee in `role_name`
 * (https://clickhouse.com/docs/operations/system-tables/role_grants); tools/migrate/tests/clickhouse.rs
 * filters on `user_name` the same way against the pinned ClickHouse 26.8.
 */
export const ACCOUNT_ROLES_SQL =
  "SELECT granted_role_name, toUInt8(with_admin_option) FROM system.role_grants WHERE user_name = {user:String} FORMAT TSV";

/**
 * What differs between the account the service creates (services/telemetry-query/README.md,
 * 「租戶帳號」) and what ClickHouse answered to ACCOUNT_USER_SQL (`user`) and ACCOUNT_ROLES_SQL
 * (`roles`): the account exists with the tenant role as its only default role, and holds that
 * role only, without the right to pass it on. Empty when they agree.
 */
export function tenantAccountProblems(account: string, user: string, roles: string): string[] {
  const problems: string[] = [];
  const userRows = user.split("\n").filter((line) => line !== "");
  const roleRows = roles.split("\n").filter((line) => line !== "");
  if (userRows.length === 0) problems.push(`ClickHouse has no user ${account}`);
  else if (userRows.length !== 1 || userRows[0] !== `${account}\t${TENANT_READER_ROLE}`) {
    problems.push(`ClickHouse user ${account} is ${JSON.stringify(userRows)}, expected the default role ${TENANT_READER_ROLE} only`);
  }
  if (userRows.length > 0 && (roleRows.length !== 1 || roleRows[0] !== `${TENANT_READER_ROLE}\t0`)) {
    problems.push(`ClickHouse user ${account} holds the roles ${JSON.stringify(roleRows)}, expected ${TENANT_READER_ROLE} without admin option only`);
  }
  return problems;
}

/**
 * The tenant's ClickHouse account exists, with the tenant role as its only default role and only
 * role, after checkTelemetryApi saw the service answer as that account. Its settings profile and
 * tenant setting are not checked here: the service checks them itself (accounts.rs,
 * Accounts::verify) before it records the account and answers 200. `query` runs SQL with the
 * account's name as the parameter `user` and returns ClickHouse's TSV. Returns the report line.
 */
export async function checkTenantAccount(query: (sql: string, user: string) => Promise<string>, tenantId: string): Promise<string> {
  const account = tenantAccountName(tenantId);
  const problems = tenantAccountProblems(account, await query(ACCOUNT_USER_SQL, account), await query(ACCOUNT_ROLES_SQL, account));
  if (problems.length > 0) throw new Error(problems.join("; "));
  return `ClickHouse user ${account} of tenant ${tenantId} exists with the default role ${TENANT_READER_ROLE}, granted without admin option`;
}

/**
 * Spellings of the assets service's tenant path that the mesh's path normalization
 * (deploy/istio/istiod-values.yaml, DECODE_AND_MERGE_SLASHES) turns into ASSETS_TENANT_PATH. The
 * last one starts on the identity service's own prefix: the gateway routes it to the assets
 * service, so the identity service must address the token from the normalized path too, or the
 * assets service would refuse it (401 wrong audience).
 */
export const NORMALIZED_TENANT_PATHS = [
  "//api/assets/v1/tenant",
  "/api/assets//v1/tenant",
  "/api/assets/v1/./tenant",
  "/api/assets/v1%2ftenant",
  "/api%2Fassets/v1/tenant",
  "/api/identity/../assets/v1/tenant",
] as const;

/** What checkIngressEdges needs besides the gateway: the assets service without the ingress, and a stolen token. */
export interface EdgeCheckDeps {
  /** Sends one request to the ingress gateway. */
  send: (request: RawRequest) => Promise<RawResponse>;
  /** Sends one request straight to the assets service, past the ingress and the mesh's policies. */
  sendToAssets: (request: RawRequest) => Promise<RawResponse>;
  /** A fresh genuine token of the tenant's member for the assets service (stealIdentityToken). */
  steal: () => Promise<string>;
  log: (line: string) => void;
}

/**
 * The completion criteria's edges with the real services (Laysi/trustiot-v2#77), after
 * checkTenantApi: `listenerPort` is the Gateway listener's port. Returns the report line; throws
 * naming the case that differed.
 */
export async function checkIngressEdges(deps: EdgeCheckDeps, appHost: string, listenerPort: number, made: TenantApiResult): Promise<string> {
  const { tenantId, admin, member } = made;
  const cookie = (session: TestSession): [string, string] => ["cookie", `${SESSION_COOKIE}=${session.cookie}`];
  const stolen = async (): Promise<[string, string]> => [IDENTITY_HEADER, await deps.steal()];
  const servesTenant = (response: RawResponse): boolean => {
    if (response.status !== 200) return false;
    try {
      return (JSON.parse(response.body) as { tenant_id?: unknown }).tenant_id === tenantId;
    } catch {
      return false;
    }
  };

  // 1. The stolen header is one the assets service accepts: sent past the ingress, it serves the tenant.
  const direct = await deps.sendToAssets({ target: ASSETS_TENANT_PATH, hostHeader: appHost, headers: [await stolen()] });
  if (!servesTenant(direct)) {
    throw new Error(
      `the assets service, called past the ingress with the stolen identity header, answered ${summary(direct)}; ` +
        "it must accept it (200 with the tenant), or the stolen-header checks below prove nothing",
    );
  }
  deps.log("the assets service accepts the stolen identity header when it reaches it");

  // 2. Through the ingress without a cookie: the gateway refuses it before any service.
  const anonymous = await deps.send({ target: ASSETS_TENANT_PATH, hostHeader: appHost, headers: [await stolen()] });
  if (anonymous.status !== 401 || errorCode(anonymous.body) !== "unauthenticated") {
    throw new Error(`a stolen identity header without a cookie answered ${summary(anonymous)} through the ingress, expected 401 unauthenticated`);
  }

  // 3. With the administrator's cookie: the assets service sees the administrator's token only.
  const replaced = await deps.send({ target: ASSETS_TENANT_PATH, hostHeader: appHost, headers: [cookie(admin), await stolen()] });
  if (replaced.status !== 403 || errorCode(replaced.body) !== "permission_denied") {
    throw new Error(
      `the administrator's cookie with a stolen identity header answered ${summary(replaced)} through the ingress, ` +
        "expected 403 permission_denied (the administrator's own token, without a tenant)",
    );
  }

  // 4. Host with a port (deploy/chart/README.md): the same route and audience as without one.
  const hosts = [`${appHost}:${listenerPort}`, `${appHost}:8443`];
  for (const host of hosts) {
    const response = await deps.send({ target: ASSETS_TENANT_PATH, hostHeader: host, headers: [cookie(member)] });
    if (!servesTenant(response)) throw new Error(`the member's cookie with Host ${host} answered ${summary(response)}, expected 200 with tenant ${tenantId}`);
  }

  // 5. Path spellings the mesh normalizes: the identity service addresses the token from the normalized path.
  for (const target of NORMALIZED_TENANT_PATHS) {
    const response = await deps.send({ target, hostHeader: appHost, headers: [cookie(member)] });
    if (!servesTenant(response)) throw new Error(`the member's cookie on ${target} answered ${summary(response)}, expected 200 with tenant ${tenantId}`);
  }

  return (
    "a stolen identity header (the member's genuine token) that the assets service accepts directly: 401 unauthenticated through the ingress without a cookie, " +
    "403 permission_denied with the administrator's cookie; " +
    `the member's cookie with Host ${hosts.map((host) => host.slice(appHost.length)).join(" and ")}: 200 with the tenant; ` +
    `on ${NORMALIZED_TENANT_PATHS.length} path spellings the mesh normalizes: 200 with the tenant`
  );
}

/** Where the check runs: the smoke cluster and its log. */
export interface ApiCheckContext {
  kubeconfig: string;
  kubectl: string;
  log: RunLog;
}

/**
 * Runs checkTenantApi, checkTelemetryApi, checkTenantAccount and checkIngressEdges against the
 * release in `namespace`: psql through `kubectl exec` in the CloudNativePG primary, ClickHouse
 * queries through `kubectl exec` in the bundled ClickHouse's pod, requests through `kubectl
 * port-forward` to the ingress gateway's Service (`ca`: the certificate the smoke made for an
 * HTTPS listener).
 */
export async function runApiCheck(
  ctx: ApiCheckContext,
  namespace: string,
  objects: K8sObject[],
  ca: string | undefined,
  name: string,
): Promise<string> {
  const env = { KUBECONFIG: ctx.kubeconfig };
  const settings = platformSettingsOf(objects);
  const appHost = settings.TRUSTIOT_APP_HOST;
  const database = settings.TRUSTIOT_POSTGRES_DATABASE;
  if (appHost === undefined || database === undefined) throw new Error("the platform settings lack TRUSTIOT_APP_HOST or TRUSTIOT_POSTGRES_DATABASE");
  const clusters = postgresClustersOf(objects);
  if (clusters.length !== 1) throw new Error(`the release should have one CloudNativePG Cluster, found ${clusters.length}`);
  const status = await run(ctx.log, [ctx.kubectl, "get", "--namespace", namespace, `cluster.postgresql.cnpg.io/${clusters[0]}`, "--output", "jsonpath={.status.currentPrimary}"], { env, timeoutMs: 60_000 });
  const primary = status.stdout.trim();
  if (primary === "") throw new Error(`Cluster ${clusters[0]} has no current primary`);
  const listener = ingressListenerOf(objects);
  if (listener.tls && ca === undefined) throw new Error(`Gateway ${listener.gateway} listens with TLS, but the smoke made no certificate for it`);
  const assets = servicePortOf(objects, "assets", "http");
  const clickhousePod = clickhousePodOf(objects);
  const identity = servicePortOf(objects, "identity", "http");

  // SQL on stdin; the output (row counts only) and the SQL stay out of the log.
  const psql = async (sql: string) => {
    await run(
      ctx.log,
      [ctx.kubectl, "exec", "--namespace", namespace, "-i", primary, "-c", "postgres", "--", "psql", "--no-psqlrc", "--quiet", "--tuples-only", "--no-align", "--set", "ON_ERROR_STOP=1", "--dbname", database],
      { env, stdin: sql, timeoutMs: 60_000, quiet: true },
    );
  };

  // SQL as the bundled ClickHouse's admin, in its pod: the client reads the password from the
  // file the init container wrote (CLICKHOUSE_PASSWORD_FILE), so it never leaves the pod. The SQL
  // and the account name are arguments; the answer holds names only and stays out of the log.
  const clickhouse = async (sql: string, user: string): Promise<string> => {
    const script = 'exec clickhouse-client --user "$CLICKHOUSE_USER" --password "$(cat "$CLICKHOUSE_PASSWORD_FILE")" --param_user="$1" --query "$2"';
    const result = await run(
      ctx.log,
      [ctx.kubectl, "exec", "--namespace", namespace, clickhousePod, "-c", "clickhouse", "--", "bash", "-c", script, "clickhouse-query", user, sql],
      { env, timeoutMs: 60_000, quiet: true },
    );
    return result.stdout;
  };

  const gateway = new PortForward(ctx, namespace, `service/${listener.gateway}-istio`, listener.port, listener.tls ? { ca } : undefined);
  // The assets service without the ingress: kubectl port-forward enters its pod, so the request
  // passes neither the gateway nor the mesh's policies, only the service's own token check.
  const direct = new PortForward(ctx, namespace, `service/${assets.service}`, assets.port, undefined);
  // The identity service without the ingress, for a genuine token to steal: its ext_authz endpoint
  // is under /internal, which the ingress never routes.
  const authz = new PortForward(ctx, namespace, `service/${identity.service}`, identity.port, undefined);
  try {
    await gateway.open();
    const made = await checkTenantApi({ psql, send: (request) => gateway.send(request), log: (line) => ctx.log.line(line) }, appHost, name);
    const telemetry = await checkTelemetryApi({ send: (request) => gateway.send(request), log: (line) => ctx.log.line(line) }, appHost, made);
    const account = await checkTenantAccount(clickhouse, made.tenantId);
    ctx.log.line(account);
    await direct.open();
    await authz.open();
    const edges = await checkIngressEdges(
      {
        send: (request) => gateway.send(request),
        sendToAssets: (request) => direct.send(request),
        steal: () => stealIdentityToken((request) => authz.send(request), appHost, made.member),
        log: (line) => ctx.log.line(line),
      },
      appHost,
      listener.port,
      made,
    );
    return `${made.detail}; ${telemetry}; ${account}; ${edges}`;
  } finally {
    gateway.close();
    direct.close();
    authz.close();
  }
}

/**
 * A `kubectl port-forward` to `target` on a free local port, reopened when it is lost while a GET
 * is in flight (tunnel-recovery.ts). `tls`: the target speaks HTTPS with this CA.
 */
class PortForward {
  private tunnel: Subprocess | undefined;
  private endpoint: RawEndpoint | undefined;

  constructor(
    private readonly ctx: ApiCheckContext,
    private readonly namespace: string,
    private readonly target: string,
    private readonly remotePort: number,
    private readonly tls: { ca: string | undefined } | undefined,
  ) {}

  async open(): Promise<void> {
    this.tunnel?.kill();
    const port = await freePort();
    const args = [this.ctx.kubectl, "port-forward", "--namespace", this.namespace, this.target, `${port}:${this.remotePort}`];
    this.ctx.log.line(`$ ${args.join(" ")}`);
    const tunnel = Bun.spawn(args, { env: { ...process.env, KUBECONFIG: this.ctx.kubeconfig }, stdout: "ignore", stderr: "ignore" });
    this.tunnel = tunnel;
    const ca = this.tls?.ca;
    this.endpoint = { host: "127.0.0.1", port, tls: this.tls !== undefined, ...(ca === undefined ? {} : { ca }) };
    const started = Date.now();
    for (;;) {
      try {
        const socket = await Bun.connect({ hostname: "127.0.0.1", port, socket: { data() {} } });
        socket.end();
        return;
      } catch {
        if (tunnel.exitCode !== null) throw new Error(`kubectl port-forward to ${this.target} exited with ${tunnel.exitCode}`);
        if (Date.now() - started > 60_000) throw new Error(`timed out after 60 s waiting for kubectl port-forward to ${this.target} to listen`);
        await Bun.sleep(500);
      }
    }
  }

  send(request: RawRequest): Promise<RawResponse> {
    const endpoint = this.endpoint;
    if (endpoint === undefined) return Promise.reject(new Error(`the port-forward to ${this.target} is not open`));
    const attempt = () => sendRaw(this.endpoint ?? endpoint, request);
    // Only a GET is sent again after the tunnel was lost: a POST may have reached the service.
    if ((request.method ?? "GET") !== "GET") return attempt();
    return withTunnelRecovery(attempt, { tunnel: () => this.tunnel, reopen: () => this.open(), log: (line) => this.ctx.log.line(line) });
  }

  close(): void {
    this.tunnel?.kill();
  }
}
