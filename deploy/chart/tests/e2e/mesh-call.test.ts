// Cluster-free tests of mesh-call.ts (Laysi/trustiot-v2#106): runs the script mesh.e2e.ts sends
// into a pod with this Bun, against a local stand-in of the identity double's echo. Runs with
// `bun test deploy/chart`.
import { afterAll, describe, expect, test } from "bun:test";

import { COMMAND_TIMEOUT_MS, useProcessTestTimeouts } from "../timing.ts";
import { callScript, parseCallOutput } from "./mesh-call.ts";

// Every test starts a Bun process: see ../timing.ts.
useProcessTestTimeouts();

type Seen = { method: string; path: string; headers: Record<string, string>; body: string };
const seen: Seen[] = [];
// Like identity-double/server.ts: every request answered with its headers; /plain answers text,
// /hang never answers.
const echo = Bun.serve({
  port: 0,
  hostname: "127.0.0.1",
  async fetch(req) {
    const url = new URL(req.url);
    const headers = Object.fromEntries(req.headers.entries());
    seen.push({ method: req.method, path: url.pathname, headers, body: await req.text() });
    if (url.pathname === "/plain") return new Response("ok");
    if (url.pathname === "/hang") return new Promise<Response>(() => {});
    return Response.json({ service: "identity-double", method: req.method, path: url.pathname, headers });
  },
});
afterAll(() => echo.stop(true));
const at = (path: string) => `http://127.0.0.1:${echo.port}${path}`;

/** A token-shaped value made per run. */
const fakeToken = () => `${crypto.randomUUID()}.${crypto.randomUUID()}.${crypto.randomUUID()}`.replaceAll("-", "");

async function runScript(script: string, stdin?: string) {
  const proc = Bun.spawn([process.execPath, "-e", script], {
    stdin: stdin === undefined ? "ignore" : new TextEncoder().encode(stdin),
    stdout: "pipe",
    stderr: "pipe",
    timeout: COMMAND_TIMEOUT_MS,
  });
  const [stdout, stderr] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text()]);
  await proc.exited;
  return { stdout, stderr, result: parseCallOutput(stdout) };
}

describe("the call a pod makes in the mesh e2e test", () => {
  test("a token from stdin goes as x-internal-identity, and only whether it arrived is printed", async () => {
    const token = fakeToken();
    const { stdout, stderr, result } = await runScript(callScript(at("/v1/mesh-e2e"), { withToken: true, timeoutMs: COMMAND_TIMEOUT_MS }), `${token}\n`);
    expect(result).toMatchObject({ outcome: "response", status: 200, tokenEchoed: true });
    expect(seen.at(-1)).toMatchObject({ method: "GET", path: "/v1/mesh-e2e" });
    expect(seen.at(-1)?.headers["x-internal-identity"]).toBe(token);
    expect(stdout + stderr).not.toContain(token);
  });

  test("a body makes it a JSON POST, with the Host given", async () => {
    const token = fakeToken();
    const script = callScript(at("/internal/token-exchange"), { withToken: true, body: { audience: "certs" }, host: "identity.example.test", timeoutMs: COMMAND_TIMEOUT_MS });
    const { result } = await runScript(script, token);
    expect(result).toMatchObject({ outcome: "response", status: 200, tokenEchoed: true });
    const request = seen.at(-1);
    expect(request?.method).toBe("POST");
    expect(request?.path).toBe("/internal/token-exchange");
    expect(JSON.parse(request?.body ?? "null")).toEqual({ audience: "certs" });
    expect(request?.headers["content-type"]).toBe("application/json");
    expect(request?.headers.host).toBe("identity.example.test");
  });

  test("an answer that does not echo the token says so", async () => {
    const { result } = await runScript(callScript(at("/plain"), { withToken: true, timeoutMs: COMMAND_TIMEOUT_MS }), fakeToken());
    expect(result).toMatchObject({ outcome: "response", status: 200, tokenEchoed: false });
  });

  test("without a token nothing is read or sent", async () => {
    const { result } = await runScript(callScript(at("/v1/mesh-e2e"), { withToken: false, timeoutMs: COMMAND_TIMEOUT_MS }));
    expect(result).toMatchObject({ outcome: "response", status: 200 });
    expect(result?.tokenEchoed).toBeUndefined();
    expect(seen.at(-1)?.headers["x-internal-identity"]).toBeUndefined();
  });

  test("a refused connection and a call that never answers count as refused", async () => {
    const closed = Bun.serve({ port: 0, hostname: "127.0.0.1", fetch: () => new Response("") });
    const port = closed.port;
    closed.stop(true);
    const refused = await runScript(callScript(`http://127.0.0.1:${port}/v1/mesh-e2e`, { withToken: true, timeoutMs: COMMAND_TIMEOUT_MS }), fakeToken());
    expect(refused.result?.outcome).toBe("error");
    // The script's own limit is what is under test, so it is short; it waits all of it.
    const hung = await runScript(callScript(at("/hang"), { withToken: true, timeoutMs: 500 }), fakeToken());
    expect(hung.result?.outcome).toBe("timeout");
  });

  test("the result is the last JSON line with an outcome", () => {
    expect(parseCallOutput('noise\n{"outcome":"error","ms":1}\n')).toEqual({ outcome: "error", ms: 1 });
    expect(parseCallOutput("no result\n")).toBeUndefined();
  });
});
