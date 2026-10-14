import { describe, expect, test } from "bun:test";
import { generateKeyPairSync } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { ConnectionError } from "../../deploy/chart/tests/e2e/raw-client.ts";
import type { RawRequest, RawResponse } from "../../deploy/chart/tests/e2e/raw-client.ts";
import {
  adminSessionSql,
  API_ROUTES,
  apiValues,
  ASSETS_TENANT_PATH,
  checkTenantApi,
  ed25519PublicKeyX,
  errorCode,
  memberSessionSql,
  newSession,
  routesOfValues,
  SESSION_COOKIE,
  SIGNING_KEY_SECRET,
  TENANTS_PATH,
} from "./api.ts";
import { pinnedTool } from "./exec.ts";
import { ingressHostsOf, ingressListenerOf, missingSecrets, missingTlsSecrets, parseManifests, platformSettingsOf } from "./manifests.ts";
import type { K8sObject } from "./manifests.ts";
import { discoverShapes, valuesArgs } from "./smoke.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");
const CHART_DIR = join(REPO_ROOT, "deploy", "chart");
const APP = "app.saas.trustiot.test";
const TENANT = "0b6f3a52-3c1e-4c39-9d36-2f4f7f6a8e10";

describe("the identity service's signing key", () => {
  test("x is the raw public key of the DER that openssl prints, as internalIdentity.publicKeys takes it", () => {
    const { publicKey } = generateKeyPairSync("ed25519");
    const der = publicKey.export({ format: "der", type: "spki" });
    const jwk = publicKey.export({ format: "jwk" });
    expect(ed25519PublicKeyX(new Uint8Array(der))).toBe(jwk.x!);
    expect(ed25519PublicKeyX(new Uint8Array(der))).toMatch(/^[A-Za-z0-9_-]{43}$/);
  });

  test("anything but an Ed25519 public key is refused", () => {
    const { publicKey } = generateKeyPairSync("ec", { namedCurve: "P-256" });
    expect(() => ed25519PublicKeyX(new Uint8Array(publicKey.export({ format: "der", type: "spki" })))).toThrow("not an Ed25519 public key");
    expect(() => ed25519PublicKeyX(new Uint8Array(44))).toThrow("not an Ed25519 public key");
  });
});

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
    const detail = await checkTenantApi(gateway.deps, APP, "nightly smoke saas");
    expect(detail).toContain(`tenant ${TENANT}`);
    expect(detail).toContain("200 with that tenant after 2 s");
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
  const key = { kid: "nightly-1", x: "x" };
  const existing = { host: "gateway", path: "/certs", match: "PathPrefix", service: "certs" };

  test("come after the routes the shape's values set, which helm would otherwise drop", () => {
    const values = apiValues("saas", key, [existing]) as { ingress: { routes: unknown[]; authzProvider: string } };
    expect(values.ingress.routes).toEqual([existing, ...API_ROUTES]);
    expect(values.ingress.authzProvider).toBe("trustiot-smoke-saas");
    expect(() => apiValues("saas", key, [{ ...existing, host: "app", path: "/api/assets" }])).toThrow("already route app /api/assets to certs");
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
    test(`values-${shape.name}.yaml with them renders identity and assets behind the smoke namespace's provider`, async () => {
      const helm = await pinnedTool("helm", REPO_ROOT);
      const dir = mkdtempSync(join(tmpdir(), "nightly-api-values-"));
      let objects: K8sObject[];
      try {
        const file = join(dir, "values-api.yaml");
        const values = apiValues(shape.name, { kid: "nightly-1", x: "jxSgv4kffdHZ1jN2sf_IVpE4VGbJdheBqhCAAqI89Q8" }, routesOfValues(CHART_DIR, shape.valuesFiles));
        writeFileSync(file, Bun.YAML.stringify(values, null, 2));
        const rendered = Bun.spawnSync({
          cmd: [helm, "template", "trustiot", CHART_DIR, "--namespace", `nightly-${shape.name}`, ...valuesArgs(CHART_DIR, shape), "-f", file],
          stdout: "pipe",
          stderr: "pipe",
        });
        expect(rendered.stderr.toString()).toBe("");
        objects = parseManifests(rendered.stdout.toString());
      } finally {
        rmSync(dir, { recursive: true, force: true });
      }
      const named = (kind: string, name: string) => objects.find((o) => o.kind === kind && o.metadata?.name === name);
      expect(named("Deployment", "trustiot-identity")).toBeDefined();
      expect(named("Deployment", "trustiot-assets")).toBeDefined();
      // The cookie check goes to this namespace's identity service (platform.ts).
      const custom = objects.filter((o) => o.kind === "AuthorizationPolicy" && o.spec?.action === "CUSTOM");
      expect(custom.length).toBeGreaterThan(0);
      for (const policy of custom) expect((policy.spec?.provider as { name?: string }).name).toBe(`trustiot-smoke-${shape.name}`);
      // Both APIs are cookie routes on the app host.
      const settings = platformSettingsOf(objects);
      const routes = objects.filter((o) => o.kind === "HTTPRoute").map((o) => JSON.stringify(o.spec));
      for (const path of ["/api/identity", "/api/assets"]) {
        expect(routes.some((r) => r.includes(`"value":"${path}"`) && r.includes(settings.TRUSTIOT_APP_HOST!)), path).toBe(true);
      }
      // The smoke creates the signing key Secret itself, before the dummy Secrets.
      expect([...missingSecrets(objects).keys()]).toContain(SIGNING_KEY_SECRET);
      // What the check reads from the release.
      expect(settings.TRUSTIOT_POSTGRES_DATABASE).toBeTruthy();
      expect(ingressHostsOf(objects)).toHaveLength(3);
      const listener = ingressListenerOf(objects);
      expect(listener).toEqual(shape.name === "saas" ? { gateway: listener.gateway, port: 80, tls: false } : { gateway: listener.gateway, port: 443, tls: true });
      expect(missingTlsSecrets(objects).length).toBe(listener.tls ? 1 : 0);
    });
  }
});
