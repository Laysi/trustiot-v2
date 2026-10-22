// Internal identity tokens for the cluster e2e tests (decision 27, Laysi/trustiot-v2#106): a
// user's token signed with the release's own signing key, in the format the identity service signs
// (services/identity/src/issuer.rs) and every service verifies (crates/service-kit/src/identity/
// token.rs). mesh.e2e.ts reads the key from the Secret the identity pod's init container fills;
// it stays in this process. A token goes to a pod on stdin, never on a command line, and neither a
// key nor a token is printed or recorded: errors here name what is wrong, never the token.
import { createPrivateKey, createPublicKey, type KeyObject, sign, verify } from "node:crypto";

export const TOKEN_TYPE = "internal-identity+jwt";
export const ALGORITHM = "EdDSA";
/** The issuer claim: the identity service's name. */
export const ISSUER = "identity";
/** The lifetime the identity service gives its tokens (TOKEN_LIFETIME_SECS in issuer.rs). */
export const LIFETIME_SECS = 60;
/** The longest lifetime services accept, and the clock skew they allow (service-kit). */
export const MAX_LIFETIME_SECS = 300;
export const LEEWAY_SECS = 5;

/** The signing keys file (crates/identity-keys); `d` is the private key, base64url. */
export interface SigningKeysDocument {
  keys: { kid: string; generation: number; x: string; d: string }[];
}
/** The JWK Set the identity service publishes and every service reads. */
export interface PublishedKeySet {
  keys: { kty: string; crv: string; kid: string; x: string }[];
}
export interface UserClaims {
  sub: string;
  tenant: { id: string; role: "viewer" | "editor" | "admin" } | null;
  platform_admin: boolean;
}
export interface TokenClaims extends UserClaims {
  iss: string;
  aud: string;
  iat: number;
  exp: number;
}

export const unixNow = () => Math.floor(Date.now() / 1000);
const encode = (value: unknown) => Buffer.from(JSON.stringify(value)).toString("base64url");

/** Signs users' tokens with one key of the release. Its key never leaves the instance. */
export class TokenSigner {
  readonly kid: string;
  readonly #key: KeyObject;

  private constructor(kid: string, key: KeyObject) {
    this.kid = kid;
    this.#key = key;
  }

  /**
   * The newest key (highest generation) that `published` lists with the same public key, so every
   * service that reads the published set accepts its tokens.
   */
  static from(document: SigningKeysDocument, published: PublishedKeySet): TokenSigner {
    const listed = (key: SigningKeysDocument["keys"][number]) => published.keys.some((p) => p.kid === key.kid && p.x === key.x);
    const newest = [...document.keys].filter(listed).sort((a, b) => b.generation - a.generation)[0];
    if (!newest) throw new Error("no signing key is in the published key set");
    const key = createPrivateKey({ key: { kty: "OKP", crv: "Ed25519", d: newest.d, x: newest.x }, format: "jwk" });
    return new TokenSigner(newest.kid, key);
  }

  /** A token for `user` addressed to `audience`, issued at `now` for the identity service's lifetime. */
  token(user: UserClaims, audience: string, now = unixNow()): string {
    const claims: TokenClaims = { iss: ISSUER, sub: user.sub, aud: audience, iat: now, exp: now + LIFETIME_SECS, tenant: user.tenant, platform_admin: user.platform_admin };
    const signed = `${encode({ alg: ALGORITHM, typ: TOKEN_TYPE, kid: this.kid })}.${encode(claims)}`;
    return `${signed}.${sign(null, Buffer.from(signed), this.#key).toString("base64url")}`;
  }

  toJSON() {
    return { kid: this.kid };
  }
}

/**
 * The claims of `token` when a service would accept it with `published` at `now`, for any
 * audience; otherwise an error that says why (never the token). The same checks as service-kit,
 * so a test can show that the token it sends is one the services accept.
 */
export function verifyToken(token: string, published: PublishedKeySet, now = unixNow()): TokenClaims {
  const parts = token.split(".");
  if (parts.length !== 3) throw new Error("malformed token: not three segments");
  const [header64 = "", claims64 = "", signature64 = ""] = parts;
  const decode = <T>(segment: string, what: string): T => {
    try {
      return JSON.parse(Buffer.from(segment, "base64url").toString()) as T;
    } catch {
      throw new Error(`malformed token: the ${what} is not base64url JSON`);
    }
  };
  const header = decode<{ alg?: string; typ?: string; kid?: string }>(header64, "header");
  if (header.alg !== ALGORITHM) throw new Error(`wrong algorithm ${JSON.stringify(header.alg)}`);
  if (header.typ?.toLowerCase() !== TOKEN_TYPE) throw new Error(`wrong type ${JSON.stringify(header.typ)}`);
  const jwk = published.keys.find((key) => key.kid === header.kid);
  if (!jwk) throw new Error(`unknown key ${JSON.stringify(header.kid)}`);
  const publicKey = createPublicKey({ key: { kty: "OKP", crv: "Ed25519", x: jwk.x }, format: "jwk" });
  if (!verify(null, Buffer.from(`${header64}.${claims64}`), publicKey, Buffer.from(signature64, "base64url"))) {
    throw new Error("bad signature");
  }
  const claims = decode<TokenClaims>(claims64, "payload");
  if (claims.iss !== ISSUER) throw new Error(`wrong issuer ${JSON.stringify(claims.iss)}`);
  const lifetime = claims.exp - claims.iat;
  if (!(lifetime > 0 && lifetime <= MAX_LIFETIME_SECS)) throw new Error(`invalid lifetime ${lifetime}`);
  if (now >= claims.exp + LEEWAY_SECS) throw new Error("expired");
  if (claims.iat > now + LEEWAY_SECS) throw new Error("not yet valid");
  return claims;
}
