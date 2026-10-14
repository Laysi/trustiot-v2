// What the identity service (services/identity, Laysi/trustiot-v2#61) needs in the local
// environment besides its database: the Ed25519 signing key and the public keys derived from it,
// the cookie routes, and a development session. The database side (roles, schema, grants) is
// applied by the migration Job (lib/migrate.ts), never by this file.
//
// The key and the session cookie live in `<state directory>/identity/` (directory 0700, files
// 0600), never in the repository. `down` keeps that directory, because the sessions in the
// Postgres data volume outlive `down` too; `purge` removes it with the volume.
import { createHash, createPrivateKey, createPublicKey, generateKeyPairSync, randomBytes, randomUUID } from "node:crypto";
import { existsSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

import { ensurePrivateDir, writePrivateFile } from "./state.ts";

/** The entry of the state directory that holds the key and the cookie; `down` keeps it. */
export const IDENTITY_DIR = "identity";
const SIGNING_KEY_FILE = "signing-key.pem";
const COOKIE_FILE = "dev-session-cookie";

/** The service role the Job creates for the identity service (`<schema>_app`). */
export const IDENTITY_APP_ROLE = "identity_app";
/** The service's key in `SERVICES`-style maps, and its chart service key. */
export const IDENTITY_SERVICE = "identity";

/** Name of the session cookie (services/identity/README.md, 「session」). */
export const SESSION_COOKIE_NAME = "__Host-trustiot-session";

/**
 * The development user and tenant, fixed so that seeding twice finds the same rows. They are
 * also written in `services/identity/seed/postgres/001-dev-user.sql`; `identity.test.ts` checks
 * that the two agree.
 */
export const DEV_USER_ID = "00000000-0000-4000-8000-000000000a01";
export const DEV_TENANT_ID = "00000000-0000-4000-8000-000000000b01";
export const DEV_TENANT_ROLE = "admin";

/** A dev session lives 7 days; `seed` makes a new one when less than an hour is left. */
export const SESSION_DAYS = 7;

/** The host the local route table answers for: the `Host` a caller of ext_authz sends. */
export const INGRESS_HOST = "localhost";

export function identityDir(stateDir: string): string {
  return join(stateDir, IDENTITY_DIR);
}

export function signingKeyPath(stateDir: string): string {
  return join(identityDir(stateDir), SIGNING_KEY_FILE);
}

export function cookiePath(stateDir: string): string {
  return join(identityDir(stateDir), COOKIE_FILE);
}

/** Group or other permission bits of a file's mode. */
const WIDER_THAN_OWNER = 0o077;

/**
 * Loads the Ed25519 private key at `path`, refusing a file that others can read (it may have been
 * read already; remove it so a new one is made) or that is not an Ed25519 PKCS#8 PEM. The error
 * never quotes the file.
 */
function loadSigningKey(path: string) {
  if ((statSync(path).mode & WIDER_THAN_OWNER) !== 0) {
    throw new Error(
      `${path} can be read by other users; remove it (the next up makes a new key) or purge the instance`,
    );
  }
  let key;
  try {
    key = createPrivateKey(readFileSync(path, "utf8"));
  } catch {
    throw new Error(`${path} is not a PEM private key; remove it and run up again`);
  }
  if (key.asymmetricKeyType !== "ed25519") {
    throw new Error(`${path} is not an Ed25519 key; remove it and run up again`);
  }
  return key;
}

/**
 * Makes sure the instance has a signing key and returns its path. An existing key is kept (and
 * validated), so the tokens of a running identity service stay valid across `up`; a new one is a
 * PKCS#8 PEM, the format `TRUSTIOT_IDENTITY_SIGNING_KEY_FILE` takes, written with mode 0600.
 */
export function ensureSigningKey(stateDir: string): { path: string; created: boolean } {
  const path = signingKeyPath(stateDir);
  if (existsSync(path)) {
    loadSigningKey(path);
    return { path, created: false };
  }
  ensurePrivateDir(identityDir(stateDir));
  const { privateKey } = generateKeyPairSync("ed25519");
  writePrivateFile(path, privateKey.export({ type: "pkcs8", format: "pem" }) as string);
  return { path, created: true };
}

/**
 * The JWK Set of the signing key's public key, as `TRUSTIOT_INTERNAL_IDENTITY_KEYS` takes it
 * (crates/service-kit/README.md, 「公鑰與輪替」). The `kid` is derived from the public key, so a
 * new key never reuses an old `kid`. Derived from the key file on every call; nothing else holds
 * the public key.
 */
export function publicKeys(stateDir: string): string {
  const jwk = createPublicKey(loadSigningKey(signingKeyPath(stateDir))).export({ format: "jwk" });
  const kid = `dev-${createHash("sha256").update(jwk.x ?? "").digest("hex").slice(0, 12)}`;
  return JSON.stringify({ keys: [{ kty: "OKP", crv: "Ed25519", kid, x: jwk.x }] });
}

/**
 * `TRUSTIOT_INGRESS_ROUTES` for the local environment. There is no ingress gateway here, so the
 * caller of ext_authz sends the `Host` and path itself; every path on `localhost` goes to
 * `assets`, the audience that the kit's example service accepts (`GET /v1/whoami`). The identity
 * service rejects a table that names `ai-gateway`, and the chart's values, not this table, decide
 * the real routes.
 */
export function ingressRoutes(): string {
  return JSON.stringify([{ host: INGRESS_HOST, path: "/", match: "PathPrefix", service: "assets" }]);
}

/** What the identity part of `env` prints; `undefined` when the instance has no key yet. */
export interface IdentityEnv {
  signingKeyFile: string;
  keys: string;
  routes: string;
}

export function identityEnv(stateDir: string): IdentityEnv | undefined {
  const path = signingKeyPath(stateDir);
  if (!existsSync(path)) return undefined;
  return { signingKeyFile: path, keys: publicKeys(stateDir), routes: ingressRoutes() };
}

// --- the development session ----------------------------------------------------------------

/** A cookie value: 32 random bytes, base64url without padding (43 characters). */
export function newCookie(): string {
  return randomBytes(32).toString("base64url");
}

export function isCookieValue(text: string): boolean {
  return /^[A-Za-z0-9_-]{43}$/.test(text) && Buffer.from(text, "base64url").length === 32;
}

/** The SHA-256 of the 32 decoded bytes (not of the text), as hex: what `token_hash` stores. */
export function cookieHashHex(cookie: string): string {
  if (!isCookieValue(cookie)) throw new Error("not a session cookie value");
  return createHash("sha256").update(Buffer.from(cookie, "base64url")).digest("hex");
}

/** The stored dev cookie, when the file exists and holds a well-formed value. */
export function readCookie(stateDir: string): string | undefined {
  const path = cookiePath(stateDir);
  if (!existsSync(path)) return undefined;
  const text = readFileSync(path, "utf8").trim();
  return isCookieValue(text) ? text : undefined;
}

export function writeCookie(stateDir: string, cookie: string): void {
  ensurePrivateDir(identityDir(stateDir));
  writePrivateFile(cookiePath(stateDir), `${cookie}\n`);
}

/**
 * Rows are written as the schema owner, the way services/identity/README.md says a session is
 * made while there is no login (Laysi/trustiot-v2#103): the service role has no write access.
 * `SET ROLE` makes the statements fail when the owner's own privileges would not suffice.
 */
const AS_OWNER = "SET ROLE identity_owner;\n";

/** SQL that counts the dev user's sessions with this cookie that stay valid for another hour. */
export function liveSessionSql(cookie: string): string {
  return (
    AS_OWNER +
    "SELECT count(*) FROM identity.sessions " +
    `WHERE token_hash = decode('${cookieHashHex(cookie)}', 'hex') ` +
    `AND user_id = '${DEV_USER_ID}' AND active_tenant_id = '${DEV_TENANT_ID}' ` +
    "AND revoked_at IS NULL AND expires_at > now() + interval '1 hour';\n"
  );
}

/** SQL that stores a session for `cookie`: only the hash of the cookie reaches the database. */
export function createSessionSql(cookie: string): string {
  return (
    AS_OWNER +
    "INSERT INTO identity.sessions (id, token_hash, user_id, active_tenant_id, expires_at) " +
    `VALUES ('${randomUUID()}', decode('${cookieHashHex(cookie)}', 'hex'), '${DEV_USER_ID}', ` +
    `'${DEV_TENANT_ID}', now() + interval '${SESSION_DAYS} days');\n`
  );
}

/** The `Cookie` header value to send to ext_authz. */
export function cookieHeader(cookie: string): string {
  return `${SESSION_COOKIE_NAME}=${cookie}`;
}

/**
 * What `status` reports about the signing key: present, owner-only, and an Ed25519 PKCS#8 key
 * with a usable public half. Never includes key material.
 */
export function signingKeyCheck(stateDir: string): { ok: boolean; detail: string } {
  const path = signingKeyPath(stateDir);
  if (!existsSync(path)) return { ok: false, detail: "no signing key; run up" };
  try {
    const mode = statSync(path).mode & 0o777;
    const dirMode = statSync(identityDir(stateDir)).mode & 0o777;
    if (mode !== 0o600) return { ok: false, detail: `mode is ${mode.toString(8)}, expected 600` };
    if ((dirMode & WIDER_THAN_OWNER) !== 0) return { ok: false, detail: `directory mode is ${dirMode.toString(8)}` };
    const keys = JSON.parse(publicKeys(stateDir)) as { keys: { kid: string }[] };
    return { ok: true, detail: `Ed25519, mode 600, kid ${keys.keys[0]?.kid}` };
  } catch (error) {
    return { ok: false, detail: error instanceof Error ? error.message : String(error) };
  }
}
