import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { ConnectionError } from "../../deploy/chart/tests/e2e/raw-client.ts";
import { COMMAND_TIMEOUT_MS, SLOW_TEST_MS, throwIfTimedOut } from "../../deploy/chart/tests/timing.ts";
import type { RawRequest, RawResponse } from "../../deploy/chart/tests/e2e/raw-client.ts";
import {
  adminSessionSql,
  API_ROUTES,
  apiValues,
  ASSETS_TENANT_PATH,
  checkIngressEdges,
  checkTenantApi,
  errorCode,
  EXT_AUTHZ_PREFIX,
  IDENTITY_HEADER,
  memberSessionSql,
  newSession,
  NORMALIZED_TENANT_PATHS,
  routesOfValues,
  SESSION_COOKIE,
  stealIdentityToken,
  TENANTS_PATH,
} from "./api.ts";
import type { EdgeCheckDeps, TenantApiResult } from "./api.ts";
import { pinnedTool } from "./exec.ts";
import { ingressHostsOf, ingressListenerOf, missingSecrets, missingTlsSecrets, parseManifests, platformSettingsOf } from "./manifests.ts";
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

  test("an administrator without a tenant, and a viewer acting in the new tenant", () => {
    const session = newSession();
    const admin = adminSessionSql(session);
    expect(admin).toContain(`INSERT INTO identity.users (id, platform_admin) VALUES ('${session.userId}', true);`);
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
  });
});

const json = (status: number, body: unknown): RawResponse => ({ status, headers: {}, body: JSON.stringify(body), ms: 1 });
const problem = (status: number, code: string) => json(status, { code, message_key: `error.${code}` });

/** A gateway that answers from a script per request kind, and records what it was sent. */
function fakeGateway(script: { admin: RawResponse[]; anonymous: RawResponse[]; create: RawResponse[]; member: RawResponse[] }) {
  const sent: RawRequest[] = [];
  const sql: string[] = [];
  let clock = 0;
  // The administrator's GETs come before the POST, the member's after it.
  let posted = false;
  const deps = {
    psql: async (statement: string) => {
      sql.push(statement);
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
  return { deps, sent, sql };
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
    // The sessions it made are the ones the following checks use.
    expect(gateway.sql[0]).toContain(made.admin.tokenHash);
    expect(gateway.sql[1]).toContain(made.member.tokenHash);
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
    expect(gateway.sql).toHaveLength(2);
    expect(gateway.sql[1]).toContain(`'${TENANT}'`);
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

describe("the API check's routes", () => {
  const existing = { host: "gateway", path: "/certs", match: "PathPrefix", service: "certs" };

  test("come after the routes the shape's values set, which helm would otherwise drop", () => {
    const values = apiValues("saas", [existing]) as { ingress: { routes: unknown[]; authzProvider: string } };
    expect(values.ingress.routes).toEqual([existing, ...API_ROUTES]);
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
    test(`values-${shape.name}.yaml with them renders identity and assets behind the smoke namespace's provider`, async () => {
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
      // The cookie check goes to this namespace's identity service (platform.ts).
      const custom = objects.filter((o) => o.kind === "AuthorizationPolicy" && o.spec?.action === "CUSTOM");
      expect(custom.length).toBeGreaterThan(0);
      for (const policy of custom) expect((policy.spec?.provider as { name?: string }).name).toBe(`trustiot-smoke-${shape.name}`);
      // Both APIs are cookie routes on the app host.
      const settings = platformSettingsOf(objects);
      const routes = objects.filter((o) => o.kind === "HTTPRoute").map((o) => JSON.stringify(o.spec));
      for (const path of ["/api/identity", "/api/assets", "/api/certs"]) {
        expect(routes.some((r) => r.includes(`"value":"${path}"`) && r.includes(settings.TRUSTIOT_APP_HOST!)), path).toBe(true);
      }
      // The certs service from its image, its CRL job every 10 s (Laysi/trustiot-v2#517).
      const certs = named("Deployment", "trustiot-certs");
      expect(certs).toBeDefined();
      const certsContainer = (certs?.spec?.template as { spec: { containers: { name: string; image: string; env: { name: string; value?: string }[] }[] } }).spec.containers.find(
        (c) => c.name === "certs",
      );
      expect(certsContainer?.image).toBe(serviceImageRef("certs"));
      expect(certsContainer?.env.find((e) => e.name === "TRUSTIOT__CRL__CHECK_INTERVAL")?.value).toBe("10s");
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
