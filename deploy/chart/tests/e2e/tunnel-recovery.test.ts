// Unit tests of the tunnel recovery of the ingress e2e test (Laysi/trustiot-v2#261). They need no
// cluster: a real HTTP server plays the gateway and a child process (fake-port-forward.ts) plays
// `kubectl port-forward`, including dying while a request is in flight. Runs with
// `bun test deploy/chart`.
import { afterEach, describe, expect, test } from "bun:test";
import { join } from "node:path";
import type { Subprocess } from "bun";
import { freePort } from "./ports.ts";
import { ConnectionError, type RawEndpoint, type RawRequest, sendRaw } from "./raw-client.ts";
import { COMMAND_TIMEOUT_MS, HEAD_STALL_TIMEOUT_MS, useProcessTestTimeouts } from "../timing.ts";
import { withTunnelRecovery } from "./tunnel-recovery.ts";

// Each test starts Bun child processes, which a busy machine starts slowly: see ../timing.ts.
useProcessTestTimeouts();

const FAKE_PORT_FORWARD = join(import.meta.dir, "fake-port-forward.ts");
const REQUEST: RawRequest = { target: "/api/whoami", hostHeader: "app.saas.trustiot.test" };
// How long a healthy local server may take to answer, and how long a tunnel that is about to
// exit may take to do so. Both end as soon as the event happens, so they only bound a failure.
const REQUEST_TIMEOUT_MS = 30_000;
const GRACE_MS = 30_000;
// The grace period of the test that must not wait for it: a tunnel that exited long ago is
// reopened at once, so the test finishes within a fraction of this.
const UNUSED_GRACE_MS = 120_000;

const processes: Subprocess[] = [];
const servers: { stop(force?: boolean): unknown }[] = [];

afterEach(() => {
  for (const proc of processes.splice(0)) proc.kill();
  for (const server of servers.splice(0)) server.stop(true);
});

// Waits until the proxy accepts connections. A proxy that exited instead fails the test at once,
// with its exit code, rather than after the whole wait.
async function waitListening(port: number, proc: Subprocess): Promise<void> {
  const started = Date.now();
  const deadline = started + COMMAND_TIMEOUT_MS;
  while (Date.now() < deadline) {
    if (proc.exitCode !== null || proc.signalCode !== null) {
      throw new Error(`fake-port-forward exited (${proc.exitCode ?? proc.signalCode}) before listening on ${port}`);
    }
    try {
      const socket = await Bun.connect({ hostname: "127.0.0.1", port, socket: { data() {} } });
      socket.end();
      return;
    } catch {
      await Bun.sleep(20);
    }
  }
  throw new Error(`nothing listened on ${port} after ${Date.now() - started} ms`);
}

// The gateway: answers every request with the current status and the body "gateway".
function startGateway(status = 200): { port: number; requests: () => number; setStatus: (s: number) => void } {
  let current = status;
  let count = 0;
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch() {
      count++;
      return new Response("gateway", { status: current });
    },
  });
  servers.push(server);
  return { port: server.port as number, requests: () => count, setStatus: (s) => (current = s) };
}

async function startProxy(
  upstreamPort: number,
  mode: "forward" | "die-on-request",
  exitDelay: number | "on-stdin" = 300,
): Promise<{ port: number; proc: Subprocess<"pipe", "ignore", "inherit"> }> {
  const port = await freePort();
  const proc = Bun.spawn(
    [process.execPath, FAKE_PORT_FORWARD, String(port), String(upstreamPort), mode, String(exitDelay)],
    { stdin: "pipe", stdout: "ignore", stderr: "inherit" },
  );
  processes.push(proc);
  await waitListening(port, proc);
  return { port, proc };
}

// The client side of the e2e test: an endpoint, the tunnel behind it and a reopen that starts a
// healthy tunnel, like openTunnel() in ingress.e2e.ts.
async function scenario(first: "forward" | "die-on-request", exitDelay?: number | "on-stdin", gatewayStatus = 200) {
  const gateway = startGateway(gatewayStatus);
  const initial = await startProxy(gateway.port, first, exitDelay);
  const state = {
    gateway,
    endpoint: { host: "127.0.0.1", port: initial.port, tls: false } as RawEndpoint,
    tunnel: initial.proc as Subprocess | undefined,
    attempts: 0,
    reopens: 0,
    logs: [] as string[],
  };
  const attempt = () => {
    state.attempts++;
    return sendRaw(state.endpoint, REQUEST, REQUEST_TIMEOUT_MS);
  };
  const options = (graceMs = GRACE_MS) => ({
    tunnel: () => state.tunnel,
    reopen: async () => {
      state.reopens++;
      const next = await startProxy(gateway.port, "forward");
      state.endpoint = { host: "127.0.0.1", port: next.port, tls: false };
      state.tunnel = next.proc;
    },
    graceMs,
    log: (message: string) => state.logs.push(message),
  });
  return { state, attempt, options };
}

describe("tunnel dies while a request is in flight", () => {
  test("the request fails before the process has exited, the way kubectl behaves (fault injection)", async () => {
    // The proxy exits only when the test says so, after it has seen the request fail: the order
    // under test does not depend on how fast this machine is.
    const { state } = await scenario("die-on-request", "on-stdin");
    let failure: unknown;
    try {
      await sendRaw(state.endpoint, REQUEST, REQUEST_TIMEOUT_MS);
    } catch (error) {
      failure = error;
    }
    expect(failure).toBeInstanceOf(ConnectionError);
    // The check that raw() made before #261 looked at exitCode and signalCode right here and
    // rethrew, because the process had not exited yet.
    const tunnel = state.tunnel as Subprocess;
    expect(tunnel.exitCode === null && tunnel.signalCode === null).toBe(true);
    const stdin = (tunnel as Subprocess<"pipe">).stdin;
    stdin.write("exit\n");
    await stdin.flush();
    await tunnel.exited;
    expect(tunnel.exitCode).toBe(1);
  });

  test("reopens the tunnel and sends the request once more", async () => {
    const { state, attempt, options } = await scenario("die-on-request", 300);
    const response = await withTunnelRecovery(attempt, options());
    expect(response.status).toBe(200);
    expect(response.body).toBe("gateway");
    expect(state.attempts).toBe(2);
    expect(state.reopens).toBe(1);
    expect(state.gateway.requests()).toBe(1);
    expect(state.logs).toHaveLength(1);
    expect(state.logs[0]).toContain("reopening");
  });

  test("a tunnel that exited long before the request is reopened without waiting", async () => {
    const { state, attempt, options } = await scenario("forward");
    state.tunnel?.kill();
    await state.tunnel?.exited;
    state.endpoint = { ...state.endpoint, port: await freePort() };
    const started = performance.now();
    const response = await withTunnelRecovery(attempt, options(UNUSED_GRACE_MS));
    expect(response.status).toBe(200);
    expect(performance.now() - started).toBeLessThan(UNUSED_GRACE_MS / 2);
    expect(state.reopens).toBe(1);
  });

  test("a healthy tunnel needs no recovery", async () => {
    const { state, attempt, options } = await scenario("forward");
    const response = await withTunnelRecovery(attempt, options());
    expect(response.status).toBe(200);
    expect(state.attempts).toBe(1);
    expect(state.reopens).toBe(0);
  });

  test("retries at most once: a second connection failure is thrown", async () => {
    const { state, attempt, options } = await scenario("die-on-request", 100);
    const dying = options();
    dying.reopen = async () => {
      state.reopens++;
      const next = await startProxy(state.gateway.port, "die-on-request", 100);
      state.endpoint = { host: "127.0.0.1", port: next.port, tls: false };
      state.tunnel = next.proc;
    };
    await expect(withTunnelRecovery(attempt, dying)).rejects.toBeInstanceOf(ConnectionError);
    expect(state.attempts).toBe(2);
    expect(state.reopens).toBe(1);
  });

  test("a connection failure while the tunnel stays up is thrown, not retried", async () => {
    // The process outlives the grace period: the gateway side really dropped the request.
    const { state, attempt, options } = await scenario("die-on-request", 60_000);
    await expect(withTunnelRecovery(attempt, options(200))).rejects.toBeInstanceOf(ConnectionError);
    expect(state.attempts).toBe(1);
    expect(state.reopens).toBe(0);
  });

  test("a connection failure without any tunnel is thrown, not retried", async () => {
    const { state, attempt, options } = await scenario("die-on-request", 0);
    state.tunnel = undefined;
    await expect(withTunnelRecovery(attempt, options())).rejects.toBeInstanceOf(ConnectionError);
    expect(state.attempts).toBe(1);
    expect(state.reopens).toBe(0);
  });

  test("a failing reopen is thrown", async () => {
    const { state, attempt, options } = await scenario("die-on-request", 0);
    const failing = options();
    failing.reopen = async () => {
      throw new Error("cannot open the tunnel");
    };
    await expect(withTunnelRecovery(attempt, failing)).rejects.toThrow("cannot open the tunnel");
    expect(state.attempts).toBe(1);
  });
});

describe("failures that are not connection failures are never retried", () => {
  test("a status the test does not expect is returned and fails the test's assertion", async () => {
    // The tunnel is dead (the worst case for the retry) and the gateway answers 503.
    const { state, attempt, options } = await scenario("die-on-request", 0, 503);
    state.tunnel?.kill();
    await state.tunnel?.exited;
    state.endpoint = { host: "127.0.0.1", port: state.gateway.port, tls: false };
    const response = await withTunnelRecovery(attempt, options());
    expect(response.status).toBe(503);
    expect(state.attempts).toBe(1);
    expect(state.reopens).toBe(0);
    // What the e2e test then does with the response: the mismatch fails the test.
    expect(() => expect(response.status).toBe(200)).toThrow();
    expect(state.gateway.requests()).toBe(1);
  });

  test.each([500, 502, 503, 504])("status %d is a result, not a reason to reopen", async (status) => {
    const { state, attempt, options } = await scenario("forward", undefined, status);
    state.tunnel?.kill();
    await state.tunnel?.exited;
    state.endpoint = { host: "127.0.0.1", port: state.gateway.port, tls: false };
    expect((await withTunnelRecovery(attempt, options())).status).toBe(status);
    expect(state.reopens).toBe(0);
    expect(state.gateway.requests()).toBe(1);
  });

  test("a request that was answered in part is not resent, even though the tunnel is dead", async () => {
    const { state, options } = await scenario("forward");
    state.tunnel?.kill();
    await state.tunnel?.exited;
    // The gateway sends the response head and then stalls: the timeout must not count as a lost
    // connection, because the request reached the gateway and was answered.
    let requests = 0;
    const stalled = Bun.listen({
      hostname: "127.0.0.1",
      port: 0,
      socket: {
        data(socket) {
          requests++;
          socket.write("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial");
          socket.flush();
        },
      },
    });
    try {
      let attempts = 0;
      const attempt = () => {
        attempts++;
        return sendRaw({ host: "127.0.0.1", port: stalled.port, tls: false }, REQUEST, HEAD_STALL_TIMEOUT_MS);
      };
      await expect(withTunnelRecovery(attempt, options())).rejects.toThrow("timeout");
      expect(attempts).toBe(1);
      expect(requests).toBe(1);
      expect(state.reopens).toBe(0);
    } finally {
      stalled.stop(true);
    }
  });

  test("an error that is not a ConnectionError is rethrown even though the tunnel is dead", async () => {
    const { state, options } = await scenario("forward");
    state.tunnel?.kill();
    await state.tunnel?.exited;
    let attempts = 0;
    const attempt = async () => {
      attempts++;
      throw new TypeError("unexpected");
    };
    await expect(withTunnelRecovery(attempt, options())).rejects.toThrow("unexpected");
    expect(attempts).toBe(1);
    expect(state.reopens).toBe(0);
  });
});
