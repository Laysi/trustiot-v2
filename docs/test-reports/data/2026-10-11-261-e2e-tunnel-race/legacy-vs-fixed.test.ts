// Scratch: runs the SAME fault injection against (a) raw()'s pre-fix recovery logic (the exitCode
// check of ingress.e2e.ts at 82e0480 in front of the same sendRaw) and (b) the fixed
// withTunnelRecovery.
import { afterEach, expect, test } from "bun:test";
import { sendRaw, type RawEndpoint } from "../../../../deploy/chart/tests/e2e/raw-client.ts";
import { withTunnelRecovery } from "../../../../deploy/chart/tests/e2e/tunnel-recovery.ts";
const FAKE = new URL("../../../../deploy/chart/tests/e2e/fake-port-forward.ts", import.meta.url).pathname;
const REQ = { target: "/api/whoami", hostHeader: "app.saas.trustiot.test" };
const procs: any[] = [];
const servers: any[] = [];
afterEach(() => {
  procs.splice(0).forEach((p) => p.kill());
  servers.splice(0).forEach((s) => s.stop(true));
});
async function freePort() {
  const s = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data() {} } });
  const p = s.port;
  s.stop(true);
  return p;
}
async function proxy(up: number, mode: string, delay: number) {
  const port = await freePort();
  const proc = Bun.spawn([process.execPath, FAKE, String(port), String(up), mode, String(delay)], { stdout: "ignore", stderr: "inherit" });
  procs.push(proc);
  for (let i = 0; i < 200; i++) {
    try {
      (await Bun.connect({ hostname: "127.0.0.1", port, socket: { data() {} } })).end();
      break;
    } catch {
      await Bun.sleep(20);
    }
  }
  return { port, proc };
}
async function setup() {
  const gw = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response("gateway") });
  servers.push(gw);
  const first = await proxy(gw.port as number, "die-on-request", 300);
  const st: any = { endpoint: { host: "127.0.0.1", port: first.port, tls: false } as RawEndpoint, tunnel: first.proc, reopens: 0 };
  st.reopen = async () => {
    st.reopens++;
    const n = await proxy(gw.port as number, "forward", 0);
    st.endpoint = { host: "127.0.0.1", port: n.port, tls: false };
    st.tunnel = n.proc;
  };
  return st;
}
test("pre-fix logic: fault injection makes the request fail", async () => {
  const st = await setup();
  const legacyRaw = async () => {
    try {
      return await sendRaw(st.endpoint, REQ, 5000);
    } catch (error) {
      if (st.tunnel === undefined || (st.tunnel.exitCode === null && st.tunnel.signalCode === null)) throw error;
      await st.reopen();
      return sendRaw(st.endpoint, REQ, 5000);
    }
  };
  const err: any = await legacyRaw().then(() => undefined, (e) => e);
  console.log("PRE-FIX RESULT: threw =", err?.message, "| reopens =", st.reopens);
  expect(err).toBeDefined();
  expect(st.reopens).toBe(0);
});
test("fixed logic: same fault injection is recovered", async () => {
  const st = await setup();
  const r = await withTunnelRecovery(() => sendRaw(st.endpoint, REQ, 5000), { tunnel: () => st.tunnel, reopen: st.reopen, log: (m) => console.log("LOG:", m) });
  console.log("FIXED RESULT: status =", r.status, "body =", r.body, "| reopens =", st.reopens);
  expect(r.status).toBe(200);
  expect(st.reopens).toBe(1);
});
