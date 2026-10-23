// Cluster-free tests of internal-token.ts, which mesh.e2e.ts uses to send users' tokens
// (Laysi/trustiot-v2#106). Keys are generated per run; none is written down.
import { describe, expect, test } from "bun:test";
import { generateKeyPairSync } from "node:crypto";
import { inspect } from "node:util";

import { LIFETIME_SECS, type PublishedKeySet, type SigningKeysDocument, TOKEN_TYPE, TokenSigner, type UserClaims, verifyToken } from "./internal-token.ts";

function keyPair() {
  const { privateKey } = generateKeyPairSync("ed25519");
  const { d = "", x = "" } = privateKey.export({ format: "jwk" });
  return { d, x };
}
const stored = (kid: string, generation: number) => ({ kid, generation, ...keyPair() });
const publish = (...keys: { kid: string; x: string }[]): PublishedKeySet => ({
  keys: keys.map(({ kid, x }) => ({ kty: "OKP", crv: "Ed25519", kid, x })),
});
const USER: UserClaims = { sub: crypto.randomUUID(), tenant: { id: crypto.randomUUID(), role: "viewer" }, platform_admin: false };
const NOW = 1_800_000_000;
const segment = (token: string, index: number) => JSON.parse(Buffer.from(token.split(".")[index] ?? "", "base64url").toString());

describe("internal identity tokens for the cluster tests", () => {
  const first = stored("2026-10-a", 1);
  const second = stored("2026-10-b", 2);
  const unpublished = stored("2026-10-c", 3);
  const document: SigningKeysDocument = { keys: [first, second, unpublished] };
  const published = publish(first, second);

  test("signs with the newest published key, in the identity service's format", () => {
    const signer = TokenSigner.from(document, published);
    expect(signer.kid).toBe("2026-10-b");
    const token = signer.token(USER, "certs", NOW);
    expect(segment(token, 0)).toEqual({ alg: "EdDSA", typ: TOKEN_TYPE, kid: "2026-10-b" });
    expect(segment(token, 1)).toEqual({ iss: "identity", sub: USER.sub, aud: "certs", iat: NOW, exp: NOW + LIFETIME_SECS, tenant: USER.tenant, platform_admin: false });
    expect(verifyToken(token, published, NOW)).toEqual(segment(token, 1));
  });

  test("a key listed with another public key, or not listed, never signs", () => {
    expect(() => TokenSigner.from({ keys: [unpublished] }, published)).toThrow("no signing key is in the published key set");
    expect(() => TokenSigner.from({ keys: [{ ...second, x: first.x }] }, published)).toThrow("no signing key");
  });

  test("the services' checks: signature, key, type, issuer, lifetime and time", () => {
    const signer = TokenSigner.from(document, published);
    const token = signer.token(USER, "certs", NOW);
    const [header, claims, signature] = token.split(".") as [string, string, string];
    const reencode = (value: unknown) => Buffer.from(JSON.stringify(value)).toString("base64url");
    // Another audience under the old signature.
    expect(() => verifyToken(`${header}.${reencode({ ...segment(token, 1), aud: "assets" })}.${signature}`, published, NOW)).toThrow("bad signature");
    expect(() => verifyToken(token, publish(first), NOW)).toThrow("unknown key");
    expect(() => verifyToken(token, publish(first, { kid: "2026-10-b", x: first.x }), NOW)).toThrow("bad signature");
    expect(() => verifyToken(`${reencode({ alg: "none", typ: TOKEN_TYPE, kid: "2026-10-b" })}.${claims}.`, published, NOW)).toThrow("wrong algorithm");
    expect(() => verifyToken(`${reencode({ alg: "EdDSA", typ: "JWT", kid: "2026-10-b" })}.${claims}.${signature}`, published, NOW)).toThrow("wrong type");
    expect(() => verifyToken("a.b", published, NOW)).toThrow("not three segments");
    expect(() => verifyToken(token, published, NOW + LIFETIME_SECS + 5)).toThrow("expired");
    expect(verifyToken(token, published, NOW + LIFETIME_SECS + 4).aud).toBe("certs");
    expect(() => verifyToken(token, published, NOW - 6)).toThrow("not yet valid");
  });

  test("neither the signer nor an error shows a key or a token", () => {
    const signer = TokenSigner.from(document, published);
    for (const text of [JSON.stringify(signer), inspect(signer), String(signer)]) {
      for (const key of document.keys) expect(text).not.toContain(key.d);
    }
    const token = signer.token(USER, "certs", NOW);
    try {
      verifyToken(token, published, NOW + 3600);
      throw new Error("accepted an expired token");
    } catch (error) {
      expect(String(error)).not.toContain(token.split(".")[2]);
      expect(String(error)).not.toContain(token.split(".")[1]);
    }
  });
});
