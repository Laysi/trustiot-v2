// Unit tests of the raw HTTP client of the ingress e2e test (Laysi/trustiot-v2#261): what it
// returns for a response, and which failures it reports as ConnectionError. No cluster needed.
import { afterEach, describe, expect, test } from "bun:test";
import type { Socket } from "bun";
import { HEAD_STALL_TIMEOUT_MS, useProcessTestTimeouts } from "../timing.ts";
import { ConnectionError, decodeChunked, sendRaw } from "./raw-client.ts";

useProcessTestTimeouts();

const servers: { stop(force?: boolean): unknown }[] = [];
afterEach(() => {
  for (const server of servers.splice(0)) server.stop(true);
});

const request = { target: "/a//b/../c?x=1", hostHeader: "app.test:8080", headers: [["x-test", "1"]] as [string, string][] };

// A TCP server that answers with the given bytes (or nothing) once the request head arrived.
function tcpServer(respond: (socket: Socket) => void) {
  const received: string[] = [];
  const server = Bun.listen({
    hostname: "127.0.0.1",
    port: 0,
    socket: {
      data(socket, data) {
        received.push(Buffer.from(data).toString());
        respond(socket);
      },
    },
  });
  servers.push(server);
  return { port: server.port, received };
}

describe("sendRaw", () => {
  test("sends the request target unchanged and parses status, headers and a content-length body", async () => {
    const server = tcpServer((socket) => {
      socket.write("HTTP/1.1 403 Forbidden\r\nContent-Length: 4\r\nX-A: 1\r\nX-A: 2\r\n\r\nnope");
      socket.end();
    });
    const response = await sendRaw({ host: "127.0.0.1", port: server.port, tls: false }, request);
    expect(response.status).toBe(403);
    expect(response.body).toBe("nope");
    expect(response.headers["x-a"]).toEqual(["1", "2"]);
    expect(server.received[0]).toStartWith("GET /a//b/../c?x=1 HTTP/1.1\r\nhost: app.test:8080\r\n");
    expect(server.received[0]).toContain("\r\nx-test: 1\r\n");
  });

  test("sends a body after the head, with its length in bytes", async () => {
    const server = tcpServer((socket) => {
      socket.write("HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}");
      socket.end();
    });
    const body = JSON.stringify({ name: "nightly ü" });
    const response = await sendRaw({ host: "127.0.0.1", port: server.port, tls: false }, { ...request, method: "POST", body });
    expect(response.status).toBe(201);
    const sent = server.received.join("");
    expect(sent).toStartWith("POST /a//b/../c?x=1 HTTP/1.1\r\n");
    expect(sent).toContain(`\r\ncontent-length: ${Buffer.byteLength(body)}\r\n`);
    expect(sent).toEndWith(`\r\n\r\n${body}`);
    // Without a body there is no content-length.
    const plain = tcpServer((socket) => {
      socket.write("HTTP/1.1 204 No Content\r\n\r\n");
      socket.end();
    });
    await sendRaw({ host: "127.0.0.1", port: plain.port, tls: false }, request);
    expect(plain.received.join("")).not.toContain("content-length");
  });

  test("decodes a chunked body", async () => {
    const server = tcpServer((socket) => {
      socket.write("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n");
      socket.end();
    });
    const response = await sendRaw({ host: "127.0.0.1", port: server.port, tls: false }, request);
    expect(response.body).toBe("hello world");
    expect(decodeChunked(Buffer.from("3\r\nabc\r\n0\r\n\r\n")).toString()).toBe("abc");
  });

  test("a connection closed before any response is a ConnectionError", async () => {
    const server = tcpServer((socket) => socket.end());
    const error = await sendRaw({ host: "127.0.0.1", port: server.port, tls: false }, request).catch((e) => e);
    expect(error).toBeInstanceOf(ConnectionError);
    expect(error.message).toContain("no HTTP response for /a//b/../c?x=1");
  });

  test("a response cut inside the head is a ConnectionError", async () => {
    const server = tcpServer((socket) => {
      socket.write("HTTP/1.1 200 OK\r\nContent-Le");
      socket.end();
    });
    await expect(sendRaw({ host: "127.0.0.1", port: server.port, tls: false }, request)).rejects.toBeInstanceOf(
      ConnectionError,
    );
  });

  test("a failure after the response head arrived is not a ConnectionError, so it is never retried", async () => {
    // The gateway answered and the connection broke while the body was still on its way.
    const reset = tcpServer((socket) => {
      socket.write("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 100\r\n\r\npartial");
      socket.flush();
      socket.terminate();
    });
    const resetError = await sendRaw({ host: "127.0.0.1", port: reset.port, tls: false }, request).then(
      () => undefined,
      (e) => e,
    );
    // A reset may also surface as a plain close, which returns the partial response; only a
    // reported error matters here: it must not be a ConnectionError.
    if (resetError !== undefined) expect(resetError).not.toBeInstanceOf(ConnectionError);
    // A stall after the head is the same: a timeout that must not look like a lost connection.
    const stalled = tcpServer((socket) => {
      socket.write("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial");
      socket.flush();
    });
    const stallError = await sendRaw({ host: "127.0.0.1", port: stalled.port, tls: false }, request, HEAD_STALL_TIMEOUT_MS).catch(
      (e) => e,
    );
    expect(stallError).toBeInstanceOf(Error);
    expect(stallError).not.toBeInstanceOf(ConnectionError);
    expect(stallError.message).toContain("timeout");
  });

  test("a refused connection is a ConnectionError", async () => {
    const probe = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data() {} } });
    const port = probe.port;
    probe.stop(true);
    await expect(sendRaw({ host: "127.0.0.1", port, tls: false }, request)).rejects.toBeInstanceOf(ConnectionError);
  });

  test("a response that never comes is a ConnectionError after the timeout", async () => {
    const server = tcpServer(() => {});
    await expect(sendRaw({ host: "127.0.0.1", port: server.port, tls: false }, request, 200)).rejects.toBeInstanceOf(
      ConnectionError,
    );
  });
});
