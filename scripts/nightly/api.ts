// The smoke test's API check through the ingress (Laysi/trustiot-v2#65): the release runs the
// identity and assets services built from the commit, and the check calls them the way the SPA
// will, through the ingress gateway with a session cookie. A platform administrator creates a
// tenant (POST /api/identity/v1/tenants, 201); the identity service writes its
// identity.tenant_created event in the same transaction, the assets service consumes it, and a
// member of the new tenant then reads the tenant from the assets service (GET
// /api/assets/v1/tenant, 200 with the same ID). Without a cookie the ingress answers 401, and the
// administrator, who has no tenant, gets 403 from the assets service.
//
// Nothing logs in yet (Laysi/trustiot-v2#103), so the check makes its two sessions the way the
// service's own tests do: rows in identity.users, identity.memberships and identity.sessions,
// written as the database superuser over the Postgres pod's local socket. Only the SHA-256 of a
// session secret reaches the database and the SQL; the secret itself lives in this process and is
// never logged. The cluster and its sessions are thrown away after the run.
import type { Subprocess } from "bun";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { ConnectionError, sendRaw } from "../../deploy/chart/tests/e2e/raw-client.ts";
import type { RawEndpoint, RawRequest, RawResponse } from "../../deploy/chart/tests/e2e/raw-client.ts";
import { freePort } from "../../deploy/chart/tests/e2e/ports.ts";
import { withTunnelRecovery } from "../../deploy/chart/tests/e2e/tunnel-recovery.ts";
import { run } from "./exec.ts";
import type { RunLog } from "./exec.ts";
import { ingressListenerOf, platformSettingsOf, postgresClustersOf } from "./manifests.ts";
import type { K8sObject } from "./manifests.ts";
import { smokeProviderNameOf } from "./platform.ts";
import { serviceImageValues } from "./service-image.ts";

/** The identity service's session cookie (services/identity/src/session.rs, SESSION_COOKIE). */
export const SESSION_COOKIE = "__Host-trustiot-session";
/** The identity service's tenant API (services/identity/src/tenants.rs, TENANTS_PATH). */
export const TENANTS_PATH = "/api/identity/v1/tenants";
/** The assets service's tenant API (services/assets/src/tenants.rs, TENANT_PATH). */
export const ASSETS_TENANT_PATH = "/api/assets/v1/tenant";
/** The Secret with the identity service's private key, which the smoke creates in its namespace. */
export const SIGNING_KEY_SECRET = "trustiot-nightly-identity-signing-key";
/** The key ID of that key in internalIdentity.publicKeys. */
export const SIGNING_KEY_ID = "nightly-1";

/** How long the ingress may take to route the cookie check, and the assets service to consume the event. */
export const ROUTE_LIMIT_MS = 180_000;
export const EVENT_LIMIT_MS = 180_000;
const POLL_MS = 2_000;

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** The DER prefix of an Ed25519 SubjectPublicKeyInfo (RFC 8410); the 32-byte key follows it. */
const ED25519_SPKI_PREFIX = Buffer.from("302a300506032b6570032100", "hex");

/**
 * The `x` of internalIdentity.publicKeys (base64url of the raw 32-byte key, no padding) from the
 * DER public key that `openssl pkey -pubout -outform DER` prints for an Ed25519 key.
 */
export function ed25519PublicKeyX(der: Uint8Array): string {
  const bytes = Buffer.from(der);
  if (bytes.length !== ED25519_SPKI_PREFIX.length + 32 || !bytes.subarray(0, ED25519_SPKI_PREFIX.length).equals(ED25519_SPKI_PREFIX)) {
    throw new Error(`not an Ed25519 public key in DER (${bytes.length} bytes)`);
  }
  return bytes.subarray(ED25519_SPKI_PREFIX.length).toString("base64url");
}

/** One entry of the chart's ingress.routes. */
export interface IngressRoute {
  host: string;
  path: string;
  match?: string;
  service: string;
  [key: string]: unknown;
}

/** The cookie routes the check needs: the identity service's tenant API and the assets service's API. */
export const API_ROUTES: readonly IngressRoute[] = [
  { host: "app", path: "/api/identity", match: "PathPrefix", service: "identity" },
  { host: "app", path: "/api/assets", match: "PathPrefix", service: "assets" },
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
 * The values the smoke adds for the check (after tests/values/smoke.yaml): the identity and assets
 * services from the images the nightly built, the cookie routes to their APIs after the routes the
 * shape's values already set (`routes`, from routesOfValues; helm would otherwise drop them), the
 * smoke namespace's ext_authz provider (platform.ts), and the signing key the smoke generated.
 */
export function apiValues(shape: string, publicKey: { kid: string; x: string }, routes: readonly IngressRoute[]): Record<string, unknown> {
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
    },
    ingress: {
      authzProvider: smokeProviderNameOf(shape),
      routes: [...routes, ...API_ROUTES],
    },
    internalIdentity: {
      publicKeys: [publicKey],
      signingKeySecret: { name: SIGNING_KEY_SECRET },
    },
  };
}

/**
 * Generates the identity service's Ed25519 signing key into SIGNING_KEY_SECRET in `namespace` and
 * returns its public key for internalIdentity.publicKeys. The private key exists only in a private
 * temporary directory while the commands need it and in the Secret; nothing logs it.
 */
export async function createSigningKey(
  log: RunLog,
  kube: { kubectl: string; kubeconfig: string },
  namespace: string,
  commands: { run: typeof run } = { run },
): Promise<{ kid: string; x: string }> {
  const dir = mkdtempSync(join(tmpdir(), "nightly-signing-key-"));
  try {
    const key = join(dir, "signing-key.pem");
    const der = join(dir, "public.der");
    await commands.run(log, ["openssl", "genpkey", "-algorithm", "ed25519", "-out", key], { timeoutMs: 30_000 });
    await commands.run(log, ["openssl", "pkey", "-in", key, "-pubout", "-outform", "DER", "-out", der], { timeoutMs: 30_000 });
    const x = ed25519PublicKeyX(new Uint8Array(await Bun.file(der).arrayBuffer()));
    await commands.run(
      log,
      [kube.kubectl, "create", "secret", "generic", SIGNING_KEY_SECRET, "--namespace", namespace, `--from-file=signing-key.pem=${key}`],
      { env: { KUBECONFIG: kube.kubeconfig }, timeoutMs: 60_000, quiet: true },
    );
    return { kid: SIGNING_KEY_ID, x };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
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

/** The `code` of an ErrorBody, or undefined when the body is not one. */
export function errorCode(body: string): string | undefined {
  try {
    const parsed: unknown = JSON.parse(body);
    const code = typeof parsed === "object" && parsed !== null ? (parsed as { code?: unknown }).code : undefined;
    return typeof code === "string" ? code : undefined;
  } catch {
    return undefined;
  }
}

function summary(response: RawResponse): string {
  return `${response.status} ${response.body.slice(0, 200).replaceAll("\n", " ")}`;
}

/**
 * The check itself. `appHost` is hosts.app, where the cookie routes are; `name` is the tenant's
 * name. Returns the report line; throws with what was awaited or what differed.
 */
export async function checkTenantApi(deps: ApiCheckDeps, appHost: string, name: string): Promise<string> {
  const sleep = deps.sleep ?? Bun.sleep;
  const now = deps.now ?? Date.now;
  const routeMs = deps.limits?.routeMs ?? ROUTE_LIMIT_MS;
  const eventMs = deps.limits?.eventMs ?? EVENT_LIMIT_MS;
  const pollMs = deps.limits?.pollMs ?? POLL_MS;
  const get = (session?: TestSession): RawRequest => ({
    target: ASSETS_TENANT_PATH,
    hostHeader: appHost,
    headers: session === undefined ? [] : [["cookie", `${SESSION_COOKIE}=${session.cookie}`]],
  });
  /** Polls until `done` accepts a response, a response `fatal` rejects, or `limitMs` passes. */
  async function poll(what: string, limitMs: number, request: RawRequest, done: (r: RawResponse) => boolean, pending: (r: RawResponse) => boolean): Promise<{ response: RawResponse; waitedMs: number }> {
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

  return (
    `POST ${TENANTS_PATH} as a platform administrator: 201, tenant ${tenantId}; ` +
    `GET ${ASSETS_TENANT_PATH} as its member: 200 with that tenant after ${Math.round(served.waitedMs / 1000)} s; ` +
    "without a cookie: 401 unauthenticated; the administrator without a tenant: 403 permission_denied"
  );
}

/** Where the check runs: the smoke cluster and its log. */
export interface ApiCheckContext {
  kubeconfig: string;
  kubectl: string;
  log: RunLog;
}

/**
 * Runs checkTenantApi against the release in `namespace`: psql through `kubectl exec` in the
 * CloudNativePG primary, requests through `kubectl port-forward` to the ingress gateway's Service
 * (`ca`: the certificate the smoke made for an HTTPS listener).
 */
export async function runApiCheck(ctx: ApiCheckContext, namespace: string, objects: K8sObject[], ca: string | undefined, name: string): Promise<string> {
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

  // SQL on stdin; the output (row counts only) and the SQL stay out of the log.
  const psql = async (sql: string) => {
    await run(
      ctx.log,
      [ctx.kubectl, "exec", "--namespace", namespace, "-i", primary, "-c", "postgres", "--", "psql", "--no-psqlrc", "--quiet", "--tuples-only", "--no-align", "--set", "ON_ERROR_STOP=1", "--dbname", database],
      { env, stdin: sql, timeoutMs: 60_000, quiet: true },
    );
  };

  let tunnel: Subprocess | undefined;
  let endpoint: RawEndpoint | undefined;
  const openTunnel = async () => {
    tunnel?.kill();
    const port = await freePort();
    ctx.log.line(`$ ${ctx.kubectl} port-forward --namespace ${namespace} service/${listener.gateway}-istio ${port}:${listener.port}`);
    tunnel = Bun.spawn([ctx.kubectl, "port-forward", "--namespace", namespace, `service/${listener.gateway}-istio`, `${port}:${listener.port}`], {
      env: { ...process.env, ...env },
      stdout: "ignore",
      stderr: "ignore",
    });
    endpoint = { host: "127.0.0.1", port, tls: listener.tls, ...(ca === undefined ? {} : { ca }) };
    const started = Date.now();
    for (;;) {
      try {
        const socket = await Bun.connect({ hostname: "127.0.0.1", port, socket: { data() {} } });
        socket.end();
        return;
      } catch {
        if (tunnel.exitCode !== null) throw new Error(`kubectl port-forward to ${listener.gateway}-istio exited with ${tunnel.exitCode}`);
        if (Date.now() - started > 60_000) throw new Error(`timed out after 60 s waiting for kubectl port-forward to ${listener.gateway}-istio to listen`);
        await Bun.sleep(500);
      }
    }
  };
  try {
    await openTunnel();
    const send = (request: RawRequest) => {
      const attempt = () => sendRaw(endpoint!, request);
      // Only a GET is sent again after the tunnel was lost: a POST may have reached the service.
      if ((request.method ?? "GET") !== "GET") return attempt();
      return withTunnelRecovery(attempt, { tunnel: () => tunnel, reopen: openTunnel, log: (line) => ctx.log.line(line) });
    };
    return await checkTenantApi({ psql, send, log: (line) => ctx.log.line(line) }, appHost, name);
  } finally {
    tunnel?.kill();
  }
}
