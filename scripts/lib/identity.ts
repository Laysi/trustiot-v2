// What the identity service (services/identity, Laysi/trustiot-v2#61) needs in the local
// environment besides its database: the Ed25519 signing key and the public keys derived from it,
// the cookie routes, and a development session. The database side (roles, schema, grants) is
// applied by the migration Job (lib/migrate.ts), never by this file.
//
// The key and the session cookie live in `<state directory>/identity/` (directory 0700, files
// 0600), never in the repository. `down` keeps that directory, because the sessions in the
// Postgres data volume outlive `down` too; `purge` removes it with the volume. From the key,
// `up` writes the two files a cluster's identity pod gets from its init container
// (Laysi/trustiot-v2#404): the signing keys file the identity service reads
// (`TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE`) and the public keys file every service reads
// (`TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`).
import { createHash, createPrivateKey, createPublicKey, generateKeyPairSync, randomBytes, randomUUID } from "node:crypto";
import { existsSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

import { ensurePrivateDir, writePrivateFile } from "./state.ts";

/** The entry of the state directory that holds the key and the cookie; `down` keeps it. */
export const IDENTITY_DIR = "identity";
const SIGNING_KEY_FILE = "signing-key.pem";
const SIGNING_KEYS_FILE = "signing-keys.json";
const PUBLIC_KEYS_FILE = "jwks.json";
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

/** The signing keys file (crates/identity-keys), written from the key by `up`. */
export function signingKeysPath(stateDir: string): string {
  return join(identityDir(stateDir), SIGNING_KEYS_FILE);
}

/** The public keys file, a JWK Set, written from the key by `up`. */
export function publicKeysPath(stateDir: string): string {
  return join(identityDir(stateDir), PUBLIC_KEYS_FILE);
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
 * PKCS#8 PEM, written with mode 0600. Then writes the signing keys file and the public keys file
 * from it (mode 0600 each, replaced atomically), so a running service rereads the same content.
 */
export function ensureSigningKey(stateDir: string): { path: string; created: boolean } {
  const path = signingKeyPath(stateDir);
  let created = false;
  if (existsSync(path)) {
    loadSigningKey(path);
  } else {
    ensurePrivateDir(identityDir(stateDir));
    const { privateKey } = generateKeyPairSync("ed25519");
    writePrivateFile(path, privateKey.export({ type: "pkcs8", format: "pem" }) as string);
    created = true;
  }
  writePrivateFile(signingKeysPath(stateDir), signingKeys(stateDir));
  writePrivateFile(publicKeysPath(stateDir), publicKeys(stateDir));
  return { path, created };
}

/** The key id of the instance's key: derived from the public key, so a new key never reuses one. */
function keyId(x: string): string {
  return `dev-${createHash("sha256").update(x).digest("hex").slice(0, 12)}`;
}

/**
 * The signing keys file of the instance's key, in the format the identity pod's init container
 * writes (crates/identity-keys): one key of generation 1, published and signing from the start.
 */
function signingKeys(stateDir: string): string {
  const jwk = loadSigningKey(signingKeyPath(stateDir)).export({ format: "jwk" });
  const key = { kid: keyId(jwk.x ?? ""), generation: 1, x: jwk.x, d: jwk.d, published_at: 0, signs_from: 0 };
  return JSON.stringify({ keys: [key] });
}

/**
 * The JWK Set of the signing key's public key, the content of the file
 * `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` names (crates/service-kit/README.md, 「公鑰與輪替」). The
 * `kid` is derived from the public key, so a new key never reuses an old `kid`. Derived from the
 * key file on every call.
 */
export function publicKeys(stateDir: string): string {
  const jwk = createPublicKey(loadSigningKey(signingKeyPath(stateDir))).export({ format: "jwk" });
  return JSON.stringify({ keys: [{ kty: "OKP", crv: "Ed25519", kid: keyId(jwk.x ?? ""), x: jwk.x }] });
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

/** What the identity part of `env` prints (paths, never a key); `undefined` before `up` wrote the files. */
export interface IdentityEnv {
  signingKeysFile: string;
  publicKeysFile: string;
  routes: string;
}

export function identityEnv(stateDir: string): IdentityEnv | undefined {
  const signingKeysFile = signingKeysPath(stateDir);
  const publicKeysFile = publicKeysPath(stateDir);
  if (!existsSync(signingKeysFile) || !existsSync(publicKeysFile)) return undefined;
  return { signingKeysFile, publicKeysFile, routes: ingressRoutes() };
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
 * Rows are written as the schema owner, the way services/identity/README.md says test tooling
 * makes a session: the local environment has no mail relay for the email login
 * (Laysi/trustiot-v2#103), and the service role may not write users or memberships. A session
 * lives at most 7 days from its creation (the database refuses longer ones). `SET ROLE` makes
 * the statements fail when the owner's own privileges would not suffice.
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
 * with a usable public half, and the signing keys and public keys files written from it. Never
 * includes key material.
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
    for (const [file, expected] of [
      [signingKeysPath(stateDir), signingKeys(stateDir)],
      [publicKeysPath(stateDir), publicKeys(stateDir)],
    ] as const) {
      if (!existsSync(file) || readFileSync(file, "utf8") !== expected) {
        return { ok: false, detail: `${file} is missing or not written from the key; run up` };
      }
      const fileMode = statSync(file).mode & 0o777;
      if (fileMode !== 0o600) return { ok: false, detail: `${file}: mode is ${fileMode.toString(8)}, expected 600` };
    }
    return { ok: true, detail: `Ed25519, mode 600, kid ${keys.keys[0]?.kid}, key files written` };
  } catch (error) {
    return { ok: false, detail: error instanceof Error ? error.message : String(error) };
  }
}
