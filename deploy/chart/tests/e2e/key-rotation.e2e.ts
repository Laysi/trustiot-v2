// Rotation of the internal identity signing key by age, e2e (Laysi/trustiot-v2#429;
// docs/architecture/secrets.md, 「依金鑰年齡自動輪替」): installs the chart as identity-keys.e2e.ts
// does (the bundled Postgres, the migration Job, the identity service built from the commit and
// crates/service-kit's example service in the place of assets), with
// internalIdentity.keyPropagationSeconds 120, maxKeyAgeSeconds 900 and the rotation CronJob running
// every minute. Then checks
//   1. the CronJob runs as the identity service's ServiceAccount, mounts no Secret, gives no other
//      account access, and leaves a key that is not due as it is;
//   2. automatic rotation: once the key is 900 s old a run adds a key of the same generation,
//      published first and signing 120 s later; tokens of the old key still verify after the
//      switch, the new key's tokens verify, and a probe that asks the identity service for a token
//      and has the example service verify it every few seconds never fails; no pod restarts;
//   3. the CronJob, not a restart, retires the old key once its last token expired;
//   4. a manual rotation (keyGeneration 2) while automatic rotation is on adds generation 2;
//   5. a revocation (revokeBeforeGeneration 3) removes every older key, the automatic ones too;
//   6. maxKeyAgeSeconds 0 removes the CronJob, and a key past the age is not rotated, not even by
//      a restart of the identity service;
//   7. no private key is in a log (the CronJob's runs included), the ConfigMap or an event.
// The test reads the Secret into this process only to know each key's generation and times and to
// check that no private key appears anywhere else; it prints and records key ids, generations and
// times, never a key, and asserts on a private key only as a yes or no.
// README.md in this directory lists the prerequisites; run from the repository root:
//
//   E2E_NAMESPACE=e2e-429 E2E_POSTGRES_IMAGE=<image> E2E_IDENTITY_IMAGE=<image> \
//   E2E_SERVICE_IMAGE=<image> E2E_MIGRATE_IMAGE=<image> \
//     bun test --timeout 3600000 ./deploy/chart/tests/e2e/key-rotation.e2e.ts
//
// Not part of `bun test deploy/chart` (the file name has no .test. suffix): it needs a cluster.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import type { Subprocess } from "bun";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { isAbsolute, join } from "node:path";

import { freePort } from "./ports.ts";
import { sendRaw } from "./raw-client.ts";

const CHART_DIR = join(import.meta.dir, "..", "..");
const REPO_ROOT = join(CHART_DIR, "..", "..");

function required(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required (see deploy/chart/tests/e2e/README.md)`);
  return value;
}

const NAMESPACE = required("E2E_NAMESPACE");
const POSTGRES_IMAGE = required("E2E_POSTGRES_IMAGE");
const IDENTITY_IMAGE = required("E2E_IDENTITY_IMAGE");
const SERVICE_IMAGE = required("E2E_SERVICE_IMAGE");
const MIGRATE_IMAGE = required("E2E_MIGRATE_IMAGE");
const SHAPE = process.env.E2E_SHAPE ?? "saas";
const CONTEXT = process.env.E2E_KUBECONTEXT;
const RELEASE = process.env.E2E_RELEASE ?? "trustiot";
const PULL_POLICY = process.env.E2E_IMAGE_PULL_POLICY ?? "IfNotPresent";
const REPORT_DIR = process.env.E2E_REPORT_DIR;
const KEEP = process.env.E2E_KEEP === "1";
if (SHAPE !== "saas" && SHAPE !== "onprem") throw new Error(`E2E_SHAPE must be saas or onprem, not ${SHAPE}`);

const PREFIX = RELEASE.includes("trustiot") ? RELEASE : `${RELEASE}-trustiot`;
const CLUSTER = `${PREFIX}-postgres`;
if (CLUSTER.length > 50) throw new Error(`E2E_RELEASE ${RELEASE} is too long: the Cluster name ${CLUSTER} must fit in 50 characters`);
const DATABASE = "trustiot";
const IDENTITY = `${PREFIX}-identity`;
const ASSETS = `${PREFIX}-assets`;
const ROTATION = `${PREFIX}-identity-key-rotation`;
const ROTATION_COMPONENT = "identity-key-rotation";
const SIGNING_KEYS = `${PREFIX}-identity-signing-keys`;
const PUBLIC_KEYS = `${PREFIX}-internal-identity-keys`;
const SIGNING_KEYS_KEY = "signing-keys.json";
const PUBLIC_KEYS_KEY = "jwks.json";
const INIT_CONTAINER = "signing-keys";
// The Secrets the chart declares empty for this install and keeps on uninstall.
const KEPT_SECRETS = [`${PREFIX}-identity-password`, `${PREFIX}-clickhouse-admin-password`, SIGNING_KEYS];
const VALUES = "tests/e2e/values-identity-keys-e2e.yaml";
// values-identity-keys-e2e.yaml: internalIdentity.keyPropagationSeconds.
const PROPAGATION_S = 120;
// The maximum key age of this test: 355 s above the shortest the chart allows (2 * 120 + 305 =
// 545 s), so the CronJob retires the old key, and the example service drops it, well before the
// new key is due in turn; the manual rotation after that step then finds no automatic one under
// way.
const MAX_AGE_S = 900;
const SCHEDULE = "* * * * *";
// crates/service-kit: the longest token lifetime a service accepts plus the clock leeway.
const LAST_TOKEN_S = 300 + 5;
// The identity service's ext_authz endpoint and the cookie (services/identity).
const EXT_AUTHZ = "/internal/ext-authz";
const SESSION_COOKIE = "__Host-trustiot-session";
const IDENTITY_HEADER = "x-internal-identity";
const HTTP_PORT = 8080;
// The log messages the test reads (crates/service-kit/src/identity/published.rs,
// services/identity/src/issuer.rs, tools/migrate).
const PUBLIC_KEYS_LOADED = "verifying internal identity tokens with these public keys of the identity service";
const SIGNING_WITH = "signing internal identity tokens";
const KEYS_IN_PLACE = "the internal identity signing keys are in place";
const MIGRATION_CHANGE = "migration change";

function pinnedTool(tool: string): string {
  const result = Bun.spawnSync({ cmd: ["proto", "bin", tool, "--reporter", "text"], cwd: REPO_ROOT });
  const path = result.stdout.toString().trim().split("\n").at(-1)?.trim() ?? "";
  if (result.exitCode !== 0 || !isAbsolute(path) || !existsSync(path)) {
    throw new Error(`cannot find the pinned ${tool}; run \`proto install\``);
  }
  return path;
}
const HELM = pinnedTool("helm");
const KUBECTL = pinnedTool("kubectl");

interface Result {
  code: number;
  stdout: string;
  stderr: string;
}
function exec(cmd: string[], stdin?: string): Result {
  const proc = Bun.spawnSync({
    cmd,
    cwd: CHART_DIR,
    stdin: stdin === undefined ? "ignore" : new TextEncoder().encode(stdin),
    stdout: "pipe",
    stderr: "pipe",
  });
  return { code: proc.exitCode ?? -1, stdout: proc.stdout.toString(), stderr: proc.stderr.toString() };
}
function run(cmd: string[], stdin?: string): string {
  const result = exec(cmd, stdin);
  if (result.code !== 0) throw new Error(`${cmd.slice(0, 6).join(" ")} failed (${result.code}):\n${result.stderr}`);
  return result.stdout;
}
const kubectlArgs = () => [KUBECTL, ...(CONTEXT ? ["--context", CONTEXT] : []), "--namespace", NAMESPACE];
const kubectl = (...args: string[]) => run([...kubectlArgs(), ...args]);
const kubectlIn = (stdin: string, ...args: string[]) => run([...kubectlArgs(), ...args], stdin);
const helm = (...args: string[]) => run([HELM, ...(CONTEXT ? ["--kube-context", CONTEXT] : []), "--namespace", NAMESPACE, ...args]);
const json = <T>(...args: string[]) => JSON.parse(kubectl(...args, "--output", "json")) as T;

async function waitFor<T>(what: string, check: () => T | undefined | Promise<T | undefined>, timeoutMs = 300_000, everyMs = 2_000) {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  while (Date.now() < deadline) {
    try {
      const value = await check();
      if (value !== undefined) return value;
    } catch (error) {
      last = error;
    }
    await Bun.sleep(everyMs);
  }
  throw new Error(`timed out after ${Math.round(timeoutMs / 1000)} s waiting for ${what}${last ? `; last error: ${String(last)}` : ""}`);
}

function imageValues(path: string, image: string, what: string): string[] {
  const match = /^(?<repository>[^@]+?)(?::(?<tag>[\w][\w.-]*))?(?:@(?<digest>sha256:[a-f0-9]{64}))?$/.exec(image);
  if (!match?.groups?.repository) throw new Error(`${what} is not an image reference: ${image}`);
  const { repository, tag, digest } = match.groups;
  return [
    "--set-string",
    `${path}.repository=${repository}`,
    "--set-string",
    `${path}.tag=${tag ?? "latest"}`,
    "--set-string",
    `${path}.digest=${digest ?? ""}`,
    "--set-string",
    `${path}.pullPolicy=${PULL_POLICY}`,
  ];
}

const base = [
  "--values",
  `values-${SHAPE}.yaml`,
  "--values",
  `tests/values/${SHAPE}-instance.yaml`,
  "--values",
  VALUES,
  ...imageValues("postgres.bundled.image", POSTGRES_IMAGE, "E2E_POSTGRES_IMAGE"),
  ...imageValues("services.identity.image", IDENTITY_IMAGE, "E2E_IDENTITY_IMAGE"),
  ...imageValues("services.assets.image", SERVICE_IMAGE, "E2E_SERVICE_IMAGE"),
  ...imageValues("migrate.image", MIGRATE_IMAGE, "E2E_MIGRATE_IMAGE"),
  "--set",
  `internalIdentity.maxKeyAgeSeconds=${MAX_AGE_S}`,
  "--set-string",
  `internalIdentity.keyRotationSchedule=${SCHEDULE}`,
];
const APP_HOST = (Bun.YAML.parse(readFileSync(join(CHART_DIR, `tests/values/${SHAPE}-instance.yaml`), "utf8")) as { hosts: { app: string } }).hosts.app;

const RUN = crypto.randomUUID().replaceAll("-", "").slice(0, 8);
const created = { release: false, kept: false };
const report: Record<string, unknown> = {
  run: RUN,
  shape: SHAPE,
  namespace: NAMESPACE,
  release: RELEASE,
  propagationSeconds: PROPAGATION_S,
  maxKeyAgeSeconds: MAX_AGE_S,
  schedule: SCHEDULE,
};
const forwards: Subprocess[] = [];

function upgrade(...extra: string[]) {
  helm("upgrade", RELEASE, ".", ...base, ...extra, "--wait=legacy", "--timeout", "15m");
  rolledOut();
}
function rolledOut() {
  for (const deployment of [IDENTITY, ASSETS]) kubectl("rollout", "status", `deployment/${deployment}`, "--timeout=900s");
}

// ---------------------------------------------------------------------------------------------
// What the cluster holds

interface StoredKey {
  kid: string;
  generation: number;
  published_at?: number;
  signs_from?: number;
}
/** The Secret's keys without their private parts, and the private parts on their own (never printed). */
function signingKeys(): { keys: StoredKey[]; privates: string[] } {
  const secret = json<{ data?: Record<string, string> }>("get", "secret", SIGNING_KEYS);
  const text = Buffer.from(secret.data?.[SIGNING_KEYS_KEY] ?? "", "base64").toString();
  const parsed = JSON.parse(text) as { keys: (StoredKey & { d?: string })[] };
  return {
    keys: parsed.keys.map(({ kid, generation, published_at, signs_from }) => ({ kid, generation, published_at, signs_from })),
    privates: parsed.keys.map((key) => key.d ?? "").filter((d) => d !== ""),
  };
}
function publicKids(): string[] {
  const configMap = json<{ data?: Record<string, string> }>("get", "configmap", PUBLIC_KEYS);
  const set = JSON.parse(configMap.data?.[PUBLIC_KEYS_KEY] ?? "") as { keys: { kid?: string }[] };
  return set.keys.map((key) => key.kid ?? "");
}
const kidsOfKeys = (keys: StoredKey[]) => keys.map((k) => k.kid);

interface Pod {
  metadata: { name: string };
  status: { containerStatuses?: { name: string; restartCount: number }[] };
}
function podsOf(component: string): Pod[] {
  return json<{ items: (Pod & { metadata: { deletionTimestamp?: string } })[] }>(
    "get",
    "pods",
    "--selector",
    `app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=${component}`,
  ).items.filter((pod) => pod.metadata.deletionTimestamp === undefined);
}
function onePod(component: string): Pod {
  const pods = podsOf(component);
  if (pods.length !== 1) throw new Error(`expected one ${component} pod, found ${pods.length}`);
  return pods[0] as Pod;
}
const restartsOf = (pod: Pod, container: string) => pod.status.containerStatuses?.find((c) => c.name === container)?.restartCount;
const time = (value: string | undefined) => (value === undefined ? undefined : Date.parse(value));
const seconds = (ms: number | undefined) => (ms === undefined ? undefined : Math.round(ms / 100) / 10);

interface LogLine {
  at: number;
  message: string;
  fields: Record<string, unknown>;
}
function parseLog(text: string): LogLine[] {
  return text.split("\n").flatMap((line) => {
    try {
      const parsed = JSON.parse(line) as { timestamp?: string; message?: string; fields?: Record<string, unknown> };
      const at = time(parsed.timestamp);
      return at === undefined || parsed.message === undefined ? [] : [{ at, message: parsed.message, fields: parsed.fields ?? {} }];
    } catch {
      return [];
    }
  });
}
const logOf = (pod: string, container: string) => parseLog(kubectl("logs", pod, "--container", container));
const kidsOfLine = (line: LogLine) => [...String(line.fields.keys ?? "").matchAll(/"([A-Za-z0-9_-]+)"/g)].map((m) => m[1] as string).sort();
const sameKids = (a: string[], b: string[]) => JSON.stringify([...a].sort()) === JSON.stringify([...b].sort());
function publicKeysLoadedAt(pod: string, container: string, kids: string[], since = 0): number | undefined {
  return logOf(pod, container).find((line) => line.message === PUBLIC_KEYS_LOADED && line.at >= since && sameKids(kidsOfLine(line), kids))?.at;
}
function signingWithAt(pod: string, kid: string, since = 0): number | undefined {
  return logOf(pod, "identity").find((line) => line.message === SIGNING_WITH && line.at >= since && line.fields.kid === kid)?.at;
}

// The rotation CronJob's runs: every Job's log, read while the Job exists (the CronJob keeps only
// the last successful one), with the times it started and finished.
interface RotationRun {
  job: string;
  startedAt?: number;
  finishedAt?: number;
  succeeded: boolean;
  serviceAccount?: string;
  changes: string[];
  inPlace?: Record<string, unknown>;
  text: string;
}
const runs = new Map<string, RotationRun>();
function collectRuns(): RotationRun[] {
  const jobs = json<{
    items: {
      metadata: { name: string };
      spec: { template: { spec: { serviceAccountName?: string } } };
      status: { startTime?: string; completionTime?: string; succeeded?: number; failed?: number };
    }[];
  }>("get", "jobs", "--selector", `app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=${ROTATION_COMPONENT}`).items;
  for (const job of jobs) {
    const name = job.metadata.name;
    const finished = (job.status.succeeded ?? 0) > 0 || (job.status.failed ?? 0) > 0;
    if (!finished || runs.has(name)) continue;
    const logs = exec([...kubectlArgs(), "logs", `job/${name}`, "--all-containers"]);
    const text = logs.code === 0 ? logs.stdout : "";
    const lines = parseLog(text);
    runs.set(name, {
      job: name,
      startedAt: time(job.status.startTime),
      finishedAt: time(job.status.completionTime),
      succeeded: (job.status.succeeded ?? 0) > 0,
      serviceAccount: job.spec.template.spec.serviceAccountName,
      changes: lines.filter((line) => line.message === MIGRATION_CHANGE).map((line) => String(line.fields.change ?? "")),
      inPlace: lines.find((line) => line.message === KEYS_IN_PLACE)?.fields,
      text,
    });
  }
  return [...runs.values()];
}

// ---------------------------------------------------------------------------------------------
// Tokens

interface Session {
  userId: string;
  sessionId: string;
  cookie: string;
  tokenHash: string;
}
function newSession(): Session {
  const secret = crypto.getRandomValues(new Uint8Array(32));
  return {
    userId: crypto.randomUUID(),
    sessionId: crypto.randomUUID(),
    cookie: Buffer.from(secret).toString("base64url"),
    tokenHash: new Bun.CryptoHasher("sha256").update(secret).digest("hex"),
  };
}
const primary = () => {
  const name = json<{ status?: { currentPrimary?: string } }>("get", `cluster.postgresql.cnpg.io/${CLUSTER}`).status?.currentPrimary;
  if (!name) throw new Error(`Cluster ${CLUSTER} has no current primary`);
  return name;
};
// A platform administrator with a session, as identity-keys.e2e.ts makes it: SQL in the primary
// as the postgres role over the local socket, on stdin. Only the session secret's hash is stored.
function addSession(session: Session) {
  kubectlIn(
    [
      "BEGIN;",
      `INSERT INTO identity.users (id, platform_admin) VALUES ('${session.userId}', true);`,
      "INSERT INTO identity.sessions (id, token_hash, user_id, expires_at)",
      `  VALUES ('${session.sessionId}', '\\x${session.tokenHash}', '${session.userId}', now() + interval '3 hours');`,
      "COMMIT;",
      "",
    ].join("\n"),
    "exec",
    "-i",
    primary(),
    "-c",
    "postgres",
    "--",
    "psql",
    "--no-psqlrc",
    "--quiet",
    "--set",
    "ON_ERROR_STOP=1",
    "--dbname",
    DATABASE,
  );
}

/** A kubectl port-forward to one Service's port, until `close`. */
async function openForward(service: string): Promise<{ port: number; close: () => Promise<void> }> {
  const port = await freePort();
  const forward = Bun.spawn([...kubectlArgs(), "port-forward", `service/${service}`, `${port}:${HTTP_PORT}`], { stdout: "ignore", stderr: "ignore" });
  forwards.push(forward);
  await waitFor(`port-forward to ${service}`, async () => {
    try {
      const socket = await Bun.connect({ hostname: "127.0.0.1", port, socket: { data() {} } });
      socket.end();
      return true;
    } catch {
      return undefined;
    }
  }, 60_000, 250);
  return {
    port,
    close: async () => {
      forward.kill();
      await forward.exited;
    },
  };
}
// A new forward for each use where pods are replaced (a forward stays with the pod it picked).
async function forwarded<T>(service: string, use: (port: number) => Promise<T>): Promise<T> {
  const forward = await openForward(service);
  try {
    return await use(forward.port);
  } finally {
    await forward.close();
  }
}

/** A token of the session's user for the assets route, from the identity service's ext_authz check; never printed. */
async function tokenAt(port: number, session: Session): Promise<string> {
  const answer = await sendRaw(
    { host: "127.0.0.1", port, tls: false },
    { target: `${EXT_AUTHZ}/api/assets/v1/whoami`, hostHeader: APP_HOST, headers: [["cookie", `${SESSION_COOKIE}=${session.cookie}`]] },
  );
  const tokens = answer.headers[IDENTITY_HEADER] ?? [];
  if (answer.status !== 200 || tokens.length !== 1) throw new Error(`the ext_authz check answered ${answer.status} with ${tokens.length} identity header(s)`);
  return tokens[0] as string;
}
function kidOf(token: string): string {
  const header = JSON.parse(Buffer.from(token.split(".")[0] ?? "", "base64url").toString()) as { kid?: string };
  return header.kid ?? "";
}
/** The example service's answer to GET /v1/whoami with the token: its status and the user it saw. */
async function verifyAt(port: number, token: string): Promise<{ status: number; user?: string }> {
  const answer = await fetch(`http://127.0.0.1:${port}/v1/whoami`, { headers: { [IDENTITY_HEADER]: token } });
  const body = (await answer.json().catch(() => ({}))) as { user_id?: string; user?: string };
  return { status: answer.status, user: body.user_id ?? body.user };
}
async function roundTrip(session: Session): Promise<{ kid: string; status: number; userMatches: boolean }> {
  const issued = await forwarded(IDENTITY, (port) => tokenAt(port, session));
  const answer = await forwarded(ASSETS, (port) => verifyAt(port, issued));
  return { kid: kidOf(issued), status: answer.status, userMatches: answer.user === session.userId };
}

// ---------------------------------------------------------------------------------------------

const privates = new Set<string>();
function remember(keys: { privates: string[] }) {
  for (const d of keys.privates) privates.add(d);
}
const leaks = (text: string) => [...privates].some((d) => text.includes(d));

const session = newSession();

beforeAll(async () => {
  const status = exec([HELM, ...(CONTEXT ? ["--kube-context", CONTEXT] : []), "--namespace", NAMESPACE, "status", RELEASE]);
  if (status.code === 0) throw new Error(`release ${RELEASE} already exists in ${NAMESPACE}; use another E2E_RELEASE or namespace`);
  if (!status.stderr.includes("not found")) throw new Error(`helm status ${RELEASE} failed:\n${status.stderr}`);
  const leftovers = [
    kubectl("get", `cluster.postgresql.cnpg.io/${CLUSTER}`, `imagecatalog.postgresql.cnpg.io/${CLUSTER}`, "--ignore-not-found", "--output", "name"),
    kubectl("get", "pvc", "--selector", `cnpg.io/cluster=${CLUSTER}`, "--output", "name"),
    kubectl("get", "pvc", `data-${PREFIX}-clickhouse-0`, "--ignore-not-found", "--output", "name"),
    kubectl("get", "secret", ...KEPT_SECRETS, "--ignore-not-found", "--output", "name"),
    kubectl("get", "configmap", PUBLIC_KEYS, "--ignore-not-found", "--output", "name"),
  ]
    .join("\n")
    .split("\n")
    .filter((line) => line.trim() !== "");
  if (leftovers.length > 0) {
    throw new Error(`${NAMESPACE} already has ${leftovers.join(", ")}, which this run would delete when it cleans up; use another E2E_RELEASE or namespace`);
  }
  const started = Date.now();
  created.release = true;
  created.kept = true;
  helm("install", RELEASE, ".", ...base, "--wait=legacy", "--wait-for-jobs", "--timeout", "20m");
  rolledOut();
  report.install = { helmSeconds: seconds(Date.now() - started) };
  addSession(session);
}, 1_500_000);

afterAll(() => {
  for (const forward of forwards) forward.kill();
  if (REPORT_DIR) {
    mkdirSync(REPORT_DIR, { recursive: true });
    writeFileSync(join(REPORT_DIR, `key-rotation-e2e-${SHAPE}.json`), `${JSON.stringify(report, null, 2)}\n`);
  }
  if (KEEP) return;
  const errors: string[] = [];
  const steps: [boolean, () => unknown][] = [
    [created.release, () => helm("uninstall", RELEASE, "--wait", "--timeout", "5m")],
    [created.kept, () => kubectl("delete", "secret", ...KEPT_SECRETS, "--ignore-not-found")],
    [created.kept, () => kubectl("delete", "configmap", PUBLIC_KEYS, "--ignore-not-found")],
    [created.kept, () => kubectl("delete", "pvc", `data-${PREFIX}-clickhouse-0`, "--ignore-not-found", "--wait=true", "--timeout=180s")],
    [created.kept, () => kubectl("delete", `cluster.postgresql.cnpg.io/${CLUSTER}`, "--ignore-not-found", "--wait=true", "--timeout=180s")],
    [created.kept, () => kubectl("delete", `imagecatalog.postgresql.cnpg.io/${CLUSTER}`, "--ignore-not-found", "--wait=true")],
    [created.kept, () => kubectl("delete", "pvc", "--selector", `cnpg.io/cluster=${CLUSTER}`, "--ignore-not-found", "--wait=true", "--timeout=180s")],
  ];
  for (const [done, cleanup] of steps) {
    if (!done) continue;
    try {
      cleanup();
    } catch (error) {
      errors.push(String(error));
    }
  }
  if (errors.length) throw new Error(`cleanup failed:\n${errors.join("\n")}`);
}, 900_000);

// The key ids as the steps find them.
const kid: Record<string, string> = {};

describe(`rotation of the internal identity key by age (${SHAPE} shape)`, () => {
  test("the CronJob runs as the identity service's account, mounts no Secret, and leaves a key that is not due", async () => {
    const keys = signingKeys();
    remember(keys);
    expect(keys.keys.length).toBe(1);
    const first = keys.keys[0] as StoredKey;
    kid.first = first.kid;
    expect(first.generation).toBe(1);

    const cronJob = json<{
      spec: { schedule: string; jobTemplate: { spec: { template: { spec: { serviceAccountName: string; volumes?: unknown[]; containers: unknown[] } } } } };
    }>("get", "cronjob", ROTATION);
    const pod = cronJob.spec.jobTemplate.spec.template.spec;
    expect(cronJob.spec.schedule).toBe(SCHEDULE);
    expect(pod.serviceAccountName).toBe(IDENTITY);
    expect(JSON.stringify(pod.volumes ?? [])).not.toContain('"secret"');
    expect(JSON.stringify(pod)).not.toContain("secretKeyRef");

    // Only the identity service's account may get and patch the two objects, as without the CronJob.
    const accounts = json<{ items: { metadata: { name: string } }[] }>("get", "serviceaccounts").items.map((item) => item.metadata.name);
    const named = [`secrets/${SIGNING_KEYS}`, `configmaps/${PUBLIC_KEYS}`];
    const questions = [
      ...named.flatMap((resource) => ["get", "patch", "update", "delete"].map((verb) => ({ verb, resource }))),
      ...["secrets", "configmaps"].flatMap((resource) => ["create", "list", "watch"].map((verb) => ({ verb, resource }))),
    ];
    const allowed: string[] = [];
    for (const account of accounts) {
      for (const { verb, resource } of questions) {
        const answer = exec([...kubectlArgs(), "auth", "can-i", verb, resource, "--as", `system:serviceaccount:${NAMESPACE}:${account}`]).stdout.trim();
        if (answer !== "yes" && answer !== "no") throw new Error(`kubectl auth can-i ${verb} ${resource} as ${account} answered ${JSON.stringify(answer)}`);
        if (answer === "yes") allowed.push(`${account} ${verb} ${resource}`);
      }
    }
    report.access = { serviceAccounts: accounts, questions: questions.length, allowed };
    expect(allowed.sort()).toEqual(
      [`${IDENTITY} get secrets/${SIGNING_KEYS}`, `${IDENTITY} patch secrets/${SIGNING_KEYS}`, `${IDENTITY} get configmaps/${PUBLIC_KEYS}`, `${IDENTITY} patch configmaps/${PUBLIC_KEYS}`].sort(),
    );

    // A run that finds the key not yet due changes nothing and says when it will be.
    // (A run during the install may have generated the first key itself, racing the init
    // container; only runs after its publication count here.)
    const notDue = await waitFor(
      "a finished run of the rotation CronJob after the key was published",
      () => collectRuns().find((r) => r.succeeded && (r.startedAt ?? 0) >= (first.published_at ?? 0) * 1000),
      300_000,
      5_000,
    );
    expect(notDue.serviceAccount).toBe(IDENTITY);
    expect(notDue.changes).toEqual([]);
    expect(notDue.inPlace?.rotation_due_at).toBe((first.published_at ?? 0) + MAX_AGE_S);
    expect(signingKeys().keys).toEqual(keys.keys);
    report.notDue = { job: notDue.job, rotationDueAt: notDue.inPlace?.rotation_due_at, publishedAt: first.published_at };

    const trip = await roundTrip(session);
    expect(trip).toEqual({ kid: first.kid, status: 200, userMatches: true });
  }, 600_000);

  test("once the key reached the maximum age a run replaces it on the schedule of a manual rotation, with no failed request and no restart", async () => {
    const first = signingKeys().keys[0] as StoredKey;
    const identityBefore = onePod("identity");
    const assetsBefore = onePod("assets");
    const restarts = { identity: restartsOf(identityBefore, "identity"), assets: restartsOf(assetsBefore, "assets") };

    // The probe: a token from the identity service, verified by the example service, every 2 s,
    // through forwards that stay with the two pods (nothing restarts them in this step).
    const identityForward = await openForward(IDENTITY);
    const assetsForward = await openForward(ASSETS);
    const probes: { at: number; kid?: string; status: number; userMatches: boolean; error?: string }[] = [];
    let probing = true;
    const prober = (async () => {
      while (probing) {
        const at = Date.now();
        try {
          const issued = await tokenAt(identityForward.port, session);
          const answer = await verifyAt(assetsForward.port, issued);
          probes.push({ at, kid: kidOf(issued), status: answer.status, userMatches: answer.user === session.userId });
        } catch (error) {
          probes.push({ at, status: 0, userMatches: false, error: String(error).slice(0, 200) });
        }
        await Bun.sleep(2_000);
      }
    })();

    try {
      const dueAt = ((first.published_at ?? 0) + MAX_AGE_S) * 1000;
      // A run a minute: the new key appears within a minute or two after it is due.
      const keys = await waitFor(
        "the rotation CronJob to add a key",
        () => {
          const now = signingKeys();
          return now.keys.length === 2 && now.keys.every((k) => k.published_at !== undefined) ? now : undefined;
        },
        dueAt - Date.now() + 300_000,
        3_000,
      );
      remember(keys);
      const second = keys.keys[1] as StoredKey;
      kid.second = second.kid;
      expect(keys.keys[0]?.kid).toBe(first.kid);
      // The same generation; published first, signing keyPropagationSeconds later.
      expect(second.generation).toBe(1);
      expect((second.published_at ?? 0) * 1000).toBeGreaterThanOrEqual(dueAt);
      expect((second.signs_from ?? 0) - (second.published_at ?? 0)).toBe(PROPAGATION_S);
      expect(publicKids().sort()).toEqual([first.kid, second.kid].sort());
      const publishedAt = (second.published_at ?? 0) * 1000;
      const signsFrom = (second.signs_from ?? 0) * 1000;
      const rotated = await waitFor(
        "the log of the run that rotated",
        () => collectRuns().find((r) => r.changes.some((change) => change.includes(`generated signing key ${second.kid}`))),
        120_000,
        3_000,
      );
      expect(rotated.serviceAccount).toBe(IDENTITY);
      expect(rotated.changes[0]).toContain(`(generation 1) to replace key ${first.kid}, published `);
      expect(rotated.changes[0]).toContain("which reached internalIdentity.maxKeyAgeSeconds");

      // A token of the old key, taken shortly before the switch, still verifies after it.
      await Bun.sleep(Math.max(signsFrom - Date.now() - 20_000, 0));
      const oldToken = await tokenAt(identityForward.port, session);
      const takenAt = Date.now();
      expect(kidOf(oldToken)).toBe(first.kid);
      const switched = await waitFor(
        "the identity service to sign with the new key",
        () => signingWithAt(identityBefore.metadata.name, second.kid),
        signsFrom - Date.now() + 300_000,
        3_000,
      );
      expect(switched).toBeGreaterThanOrEqual(signsFrom);
      const oldAfterSwitch = await verifyAt(assetsForward.port, oldToken);
      const verifiedAt = Date.now();
      expect({ status: oldAfterSwitch.status, userMatches: oldAfterSwitch.user === session.userId }).toEqual({ status: 200, userMatches: true });
      const newToken = await tokenAt(identityForward.port, session);
      expect(kidOf(newToken)).toBe(second.kid);
      const fresh = await verifyAt(assetsForward.port, newToken);
      expect({ status: fresh.status, userMatches: fresh.user === session.userId }).toEqual({ status: 200, userMatches: true });

      // Every pod loaded the new public key without a restart.
      const assetsLoaded = publicKeysLoadedAt(assetsBefore.metadata.name, "assets", [first.kid, second.kid]);
      const identityLoaded = publicKeysLoadedAt(identityBefore.metadata.name, "identity", [first.kid, second.kid]);
      expect(assetsLoaded).toBeDefined();
      expect(identityLoaded).toBeDefined();
      const identityAfter = onePod("identity");
      const assetsAfter = onePod("assets");
      expect([identityAfter.metadata.name, assetsAfter.metadata.name]).toEqual([identityBefore.metadata.name, assetsBefore.metadata.name]);
      expect({ identity: restartsOf(identityAfter, "identity"), assets: restartsOf(assetsAfter, "assets") }).toEqual(restarts);

      // Keep probing a little past the switch.
      await Bun.sleep(30_000);
      probing = false;
      await prober;
      const failed = probes.filter((p) => p.status !== 200 || !p.userMatches);
      const kidsInOrder = probes.map((p) => p.kid).filter((k, i, all) => k !== undefined && k !== all[i - 1]);
      report.automaticRotation = {
        first: { kid: first.kid, generation: first.generation, publishedAt: first.published_at },
        second: { kid: second.kid, generation: second.generation, publishedAt: second.published_at, signsFrom: second.signs_from },
        rotatingJob: rotated.job,
        rotatingJobChanges: rotated.changes,
        publishedAfterDueSeconds: seconds(publishedAt - dueAt),
        assetsLoadedAfterPublishedSeconds: seconds((assetsLoaded ?? 0) - publishedAt),
        identityPublicKeysLoadedAfterPublishedSeconds: seconds((identityLoaded ?? 0) - publishedAt),
        identitySwitchedAfterSignsFromSeconds: seconds(switched - signsFrom),
        oldTokenTakenBeforeSignsFromSeconds: seconds(signsFrom - takenAt),
        oldTokenVerifiedAfterSignsFromSeconds: seconds(verifiedAt - signsFrom),
        oldTokenAfterSwitch: oldAfterSwitch.status,
        newToken: { kid: kidOf(newToken), status: fresh.status },
        probes: { count: probes.length, failed: failed.length, firstAt: probes[0]?.at, lastAt: probes.at(-1)?.at, kidsInOrder, failures: failed.slice(0, 5) },
        restarts: { before: restarts, after: { identity: restartsOf(identityAfter, "identity"), assets: restartsOf(assetsAfter, "assets") } },
      };
      console.log(`automatic rotation: ${JSON.stringify(report.automaticRotation)}`);
      expect(failed).toEqual([]);
      expect(kidsInOrder).toEqual([first.kid, second.kid]);
    } finally {
      probing = false;
      await prober;
      await identityForward.close();
      await assetsForward.close();
    }
  }, 1_800_000);

  test("a run of the CronJob, not a restart, retires the old key once its last token expired", async () => {
    const second = signingKeys().keys.find((k) => k.kid === kid.second) as StoredKey;
    const identity = onePod("identity");
    const assets = onePod("assets");
    const retireAt = ((second.signs_from ?? 0) + PROPAGATION_S + LAST_TOKEN_S) * 1000;
    const retired = await waitFor(
      "the CronJob to retire the old key",
      () => {
        const keys = signingKeys();
        return keys.keys.length === 1 && keys.keys[0]?.kid === kid.second ? Date.now() : undefined;
      },
      retireAt - Date.now() + 300_000,
      3_000,
    );
    expect(retired).toBeGreaterThanOrEqual(retireAt);
    expect(publicKids()).toEqual([kid.second as string]);
    const run = await waitFor(
      "the log of the run that retired the old key",
      () => collectRuns().find((r) => r.changes.some((change) => change.startsWith(`retired signing key ${kid.first}`))),
      120_000,
      3_000,
    );
    const dropped = await waitFor(
      "the example service to drop the retired key",
      () => publicKeysLoadedAt(assets.metadata.name, "assets", [kid.second as string], retireAt),
      600_000,
      5_000,
    );
    // The same pods: nothing restarted to retire it.
    expect(onePod("identity").metadata.name).toBe(identity.metadata.name);
    expect(onePod("assets").metadata.name).toBe(assets.metadata.name);
    const trip = await roundTrip(session);
    report.retirement = {
      job: run.job,
      retiredAfterRetireAtSeconds: seconds(retired - retireAt),
      assetsDroppedAfterRetireAtSeconds: seconds(dropped - retireAt),
      token: trip,
    };
    console.log(`retirement: ${JSON.stringify(report.retirement)}`);
    expect(trip).toEqual({ kid: kid.second as string, status: 200, userMatches: true });
  }, 1_800_000);

  test("a manual rotation works while automatic rotation is on", async () => {
    const started = Date.now();
    upgrade("--set", "internalIdentity.keyGeneration=2");
    // Settled: every key published (a run of the CronJob may have raced the upgrade; either way
    // exactly one key of generation 2 comes out, signing after the propagation time).
    const keys = await waitFor(
      "a published key of generation 2",
      () => {
        const now = signingKeys();
        return now.keys.some((k) => k.generation === 2) && now.keys.every((k) => k.published_at !== undefined) ? now : undefined;
      },
      300_000,
      3_000,
    );
    remember(keys);
    const manual = keys.keys.filter((k) => k.generation === 2);
    expect(manual.length).toBe(1);
    const third = manual[0] as StoredKey;
    kid.manual = third.kid;
    expect((third.signs_from ?? 0) - (third.published_at ?? 0)).toBe(PROPAGATION_S);
    expect(keys.keys.map((k) => k.kid)).toContain(kid.second as string);
    expect(publicKids().sort()).toEqual(kidsOfKeys(keys.keys).sort());
    const signsFrom = (third.signs_from ?? 0) * 1000;
    const identity = onePod("identity");
    await waitFor("the identity service to sign with the generation 2 key", () => signingWithAt(identity.metadata.name, third.kid), signsFrom - Date.now() + 300_000, 3_000);
    const trip = await roundTrip(session);
    report.manualRotation = {
      keys: keys.keys.map(({ kid: id, generation, published_at, signs_from }) => ({ kid: id, generation, published_at, signs_from })),
      upgradeToPublishedSeconds: seconds((third.published_at ?? 0) * 1000 - started),
      token: trip,
    };
    console.log(`manual rotation: ${JSON.stringify(report.manualRotation)}`);
    expect(trip).toEqual({ kid: third.kid, status: 200, userMatches: true });
  }, 1_800_000);

  test("a revocation removes every older key, the automatic ones too, and the new key signs at once", async () => {
    const assets = onePod("assets");
    const started = Date.now();
    const revoked = kidsOfKeys(signingKeys().keys);
    upgrade("--set", "internalIdentity.keyGeneration=3", "--set", "internalIdentity.revokeBeforeGeneration=3");
    const keys = signingKeys();
    remember(keys);
    expect(keys.keys.map((k) => k.generation)).toEqual([3]);
    const fourth = keys.keys[0] as StoredKey;
    kid.revocation = fourth.kid;
    expect(fourth.signs_from).toBe(fourth.published_at);
    expect(revoked).toContain(kid.second as string);
    expect(revoked).not.toContain(fourth.kid);
    expect(publicKids()).toEqual([fourth.kid]);
    const dropped = await waitFor(
      "the example service to drop the revoked keys",
      () => publicKeysLoadedAt(assets.metadata.name, "assets", [fourth.kid], started),
      600_000,
      5_000,
    );
    const trip = await roundTrip(session);
    report.revocation = {
      revoked,
      kid: fourth.kid,
      upgradeToPublishedSeconds: seconds((fourth.published_at ?? 0) * 1000 - started),
      assetsDroppedAfterPublishedSeconds: seconds(dropped - (fourth.published_at ?? 0) * 1000),
      token: trip,
    };
    console.log(`revocation: ${JSON.stringify(report.revocation)}`);
    expect(trip).toEqual({ kid: fourth.kid, status: 200, userMatches: true });
  }, 1_800_000);

  test("maxKeyAgeSeconds 0 removes the CronJob, and a key past the age is not rotated, not even by a restart", async () => {
    upgrade("--set", "internalIdentity.keyGeneration=3", "--set", "internalIdentity.revokeBeforeGeneration=3", "--set", "internalIdentity.maxKeyAgeSeconds=0");
    expect(exec([...kubectlArgs(), "get", "cronjob", ROTATION, "--output", "name"]).code).not.toBe(0);
    const identityEnv = json<{ spec: { template: { spec: { containers: { name: string; env?: { name: string; value?: string }[] }[] } } } }>(
      "get",
      "deployment",
      IDENTITY,
    ).spec.template.spec.containers.find((c) => c.name === "identity")?.env;
    expect(identityEnv?.find((e) => e.name === "TRUSTIOT_IDENTITY_MAX_KEY_AGE_SECONDS")?.value).toBe("0");
    const before = signingKeys().keys;
    expect(before.map((k) => k.kid)).toEqual([kid.revocation as string]);
    const key = before[0] as StoredKey;
    // Past the age and past a run that would have rotated it.
    const pastDue = ((key.published_at ?? 0) + MAX_AGE_S + 90) * 1000;
    const waited = Math.max(pastDue - Date.now(), 0);
    await Bun.sleep(waited);
    expect(signingKeys().keys).toEqual(before);
    // The init container does not rotate by age either.
    kubectl("rollout", "restart", `deployment/${IDENTITY}`);
    rolledOut();
    expect(signingKeys().keys).toEqual(before);
    expect(publicKids()).toEqual([key.kid]);
    const trip = await roundTrip(session);
    report.disabled = { waitedSeconds: seconds(waited), keyAgeSeconds: seconds(Date.now() - (key.published_at ?? 0) * 1000), keys: before.map((k) => k.kid), token: trip };
    console.log(`disabled: ${JSON.stringify(report.disabled)}`);
    expect(trip).toEqual({ kid: key.kid, status: 200, userMatches: true });
  }, 1_800_000);

  test("no private key is in a log, the ConfigMap or an event", () => {
    expect(privates.size).toBeGreaterThanOrEqual(4);
    const texts: Record<string, string> = {
      configMap: kubectl("get", "configmap", PUBLIC_KEYS, "--output", "json"),
      events: kubectl("get", "events", "--output", "json"),
    };
    for (const component of ["identity", "assets"]) {
      for (const pod of podsOf(component)) {
        texts[`${pod.metadata.name}/${component}`] = kubectl("logs", pod.metadata.name, "--container", component);
        if (component === "identity") texts[`${pod.metadata.name}/${INIT_CONTAINER}`] = kubectl("logs", pod.metadata.name, "--container", INIT_CONTAINER);
      }
    }
    for (const run of runs.values()) texts[`job/${run.job}`] = run.text;
    const leaked = Object.entries(texts)
      .filter(([, text]) => leaks(text))
      .map(([where]) => where);
    report.rotationRuns = [...runs.values()].map(({ job, startedAt, finishedAt, succeeded, serviceAccount, changes, inPlace }) => ({
      job,
      startedAt,
      finishedAt,
      succeeded,
      serviceAccount,
      changes,
      keys: inPlace?.keys,
      signing: inPlace?.signing,
      retireAt: inPlace?.retire_at,
      rotationDueAt: inPlace?.rotation_due_at,
    }));
    report.logsChecked = Object.keys(texts).length;
    report.privateKeysFound = leaked;
    expect(runs.size).toBeGreaterThan(0);
    expect([...runs.values()].every((r) => r.succeeded)).toBe(true);
    expect(leaked).toEqual([]);
  });
});
