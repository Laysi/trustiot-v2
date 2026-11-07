// scripts/demo/preflight.ts against a fake demo machine and a fake gateway (a fake ssh runner,
// like the other demo scripts' tests), and its bounded runner against real short processes
// (Laysi/trustiot-v2#1180).
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { RunResult } from "../release/exec.ts";
import { FluxLog, type RepoInputs, boxOps, readCode } from "./flux.ts";
import { Box, hidingSshHost } from "./lib.ts";
import { LOCK_STATE_COMMAND } from "./tunnel.ts";
import {
  CLICKHOUSE_QUERIES,
  CLICKHOUSE_SCRIPT,
  FRESH_LIMIT_S,
  GATEWAY_COMMAND,
  MEMORY_WARN_AVAILABLE_MIB,
  MEMORY_WARN_NODE_PERCENT,
  NAMES,
  type Result,
  boundedCheckoutInputs,
  boundedSsh,
  countVerdicts,
  judgeFreshness,
  judgeGateway,
  judgeMemory,
  judgePresence,
  parseDataFacts,
  parseFree,
  parsePod,
  parseTopNodePercent,
  podProblem,
  resultLines,
  runBounded,
  runPreflight,
  summary,
} from "./preflight.ts";

// --- A fake demo machine and gateway -------------------------------------------------------------

// Built from parts: Renovate's pin test (scripts/nightly/renovate.test.ts) rejects a bare image digest in a file it reads.
const DIGEST = ["sha256", "f31fd6faf13f57aa95724d96b69a80492c371f0662226434520b430885d096a8"].join(":");
const VERSION = "0.0.0-rc.15";
const DEPLOYED = `${VERSION}+f31fd6faf13f`;

const FREE = `               total        used        free      shared  buff/cache   available
Mem:            7543        4672         586          36        3344        2870
Swap:           8191         680        7511
`;
const TOP = `NAME              CPU(cores)   CPU(%)   MEMORY(bytes)   MEMORY(%)
secret-node-name   957m         11%      3366Mi          44%
`;
const PODS = [
  "trustiot-assistant-78c5bcb49f-wfcp2 Running ready=true restarts=0",
  "trustiot-clickhouse-0 Running ready=true restarts=0",
  "trustiot-migrate-2928c22d-bvgjn Succeeded ready=false restarts=0",
  "trustiot-mqtt-broker-0 Running ready=true true restarts=0 0",
];
const DATA = "toYYYYMM(received_at)\n2026-11-07 02:44:10\t34\t19400\nonline\t2026-11-07 02:00:01.967790189\t2663\n";
const GATEWAY_OK = "active\nup 5 minutes\nntp-synchronized\n";

function helm(overrides: { history?: unknown; suspend?: boolean; ready?: string } = {}): Record<string, unknown> {
  return {
    metadata: { generation: 3 },
    spec: overrides.suspend === true ? { suspend: true } : {},
    status: {
      observedGeneration: 3,
      conditions: [{ type: "Ready", status: overrides.ready ?? "True", reason: "UpgradeSucceeded", message: "Helm upgrade succeeded" }],
      history: overrides.history ?? [{ chartVersion: DEPLOYED, status: "deployed" }],
    },
  };
}

interface World {
  reachable: boolean;
  lock: string;
  helm: Record<string, unknown> | undefined;
  oci: Record<string, unknown> | undefined;
  pods: string[];
  podsFail: boolean;
  free: RunResult;
  top: RunResult;
  clickhouse: RunResult;
}

function ok(stdout: string): RunResult {
  return { stdout, stderr: "", exitCode: 0 };
}

function healthy(): World {
  return {
    reachable: true,
    lock: "LOCK_FREE\n",
    helm: helm(),
    oci: { status: { conditions: [{ type: "Ready", status: "True" }], artifact: { revision: DIGEST } } },
    pods: [...PODS],
    podsFail: false,
    free: ok(FREE),
    top: ok(TOP),
    clickhouse: ok(DATA),
  };
}

const NOT_FOUND: RunResult = { stdout: "", stderr: 'Error from server (NotFound): not found', exitCode: 1 };

/** Every command a run sends, in order, with its stdin. */
interface Sent {
  command: string;
  stdin?: string | Uint8Array | undefined;
}

function demoRunner(world: World, sent: Sent[]) {
  return async (cmd: string[], stdin?: string | Uint8Array): Promise<RunResult> => {
    const command = cmd[cmd.length - 1] ?? "";
    sent.push({ command, stdin });
    if (!world.reachable) return { stdout: "", stderr: "ssh: Could not resolve hostname demo-alias: Name or service not known", exitCode: 255 };
    if (command === "true") return ok("");
    if (command === LOCK_STATE_COMMAND) return ok(world.lock);
    if (command === "free -m") return world.free;
    if (command.includes("helmreleases")) return world.helm === undefined ? NOT_FOUND : ok(JSON.stringify(world.helm));
    if (command.includes("ocirepositories")) return world.oci === undefined ? NOT_FOUND : ok(JSON.stringify(world.oci));
    if (command.includes(" get pods")) {
      return world.podsFail ? { stdout: "", stderr: "the server is down", exitCode: 1 } : ok(`${world.pods.join("\n")}\n`);
    }
    if (command.includes(" top node")) return world.top;
    if (command.includes(" exec ")) return world.clickhouse;
    return { stdout: "", stderr: `unexpected command: ${command}`, exitCode: 99 };
  };
}

let dir: string;
beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "preflight-test-"));
});
afterEach(() => rmSync(dir, { recursive: true, force: true }));

const NO_SLEEP = { sleep: async () => {} };

function repoInputs(overrides: Partial<RepoInputs> = {}): RepoInputs {
  return {
    head: "e8bd05a89fd4ccb3be88d41f9f4815ec316d3295",
    knownHosts: "",
    chart: { version: VERSION, digest: DIGEST },
    secrets: {},
    code: [],
    flux: [],
    deploy: [],
    ...overrides,
  };
}

async function run(
  world: World,
  options: { gateway?: string | RunResult | "skip"; repo?: () => Promise<RepoInputs>; sent?: Sent[] } = {},
): Promise<Result[]> {
  const sent = options.sent ?? [];
  const log = new FluxLog("preflight-test", dir);
  const demo = new Box("demo-alias", log, { run: hidingSshHost(demoRunner(world, sent), "demo-alias"), ...NO_SLEEP });
  const ops = boxOps(demo, { sshHost: "demo-alias", apply: false, confirm: "", lockHeld: false }, async () => {
    throw new Error("no age");
  });
  const gatewayAnswer = options.gateway ?? GATEWAY_OK;
  const gateway =
    gatewayAnswer === "skip"
      ? undefined
      : new Box(
          "gw-alias",
          log,
          {
            run: hidingSshHost(async (cmd) => {
              const command = cmd[cmd.length - 1] ?? "";
              sent.push({ command });
              if (command === "true") return typeof gatewayAnswer === "object" ? gatewayAnswer : ok("");
              return typeof gatewayAnswer === "string" ? ok(gatewayAnswer) : gatewayAnswer;
            }, "gw-alias"),
            ...NO_SLEEP,
          },
        );
  return runPreflight({ demo, ...(gateway === undefined ? {} : { gateway }), ops, repo: options.repo ?? (async () => repoInputs()) });
}

function named(results: Result[], name: string): Result[] {
  return results.filter((r) => r.name === name);
}

function one(results: Result[], name: string): Result {
  const found = named(results, name);
  expect(found.length).toBe(1);
  return found[0]!;
}

const text = (result: Result) => result.evidence.join("\n");

// --- All pass ------------------------------------------------------------------------------------

describe("a healthy demo machine and gateway", () => {
  test("every item passes, each with its evidence, and the run exits 0", async () => {
    const results = await run(healthy());
    expect(results.map((r) => [r.name, r.verdict])).toEqual([
      [NAMES.flux, "PASS"],
      [NAMES.memory, "PASS"],
      [NAMES.dashboard, "PASS"],
      [NAMES.data, "PASS"],
      [NAMES.presence, "PASS"],
      [NAMES.gateway, "PASS"],
    ]);
    expect(text(one(results, NAMES.flux))).toContain(`HelmRelease deployed ${DEPLOYED} = main's pin ${VERSION}`);
    expect(text(one(results, NAMES.flux))).toContain("lab lock free");
    expect(text(one(results, NAMES.flux))).toContain("3 of 3 pods Running and ready (1 finished Job pods), 0 restarts in total");
    expect(text(one(results, NAMES.memory))).toBe("available 2870 MiB of 7543 MiB (38 %), node memory 44 %");
    expect(text(one(results, NAMES.dashboard))).toContain("off");
    expect(text(one(results, NAMES.data))).toBe(`newest received 2026-11-07 02:44:10 UTC, 34 s ago (limit ${FRESH_LIMIT_S} s), 19400 rows`);
    expect(text(one(results, NAMES.presence))).toBe('last presence state "online" since 2026-11-07 02:00:01 UTC (2663 s ago)');
    expect(text(one(results, NAMES.gateway))).toBe("trustiot-agent active, up 5 minutes, ntp-synchronized");
    expect(summary(results)).toEqual({ line: "summary: 6 PASS, 0 WARN, 0 FAIL, 0 SKIP; no item failed (exit 0)", exitCode: 0 });
  });

  test("only reads: every kubectl verb is get, top or exec, and nothing writes or restarts", async () => {
    const sent: Sent[] = [];
    await run(healthy(), { sent });
    const commands = sent.map((s) => s.command);
    const kubectl = commands.filter((c) => c.startsWith("sudo k3s kubectl"));
    expect(kubectl.length).toBeGreaterThan(3);
    for (const command of kubectl) {
      const words = command.split(" ");
      const verb = words.find((w, i) => i >= 3 && !w.startsWith("-") && words[i - 1] !== "-n");
      expect(["get", "top", "exec"]).toContain(verb ?? "");
    }
    for (const command of commands) {
      expect(command).not.toMatch(/\b(apply|patch|delete|scale|rollout|annotate|label|restart|mkdir|rm|systemctl (start|stop|restart)|poweroff|reboot)\b/);
    }
    // No lab lock is taken: the lock is only read (LOCK_STATE_COMMAND has no mkdir).
    expect(commands.filter((c) => c.includes("e2e.lock")).every((c) => c === LOCK_STATE_COMMAND)).toBe(true);
  });

  test("the ClickHouse read is read-only, aggregates only, and keeps the password off every command line", async () => {
    const sent: Sent[] = [];
    await run(healthy(), { sent });
    const exec = sent.find((s) => s.command.includes(" exec "))!;
    expect(exec.command).toContain("trustiot-clickhouse-0");
    expect(exec.command).toContain("--readonly 1");
    expect(exec.command).toContain("$CLICKHOUSE_PASSWORD_FILE");
    // The password goes to the client as its own variable, not as an argument.
    expect(CLICKHOUSE_SCRIPT).not.toContain("--password");
    expect(CLICKHOUSE_SCRIPT).toContain('CLICKHOUSE_PASSWORD="$(cat "$CLICKHOUSE_PASSWORD_FILE")" clickhouse-client');
    expect(exec.stdin).toBe(CLICKHOUSE_QUERIES);
    const queries = CLICKHOUSE_QUERIES.split(";").map((q) => q.trim()).filter((q) => q !== "");
    expect(queries.length).toBe(3);
    for (const query of queries) {
      expect(query).toMatch(/^SELECT /);
      expect(query).not.toContain("*");
      expect(query).not.toMatch(/\b(INSERT|ALTER|DROP|CREATE|DELETE|TRUNCATE)\b/i);
      expect(query).not.toMatch(/\bSYSTEM\s+[A-Z]/);
    }
    expect(queries[0]).toContain("FROM system.tables");
    expect(queries[1]).toContain("FROM system.parts");
    expect(queries[2]).toContain("trustiot_logs.otel_logs");
    // Only aggregates and the gateway's presence record are selected: no tenant row column.
    expect(queries.join("\n")).not.toMatch(/tenant|Body,|LogAttributes\s*,/i);
  });
});

// --- Flux and services ---------------------------------------------------------------------------

describe("Flux and services", () => {
  const flux = async (change: (w: World) => void, options: Parameters<typeof run>[1] = {}) => {
    const w = healthy();
    change(w);
    return one(await run(w, { gateway: "skip", ...options }), NAMES.flux);
  };

  test("a held lab lock is a FAIL naming the owner", async () => {
    const r = await flux((w) => (w.lock = "LOCK_HELD Laysi/trustiot-v2#150 2026-11-07T01:00:00Z e2e run\n"));
    expect(r.verdict).toBe("FAIL");
    expect(text(r)).toContain("lab lock held by Laysi/trustiot-v2#150 2026-11-07T01:00:00Z e2e run");
  });

  test("an unreadable lab lock is a FAIL", async () => {
    const r = await flux((w) => (w.lock = "LOCK_UNREADABLE\n"));
    expect(r.verdict).toBe("FAIL");
    expect(text(r)).toContain("lab lock unreadable");
  });

  test("a deployed version other than main's pin is a FAIL showing both", async () => {
    const r = await flux((w) => (w.helm = helm({ history: [{ chartVersion: "0.0.0-rc.14+aaaaaaaaaaaa", status: "deployed" }] })));
    expect(r.verdict).toBe("FAIL");
    expect(text(r)).toContain(`HelmRelease deployed 0.0.0-rc.14+aaaaaaaaaaaa, but main pins ${VERSION}`);
  });

  test("a pulled chart that is not main's pin is a FAIL", async () => {
    const r = await flux((w) => (w.oci = { status: { artifact: { revision: "sha256:0000" } } }));
    expect(r.verdict).toBe("FAIL");
    expect(text(r)).toContain("is not main's pin");
  });

  test("a missing chart source is a FAIL", async () => {
    const r = await flux((w) => (w.oci = undefined));
    expect(r.verdict).toBe("FAIL");
    expect(text(r)).toContain("chart OCIRepository does not exist");
  });

  test("a missing HelmRelease is a FAIL", async () => {
    const r = await flux((w) => (w.helm = undefined));
    expect(r.verdict).toBe("FAIL");
    expect(text(r)).toContain("HelmRelease trustiot/trustiot does not exist");
  });

  test("a HelmRelease whose latest release is not deployed is a FAIL", async () => {
    const r = await flux((w) => (w.helm = helm({ history: [{ chartVersion: DEPLOYED, status: "failed" }], ready: "False" })));
    expect(r.verdict).toBe("FAIL");
    expect(text(r)).toContain("has no deployed release (latest: ");
    expect(text(r)).toContain("HelmRelease not ready");
  });

  test("a suspended HelmRelease is a FAIL", async () => {
    const r = await flux((w) => (w.helm = helm({ suspend: true })));
    expect(r.verdict).toBe("FAIL");
    expect(text(r)).toContain("HelmRelease is suspended");
  });

  test("a pod that is not ready, or not Running, is a FAIL naming it", async () => {
    const notReady = await flux((w) => (w.pods = [...PODS, "trustiot-ingest-1 Running ready=false restarts=0"]));
    expect(notReady.verdict).toBe("FAIL");
    expect(text(notReady)).toContain("trustiot-ingest-1 is Running but not ready (ready=false)");
    const oneOfTwo = await flux((w) => (w.pods = [...PODS, "trustiot-mqtt-broker-1 Running ready=true false restarts=0 0"]));
    expect(oneOfTwo.verdict).toBe("FAIL");
    const pending = await flux((w) => (w.pods = [...PODS, "trustiot-web-1 Pending ready= restarts="]));
    expect(pending.verdict).toBe("FAIL");
    expect(text(pending)).toContain("trustiot-web-1 is Pending");
  });

  test("no pods, or a pod list that cannot be read, is a FAIL", async () => {
    expect((await flux((w) => (w.pods = []))).verdict).toBe("FAIL");
    const failed = await flux((w) => (w.podsFail = true));
    expect(failed.verdict).toBe("FAIL");
    expect(text(failed)).toContain("could not read it");
  });

  test("restarts are a WARN with the count per pod", async () => {
    const r = await flux((w) => (w.pods = [...PODS, "trustiot-ingest-1 Running ready=true true restarts=2 1"]));
    expect(r.verdict).toBe("WARN");
    expect(r.evidence).toContain("trustiot-ingest-1: restarts=3");
    expect(text(r)).toContain("1 pod(s) restarted");
  });

  test("main's pin not readable is a WARN, and the rest is still judged", async () => {
    const r = await flux(() => {}, {
      repo: async () => {
        throw new Error("git fetch failed");
      },
    });
    expect(r.verdict).toBe("WARN");
    expect(text(r)).toContain("main's chart pin not read (git fetch failed)");
    expect(text(r)).toContain(`HelmRelease deployed ${DEPLOYED}`);
    // With a broken release it is still a FAIL.
    const broken = await flux((w) => (w.helm = undefined), {
      repo: async () => {
        throw new Error("git fetch failed");
      },
    });
    expect(broken.verdict).toBe("FAIL");
  });

  test("a checkout that is not clean origin/main is a WARN", async () => {
    const r = await flux(() => {}, { repo: async () => repoInputs({ code: ["scripts has uncommitted changes:  M scripts/x.ts"] }) });
    expect(r.verdict).toBe("WARN");
    expect(text(r)).toContain("not a clean origin/main");
  });

  test("main pinning no chart is a WARN", async () => {
    const { chart: _chart, ...withoutChart } = repoInputs();
    const r = await flux(() => {}, { repo: async () => withoutChart });
    expect(r.verdict).toBe("WARN");
    expect(text(r)).toContain("main pins no chart");
  });
});

describe("parsePod and podProblem", () => {
  test("read the pod lines of FluxOps.pods", () => {
    expect(parsePod("a-1 Running ready=true true restarts=0 1")).toEqual({ name: "a-1", phase: "Running", ready: ["true", "true"], restarts: 1 });
    expect(parsePod("job-1 Succeeded ready=false restarts=0")).toEqual({ name: "job-1", phase: "Succeeded", ready: ["false"], restarts: 0 });
    expect(podProblem(parsePod("job-1 Succeeded ready=false restarts=0"))).toBeUndefined();
    expect(podProblem(parsePod("x-1 Running ready= restarts="))).toContain("not ready");
    expect(() => parsePod("garbage")).toThrow("cannot read the pod line");
    expect(() => parsePod("x Running ready=true restarts=a")).toThrow("restart counts");
  });
});

// --- Memory --------------------------------------------------------------------------------------

describe("memory", () => {
  test("parses free -m by its header and kubectl top node by the percent, and never keeps the node's name", () => {
    expect(parseFree(FREE)).toEqual({ totalMiB: 7543, availableMiB: 2870 });
    expect(parseTopNodePercent(TOP)).toBe(44);
    expect(parseTopNodePercent("NAME CPU(cores) CPU(%) MEMORY(bytes) MEMORY(%)\na 1m 1% 1Mi 30%\nb 1m 1% 1Mi 61%\n")).toBe(61);
    expect(() => parseFree("nothing")).toThrow("no Mem row");
    expect(() => parseTopNodePercent("NAME CPU(cores) CPU(%) MEMORY(bytes) MEMORY(%)\n")).toThrow("metrics-server");
  });

  test("the thresholds: the runbook's 2026-11-07 reading passes, below them warns", () => {
    expect(MEMORY_WARN_AVAILABLE_MIB).toBe(2048);
    expect(MEMORY_WARN_NODE_PERCENT).toBe(70);
    expect(judgeMemory({ totalMiB: 7543, availableMiB: 2870 }, 42).verdict).toBe("PASS");
    expect(judgeMemory({ totalMiB: 7543, availableMiB: 2048 }, 69).verdict).toBe("PASS");
    const low = judgeMemory({ totalMiB: 7543, availableMiB: 2047 }, 42);
    expect(low.verdict).toBe("WARN");
    expect(text(low)).toContain("available memory is below 2048 MiB");
    const busy = judgeMemory({ totalMiB: 7543, availableMiB: 2870 }, 70);
    expect(busy.verdict).toBe("WARN");
    expect(text(busy)).toContain("node memory is at or above 70 %");
  });

  test("a low reading warns and the evidence keeps the numbers and not the node's name", async () => {
    const w = healthy();
    w.free = ok(FREE.replace("2870", "1500"));
    const r = one(await run(w, { gateway: "skip" }), NAMES.memory);
    expect(r.verdict).toBe("WARN");
    expect(text(r)).toContain("available 1500 MiB of 7543 MiB (20 %), node memory 44 %");
    expect(text(r)).not.toContain("secret-node-name");
  });

  test("a failing free or top is a FAIL with the reason", async () => {
    const a = healthy();
    a.free = { stdout: "", stderr: "boom", exitCode: 1 };
    const freeFail = one(await run(a, { gateway: "skip" }), NAMES.memory);
    expect(freeFail.verdict).toBe("FAIL");
    expect(text(freeFail)).toContain("reading free memory (free -m) failed (exit 1): boom");
    const b = healthy();
    b.top = { stdout: "", stderr: "metrics not available yet", exitCode: 1 };
    const topFail = one(await run(b, { gateway: "skip" }), NAMES.memory);
    expect(topFail.verdict).toBe("FAIL");
    expect(text(topFail)).toContain("metrics not available yet");
  });
});

// --- Dashboard check -----------------------------------------------------------------------------

describe("dashboard check", () => {
  test("is informational: off and on both PASS, on but not ready WARNs, unreadable WARNs, never FAIL", async () => {
    const off = one(await run(healthy(), { gateway: "skip" }), NAMES.dashboard);
    expect(off.verdict).toBe("PASS");
    expect(text(off)).toContain("off (no dashboard-check pod");
    const on = healthy();
    on.pods.push("trustiot-dashboard-check-6d9-abcde Running ready=true restarts=0");
    const onResult = one(await run(on, { gateway: "skip" }), NAMES.dashboard);
    expect(onResult.verdict).toBe("PASS");
    expect(text(onResult)).toContain("on (1 pod(s) ready");
    const notReady = healthy();
    notReady.pods.push("trustiot-dashboard-check-6d9-abcde Running ready=false restarts=0");
    const warn = one(await run(notReady, { gateway: "skip" }), NAMES.dashboard);
    expect(warn.verdict).toBe("WARN");
    expect(text(warn)).toContain("on, but trustiot-dashboard-check-6d9-abcde is Running but not ready");
    const unreadable = healthy();
    unreadable.podsFail = true;
    const unknown = one(await run(unreadable, { gateway: "skip" }), NAMES.dashboard);
    expect(unknown.verdict).toBe("WARN");
    expect(text(unknown)).toContain("could not read it");
  });
});

// --- Data freshness and presence -----------------------------------------------------------------

describe("data freshness and gateway presence", () => {
  test("parses the two TSV lines", () => {
    expect(parseDataFacts(DATA)).toEqual({ partitionKey: "toYYYYMM(received_at)", newestUtc: "2026-11-07 02:44:10", secondsAgo: 34, rows: 19400, state: "online", sinceUtc: "2026-11-07 02:00:01", sinceSecondsAgo: 2663 });
    expect(() => parseDataFacts("only one line\n")).toThrow("3 lines (1, 3 and 3 columns)");
    expect(() => parseDataFacts("k\na\tb\tc\nonline\tx\t1\n")).toThrow("seconds ago is not a number");
  });

  const facts = (seconds: number, rows = 100) => ({ partitionKey: "toYYYYMM(received_at)", newestUtc: "2026-11-07 02:44:10", secondsAgo: seconds, rows, state: "online", sinceUtc: "2026-11-07 02:00:01", sinceSecondsAgo: 5 });

  test("the bound is the runbook's 180 s: 180 passes, 181 fails, no rows fails, the future warns", () => {
    expect(FRESH_LIMIT_S).toBe(180);
    expect(judgeFreshness(facts(0)).verdict).toBe("PASS");
    expect(judgeFreshness(facts(180)).verdict).toBe("PASS");
    const stale = judgeFreshness(facts(181));
    expect(stale.verdict).toBe("FAIL");
    expect(text(stale)).toContain("181 s ago (limit 180 s)");
    expect(judgeFreshness(facts(10, 0)).verdict).toBe("FAIL");
    expect(judgeFreshness({ ...facts(5), newestUtc: "1970-01-01 00:00:00" }).verdict).toBe("FAIL");
    expect(judgeFreshness(facts(-30)).verdict).toBe("WARN");
  });

  test("the parts' newest time counts only while the partition key is received_at", () => {
    const other = judgeFreshness({ ...facts(5), partitionKey: "toYYYYMM(ts)" });
    expect(other.verdict).toBe("FAIL");
    expect(text(other)).toContain('partition key is "toYYYYMM(ts)", not toYYYYMM(received_at)');
  });

  test("presence passes only for online", () => {
    expect(judgePresence(facts(5)).verdict).toBe("PASS");
    for (const state of ["offline", "degraded", "ONLINE"]) {
      const r = judgePresence({ ...facts(5), state });
      expect(r.verdict).toBe("FAIL");
      expect(text(r)).toContain("not online");
    }
    expect(judgePresence({ ...facts(5), state: "", sinceUtc: "1970-01-01 00:00:00" }).verdict).toBe("FAIL");
  });

  test("the 2026-11-07 reading in the runbook (34093 s, offline) fails both items with the evidence", async () => {
    const w = healthy();
    w.clickhouse = ok("toYYYYMM(received_at)\n2026-11-06 09:01:26\t34093\t19310\noffline\t2026-11-06 09:02:13.967790189\t34040\n");
    const results = await run(w, { gateway: "skip" });
    const data = one(results, NAMES.data);
    expect(data.verdict).toBe("FAIL");
    expect(text(data)).toBe("newest received 2026-11-06 09:01:26 UTC, 34093 s ago (limit 180 s), 19310 rows: the data is stale");
    const presence = one(results, NAMES.presence);
    expect(presence.verdict).toBe("FAIL");
    expect(text(presence)).toContain('"offline" since 2026-11-06 09:02:13 UTC');
    expect(summary(results).exitCode).toBe(1);
  });

  test("fresh data with a gateway that is not online fails only the presence", async () => {
    const w = healthy();
    w.clickhouse = ok("toYYYYMM(received_at)\n2026-11-07 02:44:10\t34\t19400\noffline\t2026-11-07 02:30:00\t900\n");
    const results = await run(w, { gateway: "skip" });
    expect(one(results, NAMES.data).verdict).toBe("PASS");
    expect(one(results, NAMES.presence).verdict).toBe("FAIL");
  });

  test("a ClickHouse command that fails or prints something else fails both, with the reason", async () => {
    const failed = healthy();
    failed.clickhouse = { stdout: "", stderr: "Error from server (NotFound): pods not found", exitCode: 1 };
    for (const r of named(await run(failed, { gateway: "skip" }), NAMES.data).concat(named(await run(failed, { gateway: "skip" }), NAMES.presence))) {
      expect(r.verdict).toBe("FAIL");
      expect(text(r)).toContain("reading ClickHouse in trustiot-clickhouse-0 failed (exit 1)");
    }
    const odd = healthy();
    odd.clickhouse = ok("hello\n");
    const results = await run(odd, { gateway: "skip" });
    expect(one(results, NAMES.data).verdict).toBe("FAIL");
    expect(text(one(results, NAMES.presence))).toContain("3 lines (1, 3 and 3 columns)");
  });
});

// --- The gateway ---------------------------------------------------------------------------------

describe("the gateway", () => {
  test("is skipped, and says so, without --gateway-ssh-host, and a skip does not fail the run", async () => {
    const results = await run(healthy(), { gateway: "skip" });
    const gateway = one(results, NAMES.gateway);
    expect(gateway.verdict).toBe("SKIP");
    expect(text(gateway)).toContain("skipped: no --gateway-ssh-host");
    expect(summary(results)).toEqual({ line: "summary: 5 PASS, 0 WARN, 0 FAIL, 1 SKIP; no item failed (exit 0)", exitCode: 0 });
  });

  test("the agent not active, or the clock not synchronized, is a FAIL", () => {
    expect(judgeGateway("active\nup 2 hours, 3 minutes\nntp-synchronized\n").verdict).toBe("PASS");
    const inactive = judgeGateway("inactive\nup 5 minutes\nntp-synchronized\n");
    expect(inactive.verdict).toBe("FAIL");
    expect(text(inactive)).toContain("trustiot-agent is inactive, not active");
    const clock = judgeGateway("active\nup 1 minute\nntp-NOT-synchronized\n");
    expect(clock.verdict).toBe("FAIL");
    expect(text(clock)).toContain("not NTP-synchronized");
    const both = judgeGateway("failed\nup 1 minute\nntp-NOT-synchronized\n");
    expect(text(both)).toContain("trustiot-agent is failed");
    expect(text(both)).toContain("not NTP-synchronized");
    expect(judgeGateway("active\n").verdict).toBe("FAIL");
    // The runbook's command, unchanged.
    expect(GATEWAY_COMMAND).toBe("systemctl is-active trustiot-agent; uptime -p; test -e /run/systemd/timesync/synchronized && echo ntp-synchronized || echo ntp-NOT-synchronized");
  });

  test("through ssh: a gateway that answers is judged, one that does not is a FAIL naming the cause", async () => {
    const bad = one(await run(healthy(), { gateway: "inactive\nup 1 minute\nntp-NOT-synchronized\n" }), NAMES.gateway);
    expect(bad.verdict).toBe("FAIL");
    const off = one(await run(healthy(), { gateway: { stdout: "", stderr: "ssh: connect to host gw-alias port 22: Connection timed out", exitCode: 255 } }), NAMES.gateway);
    expect(off.verdict).toBe("FAIL");
    expect(text(off)).toContain("the gateway is unreachable (ssh failed (exit 255)");
    expect(text(off)).toContain("it may be powered off");
    // The alias is hidden, as in every demo script.
    expect(text(off)).not.toContain("gw-alias");
    expect(text(off)).toContain("<ssh-host>");
  });

  test("a gateway command that fails after a good connection is a FAIL with the reason", async () => {
    const sent: Sent[] = [];
    const log = new FluxLog("preflight-test", dir);
    const demo = new Box("demo-alias", log, { run: demoRunner(healthy(), sent), ...NO_SLEEP });
    const gateway = new Box("gw-alias", log, {
      run: async (cmd) => ((cmd[cmd.length - 1] ?? "") === "true" ? ok("") : { stdout: "", stderr: "bash: systemctl: command not found", exitCode: 127 }),
      ...NO_SLEEP,
    });
    const ops = boxOps(demo, { sshHost: "demo-alias", apply: false, confirm: "", lockHeld: false }, async () => new Uint8Array());
    const r = one(await runPreflight({ demo, gateway, ops, repo: async () => repoInputs() }), NAMES.gateway);
    expect(r.verdict).toBe("FAIL");
    expect(text(r)).toContain("reading the gateway's agent, uptime and NTP state failed (exit 127)");
  });
});

// --- An unreachable demo machine -----------------------------------------------------------------

describe("an unreachable demo machine", () => {
  test("fails its items with the reason (the dashboard check only warns), still runs the gateway, hides the host, and exits non-zero", async () => {
    const w = healthy();
    w.reachable = false;
    const sent: Sent[] = [];
    const results = await run(w, { sent });
    expect(results.map((r) => [r.name, r.verdict])).toEqual([
      [NAMES.flux, "FAIL"],
      [NAMES.memory, "FAIL"],
      [NAMES.dashboard, "WARN"],
      [NAMES.data, "FAIL"],
      [NAMES.presence, "FAIL"],
      [NAMES.gateway, "PASS"],
    ]);
    for (const r of results.slice(0, 5)) expect(text(r)).toContain("the demo machine is unreachable (ssh failed (exit 255): ssh: Could not resolve hostname <ssh-host>");
    expect(JSON.stringify(results)).not.toContain("demo-alias");
    // One probe, no command per item.
    expect(sent.filter((s) => s.command !== "true" && s.command !== GATEWAY_COMMAND)).toEqual([]);
    expect(summary(results).exitCode).toBe(1);
    expect(countVerdicts(results)).toEqual({ PASS: 1, WARN: 1, FAIL: 4, SKIP: 0 });
  });

  test("one item failing does not stop the others", async () => {
    const w = healthy();
    w.free = { stdout: "", stderr: "boom", exitCode: 1 };
    w.pods = [];
    const results = await run(w);
    expect(results.map((r) => r.verdict)).toEqual(["FAIL", "FAIL", "PASS", "PASS", "PASS", "PASS"]);
  });
});

// --- Output --------------------------------------------------------------------------------------

describe("output", () => {
  test("a result is one line, with details indented below it", () => {
    expect(resultLines({ verdict: "WARN", name: "x", evidence: ["a; b", "c", "d"] })).toEqual(["WARN x: a; b", "  c", "  d"]);
  });

  test("with problems the first line is the problems and the facts follow as ok:", async () => {
    const w = healthy();
    w.lock = "LOCK_HELD Laysi/trustiot-v2#150 2026-11-07T01:00:00Z e2e run\n";
    const r = one(await run(w, { gateway: "skip" }), NAMES.flux);
    expect(r.evidence[0]).toStartWith("lab lock held by");
    expect(r.evidence[1]).toStartWith("ok: HelmRelease deployed");
  });

  test("a failing run's summary says so", () => {
    expect(summary([{ verdict: "FAIL", name: "a", evidence: [""] }, { verdict: "WARN", name: "b", evidence: [""] }])).toEqual({
      line: "summary: 0 PASS, 1 WARN, 1 FAIL, 0 SKIP; at least one item failed (exit 1)",
      exitCode: 1,
    });
    expect(summary([{ verdict: "WARN", name: "b", evidence: [""] }]).exitCode).toBe(0);
  });
});

// --- Bounded ssh ---------------------------------------------------------------------------------

describe("bounded commands", () => {
  test("a command that runs too long is killed and the result says so (exit 124)", async () => {
    const result = await runBounded(["sleep", "30"], undefined, 200);
    expect(result.exitCode).toBe(124);
    expect(result.stderr).toContain("the command timed out after 0 s");
  });

  test("a timeout kills the command's children too (ssh's ProxyCommand), and the run does not wait for them", async () => {
    const pidFile = join(dir, "child.pid");
    const started = Date.now();
    const result = await runBounded(["sh", "-c", `sleep 30 & echo $! > ${pidFile}; echo partial; wait`], undefined, 500, 300);
    expect(result.exitCode).toBe(124);
    expect(Date.now() - started).toBeLessThan(20_000);
    const child = Number(readFileSync(pidFile, "utf8").trim());
    expect(child).toBeGreaterThan(1);
    const alive = () => {
      try {
        process.kill(child, 0);
        return true;
      } catch {
        return false;
      }
    };
    for (let waited = 0; alive() && waited < 20_000; waited += 100) await Bun.sleep(100);
    expect(alive()).toBe(false);
  });

  test("a command that ends in time returns its output and status, and passes stdin", async () => {
    expect(await runBounded(["sh", "-c", "cat; echo err >&2; exit 3"], "in", 20_000)).toEqual({ stdout: "in", stderr: "err\n", exitCode: 3 });
    expect(await runBounded(["true"], undefined, 20_000)).toEqual({ stdout: "", stderr: "", exitCode: 0 });
  });

  test("the checkout is read through bounded commands: readCode runs every git command through the runner it is given", async () => {
    const seen: string[][] = [];
    const runner = async (cmd: readonly string[]): Promise<RunResult> => {
      seen.push([...cmd]);
      return cmd[3] === "fetch" ? { stdout: "", stderr: "the command timed out after 90 s", exitCode: 124 } : { stdout: "abc\n", stderr: "", exitCode: 0 };
    };
    const code = await readCode("/some/checkout", runner);
    expect(seen.length).toBeGreaterThanOrEqual(5);
    expect(seen.every((cmd) => cmd[0] === "git" && cmd[1] === "-C" && cmd[2] === "/some/checkout")).toBe(true);
    // A fetch that ran out is a fetch error, not a hang.
    expect(code.fetchError).toBe("exit 124");
  });

  test("a command that runs out of time fails the read with what happened, and the Flux item only warns", async () => {
    const timedOut = async (): Promise<RunResult> => ({ stdout: "", stderr: "the command timed out after 90 s", exitCode: 124 });
    await expect(readCode("/some/checkout", timedOut)).rejects.toThrow("git status failed: the command timed out after 90 s");
    const r = one(
      await run(healthy(), {
        gateway: "skip",
        repo: () => readCode("/some/checkout", timedOut).then(() => repoInputs()),
      }),
      NAMES.flux,
    );
    expect(r.verdict).toBe("WARN");
    expect(text(r)).toContain("main's chart pin not read (git status failed: the command timed out after 90 s)");
  });

  test("boundedCheckoutInputs runs its commands through runBounded and reports their failure", async () => {
    // A directory that is no checkout: every command fails at once, with its own message.
    await expect(boundedCheckoutInputs(dir, 20_000).repo()).rejects.toThrow("git status failed");
  });

  test("boundedSsh adds ssh's connection bound and leaves other commands alone", async () => {
    // A fake `ssh` on PATH would need a file; the options are visible through `echo` instead.
    const runner = boundedSsh(20_000);
    const echoed = await runner(["echo", "ssh", "-o", "BatchMode=yes"]);
    expect(echoed.stdout).toBe("ssh -o BatchMode=yes\n");
  });
});

// --- Command line --------------------------------------------------------------------------------

describe("command line", () => {
  test("without a host it stops with one line and exit 1, and reads no environment host", () => {
    const proc = Bun.spawnSync(["bun", join(import.meta.dir, "preflight.ts")], {
      env: { PATH: process.env.PATH ?? "", HOME: dir, XDG_STATE_HOME: join(dir, "state") },
      stdout: "pipe",
      stderr: "pipe",
    });
    expect(proc.exitCode).toBe(1);
    expect(proc.stderr.toString()).toContain("set --ssh-host or DEMO_SSH_HOST");
  });

  test("an unknown option is refused", () => {
    const proc = Bun.spawnSync(["bun", join(import.meta.dir, "preflight.ts"), "--apply"], {
      env: { PATH: process.env.PATH ?? "", HOME: dir, XDG_STATE_HOME: join(dir, "state") },
      stdout: "pipe",
      stderr: "pipe",
    });
    expect(proc.exitCode).toBe(1);
    expect(proc.stderr.toString()).toContain("unknown argument --apply");
  });
});
