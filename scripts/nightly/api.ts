// The smoke test's API check through the ingress (Laysi/trustiot-v2#65): the release runs the
// identity and assets services built from the commit, and the check calls them the way the SPA
// will, through the ingress gateway with a session cookie. A platform administrator creates a
// tenant (POST /api/identity/v1/tenants, 201); the identity service writes its
// identity.tenant_created event in the same transaction, the assets service consumes it, and a
// member of the new tenant then reads the tenant from the assets service (GET
// /api/assets/v1/tenant, 200 with the same ID). Without a cookie the ingress answers 401, and the
// administrator, who has no tenant, gets 403 from the assets service.
//
// Then the edges of the MS0 completion criteria (Laysi/trustiot-v2#77), with the real services:
// a forged identity header, signed with the deployment's own key so the assets service accepts it
// when it reaches it directly (checked through a port-forward that bypasses the ingress), never
// gets through the ingress: without a cookie the gateway answers 401, and with the
// administrator's cookie the assets service sees only the administrator's token (403). A Host
// with a port, and spellings of the path that the mesh normalizes, still reach the assets service
// with a token addressed to it (200 with the tenant), so the identity service resolves the
// audience from the Host and path the gateway hands it, not from what the client wrote.
//
// Nothing logs in yet (Laysi/trustiot-v2#103), so the check makes its two sessions the way the
// service's own tests do: rows in identity.users, identity.memberships and identity.sessions,
// written as the database superuser over the Postgres pod's local socket. Only the SHA-256 of a
// session secret reaches the database and the SQL; the secret itself lives in this process and is
// never logged. The cluster and its sessions are thrown away after the run.
import type { Subprocess } from "bun";
import { createPrivateKey, sign as signWithKey } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { ConnectionError, sendRaw } from "../../deploy/chart/tests/e2e/raw-client.ts";
import type { RawEndpoint, RawRequest, RawResponse } from "../../deploy/chart/tests/e2e/raw-client.ts";
import { freePort } from "../../deploy/chart/tests/e2e/ports.ts";
import { withTunnelRecovery } from "../../deploy/chart/tests/e2e/tunnel-recovery.ts";
import { run } from "./exec.ts";
import type { RunLog } from "./exec.ts";
import { ingressListenerOf, platformSettingsOf, postgresClustersOf, servicePortOf } from "./manifests.ts";
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
export const SIGNING_KEY_STORE = "trustiot-nightly-identity-signing-key";
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
      signingKeySecret: { name: SIGNING_KEY_STORE },
    },
  };
}

/** Signs with the identity service's key, so the check can forge a header the services accept. */
export interface TokenSigner {
  kid: string;
  sign(data: Uint8Array): Uint8Array;
}

/**
 * Generates the identity service's Ed25519 signing key into SIGNING_KEY_STORE in `namespace` and
 * returns its public key for internalIdentity.publicKeys, and a signer for the forged identity
 * header of the check. The private key exists only in a private temporary directory while the
 * commands need it, in the Secret, and in this process as a key object; nothing logs it.
 */
export async function createSigningKey(
  log: RunLog,
  kube: { kubectl: string; kubeconfig: string },
  namespace: string,
  commands: { run: typeof run } = { run },
): Promise<{ publicKey: { kid: string; x: string }; signer: TokenSigner }> {
  const dir = mkdtempSync(join(tmpdir(), "nightly-signing-key-"));
  try {
    const key = join(dir, "signing-key.pem");
    const der = join(dir, "public.der");
    await commands.run(log, ["openssl", "genpkey", "-algorithm", "ed25519", "-out", key], { timeoutMs: 30_000 });
    await commands.run(log, ["openssl", "pkey", "-in", key, "-pubout", "-outform", "DER", "-out", der], { timeoutMs: 30_000 });
    const x = ed25519PublicKeyX(new Uint8Array(await Bun.file(der).arrayBuffer()));
    const privateKey = createPrivateKey(await Bun.file(key).text());
    await commands.run(
      log,
      [kube.kubectl, "create", "secret", "generic", SIGNING_KEY_STORE, "--namespace", namespace, `--from-file=signing-key.pem=${key}`],
      { env: { KUBECONFIG: kube.kubeconfig }, timeoutMs: 60_000, quiet: true },
    );
    return {
      publicKey: { kid: SIGNING_KEY_ID, x },
      signer: { kid: SIGNING_KEY_ID, sign: (data) => new Uint8Array(signWithKey(null, data, privateKey)) },
    };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

/** The protected header of an internal identity token (services/identity/src/issuer.rs), in its field order. */
export function identityTokenHeader(kid: string): string {
  return JSON.stringify({ alg: "EdDSA", typ: "internal-identity+jwt", kid });
}

/**
 * An internal identity token that the client writes itself: a viewer of `tenantId`, addressed to
 * the assets service, valid for a minute from `nowSecs`, in the format and with the key the
 * identity service signs with (services/identity/src/issuer.rs, contract::InternalClaims). The
 * assets service accepts it when it reaches it; only the ingress may keep it away.
 */
export function forgeIdentityToken(signer: TokenSigner, userId: string, tenantId: string, nowSecs: number): string {
  const claims = {
    iss: "identity",
    sub: uuid(userId),
    aud: "assets",
    iat: nowSecs,
    exp: nowSecs + 60,
    tenant: { id: uuid(tenantId), role: "viewer" },
    platform_admin: false,
  };
  const encode = (value: string) => Buffer.from(value, "utf8").toString("base64url");
  const signed = `${encode(identityTokenHeader(signer.kid))}.${encode(JSON.stringify(claims))}`;
  return `${signed}.${Buffer.from(signer.sign(Buffer.from(signed, "ascii"))).toString("base64url")}`;
}

/** The identity header of decision 27, which only the ingress may set (services read it, crates/service-kit). */
export const IDENTITY_HEADER = "x-internal-identity";

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
 * The check itself. `appHost` is hosts.app, where the cookie routes are; `name` is the tenant's
 * name. Returns the report line and what it made; throws with what was awaited or what differed.
 */
export async function checkTenantApi(deps: ApiCheckDeps, appHost: string, name: string): Promise<TenantApiResult> {
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

  const detail =
    `POST ${TENANTS_PATH} as a platform administrator: 201, tenant ${tenantId}; ` +
    `GET ${ASSETS_TENANT_PATH} as its member: 200 with that tenant after ${Math.round(served.waitedMs / 1000)} s; ` +
    "without a cookie: 401 unauthenticated; the administrator without a tenant: 403 permission_denied";
  return { detail, tenantId, admin, member };
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

/** What checkIngressEdges needs besides the gateway: the assets service without the ingress, and the forger. */
export interface EdgeCheckDeps {
  /** Sends one request to the ingress gateway. */
  send: (request: RawRequest) => Promise<RawResponse>;
  /** Sends one request straight to the assets service, past the ingress and the mesh's policies. */
  sendToAssets: (request: RawRequest) => Promise<RawResponse>;
  /** A fresh forged token of a viewer of `tenantId` for the assets service (forgeIdentityToken). */
  forge: (tenantId: string) => string;
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
  const forged = (): [string, string] => [IDENTITY_HEADER, deps.forge(tenantId)];
  const servesTenant = (response: RawResponse): boolean => {
    if (response.status !== 200) return false;
    try {
      return (JSON.parse(response.body) as { tenant_id?: unknown }).tenant_id === tenantId;
    } catch {
      return false;
    }
  };

  // 1. The forged header is one the assets service accepts: sent past the ingress, it serves the tenant.
  const direct = await deps.sendToAssets({ target: ASSETS_TENANT_PATH, hostHeader: appHost, headers: [forged()] });
  if (!servesTenant(direct)) {
    throw new Error(
      `the assets service, called past the ingress with the forged identity header, answered ${summary(direct)}; ` +
        "it must accept it (200 with the tenant), or the forged-header checks below prove nothing",
    );
  }
  deps.log("the assets service accepts the forged identity header when it reaches it");

  // 2. Through the ingress without a cookie: the gateway refuses it before any service.
  const anonymous = await deps.send({ target: ASSETS_TENANT_PATH, hostHeader: appHost, headers: [forged()] });
  if (anonymous.status !== 401 || errorCode(anonymous.body) !== "unauthenticated") {
    throw new Error(`a forged identity header without a cookie answered ${summary(anonymous)} through the ingress, expected 401 unauthenticated`);
  }

  // 3. With the administrator's cookie: the assets service sees the administrator's token only.
  const replaced = await deps.send({ target: ASSETS_TENANT_PATH, hostHeader: appHost, headers: [cookie(admin), forged()] });
  if (replaced.status !== 403 || errorCode(replaced.body) !== "permission_denied") {
    throw new Error(
      `the administrator's cookie with a forged identity header answered ${summary(replaced)} through the ingress, ` +
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
    "a forged identity header that the assets service accepts directly: 401 unauthenticated through the ingress without a cookie, " +
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
 * Runs checkTenantApi against the release in `namespace`: psql through `kubectl exec` in the
 * CloudNativePG primary, requests through `kubectl port-forward` to the ingress gateway's Service
 * (`ca`: the certificate the smoke made for an HTTPS listener).
 */
export async function runApiCheck(
  ctx: ApiCheckContext,
  namespace: string,
  objects: K8sObject[],
  ca: string | undefined,
  name: string,
  signer: TokenSigner,
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

  // SQL on stdin; the output (row counts only) and the SQL stay out of the log.
  const psql = async (sql: string) => {
    await run(
      ctx.log,
      [ctx.kubectl, "exec", "--namespace", namespace, "-i", primary, "-c", "postgres", "--", "psql", "--no-psqlrc", "--quiet", "--tuples-only", "--no-align", "--set", "ON_ERROR_STOP=1", "--dbname", database],
      { env, stdin: sql, timeoutMs: 60_000, quiet: true },
    );
  };

  const gateway = new PortForward(ctx, namespace, `service/${listener.gateway}-istio`, listener.port, listener.tls ? { ca } : undefined);
  // The assets service without the ingress: kubectl port-forward enters its pod, so the request
  // passes neither the gateway nor the mesh's policies, only the service's own token check.
  const direct = new PortForward(ctx, namespace, `service/${assets.service}`, assets.port, undefined);
  try {
    await gateway.open();
    const made = await checkTenantApi({ psql, send: (request) => gateway.send(request), log: (line) => ctx.log.line(line) }, appHost, name);
    await direct.open();
    const edges = await checkIngressEdges(
      {
        send: (request) => gateway.send(request),
        sendToAssets: (request) => direct.send(request),
        forge: (tenantId) => forgeIdentityToken(signer, crypto.randomUUID(), tenantId, Math.floor(Date.now() / 1000)),
        log: (line) => ctx.log.line(line),
      },
      appHost,
      listener.port,
      made,
    );
    return `${made.detail}; ${edges}`;
  } finally {
    gateway.close();
    direct.close();
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
