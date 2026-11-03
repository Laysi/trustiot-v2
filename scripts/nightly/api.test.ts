import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { ConnectionError } from "../../deploy/chart/tests/e2e/raw-client.ts";
import { COMMAND_TIMEOUT_MS, SLOW_TEST_MS, throwIfTimedOut } from "../../deploy/chart/tests/timing.ts";
import type { RawRequest, RawResponse } from "../../deploy/chart/tests/e2e/raw-client.ts";
import {
  ACCOUNT_NOT_READY,
  ACCOUNT_ROLES_SQL,
  ACCOUNT_USER_SQL,
  accountProblems,
  adminSessionSql,
  API_CHECKS,
  API_ROUTES,
  apiValues,
  ASSETS_TENANT_PATH,
  ASSISTANT_CONVERSATION_PATH,
  ASSISTANT_CONVERSATIONS_PATH,
  ASSISTANT_DASHBOARDS_PATH,
  ASSISTANT_DELETED_PATH,
  ASSISTANT_RESTORE_PATH,
  ASSISTANT_ROLE,
  assistantConversationPath,
  assistantRestorePath,
  checkAssistantApi,
  checkAssistantDashboards,
  checkAssistantLogin,
  checkCrossSiteRequests,
  CROSS_SITE_REQUEST,
  DASHBOARD_CHECK_SHAPES,
  FOREIGN_ORIGIN,
  checkGatewayOps,
  checkIngest,
  checkExpectedSession,
  checkIngressEdges,
  checkTelemetryApi,
  checkTenantAccount,
  checkTenantApi,
  errorCode,
  EXPECTED_NO_TENANT,
  EXPECTED_TENANT_HEADER,
  EXPECTED_USER_HEADER,
  EXT_AUTHZ_PREFIX,
  grantsLoggedSql,
  GATEWAY_OPS_ROLES,
  IDENTITY_HEADER,
  SESSION_CHANGED,
  INGEST_ACCOUNT,
  INGEST_ROLE,
  INGEST_ROLES,
  memberSessionSql,
  newSession,
  NORMALIZED_TENANT_PATHS,
  roleSessionsSql,
  routesOfValues,
  runApiChecks,
  SESSION_COOKIE,
  stealIdentityToken,
  TELEMETRY_LATEST_PATH,
  telemetryLatestPath,
  TENANT_ACCOUNT_PREFIX,
  TENANT_READER_ROLE,
  tenantAccountName,
  tenantAccountProblems,
  TENANTS_PATH,
} from "./api.ts";
import type { ApiCheckIo, ApiChecks, AssistantCheckDeps, EdgeCheckDeps, TenantApiResult, Tunnel } from "./api.ts";
import { pinnedTool } from "./exec.ts";
import {
  clickhousePodOf,
  ingressHostsOf,
  ingressListenerOf,
  missingSecrets,
  missingTlsSecrets,
  parseManifests,
  passwordSecretsOf,
  platformSettingsOf,
} from "./manifests.ts";
import type { K8sObject } from "./manifests.ts";
import { serviceImageRef } from "./service-image.ts";
import { discoverShapes, valuesArgs } from "./smoke.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");
const CHART_DIR = join(REPO_ROOT, "deploy", "chart");
const APP = "app.saas.trustiot.test";
const TENANT = "0b6f3a52-3c1e-4c39-9d36-2f4f7f6a8e10";

describe("the check's sessions", () => {
  test("the cookie carries 256 random bits; the database gets only their SHA-256", () => {
    const session = newSession();
    expect(session.cookie).toMatch(/^[A-Za-z0-9_-]{43}$/);
    const secret = Buffer.from(session.cookie, "base64url");
    expect(secret.length).toBe(32);
    expect(session.tokenHash).toBe(new Bun.CryptoHasher("sha256").update(secret).digest("hex"));
    expect(newSession().cookie).not.toBe(session.cookie);
    for (const sql of [adminSessionSql(session), memberSessionSql(session, TENANT)]) {
      expect(sql).toContain(`'\\x${session.tokenHash}'`);
      expect(sql).not.toContain(session.cookie);
    }
  });

  test("a session for the administrator the bootstrap made, without a tenant, and a viewer acting in the new tenant", () => {
    const session = newSession();
    const admin = adminSessionSql(session);
    // The user is the bootstrap's (Laysi/trustiot-v2#582): the check never makes a platform
    // administrator with SQL.
    expect(admin).not.toContain("identity.users");
    expect(admin).not.toContain("platform_admin");
    expect(admin).toContain(`'${session.userId}', now() + interval '1 hour');`);
    expect(admin).not.toContain("memberships");
    expect(admin).not.toContain("active_tenant_id");
    const member = memberSessionSql(session, TENANT);
    expect(member).toContain(`VALUES ('${TENANT}', '${session.userId}', 'viewer');`);
    expect(member).toContain(`'${session.userId}', '${TENANT}', now() + interval '1 hour');`);
    for (const sql of [admin, member]) {
      expect(sql.trim().startsWith("BEGIN;")).toBe(true);
      expect(sql.trim().endsWith("COMMIT;")).toBe(true);
    }
  });

  test("a member with another tenant role, which only a known role may be (the certs check's tenant administrator)", () => {
    const session = newSession();
    expect(memberSessionSql(session, TENANT, "admin")).toContain(`VALUES ('${TENANT}', '${session.userId}', 'admin');`);
    expect(memberSessionSql(session, TENANT, "admin")).not.toContain("platform_admin");
    expect(() => memberSessionSql(session, TENANT, "owner'); --" as "admin")).toThrow("not a tenant role");
  });

  test("only UUIDs and hashes reach the SQL", () => {
    const session = newSession();
    expect(() => memberSessionSql(session, "x'); DROP TABLE identity.users; --")).toThrow("not a UUID");
    expect(() => adminSessionSql({ ...session, userId: "1" })).toThrow("not a UUID");
    expect(() => adminSessionSql({ ...session, tokenHash: "'" })).toThrow("not a SHA-256");
    expect(() => grantsLoggedSql("x' OR true --")).toThrow("not a UUID");
  });

  test("the operation log query counts the user's bootstrap grants without an address, then all grants", () => {
    const session = newSession();
    const sql = grantsLoggedSql(session.userId);
    expect(sql).toContain(`payload->>'user_id' = '${session.userId}'`);
    expect(sql).toContain("payload->>'via' = 'bootstrap'");
    expect(sql).toContain("position('@' in payload::text) = 0");
    expect(sql).toContain("FROM identity.platform_event_log WHERE event_type = 'identity.platform_admin_granted';");
    // The table and its columns are the identity service's.
    const schema = readFileSync(join(REPO_ROOT, "services", "identity", "migrations", "postgres", "0003_event_log.sql"), "utf8");
    expect(schema).toMatch(/CREATE TABLE identity\.platform_event_log \([\s\S]*?\n    event_type text NOT NULL,[\s\S]*?\n    payload jsonb NOT NULL/);
  });

  test("the tables and columns exist in the identity service's schema", () => {
    // The check writes the rows the service's own tests write; a renamed column fails here first.
    const schema = readFileSync(join(REPO_ROOT, "services", "identity", "migrations", "postgres", "0001_identity.sql"), "utf8");
    const table = (name: string) => {
      const match = new RegExp(`CREATE TABLE identity\\.${name} \\(([\\s\\S]*?)\\n\\);`).exec(schema);
      if (match === null) throw new Error(`no table identity.${name}`);
      return match[1]!;
    };
    const session = newSession();
    for (const sql of [adminSessionSql(session), memberSessionSql(session, TENANT)]) {
      for (const [, name, columns] of sql.matchAll(/INSERT INTO identity\.(\w+) \(([^)]*)\)/g)) {
        const definition = table(name!);
        for (const column of columns!.split(",").map((c) => c.trim())) {
          expect(definition, `identity.${name}.${column}`).toMatch(new RegExp(`^\\s+${column} `, "m"));
        }
      }
    }
    expect(schema).toContain("CHECK (tenant_role IN ('viewer', 'editor', 'admin'))");
    expect(schema).toContain("CHECK (octet_length(token_hash) = 32)");
  });

  test("the cookie name and the paths are the services' own", () => {
    expect(readFileSync(join(REPO_ROOT, "services", "identity", "src", "session.rs"), "utf8")).toContain(
      `pub const SESSION_COOKIE: &str = "${SESSION_COOKIE}";`,
    );
    expect(readFileSync(join(REPO_ROOT, "services", "identity", "src", "tenants.rs"), "utf8")).toContain(`"${TENANTS_PATH}"`);
    expect(readFileSync(join(REPO_ROOT, "services", "assets", "src", "tenants.rs"), "utf8")).toContain(`"${ASSETS_TENANT_PATH}"`);
    const history = readFileSync(join(REPO_ROOT, "services", "telemetry-query", "src", "history.rs"), "utf8");
    expect(history).toContain(`pub const LATEST_PATH: &str = "${TELEMETRY_LATEST_PATH}";`);
    expect(history).toContain(`MessageKey::new("${ACCOUNT_NOT_READY}")`);
    // The assistant's conversation API (Laysi/trustiot-v2#647).
    const conversations = readFileSync(join(REPO_ROOT, "services", "assistant", "src", "api", "conversations.rs"), "utf8");
    for (const [name, path] of [
      ["CONVERSATIONS_PATH", ASSISTANT_CONVERSATIONS_PATH],
      ["CONVERSATION_PATH", ASSISTANT_CONVERSATION_PATH],
      ["RESTORE_PATH", ASSISTANT_RESTORE_PATH],
      ["DELETED_CONVERSATIONS_PATH", ASSISTANT_DELETED_PATH],
    ] as const) {
      expect(conversations, name).toContain(`pub const ${name}: &str = "${path}";`);
    }
    // The assistant's list of saved dashboards (Laysi/trustiot-v2#150).
    const dashboards = readFileSync(join(REPO_ROOT, "services", "assistant", "src", "api", "dashboards.rs"), "utf8");
    expect(dashboards).toContain(`pub const DASHBOARDS_PATH: &str = "${ASSISTANT_DASHBOARDS_PATH}";`);
    // The cross-site refusal's message key (Laysi/trustiot-v2#943), and the assets service's tenant
    // route with GET only, so the same-origin POST there gets 405 (checkCrossSiteRequests).
    expect(readFileSync(join(REPO_ROOT, "services", "identity", "src", "csrf.rs"), "utf8")).toContain(
      `pub const CROSS_SITE_REQUEST: MessageKey = MessageKey::new("${CROSS_SITE_REQUEST}");`,
    );
    expect(readFileSync(join(REPO_ROOT, "services", "assets", "src", "tenants.rs"), "utf8")).toContain(".route(TENANT_PATH, get(get_tenant))");
  });
});

const json = (status: number, body: unknown): RawResponse => ({ status, headers: {}, body: JSON.stringify(body), ms: 1 });
const problem = (status: number, code: string, messageKey = `error.${code}`) => json(status, { code, message_key: messageKey });
/** What the telemetry-query service answers until the tenant's account is ready. */
const notReady = () => problem(404, "not_found", ACCOUNT_NOT_READY);

/** The bootstrap's user in the fakes below. */
const ADMIN_USER = "2c1d0e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f";

/**
 * A gateway that answers from a script per request kind, and records what it was sent; a
 * bootstrap that answers from `bootstraps` (default: created, then already created) and records
 * the addresses; psql that answers the operation log query from `grants` (default `1|1`).
 */
function fakeGateway(
  script: { admin: RawResponse[]; anonymous: RawResponse[]; create: RawResponse[]; member: RawResponse[] },
  options: { bootstraps?: { exitCode: number; userId: string }[]; grants?: string[] } = {},
) {
  const sent: RawRequest[] = [];
  const sql: string[] = [];
  const addresses: string[] = [];
  const bootstraps = [...(options.bootstraps ?? [{ exitCode: 0, userId: ADMIN_USER }, { exitCode: 3, userId: ADMIN_USER }])];
  const grants = [...(options.grants ?? ["1|1"])];
  let clock = 0;
  // The administrator's GETs come before the POST, the member's after it.
  let posted = false;
  const deps = {
    psql: async (statement: string) => {
      sql.push(statement);
      if (!statement.includes("platform_event_log")) return "";
      return grants.length > 1 ? grants.shift()! : (grants[0] ?? "");
    },
    bootstrapAdmin: async (address: string) => {
      addresses.push(address);
      const next = bootstraps.shift();
      if (next === undefined) throw new Error("no bootstrap scripted");
      return { job: `platform-admin-bootstrap-${addresses.length}`, ...next };
    },
    send: async (request: RawRequest) => {
      sent.push(request);
      const cookie = request.headers?.find(([name]) => name === "cookie")?.[1];
      let queue: RawResponse[];
      if (request.method === "POST") {
        posted = true;
        queue = script.create;
      } else if (cookie === undefined) queue = script.anonymous;
      else queue = posted ? script.member : script.admin;
      const next = queue.length > 1 ? queue.shift()! : queue[0];
      if (next === undefined) throw new Error(`no answer scripted for ${request.method ?? "GET"} ${request.target}`);
      if (next.status === 0) throw new ConnectionError("connection refused");
      return next;
    },
    log: () => {},
    sleep: async (ms: number) => {
      clock += ms;
    },
    now: () => clock,
    limits: { routeMs: 20_000, eventMs: 20_000, pollMs: 1_000 },
  };
  return { deps, sent, sql, addresses };
}

const created = json(201, { tenant_id: TENANT, name: "nightly smoke saas", created_at: "2026-10-14T00:00:00Z" });

describe("checkTenantApi", () => {
  test("waits for the routes, then creates a tenant once and waits until its member reads it from the assets service", async () => {
    const gateway = fakeGateway({
      // Not routed yet, then Istio's own denial, then the assets service's answer.
      admin: [{ status: 0, headers: {}, body: "", ms: 0 }, json(404, ""), { status: 403, headers: {}, body: "RBAC: access denied", ms: 1 }, problem(403, "permission_denied")],
      anonymous: [problem(401, "unauthenticated")],
      create: [created],
      member: [problem(404, "not_found"), problem(503, "unavailable"), json(200, { tenant_id: TENANT, created_at: "2026-10-14T00:00:01Z" })],
    });
    const made = await checkTenantApi(gateway.deps, APP, "nightly smoke saas");
    expect(made.detail).toContain(`tenant ${TENANT}`);
    expect(made.detail).toContain("200 with that tenant after 2 s");
    expect(made.tenantId).toBe(TENANT);
    // The administrator is the bootstrap's: two runs, each with its own example.com address, and
    // no SQL makes a platform administrator.
    expect(made.admin.userId).toBe(ADMIN_USER);
    expect(gateway.addresses).toHaveLength(2);
    for (const address of gateway.addresses) expect(address).toMatch(/^nightly-admin-[0-9a-f]{8}@example\.com$/);
    expect(gateway.addresses[0]).not.toBe(gateway.addresses[1]);
    for (const statement of gateway.sql) {
      expect(statement).not.toContain("platform_admin)");
      for (const address of gateway.addresses) expect(statement).not.toContain(address);
    }
    expect(made.detail).toContain(`platform administrator ${ADMIN_USER} from the chart's bootstrap CronJob`);
    // The sessions it made are the ones the following checks use; between them, the operation log.
    expect(gateway.sql[0]).toContain(made.admin.tokenHash);
    expect(gateway.sql[0]).toContain(`'${ADMIN_USER}'`);
    expect(gateway.sql[1]).toBe(grantsLoggedSql(ADMIN_USER));
    expect(gateway.sql[2]).toContain(made.member.tokenHash);
    // One POST, with the administrator's cookie and the name as JSON.
    const posts = gateway.sent.filter((r) => r.method === "POST");
    expect(posts).toHaveLength(1);
    expect(posts[0]).toMatchObject({ target: TENANTS_PATH, hostHeader: APP, body: JSON.stringify({ name: "nightly smoke saas" }) });
    expect(posts[0]?.headers).toContainEqual(["content-type", "application/json"]);
    // Every request goes to the app host; the cookie is the identity service's.
    for (const request of gateway.sent) {
      expect(request.hostHeader).toBe(APP);
      for (const [name, value] of request.headers ?? []) if (name === "cookie") expect(value).toMatch(new RegExp(`^${SESSION_COOKIE}=[A-Za-z0-9_-]{43}$`));
    }
    // The member is made only after the tenant exists, in that tenant.
    expect(gateway.sql).toHaveLength(3);
    expect(gateway.sql[2]).toContain(`'${TENANT}'`);
  });

  test("the bootstrap must make the administrator once, and the operation log must record it once", async () => {
    const script = () => ({
      admin: [problem(403, "permission_denied")],
      anonymous: [problem(401, "unauthenticated")],
      create: [created],
      member: [json(200, { tenant_id: TENANT })],
    });
    const OTHER = "3d2e1f50-6b7c-4d8e-9fa0-1b2c3d4e5f60";
    const failedFirst = fakeGateway(script(), { bootstraps: [{ exitCode: 1, userId: ADMIN_USER }] });
    await expect(checkTenantApi(failedFirst.deps, APP, "nightly smoke saas")).rejects.toThrow("exited with 1, expected 0");
    expect(failedFirst.sql).toEqual([]);
    const twice = fakeGateway(script(), { bootstraps: [{ exitCode: 0, userId: ADMIN_USER }, { exitCode: 0, userId: OTHER }] });
    await expect(checkTenantApi(twice.deps, APP, "nightly smoke saas")).rejects.toThrow(`exited with 0 naming user ${OTHER}, expected 3 naming ${ADMIN_USER}`);
    const otherUser = fakeGateway(script(), { bootstraps: [{ exitCode: 0, userId: ADMIN_USER }, { exitCode: 3, userId: OTHER }] });
    await expect(checkTenantApi(otherUser.deps, APP, "nightly smoke saas")).rejects.toThrow(`naming user ${OTHER}`);
    // The grant reaches the operation log a little later: the check waits for it.
    const late = fakeGateway(script(), { grants: ["0|0", "0|0", "1|1"] });
    await checkTenantApi(late.deps, APP, "nightly smoke saas");
    expect(late.sql.filter((statement) => statement.includes("platform_event_log"))).toHaveLength(3);
    // Two grants: fails at once.
    const doubled = fakeGateway(script(), { grants: ["1|2"] });
    await expect(checkTenantApi(doubled.deps, APP, "nightly smoke saas")).rejects.toThrow("has 2 identity.platform_admin_granted events, expected one");
    // Never recorded: fails at the bound, naming what it waited for.
    const never = fakeGateway(script(), { grants: ["0|0"] });
    await expect(checkTenantApi(never.deps, APP, "nightly smoke saas")).rejects.toThrow(
      `timed out after 20 s waiting for the identity service to record identity.platform_admin_granted of user ${ADMIN_USER}`,
    );
  });

  test("an answer that is neither the expected one nor pending fails at once", async () => {
    const forbidden = fakeGateway({
      admin: [problem(403, "permission_denied")],
      anonymous: [problem(401, "unauthenticated")],
      create: [created],
      member: [problem(403, "permission_denied")],
    });
    await expect(checkTenantApi(forbidden.deps, APP, "nightly smoke saas")).rejects.toThrow("unexpected answer 403");
  });

  test("a missing 401, a failed creation or a wrong tenant fails the check", async () => {
    const base = { admin: [problem(403, "permission_denied")], create: [created], member: [json(200, { tenant_id: TENANT })] };
    await expect(checkTenantApi(fakeGateway({ ...base, anonymous: [problem(403, "permission_denied")] }).deps, APP, "nightly smoke saas")).rejects.toThrow(
      "without a cookie the ingress answered 403",
    );
    const failed = fakeGateway({ ...base, anonymous: [problem(401, "unauthenticated")], create: [problem(503, "unavailable")] });
    await expect(checkTenantApi(failed.deps, APP, "nightly smoke saas")).rejects.toThrow("creating a tenant answered 503");
    // Never sent twice, whatever it answered.
    expect(failed.sent.filter((r) => r.method === "POST")).toHaveLength(1);
    const other = fakeGateway({ ...base, anonymous: [problem(401, "unauthenticated")], member: [json(200, { tenant_id: "1b6f3a52-3c1e-4c39-9d36-2f4f7f6a8e10" })] });
    await expect(checkTenantApi(other.deps, APP, "nightly smoke saas")).rejects.toThrow(`not tenant ${TENANT}`);
  });

  test("gives up after the bound and names what it waited for", async () => {
    const never = fakeGateway({ admin: [json(404, "")], anonymous: [], create: [], member: [] });
    await expect(checkTenantApi(never.deps, APP, "nightly smoke saas")).rejects.toThrow(
      "timed out after 20 s waiting for the ingress to pass the administrator's cookie to the assets service",
    );
    const unconsumed = fakeGateway({
      admin: [problem(403, "permission_denied")],
      anonymous: [problem(401, "unauthenticated")],
      create: [created],
      member: [problem(404, "not_found")],
    });
    await expect(checkTenantApi(unconsumed.deps, APP, "nightly smoke saas")).rejects.toThrow(
      `timed out after 20 s waiting for the assets service to serve tenant ${TENANT} to its member (200); last answer: 404`,
    );
  });

  test("errorCode reads an ErrorBody and nothing else", () => {
    expect(errorCode(JSON.stringify({ code: "not_found" }))).toBe("not_found");
    expect(errorCode("RBAC: access denied")).toBeUndefined();
    expect(errorCode("[]")).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------------------------
// Laysi/trustiot-v2#539: the telemetry-query service's history API and the tenant's ClickHouse account.

describe("checkTelemetryApi", () => {
  const made = (): TenantApiResult => ({ detail: "", tenantId: TENANT, admin: newSession(), member: newSession() });
  /** The latest values the service answers for the device the request names, with no points. */
  const empty = (request: RawRequest) =>
    json(200, { device_id: /\/devices\/([^/]+)\/latest$/.exec(request.target)?.[1], earliest: "2026-10-16T00:00:00Z", points: [] });

  /** A gateway answering by cookie from a script per caller; the last answer of a script repeats. */
  function telemetryGateway(result: TenantApiResult, script: Record<"admin" | "anonymous" | "member", ((request: RawRequest) => RawResponse)[]>) {
    const sent: RawRequest[] = [];
    let clock = 0;
    const deps = {
      send: async (request: RawRequest) => {
        sent.push(request);
        const cookie = request.headers?.find(([name]) => name === "cookie")?.[1];
        const queue = cookie === undefined ? script.anonymous : cookie.endsWith(result.admin.cookie) ? script.admin : script.member;
        const next = queue.length > 1 ? queue.shift()! : queue[0];
        if (next === undefined) throw new Error(`no answer scripted for ${request.target}`);
        const response = next(request);
        if (response.status === 0) throw new ConnectionError("connection refused");
        return response;
      },
      log: () => {},
      sleep: async (ms: number) => {
        clock += ms;
      },
      now: () => clock,
      limits: { routeMs: 20_000, eventMs: 20_000, pollMs: 1_000 },
    };
    return { deps, sent };
  }

  test("waits for the route, then for the tenant's account, and reads a device with no points", async () => {
    const result = made();
    const gateway = telemetryGateway(result, {
      // Not routed yet, then Istio's own denial, then the service's answer for a user without a tenant.
      admin: [() => ({ status: 0, headers: {}, body: "", ms: 0 }), () => json(404, ""), () => ({ status: 403, headers: {}, body: "RBAC: access denied", ms: 1 }), () => problem(403, "permission_denied")],
      anonymous: [() => problem(401, "unauthenticated")],
      member: [notReady, () => problem(503, "unavailable"), empty],
    });
    const detail = await checkTelemetryApi(gateway.deps, APP, result);
    expect(detail).toContain("200 with no points after 2 s");
    expect(detail).toContain("without a cookie: 401 unauthenticated");
    expect(detail).toContain("the administrator without a tenant: 403 permission_denied");
    // One random device for every request, on the app host, with the identity service's cookie.
    const targets = new Set(gateway.sent.map((request) => request.target));
    expect(targets.size).toBe(1);
    const [target] = [...targets];
    expect(target).toMatch(/^\/api\/telemetry\/v1\/devices\/[0-9a-f-]{36}\/latest$/);
    for (const request of gateway.sent) {
      expect(request.hostHeader).toBe(APP);
      expect(request.method ?? "GET").toBe("GET");
    }
    // The administrator until routed, one request without a cookie, then the member.
    const who = gateway.sent.map((request) => {
      const cookie = request.headers?.find(([name]) => name === "cookie")?.[1];
      return cookie === undefined ? "anonymous" : cookie === `${SESSION_COOKIE}=${result.admin.cookie}` ? "admin" : "member";
    });
    expect(who).toEqual(["admin", "admin", "admin", "admin", "anonymous", "member", "member", "member"]);
  });

  test("a missing 401, an unexpected answer or a wrong body fails the check", async () => {
    const result = made();
    const base = { admin: [() => problem(403, "permission_denied")], anonymous: [() => problem(401, "unauthenticated")], member: [empty] };
    await expect(checkTelemetryApi(telemetryGateway(result, { ...base, anonymous: [() => problem(404, "not_found")] }).deps, APP, result)).rejects.toThrow(
      "without a cookie answered 404",
    );
    // The member refused outright: neither the account being made nor a passing outage.
    await expect(checkTelemetryApi(telemetryGateway(result, { ...base, member: [() => problem(403, "permission_denied")] }).deps, APP, result)).rejects.toThrow(
      "unexpected answer 403",
    );
    // A 404 that is not the account being made (another route, a device lookup) does not wait.
    await expect(checkTelemetryApi(telemetryGateway(result, { ...base, member: [() => problem(404, "not_found")] }).deps, APP, result)).rejects.toThrow(
      "unexpected answer 404",
    );
    // The administrator without a tenant served: authorization is broken, so no waiting either.
    await expect(checkTelemetryApi(telemetryGateway(result, { ...base, admin: [empty] }).deps, APP, result)).rejects.toThrow(
      "telemetry-query service (403 permission_denied): unexpected answer 200",
    );
    const wrong = [
      () => json(200, { device_id: "0b6f3a52-3c1e-4c39-9d36-2f4f7f6a8e10", earliest: "2026-10-16T00:00:00Z", points: [] }),
      (request: RawRequest) => ({ ...empty(request), body: JSON.stringify({ ...JSON.parse(empty(request).body), points: [{ point: "p" }] }) }),
      () => ({ status: 200, headers: {}, body: "<html>", ms: 1 }),
    ];
    for (const answer of wrong) {
      await expect(checkTelemetryApi(telemetryGateway(result, { ...base, member: [answer] }).deps, APP, result)).rejects.toThrow("answered 200 with");
    }
  });

  test("gives up after the bound and names the tenant whose account it waited for", async () => {
    const result = made();
    const never = telemetryGateway(result, {
      admin: [() => problem(403, "permission_denied")],
      anonymous: [() => problem(401, "unauthenticated")],
      member: [notReady],
    });
    await expect(checkTelemetryApi(never.deps, APP, result)).rejects.toThrow(
      `timed out after 20 s waiting for the telemetry-query service to answer tenant ${TENANT}'s member with its ClickHouse account (200); last answer: 404`,
    );
  });

  test("the device in the path is a UUID", () => {
    expect(telemetryLatestPath(TENANT)).toBe(`/api/telemetry/v1/devices/${TENANT}/latest`);
    expect(() => telemetryLatestPath("../../identity")).toThrow("not a UUID");
  });
});

describe("checkAssistantDashboards", () => {
  const made = (): TenantApiResult => ({ detail: "", tenantId: TENANT, admin: newSession(), member: newSession() });
  const noStore = { "cache-control": ["no-store"] };
  const list = (body: unknown, headers: Record<string, string[]> = noStore): RawResponse => ({ status: 200, headers, body: JSON.stringify(body), ms: 1 });
  const unauthenticated = (): RawResponse => ({ ...problem(401, "unauthenticated"), headers: noStore });

  /** A gateway answering the member from `member` (the last answer repeats) and a request without a cookie from `anonymous`. */
  function dashboardsGateway(member: (() => RawResponse)[], anonymous: () => RawResponse = unauthenticated) {
    const sent: RawRequest[] = [];
    let clock = 0;
    const deps = {
      send: async (request: RawRequest) => {
        sent.push(request);
        const cookie = request.headers?.find(([name]) => name === "cookie")?.[1];
        const next = cookie === undefined ? anonymous : member.length > 1 ? member.shift()! : member[0]!;
        const response = next();
        if (response.status === 0) throw new ConnectionError("connection refused");
        return response;
      },
      log: () => {},
      sleep: async (ms: number) => {
        clock += ms;
      },
      now: () => clock,
      limits: { routeMs: 20_000, eventMs: 20_000, pollMs: 1_000 },
    };
    return { deps, sent };
  }

  test("waits for the route, then lists the new tenant's dashboards as its member: none, and not to be cached", async () => {
    const result = made();
    const gateway = dashboardsGateway([
      () => ({ status: 0, headers: {}, body: "", ms: 0 }),
      () => json(404, ""),
      () => ({ status: 403, headers: {}, body: "RBAC: access denied", ms: 1 }),
      () => ({ status: 503, headers: {}, body: "no healthy upstream", ms: 1 }),
      () => ({ ...problem(503, "unavailable"), headers: noStore }),
      () => list({ dashboards: [] }),
    ]);
    const detail = await checkAssistantDashboards(gateway.deps, APP, result);
    expect(detail).toBe(
      `GET ${ASSISTANT_DASHBOARDS_PATH} as the tenant's member: 200 {"dashboards":[]} with cache-control: no-store after 5 s; without a cookie: 401 unauthenticated`,
    );
    for (const request of gateway.sent) {
      expect(request.target).toBe(ASSISTANT_DASHBOARDS_PATH);
      expect(request.hostHeader).toBe(APP);
      expect(request.method ?? "GET").toBe("GET");
    }
    const who = gateway.sent.map((request) => request.headers?.find(([name]) => name === "cookie")?.[1]);
    expect(who).toEqual([...Array(6).fill(`${SESSION_COOKIE}=${result.member.cookie}`), undefined]);
  });

  test("an answer of the assistant's own that is not the list fails at once", async () => {
    for (const [status, code] of [[403, "permission_denied"], [401, "unauthenticated"], [400, "invalid_request"], [404, "not_found"]] as const) {
      const gateway = dashboardsGateway([() => ({ ...problem(status, code), headers: noStore }), () => list({ dashboards: [] })]);
      await expect(checkAssistantDashboards(gateway.deps, APP, made())).rejects.toThrow(`unexpected answer ${status}`);
      expect(gateway.sent).toHaveLength(1);
    }
  });

  test("a list that may be cached, is not JSON or is not the new tenant's empty list fails", async () => {
    const run = (answer: RawResponse) => checkAssistantDashboards(dashboardsGateway([() => answer]).deps, APP, made());
    const cacheable: Record<string, string[]>[] = [{}, { "cache-control": ["private"] }, { "cache-control": ["no-store", "max-age=60"] }];
    for (const headers of cacheable) {
      await expect(run(list({ dashboards: [] }, headers))).rejects.toThrow("expected exactly no-store");
    }
    await expect(run({ status: 200, headers: noStore, body: "<html>", ms: 1 })).rejects.toThrow("which is not JSON");
    for (const body of [[], { dashboards: [{ dashboard_id: TENANT }] }, { dashboards: [], next_cursor: "x" }, { dashboards: null }, {}]) {
      await expect(run(list(body))).rejects.toThrow('expected {"dashboards":[]} for a new tenant');
    }
  });

  test("a request without a cookie must get 401 unauthenticated", async () => {
    for (const anonymous of [() => list({ dashboards: [] }), () => ({ ...problem(403, "permission_denied"), headers: noStore }), () => json(401, "")]) {
      await expect(checkAssistantDashboards(dashboardsGateway([() => list({ dashboards: [] })], anonymous).deps, APP, made())).rejects.toThrow(
        "without a cookie answered",
      );
    }
  });

  test("gives up after the bound and names what it waited for", async () => {
    const gateway = dashboardsGateway([() => json(404, "")]);
    await expect(checkAssistantDashboards(gateway.deps, APP, made())).rejects.toThrow(
      `timed out after 20 s waiting for the ingress to pass tenant ${TENANT}'s member to the assistant's dashboard list (200); last answer: 404`,
    );
  });
});

describe("checkAssistantApi", () => {
  const made = (): TenantApiResult => ({ detail: "", tenantId: TENANT, admin: newSession(), member: newSession() });
  type Caller = "anonymous" | "admin" | "owner" | "other";
  type Override = (request: RawRequest, caller: Caller) => RawResponse | undefined;

  /**
   * The gateway and the assistant as the check expects them: a conversation belongs to the cookie
   * that started it, `override` may replace any answer (return undefined to keep it), and the
   * assistant reached directly takes only a token for its own audience. Records every request.
   */
  function assistantFake(result: TenantApiResult, override: Override = () => undefined, options: { directControl?: RawResponse; directWrong?: RawResponse } = {}) {
    const sent: { to: "gateway" | "assistant"; request: RawRequest; caller: Caller }[] = [];
    const sql: string[] = [];
    const stolenFor: string[] = [];
    const conversations = new Map<string, { owner: Caller; deleted: boolean }>();
    let clock = 0;
    const callerOf = (request: RawRequest): Caller => {
      const cookie = request.headers?.find(([name]) => name === "cookie")?.[1];
      if (cookie === undefined) return "anonymous";
      if (cookie.endsWith(result.admin.cookie)) return "admin";
      return cookie.endsWith(result.member.cookie) ? "owner" : "other";
    };
    const visible = (caller: Caller, deleted: boolean) => [...conversations].filter(([, c]) => c.owner === caller && c.deleted === deleted).map(([id]) => ({ id }));
    const answer = (request: RawRequest, caller: Caller): RawResponse => {
      if (caller === "anonymous") return problem(401, "unauthenticated");
      if (caller === "admin") return problem(403, "permission_denied");
      const method = request.method ?? "GET";
      if (request.target === ASSISTANT_CONVERSATIONS_PATH) {
        if (method === "POST") {
          const id = crypto.randomUUID();
          conversations.set(id, { owner: caller, deleted: false });
          return json(201, { id });
        }
        return json(200, { conversations: visible(caller, false), next_cursor: null });
      }
      if (request.target === ASSISTANT_DELETED_PATH) return json(200, { conversations: visible(caller, true), next_cursor: null });
      const [, id, action] = /^\/api\/assistant\/v1\/conversations\/([^/]+)(?:\/(restore))?$/.exec(request.target) ?? [];
      const found = id === undefined ? undefined : conversations.get(id);
      if (id === undefined || found === undefined || found.owner !== caller) return problem(404, "not_found");
      if (action === "restore") {
        if (!found.deleted) return problem(404, "not_found");
        found.deleted = false;
        return json(200, { id });
      }
      if (method === "DELETE") {
        found.deleted = true;
        return { status: 204, headers: {}, body: "", ms: 1 };
      }
      return found.deleted ? problem(404, "not_found") : json(200, { id, messages: [] });
    };
    const deps: AssistantCheckDeps = {
      psql: async (statement) => {
        sql.push(statement);
        return "";
      },
      send: async (request) => {
        const caller = callerOf(request);
        sent.push({ to: "gateway", request, caller });
        const response = override(request, caller) ?? answer(request, caller);
        if (response.status === 0) throw new ConnectionError("connection refused");
        return response;
      },
      sendToAssistant: async (request) => {
        sent.push({ to: "assistant", request, caller: "anonymous" });
        const token = request.headers?.find(([name]) => name === IDENTITY_HEADER)?.[1];
        if (token === "token-for-assistant") return options.directControl ?? json(200, { conversations: [], next_cursor: null });
        return options.directWrong ?? problem(401, "unauthenticated");
      },
      steal: async (path) => {
        stolenFor.push(path);
        return path.startsWith("/api/assistant/") ? "token-for-assistant" : "token-for-assets";
      },
      log: () => {},
      sleep: async (ms) => {
        clock += ms;
      },
      now: () => clock,
      limits: { routeMs: 20_000, eventMs: 20_000, pollMs: 1_000 },
    };
    return { deps, sent, sql, stolenFor, conversations };
  }

  test("the owner starts, lists, deletes and restores a conversation; another member never sees it; the assistant takes only its own token", async () => {
    const result = made();
    const fake = assistantFake(result);
    const detail = await checkAssistantApi(fake.deps, APP, result);
    for (const part of [
      "201",
      "another member of the tenant: 404 not_found and no entry in its list",
      "delete 204, read 404 not_found, in the deleted list, restore 200, read 200",
      "without a cookie: 401 unauthenticated",
      "the administrator without a tenant: 403 permission_denied",
      "its token for the assets service 401 unauthenticated",
    ]) {
      expect(detail).toContain(part);
    }
    // The order: route (administrator), no cookie, then A starts and lists, B reads and lists,
    // A deletes, reads, lists the deleted, restores and reads; the assistant directly last.
    const steps = fake.sent.map(({ to, request, caller }) => `${to} ${caller} ${request.method ?? "GET"} ${request.target.replace(/[0-9a-f-]{36}/, "<id>")}`);
    expect(steps).toEqual([
      "gateway admin GET /api/assistant/v1/conversations",
      "gateway anonymous GET /api/assistant/v1/conversations",
      "gateway owner POST /api/assistant/v1/conversations",
      "gateway owner GET /api/assistant/v1/conversations",
      "gateway other GET /api/assistant/v1/conversations/<id>",
      "gateway other GET /api/assistant/v1/conversations",
      "gateway owner DELETE /api/assistant/v1/conversations/<id>",
      "gateway owner GET /api/assistant/v1/conversations/<id>",
      "gateway owner GET /api/assistant/v1/deleted-conversations",
      "gateway owner POST /api/assistant/v1/conversations/<id>/restore",
      "gateway owner GET /api/assistant/v1/conversations/<id>",
      "assistant anonymous GET /api/assistant/v1/conversations",
      "assistant anonymous GET /api/assistant/v1/conversations",
    ]);
    for (const { to, request, caller } of fake.sent) {
      expect(request.hostHeader).toBe(APP);
      const header = (name: string) => request.headers?.find(([key]) => key === name)?.[1];
      if (to === "gateway") {
        // Through the ingress only a cookie, never an identity header.
        expect(header(IDENTITY_HEADER)).toBeUndefined();
        if (caller === "anonymous") expect(header("cookie")).toBeUndefined();
        else expect(header("cookie")?.startsWith(`${SESSION_COOKIE}=`)).toBe(true);
      } else {
        expect(header("cookie")).toBeUndefined();
        expect(header(IDENTITY_HEADER)).toMatch(/^token-for-/);
      }
      // A write carries the JSON Content-Type the service demands (the CSRF rule) and an empty object.
      if (request.method === "POST") {
        expect(header("content-type")).toBe("application/json");
        expect(request.body).toBe("{}");
      } else {
        expect(request.body).toBeUndefined();
      }
    }
    // The direct requests carry the token for the assistant, then the one for the assets service.
    expect(fake.stolenFor).toEqual([ASSISTANT_CONVERSATIONS_PATH, ASSETS_TENANT_PATH]);
    expect(fake.sent.filter((s) => s.to === "assistant").map((s) => s.request.headers?.[0]?.[1])).toEqual(["token-for-assistant", "token-for-assets"]);
    // Member B is a viewer of the same tenant, made with SQL that holds no secret and no platform admin.
    expect(fake.sql).toHaveLength(1);
    expect(fake.sql[0]).toContain(`VALUES ('${TENANT}', `);
    expect(fake.sql[0]).toContain("'viewer');");
    expect(fake.sql[0]).not.toContain("platform_admin");
    expect(fake.conversations.size).toBe(1);
    expect(detail).not.toContain(result.member.cookie);
  });

  test("waits for the route and the cookie check, which answer 404, 503, Istio's 403 and a lost connection before the service", async () => {
    const result = made();
    const before = [
      () => ({ status: 0, headers: {}, body: "", ms: 0 }),
      () => json(404, ""),
      () => problem(503, "unavailable"),
      () => ({ status: 403, headers: {}, body: "RBAC: access denied", ms: 1 }),
    ];
    const fake = assistantFake(result, (_request, caller) => (caller === "admin" && before.length > 0 ? before.shift()!() : undefined));
    const detail = await checkAssistantApi(fake.deps, APP, result);
    expect(detail).toContain("the route answered the administrator after 4 s");
    expect(fake.sent.filter((s) => s.caller === "admin")).toHaveLength(5);
  });

  test("an administrator without a tenant who is served breaks authorization, so it fails at once", async () => {
    const result = made();
    const fake = assistantFake(result, (_request, caller) => (caller === "admin" ? json(200, { conversations: [] }) : undefined));
    await expect(checkAssistantApi(fake.deps, APP, result)).rejects.toThrow("the assistant (403 permission_denied): unexpected answer 200");
    expect(fake.sent).toHaveLength(1);
  });

  test("gives up after the bound and names what it waited for", async () => {
    const result = made();
    const fake = assistantFake(result, (_request, caller) => (caller === "admin" ? json(404, "") : undefined));
    await expect(checkAssistantApi(fake.deps, APP, result)).rejects.toThrow(
      "timed out after 20 s waiting for the ingress to pass the administrator's cookie to the assistant (403 permission_denied); last answer: 404",
    );
  });

  /** A deviation from the service's contract: where it happens, the answer, and the message that must name it. */
  const deviations: { name: string; at: (request: RawRequest, caller: Caller) => boolean; answer: RawResponse; message: string }[] = [
    {
      name: "a request without a cookie that is not refused at the gateway",
      at: (_r, caller) => caller === "anonymous",
      answer: problem(404, "not_found"),
      message: "GET /api/assistant/v1/conversations without a cookie answered 404",
    },
    {
      name: "a conversation that is not started",
      at: (r) => r.method === "POST" && r.target === ASSISTANT_CONVERSATIONS_PATH,
      answer: problem(403, "permission_denied"),
      message: "POST /api/assistant/v1/conversations as the tenant's member answered 403",
    },
    {
      name: "a started conversation without an ID",
      at: (r) => r.method === "POST" && r.target === ASSISTANT_CONVERSATIONS_PATH,
      answer: json(201, { title: null }),
      message: "expected a conversation",
    },
    {
      name: "a list that does not have the conversation its owner started",
      at: (r, caller) => caller === "owner" && r.target === ASSISTANT_CONVERSATIONS_PATH && r.method === undefined,
      answer: json(200, { conversations: [], next_cursor: null }),
      message: "as its owner answered 200 without the conversation",
    },
    {
      name: "a list that is not JSON",
      at: (r, caller) => caller === "owner" && r.target === ASSISTANT_CONVERSATIONS_PATH && r.method === undefined,
      answer: { status: 200, headers: {}, body: "<html>", ms: 1 },
      message: "which is not JSON",
    },
    {
      name: "another member who can read the conversation",
      at: (r, caller) => caller === "other" && r.target !== ASSISTANT_CONVERSATIONS_PATH,
      answer: json(200, { id: "x", messages: [] }),
      message: "as another member of the tenant answered 200",
    },
    {
      name: "another member who is refused with another code",
      at: (r, caller) => caller === "other" && r.target !== ASSISTANT_CONVERSATIONS_PATH,
      answer: problem(403, "permission_denied"),
      message: "as another member of the tenant answered 403",
    },
    {
      name: "another member whose list has the conversation",
      at: (r, caller) => caller === "other" && r.target === ASSISTANT_CONVERSATIONS_PATH,
      answer: json(200, { conversations: [{ id: "*" }], next_cursor: null }),
      message: "listed the conversation",
    },
    {
      name: "a delete that does not answer 204",
      at: (r) => r.method === "DELETE",
      answer: json(200, {}),
      message: "as its owner answered 200",
    },
    {
      name: "a delete with a body",
      at: (r) => r.method === "DELETE",
      answer: { status: 204, headers: {}, body: "gone", ms: 1 },
      message: "answered 204 with a body",
    },
    {
      name: "a deleted conversation that can still be read",
      at: (r, caller) => caller === "owner" && r.method === undefined && r.target.startsWith(`${ASSISTANT_CONVERSATIONS_PATH}/`),
      answer: json(200, { id: "x", messages: [] }),
      message: "after the delete answered 200",
    },
    {
      name: "a deleted list without the conversation",
      at: (r) => r.target === ASSISTANT_DELETED_PATH,
      answer: json(200, { conversations: [], next_cursor: null }),
      message: "without the deleted conversation",
    },
    {
      name: "a restore that is refused",
      at: (r) => r.target.endsWith("/restore"),
      answer: problem(404, "not_found"),
      message: "POST /api/assistant/v1/conversations/{conversation_id}/restore as its owner answered 404",
    },
    {
      name: "a restore that answers another conversation",
      at: (r) => r.target.endsWith("/restore"),
      answer: json(200, { id: "0b6f3a52-3c1e-4c39-9d36-2f4f7f6a8e10" }),
      message: "expected a conversation with ID",
    },
  ];
  for (const deviation of deviations) {
    test(`fails on ${deviation.name}, naming the step and the answer`, async () => {
      const result = made();
      let started = "";
      const fake = assistantFake(result, (request, caller) => {
        if (!deviation.at(request, caller)) return undefined;
        // A list that must hold the conversation names the real ID.
        const body = started === "" ? deviation.answer.body : deviation.answer.body.replace('"*"', JSON.stringify(started));
        return { ...deviation.answer, body };
      });
      const original = fake.deps.send;
      fake.deps.send = async (request) => {
        const response = await original(request);
        if (request.method === "POST" && request.target === ASSISTANT_CONVERSATIONS_PATH && response.status === 201) {
          started = (JSON.parse(response.body) as { id: string }).id;
        }
        return response;
      };
      const failure = await checkAssistantApi(fake.deps, APP, result).then(
        () => undefined,
        (error: Error) => error,
      );
      expect(failure?.message).toContain(deviation.message);
      // Never a cookie or a token.
      for (const secret of [result.member.cookie, result.admin.cookie, "token-for-"]) expect(failure?.message).not.toContain(secret);
    });
  }

  test("a direct request with the token for the assistant that is refused proves nothing, so the check fails", async () => {
    const result = made();
    const fake = assistantFake(result, undefined, { directControl: problem(401, "unauthenticated") });
    await expect(checkAssistantApi(fake.deps, APP, result)).rejects.toThrow("the wrong-audience check below proves nothing");
  });

  test("a token for the assets service that the assistant accepts, or refuses with another status, fails the check", async () => {
    const result = made();
    await expect(checkAssistantApi(assistantFake(result, undefined, { directWrong: json(200, { conversations: [] }) }).deps, APP, result)).rejects.toThrow(
      "token for the assets service, answered 200",
    );
    await expect(checkAssistantApi(assistantFake(result, undefined, { directWrong: problem(403, "permission_denied") }).deps, APP, result)).rejects.toThrow(
      "answered 403",
    );
  });

  test("the conversation in a path is a UUID", () => {
    expect(assistantConversationPath(TENANT)).toBe(`/api/assistant/v1/conversations/${TENANT}`);
    expect(assistantRestorePath(TENANT)).toBe(`/api/assistant/v1/conversations/${TENANT}/restore`);
    expect(() => assistantConversationPath("../../identity")).toThrow("not a UUID");
    expect(() => assistantRestorePath("1/restore")).toThrow("not a UUID");
  });
});

describe("the tenant's ClickHouse account", () => {
  const account = tenantAccountName(TENANT);
  const user = `${account}\t${TENANT_READER_ROLE}\n`;
  const roles = `${TENANT_READER_ROLE}\t0\n`;

  test("is named as the telemetry-query service names it", () => {
    // services/telemetry-query/src/accounts.rs: the prefix, and its unit test's example name.
    const accounts = readFileSync(join(REPO_ROOT, "services", "telemetry-query", "src", "accounts.rs"), "utf8");
    expect(accounts).toContain(`pub const ACCOUNT_PREFIX: &str = "${TENANT_ACCOUNT_PREFIX}";`);
    expect(accounts).toContain('"0192a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"');
    expect(accounts).toContain(`"${tenantAccountName("0192a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b")}"`);
    expect(readFileSync(join(REPO_ROOT, "services", "ingest", "migrations", "clickhouse", "0004_access.sql"), "utf8")).toContain(
      `CREATE ROLE IF NOT EXISTS ${TENANT_READER_ROLE};`,
    );
    expect(() => tenantAccountName("x' OR 1 = 1")).toThrow("not a UUID");
  });

  test("the queries read names and roles only, and take the account as a parameter", () => {
    for (const sql of [ACCOUNT_USER_SQL, ACCOUNT_ROLES_SQL]) {
      expect(sql).toContain("{user:String}");
      expect(sql).not.toMatch(/auth_params|password|hash|\*/i);
    }
  });

  test("passes as the service creates it, and names what differs otherwise", async () => {
    expect(tenantAccountProblems(account, user, roles)).toEqual([]);
    expect(tenantAccountProblems(account, "", "")).toEqual([`ClickHouse has no user ${account}`]);
    expect(tenantAccountProblems(account, `${account}\ttrustiot_ingest\n`, roles)[0]).toContain("expected the default role trustiot_tenant_reader only");
    expect(tenantAccountProblems(account, `${account}\t${TENANT_READER_ROLE},other_role\n`, roles)[0]).toContain("expected the default role trustiot_tenant_reader only");
    expect(tenantAccountProblems(account, `${account}\t\n`, roles)[0]).toContain("expected the default role trustiot_tenant_reader only");
    expect(tenantAccountProblems(account, user, `${TENANT_READER_ROLE}\t1\n`)[0]).toContain("without admin option only");
    expect(tenantAccountProblems(account, user, `${roles}other_role\t0\n`)[0]).toContain("without admin option only");

    const asked: [string, string][] = [];
    const detail = await checkTenantAccount(async (sql, name) => {
      asked.push([sql, name]);
      return sql === ACCOUNT_USER_SQL ? user : roles;
    }, TENANT);
    expect(detail).toBe(`ClickHouse user ${account} of tenant ${TENANT} exists with the default role ${TENANT_READER_ROLE}, granted without admin option`);
    expect(asked).toEqual([
      [ACCOUNT_USER_SQL, account],
      [ACCOUNT_ROLES_SQL, account],
    ]);
    await expect(checkTenantAccount(async () => "", TENANT)).rejects.toThrow(`ClickHouse has no user ${account}`);
  });
});

// Laysi/trustiot-v2#592: a ready assistant holds a Postgres session of its own role.
describe("the assistant's login", () => {
  const fixed = { sleep: async () => {}, limits: { loginMs: 10_000, pollMs: 1_000 } };
  // A clock that moves one poll interval per reading, so the limit is reached without waiting.
  const ticking = () => {
    let now = 0;
    return () => (now += 1_000);
  };

  test("reads only the role's login flag and its session count, never a password", () => {
    const sql = roleSessionsSql(ASSISTANT_ROLE);
    expect(sql).toContain("FROM pg_roles r WHERE r.rolname = 'assistant_app'");
    expect(sql).toContain("pg_stat_activity a WHERE a.usename = 'assistant_app' AND a.datname = current_database()");
    expect(sql).not.toMatch(/passw|pg_authid|select \*/i);
    expect(() => roleSessionsSql("x' OR '1' = '1")).toThrow("not a role name");
  });

  test("passes once the role may log in and has a session, after waiting for it", async () => {
    const answers = ["f|0\n", "t|0\n", "t|2\n"];
    const asked: string[] = [];
    const detail = await checkAssistantLogin(
      {
        ...fixed,
        now: ticking(),
        psql: async (sql) => {
          asked.push(sql);
          return answers.shift() ?? "t|2\n";
        },
      },
      "deployment/trustiot-assistant",
      1,
    );
    expect(detail).toStartWith("deployment/trustiot-assistant: 1 ready replica(s); assistant_app may log in and has 2 session(s) open");
    expect(asked).toEqual(Array(3).fill(roleSessionsSql(ASSISTANT_ROLE)));
  });

  test("fails without a ready replica, a role, a login or a session", async () => {
    const psql = (out: string) => async () => out;
    await expect(checkAssistantLogin({ ...fixed, now: ticking(), psql: psql("t|1\n") }, "deployment/a", 0)).rejects.toThrow("deployment/a has no ready replica");
    await expect(checkAssistantLogin({ ...fixed, now: ticking(), psql: psql("") }, "deployment/a", 1)).rejects.toThrow("the role assistant_app does not exist");
    await expect(checkAssistantLogin({ ...fixed, now: ticking(), psql: psql("f|0\n") }, "deployment/a", 1)).rejects.toThrow(
      'timed out after 10 s waiting for a session of assistant_app; psql printed "f|0"',
    );
    await expect(checkAssistantLogin({ ...fixed, now: ticking(), psql: psql("t|0\n") }, "deployment/a", 1)).rejects.toThrow("waiting for a session of assistant_app");
    await expect(checkAssistantLogin({ ...fixed, now: ticking(), psql: psql("error\n") }, "deployment/a", 1)).rejects.toThrow('unexpected psql output "error"');
  });
});

// Laysi/trustiot-v2#642: a ready ingest holds sessions of both its roles, and the migration Job
// gave its ClickHouse account the ingest role alone.
describe("ingest's deployment", () => {
  const fixed = { sleep: async () => {}, limits: { loginMs: 10_000, pollMs: 1_000 } };
  const ticking = () => {
    let now = 0;
    return () => (now += 1_000);
  };
  const user = `${INGEST_ACCOUNT}\t${INGEST_ROLE}\n`;
  const roles = `${INGEST_ROLE}\t0\n`;
  const clickhouse = (answers: { user: string; roles: string }) => async (sql: string, name: string) => {
    expect(name).toBe(INGEST_ACCOUNT);
    return sql === ACCOUNT_USER_SQL ? answers.user : answers.roles;
  };

  test("names the roles, the account and the role the service and the migration Job use", () => {
    const config = readFileSync(join(REPO_ROOT, "tools", "migrate", "src", "config.rs"), "utf8");
    expect(config).toContain(`pub const INGEST_ACCOUNT: &str = "${INGEST_ACCOUNT}";`);
    const clickhouseRs = readFileSync(join(REPO_ROOT, "tools", "migrate", "src", "clickhouse.rs"), "utf8");
    expect(clickhouseRs).toContain(`pub const INGEST_ROLE: &str = "${INGEST_ROLE}";`);
    expect(INGEST_ROLES).toEqual(["ingest_app", "ingest_platform"]);
  });

  test("passes once both roles have a session and the account holds the role alone", async () => {
    const answers = ["t|0\n", "t|1\n", "f|0\n", "t|4\n"];
    const asked: string[] = [];
    const detail = await checkIngest(
      {
        ...fixed,
        now: ticking(),
        psql: async (sql) => {
          asked.push(sql);
          return answers.shift() ?? "t|1\n";
        },
        clickhouse: clickhouse({ user, roles }),
      },
      "deployment/trustiot-ingest",
      1,
    );
    expect(detail).toStartWith("deployment/trustiot-ingest: 1 ready replica(s), so it reached ClickHouse and the MQTT broker; ingest_app has 1 and ingest_platform has 4 session(s) open");
    expect(detail).toEndWith(`ClickHouse user ${INGEST_ACCOUNT} has the default role ${INGEST_ROLE} only, granted without admin option`);
    expect(asked).toEqual([...Array(2).fill(roleSessionsSql("ingest_app")), ...Array(2).fill(roleSessionsSql("ingest_platform"))]);
  });

  test("fails without a ready replica, a session of either role, or the account as the Job makes it", async () => {
    const deps = (psql: string, answers = { user, roles }) => ({ ...fixed, now: ticking(), psql: async () => psql, clickhouse: clickhouse(answers) });
    await expect(checkIngest(deps("t|1\n"), "deployment/i", 0)).rejects.toThrow("deployment/i has no ready replica");
    await expect(checkIngest(deps(""), "deployment/i", 1)).rejects.toThrow("the role ingest_app does not exist");
    await expect(checkIngest(deps("t|0\n"), "deployment/i", 1)).rejects.toThrow("waiting for a session of ingest_app");
    let calls = 0;
    const platformMissing = { ...fixed, now: ticking(), psql: async () => (calls++ === 0 ? "t|1\n" : "f|0\n"), clickhouse: clickhouse({ user, roles }) };
    await expect(checkIngest(platformMissing, "deployment/i", 1)).rejects.toThrow("waiting for a session of ingest_platform");
    await expect(checkIngest(deps("t|1\n", { user: "", roles: "" }), "deployment/i", 1)).rejects.toThrow(`ClickHouse has no user ${INGEST_ACCOUNT}`);
    await expect(checkIngest(deps("t|1\n", { user: `${INGEST_ACCOUNT}\t\n`, roles }), "deployment/i", 1)).rejects.toThrow("expected the default role trustiot_ingest only");
    await expect(checkIngest(deps("t|1\n", { user, roles: `${INGEST_ROLE}\t1\n` }), "deployment/i", 1)).rejects.toThrow("expected trustiot_ingest without admin option only");
    await expect(checkIngest(deps("t|1\n", { user, roles: `${roles}trustiot_tenant_reader\t0\n` }), "deployment/i", 1)).rejects.toThrow("expected trustiot_ingest without admin option only");
  });

  test("accountProblems is the tenant account's check with another role", () => {
    expect(accountProblems(INGEST_ACCOUNT, INGEST_ROLE, user, roles)).toEqual([]);
    expect(accountProblems(INGEST_ACCOUNT, INGEST_ROLE, `${INGEST_ACCOUNT}\t${INGEST_ROLE},other\n`, roles)[0]).toContain("expected the default role trustiot_ingest only");
  });
});

// Laysi/trustiot-v2#721: a ready gateway-ops reached the broker (its readiness) and holds sessions
// of both its roles.
describe("gateway-ops' deployment", () => {
  const fixed = { sleep: async () => {}, limits: { loginMs: 10_000, pollMs: 1_000 } };
  const ticking = () => {
    let now = 0;
    return () => (now += 1_000);
  };

  test("names the roles the migration Job makes, and the broker knows the service's account", () => {
    expect(GATEWAY_OPS_ROLES).toEqual(["gateway_ops_app", "gateway_ops_platform"]);
    const accounts = readFileSync(join(REPO_ROOT, "tools", "migrate", "src", "mqtt_accounts.rs"), "utf8");
    expect(accounts).toContain('service: "gateway-ops",');
  });

  test("passes once both roles have a session", async () => {
    const answers = ["t|0\n", "t|1\n", "t|2\n"];
    const asked: string[] = [];
    const detail = await checkGatewayOps(
      {
        ...fixed,
        now: ticking(),
        psql: async (sql) => {
          asked.push(sql);
          return answers.shift() ?? "t|1\n";
        },
      },
      "deployment/trustiot-gateway-ops",
      1,
    );
    expect(detail).toStartWith(
      "deployment/trustiot-gateway-ops: 1 ready replica(s), so it reached the MQTT broker as service:gateway-ops; gateway_ops_app has 1 and gateway_ops_platform has 2 session(s) open",
    );
    expect(asked).toEqual([...Array(2).fill(roleSessionsSql("gateway_ops_app")), roleSessionsSql("gateway_ops_platform")]);
  });

  test("fails without a ready replica or a session of either role", async () => {
    const deps = (psql: string) => ({ ...fixed, now: ticking(), psql: async () => psql });
    await expect(checkGatewayOps(deps("t|1\n"), "deployment/g", 0)).rejects.toThrow("deployment/g has no ready replica");
    await expect(checkGatewayOps(deps(""), "deployment/g", 1)).rejects.toThrow("the role gateway_ops_app does not exist");
    await expect(checkGatewayOps(deps("t|0\n"), "deployment/g", 1)).rejects.toThrow("waiting for a session of gateway_ops_app");
    let calls = 0;
    const platformMissing = { ...fixed, now: ticking(), psql: async () => (calls++ === 0 ? "t|1\n" : "f|0\n") };
    await expect(checkGatewayOps(platformMissing, "deployment/g", 1)).rejects.toThrow("waiting for a session of gateway_ops_platform");
  });
});

describe("the API check's routes", () => {
  const existing = { host: "gateway", path: "/certs", match: "PathPrefix", service: "certs" };

  test("come after the routes the shape's values set, which helm would otherwise drop", () => {
    const values = apiValues("saas", [existing]) as { ingress: { routes: unknown[]; authzProvider: string } };
    expect(values.ingress.routes).toEqual([existing, ...API_ROUTES]);
    expect(API_ROUTES).toContainEqual({ host: "app", path: "/api/assistant", match: "PathPrefix", service: "assistant" });
    expect(values.ingress.authzProvider).toBe("trustiot-smoke-saas");
    expect(() => apiValues("saas", [{ ...existing, host: "app", path: "/api/assets" }])).toThrow("already route app /api/assets to certs");
    // No key and no key setting: the identity pod's init container generates it (Laysi/trustiot-v2#404).
    expect(values).not.toHaveProperty("internalIdentity");
  });

  test("routesOfValues takes the list from the last values file that sets it", () => {
    const chart = mkdtempSync(join(tmpdir(), "nightly-api-routes-"));
    try {
      writeFileSync(join(chart, "values.yaml"), "ingress:\n  routes: []\n");
      writeFileSync(join(chart, "a.yaml"), `ingress:\n  routes:\n    - ${JSON.stringify(existing)}\n`);
      writeFileSync(join(chart, "b.yaml"), "ingress:\n  authzProvider: other\n");
      writeFileSync(join(chart, "c.yaml"), "ingress:\n  routes: []\n");
      writeFileSync(join(chart, "d.yaml"), "ingress:\n  routes: nope\n");
      expect(routesOfValues(chart, ["a.yaml", "b.yaml"])).toEqual([existing]);
      expect(routesOfValues(chart, ["a.yaml", "c.yaml"])).toEqual([]);
      expect(() => routesOfValues(chart, ["d.yaml"])).toThrow("d.yaml: ingress.routes is not a list");
    } finally {
      rmSync(chart, { recursive: true, force: true });
    }
  });
});

describe("the API check's values on each deployment shape", () => {
  for (const shape of discoverShapes(CHART_DIR)) {
    // Two commands (resolving the pinned helm, one render), each with its own limit: the test's
    // limit covers both on a loaded machine (CLAUDE.md, "Test time limits").
    test(`values-${shape.name}.yaml with them renders identity, assets and telemetry-query behind the smoke namespace's provider`, async () => {
      const helm = await pinnedTool("helm", REPO_ROOT);
      const dir = mkdtempSync(join(tmpdir(), "nightly-api-values-"));
      let objects: K8sObject[];
      try {
        const file = join(dir, "values-api.yaml");
        const values = apiValues(shape.name, routesOfValues(CHART_DIR, shape.valuesFiles));
        writeFileSync(file, Bun.YAML.stringify(values, null, 2));
        const cmd = [helm, "template", "trustiot", CHART_DIR, "--namespace", `nightly-${shape.name}`, ...valuesArgs(CHART_DIR, shape), "-f", file];
        const rendered = Bun.spawnSync({ cmd, stdout: "pipe", stderr: "pipe", timeout: COMMAND_TIMEOUT_MS });
        throwIfTimedOut(cmd, rendered);
        expect(rendered.stderr.toString()).toBe("");
        expect(rendered.exitCode, "helm template exit code").toBe(0);
        objects = parseManifests(rendered.stdout.toString());
      } finally {
        rmSync(dir, { recursive: true, force: true });
      }
      const named = (kind: string, name: string) => objects.find((o) => o.kind === kind && o.metadata?.name === name);
      expect(named("Deployment", "trustiot-identity")).toBeDefined();
      expect(named("Deployment", "trustiot-assets")).toBeDefined();
      // The AI gateway starts with the smoke's provider and role (Laysi/trustiot-v2#536); its key
      // is a dummy the smoke makes, like every Secret the pods read and the chart does not declare.
      expect(named("Deployment", "trustiot-ai-gateway")).toBeDefined();
      const gatewayConfig = named("ConfigMap", "trustiot-ai-gateway")?.data as Record<string, string> | undefined;
      expect(gatewayConfig?.AI_GATEWAY__ROLES__SMOKE__MODELS).toBe("anthropic/claude-smoke");
      expect(gatewayConfig?.AI_GATEWAY__PROVIDERS__ANTHROPIC__KEY_ENV).toBe("TRUSTIOT_AI_ANTHROPIC_API_KEY");
      expect(missingSecrets(objects).get("trustiot-anthropic")).toEqual(new Set(["api-key"]));
      // telemetry-query (Laysi/trustiot-v2#539) runs from the nightly image with the passwords the
      // migration Job generates into the Secret the chart declares: no dummy is made for it.
      const telemetry = named("Deployment", "trustiot-telemetry-query");
      expect(telemetry).toBeDefined();
      expect(JSON.stringify(telemetry)).toContain('"image":"localhost/trustiot-e2e/telemetry-query:nightly"');
      expect(passwordSecretsOf(objects).get("trustiot-telemetry-query-password")).toEqual(
        expect.arrayContaining(["telemetry_query_app", "telemetry_query_platform", "trustiot_telemetry_query"]),
      );
      expect([...missingSecrets(objects).keys()].filter((name) => name.includes("telemetry"))).toEqual([]);
      // The tenant account check runs SQL in the bundled ClickHouse's pod.
      expect(clickhousePodOf(objects)).toBe("trustiot-clickhouse-0");
      // The cookie check goes to this namespace's identity service (platform.ts).
      const custom = objects.filter((o) => o.kind === "AuthorizationPolicy" && o.spec?.action === "CUSTOM");
      expect(custom.length).toBeGreaterThan(0);
      for (const policy of custom) expect((policy.spec?.provider as { name?: string }).name).toBe(`trustiot-smoke-${shape.name}`);
      // The APIs are cookie routes on the app host.
      const settings = platformSettingsOf(objects);
      const routes = objects.filter((o) => o.kind === "HTTPRoute").map((o) => JSON.stringify(o.spec));
      for (const path of ["/api/identity", "/api/assets", "/api/telemetry", "/api/certs", "/api/assistant"]) {
        expect(routes.some((r) => r.includes(`"value":"${path}"`) && r.includes(settings.TRUSTIOT_APP_HOST!)), path).toBe(true);
      }
      expect(routes.some((r) => r.includes('"value":"/api/telemetry"') && r.includes('"name":"trustiot-telemetry-query"'))).toBe(true);
      // The assistant's chat API (Laysi/trustiot-v2#647): a cookie route to its Service.
      expect(routes.some((r) => r.includes('"value":"/api/assistant"') && r.includes('"name":"trustiot-assistant"') && r.includes(settings.TRUSTIOT_APP_HOST!))).toBe(true);
      // The certs service from its image, its CRL job every 10 s (Laysi/trustiot-v2#517).
      const certs = named("Deployment", "trustiot-certs");
      expect(certs).toBeDefined();
      const certsContainer = (certs?.spec?.template as { spec: { containers: { name: string; image: string; env: { name: string; value?: string }[] }[] } }).spec.containers.find(
        (c) => c.name === "certs",
      );
      expect(certsContainer?.image).toBe(serviceImageRef("certs"));
      expect(certsContainer?.env.find((e) => e.name === "TRUSTIOT__CRL__CHECK_INTERVAL")?.value).toBe("10s");
      // The assistant from its image (Laysi/trustiot-v2#592): the smoke's model role, the
      // addresses the chart derives from mesh.calls.assistant, and its own database login, whose
      // password the migration Job generates into the Secret the chart declares.
      const assistant = named("Deployment", "trustiot-assistant");
      const assistantContainer = (assistant?.spec?.template as { spec: { containers: { name: string; image: string; env: { name: string; value?: string }[] }[] } } | undefined)?.spec.containers.find(
        (c) => c.name === "assistant",
      );
      expect(assistantContainer?.image).toBe(serviceImageRef("assistant"));
      const assistantEnv = Object.fromEntries((assistantContainer?.env ?? []).map((e) => [e.name, e.value]));
      expect(assistantEnv).toMatchObject({
        TRUSTIOT__AI_GATEWAY__ROLE: "smoke",
        TRUSTIOT__AI_GATEWAY__URL: "http://trustiot-ai-gateway:8080",
        TRUSTIOT__TELEMETRY_QUERY__URL: "http://trustiot-telemetry-query:8080",
        TRUSTIOT__ASSETS__URL: "http://trustiot-assets:8080",
        TRUSTIOT__IDENTITY__EXCHANGE_URL: "http://trustiot-identity-exchange:8082",
        TRUSTIOT__DATABASE__USERNAME: ASSISTANT_ROLE,
      });
      expect(passwordSecretsOf(objects).get("trustiot-assistant-password")).toEqual(expect.arrayContaining([ASSISTANT_ROLE]));
      // The dashboard check from its image where the smoke runs it (Laysi/trustiot-v2#150), and the
      // assistant's address of it; elsewhere neither.
      const checker = named("Deployment", "trustiot-dashboard-check");
      if (DASHBOARD_CHECK_SHAPES.has(shape.name)) {
        const checkerContainer = (checker?.spec?.template as { spec: { containers: { name: string; image: string }[] } } | undefined)?.spec.containers.find(
          (c) => c.name === "dashboard-check",
        );
        expect(checkerContainer?.image).toBe(serviceImageRef("dashboard-check"));
        expect(assistantEnv.TRUSTIOT__DASHBOARD_CHECK__URL).toBe("http://trustiot-dashboard-check:8080");
      } else {
        expect(checker).toBeUndefined();
        expect(assistantEnv).not.toHaveProperty("TRUSTIOT__DASHBOARD_CHECK__URL");
      }
      expect([...missingSecrets(objects).keys()].filter((name) => name.includes("assistant"))).toEqual([]);
      // ingest from its image (Laysi/trustiot-v2#642): its Postgres roles and ClickHouse account
      // with the passwords the migration Job generates into the Secret the chart declares, and its
      // broker account from the Secret the broker's init container fills; no dummy for any.
      const ingest = named("Deployment", "trustiot-ingest");
      const ingestContainer = (ingest?.spec?.template as { spec: { containers: { name: string; image: string; env: { name: string; value?: string }[] }[] } } | undefined)?.spec.containers.find(
        (c) => c.name === "ingest",
      );
      expect(ingestContainer?.image).toBe(serviceImageRef("ingest"));
      // The file the chart mounts from the broker accounts Secret (its key ingest only).
      const ingestAccountFile = ["/etc/trustiot/mqtt-broker-account", "ingest"].join("/");
      const ingestEnv = Object.fromEntries((ingestContainer?.env ?? []).map((e) => [e.name, e.value]));
      expect(ingestEnv).toMatchObject({
        TRUSTIOT__DATABASE__USERNAME: INGEST_ROLES[0],
        TRUSTIOT__DATABASE__PLATFORM_USERNAME: INGEST_ROLES[1],
        TRUSTIOT__CLICKHOUSE__USERNAME: INGEST_ACCOUNT,
        TRUSTIOT__MQTT__USERNAME: "service:ingest",
        TRUSTIOT__MQTT__PASSWORD_FILE: ingestAccountFile,
      });
      expect(passwordSecretsOf(objects).get("trustiot-ingest-password")).toEqual(expect.arrayContaining([...INGEST_ROLES, INGEST_ACCOUNT]));
      expect([...missingSecrets(objects).keys()].filter((name) => name.includes("ingest") || name.includes("mqtt-broker"))).toEqual([]);
      // gateway-ops from its image (Laysi/trustiot-v2#721): its Postgres roles with the passwords
      // the migration Job generates, and its broker account from the Secret the broker's init
      // container fills; no dummy for either.
      const gatewayOps = named("Deployment", "trustiot-gateway-ops");
      const gatewayOpsContainer = (gatewayOps?.spec?.template as { spec: { containers: { name: string; image: string; env: { name: string; value?: string }[] }[] } } | undefined)?.spec.containers.find(
        (c) => c.name === "gateway-ops",
      );
      expect(gatewayOpsContainer?.image).toBe(serviceImageRef("gateway-ops"));
      const gatewayOpsEnv = Object.fromEntries((gatewayOpsContainer?.env ?? []).map((e) => [e.name, e.value]));
      expect(gatewayOpsEnv).toMatchObject({
        TRUSTIOT__DATABASE__USERNAME: GATEWAY_OPS_ROLES[0],
        TRUSTIOT__DATABASE__PLATFORM_USERNAME: GATEWAY_OPS_ROLES[1],
        TRUSTIOT__MQTT__USERNAME: "service:gateway-ops",
        TRUSTIOT__MQTT__PASSWORD_FILE: ["/etc/trustiot/mqtt-broker-account", "gateway-ops"].join("/"),
      });
      expect(passwordSecretsOf(objects).get("trustiot-gateway-ops-password")).toEqual(expect.arrayContaining([...GATEWAY_OPS_ROLES]));
      expect([...missingSecrets(objects).keys()].filter((name) => name.includes("gateway-ops"))).toEqual([]);
      // Nothing makes a key for the identity service: the chart declares the Secret its init
      // container fills, so no dummy is made for it, and nothing names another one.
      expect(named("Secret", "trustiot-identity-signing-keys")).toBeDefined();
      expect([...missingSecrets(objects).keys()].filter((name) => name.includes("signing"))).toEqual([]);
      expect(JSON.stringify(objects)).not.toContain("signing-key.pem");
      // What the check reads from the release.
      expect(settings.TRUSTIOT_POSTGRES_DATABASE).toBeTruthy();
      expect(ingressHostsOf(objects)).toHaveLength(3);
      const listener = ingressListenerOf(objects);
      expect(listener).toEqual(shape.name === "saas" ? { gateway: listener.gateway, port: 80, tls: false } : { gateway: listener.gateway, port: 443, tls: true });
      expect(missingTlsSecrets(objects).length).toBe(listener.tls ? 1 : 0);
    }, SLOW_TEST_MS);
  }
});

// ---------------------------------------------------------------------------------------------
// Laysi/trustiot-v2#77: the stolen identity header, Host ports and normalized paths.

describe("the stolen identity header", () => {
  const session = newSession();
  // Made up, and built at run time like every credential-shaped value in scripts/.
  const token = ["eyJh", "eyJz", "c2ln"].join(".");

  test("is the one token the identity service's ext_authz check hands out for the member's GET of the tenant", async () => {
    const asked: RawRequest[] = [];
    const stolen = await stealIdentityToken(
      async (request) => {
        asked.push(request);
        return { status: 200, headers: { [IDENTITY_HEADER]: [token] }, body: "", ms: 1 };
      },
      APP,
      session,
    );
    expect(stolen).toBe(token);
    expect(asked).toEqual([
      { target: `${EXT_AUTHZ_PREFIX}${ASSETS_TENANT_PATH}`, hostHeader: APP, headers: [["cookie", `${SESSION_COOKIE}=${session.cookie}`]] },
    ]);
  });

  test("anything but 200 with one token fails without printing what came back", async () => {
    const answers: RawResponse[] = [
      { status: 401, headers: {}, body: "", ms: 1 },
      { status: 200, headers: {}, body: "", ms: 1 },
      { status: 200, headers: { [IDENTITY_HEADER]: [token, token] }, body: "", ms: 1 },
      { status: 200, headers: { [IDENTITY_HEADER]: ["not-a-token"] }, body: "", ms: 1 },
    ];
    for (const answer of answers) {
      const failed = stealIdentityToken(async () => answer, APP, session);
      await expect(failed).rejects.toThrow("the stolen-header checks below prove nothing");
      await failed.catch((error: Error) => expect(error.message).not.toContain(token));
    }
  });

  test("the ext_authz prefix is the identity service's", () => {
    // A renamed prefix fails here first, before a night shows the check was refused.
    const authz = readFileSync(join(REPO_ROOT, "services", "identity", "src", "ext_authz.rs"), "utf8");
    expect(authz).toContain(`"${EXT_AUTHZ_PREFIX}`);
  });
});

describe("checkIngressEdges", () => {
  const LISTENER_PORT = 80;
  const made = (): TenantApiResult => ({ detail: "", tenantId: TENANT, admin: newSession(), member: newSession() });
  const served = json(200, { tenant_id: TENANT, created_at: "2026-10-14T00:00:01Z" });

  /**
   * The ingress and the assets service as the check expects them, with `override` replacing the
   * answer to one kind of request. Records what reached the gateway and the service.
   */
  function edges(result: TenantApiResult, override: Partial<Record<"direct" | "anonymous" | "admin" | "member", (request: RawRequest) => RawResponse>> = {}) {
    const sent: { to: "gateway" | "assets"; request: RawRequest }[] = [];
    const header = (request: RawRequest, name: string) => request.headers?.find(([key]) => key === name)?.[1];
    const stolen: string[] = [];
    const deps: EdgeCheckDeps = {
      sendToAssets: async (request) => {
        sent.push({ to: "assets", request });
        return (override.direct ?? (() => served))(request);
      },
      send: async (request) => {
        sent.push({ to: "gateway", request });
        const cookie = header(request, "cookie");
        if (cookie === undefined) return (override.anonymous ?? (() => problem(401, "unauthenticated")))(request);
        if (cookie.endsWith(result.admin.cookie)) return (override.admin ?? (() => problem(403, "permission_denied")))(request);
        if (cookie.endsWith(result.member.cookie)) return (override.member ?? (() => served))(request);
        throw new Error(`unknown cookie ${cookie}`);
      },
      steal: async () => {
        const token = `stolen-${stolen.length}`;
        stolen.push(token);
        return token;
      },
      log: () => {},
    };
    return { deps, sent, stolen, header };
  }

  test("a stolen header that the assets service accepts never gets through; Host ports and normalized paths reach the tenant", async () => {
    const result = made();
    const run = edges(result);
    const detail = await checkIngressEdges(run.deps, APP, LISTENER_PORT, result);
    expect(detail).toContain("401 unauthenticated through the ingress without a cookie");
    expect(detail).toContain("403 permission_denied with the administrator's cookie");
    expect(detail).toContain("Host :80 and :8443: 200 with the tenant");
    expect(detail).toContain(`on ${NORMALIZED_TENANT_PATHS.length} path spellings`);
    // First the positive control past the ingress, then the two stolen requests through it.
    expect(run.sent[0]?.to).toBe("assets");
    expect(run.header(run.sent[0]!.request, IDENTITY_HEADER)).toBe(run.stolen[0]);
    expect(run.sent.filter((s) => s.to === "assets")).toHaveLength(1);
    const stolenThroughGateway = run.sent.filter((s) => s.to === "gateway" && run.header(s.request, IDENTITY_HEADER) !== undefined);
    expect(stolenThroughGateway.map((s) => run.header(s.request, "cookie"))).toEqual([undefined, `${SESSION_COOKIE}=${result.admin.cookie}`]);
    // A fresh token for every request: none outlives its minute.
    expect(new Set(run.stolen).size).toBe(3);
    // The member's requests: two Hosts with a port, then every path spelling, never with a stolen header.
    const member = run.sent.filter((s) => run.header(s.request, "cookie") === `${SESSION_COOKIE}=${result.member.cookie}`);
    expect(member.map((s) => [s.request.hostHeader, s.request.target])).toEqual([
      [`${APP}:80`, ASSETS_TENANT_PATH],
      [`${APP}:8443`, ASSETS_TENANT_PATH],
      ...NORMALIZED_TENANT_PATHS.map((target) => [APP, target]),
    ]);
    for (const s of member) expect(run.header(s.request, IDENTITY_HEADER)).toBeUndefined();
  });

  test("a stolen header the assets service refuses proves nothing, so the check fails", async () => {
    const result = made();
    await expect(checkIngressEdges(edges(result, { direct: () => problem(401, "unauthenticated") }).deps, APP, LISTENER_PORT, result)).rejects.toThrow(
      "the stolen-header checks below prove nothing",
    );
  });

  test("a stolen header that reaches the assets service through the ingress fails the check", async () => {
    const result = made();
    await expect(checkIngressEdges(edges(result, { anonymous: () => served }).deps, APP, LISTENER_PORT, result)).rejects.toThrow(
      "a stolen identity header without a cookie answered 200",
    );
    // With the administrator's cookie: the service saw the stolen tenant (200), or both headers (401).
    for (const answer of [served, problem(401, "unauthenticated")]) {
      await expect(checkIngressEdges(edges(result, { admin: () => answer }).deps, APP, LISTENER_PORT, result)).rejects.toThrow(
        `the administrator's cookie with a stolen identity header answered ${answer.status}`,
      );
    }
  });

  test("a Host with a port or a normalized path that misses the tenant fails, naming the case", async () => {
    const result = made();
    const byHost = edges(result, { member: (request) => (request.hostHeader.endsWith(":8443") ? problem(403, "permission_denied") : served) });
    await expect(checkIngressEdges(byHost.deps, APP, LISTENER_PORT, result)).rejects.toThrow(`the member's cookie with Host ${APP}:8443 answered 403`);
    const target = "/api/identity/../assets/v1/tenant";
    const byPath = edges(result, { member: (request) => (request.target === target ? problem(401, "unauthenticated") : served) });
    await expect(checkIngressEdges(byPath.deps, APP, LISTENER_PORT, result)).rejects.toThrow(`the member's cookie on ${target} answered 401`);
    const otherTenant = edges(result, { member: () => json(200, { tenant_id: "1b6f3a52-3c1e-4c39-9d36-2f4f7f6a8e10" }) });
    await expect(checkIngressEdges(otherTenant.deps, APP, LISTENER_PORT, result)).rejects.toThrow(`expected 200 with tenant ${TENANT}`);
  });

  test("every path spelling is one the mesh turns into the tenant path", () => {
    // Percent-decode %2F, apply RFC 3986 dot segments and merge slashes, as DECODE_AND_MERGE_SLASHES does.
    const normalize = (target: string) => {
      const merged = target.replaceAll(/%2f/gi, "/").replaceAll(/\/+/g, "/");
      const out: string[] = [];
      for (const segment of merged.split("/").slice(1)) {
        if (segment === ".") continue;
        if (segment === "..") out.pop();
        else out.push(segment);
      }
      return `/${out.join("/")}`;
    };
    for (const target of NORMALIZED_TENANT_PATHS) {
      expect(target).not.toBe(ASSETS_TENANT_PATH);
      expect(normalize(target)).toBe(ASSETS_TENANT_PATH);
    }
    expect(new Set(NORMALIZED_TENANT_PATHS).size).toBe(NORMALIZED_TENANT_PATHS.length);
  });
});

// Laysi/trustiot-v2#678: the cookie check compares the user and the tenant a tab shows.

describe("checkExpectedSession", () => {
  const made = (): TenantApiResult => ({ detail: "", tenantId: TENANT, admin: newSession(), member: newSession() });
  const served = json(200, { tenant_id: TENANT, created_at: "2026-10-14T00:00:01Z" });
  const changed = problem(409, SESSION_CHANGED);
  const header = (request: RawRequest, name: string) => request.headers?.find(([key]) => key === name)?.[1];

  /**
   * The ingress as istiod passes it the headers in `forwarded`: the identity service compares only
   * the ones it receives, as it does with a provider that leaves a header out.
   */
  function ingress(result: TenantApiResult, forwarded: readonly string[] = [EXPECTED_USER_HEADER, EXPECTED_TENANT_HEADER]) {
    const sent: RawRequest[] = [];
    const send = async (request: RawRequest): Promise<RawResponse> => {
      sent.push(request);
      if (header(request, "cookie") !== `${SESSION_COOKIE}=${result.member.cookie}`) throw new Error("not the member's cookie");
      const user = forwarded.includes(EXPECTED_USER_HEADER) ? header(request, EXPECTED_USER_HEADER) : undefined;
      const tenant = forwarded.includes(EXPECTED_TENANT_HEADER) ? header(request, EXPECTED_TENANT_HEADER) : undefined;
      if (user !== undefined && user !== result.member.userId) return changed;
      if (tenant !== undefined && tenant !== result.tenantId) return changed;
      return served;
    };
    return { deps: { send, log: () => {} }, sent };
  }

  test("the session's own user and tenant are served; another user, another tenant and no tenant are refused with 409 session_changed", async () => {
    const result = made();
    const run = ingress(result);
    const detail = await checkExpectedSession(run.deps, APP, result);
    expect(detail).toContain("naming its own user and tenant");
    expect(detail).toContain(`409 ${SESSION_CHANGED} each`);
    const named = run.sent.map((request) => [request.target, header(request, EXPECTED_USER_HEADER), header(request, EXPECTED_TENANT_HEADER)]);
    expect(named).toHaveLength(4);
    expect(named[0]).toEqual([ASSETS_TENANT_PATH, result.member.userId, TENANT]);
    // Another user in the same tenant: only the user header can catch it.
    expect(named[1]).toEqual([ASSETS_TENANT_PATH, result.admin.userId, TENANT]);
    expect(named[2]?.[1]).toBe(result.member.userId);
    expect(named[2]?.[2]).not.toBe(TENANT);
    expect(named[2]?.[2]).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/);
    expect(named[3]).toEqual([ASSETS_TENANT_PATH, result.member.userId, EXPECTED_NO_TENANT]);
    for (const request of run.sent) expect(request.hostHeader).toBe(APP);
  });

  test("an istiod that does not pass a header to the cookie check fails the check, naming the header and the istiod values", async () => {
    const result = made();
    for (const [forwarded, missing] of [
      [[EXPECTED_TENANT_HEADER], EXPECTED_USER_HEADER],
      [[EXPECTED_USER_HEADER], EXPECTED_TENANT_HEADER],
      [[], EXPECTED_USER_HEADER],
    ] as const) {
      const failed = checkExpectedSession(ingress(result, forwarded).deps, APP, result);
      await expect(failed).rejects.toThrow(`answered 200`);
      await expect(failed).rejects.toThrow(`istiod did not pass ${missing} to the cookie check`);
      await expect(failed).rejects.toThrow("deploy/istio/istiod-values.yaml");
    }
  });

  test("a refusal with another status or code, or a matching request that is not served, fails the check", async () => {
    const result = made();
    const answers = (respond: (request: RawRequest) => RawResponse) => ({
      send: async (request: RawRequest) => respond(request),
      log: () => {},
    });
    const matching = (request: RawRequest) => header(request, EXPECTED_USER_HEADER) === result.member.userId && header(request, EXPECTED_TENANT_HEADER) === TENANT;
    await expect(checkExpectedSession(answers(() => changed), APP, result)).rejects.toThrow(
      `the member's cookie naming its own user and tenant answered 409 `,
    );
    await expect(checkExpectedSession(answers(() => json(200, { tenant_id: "1b6f3a52-3c1e-4c39-9d36-2f4f7f6a8e10" })), APP, result)).rejects.toThrow(
      `expected 200 with tenant ${TENANT}`,
    );
    for (const refusal of [problem(409, "conflict"), problem(403, "permission_denied"), problem(409, SESSION_CHANGED, "error.conflict")]) {
      const failed = checkExpectedSession(answers((request) => (matching(request) ? served : refusal)), APP, result);
      await expect(failed).rejects.toThrow(`the member's cookie naming another user than the session's answered ${refusal.status}`);
      await expect(failed).rejects.toThrow(`expected 409 ${SESSION_CHANGED}`);
      await failed.catch((error: Error) => expect(error.message).not.toContain("istiod"));
    }
  });

  test("the headers, the no-tenant value and the code are the contract's and the identity service's", () => {
    // Renamed in the contract, the nightly would send headers nobody reads and pass nothing; this fails first.
    const identity = readFileSync(join(REPO_ROOT, "crates", "contract", "src", "identity.rs"), "utf8");
    expect(identity).toContain(`pub const EXPECTED_USER_HEADER: &str = "${EXPECTED_USER_HEADER}";`);
    expect(identity).toContain(`pub const EXPECTED_TENANT_HEADER: &str = "${EXPECTED_TENANT_HEADER}";`);
    expect(identity).toContain(`pub const EXPECTED_NO_TENANT: &str = "${EXPECTED_NO_TENANT}";`);
    expect(readFileSync(join(REPO_ROOT, "crates", "contract", "src", "error.rs"), "utf8")).toContain(`"${SESSION_CHANGED}"`);
    // The nightly's provider is the repository's (platform.ts istiodOverlay), so it forwards both.
    const istiod = readFileSync(join(REPO_ROOT, "deploy", "istio", "istiod-values.yaml"), "utf8");
    expect(istiod).toContain(`- ${EXPECTED_USER_HEADER}\n`);
    expect(istiod).toContain(`- ${EXPECTED_TENANT_HEADER}\n`);
  });
});

describe("checkCrossSiteRequests", () => {
  const made = (): TenantApiResult => ({ detail: "", tenantId: TENANT, admin: newSession(), member: newSession() });
  type CrossSiteHeader = "origin" | "sec-fetch-site";

  /**
   * The ingress, the identity service's cookie check and the assets service. The gateway hands the
   * check only the headers in `passes` (includeRequestHeadersInCheck); the check refuses as
   * services/identity/src/csrf.rs does for these requests, then lets the member through; the assets
   * service serves the tenant to a GET and answers 405 to anything else. `override` replaces the
   * gateway's answer to one request. Records what was sent.
   */
  function gateway(result: TenantApiResult, passes: CrossSiteHeader[] = ["origin", "sec-fetch-site"], override?: (request: RawRequest) => RawResponse | undefined) {
    const sent: RawRequest[] = [];
    const header = (request: RawRequest, name: string) => request.headers?.find(([key]) => key === name)?.[1];
    const send = async (request: RawRequest): Promise<RawResponse> => {
      sent.push(request);
      const replaced = override?.(request);
      if (replaced !== undefined) return replaced;
      const method = request.method ?? "GET";
      const safe = method === "GET" || method === "HEAD";
      const seen = (name: CrossSiteHeader) => (passes.includes(name) ? header(request, name) : undefined);
      const site = seen("sec-fetch-site");
      const origin = seen("origin");
      const crossSite =
        site !== undefined ? !(site === "same-origin" || (site === "none" && safe)) : !safe && origin !== undefined && origin !== `https://${request.hostHeader}`;
      if (crossSite) return problem(403, "permission_denied", CROSS_SITE_REQUEST);
      if (header(request, "cookie") !== `${SESSION_COOKIE}=${result.member.cookie}`) return problem(401, "unauthenticated");
      return method === "GET" ? json(200, { tenant_id: TENANT, created_at: "2026-10-14T00:00:01Z" }) : { status: 405, headers: { allow: ["GET,HEAD"] }, body: "", ms: 1 };
    };
    return { deps: { send, log: () => {} }, sent, header };
  }

  test("sends Sec-Fetch-Site and Origin, same-origin and cross-site, and passes when only the cross-site ones are refused by the check", async () => {
    const result = made();
    const run = gateway(result);
    const detail = await checkCrossSiteRequests(run.deps, APP, result);
    expect(detail).toContain(`403 permission_denied ${CROSS_SITE_REQUEST}`);
    expect(detail).toContain(`Origin ${FOREIGN_ORIGIN} (POST)`);
    expect(run.sent.map((request) => [request.method, request.target, request.hostHeader, request.body])).toEqual([
      ["GET", ASSETS_TENANT_PATH, APP, undefined],
      ["GET", ASSETS_TENANT_PATH, APP, undefined],
      ["POST", ASSETS_TENANT_PATH, APP, ""],
      ["POST", ASSETS_TENANT_PATH, APP, ""],
    ]);
    // Every request carries the member's cookie and exactly one of the two headers.
    expect(run.sent.map((request) => request.headers)).toEqual([
      [["cookie", `${SESSION_COOKIE}=${result.member.cookie}`], ["sec-fetch-site", "same-origin"]],
      [["cookie", `${SESSION_COOKIE}=${result.member.cookie}`], ["sec-fetch-site", "cross-site"]],
      [["cookie", `${SESSION_COOKIE}=${result.member.cookie}`], ["origin", `https://${APP}`]],
      [["cookie", `${SESSION_COOKIE}=${result.member.cookie}`], ["origin", FOREIGN_ORIGIN]],
    ]);
  });

  // The misalignment the check is for: istiod without includeRequestHeadersInCheck's origin and
  // sec-fetch-site lets every cross-site request through to the service.
  test("fails when the gateway does not pass the headers to the check, naming the setting", async () => {
    const result = made();
    const failure = checkCrossSiteRequests(gateway(result, []).deps, APP, result);
    await expect(failure).rejects.toThrow("GET with Sec-Fetch-Site: cross-site and the member's cookie answered 200");
    await expect(failure).rejects.toThrow("includeRequestHeadersInCheck");
  });

  test("fails when only one of the two headers reaches the check", async () => {
    const result = made();
    await expect(checkCrossSiteRequests(gateway(result, ["origin"]).deps, APP, result)).rejects.toThrow(
      "GET with Sec-Fetch-Site: cross-site and the member's cookie answered 200",
    );
    await expect(checkCrossSiteRequests(gateway(result, ["sec-fetch-site"]).deps, APP, result)).rejects.toThrow(
      `POST with Origin ${FOREIGN_ORIGIN} and the member's cookie answered 405`,
    );
  });

  test("fails on a 403 that is not the check's cross-site refusal", async () => {
    const result = made();
    const crossSite = (request: RawRequest) => request.headers?.some(([name, value]) => name === "sec-fetch-site" && value === "cross-site");
    for (const answer of [problem(403, "permission_denied"), { status: 403, headers: {}, body: "RBAC: access denied", ms: 1 }, problem(401, "unauthenticated")]) {
      const run = gateway(result, undefined, (request) => (crossSite(request) ? answer : undefined));
      await expect(checkCrossSiteRequests(run.deps, APP, result)).rejects.toThrow(`GET with Sec-Fetch-Site: cross-site and the member's cookie answered ${answer.status}`);
    }
  });

  test("fails when the same-origin controls are refused or reach no service, so the refusals would prove nothing", async () => {
    const result = made();
    const refuseAll = gateway(result, undefined, () => problem(403, "permission_denied", CROSS_SITE_REQUEST));
    await expect(checkCrossSiteRequests(refuseAll.deps, APP, result)).rejects.toThrow(
      "GET with Sec-Fetch-Site: same-origin and the member's cookie answered 403",
    );
    expect(refuseAll.sent).toHaveLength(1);
    const otherTenant = gateway(result, undefined, (request) => ((request.method ?? "GET") === "GET" && request.headers?.some(([, value]) => value === "same-origin") ? json(200, { tenant_id: "1b6f3a52-3c1e-4c39-9d36-2f4f7f6a8e10" }) : undefined));
    await expect(checkCrossSiteRequests(otherTenant.deps, APP, result)).rejects.toThrow(`expected 200 with tenant ${TENANT}`);
    const ownOrigin = (request: RawRequest) => request.headers?.some(([name, value]) => name === "origin" && value === `https://${APP}`);
    for (const answer of [problem(403, "permission_denied", CROSS_SITE_REQUEST), problem(503, "unavailable")]) {
      const run = gateway(result, undefined, (request) => (ownOrigin(request) ? answer : undefined));
      await expect(checkCrossSiteRequests(run.deps, APP, result)).rejects.toThrow(`POST with Origin https://${APP} and the member's cookie answered ${answer.status}`);
    }
  });

  test("never names the cookie in a failure", async () => {
    const result = made();
    const error = await checkCrossSiteRequests(gateway(result, []).deps, APP, result).catch((caught: unknown) => caught);
    expect(error).toBeInstanceOf(Error);
    expect((error as Error).message).not.toContain(result.member.cookie);
  });
});

describe("runApiChecks", () => {
  type Name = keyof ApiChecks;
  const NAMES = Object.keys(API_CHECKS) as Name[];
  const made = (): TenantApiResult => ({ detail: "part checkTenantApi", tenantId: TENANT, admin: newSession(), member: newSession() });

  /**
   * Fake tunnels that record when they open and what they carry, and a stand-in for every check
   * that sends one request through each tunnel its deps reach, then returns a part of the report.
   */
  function fakes() {
    const events: string[] = [];
    const tunnel = (label: string): Tunnel => ({
      open: async () => {
        events.push(`open ${label}`);
      },
      send: async (request) => {
        events.push(`${label} ${request.target}`);
        // The identity service's ext_authz answer, so the steal callbacks get a token.
        return label === "identity" ? { status: 200, headers: { [IDENTITY_HEADER]: ["a.b.c"] }, body: "", ms: 1 } : json(200, {});
      },
    });
    const result = made();
    const io: ApiCheckIo = {
      appHost: APP,
      listenerPort: 80,
      name: "nightly smoke saas",
      gateway: tunnel("gateway"),
      assets: tunnel("assets"),
      identity: tunnel("identity"),
      assistant: tunnel("assistant"),
      psql: async () => "",
      bootstrapAdmin: async () => {
        throw new Error("the stand-in checks never bootstrap");
      },
      clickhouse: async () => "",
      log: () => {},
    };
    const probe = (name: Name): RawRequest => ({ target: `/${name}`, hostHeader: APP });
    const checks: ApiChecks = {
      checkTenantApi: async (deps) => {
        events.push("checkTenantApi");
        await deps.send(probe("checkTenantApi"));
        return result;
      },
      checkTelemetryApi: async (deps) => {
        events.push("checkTelemetryApi");
        await deps.send(probe("checkTelemetryApi"));
        return "part checkTelemetryApi";
      },
      checkTenantAccount: async (query) => {
        events.push("checkTenantAccount");
        await query("SELECT 1", "nobody");
        return "part checkTenantAccount";
      },
      checkIngressEdges: async (deps) => {
        events.push("checkIngressEdges");
        await deps.send(probe("checkIngressEdges"));
        await deps.sendToAssets(probe("checkIngressEdges"));
        await deps.steal();
        return "part checkIngressEdges";
      },
      checkExpectedSession: async (deps) => {
        events.push("checkExpectedSession");
        await deps.send(probe("checkExpectedSession"));
        return "part checkExpectedSession";
      },
      checkAssistantApi: async (deps) => {
        events.push("checkAssistantApi");
        await deps.send(probe("checkAssistantApi"));
        await deps.sendToAssistant(probe("checkAssistantApi"));
        await deps.steal(ASSISTANT_CONVERSATIONS_PATH);
        return "part checkAssistantApi";
      },
      checkAssistantDashboards: async (deps) => {
        events.push("checkAssistantDashboards");
        await deps.send(probe("checkAssistantDashboards"));
        return "part checkAssistantDashboards";
      },
      checkCrossSiteRequests: async (deps) => {
        events.push("checkCrossSiteRequests");
        await deps.send(probe("checkCrossSiteRequests"));
        return "part checkCrossSiteRequests";
      },
    };
    return { io, checks, events, result };
  }

  test("runs every check of API_CHECKS once, in order, each through the tunnel it needs, opened before", async () => {
    const run = fakes();
    const outcome = await runApiChecks(run.io, run.checks);
    expect(run.events.filter((event) => (NAMES as string[]).includes(event))).toEqual(NAMES);
    expect(run.events).toEqual([
      "open gateway",
      "checkTenantApi",
      "gateway /checkTenantApi",
      "checkTelemetryApi",
      "gateway /checkTelemetryApi",
      "checkTenantAccount",
      "open assets",
      "open identity",
      "checkIngressEdges",
      "gateway /checkIngressEdges",
      "assets /checkIngressEdges",
      `identity ${EXT_AUTHZ_PREFIX}${ASSETS_TENANT_PATH}`,
      "checkExpectedSession",
      "gateway /checkExpectedSession",
      "open assistant",
      "checkAssistantApi",
      "gateway /checkAssistantApi",
      "assistant /checkAssistantApi",
      `identity ${EXT_AUTHZ_PREFIX}${ASSISTANT_CONVERSATIONS_PATH}`,
      "checkAssistantDashboards",
      "gateway /checkAssistantDashboards",
      "checkCrossSiteRequests",
      "gateway /checkCrossSiteRequests",
    ]);
    // Every check's part is in the report line, the cross-site one included.
    expect(outcome.detail).toBe(NAMES.map((name) => `part ${name}`).join("; "));
    expect(outcome.platformAdmin).toBe(run.result.admin.userId);
  });

  test("runs the real checks by default, the expected-session one after the ingress edges, the cross-site one last", () => {
    expect(API_CHECKS.checkCrossSiteRequests).toBe(checkCrossSiteRequests);
    expect(API_CHECKS.checkIngressEdges).toBe(checkIngressEdges);
    expect(API_CHECKS.checkExpectedSession).toBe(checkExpectedSession);
    expect(NAMES).toEqual([
      "checkTenantApi",
      "checkTelemetryApi",
      "checkTenantAccount",
      "checkIngressEdges",
      "checkExpectedSession",
      "checkAssistantApi",
      "checkAssistantDashboards",
      "checkCrossSiteRequests",
    ]);
  });
});
