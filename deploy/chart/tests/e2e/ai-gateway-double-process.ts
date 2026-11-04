// Runs the AI gateway stand-in (ai-gateway-double/server.ts, Laysi/trustiot-v2#648) as a Bun child
// process on free loopback ports, for tests without a cluster or a container: its own test
// (ai-gateway-double.test.ts) and the nightly's (scripts/nightly/assistant-tools.test.ts), which
// sends it the directives the nightly writes. It keeps every line the stand-in prints, as
// `kubectl logs` would read them in the cluster.
import { join } from "node:path";
import type { Subprocess } from "bun";
import { freePort } from "./ports.ts";
import { type RawRequest, type RawResponse, sendRaw } from "./raw-client.ts";

export const AI_GATEWAY_DOUBLE_SERVER = join(import.meta.dir, "ai-gateway-double", "server.ts");

// How long the stand-in may take to listen, and to print a line after the request that causes
// it: a Bun start or a write on a loaded machine, far above an idle one.
const START_LIMIT_MS = 60_000;
const LOG_LIMIT_MS = 30_000;
const POLL_MS = 50;

export interface AiGatewayDouble {
  port: number;
  healthPort: number;
  adminPort: number;
  /** Sends one request to the API port, the health port or the admin port. */
  send(request: RawRequest, port?: number): Promise<RawResponse>;
  /** Every stdout line so far, parsed (a line that is not JSON is kept as its text). */
  lines(): unknown[];
  /** Waits until a line passes `match`; fails naming `what` and the lines it saw. */
  waitForLine(what: string, match: (line: unknown) => boolean): Promise<unknown>;
  stop(): void;
}

export async function startAiGatewayDouble(): Promise<AiGatewayDouble> {
  const port = await freePort();
  const healthPort = await freePort();
  const adminPort = await freePort();
  const child: Subprocess<"ignore", "pipe", "inherit"> = Bun.spawn([process.execPath, "run", AI_GATEWAY_DOUBLE_SERVER], {
    env: { ...process.env, PORT: String(port), HEALTH_PORT: String(healthPort), ADMIN_PORT: String(adminPort) },
    stdin: "ignore",
    stdout: "pipe",
    stderr: "inherit",
  });
  const printed: unknown[] = [];
  void (async () => {
    const decoder = new TextDecoder();
    let buffered = "";
    for await (const chunk of child.stdout) {
      buffered += decoder.decode(chunk, { stream: true });
      for (let end = buffered.indexOf("\n"); end >= 0; end = buffered.indexOf("\n")) {
        const line = buffered.slice(0, end);
        buffered = buffered.slice(end + 1);
        try {
          printed.push(JSON.parse(line));
        } catch {
          printed.push(line);
        }
      }
    }
  })();
  const send = (request: RawRequest, to = port) => sendRaw({ host: "127.0.0.1", port: to, tls: false }, request);

  const started = Date.now();
  for (;;) {
    if (child.exitCode !== null) throw new Error(`the AI gateway stand-in exited with ${child.exitCode} before it listened`);
    const ready = await send({ target: "/readyz", hostHeader: "localhost" }, healthPort).then(
      (response) => response.status === 200,
      () => false,
    );
    if (ready) break;
    if (Date.now() - started > START_LIMIT_MS) {
      child.kill();
      throw new Error(`timed out after ${START_LIMIT_MS / 1000} s waiting for the AI gateway stand-in to answer /readyz`);
    }
    await Bun.sleep(100);
  }

  return {
    port,
    healthPort,
    adminPort,
    send,
    lines: () => [...printed],
    async waitForLine(what, match) {
      const since = Date.now();
      for (;;) {
        const found = printed.find(match);
        if (found !== undefined) return found;
        if (Date.now() - since > LOG_LIMIT_MS) {
          throw new Error(`timed out after ${LOG_LIMIT_MS / 1000} s waiting for the stand-in to print ${what}; it printed ${JSON.stringify(printed).slice(0, 2000)}`);
        }
        await Bun.sleep(POLL_MS);
      }
    },
    stop: () => child.kill(),
  };
}
