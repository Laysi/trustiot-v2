// The identity service's local setup: the signing key, the public keys, the routes and the dev
// session (scripts/lib/identity.ts).
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { createHash, createPrivateKey, createPublicKey, generateKeyPairSync, sign, verify } from "node:crypto";
import { chmodSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  cookieHashHex,
  cookiePath,
  createSessionSql,
  DEV_TENANT_ID,
  DEV_USER_ID,
  ensureSigningKey,
  identityDir,
  identityEnv,
  ingressRoutes,
  isCookieValue,
  liveSessionSql,
  newCookie,
  publicKeys,
  readCookie,
  signingKeyCheck,
  signingKeyPath,
  writeCookie,
} from "../lib/identity.ts";

let dir: string;

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "trustiot-identity-test-"));
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

const mode = (path: string) => statSync(path).mode & 0o777;

describe("the signing key", () => {
  test("is a PKCS#8 PEM Ed25519 key, mode 0600 in a 0700 directory of the state directory", () => {
    const { path, created } = ensureSigningKey(dir);
    expect(created).toBe(true);
    expect(path).toBe(signingKeyPath(dir));
    expect(path.startsWith(dir)).toBe(true);
    const pem = readFileSync(path, "utf8");
    expect(pem).toStartWith("-----BEGIN PRIVATE KEY-----");
    expect(createPrivateKey(pem).asymmetricKeyType).toBe("ed25519");
    expect(mode(path)).toBe(0o600);
    expect(mode(identityDir(dir))).toBe(0o700);
  });

  test("is kept by the next run, so tokens of a running identity service stay valid", () => {
    const first = ensureSigningKey(dir);
    const before = readFileSync(first.path, "utf8");
    const second = ensureSigningKey(dir);
    expect(second.created).toBe(false);
    expect(readFileSync(second.path, "utf8")).toBe(before);
  });

  test("is different in every instance", () => {
    const other = mkdtempSync(join(tmpdir(), "trustiot-identity-test-"));
    try {
      ensureSigningKey(dir);
      ensureSigningKey(other);
      expect(readFileSync(signingKeyPath(dir), "utf8")).not.toBe(readFileSync(signingKeyPath(other), "utf8"));
    } finally {
      rmSync(other, { recursive: true, force: true });
    }
  });

  test("a key that other users can read is refused, without quoting it", () => {
    ensureSigningKey(dir);
    chmodSync(signingKeyPath(dir), 0o644);
    const secretText = readFileSync(signingKeyPath(dir), "utf8").split("\n")[1]!;
    let message = "";
    try {
      ensureSigningKey(dir);
    } catch (error) {
      message = (error as Error).message;
    }
    expect(message).toMatch(/can be read by other users/);
    expect(message).not.toContain(secretText);
    expect(() => publicKeys(dir)).toThrow(/other users/);
  });

  test("a file that is not an Ed25519 key is refused without quoting it", () => {
    mkdirSync(identityDir(dir), { recursive: true, mode: 0o700 });
    writeFileSync(signingKeyPath(dir), "not a key, just text\n", { mode: 0o600 });
    let message = "";
    try {
      ensureSigningKey(dir);
    } catch (error) {
      message = (error as Error).message;
    }
    expect(message).toMatch(/is not a PEM private key/);
    expect(message).not.toContain("just text");
  });

  test("an RSA key is refused", () => {
    const rsa = generateKeyPairSync("rsa", { modulusLength: 2048 }).privateKey.export({ type: "pkcs8", format: "pem" }) as string;
    mkdirSync(identityDir(dir), { recursive: true, mode: 0o700 });
    writeFileSync(signingKeyPath(dir), rsa, { mode: 0o600 });
    expect(() => ensureSigningKey(dir)).toThrow(/not an Ed25519 key/);
  });

  test("leaves nothing else behind in the directory", () => {
    ensureSigningKey(dir);
    expect(readdirSync(identityDir(dir))).toEqual(["signing-key.pem"]);
  });
});

describe("the public keys", () => {
  test("are the JWK Set service-kit reads: one OKP Ed25519 key with a kid and no private part", () => {
    ensureSigningKey(dir);
    const set = JSON.parse(publicKeys(dir)) as { keys: Record<string, string>[] };
    expect(set.keys).toHaveLength(1);
    const [key] = set.keys;
    expect(Object.keys(key!).sort()).toEqual(["crv", "kid", "kty", "x"]);
    expect(key!.kty).toBe("OKP");
    expect(key!.crv).toBe("Ed25519");
    expect(key!.kid).toMatch(/^dev-[0-9a-f]{12}$/);
    expect(Buffer.from(key!.x!, "base64url")).toHaveLength(32);
    expect(publicKeys(dir)).not.toContain("PRIVATE");
    expect(publicKeys(dir)).not.toContain('"d"');
  });

  test("verify what the key signs, and nothing else", () => {
    ensureSigningKey(dir);
    const privateKey = createPrivateKey(readFileSync(signingKeyPath(dir), "utf8"));
    const jwk = JSON.parse(publicKeys(dir)).keys[0];
    const publicKey = createPublicKey({ key: { kty: jwk.kty, crv: jwk.crv, x: jwk.x }, format: "jwk" });
    const message = Buffer.from("header.payload");
    expect(verify(null, message, publicKey, sign(null, message, privateKey))).toBe(true);
    expect(verify(null, Buffer.from("other"), publicKey, sign(null, message, privateKey))).toBe(false);
  });

  test("get a kid that follows the key, and stay the same while the key stays", () => {
    ensureSigningKey(dir);
    const first = JSON.parse(publicKeys(dir)).keys[0];
    expect(JSON.parse(publicKeys(dir)).keys[0].kid).toBe(first.kid);
    expect(first.kid).toBe(`dev-${createHash("sha256").update(first.x).digest("hex").slice(0, 12)}`);
    rmSync(signingKeyPath(dir));
    ensureSigningKey(dir);
    expect(JSON.parse(publicKeys(dir)).keys[0].kid).not.toBe(first.kid);
  });
});

describe("the ingress routes", () => {
  test("are the table the identity service accepts: a host, a path, a match and a service", () => {
    const routes = JSON.parse(ingressRoutes()) as Record<string, string>[];
    expect(routes).toEqual([{ host: "localhost", path: "/", match: "PathPrefix", service: "assets" }]);
    // The identity service refuses ai-gateway and requires a lower-case host.
    for (const route of routes) {
      expect(route.service).not.toBe("ai-gateway");
      expect(route.host).toBe(route.host!.toLowerCase());
    }
  });
});

describe("identityEnv", () => {
  test("is absent without a key and carries a path, never the key, with one", () => {
    expect(identityEnv(dir)).toBeUndefined();
    ensureSigningKey(dir);
    const env = identityEnv(dir)!;
    expect(env.signingKeyFile).toBe(signingKeyPath(dir));
    const text = JSON.stringify(env);
    expect(text).not.toContain("PRIVATE KEY");
    expect(text).not.toContain(readFileSync(signingKeyPath(dir), "utf8").split("\n")[1]!);
  });
});

describe("the dev session", () => {
  test("a cookie is 32 random bytes, base64url without padding: 43 characters", () => {
    const cookie = newCookie();
    expect(cookie).toHaveLength(43);
    expect(isCookieValue(cookie)).toBe(true);
    expect(newCookie()).not.toBe(cookie);
    for (const bad of ["", "short", `${cookie}=`, `${cookie}x`, cookie.replace(/.$/, "+"), "!".repeat(43)]) {
      expect(isCookieValue(bad), bad).toBe(false);
    }
  });

  test("the database gets the SHA-256 of the 32 decoded bytes, not of the text", () => {
    const bytes = Buffer.alloc(32, 7);
    const cookie = bytes.toString("base64url");
    expect(cookieHashHex(cookie)).toBe(createHash("sha256").update(bytes).digest("hex"));
    expect(cookieHashHex(cookie)).not.toBe(createHash("sha256").update(cookie).digest("hex"));
    expect(() => cookieHashHex("not a cookie")).toThrow();
  });

  test("the SQL carries the hash and never the cookie, and runs as the schema owner", () => {
    const cookie = newCookie();
    for (const sql of [createSessionSql(cookie), liveSessionSql(cookie)]) {
      expect(sql).not.toContain(cookie);
      expect(sql).toContain(cookieHashHex(cookie));
      expect(sql).toStartWith("SET ROLE identity_owner;\n");
      expect(sql).toContain(DEV_USER_ID);
    }
    expect(createSessionSql(cookie)).toContain(DEV_TENANT_ID);
    expect(createSessionSql(cookie)).toContain("interval '7 days'");
    expect(liveSessionSql(cookie)).toContain("revoked_at IS NULL");
    expect(liveSessionSql(cookie)).toContain("expires_at > now() + interval '1 hour'");
  });

  test("the cookie file is private and read back; a damaged one reads as absent", () => {
    expect(readCookie(dir)).toBeUndefined();
    const cookie = newCookie();
    writeCookie(dir, cookie);
    expect(mode(cookiePath(dir))).toBe(0o600);
    expect(mode(identityDir(dir))).toBe(0o700);
    expect(readCookie(dir)).toBe(cookie);
    writeFileSync(cookiePath(dir), "garbage\n");
    expect(readCookie(dir)).toBeUndefined();
  });

  test("the seed file writes the same user and tenant as the constants", () => {
    const seed = readFileSync(join(import.meta.dir, "..", "..", "services", "identity", "seed", "postgres", "001-dev-user.sql"), "utf8");
    expect(seed).toContain(DEV_USER_ID);
    expect(seed).toContain(DEV_TENANT_ID);
    expect(seed).toContain("SET ROLE identity_owner;");
    expect(seed).toContain("ON CONFLICT");
    expect(seed).not.toMatch(/\bPASSWORD\b|CREATE\s+(USER|ROLE)/i);
  });
});

describe("signingKeyCheck", () => {
  test("passes for a fresh key and says so without key material", () => {
    ensureSigningKey(dir);
    const check = signingKeyCheck(dir);
    expect(check.ok).toBe(true);
    expect(check.detail).toMatch(/Ed25519, mode 600, kid dev-/);
  });

  test("fails without a key, with a wide mode, and with a damaged file", () => {
    expect(signingKeyCheck(dir).ok).toBe(false);
    ensureSigningKey(dir);
    chmodSync(signingKeyPath(dir), 0o640);
    expect(signingKeyCheck(dir)).toMatchObject({ ok: false });
    chmodSync(signingKeyPath(dir), 0o600);
    writeFileSync(signingKeyPath(dir), "damaged\n");
    const check = signingKeyCheck(dir);
    expect(check.ok).toBe(false);
    expect(check.detail).not.toContain("damaged");
  });
});
