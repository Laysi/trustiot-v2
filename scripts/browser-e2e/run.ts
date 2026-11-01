// Runs the browser end-to-end tests of the single-page app (frontend/e2e) against the real
// identity service and the real `services/web` (with its production CSP):
//
//   bun scripts/browser-e2e/run.ts [--out <dir>] [--grep <test-name-filter>]
//
// Everything is made for the run and removed at its end: a separate dev environment instance
// (`scripts/dev.ts up`, own ports; Postgres with TLS and the migration Job), the frontend build,
// the identity service and `services/web` as host processes, a mail relay for the sign-in code
// (smtp.ts), a stand-in for the ingress (ingress.ts) and the control surface the tests use to
// make users and sessions (control.ts). The tests themselves run in the Playwright container
// (frontend/e2e/image.ts: pinned by digest) on the host network, so the browser reaches the
// stand-in at http://localhost:<port>. frontend/README.md, 「瀏覽器端對端測試」, has the steps and
// what the run needs; the output directory (outside the repository) has the logs and the
// Playwright report.
import { randomBytes } from "node:crypto";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { PLAYWRIGHT_IMAGE, PLAYWRIGHT_VERSION } from "../../frontend/e2e/image.ts";
import { INGRESS_HOST } from "../lib/identity.ts";
import { podman, podmanOk, requirePodman } from "../lib/podman.ts";
import { DEV_USER, POSTGRES_DATABASE } from "../lib/secrets.ts";
import { LABEL_KEY, namesFor } from "../lib/stack.ts";
import { generateDevCertificates } from "../lib/tls.ts";
import { createControl, type Database, newToken, SEED_SQL, seedVariables } from "./control.ts";
import { startIngress } from "./ingress.ts";
import { startSmtpReceiver } from "./smtp.ts";

const REPO = resolve(import.meta.dir, "..", "..");
const SCRIPT = join(REPO, "scripts", "dev.ts");
const FRONTEND = join(REPO, "frontend");

/** A cold cargo build of the two services on a loaded machine can take most of ten minutes. */
const BUILD_MS = 1_800_000;
/** What a service gets to become ready after it started. */
const READY_MS = 180_000;
/** The whole browser run: a generous bound that names what it waited for when it is reached. */
const BROWSER_MS = 1_800_000;

export interface Arguments {
  out: string | undefined;
  grep: string | undefined;
}

export function parseArguments(argv: readonly string[]): Arguments {
  const parsed: Arguments = { out: undefined, grep: undefined };
  for (let index = 0; index < argv.length; index++) {
    const flag = argv[index];
    const value = argv[index + 1];
    if ((flag === "--out" || flag === "--grep") && value !== undefined && !value.startsWith("--")) {
      parsed[flag === "--out" ? "out" : "grep"] = value;
      index++;
    } else {
      throw new Error(`usage: bun scripts/browser-e2e/run.ts [--out <dir>] [--grep <test-name-filter>] (got ${flag})`);
    }
  }
  return parsed;
}

function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen({ host: "127.0.0.1", port: 0 }, () => {
      const { port } = server.address() as { port: number };
      server.close(() => resolvePort(port));
    });
  });
}

/** KEY=value lines, as a dotenv reader or a shell `eval` takes them: single quotes are removed. */
export function parseEnv(text: string): Record<string, string> {
  return Object.fromEntries(
    text
      .trim()
      .split("\n")
      .filter((line) => line.includes("="))
      .map((line) => {
        const value = line.slice(line.indexOf("=") + 1);
        return [line.slice(0, line.indexOf("=")), /^'.*'$/.test(value) ? value.slice(1, -1) : value];
      }),
  );
}

/** Runs a command to its end, with its output in `log`; throws with the log's tail on failure. */
async function step(
  what: string,
  command: readonly string[],
  options: { cwd?: string; env?: Record<string, string | undefined>; log: string; timeoutMs: number },
): Promise<string> {
  console.log(`== ${what}`);
  const child = Bun.spawn([...command], {
    cwd: options.cwd ?? REPO,
    env: { ...process.env, ...options.env },
    stdout: "pipe",
    stderr: "pipe",
    timeout: options.timeoutMs,
  });
  const [stdout, stderr, code] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  writeFileSync(options.log, `${stdout}\n${stderr}`);
  if (code !== 0) {
    throw new Error(`${what} failed (exit ${code}); the end of ${options.log}:\n${`${stdout}\n${stderr}`.slice(-3000)}`);
  }
  return stdout;
}

/** The executables cargo built, by binary name. */
async function cargoBuild(packages: readonly string[], log: string): Promise<Record<string, string>> {
  const output = await step(
    `cargo build ${packages.join(" ")}`,
    ["cargo", "build", "--locked", ...packages.flatMap((name) => ["--package", name]), "--message-format", "json-render-diagnostics"],
    { log, timeoutMs: BUILD_MS },
  );
  const found: Record<string, string> = {};
  for (const line of output.split("\n")) {
    if (!line.startsWith("{")) continue;
    const message = JSON.parse(line) as { reason?: string; executable?: string | null; target?: { name?: string; kind?: string[] } };
    if (message.reason === "compiler-artifact" && message.executable && message.target?.kind?.includes("bin")) {
      found[message.target.name ?? ""] = message.executable;
    }
  }
  for (const name of packages) {
    if (found[name] === undefined) throw new Error(`cargo did not report an executable for ${name}`);
  }
  return found;
}

async function status(url: string): Promise<number> {
  try {
    return (await fetch(url)).status;
  } catch {
    return 0;
  }
}

async function untilReady(name: string, url: string, alive: () => boolean, logOf: () => string): Promise<void> {
  const deadline = Date.now() + READY_MS;
  while ((await status(url)) !== 200) {
    if (!alive() || Date.now() >= deadline) {
      throw new Error(`${name} did not answer 200 at ${url} within ${READY_MS / 1000} s:\n${logOf().slice(-2000)}`);
    }
    await Bun.sleep(500);
  }
}

interface Started {
  child: Bun.Subprocess;
  log(): string;
}

/** A host process whose output is collected; only this run's own handle is ever signalled. */
function start(command: readonly string[], env: Record<string, string>): Started {
  const child = Bun.spawn([...command], { env, stdout: "pipe", stderr: "pipe" });
  let text = "";
  for (const stream of [child.stdout, child.stderr]) {
    void (async () => {
      for await (const chunk of stream as AsyncIterable<Uint8Array>) text += new TextDecoder().decode(chunk);
    })();
  }
  return { child, log: () => text };
}

/** psql inside this run's own Postgres container, as the superuser over the container's socket. */
function containerDatabase(container: string): Database {
  return {
    async run(script, variables) {
      const args = [
        "exec",
        "-i",
        container,
        "psql",
        "-U",
        DEV_USER,
        "-d",
        POSTGRES_DATABASE,
        "--no-psqlrc",
        "-q",
        "-At",
        "-v",
        "ON_ERROR_STOP=1",
        ...Object.entries(variables).flatMap(([name, value]) => ["-v", `${name}=${value}`]),
        "-f",
        "-",
      ];
      const result = await podman(args, { stdin: script, timeoutMs: 60_000 });
      if (result.code !== 0) throw new Error(`psql failed: ${result.stderr.trim()}`);
      return result.stdout;
    },
  };
}

/** The Playwright packages and the tests, copied into a directory the container gets read-only. */
function stageBrowserWork(dir: string): void {
  const installed = join(FRONTEND, "node_modules", "@playwright", "test", "package.json");
  if (!existsSync(installed)) {
    throw new Error("@playwright/test is not installed: run `bun install --cwd frontend --frozen-lockfile` first");
  }
  const version = (JSON.parse(readFileSync(installed, "utf8")) as { version: string }).version;
  if (version !== PLAYWRIGHT_VERSION) {
    throw new Error(`@playwright/test ${version} is installed but the image is Playwright ${PLAYWRIGHT_VERSION}: run \`bun install --cwd frontend --frozen-lockfile\``);
  }
  mkdirSync(join(dir, "node_modules", "@playwright"), { recursive: true });
  writeFileSync(join(dir, "package.json"), `${JSON.stringify({ private: true, type: "module" })}\n`);
  cpSync(join(FRONTEND, "node_modules", "@playwright", "test"), join(dir, "node_modules", "@playwright", "test"), { recursive: true });
  for (const name of ["playwright", "playwright-core"]) {
    cpSync(join(FRONTEND, "node_modules", name), join(dir, "node_modules", name), { recursive: true });
  }
  cpSync(join(FRONTEND, "e2e"), join(dir, "e2e"), { recursive: true });
}

/** Writes what the container prints to the console and to `file`. */
async function tee(stream: ReadableStream<Uint8Array>, file: string): Promise<void> {
  let all = "";
  for await (const chunk of stream as AsyncIterable<Uint8Array>) {
    const text = new TextDecoder().decode(chunk);
    all += text;
    process.stdout.write(text);
  }
  writeFileSync(file, all);
}

export async function main(argv: readonly string[]): Promise<number> {
  const args = parseArguments(argv);
  await requirePodman();
  const out = resolve(args.out ?? mkdtempSync(join(tmpdir(), "trustiot-browser-e2e-")));
  if (out === REPO || out.startsWith(`${REPO}/`)) throw new Error("--out must be outside the repository");
  mkdirSync(out, { recursive: true });
  const work = mkdtempSync(join(tmpdir(), "trustiot-browser-e2e-work-"));
  const instance = `e2e-${randomBytes(3).toString("hex")}`;
  const containerName = `trustiot-browser-e2e-${randomBytes(3).toString("hex")}`;
  const names = namesFor(instance);
  const devEnv: Record<string, string> = {
    TRUSTIOT_DEV_INSTANCE: instance,
    TRUSTIOT_DEV_POSTGRES_PORT: String(await freePort()),
    TRUSTIOT_DEV_CLICKHOUSE_PORT: String(await freePort()),
    TRUSTIOT_DEV_MQTT_PORT: String(await freePort()),
    TRUSTIOT_DEV_PORT_EXAMPLE_SERVICE: String(await freePort()),
  };
  console.log(`output directory: ${out}\ndev environment instance: ${instance}`);

  const started: Started[] = [];
  const closers: (() => Promise<void>)[] = [];
  // What each part of the run printed, written to the output directory when the run ends.
  const logs: Record<string, () => string> = {};
  let upAttempted = false;
  let cleaned = false;
  const cleanup = async () => {
    if (cleaned) return;
    cleaned = true;
    for (const [file, text] of Object.entries(logs)) writeFileSync(join(out, file), text());
    for (const service of started) service.child.kill("SIGTERM");
    for (const service of started) await service.child.exited;
    for (const close of closers) await close().catch(() => undefined);
    await podman(["rm", "--force", "--volumes", containerName]);
    if (upAttempted) {
      const purge = await step("dev.ts purge", [process.execPath, SCRIPT, "purge"], { env: devEnv, log: join(out, "purge.log"), timeoutMs: 300_000 }).catch(
        (error: unknown) => `purge failed: ${String(error)}`,
      );
      if (purge.startsWith("purge failed")) console.error(purge);
    }
    rmSync(work, { recursive: true, force: true });
  };
  for (const signal of ["SIGINT", "SIGTERM"] as const) {
    process.on(signal, () => {
      void cleanup().finally(() => process.exit(130));
    });
  }

  try {
    upAttempted = true;
    await step("dev.ts up", [process.execPath, SCRIPT, "up"], { env: devEnv, log: join(out, "up.log"), timeoutMs: BUILD_MS });
    const settings = parseEnv(
      await step("dev.ts env", [process.execPath, SCRIPT, "env"], { env: devEnv, log: join(out, "env.log.secret"), timeoutMs: 60_000 }),
    );
    // The log of `env` holds the run's generated passwords: it is not kept.
    rmSync(join(out, "env.log.secret"), { force: true });

    const postgres = names.container("postgres");
    const label = await podmanOk(["inspect", "--format", `{{index .Config.Labels "${LABEL_KEY}"}}`, postgres]);
    if (label.trim() !== instance) throw new Error(`${postgres} is not this run's Postgres`);
    const database = containerDatabase(postgres);
    await database.run(SEED_SQL, seedVariables());

    await step("frontend build", ["bun", "run", "--cwd", FRONTEND, "build"], { log: join(out, "build.log"), timeoutMs: 600_000 });
    cpSync(join(FRONTEND, "dist"), join(work, "dist"), { recursive: true });
    const binaries = await cargoBuild(["identity", "web"], join(out, "cargo.log"));

    const certificates = generateDevCertificates(`browser-e2e-${instance}`);
    const caFile = join(work, "smtp-ca.pem");
    writeFileSync(caFile, certificates.caCertificate);
    const smtp = await startSmtpReceiver({ key: certificates.serverKey, certificate: certificates.serverCertificate });
    closers.push(smtp.close);

    const identityPort = await freePort();
    const identityHealth = await freePort();
    const identityExchange = await freePort();
    const webPort = await freePort();
    const webHealth = await freePort();
    const identity = start([binaries.identity!], {
      PATH: process.env.PATH ?? "",
      ...settings,
      // The routes of the cluster's ingress that matter here: the identity API's own audience for
      // /api/identity (`env` prints a table with the assets route only), and assets for the rest.
      TRUSTIOT_INGRESS_ROUTES: JSON.stringify([
        { host: INGRESS_HOST, path: "/api/identity", match: "PathPrefix", service: "identity" },
        { host: INGRESS_HOST, path: "/", match: "PathPrefix", service: "assets" },
      ]),
      TRUSTIOT__DATABASE__USERNAME: settings.TRUSTIOT_DEV_IDENTITY_DATABASE_USERNAME ?? "",
      TRUSTIOT__DATABASE__PASSWORD: settings.TRUSTIOT_DEV_IDENTITY_DATABASE_PASSWORD ?? "",
      TRUSTIOT__DATABASE__PLATFORM_USERNAME: settings.TRUSTIOT_DEV_IDENTITY_PLATFORM_DATABASE_USERNAME ?? "",
      TRUSTIOT__DATABASE__PLATFORM_PASSWORD: settings.TRUSTIOT_DEV_IDENTITY_PLATFORM_DATABASE_PASSWORD ?? "",
      TRUSTIOT__HTTP__LISTEN_ADDR: `127.0.0.1:${identityPort}`,
      TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR: `127.0.0.1:${identityHealth}`,
      TRUSTIOT__EXCHANGE__LISTEN_ADDR: `127.0.0.1:${identityExchange}`,
      TRUSTIOT__SHUTDOWN__DRAIN_DELAY: "100ms",
      TRUSTIOT_SMTP_HOST: "127.0.0.1",
      TRUSTIOT_SMTP_PORT: String(smtp.port),
      TRUSTIOT_SMTP_SECURITY: "tls",
      TRUSTIOT_SMTP_FROM: "TrustIoT <noreply@example.com>",
      TRUSTIOT_SMTP_CA_FILE: caFile,
    });
    started.push(identity);
    logs["identity.log"] = () => identity.log().replace(/\x1b\[[0-9;]*m/g, "");
    const web = start([binaries.web!], {
      PATH: process.env.PATH ?? "",
      TRUSTIOT__ROOT_DIR: join(work, "dist"),
      TRUSTIOT__HTTP__LISTEN_ADDR: `127.0.0.1:${webPort}`,
      TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR: `127.0.0.1:${webHealth}`,
      TRUSTIOT__SHUTDOWN__DRAIN_DELAY: "100ms",
    });
    started.push(web);
    logs["web.log"] = web.log;
    await untilReady("identity", `http://127.0.0.1:${identityHealth}/readyz`, () => identity.child.exitCode === null, identity.log);
    await untilReady("web", `http://127.0.0.1:${webHealth}/readyz`, () => web.child.exitCode === null, web.log);

    const ingress = startIngress({ identity: `http://127.0.0.1:${identityPort}`, web: `http://127.0.0.1:${webPort}` });
    closers.push(ingress.close);
    logs["ingress-requests.log"] = () => `${ingress.log().join("\n")}\n`;
    const token = newToken();
    const control = Bun.serve({
      hostname: "127.0.0.1",
      port: 0,
      fetch: createControl({ database, mails: smtp.mails, token }),
    });
    closers.push(async () => {
      await control.stop(true);
    });

    stageBrowserWork(work);
    mkdirSync(join(out, "playwright"), { recursive: true });
    console.log(`== browser (Playwright ${PLAYWRIGHT_VERSION}, ${PLAYWRIGHT_IMAGE})`);
    const browser = Bun.spawn(
      [
        "podman",
        "run",
        "--rm",
        "--name",
        containerName,
        "--network",
        "host",
        "--shm-size",
        "1g",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "-v",
        `${work}:/work:ro,Z`,
        "-v",
        `${join(out, "playwright")}:/out:Z`,
        "-w",
        "/work",
        "-e",
        `E2E_BASE_URL=http://localhost:${ingress.port}`,
        "-e",
        `E2E_CONTROL_URL=http://127.0.0.1:${control.port}`,
        "-e",
        `E2E_CONTROL_TOKEN=${token}`,
        "-e",
        "E2E_OUT=/out",
        "-e",
        "CI=1",
        PLAYWRIGHT_IMAGE,
        "node",
        "node_modules/@playwright/test/cli.js",
        "test",
        "--config",
        "e2e/playwright.config.ts",
        ...(args.grep === undefined ? [] : ["--grep", args.grep]),
      ],
      { stdout: "pipe", stderr: "pipe", timeout: BROWSER_MS },
    );
    const printed = Promise.all([tee(browser.stdout, join(out, "browser.log")), tee(browser.stderr, join(out, "browser-stderr.log"))]);
    const code = await browser.exited;
    await printed;

    console.log(`== browser exit ${code}; logs in ${out}`);
    return code;
  } finally {
    await cleanup();
  }
}

if (import.meta.main) {
  main(process.argv.slice(2)).then(
    (code) => process.exit(code),
    (error: unknown) => {
      console.error(String(error instanceof Error ? error.message : error));
      process.exit(1);
    },
  );
}
