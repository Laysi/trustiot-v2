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
  adminSessionSql,
  API_ROUTES,
  apiValues,
  ASSETS_TENANT_PATH,
  ASSISTANT_DASHBOARDS_PATH,
  ASSISTANT_ROLE,
  checkAssistantDashboards,
  checkAssistantLogin,
  DASHBOARD_CHECK_SHAPES,
  checkIngressEdges,
  checkTelemetryApi,
  checkTenantAccount,
  checkTenantApi,
  errorCode,
  EXT_AUTHZ_PREFIX,
  grantsLoggedSql,
  IDENTITY_HEADER,
  memberSessionSql,
  newSession,
  NORMALIZED_TENANT_PATHS,
  roleSessionsSql,
  routesOfValues,
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
import type { EdgeCheckDeps, TenantApiResult } from "./api.ts";
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
    // The assistant's list of saved dashboards (Laysi/trustiot-v2#150).
    const dashboards = readFileSync(join(REPO_ROOT, "services", "assistant", "src", "api", "dashboards.rs"), "utf8");
    expect(dashboards).toContain(`pub const DASHBOARDS_PATH: &str = "${ASSISTANT_DASHBOARDS_PATH}";`);
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
