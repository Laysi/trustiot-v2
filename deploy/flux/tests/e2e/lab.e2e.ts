/**
 * End-to-end test of the Flux mechanics behind deploy/flux/ on a real cluster (Laysi/trustiot-v2#70).
 * It installs Flux the way deploy/flux/README.md bootstraps the SaaS cluster, then proves:
 *
 * 1. Merging a deploy commit (chart.yaml for a new version, as release.yml's deploy PR writes it)
 *    deploys the new chart within the GitRepository interval; reverting it rolls back.
 * 2. Flux installs a chart only with a valid cosign signature: a chart signed by another key is
 *    refused and the running release stays as it was.
 * 3. The pull credential reaches the cluster as a SOPS-encrypted Secret in git, decrypted by
 *    kustomize-controller with the cluster's age key, and source-controller pulls the chart from
 *    a registry that refuses anonymous pulls with it.
 *
 * What differs from production, and why (deploy/flux/README.md, "lab 測試"):
 * - The git server is a throwaway soft-serve in the test namespace, reached over SSH with a
 *   read-only key, instead of GitHub with a deploy key. The repository has the same layout.
 * - The registry is a throwaway distribution registry with TLS from a throwaway CA and htpasswd
 *   auth, instead of GHCR; the chart is a small test chart, not the TrustIoT chart.
 * - The signature is a throwaway cosign key pair, not keyless: keyless signatures come only from
 *   release.yml on a v* tag (decision 30). So chart.yaml here verifies with `secretRef` (the test
 *   public key) where production verifies with `matchOIDCIdentity`; everything else in it is what
 *   scripts/release/flux.ts renders.
 * - Everything runs in one namespace (E2E_NAMESPACE) instead of trustiot, and Flux does not own
 *   that namespace.
 * Every key, password and certificate is generated at run time and deleted afterwards.
 *
 * Installing Flux is a cluster-wide change (CRDs, ClusterRoles, the flux-system namespace). The
 * script records the cluster-scoped objects before it starts, removes everything it created, and
 * compares (written to E2E_OUT/cluster-state.json). It refuses to start, and checks again right
 * before installing Flux, unless the lab lock ~/e2e.lock names the issue of E2E_NAMESPACE and the
 * cluster has no Flux (no flux-system, no Flux CRDs). It removes Flux only while flux-system still
 * carries the label of this run (preflight.ts).
 *
 *   E2E_SSH_HOST=trustiot-mac-m1 E2E_NAMESPACE=e2e-70 E2E_OUT=<dir> bun deploy/flux/tests/e2e/lab.e2e.ts
 *
 * kubectl runs on the host as `sudo k3s kubectl` over ssh; helm, cosign, sops and kubectl
 * (kustomize) run here, at the versions in .prototools, through an SSH tunnel to the registry.
 */

import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { protoTool, run, tryRun } from "../../../../scripts/release/exec.ts";
import { APP_DIR, CHART_SOURCE_FILE, CLUSTER_DIR, SHAPE_VALUES_FILE, renderChartSource, renderShapeValues } from "../../../../scripts/release/flux.ts";
import { ageTool } from "../../../../scripts/flux/age.ts";
import { renderPullSecret } from "../../../../scripts/flux/lib.ts";
import { FluxNamespace, type NamespaceApi, RUN_LABEL, existingFluxProblem, fluxCrdNames, issueOfNamespace, lockProblem, newRunId } from "./preflight.ts";

const ROOT = resolve(import.meta.dir, "..", "..", "..", "..");
const HOST = process.env.E2E_SSH_HOST ?? "";
const NS = process.env.E2E_NAMESPACE ?? "";
const OUT = resolve(process.env.E2E_OUT ?? "");
if (HOST === "" || !/^e2e-[1-9][0-9]*$/.test(NS) || process.env.E2E_OUT === undefined) {
  console.error("set E2E_SSH_HOST, E2E_NAMESPACE (e2e-<issue>) and E2E_OUT");
  process.exit(2);
}
/** The issue this run belongs to; the lab lock must name it (preflight.ts, lockProblem). */
const ISSUE = issueOfNamespace(NS);
/** Labels namespace flux-system, so that teardown removes Flux only when this run created it. */
const RUN_ID = newRunId();

// Throwaway servers, pinned by digest (multi-arch indexes; the lab is arm64).
const REGISTRY_IMAGE = "docker.io/library/registry:3.1.2@sha256:ddf754342cfc8acc51a56d5d0ab6af06826461864460636d8bd5c546dab2a7b8";
const SOFT_SERVE_IMAGE = "docker.io/charmcli/soft-serve:v0.9.1@sha256:c1baa7e942a6f9f06515705fabb58b721d8a7c71de700d8f2f95e9c951b10add";
const GIT_IMAGE = "docker.io/alpine/git:v2.52.0@sha256:4a0e72d49596a1f5d3701aeedafdadc5c0da4062be4657c7bdc4017387f591cc";

const REGISTRY_HOST = `registry.${NS}.svc.cluster.local:5000`;
const LOCAL_PORT = 15_070;
const LOCAL_REGISTRY = `localhost:${LOCAL_PORT}`;
const GIT_URL = `ssh://git@soft-serve.${NS}.svc.cluster.local:23231/flux-e2e`;
const REPOSITORY = "Laysi/trustiot-v2";
const CHART_PATH = "charts/trustiot";
/** The interval of the GitRepository in deploy/flux; a deploy must finish within it plus this. */
const APPLY_SLACK_MS = 60_000;

const log: string[] = [];
const timeline: Record<string, unknown>[] = [];
function note(message: string): void {
  const line = `${new Date().toISOString()} ${message}`;
  console.log(line);
  log.push(line);
}

function shq(arg: string): string {
  return /^[A-Za-z0-9_./:=@%+,-]+$/.test(arg) ? arg : `'${arg.replaceAll("'", `'\\''`)}'`;
}

/** kubectl on the lab host. */
/**
 * kubectl on the lab host over ssh. The host is reached through a tunnel that sometimes drops a
 * connection before the command starts (ssh exits with 255); those attempts are repeated.
 */
async function sshKubectl(args: string[], stdin?: string): Promise<{ exitCode: number; stdout: string; stderr: string }> {
  const command = ["sudo", "k3s", "kubectl", ...args].map(shq).join(" ");
  for (let attempt = 1; ; attempt += 1) {
    const result = await tryRun(["ssh", HOST, command], stdin === undefined ? {} : { stdin });
    if (result.exitCode !== 255 || attempt === 4) return result;
    note(`ssh dropped the connection (attempt ${attempt}); retrying`);
    await Bun.sleep(3_000 * attempt);
  }
}

async function kubectl(args: string[], stdin?: string): Promise<string> {
  const result = await sshKubectl(args, stdin);
  if (result.exitCode !== 0) {
    throw new Error(`kubectl ${args[0] ?? ""} exited with ${result.exitCode}: ${(result.stderr || result.stdout).trim().split("\n").slice(-5).join("\n")}`);
  }
  return result.stdout;
}

async function tryKubectl(args: string[]): Promise<{ code: number; out: string }> {
  const result = await sshKubectl(args);
  return { code: result.exitCode, out: result.stdout + result.stderr };
}

async function waitFor<T>(what: string, check: () => Promise<T | undefined>, timeoutMs: number, everyMs = 3_000): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await check().catch(() => undefined);
    if (value !== undefined) return value;
    if (Date.now() > deadline) throw new Error(`timed out after ${timeoutMs / 1000} s waiting for ${what}`);
    await Bun.sleep(everyMs);
  }
}

function sleep(ms: number): Promise<void> {
  return Bun.sleep(ms);
}

// --- Cluster state --------------------------------------------------------------------------

const CLUSTER_KINDS = [
  "namespaces",
  "customresourcedefinitions.apiextensions.k8s.io",
  "clusterroles.rbac.authorization.k8s.io",
  "clusterrolebindings.rbac.authorization.k8s.io",
  "validatingwebhookconfigurations.admissionregistration.k8s.io",
  "mutatingwebhookconfigurations.admissionregistration.k8s.io",
  "apiservices.apiregistration.k8s.io",
  "priorityclasses.scheduling.k8s.io",
  "storageclasses.storage.k8s.io",
];

/**
 * The cluster-scoped objects, without the fields the API server changes on its own
 * (resourceVersion, managedFields, generation, status). uid and creationTimestamp stay: an
 * object deleted and created again does not compare equal.
 */
async function clusterState(): Promise<Record<string, { names: string[]; sha256: string }>> {
  const state: Record<string, { names: string[]; sha256: string }> = {};
  for (const kind of CLUSTER_KINDS) {
    const list = JSON.parse(await kubectl(["get", kind, "-o", "json"])) as { items: Record<string, unknown>[] };
    const items = list.items
      .map((item) => {
        const metadata = { ...(item.metadata as Record<string, unknown>) };
        delete metadata.resourceVersion;
        delete metadata.managedFields;
        delete metadata.generation;
        const { status: _status, ...rest } = item;
        return { ...rest, metadata };
      })
      .sort((a, b) => String(a.metadata.name).localeCompare(String(b.metadata.name)));
    const text = JSON.stringify(items);
    state[kind] = {
      names: items.map((item) => String(item.metadata.name)),
      sha256: new Bun.CryptoHasher("sha256").update(text).digest("hex"),
    };
  }
  return state;
}

/** Pods and workloads of the namespaces the test must not touch, with their uids. */
async function protectedState(): Promise<string[]> {
  const out = await kubectl(["get", "pods,deployments,statefulsets,daemonsets,services", "-A", "-o", "jsonpath={range .items[*]}{.kind}/{.metadata.namespace}/{.metadata.name}/{.metadata.uid}{\"\\n\"}{end}"]);
  return out
    .split("\n")
    .filter((line) => line !== "" && !line.includes(`/${NS}/`) && !line.includes("/flux-system/"))
    .sort();
}

// --- Fixtures -------------------------------------------------------------------------------

interface Fixtures {
  dir: string;
  ageKey: string;
  agePublic: string;
  cosignPassword: string;
  registryUser: string;
  registryPassword: string;
  sshAdmin: string;
  sshReader: string;
}

async function makeFixtures(): Promise<Fixtures> {
  const dir = mkdtempSync(join(tmpdir(), "trustiot-flux-e2e-"));
  const ageKey = join(dir, "age.agekey");
  await run([await ageTool("age-keygen"), "-o", ageKey]);
  const agePublic = (await run([await ageTool("age-keygen"), "-y", ageKey])).stdout.trim();
  const cosign = await protoTool("cosign");
  const cosignPassword = crypto.randomUUID();
  for (const prefix of ["trusted", "other"]) {
    await run([cosign, "generate-key-pair", "--output-key-prefix", join(dir, prefix)], { env: { COSIGN_PASSWORD: cosignPassword } });
  }
  // TLS for the registry from a throwaway CA: the in-cluster name for source-controller, and
  // localhost for helm and cosign here, through the tunnel.
  const openssl = (args: string[]) => run(["openssl", ...args], { cwd: dir });
  await openssl(["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", "ca.key", "-out", "ca.crt", "-days", "1", "-subj", "/CN=flux-e2e-ca"]);
  await openssl(["req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", "tls.key", "-out", "tls.csr", "-subj", "/CN=registry"]);
  writeFileSync(join(dir, "san.ext"), `subjectAltName=DNS:registry.${NS}.svc.cluster.local,DNS:registry.${NS}.svc,DNS:localhost,IP:127.0.0.1\n`);
  await openssl(["x509", "-req", "-in", "tls.csr", "-CA", "ca.crt", "-CAkey", "ca.key", "-CAcreateserial", "-out", "tls.crt", "-days", "1", "-extfile", "san.ext"]);
  const registryUser = "flux-e2e";
  const registryPassword = crypto.randomUUID();
  writeFileSync(join(dir, "htpasswd"), `${registryUser}:${await Bun.password.hash(registryPassword, { algorithm: "bcrypt", cost: 10 })}\n`);
  for (const name of ["admin", "reader"]) {
    await run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", `flux-e2e-${name}`, "-f", join(dir, `ssh-${name}`)]);
  }
  return {
    dir,
    ageKey,
    agePublic,
    cosignPassword,
    registryUser,
    registryPassword,
    sshAdmin: join(dir, "ssh-admin"),
    sshReader: join(dir, "ssh-reader"),
  };
}

/** A Secret manifest built here with the pinned kubectl, then applied over ssh's stdin. */
async function secretManifest(namespace: string, name: string, files: Record<string, string>, type = "generic"): Promise<string> {
  const kubectlLocal = await protoTool("kubectl");
  const args =
    type === "tls"
      ? ["create", "secret", "tls", name, `--cert=${files.cert}`, `--key=${files.key}`]
      : ["create", "secret", "generic", name, ...Object.entries(files).map(([key, path]) => `--from-file=${key}=${path}`)];
  return (await run([kubectlLocal, ...args, "-n", namespace, "--dry-run=client", "-o", "yaml"])).stdout;
}

function servers(fixtures: Fixtures): string {
  const adminKey = readFileSync(`${fixtures.sshAdmin}.pub`, "utf8").trim();
  return `
apiVersion: apps/v1
kind: Deployment
metadata: {name: registry, namespace: ${NS}}
spec:
  replicas: 1
  selector: {matchLabels: {app: registry}}
  template:
    metadata: {labels: {app: registry}}
    spec:
      containers:
        - name: registry
          image: ${REGISTRY_IMAGE}
          env:
            - {name: REGISTRY_HTTP_TLS_CERTIFICATE, value: /tls/tls.crt}
            - {name: REGISTRY_HTTP_TLS_KEY, value: /tls/tls.key}
            - {name: REGISTRY_AUTH, value: htpasswd}
            - {name: REGISTRY_AUTH_HTPASSWD_REALM, value: flux-e2e}
            - {name: REGISTRY_AUTH_HTPASSWD_PATH, value: /auth/htpasswd}
            - {name: OTEL_TRACES_EXPORTER, value: none}
          ports: [{containerPort: 5000}]
          readinessProbe: {tcpSocket: {port: 5000}}
          resources: {requests: {cpu: 20m, memory: 32Mi}, limits: {memory: 256Mi}}
          volumeMounts:
            - {name: tls, mountPath: /tls, readOnly: true}
            - {name: auth, mountPath: /auth, readOnly: true}
      volumes:
        - {name: tls, secret: {secretName: registry-tls}}
        - {name: auth, secret: {secretName: registry-auth}}
---
apiVersion: v1
kind: Service
metadata: {name: registry, namespace: ${NS}}
spec:
  selector: {app: registry}
  ports: [{port: 5000, targetPort: 5000}]
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: soft-serve, namespace: ${NS}}
spec:
  replicas: 1
  selector: {matchLabels: {app: soft-serve}}
  template:
    metadata: {labels: {app: soft-serve}}
    spec:
      containers:
        - name: soft-serve
          image: ${SOFT_SERVE_IMAGE}
          env:
            - {name: SOFT_SERVE_INITIAL_ADMIN_KEYS, value: ${JSON.stringify(adminKey)}}
          ports: [{containerPort: 23231}]
          readinessProbe: {tcpSocket: {port: 23231}}
          resources: {requests: {cpu: 20m, memory: 32Mi}, limits: {memory: 256Mi}}
          volumeMounts: [{name: data, mountPath: /soft-serve}]
      volumes: [{name: data, emptyDir: {}}]
---
apiVersion: v1
kind: Service
metadata: {name: soft-serve, namespace: ${NS}}
spec:
  selector: {app: soft-serve}
  ports: [{name: ssh, port: 23231, targetPort: 23231}]
---
apiVersion: v1
kind: Pod
metadata: {name: git-client, namespace: ${NS}}
spec:
  containers:
    - name: git
      image: ${GIT_IMAGE}
      command: [sh, -c, "sleep 86400"]
      env: [{name: HOME, value: /work/home}]
      resources: {requests: {cpu: 10m, memory: 16Mi}, limits: {memory: 128Mi}}
      volumeMounts:
        - {name: work, mountPath: /work}
        - {name: key, mountPath: /keys, readOnly: true}
  volumes:
    - {name: work, emptyDir: {}}
    - {name: key, secret: {secretName: soft-serve-admin, defaultMode: 0400}}
`;
}

// --- The test chart and the e2e repository --------------------------------------------------

function writeTestChart(dir: string): void {
  mkdirSync(join(dir, "templates"), { recursive: true });
  writeFileSync(join(dir, "Chart.yaml"), "apiVersion: v2\nname: trustiot\nversion: 0.0.0\ntype: application\n");
  writeFileSync(
    join(dir, "templates", "configmap.yaml"),
    [
      "apiVersion: v1",
      "kind: ConfigMap",
      "metadata:",
      "  name: flux-e2e",
      "data:",
      "  chartVersion: {{ .Chart.Version | quote }}",
      "  shape: {{ .Values.shape | quote }}",
      "  instance: {{ .Values.instance | quote }}",
      "",
    ].join("\n"),
  );
}

/** chart.yaml as release.yml renders it, with the e2e registry and the test key instead of keyless. */
function e2eChartSource(version: string, digest: string): string {
  const production = renderChartSource({ version, digest, repository: REPOSITORY, chartRepository: `${REGISTRY_HOST}/${CHART_PATH}` });
  const keyed = production.replace(
    /  # Keyless cosign signature[^\n]*\n  verify:\n(?: {4,}.*\n)+$/,
    [
      "  # e2e: the throwaway registry's CA, and the test cosign key instead of keyless.",
      "  certSecretRef:",
      "    name: registry-ca",
      "  verify:",
      "    provider: cosign",
      "    secretRef:",
      "      name: cosign-pub",
      "",
    ].join("\n"),
  );
  if (keyed === production) throw new Error("the e2e chart source did not replace the keyless verification");
  return keyed.replaceAll(`namespace: trustiot`, `namespace: ${NS}`);
}

/** The deploy/flux tree as the lab's git server serves it. */
async function buildRepository(dir: string, fixtures: Fixtures, chart: { version: string; digest: string }): Promise<void> {
  mkdirSync(join(dir, "deploy"), { recursive: true });
  cpSync(join(ROOT, "deploy", "flux"), join(dir, "deploy", "flux"), { recursive: true });
  rmSync(join(dir, "deploy", "flux", "tests"), { recursive: true, force: true });
  const edit = (path: string, change: (text: string) => string) => {
    const file = join(dir, path);
    const before = readFileSync(file, "utf8");
    const after = change(before);
    if (after === before) throw new Error(`e2e edit of ${path} changed nothing`);
    writeFileSync(file, after);
  };
  edit(`${CLUSTER_DIR}/flux-system/gotk-sync.yaml`, (text) => text.replace(/url: ssh:\/\/git@github\.com\/.*\n/, `url: ${GIT_URL}\n`));
  // Flux does not own the test namespace: drop namespace.yaml and move everything into it.
  edit(`${APP_DIR}/kustomization.yaml`, (text) =>
    text
      .replace("  - namespace.yaml\n", "")
      .replace("  - helm-release.yaml\n", "  - helm-release.yaml\n  - ghcr-pull.sops.yaml\n")
      .replace("namespace: trustiot", `namespace: ${NS}`),
  );
  rmSync(join(dir, APP_DIR, "namespace.yaml"));
  edit(`${APP_DIR}/helm-release.yaml`, (text) => text.replace("namespace: trustiot", `namespace: ${NS}`));
  writeFileSync(join(dir, APP_DIR, "values-instance.yaml"), "instance: from-values-instance\n");
  writeDeploy(dir, chart);
  // The pull Secret, encrypted for the cluster's (test) age key the way .sops.yaml says.
  const sops = await protoTool("sops");
  // Run outside the repository, so the recipients come from --age and not from .sops.yaml.
  const encrypted = await run(
    [
      sops,
      "encrypt",
      "--age",
      fixtures.agePublic,
      "--encrypted-regex",
      "^(data|stringData)$",
      "--input-type",
      "yaml",
      "--output-type",
      "yaml",
      "--filename-override",
      "ghcr-pull.sops.yaml",
    ],
    { cwd: fixtures.dir, stdin: renderPullSecret(NS, REGISTRY_HOST, fixtures.registryUser, fixtures.registryPassword) },
  );
  writeFileSync(join(dir, APP_DIR, "ghcr-pull.sops.yaml"), encrypted.stdout);
}

/** What a deploy PR changes: the chart source and the shape values of the release. */
function writeDeploy(dir: string, chart: { version: string; digest: string }): void {
  writeFileSync(join(dir, CHART_SOURCE_FILE), e2eChartSource(chart.version, chart.digest));
  writeFileSync(join(dir, SHAPE_VALUES_FILE), renderShapeValues(chart.version, `shape: from-values-saas-${chart.version}\n`));
}

/** Runs a shell script in the git client pod (the scripts hold no secrets). */
async function inGit(script: string, stdin?: string): Promise<string> {
  return kubectl(["-n", NS, "exec", ...(stdin === undefined ? [] : ["-i"]), "git-client", "--", "sh", "-euc", script], stdin);
}

const GIT_SSH = "ssh -i /keys/id -o IdentitiesOnly=yes -o UserKnownHostsFile=/work/home/known_hosts -o StrictHostKeyChecking=yes";

/** Copies `dir` over the git client's work tree, commits and pushes; returns the commit. */
async function pushTree(dir: string, message: string): Promise<string> {
  const tar = Bun.spawnSync(["tar", "-C", dir, "-cf", "-", "."]);
  if (tar.exitCode !== 0) throw new Error("tar failed");
  await inGit(
    "find /work/repo -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} + && base64 -d | tar -C /work/repo -xf -",
    Buffer.from(tar.stdout).toString("base64"),
  );
  return (
    await inGit(
      `cd /work/repo && git add -A && git commit -q -m ${shq(message)} && GIT_SSH_COMMAND=${shq(GIT_SSH)} git push -q origin HEAD:main && git rev-parse HEAD`,
    )
  ).trim();
}

async function gitRevert(): Promise<string> {
  return (
    await inGit(`cd /work/repo && git revert --no-edit HEAD >/dev/null && GIT_SSH_COMMAND=${shq(GIT_SSH)} git push -q origin HEAD:main && git rev-parse HEAD`)
  ).trim();
}

// --- Observations ---------------------------------------------------------------------------

/**
 * The chart version the release runs. Flux installs a chart pulled by digest with the digest
 * as build metadata (`0.0.1+<12 hex digits>`); this returns the version without it.
 */
async function deployedVersion(): Promise<string | undefined> {
  return (await deployedChart())?.split("+")[0];
}

async function deployedChart(): Promise<string | undefined> {
  const result = await tryKubectl(["-n", NS, "get", "configmap", "flux-e2e", "-o", "jsonpath={.data.chartVersion}"]);
  return result.code === 0 ? result.out.trim() : undefined;
}

async function resource(kind: string, namespace: string, name: string): Promise<Record<string, unknown> | undefined> {
  const result = await tryKubectl(["-n", namespace, "get", kind, name, "-o", "json"]);
  return result.code === 0 ? (JSON.parse(result.out) as Record<string, unknown>) : undefined;
}

function condition(object: Record<string, unknown> | undefined, type: string): { status?: string; reason?: string; message?: string } | undefined {
  const conditions = ((object?.status as { conditions?: { type: string }[] } | undefined)?.conditions ?? []) as { type: string; status?: string; reason?: string; message?: string }[];
  return conditions.find((c) => c.type === type);
}

/** Waits until the release runs `version` from `commit`; returns the elapsed time. */
async function waitForDeploy(version: string, commit: string, since: number, step: string): Promise<number> {
  const timeoutMs = 60_000 + APPLY_SLACK_MS * 2;
  await waitFor(`${step}: chart ${version} deployed`, async () => ((await deployedVersion()) === version ? true : undefined), timeoutMs, 2_000);
  const elapsed = Date.now() - since;
  const release = await resource("helmrelease", NS, "trustiot");
  const git = await resource("gitrepository", "flux-system", "flux-system");
  const sync = await resource("kustomization", "flux-system", "flux-system");
  const history = ((release?.status as { history?: { chartVersion: string; status: string; version: number }[] })?.history ?? []).map(
    (h) => `${h.version}:${h.chartVersion}:${h.status}`,
  );
  const entry = {
    step,
    version,
    chart: await deployedChart(),
    commit,
    elapsedSeconds: Math.round(elapsed / 100) / 10,
    gitRevision: (git?.status as { artifact?: { revision?: string } })?.artifact?.revision,
    appliedRevision: (sync?.status as { lastAppliedRevision?: string })?.lastAppliedRevision,
    helmRelease: condition(release, "Ready"),
    helmHistory: history,
  };
  timeline.push(entry);
  note(`${step}: ${version} running ${entry.elapsedSeconds} s after the push (git ${entry.gitRevision})`);
  if (!String(entry.gitRevision).endsWith(commit)) throw new Error(`${step}: Flux reports git ${entry.gitRevision}, pushed ${commit}`);
  return elapsed;
}

// --- Main -----------------------------------------------------------------------------------

const results: { name: string; passed: boolean; detail: string }[] = [];
function check(name: string, passed: boolean, detail: string): void {
  results.push({ name, passed, detail });
  note(`${passed ? "PASS" : "FAIL"} ${name}: ${detail}`);
}

mkdirSync(OUT, { recursive: true });
const fixtures = await makeFixtures();
const work = join(fixtures.dir, "work");
let tunnel: Bun.Subprocess | undefined;
let fluxManifests = "";
let before: Awaited<ReturnType<typeof clusterState>> | undefined;
let protectedBefore: string[] = [];
let failure: unknown;
// The test namespace, if this run created it. Teardown touches only that, so a namespace that
// already existed (a leftover run) is never patched or deleted. The flag is set just before the
// create: the preflight proved the namespace absent and the lab lock keeps other runs out, so a
// create whose reply is lost (dropped ssh) is still cleaned up.
const created = { test: false };
// flux-system and, with it, Flux's cluster-wide objects (CRDs, ClusterRoles, ClusterRoleBindings):
// this run's only when it created the namespace with RUN_ID, and torn down only while the label
// still says so (preflight.ts, FluxNamespace). A Flux someone installed meanwhile, such as the demo
// environment's, is never deleted.
const fluxNamespace = new FluxNamespace(RUN_ID);
const fluxNamespaceApi: NamespaceApi = {
  // One attempt only: a retry after a lost answer would report AlreadyExists for our own create.
  create: async (manifest) => {
    const result = await tryRun(["ssh", HOST, "sudo k3s kubectl create -f -"], { stdin: manifest });
    return { exitCode: result.exitCode, output: `${result.stdout}${result.stderr}` };
  },
  runLabel: async () => {
    const result = await sshKubectl(["get", "namespace", "flux-system", "-o", `jsonpath={.metadata.labels.${RUN_LABEL.replaceAll(".", "\\.")}}`]);
    const label = result.stdout.trim();
    return result.exitCode === 0 && label !== "" ? label : undefined;
  },
};

/** Whether a namespace exists; anything other than found or NotFound is an error. */
async function namespaceExists(name: string): Promise<boolean> {
  const result = await tryKubectl(["get", "namespace", name, "-o", "name"]);
  if (result.code === 0) return true;
  if (/\(NotFound\)/.test(result.out)) return false;
  throw new Error(`cannot tell whether namespace ${name} exists: ${result.out.trim().split("\n").at(-1)}`);
}

/** Stops the run unless the lab lock is held for this run's issue (preflight.ts, lockProblem). */
async function requireLabLock(): Promise<void> {
  const owner = await tryRun(["ssh", HOST, "cat ~/e2e.lock/owner"]);
  const problem = lockProblem(owner.exitCode === 0 ? owner.stdout : undefined, ISSUE);
  if (problem !== undefined) throw new Error(problem);
}

/**
 * Stops the run when the cluster has Flux. Teardown deletes Flux and its CRDs, so a cluster that
 * already runs Flux (on the lab: the demo environment, deploy/flux/README.md 「展示叢集」) is not
 * ours to test on (preflight.ts).
 */
async function requireNoFlux(): Promise<void> {
  const fluxProblem = existingFluxProblem({
    fluxSystemNamespace: await namespaceExists("flux-system"),
    fluxCrds: fluxCrdNames(await kubectl(["get", "crd", "-o", "name"])),
  });
  if (fluxProblem !== undefined) throw new Error(fluxProblem);
}

try {
  await requireLabLock();
  note(`the lab lock is held for ${ISSUE}`);
  const versions = await kubectl(["version", "-o", "json"]);
  note(`cluster ${(JSON.parse(versions) as { serverVersion: { gitVersion: string } }).serverVersion.gitVersion}`);
  await requireNoFlux();
  if (await namespaceExists(NS)) throw new Error(`${NS} already exists; refusing to touch it`);
  before = await clusterState();
  protectedBefore = await protectedState();
  note(`recorded ${Object.values(before).reduce((n, kind) => n + kind.names.length, 0)} cluster-scoped objects and ${protectedBefore.length} workloads elsewhere`);

  // Test namespace, servers, Secrets.
  created.test = true;
  await kubectl(["create", "namespace", NS]);
  const secrets = [
    await secretManifest(NS, "registry-tls", { cert: join(fixtures.dir, "tls.crt"), key: join(fixtures.dir, "tls.key") }, "tls"),
    await secretManifest(NS, "registry-auth", { htpasswd: join(fixtures.dir, "htpasswd") }),
    await secretManifest(NS, "registry-ca", { "ca.crt": join(fixtures.dir, "ca.crt") }),
    await secretManifest(NS, "cosign-pub", { "cosign.pub": join(fixtures.dir, "trusted.pub") }),
    await secretManifest(NS, "soft-serve-admin", { id: fixtures.sshAdmin }),
  ];
  await kubectl(["apply", "-f", "-"], secrets.join("\n---\n"));
  await kubectl(["apply", "-f", "-"], servers(fixtures));
  await kubectl(["-n", NS, "wait", "--for=condition=available", "deployment/registry", "deployment/soft-serve", "--timeout=300s"]);
  await kubectl(["-n", NS, "wait", "--for=condition=ready", "pod/git-client", "--timeout=300s"]);
  note("registry, soft-serve and the git client are ready");

  // Charts: 0.0.1 and 0.0.2 signed with the trusted key, 0.0.3 with another key.
  const registryIp = (await kubectl(["-n", NS, "get", "service", "registry", "-o", "jsonpath={.spec.clusterIP}"])).trim();
  tunnel = Bun.spawn(["ssh", "-N", "-o", "ExitOnForwardFailure=yes", "-L", `127.0.0.1:${LOCAL_PORT}:${registryIp}:5000`, HOST], { stdout: "ignore", stderr: "inherit" });
  const helm = await protoTool("helm");
  const cosign = await protoTool("cosign");
  const env = {
    HELM_REGISTRY_CONFIG: join(fixtures.dir, "helm-registry.json"),
    HELM_CACHE_HOME: join(fixtures.dir, "helm-cache"),
    DOCKER_CONFIG: join(fixtures.dir, "docker"),
    COSIGN_PASSWORD: fixtures.cosignPassword,
  };
  await waitFor(
    "the registry tunnel",
    async () =>
      (await tryRun([helm, "registry", "login", LOCAL_REGISTRY, "--username", fixtures.registryUser, "--password-stdin", "--ca-file", join(fixtures.dir, "ca.crt")], { env, stdin: fixtures.registryPassword })).exitCode === 0
        ? true
        : undefined,
    60_000,
  );
  await run([cosign, "login", LOCAL_REGISTRY, "--username", fixtures.registryUser, "--password-stdin"], { env, stdin: fixtures.registryPassword });
  const chartDir = join(fixtures.dir, "chart");
  writeTestChart(chartDir);
  const charts: Record<string, string> = {};
  for (const [version, key] of [["0.0.1", "trusted"], ["0.0.2", "trusted"], ["0.0.3", "other"]] as const) {
    await run([helm, "package", chartDir, "--version", version, "--destination", fixtures.dir], { env });
    const pushed = await run([helm, "push", join(fixtures.dir, `trustiot-${version}.tgz`), `oci://${LOCAL_REGISTRY}/charts`, "--ca-file", join(fixtures.dir, "ca.crt")], { env });
    const digest = /Digest:\s*(sha256:[0-9a-f]{64})/.exec(`${pushed.stdout}${pushed.stderr}`)?.[1];
    if (digest === undefined) throw new Error("helm push printed no digest");
    await run(
      [cosign, "sign", "--yes", "--key", join(fixtures.dir, `${key}.key`), "--use-signing-config=false", "--tlog-upload=false", "--registry-cacert", join(fixtures.dir, "ca.crt"), `${LOCAL_REGISTRY}/${CHART_PATH}@${digest}`],
      { env },
    );
    charts[version] = digest;
    note(`chart ${version} → ${digest}, signed with the ${key} key`);
  }
  const anonymous = await tryRun(["curl", "-sS", "-o", "/dev/null", "-w", "%{http_code}", "--cacert", join(fixtures.dir, "ca.crt"), `https://${LOCAL_REGISTRY}/v2/${CHART_PATH}/manifests/0.0.1`]);
  check("the registry refuses anonymous pulls", anonymous.stdout.trim() === "401", `HTTP ${anonymous.stdout.trim()}`);

  // The repository on the git server.
  const known = await inGit(`mkdir -p /work/home /work/repo && ssh-keyscan -p 23231 soft-serve.${NS}.svc.cluster.local 2>/dev/null | tee /work/home/known_hosts`);
  if (!known.includes("ssh-")) throw new Error("ssh-keyscan found no host key on soft-serve");
  writeFileSync(join(fixtures.dir, "known_hosts"), known);
  await inGit(
    // The tree is copied in with this machine's file owner; git refuses such a repository otherwise.
    "git config --global --add safe.directory /work/repo && cd /work/repo && git init -q -b main && git config user.email e2e@flux.test && git config user.name flux-e2e",
  );
  mkdirSync(work, { recursive: true });
  await buildRepository(work, fixtures, { version: "0.0.1", digest: charts["0.0.1"]! });
  await inGit(`cd /work/repo && git remote add origin ${GIT_URL}`);
  const first = await pushTree(work, "deploy 0.0.1");
  note(`pushed the e2e repository at ${first}`);

  // Bootstrap as deploy/flux/README.md does: namespace, the two Secrets, then Flux from the tree.
  // Minutes have passed since the first checks (servers, charts, signing): check the lock and the
  // absence of Flux again right before installing, then create flux-system as this run's.
  await requireLabLock();
  await requireNoFlux();
  await fluxNamespace.claim(fluxNamespaceApi);
  note(`created namespace flux-system with ${RUN_LABEL}=${RUN_ID}`);
  await kubectl(
    ["apply", "-f", "-"],
    [
      await secretManifest("flux-system", "flux-system", { identity: fixtures.sshReader, "identity.pub": `${fixtures.sshReader}.pub`, known_hosts: join(fixtures.dir, "known_hosts") }),
      await secretManifest("flux-system", "sops-age", { "age.agekey": fixtures.ageKey }),
    ].join("\n---\n"),
  );
  fluxManifests = (await run([await protoTool("kubectl"), "kustomize", join(work, CLUSTER_DIR, "flux-system")])).stdout;
  const bootstrapStart = Date.now();
  // The first apply creates the CRDs; the custom resources in the same stream need them established.
  await kubectl(["apply", "--server-side", "--force-conflicts", "-f", "-"], fluxManifests).catch(async () => {
    await kubectl(["wait", "--for=condition=established", "crd", "--all", "--timeout=120s"]);
    return kubectl(["apply", "--server-side", "--force-conflicts", "-f", "-"], fluxManifests);
  });
  await kubectl(["-n", "flux-system", "wait", "--for=condition=available", "deployment", "--all", "--timeout=300s"]);
  note("Flux controllers are available");
  await waitForDeploy("0.0.1", first, bootstrapStart, "bootstrap");
  const pullSecret = await resource("secret", NS, "ghcr-pull");
  check(
    "the SOPS-encrypted pull Secret is decrypted into the cluster",
    pullSecret?.type === "kubernetes.io/dockerconfigjson",
    `Secret ${NS}/ghcr-pull type ${String(pullSecret?.type)}`,
  );
  const shape = (await kubectl(["-n", NS, "get", "configmap", "flux-e2e", "-o", "jsonpath={.data.shape} {.data.instance}"])).trim();
  check("values-saas.yaml and values-instance.yaml reach the chart, in that order", shape === "from-values-saas-0.0.1 from-values-instance", shape);

  // 1. Merge a deploy commit: 0.0.2.
  writeDeploy(work, { version: "0.0.2", digest: charts["0.0.2"]! });
  let pushedAt = Date.now();
  const bump = await pushTree(work, "chore(flux): deploy trustiot 0.0.2");
  const bumpMs = await waitForDeploy("0.0.2", bump, pushedAt, "deploy");
  check("a merged deploy commit is running within the interval", bumpMs <= 60_000 + APPLY_SLACK_MS, `${(bumpMs / 1000).toFixed(1)} s (GitRepository interval 60 s)`);

  // 2. Revert it: back to 0.0.1.
  pushedAt = Date.now();
  const revert = await gitRevert();
  const revertMs = await waitForDeploy("0.0.1", revert, pushedAt, "revert");
  check("reverting the deploy commit rolls back within the interval", revertMs <= 60_000 + APPLY_SLACK_MS, `${(revertMs / 1000).toFixed(1)} s`);
  writeDeploy(work, { version: "0.0.1", digest: charts["0.0.1"]! });

  // 3. A chart signed by another key is refused; 0.0.1 keeps running.
  writeDeploy(work, { version: "0.0.3", digest: charts["0.0.3"]! });
  pushedAt = Date.now();
  const rogue = await pushTree(work, "deploy a chart with a foreign signature");
  const refused = await waitFor(
    "the OCIRepository to refuse 0.0.3",
    async () => {
      const source = await resource("ocirepository", NS, "trustiot");
      const ready = condition(source, "Ready");
      const spec = source?.spec as { ref?: { tag?: string } } | undefined;
      return spec?.ref?.tag === "0.0.3" && ready?.status === "False" ? ready : undefined;
    },
    180_000,
  );
  await sleep(30_000);
  const still = await deployedVersion();
  timeline.push({ step: "foreign signature", commit: rogue, elapsedSeconds: Math.round((Date.now() - pushedAt) / 1000), source: refused, running: still });
  check(
    "a chart with a foreign signature is not installed",
    /verif/i.test(`${refused.reason} ${refused.message}`) && still === "0.0.1",
    `OCIRepository Ready=False (${refused.reason}: ${String(refused.message).slice(0, 160)}); running ${still}`,
  );
  pushedAt = Date.now();
  const recover = await gitRevert();
  await waitFor(
    "the OCIRepository to be ready on 0.0.1 again",
    async () => {
      const source = await resource("ocirepository", NS, "trustiot");
      return (source?.spec as { ref?: { tag?: string } })?.ref?.tag === "0.0.1" && condition(source, "Ready")?.status === "True" ? true : undefined;
    },
    180_000,
  );
  note(`recovered at ${recover} after ${Math.round((Date.now() - pushedAt) / 1000)} s; running ${await deployedVersion()}`);
} catch (error) {
  failure = error;
  note(`ERROR ${error instanceof Error ? error.message : String(error)}`);
} finally {
  // Teardown in the order Flux needs: stop syncing, uninstall the release while helm-controller
  // runs, delete the custom resources while their controllers remove the finalizers, then Flux.
  note("teardown");
  // Flux is removed only while flux-system still carries this run's label: the label is read again
  // here, so a Flux that replaced ours (or a namespace someone relabelled) is left alone.
  const fluxOurs = await fluxNamespace.mayTearDown(fluxNamespaceApi);
  if (fluxNamespace.owned && !fluxOurs) {
    note(`ERROR flux-system no longer carries ${RUN_LABEL}=${RUN_ID}: Flux, its CRDs and the namespace are left in place for a person to check`);
    check("teardown removed only this run's Flux", false, "flux-system is not this run's any more; nothing of Flux was deleted");
  }
  const steps: [string, boolean, string[]][] = [
    ["suspend the sync, no pruning", fluxOurs, ["-n", "flux-system", "patch", "kustomization", "flux-system", "--type=merge", "-p", '{"spec":{"suspend":true,"prune":false}}']],
    ["uninstall the release", created.test && fluxOurs, ["-n", NS, "delete", "helmrelease", "--all", "--wait", "--timeout=180s"]],
    ["delete the sources", created.test && fluxOurs, ["-n", NS, "delete", "ocirepository", "--all", "--wait", "--timeout=120s"]],
    ["delete the sync", fluxOurs, ["-n", "flux-system", "delete", "kustomization,gitrepository", "--all", "--wait", "--timeout=120s"]],
  ];
  for (const [what, ours, args] of steps) {
    if (!ours) continue;
    const result = await tryKubectl(args);
    note(`${what}: ${result.code === 0 ? "done" : `exit ${result.code} ${result.out.trim().split("\n").at(-1)}`}`);
  }
  if (fluxOurs && fluxManifests !== "") {
    const removed = await sshKubectl(["delete", "--ignore-not-found", "--wait", "--timeout=300s", "-f", "-"], fluxManifests);
    note(`delete Flux: exit ${removed.exitCode}`);
  }
  for (const namespace of [...(created.test ? [NS] : []), ...(fluxOurs ? ["flux-system"] : [])]) {
    const result = await tryKubectl(["delete", "namespace", namespace, "--ignore-not-found", "--wait", "--timeout=300s"]);
    note(`delete namespace ${namespace}: exit ${result.code}`);
  }
  tunnel?.kill();
  if (before !== undefined) {
    const after = await clusterState();
    const protectedAfter = await protectedState();
    const differences = Object.keys(before).filter((kind) => before![kind]!.sha256 !== after[kind]!.sha256);
    const diffNames = Object.fromEntries(
      differences.map((kind) => [
        kind,
        {
          added: after[kind]!.names.filter((n) => !before![kind]!.names.includes(n)),
          removed: before![kind]!.names.filter((n) => !after[kind]!.names.includes(n)),
        },
      ]),
    );
    const workloadsChanged = protectedBefore.filter((line) => !protectedAfter.includes(line)).concat(protectedAfter.filter((line) => !protectedBefore.includes(line)));
    writeFileSync(
      join(OUT, "cluster-state.json"),
      `${JSON.stringify({ before, after, differences, diffNames, workloadsElsewhere: { before: protectedBefore.length, after: protectedAfter.length, changed: workloadsChanged } }, null, 2)}\n`,
    );
    check("cluster-scoped objects are identical after teardown", differences.length === 0, differences.length === 0 ? `${CLUSTER_KINDS.length} kinds, same sha256` : JSON.stringify(diffNames));
    check("no workload outside the test was touched", workloadsChanged.length === 0, `${protectedAfter.length} workloads, ${workloadsChanged.length} changed`);
  }
  writeFileSync(join(OUT, "timeline.json"), `${JSON.stringify({ results, timeline }, null, 2)}\n`);
  writeFileSync(join(OUT, "log.txt"), `${log.join("\n")}\n`);
  rmSync(fixtures.dir, { recursive: true, force: true });
}

const failed = results.filter((r) => !r.passed);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
if (failure !== undefined || failed.length > 0) {
  process.exit(1);
}
