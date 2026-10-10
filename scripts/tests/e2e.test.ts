// End-to-end test of the real environment with podman: up, status, seed, restart with new
// credentials, down, purge. It starts containers and pulls images on first use, so it only runs
// when TRUSTIOT_DEV_E2E=1:
//   TRUSTIOT_DEV_E2E=1 bun test scripts/tests/e2e.test.ts
// It uses its own instance, state directory and random ports, so it does not touch a
// developer's running environment.
import { describe, expect, test } from "bun:test";
import { createServer } from "node:net";
import { mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { SERVICES } from "../lib/services.ts";

const ENABLED = process.env.TRUSTIOT_DEV_E2E === "1";
const SCRIPT = join(import.meta.dir, "..", "dev.ts");
const SLOW_MS = 600_000;

function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen({ host: "127.0.0.1", port: 0 }, () => {
      const { port } = server.address() as { port: number };
      server.close(() => resolve(port));
    });
  });
}

async function dev(env: Record<string, string>, ...args: string[]) {
  const child = Bun.spawn([process.execPath, SCRIPT, ...args], {
    env: { ...process.env, ...env },
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, code] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  return { stdout, stderr, code };
}

async function podmanList(...args: string[]): Promise<string[]> {
  const child = Bun.spawn(["podman", ...args], { stdout: "pipe", stderr: "pipe" });
  const out = await new Response(child.stdout).text();
  await child.exited;
  return out.split("\n").filter(Boolean);
}

describe.skipIf(!ENABLED)("the dev environment with podman", () => {
  test(
    "starts, passes every check, seeds, rotates credentials, stops and purges without leftovers",
    async () => {
      const instance = `e2e-${Math.random().toString(36).slice(2, 8)}`;
      const state = mkdtempSync(join(tmpdir(), "trustiot-dev-e2e-"));
      const env = {
        TRUSTIOT_DEV_INSTANCE: instance,
        XDG_STATE_HOME: state,
        TRUSTIOT_DEV_POSTGRES_PORT: String(await freePort()),
        TRUSTIOT_DEV_CLICKHOUSE_PORT: String(await freePort()),
        TRUSTIOT_DEV_MQTT_PORT: String(await freePort()),
      };
      const label = `label=io.laysi.trustiot-dev.instance=${instance}`;
      const stateFile = join(state, "trustiot-v2", "dev", instance, "state.json");
      try {
        const first = await dev(env, "up");
        // cargo reports its progress on stderr; an error would also exit non-zero.
        expect(first.stderr).not.toMatch(/^error/m);
        expect(first.code).toBe(0);
        expect(first.stdout).toContain("every check passed");
        expect(first.stdout).not.toContain("FAIL");
        const firstState = JSON.parse(readFileSync(stateFile, "utf8"));
        expect(statSync(stateFile).mode & 0o777).toBe(0o600);

        const status = await dev(env, "status");
        expect(status.code).toBe(0);
        expect(status.stdout).toContain("all checks passed");

        for (let round = 0; round < 2; round++) {
          const seeded = await dev(env, "seed");
          expect(seeded.code).toBe(0);
          expect(seeded.stdout).toContain("seed verified: 2 machines (table and graph), 240 telemetry rows");
        }

        // No secret on a terminal during up, status or seed.
        const output = [first, status].map((run) => run.stdout + run.stderr).join("\n");
        for (const secret of [
          firstState.credentials.postgres.password,
          firstState.credentials.clickhouse.password,
          firstState.credentials.mqtt.password,
        ]) {
          expect(output).not.toContain(secret);
        }

        expect((await podmanList("ps", "--filter", label, "--format", "{{.Names}}")).length).toBe(3 + SERVICES.length);
        expect((await dev(env, "down")).code).toBe(0);
        expect(await podmanList("ps", "-a", "--filter", label, "--format", "{{.Names}}")).toEqual([]);
        expect(await podmanList("network", "ls", "--filter", label, "--format", "{{.Name}}")).toEqual([]);
        // The data survives down ...
        expect((await podmanList("volume", "ls", "--filter", label, "--format", "{{.Name}}")).length).toBeGreaterThan(0);

        // ... and the next up uses new credentials against the old data.
        const second = await dev(env, "up");
        expect(second.code).toBe(0);
        const secondState = JSON.parse(readFileSync(stateFile, "utf8"));
        expect(secondState.credentials.postgres.password).not.toBe(firstState.credentials.postgres.password);
        expect(secondState.credentials.clickhouse.password).not.toBe(firstState.credentials.clickhouse.password);
        expect(secondState.credentials.mqtt.password).not.toBe(firstState.credentials.mqtt.password);
        const reseeded = await dev(env, "seed");
        expect(reseeded.stdout).toContain("seed verified: 2 machines (table and graph), 240 telemetry rows");
      } finally {
        const purged = await dev(env, "purge");
        expect(purged.code).toBe(0);
        rmSync(state, { recursive: true, force: true });
      }
      expect(await podmanList("ps", "-a", "--filter", label, "--format", "{{.Names}}")).toEqual([]);
      expect(await podmanList("volume", "ls", "--filter", label, "--format", "{{.Name}}")).toEqual([]);
      expect(await podmanList("network", "ls", "--filter", label, "--format", "{{.Name}}")).toEqual([]);
    },
    SLOW_MS,
  );
});
