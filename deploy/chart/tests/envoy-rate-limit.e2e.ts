// The ingress rate limit (Laysi/trustiot-v2#113) in a real Envoy: the local rate limit filter
// the chart renders for the SaaS shape, run in the Envoy build of the Istio proxy image the
// clusters use (deploy/istio/README.md, Istio 1.30.5), in front of a route that answers 200 to
// everything. No cluster: the test checks the filter itself, not that Istio attaches it to the
// gateway (that is ingress.e2e.ts's rate limit case, on a cluster).
//
// Run from the repository root, with podman or docker that runs the image's architecture:
//   bun test ./deploy/chart/tests/envoy-rate-limit.e2e.ts
// Without an engine the tests skip; with CI set they fail instead. Not part of
// `bun test deploy/chart`, which runs only *.test.ts, so CI does not pull the image on every run.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { isAbsolute, join } from "node:path";
import { ConnectionError, sendRaw } from "./e2e/raw-client.ts";
import type { RawResponse } from "./e2e/raw-client.ts";
import { envoyBootstrap, localRateLimitOf } from "./rate-limit.ts";
import type { RateLimitManifest } from "./rate-limit.ts";
import { COMMAND_TIMEOUT_MS, SLOW_TEST_MS, throwIfTimedOut, useProcessTestTimeouts } from "./timing.ts";

useProcessTestTimeouts();

// The Istio proxy of deploy/istio/README.md (Envoy 1.38.5-dev), pinned to its multi-arch index.
// Upgrade it together with Istio.
const IMAGE = "docker.io/istio/proxyv2:1.30.5@sha256:a30fb25b1e4cfc0fbc50a8a6795f5984a49022dd0ec09bcd8683b3db35af5ed3";
const LISTENER_PORT = 10080;
const CHART_DIR = join(import.meta.dir, "..");
const REPO_ROOT = join(CHART_DIR, "..", "..");
const FILES = ["values-saas.yaml", "tests/values/saas-instance.yaml", "tests/values/rate-limit-envoy.yaml"];
// tests/values/saas-instance.yaml
const APP = "app.saas.trustiot.test";
const GATEWAY = "gw.saas.trustiot.test";
// tests/values/rate-limit-envoy.yaml
const RULE_REQUESTS = 5;
const PER_CLIENT_REQUESTS = 40;
// Envoy's body for a request it limits (local rate limit filter, local reply).
const LIMITED_BODY = "local_rate_limited";
// How long Envoy may take to start answering; a loaded machine can be slow to start a container.
const START_LIMIT_MS = 90_000;

const ENGINE = Bun.which("podman") !== null ? "podman" : Bun.which("docker") !== null ? "docker" : undefined;

function command(cmd: string[], timeout = COMMAND_TIMEOUT_MS) {
  const result = Bun.spawnSync({ cmd, cwd: CHART_DIR, stdout: "pipe", stderr: "pipe", timeout });
  throwIfTimedOut(cmd, result);
  return { code: result.exitCode, stdout: result.stdout.toString(), stderr: result.stderr.toString() };
}

function pinnedHelm(): string {
  const cmd = ["proto", "bin", "helm", "--reporter", "text"];
  const result = Bun.spawnSync({ cmd, cwd: REPO_ROOT, stdout: "pipe", stderr: "pipe", timeout: COMMAND_TIMEOUT_MS });
  throwIfTimedOut(cmd, result);
  const path = result.stdout.toString().trim().split("\n").at(-1)?.trim() ?? "";
  if (result.exitCode !== 0 || !isAbsolute(path)) throw new Error(`cannot find the pinned helm; run \`proto install\`\n${result.stderr.toString()}`);
  return path;
}

function renderFilter(args: string[] = []) {
  const result = command([pinnedHelm(), "template", "trustiot", ".", "--namespace", "trustiot", ...FILES.flatMap((f) => ["--values", f]), ...args]);
  if (result.code !== 0) throw new Error(`helm template failed:\n${result.stderr}`);
  const parsed: unknown = Bun.YAML.parse(result.stdout);
  return localRateLimitOf((Array.isArray(parsed) ? parsed : [parsed]) as RateLimitManifest[]);
}

const engineWorks = ENGINE !== undefined && command([ENGINE, "info"]).code === 0;
const runningTest = engineWorks ? test : process.env.CI ? test : test.skip;
if (!engineWorks) {
  console.warn(
    process.env.CI
      ? "envoy-rate-limit: no working podman or docker, and CI is set: the tests fail"
      : "envoy-rate-limit: no working podman or docker; the tests skip",
  );
}

type Envoy = { name: string; port: number; dir: string };
const started: Envoy[] = [];

/** Starts Envoy with the rendered filter; returns once it answers on its listener. */
async function startEnvoy(label: string, args: string[] = []): Promise<Envoy> {
  if (!ENGINE) throw new Error("no podman or docker");
  const filter = renderFilter(args);
  const dir = mkdtempSync(join(tmpdir(), `trustiot-envoy-${label}-`));
  // The image runs Envoy as an unprivileged user that does not own the files.
  chmodSync(dir, 0o755);
  writeFileSync(join(dir, "envoy.json"), JSON.stringify(envoyBootstrap(filter, LISTENER_PORT), null, 2), { mode: 0o644 });
  const mount = `${dir}:/etc/trustiot-envoy:ro,Z`;
  const validate = command(
    [ENGINE, "run", "--rm", "--volume", mount, "--entrypoint", "/usr/local/bin/envoy", IMAGE, "--mode", "validate", "-c", "/etc/trustiot-envoy/envoy.json"],
    SLOW_TEST_MS,
  );
  if (validate.code !== 0) throw new Error(`Envoy rejects the configuration:\n${validate.stdout}${validate.stderr}`);
  const name = `trustiot-envoy-rate-limit-${label}-${process.pid}`;
  const run = command([
    ENGINE, "run", "--detach", "--name", name, "--publish", `127.0.0.1::${LISTENER_PORT}`, "--volume", mount,
    "--entrypoint", "/usr/local/bin/envoy", IMAGE, "-c", "/etc/trustiot-envoy/envoy.json", "--log-level", "warning",
  ]);
  if (run.code !== 0) throw new Error(`cannot start Envoy:\n${run.stderr}`);
  const envoy: Envoy = { name, port: 0, dir };
  started.push(envoy);
  const published = command([ENGINE, "port", name, `${LISTENER_PORT}/tcp`]).stdout.trim().split("\n")[0] ?? "";
  envoy.port = Number(published.split(":").at(-1));
  if (!Number.isInteger(envoy.port) || envoy.port <= 0) throw new Error(`cannot read the published port: ${published}`);
  const deadline = Date.now() + START_LIMIT_MS;
  for (;;) {
    // A client of its own, so starting takes nothing from the counts the tests check.
    const answer = await send(envoy, "GET", APP, "/started", [["cf-connecting-ip", "192.0.2.254"]]).catch((error) => {
      if (error instanceof ConnectionError) return undefined;
      throw error;
    });
    if (answer?.status === 200) return envoy;
    if (Date.now() > deadline) {
      const logs = command([ENGINE, "logs", name]);
      throw new Error(`Envoy did not answer 200 on its listener within ${START_LIMIT_MS / 1000} s (last: ${answer?.status ?? "no connection"}):\n${logs.stdout}${logs.stderr}`);
    }
    await Bun.sleep(250);
  }
}

function send(envoy: Envoy, method: string, host: string, target: string, headers: [string, string][] = []): Promise<RawResponse> {
  return sendRaw({ host: "127.0.0.1", port: envoy.port, tls: false }, { method, target, hostHeader: host, headers });
}

const limited = (response: RawResponse) => response.status === 429 && response.body.includes(LIMITED_BODY);
const describeResponse = (r: RawResponse) => `${r.status} ${JSON.stringify(r.body.slice(0, 80))}`;

/** One request: method, Host, target and extra headers. */
type Request = [string, string, string, [string, string][]];

const sendOne = (envoy: Envoy, [method, host, target, headers]: Request) => send(envoy, method, host, target, headers);

/** Sends each request in order, one at a time. */
async function sendAll(envoy: Envoy, requests: Request[]): Promise<RawResponse[]> {
  const answers: RawResponse[] = [];
  for (const request of requests) answers.push(await sendOne(envoy, request));
  return answers;
}

function expectRouted(answers: RawResponse[]) {
  expect(answers.map(describeResponse)).toEqual(answers.map(() => `200 ${JSON.stringify("routed\n")}`));
}

function expectLimited(answer: RawResponse) {
  expect(limited(answer) ? "429 local_rate_limited" : describeResponse(answer)).toBe("429 local_rate_limited");
}

let envoy: Envoy;

beforeAll(async () => {
  if (!engineWorks) return;
  envoy = await startEnvoy("main");
}, SLOW_TEST_MS + START_LIMIT_MS);

afterAll(() => {
  if (!ENGINE) return;
  for (const { name, dir } of started) {
    command([ENGINE, "rm", "--force", "--volumes", name]);
    rmSync(dir, { recursive: true, force: true });
  }
});

describe("the SaaS shape's rate limit in Istio's Envoy (Laysi/trustiot-v2#113)", () => {
  const login = (client: string | undefined, extra: [string, string][] = []): Request => [
    "POST",
    APP,
    "/auth/login",
    [...(client === undefined ? [] : ([["cf-connecting-ip", client]] as [string, string][])), ...extra],
  ];

  runningTest("over a rule's limit the gateway answers 429, and the client's own X-Forwarded-For never starts a new count", async () => {
    // Every request claims another client in each header a client controls.
    const spoofed = (i: number): [string, string][] => [
      ["x-forwarded-for", `198.51.100.${i}`],
      ["true-client-ip", `198.51.100.${i}`],
      ["x-real-ip", `198.51.100.${i}`],
    ];
    const answers = await sendAll(envoy, Array.from({ length: RULE_REQUESTS }, (_, i) => login("192.0.2.1", spoofed(i + 1))));
    expectRouted(answers);
    expectLimited(await sendOne(envoy, login("192.0.2.1", spoofed(50))));
    // A list in X-Forwarded-For, as a proxy in front of the client would write it.
    expectLimited(await send(envoy, "POST", APP, "/auth/login", [["cf-connecting-ip", "192.0.2.1"], ["x-forwarded-for", "198.51.100.60, 198.51.100.61"]]));
  });

  runningTest("each CF-Connecting-IP has its own count, IPv6 included", async () => {
    expectRouted(await sendAll(envoy, [login("192.0.2.2"), login("2001:db8::2")]));
  });

  runningTest("without CF-Connecting-IP every request is one client, whatever else it carries", async () => {
    const answers = await sendAll(
      envoy,
      Array.from({ length: RULE_REQUESTS }, (_, i) => login(undefined, [["x-forwarded-for", `198.51.100.${100 + i}`], ["true-client-ip", `198.51.100.${100 + i}`]])),
    );
    expectRouted(answers);
    expectLimited(await send(envoy, "POST", APP, "/auth/login", [["x-forwarded-for", "198.51.100.120"]]));
  });

  runningTest("every spelling the gateway routes to the same path counts toward its rule: methods, query strings, Host case, port and trailing dot, merged slashes and dot segments", async () => {
    const client: [string, string] = ["cf-connecting-ip", "192.0.2.3"];
    const answers = await sendAll(envoy, [
      ["GET", APP, "/auth/login?step=send_code", [client]],
      ["POST", "APP.SAAS.TRUSTIOT.TEST:443", "/auth/login", [client]],
      ["POST", `${APP}.`, "//auth//login", [client]],
      ["PUT", `${APP}:8443`, "/auth/./login", [client]],
      ["POST", APP, "/auth/x/../login", [client]],
    ]);
    expectRouted(answers);
    expectLimited(await send(envoy, "POST", APP, "/auth/login", [client]));
  });

  runningTest("paths and hosts beside a rule do not count toward it", async () => {
    const client: [string, string] = ["cf-connecting-ip", "192.0.2.4"];
    const beside: Request[] = [];
    for (let i = 0; i < RULE_REQUESTS + 1; i++) {
      beside.push(["POST", APP, "/auth/loginx", [client]], ["POST", APP, "/auth/login/x", [client]], ["POST", GATEWAY, "/auth/login", [client]]);
    }
    expectRouted(await sendAll(envoy, beside));
    expectRouted(await sendAll(envoy, Array.from({ length: RULE_REQUESTS }, () => login("192.0.2.4"))));
    expectLimited(await sendOne(envoy, login("192.0.2.4")));
  }, SLOW_TEST_MS);

  runningTest("a PathPrefix rule counts the prefix and everything below it, at a segment boundary", async () => {
    const client: [string, string] = ["cf-connecting-ip", "192.0.2.5"];
    const answers = await sendAll(envoy, [
      ["GET", APP, "/api/assets", [client]],
      ["GET", APP, "/api/assets/", [client]],
      ["GET", APP, "/api/assets/v1/devices", [client]],
      ["GET", APP, "/api/assets?page=2", [client]],
      ["DELETE", APP, "/api/assets/v1/devices/1", [client]],
    ]);
    expectRouted(answers);
    expectLimited(await send(envoy, "GET", APP, "/api/assets/v1/tenant", [client]));
    // Beside the prefix: only the per-client limit counts it.
    expectRouted(await sendAll(envoy, [["GET", APP, "/api/assetsx", [client]], ["GET", APP, "/api", [client]]]));
  });

  runningTest("rules on the other host count apart: the renewal's count is not the login's", async () => {
    expectRouted(await sendAll(envoy, Array.from({ length: RULE_REQUESTS }, () => login("192.0.2.6"))));
    const renew: Request = ["POST", GATEWAY, "/certs/renew", [["cf-connecting-ip", "192.0.2.6"]]];
    expectRouted(await sendAll(envoy, Array.from({ length: RULE_REQUESTS }, () => renew)));
    expectLimited(await sendOne(envoy, renew));
  });

  runningTest("the per-client limit counts every request of a client on every host and path, and the rules' paths too", async () => {
    const client: [string, string] = ["cf-connecting-ip", "192.0.2.7"];
    const answers = await sendAll(
      envoy,
      Array.from({ length: PER_CLIENT_REQUESTS }, (_, i): Request => [i % 2 === 0 ? "GET" : "POST", i % 3 === 0 ? GATEWAY : APP, `/anything/${i}`, [client]]),
    );
    expectRouted(answers);
    expectLimited(await send(envoy, "GET", APP, "/anything/else", [client]));
    // The login's own count is untouched, but the per-client one is used up.
    expectLimited(await send(envoy, "POST", APP, "/auth/login", [client]));
    // Other clients are not affected.
    expectRouted(await sendAll(envoy, [["GET", APP, "/anything/else", [["cf-connecting-ip", "192.0.2.8"]]]]));
  }, SLOW_TEST_MS);

  runningTest("past maxClients the client seen longest ago is forgotten and starts with a full allowance again", async () => {
    // README.md, 「入口的限流」: why the count per replica is bounded, and what that costs.
    const small = await startEnvoy("lru", ["--set", "ingress.rateLimit.maxClients=2"]);
    expectRouted(await sendAll(small, Array.from({ length: RULE_REQUESTS }, () => login("192.0.2.9"))));
    expectLimited(await sendOne(small, login("192.0.2.9")));
    // Two other clients fill the two places of each count.
    expectRouted(await sendAll(small, [login("192.0.2.10"), login("192.0.2.11")]));
    expectRouted(await sendAll(small, [login("192.0.2.9")]));
  }, SLOW_TEST_MS + START_LIMIT_MS);
});
