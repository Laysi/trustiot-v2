// Just enough of the PostgreSQL wire protocol to ask a server how it wants a client to log in,
// without a password. A server that answers "authentication OK" to a startup message that
// carries no credentials trusts the client, which must never be the case on a published port.
import { connect, isIP, type Socket } from "node:net";
import { connect as tlsConnect } from "node:tls";

/** The StartupMessage (protocol 3.0) for `user` and `database`. */
export function startupMessage(user: string, database: string): Uint8Array {
  const encoder = new TextEncoder();
  const body = encoder.encode(`user\0${user}\0database\0${database}\0\0`);
  const message = new Uint8Array(8 + body.length);
  const view = new DataView(message.buffer);
  view.setInt32(0, message.length);
  view.setInt32(4, 196608);
  message.set(body, 8);
  return message;
}

export const AUTH_OK = 0;
export const AUTH_CLEARTEXT = 3;
export const AUTH_MD5 = 5;
export const AUTH_SASL = 10;

/**
 * The authentication request in the server's first answer: an AuthenticationRequest ('R') has
 * its type in bytes 5 to 8. Returns "error" for an ErrorResponse ('E') and undefined while the
 * answer is incomplete.
 */
export function authRequest(answer: Uint8Array): number | "error" | undefined {
  if (answer.length < 1) return undefined;
  if (answer[0] === 0x45) return "error";
  if (answer[0] !== 0x52) throw new Error("unexpected answer from the Postgres server");
  if (answer.length < 9) return undefined;
  return new DataView(answer.buffer, answer.byteOffset, answer.byteLength).getInt32(5);
}

/** Connects and returns how the server asks the (credential-less) client to authenticate. */
export function askAuthentication(
  port: number,
  user: string,
  database: string,
  host = "127.0.0.1",
  timeoutMs = 5000,
): Promise<number | "error"> {
  return new Promise((resolve, reject) => {
    const socket = connect({ host, port });
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error("timed out waiting for the Postgres server"));
    }, timeoutMs);
    let received: Uint8Array = new Uint8Array(0);
    socket.once("connect", () => socket.write(startupMessage(user, database)));
    socket.on("data", (chunk) => {
      // No encoding is set on the socket, so chunks arrive as bytes; the string case exists
      // only in the type of the "data" event.
      const bytes = typeof chunk === "string" ? new TextEncoder().encode(chunk) : chunk;
      const next = new Uint8Array(received.length + bytes.length);
      next.set(received);
      next.set(bytes, received.length);
      received = next;
      try {
        const answer = authRequest(received);
        if (answer === undefined) return;
        clearTimeout(timer);
        socket.destroy();
        resolve(answer);
      } catch (error) {
        clearTimeout(timer);
        socket.destroy();
        reject(error);
      }
    });
    socket.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
}

/**
 * Whether `error` is a client's refusal of the server certificate (wrong CA, wrong name,
 * expired), as opposed to a network failure: a check that expects a refusal must not pass just
 * because the server was unreachable.
 */
export function isCertificateRefusal(error: unknown): boolean {
  const code = (error as { code?: unknown } | null)?.code;
  return typeof code === "string" && /CERT|ISSUER|SIGNATURE|CHAIN|VERIFY/.test(code);
}

/** The SSLRequest message: length 8, then the request code 80877103. */
export const SSL_REQUEST = new Uint8Array([0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f]);

export interface TlsTrust {
  /** PEM of the CA that must have signed the server certificate; nothing else is trusted. */
  ca: string;
  /** The name the certificate must carry: a host name or an IP address. */
  serverName: string;
}

/**
 * Like {@link askAuthentication}, but over TLS the way a verify-full client connects: an
 * SSLRequest first, then the handshake, which only succeeds when the server certificate chains
 * to `trust.ca` and names `trust.serverName`. Rejects with the TLS error (its `code` says why
 * the certificate was refused) or, when the server does not offer TLS, with a plain error.
 */
export function askAuthenticationTls(
  port: number,
  user: string,
  database: string,
  trust: TlsTrust,
  host = "127.0.0.1",
  timeoutMs = 5000,
): Promise<number | "error"> {
  return new Promise((resolve, reject) => {
    const raw: Socket = connect({ host, port });
    let secure: ReturnType<typeof tlsConnect> | undefined;
    const timer = setTimeout(
      () => finish(() => reject(new Error("timed out waiting for the Postgres server"))),
      timeoutMs,
    );
    const finish = (action: () => void) => {
      clearTimeout(timer);
      secure?.destroy();
      raw.destroy();
      action();
    };
    raw.once("error", (error) => finish(() => reject(error)));
    raw.once("connect", () => raw.write(SSL_REQUEST));
    raw.once("data", (answer) => {
      const bytes = typeof answer === "string" ? new TextEncoder().encode(answer) : answer;
      if (bytes[0] !== 0x53) {
        finish(() => reject(new Error("the Postgres server does not offer TLS")));
        return;
      }
      // `ca` replaces the default trust store, so only the dev CA is trusted. An IP address is
      // matched against the certificate's IP entries and a name against its DNS entries.
      secure = tlsConnect({
        socket: raw,
        ca: trust.ca,
        ...(isIP(trust.serverName) ? { host: trust.serverName } : { servername: trust.serverName }),
        rejectUnauthorized: true,
        minVersion: "TLSv1.2",
      });
      secure.once("error", (error) => finish(() => reject(error)));
      secure.once("secureConnect", () => secure!.write(startupMessage(user, database)));
      let received: Uint8Array = new Uint8Array(0);
      secure.on("data", (chunk: Uint8Array) => {
        const next = new Uint8Array(received.length + chunk.length);
        next.set(received);
        next.set(chunk, received.length);
        received = next;
        try {
          const outcome = authRequest(received);
          if (outcome !== undefined) finish(() => resolve(outcome));
        } catch (error) {
          finish(() => reject(error));
        }
      });
    });
  });
}
