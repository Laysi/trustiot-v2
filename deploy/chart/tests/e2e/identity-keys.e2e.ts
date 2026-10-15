// Internal identity keys e2e test (Laysi/trustiot-v2#404; docs/architecture/secrets.md,
// 「內部身分簽章金鑰」): installs the chart with the bundled Postgres, the migration Job, the identity
// service built from the commit and crates/service-kit's example service in the place of assets
// (GET /v1/whoami verifies an internal identity token addressed to assets), and nothing that makes a
// key, with internalIdentity.keyPropagationSeconds at the schema's minimum of 120 s. Then checks
//   1. fresh install: the identity pod's init container generated one key into the Secret the chart
//      declares (bound to it, nothing but signing-keys.json) and published its public key in the
//      ConfigMap; the identity service signs with it and the example service accepts its tokens.
//      Measures how long after the publication each pod loaded the keys and became ready;
//   2. only the identity service's ServiceAccount may get and patch the Secret and the ConfigMap
//      (kubectl auth can-i for every ServiceAccount of the namespace);
//   3. rotation (keyGeneration 2): the new public key is published while the old key signs, every
//      pod loads it without a restart (measured from the pods' logs), and the identity service
//      switches to the new key at its signs_from, which the example service accepts;
//   4. a rollback to generation 1, and the upgrade back, keep the keys;
//   5. retirement: the first start of the identity service after the old key's last token expired
//      removes the old key, and every pod drops it;
//   6. revocation (revokeBeforeGeneration 3): every older key is removed at once and the new key
//      signs at once; measures how long until the example service dropped the revoked key;
//   7. no private key is in a log, the ConfigMap or an event; helm uninstall keeps both objects.
// The test reads the Secret into this process only to know each key's generation and times and to
// check that no private key appears anywhere else; it prints and records key ids, generations and
// times, never a key, and asserts on a private key only as a yes or no.
// README.md in this directory lists the prerequisites; run from the repository root:
//
//   E2E_NAMESPACE=e2e-404 E2E_POSTGRES_IMAGE=<image> E2E_IDENTITY_IMAGE=<image> \
//   E2E_SERVICE_IMAGE=<image> E2E_MIGRATE_IMAGE=<image> \
//     bun test --timeout 3600000 ./deploy/chart/tests/e2e/identity-keys.e2e.ts
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
const SIGNING_KEYS = `${PREFIX}-identity-signing-keys`;
const PUBLIC_KEYS = `${PREFIX}-internal-identity-keys`;
const SIGNING_KEYS_KEY = "signing-keys.json";
const PUBLIC_KEYS_KEY = "jwks.json";
const INIT_CONTAINER = "signing-keys";
const BOUND_UID = "trustiot.io/password-secret-uid";
// The Secrets the chart declares empty for this install and keeps on uninstall: the identity
// service's password (the Job), the ClickHouse admin password and the signing keys (init containers).
const KEPT_SECRETS = [`${PREFIX}-identity-password`, `${PREFIX}-clickhouse-admin-password`, SIGNING_KEYS];
const VALUES = "tests/e2e/values-identity-keys-e2e.yaml";
// values-identity-keys-e2e.yaml: internalIdentity.keyPropagationSeconds.
const PROPAGATION_S = 120;
// crates/service-kit: the longest token lifetime a service accepts plus the clock leeway.
const LAST_TOKEN_S = 300 + 5;
// The identity service's ext_authz endpoint and the cookie (services/identity).
const EXT_AUTHZ = "/internal/ext-authz";
const SESSION_COOKIE = "__Host-trustiot-session";
const IDENTITY_HEADER = "x-internal-identity";
// The service port of both services (values.yaml serviceDefaults.ports).
const HTTP_PORT = 8080;
// The log messages the test reads (crates/service-kit/src/identity/published.rs,
// services/identity/src/issuer.rs).
const PUBLIC_KEYS_LOADED = "verifying internal identity tokens with these public keys of the identity service";
const SIGNING_KEYS_LOADED = "loaded the internal identity signing keys";
const SIGNING_WITH = "signing internal identity tokens";

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
  throw new Error(`timed out after ${timeoutMs / 1000} s waiting for ${what}${last ? `; last error: ${String(last)}` : ""}`);
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
];
// The app host the routes of the overlay name (the instance values' hosts.app).
const APP_HOST = (Bun.YAML.parse(readFileSync(join(CHART_DIR, `tests/values/${SHAPE}-instance.yaml`), "utf8")) as { hosts: { app: string } }).hosts.app;

const RUN = crypto.randomUUID().replaceAll("-", "").slice(0, 8);
const created = { release: false, kept: false };
const report: Record<string, unknown> = { run: RUN, shape: SHAPE, namespace: NAMESPACE, release: RELEASE, propagationSeconds: PROPAGATION_S };
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
/**
 * The Secret's keys without their private parts, and the private parts on their own: kept in this
 * process to check that they appear nowhere else, never printed.
 */
function signingKeys(): { keys: StoredKey[]; privates: string[] } {
  const secret = json<{ data?: Record<string, string> }>("get", "secret", SIGNING_KEYS);
  const text = Buffer.from(secret.data?.[SIGNING_KEYS_KEY] ?? "", "base64").toString();
  const parsed = JSON.parse(text) as { keys: (StoredKey & { d?: string })[] };
  return {
    keys: parsed.keys.map(({ kid, generation, published_at, signs_from }) => ({ kid, generation, published_at, signs_from })),
    privates: parsed.keys.map((key) => key.d ?? "").filter((d) => d !== ""),
  };
}
/** The Secret's uid, the uid it is bound to and its key names, without its data. */
function secretShape(): { bound: boolean; keys: string[]; keep?: string; filledBy?: string } {
  const template = `{{.metadata.uid}}|{{with .metadata.annotations}}{{index . "${BOUND_UID}"}}{{end}}|{{range $key, $value := .data}}{{$key}},{{end}}`;
  const [uid, bound, keys] = kubectl("get", "secret", SIGNING_KEYS, "--output", `go-template=${template}`).split("|");
  const metadata = json<{ metadata: { annotations?: Record<string, string>; labels?: Record<string, string> } }>("get", "secret", SIGNING_KEYS).metadata;
  return {
    bound: uid !== undefined && uid !== "" && uid === bound,
    keys: (keys ?? "").split(",").filter((key) => key !== ""),
    keep: metadata.annotations?.["helm.sh/resource-policy"],
    filledBy: metadata.labels?.["trustiot.io/filled-by"],
  };
}
/** The ConfigMap's JWK Set: its text and its key ids. */
function publicKeys(): { text: string; kids: string[]; members: string[][]; dataKeys: string[] } {
  const configMap = json<{ data?: Record<string, string> }>("get", "configmap", PUBLIC_KEYS);
  const text = configMap.data?.[PUBLIC_KEYS_KEY] ?? "";
  const set = JSON.parse(text) as { keys: Record<string, string>[] };
  return {
    text,
    kids: set.keys.map((key) => key.kid ?? ""),
    members: set.keys.map((key) => Object.keys(key).sort()),
    dataKeys: Object.keys(configMap.data ?? {}),
  };
}

interface Pod {
  metadata: { name: string; labels?: Record<string, string> };
  status: {
    initContainerStatuses?: { name: string; state?: { terminated?: { finishedAt?: string; exitCode?: number } } }[];
    containerStatuses?: { name: string; restartCount: number; state?: { running?: { startedAt?: string } } }[];
    conditions?: { type: string; status: string; lastTransitionTime?: string }[];
  };
}
/** The running pods of a component, without the ones being deleted. */
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
const time = (value: string | undefined) => (value === undefined ? undefined : Date.parse(value));
const readyAt = (pod: Pod) => time(pod.status.conditions?.find((c) => c.type === "Ready" && c.status === "True")?.lastTransitionTime);
const startedAt = (pod: Pod, container: string) => time(pod.status.containerStatuses?.find((c) => c.name === container)?.state?.running?.startedAt);
const initFinishedAt = (pod: Pod) => time(pod.status.initContainerStatuses?.find((c) => c.name === INIT_CONTAINER)?.state?.terminated?.finishedAt);
const seconds = (ms: number | undefined) => (ms === undefined ? undefined : Math.round(ms / 100) / 10);

interface LogLine {
  at: number;
  message: string;
  fields: Record<string, unknown>;
}
/** The JSON lines of a container's log (crates/service-kit/src/telemetry/log_layer.rs). */
function logOf(pod: string, container: string): LogLine[] {
  return kubectl("logs", pod, "--container", container)
    .split("\n")
    .flatMap((line) => {
      try {
        const parsed = JSON.parse(line) as { timestamp?: string; message?: string; fields?: Record<string, unknown> };
        const at = time(parsed.timestamp);
        return at === undefined || parsed.message === undefined ? [] : [{ at, message: parsed.message, fields: parsed.fields ?? {} }];
      } catch {
        return [];
      }
    });
}
/** The key ids a `keys` field lists (a Debug list of strings). */
const kidsOf = (line: LogLine) => [...String(line.fields.keys ?? "").matchAll(/"([A-Za-z0-9_-]+)"/g)].map((m) => m[1] as string).sort();
const sameKids = (a: string[], b: string[]) => JSON.stringify([...a].sort()) === JSON.stringify([...b].sort());

/** When a container first logged that it verifies with exactly `kids`, at or after `since`. */
function publicKeysLoadedAt(pod: string, container: string, kids: string[], since = 0): number | undefined {
  return logOf(pod, container).find((line) => line.message === PUBLIC_KEYS_LOADED && line.at >= since && sameKids(kidsOf(line), kids))?.at;
}
function signingKeysLoadedAt(pod: string, kids: string[], since = 0): number | undefined {
  return logOf(pod, "identity").find((line) => line.message === SIGNING_KEYS_LOADED && line.at >= since && sameKids(kidsOf(line), kids))?.at;
}
function signingWithAt(pod: string, kid: string, since = 0): number | undefined {
  return logOf(pod, "identity").find((line) => line.message === SIGNING_WITH && line.at >= since && line.fields.kid === kid)?.at;
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
// A platform administrator with a session, as the identity service's own tests and the nightly's
// API check make them (scripts/nightly/api.ts): SQL in the primary as the postgres role over the
// local socket, on stdin. Only the session secret's hash reaches the database.
function addSession(session: Session) {
  kubectlIn(
    [
      "BEGIN;",
      `INSERT INTO identity.users (id, platform_admin) VALUES ('${session.userId}', true);`,
      "INSERT INTO identity.sessions (id, token_hash, user_id, expires_at)",
      `  VALUES ('${session.sessionId}', '\\x${session.tokenHash}', '${session.userId}', now() + interval '2 hours');`,
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

// kubectl port-forward to one Service's port for the duration of `use`: a new one each time, since
// a forward stays with the pod it picked, which a rollout replaces.
async function forwarded<T>(service: string, use: (port: number) => Promise<T>): Promise<T> {
  const port = await freePort();
  const forward = Bun.spawn([...kubectlArgs(), "port-forward", `service/${service}`, `${port}:${HTTP_PORT}`], { stdout: "ignore", stderr: "ignore" });
  forwards.push(forward);
  try {
    await waitFor(`port-forward to ${service}`, async () => {
      try {
        const socket = await Bun.connect({ hostname: "127.0.0.1", port, socket: { data() {} } });
        socket.end();
        return true;
      } catch {
        return undefined;
      }
    }, 60_000, 250);
    return await use(port);
  } finally {
    forward.kill();
    await forward.exited;
  }
}

/** A token of the session's user for the assets route, from the identity service's ext_authz check; never printed. */
async function token(session: Session): Promise<string> {
  return forwarded(IDENTITY, async (port) => {
    const answer = await sendRaw(
      { host: "127.0.0.1", port, tls: false },
      { target: `${EXT_AUTHZ}/api/assets/v1/whoami`, hostHeader: APP_HOST, headers: [["cookie", `${SESSION_COOKIE}=${session.cookie}`]] },
    );
    const tokens = answer.headers[IDENTITY_HEADER] ?? [];
    if (answer.status !== 200 || tokens.length !== 1) throw new Error(`the ext_authz check answered ${answer.status} with ${tokens.length} identity header(s)`);
    return tokens[0] as string;
  });
}
/** The key id in a token's header (the header is not secret). */
function kidOf(token: string): string {
  const header = JSON.parse(Buffer.from(token.split(".")[0] ?? "", "base64url").toString()) as { kid?: string };
  return header.kid ?? "";
}
/** What the example service answers to GET /v1/whoami with the token: its status and the user it saw. */
async function verify(token: string): Promise<{ status: number; user?: string }> {
  return forwarded(ASSETS, async (port) => {
    const answer = await fetch(`http://127.0.0.1:${port}/v1/whoami`, { headers: { [IDENTITY_HEADER]: token } });
    const body = (await answer.json().catch(() => ({}))) as { user_id?: string; user?: string };
    return { status: answer.status, user: body.user_id ?? body.user };
  });
}
/** A token from the identity service and the example service's answer to it: key id and status only. */
async function roundTrip(session: Session): Promise<{ kid: string; status: number; userMatches: boolean }> {
  const issued = await token(session);
  const answer = await verify(issued);
  return { kid: kidOf(issued), status: answer.status, userMatches: answer.user === session.userId };
}

// ---------------------------------------------------------------------------------------------

/** Every private key the Secret ever held in this run: none may appear outside the Secret. */
const privates = new Set<string>();
function remember(keys: { privates: string[] }) {
  for (const d of keys.privates) privates.add(d);
}
/** Whether any private key of the run appears in `text`; a yes or no, never the key. */
const leaks = (text: string) => [...privates].some((d) => text.includes(d));

const session = newSession();

beforeAll(async () => {
  // Never touch an existing installation or objects someone kept (the same checks as postgres.e2e.ts).
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

  // One install, as a real one, and nothing that makes a key. From here on the kept objects are
  // this run's (checked above).
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
    writeFileSync(join(REPORT_DIR, `identity-keys-e2e-${SHAPE}.json`), `${JSON.stringify(report, null, 2)}\n`);
  }
  if (KEEP) return;
  const errors: string[] = [];
  const steps: [boolean, () => unknown][] = [
    [created.release, () => helm("uninstall", RELEASE, "--wait", "--timeout", "5m")],
    // Kept on uninstall (helm.sh/resource-policy: keep).
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

// The key ids of each generation, as the steps find them.
const kid: Record<number, string> = {};

describe(`internal identity keys (${SHAPE} shape)`, () => {
  test("a fresh install generated one key, published its public key, and every service took it", async () => {
    const keys = signingKeys();
    remember(keys);
    expect(keys.keys.length).toBe(1);
    const [first] = keys.keys as [StoredKey];
    expect(first.generation).toBe(1);
    // A fresh install has no old key to wait for: the first key signs at once.
    expect(first.signs_from).toBe(first.published_at);
    kid[1] = first.kid;
    expect(secretShape()).toEqual({ bound: true, keys: [SIGNING_KEYS_KEY], keep: "keep", filledBy: "migrate" });
    const published = publicKeys();
    expect(published.dataKeys).toEqual([PUBLIC_KEYS_KEY]);
    expect(published.kids).toEqual([first.kid]);
    expect(published.members).toEqual([["alg", "crv", "kid", "kty", "use", "x"]]);
    // RFC 7638 thumbprint: SHA-256, base64url, 43 characters.
    expect(first.kid).toMatch(/^[A-Za-z0-9_-]{43}$/);

    const publishedAt = (first.published_at ?? 0) * 1000;
    const identity = onePod("identity");
    const assets = onePod("assets");
    const identityLoaded = publicKeysLoadedAt(identity.metadata.name, "identity", [first.kid]);
    const assetsLoaded = publicKeysLoadedAt(assets.metadata.name, "assets", [first.kid]);
    const signing = signingWithAt(identity.metadata.name, first.kid);
    expect(identityLoaded).toBeDefined();
    expect(assetsLoaded).toBeDefined();
    expect(signing).toBeDefined();
    report.freshInstall = {
      kid: first.kid,
      generation: first.generation,
      initFinishedAfterPublishedSeconds: seconds((initFinishedAt(identity) ?? 0) - publishedAt),
      identity: {
        containerStartedAfterPublishedSeconds: seconds((startedAt(identity, "identity") ?? 0) - publishedAt),
        signingKeysLoadedAfterPublishedSeconds: seconds((signingKeysLoadedAt(identity.metadata.name, [first.kid]) ?? 0) - publishedAt),
        publicKeysLoadedAfterPublishedSeconds: seconds((identityLoaded ?? 0) - publishedAt),
        signingAfterPublishedSeconds: seconds((signing ?? 0) - publishedAt),
        readyAfterPublishedSeconds: seconds((readyAt(identity) ?? 0) - publishedAt),
      },
      assets: {
        containerStartedAfterPublishedSeconds: seconds((startedAt(assets, "assets") ?? 0) - publishedAt),
        publicKeysLoadedAfterPublishedSeconds: seconds((assetsLoaded ?? 0) - publishedAt),
        readyAfterPublishedSeconds: seconds((readyAt(assets) ?? 0) - publishedAt),
        readyAfterContainerStartedSeconds: seconds((readyAt(assets) ?? 0) - (startedAt(assets, "assets") ?? 0)),
      },
    };
    console.log(`fresh install: ${JSON.stringify(report.freshInstall)}`);

    const trip = await roundTrip(session);
    report.freshInstallToken = trip;
    expect(trip).toEqual({ kid: first.kid, status: 200, userMatches: true });
  }, 300_000);

  test("only the identity service's ServiceAccount may get and patch the Secret and the ConfigMap", () => {
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
    expect(accounts).toContain(IDENTITY);
    expect(allowed.sort()).toEqual(
      [`${IDENTITY} get secrets/${SIGNING_KEYS}`, `${IDENTITY} patch secrets/${SIGNING_KEYS}`, `${IDENTITY} get configmaps/${PUBLIC_KEYS}`, `${IDENTITY} patch configmaps/${PUBLIC_KEYS}`].sort(),
    );
  });

  test("a rotation publishes the new key first, every pod loads it without a restart, and the identity service signs with it from signs_from", async () => {
    const assetsBefore = onePod("assets");
    const restartsBefore = assetsBefore.status.containerStatuses?.find((c) => c.name === "assets")?.restartCount;
    const upgradeStarted = Date.now();
    upgrade("--set", "internalIdentity.keyGeneration=2");
    const keys = signingKeys();
    remember(keys);
    expect(keys.keys.map((k) => k.generation).sort()).toEqual([1, 2]);
    const second = keys.keys.find((k) => k.generation === 2) as StoredKey;
    kid[2] = second.kid;
    // The old key stays as it was; the new one signs keyPropagationSeconds after it was published.
    expect(keys.keys.find((k) => k.generation === 1)?.kid).toBe(kid[1]);
    expect((second.signs_from ?? 0) - (second.published_at ?? 0)).toBe(PROPAGATION_S);
    expect(publicKeys().kids.sort()).toEqual([kid[1] as string, kid[2] as string].sort());
    const publishedAt = (second.published_at ?? 0) * 1000;
    const signsFrom = (second.signs_from ?? 0) * 1000;

    // Before signs_from the identity service still signs with the old key, which every pod accepts.
    const early = await roundTrip(session);
    const earlyAt = Date.now();
    report.rotationEarlyToken = { ...early, secondsBeforeSignsFrom: seconds(signsFrom - earlyAt) };
    if (earlyAt < signsFrom - 5_000) expect(early).toEqual({ kid: kid[1] as string, status: 200, userMatches: true });

    // Every pod loads the new public key; the example service's pod is the same, not restarted.
    const identity = onePod("identity");
    const both = [kid[1] as string, kid[2] as string];
    const assetsLoaded = await waitFor("the example service to load both public keys", () => publicKeysLoadedAt(assetsBefore.metadata.name, "assets", both), 600_000, 5_000);
    const identityLoaded = await waitFor("the identity service to load both public keys", () => publicKeysLoadedAt(identity.metadata.name, "identity", both), 600_000, 5_000);
    const secretLoaded = await waitFor("the identity service to load both signing keys", () => signingKeysLoadedAt(identity.metadata.name, both), 600_000, 5_000);
    // It switches at signs_from (it rereads its files every 5 s).
    const switched = await waitFor("the identity service to sign with the new key", () => signingWithAt(identity.metadata.name, kid[2] as string), signsFrom - Date.now() + 300_000, 5_000);
    const assetsAfter = onePod("assets");
    expect(assetsAfter.metadata.name).toBe(assetsBefore.metadata.name);
    expect(assetsAfter.status.containerStatuses?.find((c) => c.name === "assets")?.restartCount).toBe(restartsBefore);
    expect(switched).toBeGreaterThanOrEqual(signsFrom);

    const late = await roundTrip(session);
    report.rotation = {
      kids: { 1: kid[1], 2: kid[2] },
      upgradeToPublishedSeconds: seconds(publishedAt - upgradeStarted),
      initFinishedAfterPublishedSeconds: seconds((initFinishedAt(identity) ?? 0) - publishedAt),
      assetsLoadedAfterPublishedSeconds: seconds(assetsLoaded - publishedAt),
      identityPublicKeysLoadedAfterPublishedSeconds: seconds(identityLoaded - publishedAt),
      identitySigningKeysLoadedAfterPublishedSeconds: seconds(secretLoaded - publishedAt),
      switchedAfterSignsFromSeconds: seconds(switched - signsFrom),
      lateToken: late,
    };
    console.log(`rotation: ${JSON.stringify(report.rotation)}`);
    expect(late).toEqual({ kid: kid[2] as string, status: 200, userMatches: true });
  }, 1_800_000);

  test("a rollback to generation 1 and the upgrade back keep the keys", async () => {
    const before = JSON.stringify(signingKeys().keys);
    const published = publicKeys().text;
    const revisions = JSON.parse(helm("history", RELEASE, "--output", "json")) as { revision: number }[];
    helm("rollback", RELEASE, String(Math.min(...revisions.map((r) => r.revision))), "--wait=legacy", "--timeout", "15m");
    rolledOut();
    const afterRollback = JSON.stringify(signingKeys().keys) === before && publicKeys().text === published;
    const rollbackToken = await roundTrip(session);
    upgrade("--set", "internalIdentity.keyGeneration=2");
    const afterUpgrade = JSON.stringify(signingKeys().keys) === before && publicKeys().text === published;
    report.rollback = { keysUnchangedAfterRollback: afterRollback, keysUnchangedAfterUpgradeBack: afterUpgrade, rollbackToken };
    expect(afterRollback).toBe(true);
    expect(afterUpgrade).toBe(true);
    expect(rollbackToken).toEqual({ kid: kid[2] as string, status: 200, userMatches: true });
  }, 1_800_000);

  test("the first identity start after the old key's last token expired retires it, and every pod drops it", async () => {
    const second = signingKeys().keys.find((k) => k.generation === 2) as StoredKey;
    // tools/migrate removes older keys once signs_from + keyPropagationSeconds + the last token's
    // lifetime has passed.
    const retireAt = ((second.signs_from ?? 0) + PROPAGATION_S + LAST_TOKEN_S) * 1000;
    const wait = retireAt - Date.now() + 5_000;
    report.retirementWaitSeconds = seconds(Math.max(wait, 0));
    if (wait > 0) await Bun.sleep(wait);
    const assets = onePod("assets");
    const restarted = Date.now();
    kubectl("rollout", "restart", `deployment/${IDENTITY}`);
    rolledOut();
    const keys = signingKeys();
    expect(keys.keys.map((k) => k.kid)).toEqual([kid[2] as string]);
    expect(publicKeys().kids).toEqual([kid[2] as string]);
    const identity = onePod("identity");
    const removedAt = initFinishedAt(identity) ?? restarted;
    const assetsDropped = await waitFor("the example service to drop the retired key", () => publicKeysLoadedAt(assets.metadata.name, "assets", [kid[2] as string], restarted), 600_000, 5_000);
    const trip = await roundTrip(session);
    report.retirement = { assetsDroppedAfterInitSeconds: seconds(assetsDropped - removedAt), token: trip };
    console.log(`retirement: ${JSON.stringify(report.retirement)}`);
    expect(trip).toEqual({ kid: kid[2] as string, status: 200, userMatches: true });
  }, 1_800_000);

  test("a revocation removes every older key and lets the new key sign at once", async () => {
    const assets = onePod("assets");
    const upgradeStarted = Date.now();
    upgrade("--set", "internalIdentity.keyGeneration=3", "--set", "internalIdentity.revokeBeforeGeneration=3");
    const keys = signingKeys();
    remember(keys);
    expect(keys.keys.map((k) => k.generation)).toEqual([3]);
    const third = keys.keys[0] as StoredKey;
    kid[3] = third.kid;
    // No key is left to bridge a wait: the new key signs at once.
    expect(third.signs_from).toBe(third.published_at);
    expect(publicKeys().kids).toEqual([third.kid]);
    const publishedAt = (third.published_at ?? 0) * 1000;
    const identity = onePod("identity");
    const assetsDropped = await waitFor("the example service to drop the revoked key", () => publicKeysLoadedAt(assets.metadata.name, "assets", [third.kid], upgradeStarted), 600_000, 5_000);
    const trip = await roundTrip(session);
    report.revocation = {
      kid: third.kid,
      upgradeToPublishedSeconds: seconds(publishedAt - upgradeStarted),
      // The new identity pod's file held only revoked keys until the kubelet updated it.
      identityReadyAfterPublishedSeconds: seconds((readyAt(identity) ?? 0) - publishedAt),
      identitySigningAfterPublishedSeconds: seconds((signingWithAt(identity.metadata.name, third.kid) ?? 0) - publishedAt),
      assetsDroppedAfterPublishedSeconds: seconds(assetsDropped - publishedAt),
      token: trip,
    };
    console.log(`revocation: ${JSON.stringify(report.revocation)}`);
    expect(trip).toEqual({ kid: third.kid, status: 200, userMatches: true });
  }, 1_800_000);

  test("no private key is in a log, the ConfigMap or an event", () => {
    expect(privates.size).toBe(3);
    const texts: Record<string, string> = { configMap: publicKeys().text, events: kubectl("get", "events", "--output", "json") };
    for (const component of ["identity", "assets"]) {
      for (const pod of podsOf(component)) {
        texts[`${pod.metadata.name}/${component}`] = kubectl("logs", pod.metadata.name, "--container", component);
        if (component === "identity") texts[`${pod.metadata.name}/${INIT_CONTAINER}`] = kubectl("logs", pod.metadata.name, "--container", INIT_CONTAINER);
      }
    }
    const leaked = Object.entries(texts)
      .filter(([, text]) => leaks(text))
      .map(([where]) => where);
    report.privateKeysFound = leaked;
    expect(leaked).toEqual([]);
    // The init container says what it did with key ids and generations.
    const init = texts[`${onePod("identity").metadata.name}/${INIT_CONTAINER}`] ?? "";
    report.initLog = init.split("\n").filter((line) => line.trim() !== "");
  });

  test("helm uninstall keeps the Secret and the ConfigMap", () => {
    helm("uninstall", RELEASE, "--wait", "--timeout", "5m");
    created.release = false;
    const kept = kubectl("get", `secret/${SIGNING_KEYS}`, `configmap/${PUBLIC_KEYS}`, "--output", "name").trim().split("\n").sort();
    report.keptAfterUninstall = kept;
    expect(kept).toEqual([`configmap/${PUBLIC_KEYS}`, `secret/${SIGNING_KEYS}`]);
    expect(signingKeys().keys.map((k) => k.kid)).toEqual([kid[3] as string]);
  }, 600_000);
});
