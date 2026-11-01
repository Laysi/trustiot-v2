// The lab run of Laysi/trustiot-v2#840 (docs/test-reports/2026-11-01-840-root-crl-cluster.md):
// the chart with the certs service on a real cluster; helm upgrade swaps the root CRL for a newer
// one, which the running certs pod adopts without a restart once the kubelet updates the
// ConfigMap, then helm rollback brings the older one back, which the same pod refuses with an
// ERROR, keeping the newer one, and certs.root_crl.invalid becomes 1.
//
// Run from the repository root on a workstation, after taking the lab lock (checked here),
// importing the images into the node and creating the namespace (Pod Security restricted); delete
// the namespace afterwards. The report lists the steps.
//
//   LAB_SSH=<ssh host of the lab node> WORK=<private dir outside the repository>
//   ART=<dir with root.pem, crl-1.pem, crl-2.pem> E2E_POSTGRES_IMAGE=<image on the node>
//   bun docs/test-reports/data/2026-11-01-840-root-crl-cluster/lab-run.ts
//
// The node's admin kubeconfig stays in WORK (mode 0600) and is deleted on exit; the API server is
// reached through an SSH forward that reconnects when it drops. Results go to WORK/out. Nothing
// here holds a secret: the test root's key was dropped when its CRLs were signed
// (build-images.sh). RENDER_ONLY=1 only renders the chart with the run's values.
import { chmodSync, mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

const need = (name: string): string => {
  const value = process.env[name];
  if (!value) throw new Error(`set ${name}`);
  return value;
};
const LAB_SSH = need("LAB_SSH");
const WORK = need("WORK");
const ART = need("ART");
const NAMESPACE = process.env.E2E_NAMESPACE ?? "e2e-840";
const RELEASE = process.env.E2E_RELEASE ?? "trustiot";
const CERTS_IMAGE = process.env.E2E_CERTS_IMAGE ?? "localhost/trustiot-e2e/certs:840";
const MIGRATE_IMAGE = process.env.E2E_MIGRATE_IMAGE ?? "localhost/trustiot-e2e/migrate:840";
// crates/service-kit's example-service, playing the identity service (values-root-crl-e2e.yaml).
const IDENTITY_IMAGE = process.env.E2E_IDENTITY_IMAGE ?? "localhost/trustiot-e2e/example-service:840";
// The Postgres image with AGE already on the node (deploy/images/postgres, unchanged since).
const POSTGRES_IMAGE = need("E2E_POSTGRES_IMAGE");
const COLLECTOR_IMAGE =
  "docker.io/otel/opentelemetry-collector-contrib:0.161.0@sha256:fd328de2552466ad78385e1b1289c3f2402b1c45f265b252aab1955b42845ac1";
const RECEIVER = "trustiot-e2e-metrics";
const OVERLAY = "docs/test-reports/data/2026-11-01-840-root-crl-cluster/values-root-crl-e2e.yaml";
const OUT = join(WORK, "out");
const KUBECONFIG = join(WORK, "kubeconfig");
const PORT = 16443;
/** Bound on every wait: the node is shared and often loaded; a timeout names what it awaited. */
const WAIT_MS = 10 * 60_000;

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

/** The SSH tunnel and port-forwards running now, stopped on every exit, a signal included. */
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

async function waitFor<T>(what: string, probe: () => T | undefined | Promise<T | undefined>, everyMs = 5_000): Promise<T> {
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

function chartArgs(crl: string): string[] {
  return [
    "deploy/chart",
    "--namespace", NAMESPACE,
    "-f", "deploy/chart/values-onprem.yaml",
    "-f", "deploy/chart/tests/values/onprem-instance.yaml",
    "-f", OVERLAY,
    ...imageValues("services.certs.image", CERTS_IMAGE),
    ...imageValues("services.identity.image", IDENTITY_IMAGE),
    ...imageValues("migrate.image", MIGRATE_IMAGE),
    ...imageValues("postgres.bundled.image", POSTGRES_IMAGE),
    "--set-string", "services.certs.env[0].name=TRUSTIOT__TELEMETRY__OTLP_ENDPOINT",
    "--set-string", `services.certs.env[0].value=http://${RECEIVER}.${NAMESPACE}.svc:4318`,
    "--set-string", `certificateAuthority.rootCertificate=${derBase64("root.pem")}`,
    "--set-string", `certificateAuthority.rootCrl=${derBase64(crl)}`,
  ];
}

// The receiver: the chart's collector image with a debug exporter that prints every metric it
// gets. Not in the mesh; the chart denies all incoming traffic in the namespace by default, so a
// NetworkPolicy admits the certs pods on the OTLP port only (as otel.e2e.ts's stand-in SIEM).
const RECEIVER_CONFIG = `receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318
exporters:
  debug:
    verbosity: detailed
  nop:
service:
  pipelines:
    metrics: {receivers: [otlp], exporters: [debug]}
    # certs sends its logs and traces to the same endpoint; they are read with kubectl logs.
    logs: {receivers: [otlp], exporters: [nop]}
    traces: {receivers: [otlp], exporters: [nop]}
`;
function receiverManifests(): string {
  const labels = { "app.kubernetes.io/name": RECEIVER, "app.kubernetes.io/part-of": "trustiot-e2e" };
  const objects = [
    { apiVersion: "v1", kind: "ConfigMap", metadata: { name: RECEIVER, labels }, data: { "config.yaml": RECEIVER_CONFIG } },
    {
      apiVersion: "networking.k8s.io/v1",
      kind: "NetworkPolicy",
      metadata: { name: RECEIVER, labels },
      spec: {
        podSelector: { matchLabels: labels },
        policyTypes: ["Ingress"],
        ingress: [
          {
            from: [{ podSelector: { matchLabels: { "app.kubernetes.io/instance": RELEASE, "app.kubernetes.io/component": "certs" } } }],
            ports: [{ port: 4318, protocol: "TCP" }],
          },
        ],
      },
    },
    {
      apiVersion: "v1",
      kind: "Service",
      metadata: { name: RECEIVER, labels },
      spec: { type: "ClusterIP", selector: labels, ports: [{ name: "otlp-http", port: 4318, targetPort: 4318 }] },
    },
    {
      apiVersion: "apps/v1",
      kind: "Deployment",
      metadata: { name: RECEIVER, labels },
      spec: {
        replicas: 1,
        selector: { matchLabels: labels },
        template: {
          metadata: { labels },
          spec: {
            automountServiceAccountToken: false,
            securityContext: { runAsNonRoot: true, seccompProfile: { type: "RuntimeDefault" } },
            nodeSelector: { "kubernetes.io/arch": "arm64" },
            containers: [
              {
                name: "receiver",
                image: COLLECTOR_IMAGE,
                imagePullPolicy: "IfNotPresent",
                args: ["--config=/etc/receiver/config.yaml"],
                ports: [{ containerPort: 4318 }],
                resources: { requests: { cpu: "20m", memory: "48Mi" }, limits: { memory: "256Mi" } },
                securityContext: { allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: { drop: ["ALL"] } },
                volumeMounts: [{ name: "config", mountPath: "/etc/receiver", readOnly: true }],
              },
            ],
            volumes: [{ name: "config", configMap: { name: RECEIVER } }],
          },
        },
      },
    },
  ];
  return objects.map((o) => JSON.stringify(o)).join("\n---\n");
}

interface LogLine {
  timestamp: string;
  level: string;
  message: string;
  fields: Record<string, unknown>;
}

function certsPod(): { name: string; uid: string; restarts: number; started: string } {
  const pod = JSON.parse(kubectl("get", "pods", "-l", `app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=certs`, "-o", "json")) as {
    items: { metadata: { name: string; uid: string; deletionTimestamp?: string }; status: { containerStatuses?: { name: string; restartCount: number; state?: { running?: { startedAt: string } } }[] } }[];
  };
  const live = pod.items.filter((item) => !item.metadata.deletionTimestamp);
  const [item] = live;
  if (live.length !== 1 || !item) throw new Error(`expected one certs pod, found ${live.length}`);
  const container = item.status.containerStatuses?.find((status) => status.name === "certs");
  return {
    name: item.metadata.name,
    uid: item.metadata.uid,
    restarts: container?.restartCount ?? -1,
    started: container?.state?.running?.startedAt ?? "",
  };
}

function certsLog(pod: string): LogLine[] {
  return kubectl("logs", pod, "-c", "certs", "--tail=-1")
    .split("\n")
    .filter((line) => line.startsWith("{"))
    .map((line) => JSON.parse(line) as LogLine);
}

interface Point {
  metric: string;
  time: string;
  value: number;
}

/** The gauges the receiver printed (the debug exporter's detailed text). */
function points(): Point[] {
  const found: Point[] = [];
  let metric = "";
  let time = "";
  for (const line of kubectl("logs", `deployment/${RECEIVER}`, "--tail=-1").split("\n")) {
    const name = /-> Name: (\S+)/.exec(line);
    if (name?.[1]) metric = name[1];
    const timestamp = /^Timestamp: (.+)$/.exec(line.trim());
    if (timestamp?.[1]) time = new Date(timestamp[1].replace(/ \+0000 UTC$/, "Z").replace(" ", "T")).toISOString();
    const value = /^Value: (\S+)$/.exec(line.trim());
    if (value && metric.startsWith("certs.root_crl.")) found.push({ metric, time, value: Number(value[1]) });
  }
  return found;
}

/** The first export after `after` that holds all three root CRL gauges. */
function gaugesAfter(after: string): Record<string, number> | undefined {
  const byTime = new Map<string, Record<string, number>>();
  for (const point of points()) {
    if (point.time <= after) continue;
    const at = byTime.get(point.time) ?? {};
    at[point.metric] = point.value;
    byTime.set(point.time, at);
  }
  return [...byTime.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([time, gauges]) => ({ time, ...gauges }))
    .find((gauges) => "certs.root_crl.invalid" in gauges && "certs.root_crl.expiring" in gauges && "certs.root_crl.remaining" in gauges) as
    | Record<string, number>
    | undefined;
}

/** What GET /certs/root-crl serves from the certs pod, through a port-forward (no ingress). */
async function served(pod: string): Promise<string> {
  const forward = Bun.spawn([KUBECTL, "--namespace", NAMESPACE, "port-forward", `pod/${pod}`, "18080:8080"], {
    env: { ...process.env, KUBECONFIG },
    stdout: "ignore",
    stderr: "ignore",
  });
  children.add(forward);
  try {
    return await waitFor("the port-forward to the certs pod", async () => {
      const response = await fetch("http://127.0.0.1:18080/certs/root-crl", { signal: AbortSignal.timeout(10_000) });
      if (response.status !== 200) throw new Error(`GET /certs/root-crl answered ${response.status}`);
      return await response.text();
    }, 1_000);
  } finally {
    forward.kill();
    await forward.exited;
    children.delete(forward);
  }
}

const sameCrl = (pem: string, file: string) =>
  pem.split("\n").filter((line) => line && !line.startsWith("-----")).join("") === derBase64(file);

function rootCrlInConfigMap(): string {
  const cm = JSON.parse(kubectl("get", "configmap", `${RELEASE}-certs-ca-certificates`, "-o", "json")) as { data: Record<string, string> };
  return cm.data["root-crl.pem"] ?? "";
}

/**
 * Fails the run, after the report is saved, when what a step recorded is not what the acceptance
 * criteria of Laysi/trustiot-v2#840 require: a recorded value alone never makes the run pass.
 */
function expect(step: string, checks: Record<string, boolean>): void {
  const failed = Object.entries(checks)
    .filter(([, ok]) => !ok)
    .map(([what]) => what);
  if (failed.length > 0) throw new Error(`${step}: ${failed.join("; ")}`);
}

/** The certs log lines about the root CRL. */
const rootCrlLines = (lines: LogLine[]) => lines.filter((line) => line.message.includes("root CRL"));

async function main(): Promise<void> {
  mkdirSync(OUT, { recursive: true, mode: 0o700 });
  const report: Record<string, unknown> = {
    namespace: NAMESPACE,
    release: RELEASE,
    images: { certs: CERTS_IMAGE, identity: IDENTITY_IMAGE, migrate: MIGRATE_IMAGE, postgres: POSTGRES_IMAGE },
  };
  const save = () => writeFileSync(join(OUT, "root-crl-e2e.json"), `${JSON.stringify(report, null, 2)}\n`);

  // Step 1: the receiver and the chart with root CRL 1.
  log("receiver");
  run([KUBECTL, "--namespace", NAMESPACE, "apply", "-f", "-"], { input: receiverManifests() });
  kubectl("rollout", "status", `deployment/${RECEIVER}`, "--timeout=300s");
  log("helm install with root CRL 1");
  const installStart = Date.now();
  run([HELM, "install", RELEASE, ...chartArgs("crl-1.pem"), "--wait", "--timeout", "20m"], { timeoutMs: 21 * 60_000 });
  report.installSeconds = (Date.now() - installStart) / 1000;
  const pod = certsPod();
  report.pod = pod;
  log(`certs pod ${pod.name} (${pod.uid}), restarts ${pod.restarts}`);
  const first = await waitFor("certs adopting root CRL 1", () =>
    certsLog(pod.name).find((line) => line.message === "adopted the root CRL" && line.fields.number === "1"),
  );
  const install = {
    adoption: first,
    gauges: await waitFor("the gauges after root CRL 1", () => gaugesAfter(first.timestamp), 10_000),
    served: sameCrl(await served(pod.name), "crl-1.pem") ? "crl-1" : "other",
  };
  report.install = install;
  save();
  expect("install", {
    "the certs pod has not restarted": pod.restarts === 0,
    "certs serves root CRL 1": install.served === "crl-1",
    "certs.root_crl.invalid is 0": install.gauges["certs.root_crl.invalid"] === 0,
    "certs.root_crl.expiring is 0": install.gauges["certs.root_crl.expiring"] === 0,
  });

  // Step 2: helm upgrade to root CRL 2; the same pod adopts it once the kubelet updates the file.
  log("helm upgrade to root CRL 2");
  const upgradeStart = new Date().toISOString();
  run([HELM, "upgrade", RELEASE, ...chartArgs("crl-2.pem"), "--wait", "--timeout", "10m"], { timeoutMs: 11 * 60_000 });
  const upgradeDone = new Date().toISOString();
  const configMapUpdated = await waitFor("the ConfigMap holding root CRL 2", () =>
    sameCrl(rootCrlInConfigMap(), "crl-2.pem") ? new Date().toISOString() : undefined,
  );
  const second = await waitFor("certs adopting root CRL 2", () =>
    certsLog(pod.name).find((line) => line.message === "adopted the root CRL" && line.fields.number === "2"),
  );
  const afterUpgrade = certsPod();
  const upgrade = {
    started: upgradeStart,
    helmDone: upgradeDone,
    configMapUpdatedSeenAt: configMapUpdated,
    adoption: second,
    secondsFromHelmDoneToAdoption: (Date.parse(second.timestamp) - Date.parse(upgradeDone)) / 1000,
    pod: afterUpgrade,
    samePod: afterUpgrade.uid === pod.uid && afterUpgrade.restarts === pod.restarts && afterUpgrade.started === pod.started,
    gauges: await waitFor("the gauges after root CRL 2", () => gaugesAfter(second.timestamp), 10_000),
    served: sameCrl(await served(pod.name), "crl-2.pem") ? "crl-2" : "other",
  };
  report.upgrade = upgrade;
  save();
  expect("upgrade", {
    "the same certs pod, not restarted, adopted root CRL 2": upgrade.samePod && afterUpgrade.restarts === 0,
    "certs serves root CRL 2": upgrade.served === "crl-2",
    "certs.root_crl.invalid is 0": upgrade.gauges["certs.root_crl.invalid"] === 0,
    "certs.root_crl.expiring is 0": upgrade.gauges["certs.root_crl.expiring"] === 0,
  });

  // Step 3: helm rollback to revision 1 (root CRL 1): the same pod refuses it and keeps 2.
  log("helm rollback to revision 1");
  const rollbackStart = new Date().toISOString();
  run([HELM, "rollback", RELEASE, "1", "--namespace", NAMESPACE, "--wait", "--timeout", "10m"], { timeoutMs: 11 * 60_000 });
  const rollbackDone = new Date().toISOString();
  const configMapRolledBack = await waitFor("the ConfigMap holding root CRL 1 again", () =>
    sameCrl(rootCrlInConfigMap(), "crl-1.pem") ? new Date().toISOString() : undefined,
  );
  const refusal = await waitFor("certs refusing root CRL 1 after the rollback", () =>
    certsLog(pod.name).find(
      (line) => line.level === "ERROR" && line.message.startsWith("cannot adopt the root CRL") && Date.parse(line.timestamp) >= Date.parse(rollbackStart),
    ),
  );
  const afterRollback = certsPod();
  const lines = certsLog(pod.name);
  const rollback = {
    started: rollbackStart,
    helmDone: rollbackDone,
    configMapUpdatedSeenAt: configMapRolledBack,
    refusal,
    secondsFromHelmDoneToRefusal: (Date.parse(refusal.timestamp) - Date.parse(rollbackDone)) / 1000,
    adoptionsAfterRollback: lines.filter((line) => line.message === "adopted the root CRL" && Date.parse(line.timestamp) >= Date.parse(rollbackStart)).length,
    pod: afterRollback,
    samePod: afterRollback.uid === pod.uid && afterRollback.restarts === pod.restarts && afterRollback.started === pod.started,
    gauges: await waitFor("the gauges after the refusal", () => gaugesAfter(refusal.timestamp), 10_000),
    served: sameCrl(await served(pod.name), "crl-2.pem") ? "crl-2" : "other",
  };
  report.rollback = rollback;
  report.helmHistory = JSON.parse(run([HELM, "history", RELEASE, "--namespace", NAMESPACE, "-o", "json"]));
  save();
  writeFileSync(join(OUT, "certs-root-crl-log.jsonl"), `${rootCrlLines(lines).map((line) => JSON.stringify(line)).join("\n")}\n`);
  writeFileSync(join(OUT, "root-crl-gauges.jsonl"), `${points().map((point) => JSON.stringify(point)).join("\n")}\n`);
  expect("rollback", {
    "the refusal keeps root CRL 2": refusal.fields.kept_number === "2",
    "certs adopted nothing after the rollback": rollback.adoptionsAfterRollback === 0,
    "the same certs pod, not restarted, refused root CRL 1": rollback.samePod && afterRollback.restarts === 0,
    "certs still serves root CRL 2": rollback.served === "crl-2",
    "certs.root_crl.invalid is 1": rollback.gauges["certs.root_crl.invalid"] === 1,
  });
  log("done");
}

// RENDER_ONLY=1: only render the chart with the run's values, no cluster.
if (process.env.RENDER_ONLY === "1") {
  const rendered = run([HELM, "template", RELEASE, ...chartArgs("crl-1.pem")]);
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
/** Stops the tunnel and every port-forward and deletes the kubeconfig; safe to call twice. */
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
  await main();
} catch (error) {
  console.error(error);
  code = 1;
} finally {
  cleanUp();
}
process.exit(code);
