// Checks the shape of a Cloudflare Tunnel token without ever printing it.
//
// A remotely managed tunnel's token is base64 of a JSON object: `a` is the account tag, `t`
// the tunnel ID and `s` the tunnel secret. Only the tunnel ID is shown, so the operator can
// confirm the right tunnel; error messages never quote the input.

export interface TunnelToken {
  /** The token as cloudflared reads it: one line, no surrounding whitespace. */
  value: string;
  tunnelId: string;
}

const MAX_TOKEN_BYTES = 4096;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const BASE64 = /^[A-Za-z0-9+/]+={0,2}$/;

export class TokenError extends Error {}

export function parseTunnelToken(input: Uint8Array): TunnelToken {
  if (input.byteLength > MAX_TOKEN_BYTES) {
    throw new TokenError(`the tunnel token is longer than ${MAX_TOKEN_BYTES} bytes`);
  }
  const value = new TextDecoder("utf-8", { fatal: false }).decode(input).trim();
  if (value === "") throw new TokenError("the tunnel token is empty");
  if (!BASE64.test(value)) {
    throw new TokenError("the tunnel token is not one line of base64 (copy the token, not the install command)");
  }
  let payload: unknown;
  try {
    payload = JSON.parse(Buffer.from(value, "base64").toString("utf-8"));
  } catch {
    throw new TokenError("the tunnel token does not decode to JSON");
  }
  if (typeof payload !== "object" || payload === null) {
    throw new TokenError("the tunnel token does not decode to a JSON object");
  }
  const fields = payload as Record<string, unknown>;
  for (const key of ["a", "t", "s"]) {
    if (typeof fields[key] !== "string" || fields[key] === "") {
      throw new TokenError(`the tunnel token has no "${key}" field`);
    }
  }
  const tunnelId = (fields.t as string).toLowerCase();
  if (!UUID.test(tunnelId)) throw new TokenError("the tunnel token's tunnel ID is not a UUID");
  return { value, tunnelId };
}
