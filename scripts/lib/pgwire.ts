// Just enough of the PostgreSQL wire protocol to ask a server how it wants a client to log in,
// without a password. A server that answers "authentication OK" to a startup message that
// carries no credentials trusts the client, which must never be the case on a published port.
import { connect } from "node:net";

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
      const next = new Uint8Array(received.length + chunk.length);
      next.set(received);
      next.set(chunk, received.length);
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
