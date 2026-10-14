// Bundled ClickHouse admin password e2e test (Laysi/trustiot-v2#403): installs the chart (on-prem
// shape) with the bundled ClickHouse only, into an existing namespace, the way an installation from
// before Laysi/trustiot-v2#403 runs, then moves it over and rotates the password:
//   1. with clickhouse.bundled.adminSecret naming a Secret the test created (the old way),
//      ClickHouse accepts that Secret's password, and the chart renders no generated Secret and no
//      init container;
//   2. an upgrade without adminSecret (the move): the pod's init container generates a 256-bit
//      password into the empty Secret the chart declares, bound to it, and ClickHouse restarts
//      with it; the old password is refused, and the old Secret can be deleted; a restart of the
//      pod keeps the password;
//   3. clickhouse.bundled.adminPasswordGeneration=2 (a rotation): the Secret holds a new password
//      of generation 2, ClickHouse accepts it and refuses the old one;
//   4. a rollback to the release before the rotation keeps the new password (generations only go
//      up), and the upgrade after it changes nothing;
//   5. only ClickHouse's ServiceAccount may get and patch the Secret: no other ServiceAccount of the
//      chart, nor the namespace's default one, may read it.
// The migration Job is off (values-otel-e2e.yaml): its part, reading the password from the Secret,
// runs in the nightly smoke test and postgres.e2e.ts, which need Postgres. Passwords are compared
// by digest inside this process and sent to the pod on stdin; the output and the report hold only
// "unchanged" or "changed", never a value or a digest.
// README.md in this directory lists the prerequisites; run from the repository root:
//
//   E2E_NAMESPACE=e2e-403 E2E_MIGRATE_IMAGE=<image> bun test --timeout 900000 ./deploy/chart/tests/e2e/clickhouse-admin.e2e.ts
//
// Not part of `bun test deploy/chart` (the file name has no .test. suffix): it needs a cluster.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { isAbsolute, join } from "node:path";

const CHART_DIR = join(import.meta.dir, "..", "..");
const REPO_ROOT = join(CHART_DIR, "..", "..");

function required(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required (see deploy/chart/tests/e2e/README.md)`);
  return value;
}

const NAMESPACE = required("E2E_NAMESPACE");
const MIGRATE_IMAGE = required("E2E_MIGRATE_IMAGE");
const CONTEXT = process.env.E2E_KUBECONTEXT;
const RELEASE = process.env.E2E_RELEASE ?? "trustiot";
const PULL_POLICY = process.env.E2E_IMAGE_PULL_POLICY ?? "IfNotPresent";
const REPORT_DIR = process.env.E2E_REPORT_DIR;
const KEEP = process.env.E2E_KEEP === "1";

const PREFIX = RELEASE.includes("trustiot") ? RELEASE : `${RELEASE}-trustiot`;
const RUN = crypto.randomUUID().replaceAll("-", "").slice(0, 8);
const STATEFULSET = `${PREFIX}-clickhouse`;
const POD = `${STATEFULSET}-0`;
// The Secret the chart declares and the init container fills, kept by Helm on uninstall.
const ADMIN_SECRET = `${PREFIX}-clickhouse-admin-password`;
const ACCOUNT = "trustiot_admin";
// The Secret of the old way, with a name of its own, so the test never replaces one it did not create.
const OLD_SECRET = `trustiot-e2e-ch-admin-${RUN}`;

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
  if (result.code !== 0) throw new Error(`${cmd.slice(0, 5).join(" ")} failed (${result.code}):\n${result.stderr}`);
  return result.stdout;
}
const kubectlArgs = () => [KUBECTL, ...(CONTEXT ? ["--context", CONTEXT] : []), "--namespace", NAMESPACE];
const kubectl = (...args: string[]) => run([...kubectlArgs(), ...args]);
const helm = (...args: string[]) =>
  run([HELM, ...(CONTEXT ? ["--kube-context", CONTEXT] : []), "--namespace", NAMESPACE, ...args]);
const json = <T>(...args: string[]) => JSON.parse(kubectl(...args, "--output", "json")) as T;

async function waitFor<T>(what: string, check: () => T | undefined | Promise<T | undefined>, timeoutMs = 300_000) {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  while (Date.now() < deadline) {
    try {
      const value = await check();
      if (value !== undefined) return value;
    } catch (error) {
      last = error;
    }
    await Bun.sleep(3_000);
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

// ClickHouse only, like the first step of otel.e2e.ts: no collector, no service, no migration Job.
const base = [
  "--values",
  "values-onprem.yaml",
  "--values",
  "tests/values/onprem-instance.yaml",
  "--values",
  "tests/e2e/values-otel-e2e.yaml",
  "--set",
  "observability.collector.enabled=false",
  "--set",
  "services.assets.enabled=false",
  ...imageValues("migrate.image", MIGRATE_IMAGE, "E2E_MIGRATE_IMAGE"),
];
const OLD_WAY = ["--set", `clickhouse.bundled.adminSecret.name=${OLD_SECRET}`];

const work = mkdtempSync(join(tmpdir(), "trustiot-clickhouse-admin-e2e-"));
const created = { oldSecret: false, release: false };
const report: Record<string, unknown> = { run: RUN, namespace: NAMESPACE, release: RELEASE };

function hex(bytes: number): string {
  return Array.from(crypto.getRandomValues(new Uint8Array(bytes)), (b) => b.toString(16).padStart(2, "0")).join("");
}
function digest(value: string): string {
  return new Bun.CryptoHasher("sha256").update(value).digest("hex");
}
const compared = (before: string, after: string) => (digest(before) === digest(after) ? "unchanged" : "changed");

function install(command: "install" | "upgrade", ...extra: string[]) {
  helm(command, RELEASE, ".", ...base, ...extra, "--wait", "--timeout", "10m");
}

interface Pod {
  metadata: { uid: string };
  spec: { initContainers?: { name: string }[] };
  status: {
    conditions?: { type: string; status: string }[];
    initContainerStatuses?: { name: string; state?: { terminated?: { exitCode: number } } }[];
  };
}
const pod = () => json<Pod>("get", "pod", POD);
const ready = (p: Pod) => (p.status.conditions ?? []).some((c) => c.type === "Ready" && c.status === "True");

// Waits until the StatefulSet runs its current template and the pod is Ready; returns the pod.
async function rolledOut(): Promise<Pod> {
  kubectl("rollout", "status", `statefulset/${STATEFULSET}`, "--timeout=600s");
  return waitFor(`pod ${POD} Ready`, () => {
    const current = pod();
    return ready(current) ? current : undefined;
  });
}

interface StoredSecret {
  metadata: { uid?: string; annotations?: Record<string, string>; labels?: Record<string, string> };
  data?: Record<string, string>;
}
// What the generated Secret holds, by name only, and its password, which stays in this process.
function adminSecret() {
  const found = json<StoredSecret>("get", "secret", ADMIN_SECRET);
  const value = found.data?.[ACCOUNT];
  return {
    password: value === undefined ? "" : Buffer.from(value, "base64").toString(),
    shape: {
      keys: Object.keys(found.data ?? {}).sort(),
      generation: found.metadata.annotations?.["trustiot.io/password-generation"],
      bound: found.metadata.uid !== undefined && found.metadata.annotations?.["trustiot.io/password-secret-uid"] === found.metadata.uid,
      keep: found.metadata.annotations?.["helm.sh/resource-policy"],
      filledBy: found.metadata.labels?.["trustiot.io/filled-by"],
      generatedShape: /^[0-9a-f]{64}$/.test(value === undefined ? "" : Buffer.from(value, "base64").toString()),
    },
  };
}

// Whether ClickHouse accepts `password` for the admin account. The password goes to the pod on
// stdin and stays in it; the result holds ClickHouse's error code, never the password.
function accepts(password: string): { accepted: boolean; error?: string } {
  const result = exec(
    [
      ...kubectlArgs(),
      "exec",
      "-i",
      POD,
      "-c",
      "clickhouse",
      "--",
      "bash",
      "-c",
      'IFS= read -r p; exec clickhouse-client --user "$CLICKHOUSE_USER" --password "$p" --query "SELECT currentUser()"',
    ],
    `${password}\n`,
  );
  if (result.code === 0) {
    expect(result.stdout.trim()).toBe(ACCOUNT);
    return { accepted: true };
  }
  const code = /Code: (\d+)/.exec(result.stderr)?.[1];
  expect(result.stderr.includes(password)).toBe(false);
  return { accepted: false, error: code === undefined ? "(no code)" : `Code ${code}` };
}

function initLog(): string {
  return kubectl("logs", POD, "-c", "admin-password");
}

beforeAll(() => {
  // Never touch an existing installation.
  const status = exec([HELM, ...(CONTEXT ? ["--kube-context", CONTEXT] : []), "--namespace", NAMESPACE, "status", RELEASE]);
  if (status.code === 0) throw new Error(`release ${RELEASE} already exists in ${NAMESPACE}; use another E2E_RELEASE or namespace`);
  if (!status.stderr.includes("not found")) throw new Error(`helm status ${RELEASE} failed:\n${status.stderr}`);
  // The generated Secret is kept on uninstall, and the volume too: one left by an earlier run
  // would hand this run its password and data.
  const leftovers = [
    kubectl("get", "secret", ADMIN_SECRET, "--ignore-not-found", "--output", "name"),
    kubectl("get", "pvc", `data-${POD}`, "--ignore-not-found", "--output", "name"),
  ]
    .join("\n")
    .split("\n")
    .filter((line) => line.trim() !== "");
  if (leftovers.length > 0) {
    throw new Error(`${NAMESPACE} already has ${leftovers.join(", ")}, which this run would delete when it cleans up; remove them first`);
  }
  // The old way: a Secret a person made, written to a private file so the value never appears
  // in argv.
  const oldFile = join(work, "old-admin");
  writeFileSync(oldFile, hex(32), { mode: 0o600 });
  created.oldSecret = true;
  kubectl("create", "secret", "generic", OLD_SECRET, `--from-file=password=${oldFile}`);
  created.release = true;
  install("install", ...OLD_WAY);
}, 900_000);

afterAll(() => {
  if (REPORT_DIR) {
    mkdirSync(REPORT_DIR, { recursive: true });
    writeFileSync(join(REPORT_DIR, "clickhouse-admin-e2e.json"), `${JSON.stringify(report, null, 2)}\n`);
  }
  rmSync(work, { recursive: true, force: true });
  if (KEEP) return;
  const errors: string[] = [];
  const steps: [boolean, () => unknown][] = [
    [created.release, () => helm("uninstall", RELEASE, "--wait", "--timeout", "3m")],
    // Kept on uninstall: the volume (persistentVolumeClaimRetentionPolicy) and the Secret
    // (helm.sh/resource-policy: keep).
    [created.release, () => kubectl("delete", "pvc", `data-${POD}`, "--ignore-not-found", "--wait=true", "--timeout=180s")],
    [created.release, () => kubectl("delete", "secret", ADMIN_SECRET, "--ignore-not-found")],
    [created.oldSecret, () => kubectl("delete", "secret", OLD_SECRET, "--ignore-not-found")],
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
}, 600_000);

describe("bundled ClickHouse admin password generated in the cluster (Laysi/trustiot-v2#403)", () => {
  let oldPassword = "";
  let firstPassword = "";
  let rotatedPassword = "";
  let movedPod = "";

  test("an installation that names an existing Secret keeps working the old way", async () => {
    const current = await rolledOut();
    oldPassword = Buffer.from(json<StoredSecret>("get", "secret", OLD_SECRET).data?.password ?? "", "base64").toString();
    const login = accepts(oldPassword);
    const generated = kubectl("get", "secret", ADMIN_SECRET, "--ignore-not-found", "--output", "name").trim();
    report.oldWay = { login, initContainers: (current.spec.initContainers ?? []).map((c) => c.name), generatedSecret: generated !== "" };
    expect(login).toEqual({ accepted: true });
    expect(current.spec.initContainers ?? []).toEqual([]);
    expect(generated).toBe("");
  }, 600_000);

  test("moving over: the init container generates the password into the chart's Secret before ClickHouse starts with it", async () => {
    const before = pod().metadata.uid;
    install("upgrade");
    const moved = await rolledOut();
    movedPod = moved.metadata.uid;
    const { password, shape } = adminSecret();
    firstPassword = password;
    const init = moved.status.initContainerStatuses?.find((c) => c.name === "admin-password");
    const log = initLog();
    const result = {
      restarted: moved.metadata.uid !== before,
      initExitCode: init?.state?.terminated?.exitCode,
      secret: shape,
      generatedLogged: log.includes(`generated the password of ${ACCOUNT} in Secret ${ADMIN_SECRET} (it had none)`),
      passwordInLog: log.includes(password) || log.includes(Buffer.from(password).toString("base64")),
      newPassword: accepts(password),
      oldPassword: accepts(oldPassword),
    };
    report.move = result;
    console.log(`after the move: ${JSON.stringify(result)}`);
    expect(result).toEqual({
      restarted: true,
      initExitCode: 0,
      secret: { keys: [ACCOUNT], generation: "1", bound: true, keep: "keep", filledBy: "migrate", generatedShape: true },
      generatedLogged: true,
      passwordInLog: false,
      newPassword: { accepted: true },
      oldPassword: { accepted: false, error: "Code 516" },
    });
    // The Secret of the old way is no longer used: deleting it, then restarting the pod, changes
    // nothing.
    kubectl("delete", "secret", OLD_SECRET);
    created.oldSecret = false;
    kubectl("delete", "pod", POD, "--wait=true");
    const restarted = await rolledOut();
    const after = adminSecret();
    const again = { restarted: restarted.metadata.uid !== movedPod, secret: compared(firstPassword, after.password), log: initLog().includes("generated the password") ? "generated" : "kept", login: accepts(firstPassword) };
    report.restartAfterMove = again;
    console.log(`after deleting the old Secret and restarting the pod: ${JSON.stringify(again)}`);
    expect(again).toEqual({ restarted: true, secret: "unchanged", log: "kept", login: { accepted: true } });
  }, 900_000);

  test("a rotation: a higher adminPasswordGeneration restarts ClickHouse with a new password, and the old one is refused", async () => {
    install("upgrade", "--set", "clickhouse.bundled.adminPasswordGeneration=2");
    await rolledOut();
    const { password, shape } = adminSecret();
    rotatedPassword = password;
    const result = {
      secret: compared(firstPassword, password),
      generation: shape.generation,
      bound: shape.bound,
      generatedLogged: initLog().includes(`generated the password of ${ACCOUNT} in Secret ${ADMIN_SECRET} (password generation 2)`),
      newPassword: accepts(password),
      oldPassword: accepts(firstPassword),
    };
    report.rotation = result;
    console.log(`after adminPasswordGeneration=2: ${JSON.stringify(result)}`);
    expect(result).toEqual({
      secret: "changed",
      generation: "2",
      bound: true,
      generatedLogged: true,
      newPassword: { accepted: true },
      oldPassword: { accepted: false, error: "Code 516" },
    });
  }, 900_000);

  test("a rollback to the release before the rotation keeps the new password, and so does the upgrade after it", async () => {
    const revisions = JSON.parse(helm("history", RELEASE, "--output", "json")) as { revision: number }[];
    const beforeRotation = Math.max(...revisions.map((r) => r.revision)) - 1;
    helm("rollback", RELEASE, String(beforeRotation), "--wait", "--timeout", "10m");
    await rolledOut();
    const rolledBack = adminSecret();
    const rollback = {
      secret: compared(rotatedPassword, rolledBack.password),
      generation: rolledBack.shape.generation,
      log: initLog().includes("generated the password") ? "generated" : "kept",
      login: accepts(rotatedPassword),
    };
    install("upgrade", "--set", "clickhouse.bundled.adminPasswordGeneration=2");
    await rolledOut();
    const upgraded = adminSecret();
    const upgrade = { secret: compared(rotatedPassword, upgraded.password), generation: upgraded.shape.generation, login: accepts(rotatedPassword) };
    report.rollback = { rollback, upgrade };
    console.log(`after a rollback to generation 1: ${JSON.stringify(rollback)}; after the upgrade back to 2: ${JSON.stringify(upgrade)}`);
    expect(rollback).toEqual({ secret: "unchanged", generation: "2", log: "kept", login: { accepted: true } });
    expect(upgrade).toEqual({ secret: "unchanged", generation: "2", login: { accepted: true } });
  }, 1_200_000);

  test("only ClickHouse's ServiceAccount may get and patch the Secret; no other ServiceAccount may read it", () => {
    // The chart's ServiceAccounts and the namespace's default one. Whoever runs this test (an
    // account that administers the namespace) is left out: it may read every Secret here.
    const chartAccounts = json<{ items: { metadata: { name: string } }[] }>("get", "serviceaccounts", "--selector", `app.kubernetes.io/instance=${RELEASE}`).items.map(
      (a) => a.metadata.name,
    );
    const accounts = [...chartAccounts, "default"].sort();
    const can = (account: string, verb: string, resource = `secret/${ADMIN_SECRET}`) =>
      exec([...kubectlArgs(), "auth", "can-i", verb, resource, `--as=system:serviceaccount:${NAMESPACE}:${account}`]).stdout.trim();
    const access: Record<string, { get: string; patch: string; list: string; create: string }> = {};
    for (const account of accounts) {
      access[account] = { get: can(account, "get"), patch: can(account, "patch"), list: can(account, "list", "secrets"), create: can(account, "create", "secrets") };
    }
    report.access = access;
    console.log(`access to ${ADMIN_SECRET}: ${JSON.stringify(access)}`);
    expect(accounts).toContain(STATEFULSET);
    for (const account of accounts) {
      const own = account === STATEFULSET;
      expect({ account, ...(access[account] ?? { get: "", patch: "", list: "", create: "" }) }).toEqual({ account, get: own ? "yes" : "no", patch: own ? "yes" : "no", list: "no", create: "no" });
    }
  }, 300_000);
});
