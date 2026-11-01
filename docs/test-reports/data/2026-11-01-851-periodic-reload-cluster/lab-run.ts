// The lab run of Laysi/trustiot-v2#851 (docs/test-reports/2026-11-01-851-periodic-reload-cluster.md):
// the periodic reloads of crates/service-kit's `periodic` in running pods on a real cluster.
//
// STEP=identity-keys runs deploy/chart/tests/e2e/identity-keys.e2e.ts unchanged (saas shape), whose
// rotation goes through `PublishedKeys::watch` and the identity issuer's `watch`.
//
// STEP=certs installs the chart with the certs test image (feature `e2e-reload-panic`) and test
// root A, then
//   1. helm upgrade to root B: the first reload after the kubelet swapped the new files into the
//      pod's volume (read on the node) logs `loaded the issuing CA` with root B, in the same certs
//      pod, not restarted and ready throughout;
//   2. helm upgrade to root C, whose common name carries the feature's marker: the first reload
//      after the swap panics once inside its lock; the schedule logs one ERROR with job=ca-reload,
//      the next reload, one period later, repairs the lock and loads root C, and the pod stays
//      ready and is not restarted.
//
// Run from the repository root on a workstation, after taking the lab lock (checked here),
// importing the images into the node and creating the namespace (Pod Security restricted); delete
// the namespace afterwards. The report lists the steps.
//
//   LAB_SSH=<ssh host of the lab node> WORK=<private dir outside the repository> STEP=<step> \
//   ART=<dir with root-a.pem, root-b.pem, root-c.pem; STEP=certs only> \
//   E2E_POSTGRES_IMAGE=<image on the node> \
//   bun docs/test-reports/data/2026-11-01-851-periodic-reload-cluster/lab-run.ts
//
// The node's admin kubeconfig stays in WORK (mode 0600) and is deleted on exit; the API server is
// reached through an SSH forward that reconnects when it drops. Results go to WORK/out. Nothing
// here holds a secret: the test roots' keys were dropped when they were made (build-images.sh).
// RENDER_ONLY=1 only renders the certs step's chart.
import { chmodSync, mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

const need = (name: string): string => {
  const value = process.env[name];
  if (!value) throw new Error(`set ${name}`);
  return value;
};
const STEP = need("STEP");
if (STEP !== "identity-keys" && STEP !== "certs") throw new Error(`STEP must be identity-keys or certs, not ${STEP}`);
const LAB_SSH = process.env.RENDER_ONLY === "1" ? "" : need("LAB_SSH");
const WORK = process.env.RENDER_ONLY === "1" ? "" : need("WORK");
const ART = STEP === "certs" ? need("ART") : "";
const NAMESPACE = process.env.E2E_NAMESPACE ?? "e2e-851";
const RELEASE = process.env.E2E_RELEASE ?? "trustiot";
const TAG = "851";
const CERTS_IMAGE = process.env.E2E_CERTS_IMAGE ?? `localhost/trustiot-e2e/certs-reload-panic:${TAG}`;
const IDENTITY_IMAGE = process.env.E2E_IDENTITY_IMAGE ?? `localhost/trustiot-e2e/identity:${TAG}`;
const MIGRATE_IMAGE = process.env.E2E_MIGRATE_IMAGE ?? `localhost/trustiot-e2e/migrate:${TAG}`;
const SERVICE_IMAGE = process.env.E2E_SERVICE_IMAGE ?? `localhost/trustiot-e2e/example-service:${TAG}`;
// The Postgres image with AGE already on the node (deploy/images/postgres).
const POSTGRES_IMAGE = need("E2E_POSTGRES_IMAGE");
const OVERLAY = "docs/test-reports/data/2026-11-01-851-periodic-reload-cluster/values-ca-reload-e2e.yaml";
const OUT = join(WORK, "out");
const KUBECONFIG = join(WORK, "kubeconfig");
const PORT = 16443;
/** Bound on every wait: the node is shared and often loaded; a timeout names what it awaited. */
const WAIT_MS = 10 * 60_000;
/** `ca.reload_interval`: the chart leaves the service's default. */
const RELOAD_PERIOD_S = 30;
/** The marker services/certs/src/authority.rs (`e2e_reload_panic::MARKER`) looks for. */
const MARKER = "e2e-reload-panic";

function proto(tool: string): string {
  const env = { ...process.env };
  delete env.AI_AGENT;
  delete env.CLAUDECODE;
  const result = Bun.spawnSync(["proto", "bin", tool], { env });
  const path = result.stdout.toString().trim().split("\n").pop() ?? "";
  if (result.exitCode !== 0 || !path.startsWith("/")) throw new Error(`proto bin ${tool} failed`);
  return path;
}
const HELM = proto("helm");
const KUBECTL = proto("kubectl");

const log = (line: string) => console.log(`${new Date().toISOString()} ${line}`);

/** The SSH tunnel and the test process running now, stopped on every exit, a signal included. */
const children = new Set<ReturnType<typeof Bun.spawn>>();

function run(cmd: string[], options: { input?: string; allowFail?: boolean; timeoutMs?: number } = {}): string {
  const result = Bun.spawnSync(cmd, {
    env: { ...process.env, KUBECONFIG },
    stdin: options.input === undefined ? "ignore" : new TextEncoder().encode(options.input),
    timeout: options.timeoutMs ?? 120_000,
  });
  const out = result.stdout.toString();
  if (result.exitCode !== 0 && !options.allowFail) {
    throw new Error(`${cmd.slice(0, 3).join(" ")} … exited ${result.exitCode}: ${result.stderr.toString().slice(-2000)}`);
  }
  return out;
}
const kubectl = (...args: string[]) => run([KUBECTL, "--namespace", NAMESPACE, ...args]);

async function waitFor<T>(what: string, probe: () => T | undefined | Promise<T | undefined>, everyMs = 3_000): Promise<T> {
  const deadline = Date.now() + WAIT_MS;
  for (;;) {
    let value: T | undefined;
    try {
      value = await probe();
    } catch (error) {
      // A read through the forward can fail while it reconnects; the deadline still holds.
      log(`waiting for ${what}: ${String(error).slice(0, 200)}`);
    }
    if (value !== undefined) return value;
    if (Date.now() > deadline) throw new Error(`timed out after ${WAIT_MS / 1000} s waiting for ${what}`);
    await Bun.sleep(everyMs);
  }
}

/** The DER of a one-block PEM file, base64 on one line: what the chart's values take. */
function derBase64(file: string): string {
  return readFileSync(join(ART, file), "utf8")
    .split("\n")
    .filter((line) => line && !line.startsWith("-----"))
    .join("");
}

function imageValues(path: string, image: string): string[] {
  const match = /^(?<repository>[^@]+?)(?::(?<tag>[\w][\w.-]*))?(?:@(?<digest>sha256:[a-f0-9]{64}))?$/.exec(image);
  if (!match?.groups?.repository) throw new Error(`not an image reference: ${image}`);
  const { repository, tag, digest } = match.groups;
  const values = ["--set-string", `${path}.repository=${repository}`];
  if (tag) values.push("--set-string", `${path}.tag=${tag}`);
  if (digest) values.push("--set-string", `${path}.digest=${digest}`);
  values.push("--set-string", `${path}.pullPolicy=${digest ? "IfNotPresent" : "Never"}`);
  return values;
}

function chartArgs(root: string): string[] {
  return [
    "deploy/chart",
    "--namespace", NAMESPACE,
    "-f", "deploy/chart/values-onprem.yaml",
    "-f", "deploy/chart/tests/values/onprem-instance.yaml",
    "-f", OVERLAY,
    ...imageValues("services.certs.image", CERTS_IMAGE),
    // crates/service-kit's example-service plays the identity service (the overlay says why).
    ...imageValues("services.identity.image", SERVICE_IMAGE),
    ...imageValues("migrate.image", MIGRATE_IMAGE),
    ...imageValues("postgres.bundled.image", POSTGRES_IMAGE),
    "--set-string", `certificateAuthority.rootCertificate=${derBase64(root)}`,
  ];
}

interface LogLine {
  timestamp: string;
  level: string;
  message: string;
  target?: string;
  fields: Record<string, unknown>;
}

interface PodState {
  name: string;
  uid: string;
  restarts: number;
  started: string;
  ready: boolean;
  readySince: string;
}

function certsPod(): PodState {
  const pods = JSON.parse(kubectl("get", "pods", "-l", `app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=certs`, "-o", "json")) as {
    items: {
      metadata: { name: string; uid: string; deletionTimestamp?: string };
      status: {
        conditions?: { type: string; status: string; lastTransitionTime: string }[];
        containerStatuses?: { name: string; restartCount: number; ready: boolean; state?: { running?: { startedAt: string } } }[];
      };
    }[];
  };
  const live = pods.items.filter((item) => !item.metadata.deletionTimestamp);
  const [item] = live;
  if (live.length !== 1 || !item) throw new Error(`expected one certs pod, found ${live.length}`);
  const container = item.status.containerStatuses?.find((status) => status.name === "certs");
  const ready = item.status.conditions?.find((condition) => condition.type === "Ready");
  return {
    name: item.metadata.name,
    uid: item.metadata.uid,
    restarts: container?.restartCount ?? -1,
    started: container?.state?.running?.startedAt ?? "",
    ready: ready?.status === "True" && container?.ready === true,
    readySince: ready?.lastTransitionTime ?? "",
  };
}

/** Every readiness sample taken while a step waited: when, and whether the pod was ready. */
const samples: { at: string; pod: string; ready: boolean; restarts: number }[] = [];
function sample(): PodState {
  const pod = certsPod();
  samples.push({ at: new Date().toISOString(), pod: pod.name, ready: pod.ready, restarts: pod.restarts });
  return pod;
}

function certsLog(pod: string): LogLine[] {
  return kubectl("logs", pod, "-c", "certs", "--tail=-1")
    .split("\n")
    .filter((line) => line.startsWith("{"))
    .map((line) => JSON.parse(line) as LogLine);
}

const LOADED = "loaded the issuing CA";
const PANICKED = "periodic job panicked; it runs again next period";
const REPAIRED = "an earlier reload of the CA files panicked";
/** The certs log lines about the CA reload and panics. */
const relevant = (line: LogLine) =>
  line.message === LOADED ||
  line.message === PANICKED ||
  line.message === "thread panicked" ||
  line.message.startsWith(REPAIRED) ||
  line.message === "cannot load the issuing CA";
const since = (at: string) => (line: LogLine) => Date.parse(line.timestamp) >= Date.parse(at);
const loadedRoot = (name: string) => (line: LogLine) => line.message === LOADED && String(line.fields.root).includes(name);

function rootInConfigMap(): string {
  const cm = JSON.parse(kubectl("get", "configmap", `${RELEASE}-certs-ca-certificates`, "-o", "json")) as { data: Record<string, string> };
  return (cm.data["root.pem"] ?? "")
    .split("\n")
    .filter((line) => line && !line.startsWith("-----"))
    .join("");
}

/**
 * Fails the run, after the report is saved, when what a step recorded is not what the acceptance
 * criteria of Laysi/trustiot-v2#851 require: a recorded value alone never makes the run pass.
 */
function expect(step: string, checks: Record<string, boolean>): void {
  const failed = Object.entries(checks)
    .filter(([, ok]) => !ok)
    .map(([what]) => what);
  if (failed.length > 0) throw new Error(`${step}: ${failed.join("; ")}`);
}

const seconds = (from: string, to: string) => (Date.parse(to) - Date.parse(from)) / 1000;

/**
 * The directory the kubelet's atomic writer last swapped in for the pod's CA certificates volume
 * (`..data` points to `..<UTC time, seconds>.<random>`), read on the node: when the new files
 * appeared in the pod. The image has no shell to look from inside.
 */
function dataLink(podUid: string): string {
  if (!/^[0-9a-f-]{36}$/.test(podUid)) throw new Error(`not a pod uid: ${podUid}`);
  const link = run(["ssh", LAB_SSH, `sudo readlink /var/lib/kubelet/pods/${podUid}/volumes/kubernetes.io~configmap/certs-ca-certificates/..data`]).trim();
  if (!/^\.\.\d{4}_\d{2}_\d{2}_\d{2}_\d{2}_\d{2}\.\d+$/.test(link)) throw new Error(`unexpected ..data target: ${link}`);
  return link;
}
/** The UTC time in a `..data` target's name, to the second. */
function swappedAt(link: string): string {
  const [, y, mo, d, h, mi, s] = /^\.\.(\d{4})_(\d{2})_(\d{2})_(\d{2})_(\d{2})_(\d{2})\./.exec(link) ?? [];
  return `${y}-${mo}-${d}T${h}:${mi}:${s}Z`;
}

/**
 * helm upgrade to `root`, then waits until the ConfigMap holds it and the kubelet swapped the new
 * files into the pod's volume.
 */
async function upgradeTo(
  root: string,
  podUid: string,
): Promise<{ started: string; helmDone: string; configMapSeenAt: string; fileSwap: { link: string; at: string; seenAt: string } }> {
  const before = dataLink(podUid);
  const started = new Date().toISOString();
  run([HELM, "upgrade", RELEASE, ...chartArgs(root), "--wait=legacy", "--timeout", "10m"], { timeoutMs: 11 * 60_000 });
  const helmDone = new Date().toISOString();
  const configMapSeenAt = await waitFor(`the ConfigMap holding ${root}`, () => {
    sample();
    return rootInConfigMap() === derBase64(root) ? new Date().toISOString() : undefined;
  });
  const fileSwap = await waitFor(`the kubelet swapping ${root} into the pod`, () => {
    sample();
    const link = dataLink(podUid);
    return link === before ? undefined : { link, at: swappedAt(link), seenAt: new Date().toISOString() };
  }, 1_000);
  return { started, helmDone, configMapSeenAt, fileSwap };
}

/** Slack for a reload tick against the period: the tick of a loaded node may come a little late. */
const TICK_SLACK_S = 5;

async function certsStep(): Promise<void> {
  mkdirSync(OUT, { recursive: true, mode: 0o700 });
  const report: Record<string, unknown> = {
    namespace: NAMESPACE,
    release: RELEASE,
    reloadPeriodSeconds: RELOAD_PERIOD_S,
    images: { certs: CERTS_IMAGE, identity: SERVICE_IMAGE, migrate: MIGRATE_IMAGE, postgres: POSTGRES_IMAGE },
  };
  const save = () => {
    report.readinessSamples = { count: samples.length, notReady: samples.filter((s) => !s.ready) };
    writeFileSync(join(OUT, "ca-reload-e2e.json"), `${JSON.stringify(report, null, 2)}\n`);
  };

  // Install with root A.
  log("helm install with root A");
  const installStart = Date.now();
  run([HELM, "install", RELEASE, ...chartArgs("root-a.pem"), "--wait=legacy", "--wait-for-jobs", "--timeout", "20m"], { timeoutMs: 21 * 60_000 });
  report.installSeconds = (Date.now() - installStart) / 1000;
  const pod = await waitFor("a ready certs pod", () => {
    const state = certsPod();
    return state.ready ? state : undefined;
  });
  log(`certs pod ${pod.name} (${pod.uid}), restarts ${pod.restarts}`);
  const first = await waitFor("certs loading root A", () => certsLog(pod.name).find(loadedRoot("root A")));
  report.install = { pod, loaded: first };
  save();

  // Step 1: root B, loaded by the running pod without a restart.
  log("helm upgrade to root B");
  const b = await upgradeTo("root-b.pem", pod.uid);
  const loadedB = await waitFor("certs loading root B", () => {
    sample();
    return certsLog(pod.name).filter(since(b.started)).find(loadedRoot("root B"));
  });
  const afterB = sample();
  const reload = {
    ...b,
    loaded: loadedB,
    secondsFromFileSwapToLoad: seconds(b.fileSwap.at, loadedB.timestamp),
    secondsFromConfigMapToLoad: seconds(b.configMapSeenAt, loadedB.timestamp),
    secondsFromHelmDoneToLoad: seconds(b.helmDone, loadedB.timestamp),
    pod: afterB,
    errors: certsLog(pod.name).filter(since(b.started)).filter((line) => line.level === "ERROR"),
  };
  report.reload = reload;
  save();
  expect("reload", {
    "the same certs pod, not restarted, loaded root B": afterB.uid === pod.uid && afterB.restarts === 0 && afterB.started === pod.started,
    "the pod has been ready since before the upgrade": afterB.ready && Date.parse(afterB.readySince) <= Date.parse(b.started),
    "no ERROR while it reloaded": reload.errors.length === 0,
    // The swap time is cut to the second, hence the extra second.
    "loaded at the first reload after the file appeared in the pod":
      reload.secondsFromFileSwapToLoad >= 0 && reload.secondsFromFileSwapToLoad <= RELOAD_PERIOD_S + TICK_SLACK_S + 1,
  });

  // Step 2: root C, whose first reload panics in the test image; the next one loads it.
  log("helm upgrade to root C (marked)");
  const c = await upgradeTo("root-c.pem", pod.uid);
  const loadedC = await waitFor("certs loading root C after the injected panic", () => {
    sample();
    return certsLog(pod.name).filter(since(c.started)).find(loadedRoot(MARKER));
  });
  // One more period: the panic fires once per process, so no second one follows.
  log("one more reload period");
  const quietUntil = Date.now() + (RELOAD_PERIOD_S + 10) * 1000;
  while (Date.now() < quietUntil) {
    sample();
    await Bun.sleep(3_000);
  }
  const afterC = sample();
  const lines = certsLog(pod.name).filter(since(c.started)).filter(relevant);
  const panics = lines.filter((line) => line.message === PANICKED);
  const hook = lines.filter((line) => line.message === "thread panicked");
  const repairs = lines.filter((line) => line.message.startsWith(REPAIRED));
  const loadsC = lines.filter(loadedRoot(MARKER));
  const [panic] = panics;
  const [repair] = repairs;
  const panicStep = {
    ...c,
    panic,
    hook,
    repair,
    loaded: loadedC,
    secondsFromPanicToLoad: panic ? seconds(panic.timestamp, loadedC.timestamp) : null,
    secondsFromFileSwapToPanic: panic ? seconds(c.fileSwap.at, panic.timestamp) : null,
    secondsFromConfigMapToPanic: panic ? seconds(c.configMapSeenAt, panic.timestamp) : null,
    pod: afterC,
    lines,
  };
  report.panic = panicStep;
  save();
  writeFileSync(join(OUT, "certs-ca-reload-log.jsonl"), `${certsLog(pod.name).filter(relevant).map((line) => JSON.stringify(line)).join("\n")}\n`);
  expect("panic", {
    "one schedule ERROR, naming the job ca-reload": panics.length === 1 && panic?.level === "ERROR" && panic.fields.job === "ca-reload",
    "the panic came before root C was loaded": !!panic && Date.parse(panic.timestamp) <= Date.parse(loadedC.timestamp),
    "the next reload repaired the lock (WARN) and then loaded root C": repairs.length === 1 && repair?.level === "WARN" && !!panic &&
      Date.parse(panic.timestamp) <= Date.parse(repair.timestamp) && Date.parse(repair.timestamp) <= Date.parse(loadedC.timestamp),
    "root C was loaded once": loadsC.length === 1,
    "the reload that panicked was the first after the file appeared in the pod":
      panicStep.secondsFromFileSwapToPanic !== null &&
      panicStep.secondsFromFileSwapToPanic >= 0 &&
      panicStep.secondsFromFileSwapToPanic <= RELOAD_PERIOD_S + TICK_SLACK_S + 1,
    "root C was loaded by the next reload, one period after the panic":
      panicStep.secondsFromPanicToLoad !== null && panicStep.secondsFromPanicToLoad <= RELOAD_PERIOD_S + TICK_SLACK_S,
    "the same certs pod, not restarted": afterC.uid === pod.uid && afterC.restarts === 0 && afterC.started === pod.started,
    "the pod has been ready since before the first upgrade": afterC.ready && Date.parse(afterC.readySince) <= Date.parse(b.started),
    "every readiness sample was ready": samples.every((s) => s.ready && s.pod === pod.name),
  });
  report.helmHistory = JSON.parse(run([HELM, "history", RELEASE, "--namespace", NAMESPACE, "-o", "json"]));
  save();
  log("done");
}

/** deploy/chart/tests/e2e/identity-keys.e2e.ts, unchanged, with this run's images. */
async function identityKeysStep(): Promise<void> {
  mkdirSync(OUT, { recursive: true, mode: 0o700 });
  const env: Record<string, string | undefined> = { ...process.env, KUBECONFIG };
  delete env.AI_AGENT;
  delete env.CLAUDECODE;
  const test = Bun.spawn(["bun", "test", "--timeout", "3600000", "./deploy/chart/tests/e2e/identity-keys.e2e.ts"], {
    env: {
      ...env,
      E2E_NAMESPACE: NAMESPACE,
      E2E_RELEASE: RELEASE,
      E2E_SHAPE: "saas",
      E2E_POSTGRES_IMAGE: POSTGRES_IMAGE,
      E2E_IDENTITY_IMAGE: IDENTITY_IMAGE,
      E2E_SERVICE_IMAGE: SERVICE_IMAGE,
      E2E_MIGRATE_IMAGE: MIGRATE_IMAGE,
      E2E_IMAGE_PULL_POLICY: "Never",
      E2E_REPORT_DIR: OUT,
    },
    stdout: "inherit",
    stderr: "inherit",
  });
  children.add(test);
  const code = await test.exited;
  children.delete(test);
  if (code !== 0) throw new Error(`identity-keys.e2e.ts exited ${code}`);
}

// RENDER_ONLY=1: only render the certs step's chart with the run's values, no cluster.
if (process.env.RENDER_ONLY === "1") {
  const rendered = run([HELM, "template", RELEASE, ...chartArgs("root-a.pem")]);
  const kinds = [...rendered.matchAll(/^kind: (\S+)$/gm)].map((match) => match[1]);
  console.log(`${kinds.length} objects: ${[...new Set(kinds)].sort().join(", ")}`);
  console.log([...new Set([...rendered.matchAll(/^\s+image: "?([^"\s]+)"?$/gm)].map((match) => match[1]))].sort().join("\n"));
  process.exit(0);
}

// Only a test namespace.
if (!/^e2e-\d+$/.test(NAMESPACE)) throw new Error(`E2E_NAMESPACE must be e2e-<issue>, not ${NAMESPACE}`);
// The kubeconfig holds the node's admin credentials: a private directory outside the repository,
// a new 0600 file, and deleted on every exit, an interrupt included.
const repo = realpathSync(Bun.spawnSync(["git", "rev-parse", "--show-toplevel"]).stdout.toString().trim() || ".");
/** WORK must not be the repository, inside it, or a directory holding it, whose mode it changes. */
const overlaps = (dir: string) => dir === "/" || dir === repo || dir.startsWith(`${repo}/`) || repo.startsWith(`${dir}/`);
if (overlaps(resolve(WORK))) throw new Error("WORK must be a directory of its own outside the repository");
mkdirSync(WORK, { recursive: true, mode: 0o700 });
// Again on the canonical path, which a symbolic link can move into the repository.
if (overlaps(realpathSync(WORK))) throw new Error("WORK must be a directory of its own outside the repository");
chmodSync(WORK, 0o700);
// Only under the lab lock of this issue.
const owner = run(["ssh", LAB_SSH, "cat ~/e2e.lock/owner"], { allowFail: true });
if (!owner.startsWith(`Laysi/trustiot-v2#${NAMESPACE.slice(4)} `)) {
  throw new Error(`the lab lock is not held for Laysi/trustiot-v2#${NAMESPACE.slice(4)}: ${owner.trim() || "no lock"}`);
}
const kubeconfig = run(["ssh", LAB_SSH, "sudo cat /etc/rancher/k3s/k3s.yaml"]).replace("https://127.0.0.1:6443", `https://127.0.0.1:${PORT}`);
rmSync(KUBECONFIG, { force: true });
writeFileSync(KUBECONFIG, kubeconfig, { mode: 0o600, flag: "wx" });
let stopping = false;
/** Stops the tunnel and the test process and deletes the kubeconfig; safe to call twice. */
function cleanUp(): void {
  stopping = true;
  for (const child of children) child.kill();
  rmSync(KUBECONFIG, { force: true });
}
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) {
  process.on(signal, () => {
    cleanUp();
    process.exit(130);
  });
}
(async () => {
  while (!stopping) {
    const tunnel = Bun.spawn(["ssh", "-N", "-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=4", "-L", `127.0.0.1:${PORT}:127.0.0.1:6443`, LAB_SSH], {
      stdout: "ignore",
      stderr: "ignore",
    });
    children.add(tunnel);
    // A signal that arrived while it was starting.
    if (stopping) tunnel.kill();
    await tunnel.exited;
    children.delete(tunnel);
    if (!stopping) await Bun.sleep(2_000);
  }
})();
let code = 0;
try {
  await waitFor("the SSH forward to the API server", () => (run([KUBECTL, "version", "-o", "json"], { allowFail: true }).includes("serverVersion") ? true : undefined), 1_000);
  if (STEP === "certs") await certsStep();
  else await identityKeysStep();
} catch (error) {
  console.error(error);
  code = 1;
} finally {
  cleanUp();
}
process.exit(code);
