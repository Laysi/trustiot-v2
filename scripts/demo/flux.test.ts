// The rules and the runs of scripts/demo/flux.ts without ssh (Laysi/trustiot-v2#484): every
// refusal, the plan and confirmation flow against a fake cluster, the root scripts run with bash
// against a fake k3s (the private keys must never reach their output), and the commands boxOps
// sends (no Secret is ever read whole).
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { appendFileSync, chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import type { RunResult } from "../release/exec.ts";
import { appDir, renderChartSource } from "../release/flux.ts";
import { CHART_REGISTRY } from "../release/lib.ts";
import { sopsFileProblems } from "../flux/lib.ts";
import { Box, type CommonOptions, DEMO_MIN_CHART_VERSION, Log, ROOT, sha256, shq } from "./lib.ts";
import {
  type ClusterFacts,
  type CodeFacts,
  type DeployKeyFacts,
  type FluxManifests,
  type FluxOps,
  INGRESS_CLUSTER_IP_PATH,
  SERVICES_JSONPATH,
  type Inputs,
  KNOWN_HOSTS_FILE,
  MARKED_DEPLOY_KEY,
  MARKED_INSTALL,
  MARKER,
  type NamespaceState,
  POLL_MS,
  RECONCILE_ANNOTATION,
  REMOTE_BASE,
  type RepoInputs,
  SECRET_TEMPLATE,
  type SopsAgeState,
  WAITS,
  ageCheckScript,
  applyScript,
  boxOps,
  codeProblems,
  deleteScript,
  deployKeyProblem,
  deployKeyScript,
  deploymentReadiness,
  fluxReadiness,
  ipv4InCidr,
  foreignFluxObjects,
  isIpv4,
  lockProblem,
  namespaceVerdict,
  parseAgeCheck,
  parseFluxObjects,
  parseManifests,
  parseSecrets,
  parseServiceCidrs,
  parseServices,
  planInstall,
  readCode,
  readRepo,
  readyAt,
  redact,
  releaseReadiness,
  requiredSecrets,
  revisionOf,
  runDeployKey,
  runInstall,
  runRemove,
  runStatus,
  sopsAgeProblem,
  sshFingerprint,
  unexpectedNamespaceObjects,
  wantedRevisions,
} from "./flux.ts";

const DEMO = appDir("demo");
const SAAS_FLUX = "deploy/flux/clusters/saas/flux-system";
const COMPONENTS = readFileSync(join(ROOT, SAAS_FLUX, "gotk-components.yaml"), "utf8");
const SYNC = readFileSync(join(ROOT, SAAS_FLUX, "gotk-sync.yaml"), "utf8");
/** What `kubectl kustomize deploy/flux/clusters/demo/flux-system` renders, without kubectl: the demo patch applied by hand. */
const DEMO_MANIFESTS = `${COMPONENTS}\n---\n${SYNC.replace("path: ./deploy/flux/clusters/saas", "path: ./deploy/flux/clusters/demo")}`;
const build = async () => DEMO_MANIFESTS;
/** Kind to CRD name, as main's manifests define them. */
const CRD_KINDS = parseManifests(DEMO_MANIFESTS).manifests?.crdKinds ?? {};
const CLEAN: CodeFacts = { head: "a".repeat(40), originMain: "a".repeat(40), dirty: [], originUrl: "git@github.com:Laysi/trustiot-v2.git" };
const DIGEST = `sha256:${"b".repeat(64)}`;
const CLUSTER_IP = "10.43.0.50";
/** A fake age identity of the real length, built at run time so that gitleaks' age-secret-key rule does not report the source. */
const PRIVATE_AGE_KEY = `AGE-SECRET-KEY-1${"Q".repeat(58)}`;

const dirs: string[] = [];
function tempDir(prefix: string): string {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  dirs.push(dir);
  return dir;
}
afterEach(() => {
  for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

// --- A copy of the repository files the script reads ---------------------------------------------

const FILES = [
  KNOWN_HOSTS_FILE,
  ".sops.yaml",
  `${DEMO}/chart.yaml`,
  `${DEMO}/helm-release.yaml`,
  `${DEMO}/kustomization.yaml`,
  `${DEMO}/values-demo.yaml`,
  `${DEMO}/values-saas.yaml`,
  `${DEMO}/trustiot-demo-values.sops.yaml`,
];

/** main as it is (2026-10-21): no ghcr-pull.sops.yaml, the placeholder chart, no ingress ClusterIP. */
function mainAsIs(): string {
  const root = tempDir("demo-flux-repo-");
  for (const file of FILES) {
    mkdirSync(dirname(join(root, file)), { recursive: true });
    copyFileSync(join(ROOT, file), join(root, file));
  }
  return root;
}

function pinChart(root: string, version: string): void {
  writeFileSync(join(root, DEMO, "chart.yaml"), renderChartSource({ version, digest: DIGEST, repository: "Laysi/trustiot-v2", chartRepository: `${CHART_REGISTRY}/trustiot` }));
}

/** A ghcr-pull.sops.yaml of the right shape, made from the committed SOPS file (the checks never decrypt). */
function addPullSecret(root: string, list = true): void {
  const source = readFileSync(join(ROOT, DEMO, "trustiot-demo-values.sops.yaml"), "utf8").split("\n");
  const value = (source.find((line) => line.startsWith("    app-host: ")) ?? "").replace("    app-host: ", "");
  const out: string[] = [];
  let skipping = false;
  for (const line of source) {
    if (line === "stringData:") {
      out.push("data:", `    .dockerconfigjson: ${value}`);
      skipping = true;
      continue;
    }
    if (skipping && line.startsWith("    ")) continue;
    skipping = false;
    out.push(line.replace("    name: trustiot-demo-values", "    name: ghcr-pull").replace("type: Opaque", "type: kubernetes.io/dockerconfigjson"));
  }
  writeFileSync(join(root, DEMO, "ghcr-pull.sops.yaml"), out.join("\n"));
  if (list) {
    const file = join(root, DEMO, "kustomization.yaml");
    writeFileSync(file, readFileSync(file, "utf8").replace("  - trustiot-demo-values.sops.yaml\n", "  - trustiot-demo-values.sops.yaml\n  - ghcr-pull.sops.yaml\n"));
  }
}

function setClusterIp(root: string, value: string): void {
  appendFileSync(join(root, DEMO, "values-demo.yaml"), `\ningress:\n  tunnel:\n    clusterIP: ${value}\n`);
}

/** main as the install needs it: a chart after the minimum, the ClusterIP pinned, the pull Secret on main. */
function mainReady(version = "0.0.0-rc.9"): string {
  const root = mainAsIs();
  pinChart(root, version);
  setClusterIp(root, CLUSTER_IP);
  addPullSecret(root);
  return root;
}

// --- Redaction and small parsers ---------------------------------------------------------------------

describe("redact", () => {
  test("removes private keys, tokens and long base64", () => {
    const pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----";
    expect(redact(`before ${pem} after`)).toBe("before <redacted private key> after");
    expect(redact(`key ${PRIVATE_AGE_KEY}`)).toBe("key <redacted age key>");
    expect(redact(`token eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc`)).not.toContain("eyJ");
    expect(redact(`blob ${"A".repeat(100)}`)).toBe("blob <redacted>");
  });

  test("replaces host names and addresses that are not public names or cluster addresses", () => {
    expect(redact("hostnames[0]: Invalid value: \"app.example.com\"")).toBe('hostnames[0]: Invalid value: "<host>"');
    expect(redact("from TrustIoT <no-reply@mail.example.com>")).toBe("from TrustIoT <no-reply@<host>>");
    expect(redact("dial 192.0.2.10:443 and 10.43.0.50:80 and 10.42.0.7")).toBe("dial <address>:443 and 10.43.0.50:80 and 10.42.0.7");
    const kept = "unable to clone 'ssh://git@github.com/Laysi/trustiot-v2.git'; oci://ghcr.io/laysi/trustiot-v2/charts/trustiot; helmreleases.helm.toolkit.fluxcd.io";
    expect(redact(kept)).toBe(kept);
  });

  test("fails closed: any dotted name is a host unless it is public, in the cluster's DNS, a file or an API group", () => {
    for (const name of ["db.internal", "box.lan", "nas.local", "printer.home.arpa", "DB.Internal", "git.corp", "github.com.internal", "xn--bcher-kva.example", "db.internal.yaml", "app.example.com.json"]) {
      expect(redact(`dial ${name}:5432`)).toBe("dial <host>:5432");
    }
    expect(redact("lookup db.internal.svc.example on 10.43.0.10:53")).toBe("lookup <host> on 10.43.0.10:53");
    expect(redact("release sh.helm.release.v1.trustiot.v1")).toBe("release <host>.v1");
    const kept = [
      "Get https://source-controller.flux-system.svc.cluster.local./x",
      "kubernetes.default.svc",
      "stat deploy/flux/clusters/demo/trustiot/kustomization.yaml",
      "trustiot-demo-values.sops.yaml",
      "trustiot-0.0.0-rc.9.tgz",
      "deployment.apps/source-controller, job.batch/trustiot-migrate, configmap/kube-root-ca.crt",
      "keys identity, identity.pub, known_hosts; age.agekey",
      "Helm install succeeded for release trustiot/trustiot.v1 with chart trustiot@0.0.0-rc.9+bbbbbbbbbbbb",
      `main@sha1:${CLEAN.head}, ${DIGEST}, at 2026-10-21T13:07:10.226Z, k3s v1.36.5+k3s1`,
    ].join("\n");
    expect(redact(kept)).toBe(kept);
  });

  test("replaces every IPv6 address but loopback", () => {
    expect(redact("dial [fe80::1]:53, 2001:db8::5 and 2001:0db8:0000:0000:0000:ff00:0042:8329.")).toBe("dial [<address>]:53, <address> and <address>.");
    expect(redact("from ::ffff:192.0.2.1 to fd00::")).toBe("from ::ffff:<address> to <address>");
    expect(redact("listen on [::1]:8080")).toBe("listen on [::1]:8080");
    const kept = `13:07:10.226Z sha256:${"b".repeat(64)} aa:bb:cc:dd:ee:ff std::io`;
    expect(redact(kept)).toBe(kept);
  });
});

describe("small parsers", () => {
  test("isIpv4 accepts dotted quads only", () => {
    for (const ok of ["10.43.0.50", "0.0.0.0", "255.255.255.255"]) expect(isIpv4(ok)).toBe(true);
    for (const bad of ["10.43.0.256", "10.43.0", "10.43.0.050", " 10.43.0.5", "fd00::1", "ten.0.0.1"]) expect(isIpv4(bad)).toBe(false);
  });

  test("parseSecrets reads name, type, marker and key names", () => {
    const out = "sops-age\tOpaque\t<no value>\tage.agekey,\nflux-system\tOpaque\tdeploy-key\tknown_hosts,identity,identity.pub,\n";
    expect(parseSecrets(out)).toEqual([
      { name: "flux-system", type: "Opaque", marker: MARKED_DEPLOY_KEY, keys: ["identity", "identity.pub", "known_hosts"] },
      { name: "sops-age", type: "Opaque", marker: "", keys: ["age.agekey"] },
    ]);
  });

  test("SECRET_TEMPLATE prints key names and never a value", () => {
    expect(SECRET_TEMPLATE).toContain("{{range $k, $v := .data}}{{$k}},{{end}}");
    expect(SECRET_TEMPLATE.match(/\{\{[^}]*\$v[^}]*\}\}/g)).toEqual(["{{range $k, $v := .data}}"]);
  });

  test("sshFingerprint matches ssh-keygen -l", () => {
    const dir = tempDir("demo-flux-key-");
    const keygen = Bun.spawnSync(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "test", "-f", join(dir, "key")]);
    expect(keygen.exitCode).toBe(0);
    const listed = Bun.spawnSync(["ssh-keygen", "-l", "-E", "sha256", "-f", join(dir, "key.pub")]).stdout.toString().split(" ")[1];
    expect(sshFingerprint(readFileSync(join(dir, "key.pub"), "utf8"))).toBe(listed!);
    expect(() => sshFingerprint("not a key")).toThrow("not an OpenSSH public key");
    const blob = readFileSync(join(dir, "key.pub"), "utf8").split(" ")[1]!;
    expect(() => sshFingerprint(`ssh-rsa ${blob} test`)).toThrow("not an OpenSSH public key");
    expect(() => sshFingerprint(`ssh-ed25519 ${blob.slice(0, 8)} test`)).toThrow("not an OpenSSH public key");
  });

  test("parseAgeCheck reads each state and fails closed", () => {
    expect(parseAgeCheck("SOPS_AGE ok age1abc age1def\n")).toEqual({ kind: "ok", recipients: ["age1abc", "age1def"] });
    expect(parseAgeCheck("SOPS_AGE missing\n")).toEqual({ kind: "missing" });
    expect(parseAgeCheck("SOPS_AGE keys a b\n")).toEqual({ kind: "keys", keys: ["a", "b"] });
    expect(parseAgeCheck("SOPS_AGE arch x86_64\n")).toEqual({ kind: "arch", arch: "x86_64" });
    expect(parseAgeCheck("SOPS_AGE ok\n").kind).toBe("unreadable");
    expect(parseAgeCheck("SOPS_AGE ok AGE-SECRET-KEY-1X\n").kind).toBe("unreadable");
    expect(parseAgeCheck("something else").kind).toBe("unreadable");
  });
});

// --- The checkout -----------------------------------------------------------------------------------

describe("the checkout", () => {
  test("codeProblems accepts a clean, fetched origin/main of the repository on GitHub", () => {
    expect(codeProblems(CLEAN)).toEqual([]);
    for (const url of ["https://github.com/Laysi/trustiot-v2.git", "ssh://git@github.com/Laysi/trustiot-v2", "https://github.com/laysi/TrustIoT-v2/"]) {
      expect(codeProblems({ ...CLEAN, originUrl: url })).toEqual([]);
    }
  });

  test("codeProblems refuses a fork, another commit, uncommitted changes and a failed fetch", () => {
    expect(codeProblems({ ...CLEAN, originUrl: "git@github.com:someone/trustiot-v2.git" })[0]).toContain("is not Laysi/trustiot-v2 on GitHub");
    expect(codeProblems({ ...CLEAN, originUrl: "https://github.com/Laysi/trustiot-v2-fork.git" })).toHaveLength(1);
    expect(codeProblems({ ...CLEAN, head: "c".repeat(40) })[0]).toContain("is not origin/main");
    expect(codeProblems({ ...CLEAN, dirty: [" M scripts/demo/flux.ts"] })[0]).toContain("uncommitted changes");
    expect(codeProblems({ ...CLEAN, fetchError: "exit 128" })[0]).toContain("git fetch origin main failed");
  });

  test("readCode fetches main and sees a moved origin and uncommitted files", async () => {
    const base = tempDir("demo-flux-git-");
    const git = (cwd: string, ...args: string[]) => {
      const result = Bun.spawnSync(["git", "-c", "user.email=t@example.com", "-c", "user.name=t", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main", ...args], { cwd });
      if (result.exitCode !== 0) throw new Error(result.stderr.toString());
      return result.stdout.toString().trim();
    };
    git(base, "init", "--bare", "origin.git");
    git(base, "clone", join(base, "origin.git"), "work");
    const work = join(base, "work");
    mkdirSync(join(work, "scripts"));
    writeFileSync(join(work, "scripts", "a.ts"), "a\n");
    git(work, "add", ".");
    git(work, "commit", "-m", "one");
    git(work, "push", "origin", "HEAD:main");
    const head = git(work, "rev-parse", "HEAD");
    expect(await readCode(work)).toEqual({ head, originMain: head, dirty: [], originUrl: join(base, "origin.git") });

    writeFileSync(join(work, "scripts", "b.ts"), "b\n");
    expect((await readCode(work)).dirty).toEqual(["?? scripts/b.ts"]);
    rmSync(join(work, "scripts", "b.ts"));

    git(base, "clone", join(base, "origin.git"), "other");
    writeFileSync(join(base, "other", "scripts", "a.ts"), "changed\n");
    git(join(base, "other"), "commit", "-am", "two");
    git(join(base, "other"), "push", "origin", "HEAD:main");
    const moved = await readCode(work);
    expect(moved.head).toBe(head);
    expect(moved.originMain).toBe(git(join(base, "other"), "rev-parse", "HEAD"));
    expect(codeProblems({ ...moved, originUrl: CLEAN.originUrl })[0]).toContain("is not origin/main");

    git(work, "remote", "set-url", "origin", join(base, "missing.git"));
    expect((await readCode(work)).fetchError).toBeDefined();
  });
});

// --- Flux's manifests and what main needs -----------------------------------------------------------

describe("parseManifests", () => {
  test("the fixture is what the demo's flux-system directory renders: the SaaS Flux with the demo path", () => {
    // kubectl is not on every runner that runs these tests, so DEMO_MANIFESTS applies the demo
    // patch by hand; this pins the patch to the committed kustomization (bun test deploy/flux
    // builds the real one).
    const demo = Bun.YAML.parse(readFileSync(join(ROOT, "deploy/flux/clusters/demo/flux-system/kustomization.yaml"), "utf8")) as Record<string, unknown>;
    expect(demo.resources).toEqual(["../../saas/flux-system"]);
    const patches = demo.patches as { target: unknown; patch: string }[];
    expect(patches).toHaveLength(1);
    expect(patches[0]!.target).toEqual({ group: "kustomize.toolkit.fluxcd.io", kind: "Kustomization", name: "flux-system" });
    expect(Bun.YAML.parse(patches[0]!.patch)).toEqual([{ op: "replace", path: "/spec/path", value: "./deploy/flux/clusters/demo" }]);
    expect(Object.keys(demo).sort()).toEqual(["apiVersion", "kind", "patches", "resources"]);
    const saas = Bun.YAML.parse(readFileSync(join(ROOT, SAAS_FLUX, "kustomization.yaml"), "utf8")) as Record<string, unknown>;
    expect(saas.resources).toEqual(["gotk-components.yaml", "gotk-sync.yaml"]);
    expect(SYNC.split("path: ./deploy/flux/clusters/saas")).toHaveLength(2);
  });

  test("reads the demo cluster's Flux", () => {
    const { manifests, problems } = parseManifests(DEMO_MANIFESTS);
    expect(problems).toEqual([]);
    expect(manifests?.objects).toHaveLength(27);
    expect(manifests?.crds).toHaveLength(8);
    expect(manifests?.crds.every((crd) => crd.endsWith(".toolkit.fluxcd.io"))).toBe(true);
    expect(manifests?.crdKinds.HelmRelease).toBe("helmreleases.helm.toolkit.fluxcd.io");
    expect(manifests?.crdKinds.OCIRepository).toBe("ocirepositories.source.toolkit.fluxcd.io");
    expect(manifests?.deployments).toEqual(["helm-controller", "kustomize-controller", "source-controller"]);
    expect(manifests?.clusterObjects).toContain("clusterrolebinding/cluster-reconciler-flux-system");
    expect(manifests?.deployKeySecret).toBe("flux-system");
    expect(manifests?.sopsSecret).toBe("sops-age");
    expect(manifests?.sha256).toBe(sha256(DEMO_MANIFESTS));
  });

  test("refuses the SaaS tree", () => {
    const { manifests, problems } = parseManifests(`${COMPONENTS}\n---\n${SYNC}`);
    expect(manifests).toBeUndefined();
    expect(problems).toEqual(["the root Kustomization applies ./deploy/flux/clusters/saas, not ./deploy/flux/clusters/demo"]);
  });

  test("refuses objects it would not know how to remove", () => {
    const extra = (doc: string) => parseManifests(`${DEMO_MANIFESTS}\n---\n${doc}`).problems;
    expect(extra("apiVersion: v1\nkind: Secret\nmetadata:\n  name: x\n  namespace: flux-system\n")[0]).toContain("Secret");
    expect(extra("apiVersion: v1\nkind: ServiceAccount\nmetadata:\n  name: x\n  namespace: default\n")[0]).toContain("is not in namespace flux-system");
    expect(extra("apiVersion: apiextensions.k8s.io/v1\nkind: CustomResourceDefinition\nmetadata:\n  name: things.example.com\nspec:\n  names:\n    kind: Thing\n")[0]).toContain("not a Flux toolkit CRD");
    expect(extra("apiVersion: v1\nkind: Namespace\nmetadata:\n  name: other\n")).toContain("Flux's manifests declare namespace other");
    expect(parseManifests(DEMO_MANIFESTS.replace("ssh://git@github.com/Laysi/trustiot-v2.git", "ssh://git@github.com/other/repo.git")).problems[0]).toContain("does not read");
    expect(parseManifests("{").problems[0]).toContain("not YAML");
  });
});

describe("requiredSecrets", () => {
  const read = (file: string) => Bun.YAML.parse(readFileSync(join(ROOT, DEMO, file), "utf8")) as Record<string, unknown>;

  test("main's demo release needs the pull Secret and the private values, not the SMTP or AI keys", () => {
    const values = { ...read("values-saas.yaml"), ...read("values-demo.yaml") };
    const { secrets, problems } = requiredSecrets(read("chart.yaml"), read("helm-release.yaml"), values);
    expect(problems).toEqual([]);
    expect(Object.keys(secrets).sort()).toEqual(["ghcr-pull", "trustiot-demo-values"]);
  });

  test("a service that requests a credential needs its Secret; optional values and disabled services do not", () => {
    const values = {
      smtp: { credentialsSecret: { name: "trustiot-smtp" } },
      ai: { chat: { apiKeySecret: { name: "trustiot-anthropic" } } },
      services: { identity: { enabled: true, credentials: ["smtp"] }, assistant: { enabled: false, credentials: ["ai"] } },
    };
    const release = { spec: { valuesFrom: [{ kind: "Secret", name: "optional-one", optional: true }, { kind: "ConfigMap", name: "c" }] } };
    const { secrets, problems } = requiredSecrets({}, release, values);
    expect(problems).toEqual([]);
    expect(Object.keys(secrets)).toEqual(["trustiot-smtp"]);
    expect(requiredSecrets({}, {}, { services: { x: { enabled: true, credentials: ["unknown"] } } }).problems[0]).toContain("does not know");
  });
});

describe("readRepo", () => {
  test("main as it is today: the placeholder chart, no ClusterIP and no pull Secret are refused", async () => {
    const repo = await readRepo(mainAsIs(), CLEAN, build);
    expect(repo.code).toEqual([]);
    expect(repo.flux).toEqual([]);
    expect(repo.deploy).toHaveLength(3);
    expect(repo.deploy[0]).toContain(`older than ${DEMO_MIN_CHART_VERSION}`);
    expect(repo.deploy[1]).toContain(`does not set ${INGRESS_CLUSTER_IP_PATH.join(".")} to an IPv4 address (not set)`);
    expect(repo.deploy[1]).toContain("cannot change after the first deploy");
    expect(repo.deploy[1]).toContain("Laysi/trustiot-v2#478");
    expect(repo.deploy[2]).toContain("ghcr-pull.sops.yaml is not on main");
    expect(repo.demoRecipient).toMatch(/^age1/);
  });

  test("main ready for the install has no problems", async () => {
    const repo = await readRepo(mainReady(), CLEAN, build);
    expect([...repo.code, ...repo.flux, ...repo.deploy]).toEqual([]);
    expect(repo.chart).toEqual({ version: "0.0.0-rc.9", digest: DIGEST });
    expect(repo.clusterIp).toBe(CLUSTER_IP);
    expect(Object.keys(repo.secrets).sort()).toEqual(["ghcr-pull", "trustiot-demo-values"]);
    expect(repo.manifests?.objects).toHaveLength(27);
  });

  test("(a) orders release candidates by number: rc.10 is after rc.8, rc.7 is before", async () => {
    expect((await readRepo(mainReady("0.0.0-rc.10"), CLEAN, build)).deploy).toEqual([]);
    expect((await readRepo(mainReady(DEMO_MIN_CHART_VERSION), CLEAN, build)).deploy).toEqual([]);
    expect((await readRepo(mainReady("0.0.0-rc.7"), CLEAN, build)).deploy[0]).toContain("pins chart 0.0.0-rc.7, older than");
    expect((await readRepo(mainReady("0.0.1"), CLEAN, build)).deploy).toEqual([]);
  });

  test("(a) a hand-edited chart.yaml is refused", async () => {
    const root = mainReady();
    const file = join(root, DEMO, "chart.yaml");
    writeFileSync(file, readFileSync(file, "utf8").replace("  verify:\n", "  # verify:\n"));
    expect((await readRepo(root, CLEAN, build)).deploy[0]).toContain("edited by hand");
  });

  test("the ClusterIP must be a valid IPv4 address", async () => {
    for (const bad of ["10.43.0.256", "auto", '""']) {
      const root = mainAsIs();
      pinChart(root, "0.0.0-rc.9");
      addPullSecret(root);
      setClusterIp(root, bad);
      const deploy = (await readRepo(root, CLEAN, build)).deploy;
      expect(deploy).toHaveLength(1);
      expect(deploy[0]).toContain("to an IPv4 address (it is");
    }
  });

  test("(e) a needed SOPS file that kustomization.yaml does not list, or that is not encrypted, is refused", async () => {
    const unlisted = mainAsIs();
    pinChart(unlisted, "0.0.0-rc.9");
    setClusterIp(unlisted, CLUSTER_IP);
    addPullSecret(unlisted, false);
    expect((await readRepo(unlisted, CLEAN, build)).deploy).toEqual([`${DEMO}/kustomization.yaml does not list ghcr-pull.sops.yaml, so Flux does not apply Secret ghcr-pull (the OCIRepository pulls the chart with it)`]);

    const plain = mainReady();
    writeFileSync(join(plain, DEMO, "ghcr-pull.sops.yaml"), "apiVersion: v1\nkind: Secret\nmetadata:\n  name: ghcr-pull\n  namespace: trustiot\ntype: kubernetes.io/dockerconfigjson\ndata:\n  .dockerconfigjson: e30=\n");
    expect((await readRepo(plain, CLEAN, build)).deploy.join("\n")).toContain("is not a SOPS-encrypted Secret");

    const noValues = mainReady();
    rmSync(join(noValues, DEMO, "trustiot-demo-values.sops.yaml"));
    expect((await readRepo(noValues, CLEAN, build)).deploy[0]).toContain("trustiot-demo-values.sops.yaml is not on main");
  });

  test("the fixture's pull Secret has the shape the checks want", () => {
    const root = mainReady();
    expect(sopsFileProblems("demo", "ghcr-pull.sops.yaml", readFileSync(join(root, DEMO, "ghcr-pull.sops.yaml"), "utf8"))).toEqual([]);
  });

  test("a failed kustomize build and missing host keys are Flux problems", async () => {
    const root = mainReady();
    rmSync(join(root, KNOWN_HOSTS_FILE));
    const repo = await readRepo(root, CLEAN, async () => {
      throw new Error("boom");
    });
    expect(repo.manifests).toBeUndefined();
    expect(repo.flux).toEqual([`kubectl kustomize deploy/flux/clusters/demo/flux-system failed: boom`, `${KNOWN_HOSTS_FILE} has no ed25519 host key of github.com`]);
  });
});

// --- The cluster's state ------------------------------------------------------------------------------

const ageKeyNamespace = (overrides: Partial<NamespaceState> = {}): NamespaceState => ({
  uid: "uid-1",
  resourceVersion: "100",
  labels: { "kubernetes.io/metadata.name": "flux-system" },
  annotations: {},
  terminating: false,
  objects: [
    { ref: "configmap/kube-root-ca.crt", owned: false, managed: false },
    { ref: "configmap/istio-ca-root-cert", owned: false, managed: false },
    { ref: "serviceaccount/default", owned: false, managed: false },
    { ref: "secret/sops-age", owned: false, managed: false },
  ],
  secrets: [{ name: "sops-age", type: "Opaque", marker: "", keys: ["age.agekey"] }],
  ...overrides,
});

const withDeployKey = (ns: NamespaceState, marker = MARKED_DEPLOY_KEY): NamespaceState => ({
  ...ns,
  objects: [...ns.objects, { ref: "secret/flux-system", owned: false, managed: false }],
  secrets: [...ns.secrets, { name: "flux-system", type: "Opaque", marker, keys: ["identity", "identity.pub", "known_hosts"] }],
});

describe("namespaceVerdict", () => {
  test("absent, the age key step's, and with deploy-key's Secret", () => {
    expect(namespaceVerdict(undefined, "flux-system", "sops-age")).toEqual({ kind: "absent" });
    expect(namespaceVerdict(ageKeyNamespace(), "flux-system", "sops-age")).toEqual({ kind: "age-key", uid: "uid-1" });
    expect(namespaceVerdict(withDeployKey(ageKeyNamespace()), "flux-system", "sops-age")).toEqual({ kind: "age-key", uid: "uid-1" });
  });

  test("this script's install", () => {
    const ours = ageKeyNamespace({ labels: { [MARKER]: MARKED_INSTALL, "app.kubernetes.io/part-of": "flux" }, objects: [{ ref: "deployment.apps/source-controller", owned: false, managed: true }] });
    expect(namespaceVerdict(ours, "flux-system", "sops-age")).toEqual({ kind: "ours", uid: "uid-1" });
  });

  test("(d) anything else is foreign", () => {
    const foreign = (ns: NamespaceState) => {
      const verdict = namespaceVerdict(ns, "flux-system", "sops-age");
      expect(verdict.kind).toBe("foreign");
      return verdict.kind === "foreign" ? verdict.why.join("; ") : "";
    };
    expect(foreign(withDeployKey(ageKeyNamespace(), ""))).toContain("it holds secret/flux-system");
    expect(foreign(ageKeyNamespace({ objects: [...ageKeyNamespace().objects, { ref: "configmap/other", owned: false, managed: false }] }))).toContain("it holds configmap/other");
    expect(foreign(ageKeyNamespace({ objects: [{ ref: "deployment.apps/x", owned: false, managed: true }] }))).toContain("Helm or Flux manages");
    // Names from the cluster go through redact before they reach a refusal.
    const named = foreign(
      ageKeyNamespace({
        labels: { "kubernetes.io/metadata.name": "flux-system", "backup.example.com/target": "x" },
        annotations: { "db.internal/owner": "x" },
        objects: [...ageKeyNamespace().objects, { ref: "configmap/proxy.corp", owned: false, managed: false }],
      }),
    );
    expect(named).toContain("it has labels <host>/target; it has annotations <host>/owner; it holds configmap/<host>");
    expect(lockProblem({ kind: "held", owner: "Laysi/trustiot-v2#466 from box.lan 192.0.2.7" })).toContain("held by: Laysi/trustiot-v2#466 from <host> <address>;");
    expect(foreign(ageKeyNamespace({ labels: { team: "x" } }))).toContain("it has labels team");
    expect(foreign(ageKeyNamespace({ annotations: { note: "x" } }))).toContain("annotations note");
    expect(foreign(ageKeyNamespace({ terminating: true }))).toContain("being deleted");
    expect(foreign(ageKeyNamespace({ labels: { "e2e.trustiot.io/run": "abc" } }))).toContain("lab e2e");
    expect(foreign(ageKeyNamespace({ labels: { [MARKER]: MARKED_INSTALL, "e2e.trustiot.io/run": "abc" } }))).toContain("lab e2e");
    expect(foreign(ageKeyNamespace({ labels: { [MARKER]: "other" } }))).toContain(`${MARKER}=other`);
  });

  test("owned objects go with their owners and are not counted", () => {
    const ns = ageKeyNamespace({ objects: [...ageKeyNamespace().objects, { ref: "pod/x", owned: true, managed: false }] });
    expect(namespaceVerdict(ns, "flux-system", "sops-age").kind).toBe("age-key");
  });
});

describe("lockProblem", () => {
  test("(c) free or held for #467 only; #4670 is another issue", () => {
    expect(lockProblem({ kind: "free" })).toBeUndefined();
    expect(lockProblem({ kind: "held", owner: "Laysi/trustiot-v2#467 2026-10-21T00:00:00Z demo install" })).toBeUndefined();
    expect(lockProblem({ kind: "held", owner: "Laysi/trustiot-v2#4670 2026-10-21T00:00:00Z demo install" })).toContain("held by");
    expect(lockProblem({ kind: "held", owner: "Laysi/trustiot-v2#466 2026-10-21T00:00:00Z" })).toContain("held by");
    expect(lockProblem({ kind: "unreadable" })).toContain("cannot be read");
  });
});

describe("sopsAgeProblem", () => {
  const recipient = "age1recipient";
  test("(b) the Secret must hold exactly the demo cluster's key", () => {
    expect(sopsAgeProblem({ kind: "ok", recipients: [recipient] }, recipient, "sops-age")).toBeUndefined();
    expect(sopsAgeProblem({ kind: "ok", recipients: ["age1other"] }, recipient, "sops-age")).toContain("not the demo cluster's recipient");
    expect(sopsAgeProblem({ kind: "ok", recipients: [recipient, "age1other"] }, recipient, "sops-age")).toContain("not the demo cluster's recipient");
    for (const kind of ["missing", "empty", "invalid", "tool-mismatch"] as const) expect(sopsAgeProblem({ kind }, recipient, "sops-age")).toBeDefined();
    expect(sopsAgeProblem({ kind: "keys", keys: ["other"] }, recipient, "sops-age")).toContain("not exactly age.agekey");
    expect(sopsAgeProblem({ kind: "arch", arch: "x86_64" }, recipient, "sops-age")).toContain("not aarch64");
    expect(sopsAgeProblem({ kind: "unreadable", detail: "exit 1" }, recipient, "sops-age")).toContain("could not check");
  });
});

describe("deployKeyProblem", () => {
  const knownHosts = readFileSync(join(ROOT, KNOWN_HOSTS_FILE), "utf8");
  const key: DeployKeyFacts = { publicKey: "ssh-ed25519 AAAA test", knownHostsSha256: sha256(knownHosts) };
  test("(f) the deploy key Secret must be deploy-key's, complete and with main's host keys", () => {
    expect(deployKeyProblem(withDeployKey(ageKeyNamespace()), key, "flux-system", knownHosts)).toBeUndefined();
    expect(deployKeyProblem(ageKeyNamespace(), undefined, "flux-system", knownHosts)).toContain("run `bun scripts/demo/flux.ts deploy-key` first");
    expect(deployKeyProblem(withDeployKey(ageKeyNamespace(), ""), key, "flux-system", knownHosts)).toContain("was not made by deploy-key");
    const partial = withDeployKey(ageKeyNamespace());
    partial.secrets[1]!.keys = ["identity"];
    expect(deployKeyProblem(partial, key, "flux-system", knownHosts)).toContain("has the keys identity");
    expect(deployKeyProblem(withDeployKey(ageKeyNamespace()), { ...key, knownHostsSha256: sha256("old") }, "flux-system", knownHosts)).toContain("differs from main's");
  });
});

describe("readiness", () => {
  const object = (ready: string, extra: Record<string, unknown> = {}, observed = 1) => ({
    metadata: { generation: 1 },
    status: { observedGeneration: observed, conditions: [{ type: "Ready", status: ready, reason: "R", message: "dial app.example.com" }, ...((extra.conditions as unknown[]) ?? [])] },
  });
  test("a Flux object is ready for its current generation; Stalled ends the wait", () => {
    expect(fluxReadiness(undefined)).toEqual({ done: false, state: "not there yet" });
    expect(fluxReadiness(object("True")).done).toBe(true);
    expect(fluxReadiness(object("True", {}, 0)).done).toBe(false);
    expect(fluxReadiness(object("False")).state).toBe("Ready=False R: dial <host>");
    expect(fluxReadiness(object("False", { conditions: [{ type: "Stalled", status: "True", reason: "RetriesExceeded", message: "x" }] })).failed).toBe("Stalled RetriesExceeded: x");
  });
  test("a Deployment is ready when every replica of its generation is available", () => {
    const deployment = (available: number, observed = 1) => ({ metadata: { generation: 1 }, spec: { replicas: 1 }, status: { observedGeneration: observed, availableReplicas: available, updatedReplicas: available } });
    expect(deploymentReadiness(deployment(1)).done).toBe(true);
    expect(deploymentReadiness(deployment(0)).done).toBe(false);
    expect(deploymentReadiness(deployment(1, 0)).done).toBe(false);
  });

  const at = (status: Record<string, unknown>) => ({ ...object("True"), status: { ...object("True").status, ...status } });
  const wanted = wantedRevisions(CLEAN.head, { version: "0.0.0-rc.9", digest: DIGEST });

  test("wantedRevisions names main's commit, the chart's digest and the version helm-controller records", () => {
    expect(wanted).toEqual({ git: `main@sha1:${CLEAN.head}`, chartDigest: DIGEST, release: `0.0.0-rc.9+${"b".repeat(12)}` });
  });

  test("revisionOf reads each kind's revision, the chart digest from either form, and only a deployed release", () => {
    expect(revisionOf("git", at({ artifact: { revision: wanted.git } }))).toBe(wanted.git);
    expect(revisionOf("kustomization", at({ lastAppliedRevision: wanted.git }))).toBe(wanted.git);
    expect(revisionOf("chart", at({ artifact: { revision: DIGEST } }))).toBe(DIGEST);
    expect(revisionOf("chart", at({ artifact: { revision: `0.0.0-rc.9@${DIGEST}` } }))).toBe(DIGEST);
    expect(revisionOf("release", at({ history: [{ chartVersion: wanted.release, status: "deployed" }] }))).toBe(wanted.release);
    expect(revisionOf("release", at({ history: [{ chartVersion: wanted.release, status: "failed" }, { chartVersion: "0.0.0-rc.8+c", status: "superseded" }] }))).toBe("");
    for (const kind of ["git", "kustomization", "chart", "release"] as const) expect(revisionOf(kind, undefined)).toBe("");
  });

  test("readyAt needs Ready=True and the wanted revision", () => {
    expect(readyAt("git", at({ artifact: { revision: wanted.git } }), wanted.git).done).toBe(true);
    const old = readyAt("git", at({ artifact: { revision: `main@sha1:${"c".repeat(40)}` } }), wanted.git);
    expect(old).toEqual({ done: false, state: `Ready=True R: dial <host>; at main@sha1:${"c".repeat(40)}, waiting for ${wanted.git}` });
    expect(readyAt("kustomization", object("True"), wanted.git).state).toContain("at no revision yet");
    expect(readyAt("git", { ...object("False"), status: { ...object("False").status, artifact: { revision: wanted.git } } }, wanted.git).done).toBe(false);
  });

  test("releaseReadiness also needs the reconcile request of this run handled", () => {
    const deployed = { history: [{ chartVersion: wanted.release, status: "deployed" }] };
    expect(releaseReadiness(at({ ...deployed, lastHandledReconcileAt: "t2" }), wanted.release, "t2").done).toBe(true);
    expect(releaseReadiness(at({ ...deployed, lastHandledReconcileAt: "t1" }), wanted.release, "t2")).toEqual({ done: false, state: "Ready=True R: dial <host>; reconcile request t2 not handled yet" });
    expect(releaseReadiness(at({ history: [{ chartVersion: "0.0.0-rc.8+c", status: "deployed" }], lastHandledReconcileAt: "t2" }), wanted.release, "t2").done).toBe(false);
  });
});

describe("Flux objects", () => {
  test("parseFluxObjects reads the applying Kustomization from its labels and fails closed", () => {
    expect(parseFluxObjects("OCIRepository/trustiot/trustiot\tflux-system/flux-system\nHelmRelease/other/tool\t/\n\n")).toEqual([
      { ref: "HelmRelease/other/tool", appliedBy: "" },
      { ref: "OCIRepository/trustiot/trustiot", appliedBy: "flux-system/flux-system" },
    ]);
    expect(parseFluxObjects("HelmRelease/other/tool\tflux-system/\n")).toEqual([{ ref: "HelmRelease/other/tool", appliedBy: "" }]);
    expect(() => parseFluxObjects("HelmRelease/no-namespace\t/\n")).toThrow("unexpected Flux object line");
  });

  test("unexpectedNamespaceObjects allows the install and nothing else", () => {
    const manifests = parseManifests(DEMO_MANIFESTS).manifests!;
    expect(manifests.namespaceObjects).toContain("deployment.apps/source-controller");
    expect(manifests.namespaceObjects).toContain("kustomization.kustomize.toolkit.fluxcd.io/flux-system");
    expect(manifests.namespaceObjects).toContain("resourcequota/critical-pods-flux-system");
    expect(manifests.namespaceObjects).toHaveLength(13);
    const installed = withDeployKey(
      ageKeyNamespace({
        labels: { [MARKER]: MARKED_INSTALL },
        objects: [
          ...ageKeyNamespace().objects,
          ...manifests.namespaceObjects.map((ref) => ({ ref, owned: false, managed: true })),
          { ref: "lease.coordination.k8s.io/source-controller-leader-election", owned: false, managed: false },
          { ref: "pod/source-controller-abc", owned: true, managed: false },
          { ref: "endpointslice.discovery.k8s.io/source-controller-x1", owned: true, managed: false },
        ],
      }),
    );
    expect(unexpectedNamespaceObjects(installed, manifests)).toEqual([]);
    const extra = { ...installed, objects: [...installed.objects, { ref: "configmap/notes", owned: false, managed: false }, { ref: "lease.coordination.k8s.io/other", owned: false, managed: false }] };
    expect(unexpectedNamespaceObjects(extra, manifests)).toEqual(["configmap/notes", "lease.coordination.k8s.io/other"]);
    // A deploy key Secret that deploy-key did not make is not part of the install.
    expect(unexpectedNamespaceObjects(withDeployKey(ageKeyNamespace(), ""), manifests)).toEqual(["secret/flux-system"]);
  });

  test("foreignFluxObjects keeps the root sync objects and what the root Kustomization applied", () => {
    expect(
      foreignFluxObjects([
        { ref: "GitRepository/flux-system/flux-system", appliedBy: "" },
        { ref: "Kustomization/flux-system/flux-system", appliedBy: "" },
        { ref: "HelmRelease/trustiot/trustiot", appliedBy: "flux-system/flux-system" },
        { ref: "HelmRelease/other/tool", appliedBy: "" },
        { ref: "HelmRelease/other/sub", appliedBy: "flux-system/apps" },
        { ref: "GitRepository/other/flux-system", appliedBy: "" },
      ]),
    ).toEqual(["HelmRelease/other/tool", "HelmRelease/other/sub", "GitRepository/other/flux-system"]);
  });
});

// --- The plan ------------------------------------------------------------------------------------------

const DEPLOY_KEY_PUB = (() => {
  const dir = mkdtempSync(join(tmpdir(), "demo-flux-pub-"));
  try {
    Bun.spawnSync(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "trustiot-demo-flux", "-f", join(dir, "key")]);
    return readFileSync(join(dir, "key.pub"), "utf8").trim();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
})();

function readyCluster(repo: RepoInputs): ClusterFacts {
  return {
    namespace: withDeployKey(ageKeyNamespace()),
    fluxCrds: [],
    fluxClusterObjects: [],
    rootKustomization: false,
    poc: [],
    missingPlatformCrds: [],
    deployKey: { publicKey: DEPLOY_KEY_PUB, knownHostsSha256: sha256(repo.knownHosts) },
    serviceCidrs: ["10.43.0.0/16"],
    services: [
      { ref: "default/kubernetes", ips: ["10.43.0.1"], release: "/" },
      { ref: "kube-system/kube-dns", ips: ["10.43.0.10"], release: "/" },
    ],
  };
}

describe("planInstall", () => {
  let repo: RepoInputs;
  let okAge: SopsAgeState;
  beforeEach(async () => {
    repo = await readRepo(mainReady(), CLEAN, build);
    okAge = { kind: "ok", recipients: [repo.demoRecipient!] };
  });

  test("everything in place: label, apply, no refusals", () => {
    const plan = planInstall(repo, readyCluster(repo), { kind: "free" }, okAge);
    expect(plan.refusals).toEqual([]);
    expect(plan).toMatchObject({ namespace: "age-key", namespaceUid: "uid-1", mark: true, apply: true, clusterIp: CLUSTER_IP, deployKeyFingerprint: sshFingerprint(DEPLOY_KEY_PUB) });
  });

  test("a re-run over this script's install does not label or apply again once Flux manages itself", () => {
    const cluster = readyCluster(repo);
    cluster.namespace = withDeployKey(ageKeyNamespace({ labels: { [MARKER]: MARKED_INSTALL } }));
    cluster.fluxCrds = repo.manifests!.crds;
    cluster.rootKustomization = true;
    cluster.appNamespace = { labels: { "kustomize.toolkit.fluxcd.io/name": "flux-system", "kustomize.toolkit.fluxcd.io/namespace": "flux-system" }, terminating: false };
    const plan = planInstall(repo, cluster, { kind: "free" }, okAge);
    expect(plan.refusals).toEqual([]);
    expect(plan).toMatchObject({ namespace: "ours", mark: false, apply: false, notes: [] });
  });

  const refusalsWith = (change: (cluster: ClusterFacts) => void, lock: Parameters<typeof planInstall>[2] = { kind: "free" }, age: SopsAgeState = okAge) => {
    const cluster = readyCluster(repo);
    change(cluster);
    return planInstall(repo, cluster, lock, age).refusals;
  };

  test("(a), (e) and the ClusterIP: main's problems are refusals", async () => {
    const asIs = await readRepo(mainAsIs(), CLEAN, build);
    const refusals = planInstall(asIs, readyCluster(asIs), { kind: "free" }, okAge).refusals;
    expect(refusals).toHaveLength(3);
    expect(refusals.join("\n")).toContain("older than");
    expect(refusals.join("\n")).toContain("ghcr-pull.sops.yaml is not on main");
    expect(refusals.join("\n")).toContain(INGRESS_CLUSTER_IP_PATH.join("."));
  });

  test("(b) the sops-age Secret", () => {
    expect(refusalsWith(() => {}, { kind: "free" }, { kind: "missing" })).toEqual([`Secret flux-system/sops-age does not exist: make it first (deploy/flux/README.md 「展示叢集的 age 金鑰」)`]);
    expect(refusalsWith(() => {}, { kind: "free" }, { kind: "ok", recipients: ["age1other"] })[0]).toContain("not the demo cluster's recipient");
    expect(refusalsWith(() => {}, { kind: "free" }, { kind: "invalid" })).toHaveLength(1);
  });

  test("(c) the lab lock", () => {
    expect(refusalsWith(() => {}, { kind: "held", owner: "Laysi/trustiot-v2#4670 2026-10-21T00:00:00Z" })[0]).toContain("held by");
    expect(refusalsWith(() => {}, { kind: "held", owner: "Laysi/trustiot-v2#467 2026-10-21T00:00:00Z demo install" })).toEqual([]);
  });

  test("(d) flux-system", () => {
    expect(refusalsWith((c) => delete c.namespace)[0]).toContain("does not exist");
    expect(refusalsWith((c) => (c.namespace = ageKeyNamespace({ labels: { owner: "x" } })))[0]).toContain("neither by this script nor by the age key step");
  });

  test("(f) the deploy key and a Flux that is not this script's", () => {
    expect(
      refusalsWith((c) => {
        c.namespace = ageKeyNamespace();
        delete c.deployKey;
      }),
    ).toEqual(["Secret flux-system/flux-system (the deploy key) does not exist: run `bun scripts/demo/flux.ts deploy-key` first and add its public key on GitHub as a read-only deploy key"]);
    expect(refusalsWith((c) => (c.fluxCrds = ["gitrepositories.source.toolkit.fluxcd.io"]))[0]).toContain("Flux CRDs exist");
    expect(refusalsWith((c) => (c.fluxClusterObjects = ["clusterrole/crd-controller-flux-system"]))[0]).toContain("cluster-wide objects exist");
  });

  test("the PoC, a namespace trustiot that is not Flux's, and missing platform CRDs", () => {
    expect(refusalsWith((c) => (c.poc = ["trustiot/deployment.apps/app"]))[0]).toContain("cleanup.ts");
    expect(refusalsWith((c) => (c.appNamespace = { labels: {}, terminating: false }))[0]).toContain("is not Flux's");
    expect(refusalsWith((c) => (c.appNamespace = { labels: {}, terminating: true }))[0]).toContain("being deleted");
    expect(refusalsWith((c) => (c.missingPlatformCrds = ["clusters.postgresql.cnpg.io"]))[0]).toContain("platform CRDs");
    const kept = readyCluster(repo);
    kept.appNamespace = { labels: { "kustomize.toolkit.fluxcd.io/name": "flux-system", "kustomize.toolkit.fluxcd.io/namespace": "flux-system" }, terminating: false };
    const plan = planInstall(repo, kept, { kind: "free" }, okAge);
    expect(plan.refusals).toEqual([]);
    expect(plan.notes[0]).toContain("left from an earlier install");
  });

  test("the pinned ClusterIP must be inside the cluster's Service CIDRs and free, or the release's own", () => {
    const value = `ingress.tunnel.clusterIP ${CLUSTER_IP} of values-demo.yaml`;
    expect(refusalsWith((c) => (c.serviceCidrs = ["10.96.0.0/12"]))).toEqual([
      `${value} is outside the cluster's Service CIDRs (10.96.0.0/12): the API server would refuse the ingress Service and the first install would fail; pin an address inside them (Laysi/trustiot-v2#478)`,
    ]);
    expect(refusalsWith((c) => delete c.serviceCidrs)[0]).toContain(`could not read the cluster's IPv4 Service CIDRs (ServiceCIDR objects, networking.k8s.io/v1), so ${value}`);
    expect(refusalsWith((c) => (c.serviceCidrs = []))).toHaveLength(1);
    expect(refusalsWith((c) => c.services.push({ ref: "lab/proxy", ips: ["10.43.0.9", CLUSTER_IP], release: "/" }))).toEqual([
      `${value} is already the ClusterIP of Service lab/proxy: the release could not create its ingress Service with it; pin another address (Laysi/trustiot-v2#478)`,
    ]);
    // Another release, or the right release name in another namespace, is someone else's.
    expect(refusalsWith((c) => c.services.push({ ref: "trustiot/other", ips: [CLUSTER_IP], release: "trustiot/other" }))).toHaveLength(1);
    expect(refusalsWith((c) => c.services.push({ ref: "e2e-1/trustiot-ingress", ips: [CLUSTER_IP], release: "trustiot/trustiot" }))).toHaveLength(1);
    expect(refusalsWith((c) => c.services.push({ ref: "trustiot/trustiot-ingress", ips: [CLUSTER_IP], release: "trustiot/trustiot" }))).toEqual([]);
  });
});

describe("Service addresses", () => {
  test("parseServices and parseServiceCidrs read the lines boxOps asks for", () => {
    expect(parseServices("default/kubernetes\t10.43.0.1\t/\nlab/dual\t10.43.0.7 fd00::7\ttrustiot/trustiot\nlab/name\t\t/\n\n")).toEqual([
      { ref: "default/kubernetes", ips: ["10.43.0.1"], release: "/" },
      { ref: "lab/dual", ips: ["10.43.0.7", "fd00::7"], release: "trustiot/trustiot" },
      { ref: "lab/name", ips: [], release: "/" },
    ]);
    expect(() => parseServices("no-namespace\t10.43.0.1\t/\n")).toThrow("unexpected Service line");
    expect(parseServiceCidrs("10.43.0.0/16 fd00:43::/112 10.44.0.0/33 300.1.0.0/16\n")).toEqual(["10.43.0.0/16"]);
    expect(parseServiceCidrs("")).toEqual([]);
  });

  test("ipv4InCidr", () => {
    expect(ipv4InCidr("10.43.0.50", "10.43.0.0/16")).toBe(true);
    expect(ipv4InCidr("10.43.255.255", "10.43.0.0/16")).toBe(true);
    expect(ipv4InCidr("10.44.0.0", "10.43.0.0/16")).toBe(false);
    expect(ipv4InCidr("10.42.0.1", "10.43.0.0/16")).toBe(false);
    expect(ipv4InCidr("192.0.2.1", "0.0.0.0/0")).toBe(true);
    expect(ipv4InCidr("10.43.0.50", "10.43.0.50/32")).toBe(true);
    expect(ipv4InCidr("10.43.0.51", "10.43.0.50/32")).toBe(false);
  });
});

// --- Root scripts with bash and a fake k3s ---------------------------------------------------------

/** A directory with fake programs that shadow the real ones on PATH. */
function fakeBin(programs: Record<string, string>): string {
  const bin = tempDir("demo-flux-bin-");
  for (const [name, body] of Object.entries(programs)) {
    writeFileSync(join(bin, name), `#!/bin/bash\n${body}\n`);
    chmodSync(join(bin, name), 0o755);
  }
  return bin;
}

function runScript(script: string, stdin: string | Uint8Array, bin: string, env: Record<string, string> = {}): { code: number; stdout: string; stderr: string } {
  const result = Bun.spawnSync(["bash", "-c", script], { stdin: new Blob([stdin]), env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, ...env } });
  return { code: result.exitCode, stdout: result.stdout.toString(), stderr: result.stderr.toString() };
}

describe("ageCheckScript", () => {
  const FAKE_K3S = [
    'shift; args="$*"',
    'case "$args" in',
    '  *"--ignore-not-found -o go-template="*) if [ "${FAKE_STORED:-absent}" = present ]; then printf FOUND; for k in ${FAKE_KEYS:-}; do printf " %s" "$k"; done; fi; exit 0 ;;',
    '  *"jsonpath={.data.age\\.agekey}"*) printf "%s" "${FAKE_VALUE:-}"; exit 0 ;;',
    "esac",
    'echo "unexpected k3s $args" >&2; exit 2',
  ].join("\n");
  // A stand-in for age-keygen: it derives a public key from the one valid identity and, like a
  // careless tool might, quotes its input in its error.
  const FAKE_AGE_KEYGEN = [
    "#!/bin/bash",
    '[ "$1" = -y ] || exit 2',
    "in=$(cat)",
    `case "$in" in *${PRIVATE_AGE_KEY}*) echo age1fakerecipient ;; *) echo "malformed input: $in" >&2; exit 1 ;; esac`,
  ].join("\n");
  const tool = new TextEncoder().encode(FAKE_AGE_KEYGEN);
  const toolSha = sha256(FAKE_AGE_KEYGEN);

  const check = (env: Record<string, string>, bytes: Uint8Array = tool) => {
    const base = join(tempDir("demo-flux-run-"), "base");
    const bin = fakeBin({ k3s: FAKE_K3S, uname: 'echo "${FAKE_ARCH:-aarch64}"' });
    const result = runScript(ageCheckScript(base, toolSha, "sops-age"), bytes, bin, env);
    // The run's directory is gone afterwards, whatever happened.
    expect(readdirSync(base)).toEqual([]);
    expect(`${result.stdout}${result.stderr}`).not.toContain(PRIVATE_AGE_KEY);
    return result;
  };
  const b64 = (text: string) => Buffer.from(text).toString("base64");

  test("derives the public key of a valid identity and prints nothing else", () => {
    const result = check({ FAKE_STORED: "present", FAKE_KEYS: "age.agekey", FAKE_VALUE: b64(`# created: x\n${PRIVATE_AGE_KEY}\n`) });
    expect(result.code).toBe(0);
    expect(result.stdout).toBe("SOPS_AGE ok age1fakerecipient\n");
    expect(parseAgeCheck(result.stdout)).toEqual({ kind: "ok", recipients: ["age1fakerecipient"] });
  });

  test("reports missing, other keys, empty and invalid without the key's text", () => {
    expect(check({}).stdout).toBe("SOPS_AGE missing\n");
    expect(check({ FAKE_STORED: "present", FAKE_KEYS: "other.key" }).stdout).toBe("SOPS_AGE keys other.key\n");
    expect(check({ FAKE_STORED: "present", FAKE_KEYS: "" }).stdout).toBe("SOPS_AGE keys\n");
    expect(check({ FAKE_STORED: "present", FAKE_KEYS: "age.agekey", FAKE_VALUE: "" }).stdout).toBe("SOPS_AGE empty\n");
    const invalid = check({ FAKE_STORED: "present", FAKE_KEYS: "age.agekey", FAKE_VALUE: b64(`${PRIVATE_AGE_KEY.slice(0, -1)}X`) });
    expect(invalid.stdout).toBe("SOPS_AGE invalid\n");
    expect(invalid.stderr).toBe("");
  });

  test("checks the tool's hash and the machine's architecture before reading the Secret", () => {
    const changed = check({ FAKE_STORED: "present", FAKE_KEYS: "age.agekey" }, new TextEncoder().encode(`${FAKE_AGE_KEYGEN}\n# changed`));
    expect(changed.code).toBe(1);
    expect(changed.stderr).toContain("age-keygen arrived changed");
    expect(check({ FAKE_ARCH: "x86_64" }).stdout).toBe("SOPS_AGE arch x86_64\n");
  });
});

describe("deployKeyScript", () => {
  const knownHosts = readFileSync(join(ROOT, KNOWN_HOSTS_FILE), "utf8");
  const FAKE_K3S = [
    'shift; args="$*"; echo "$args" >> "$FAKE_STATE/calls"',
    'case "$args" in',
    '  *"get secret flux-system --ignore-not-found -o name"*) if [ -f "$FAKE_STATE/created.yaml" ]; then echo secret/flux-system; fi; exit 0 ;;',
    '  "create --field-manager=trustiot-demo-flux -f -") cat > "$FAKE_STATE/created.yaml"; if [ -n "${FAKE_CREATE_FAIL:-}" ]; then printf "%s\\n" "$FAKE_CREATE_FAIL" >&2; rm "$FAKE_STATE/created.yaml"; exit 1; fi; exit 0 ;;',
    '  *"jsonpath={.data.identity}"*) sed -n "s/^  identity: //p" "$FAKE_STATE/created.yaml" | tr -d "\\n"; exit 0 ;;',
    "esac",
    'echo "unexpected k3s $args" >&2; exit 2',
  ].join("\n");

  const setup = () => {
    const state = tempDir("demo-flux-state-");
    const base = join(tempDir("demo-flux-run-"), "base");
    const bin = fakeBin({ k3s: FAKE_K3S });
    return { state, base, bin, run: (env: Record<string, string> = {}) => runScript(deployKeyScript(base, sha256(knownHosts), "flux-system"), knownHosts, bin, { FAKE_STATE: state, ...env }) };
  };

  test("generates the key, stores it marked with the host keys, prints only the public key and shreds the files", () => {
    const { state, base, run } = setup();
    mkdirSync(join(base, "deploy-key.stale"), { recursive: true });
    writeFileSync(join(base, "deploy-key.stale", "key"), "left by a killed run");
    const result = run();
    expect(result.code).toBe(0);
    expect(result.stderr).toBe("");
    const lines = result.stdout.trim().split("\n");
    expect(lines[0]).toBe("STALE shredded a directory that a stopped run left behind");
    expect(lines[1]).toMatch(/^PUBLIC ssh-ed25519 [A-Za-z0-9+/=]+ trustiot-demo-flux$/);
    expect(lines[2]).toBe("DEPLOY_KEY created");
    expect(readdirSync(base)).toEqual([]);

    const secret = Bun.YAML.parse(readFileSync(join(state, "created.yaml"), "utf8")) as { metadata: { name: string; namespace: string; labels: Record<string, string> }; type: string; data: Record<string, string> };
    expect(secret.metadata).toEqual({ name: "flux-system", namespace: "flux-system", labels: { [MARKER]: MARKED_DEPLOY_KEY } });
    expect(secret.type).toBe("Opaque");
    expect(Object.keys(secret.data).sort()).toEqual(["identity", "identity.pub", "known_hosts"]);
    const identity = Buffer.from(secret.data.identity!, "base64").toString();
    expect(identity).toStartWith("-----BEGIN OPENSSH PRIVATE KEY-----");
    expect(Buffer.from(secret.data["identity.pub"]!, "base64").toString().trim()).toBe(lines[1]!.slice("PUBLIC ".length));
    expect(Buffer.from(secret.data.known_hosts!, "base64").toString()).toBe(knownHosts);
    expect(result.stdout).not.toContain("PRIVATE");
    expect(result.stdout).not.toContain(secret.data.identity!);
    // kubectl got the Secret on stdin, never on its command line.
    expect(readFileSync(join(state, "calls"), "utf8")).not.toContain(secret.data.identity!);
  });

  test("leaves an existing Secret alone", () => {
    const { state, base, run } = setup();
    writeFileSync(join(state, "created.yaml"), "existing");
    const result = run();
    expect(result.code).toBe(0);
    expect(result.stdout).toBe("DEPLOY_KEY exists\n");
    expect(readFileSync(join(state, "created.yaml"), "utf8")).toBe("existing");
    expect(readdirSync(base)).toEqual([]);
  });

  test("passes kubectl's errors on without lines that look like key material", () => {
    const { base, run } = setup();
    const result = run({ FAKE_CREATE_FAIL: `error: bad data ${"QUJD".repeat(20)}\n-----BEGIN OPENSSH PRIVATE KEY-----\nError from server (Forbidden): secrets is forbidden` });
    expect(result.code).toBe(1);
    expect(result.stderr).toBe("Error from server (Forbidden): secrets is forbidden\n");
    expect(result.stdout).toBe("");
    expect(readdirSync(base)).toEqual([]);
  });

  test("refuses host keys that arrived changed", () => {
    const { base, bin, state } = setup();
    const result = runScript(deployKeyScript(base, sha256(knownHosts), "flux-system"), `${knownHosts}#`, bin, { FAKE_STATE: state });
    expect(result.code).toBe(1);
    expect(result.stderr).toContain("known_hosts arrived changed");
    expect(existsSync(join(state, "calls"))).toBe(false);
  });
});

describe("applyScript and deleteScript", () => {
  const { manifests } = parseManifests(DEMO_MANIFESTS);
  const FAKE_K3S = [
    'shift; args="$*"; echo "$args" >> "$FAKE_STATE/calls"',
    'case "$args" in',
    '  "apply --server-side -f "*) n=$(cat "$FAKE_STATE/applies" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$FAKE_STATE/applies"; if [ $n -eq 1 ]; then echo "error: no matches for kind GitRepository" >&2; exit 1; fi; [ -z "${FAKE_SECOND_FAIL:-}" ] || exit 1; echo "namespace/flux-system serverside-applied"; exit 0 ;;',
    '  "wait --for=condition=established --timeout=120s "*) exit 0 ;;',
    '  "delete -f "*" --ignore-not-found --wait=false") echo "namespace \\"flux-system\\" deleted"; exit 0 ;;',
    "esac",
    'echo "unexpected k3s $args" >&2; exit 2',
  ].join("\n");
  const setup = () => ({ state: tempDir("demo-flux-state-"), base: join(tempDir("demo-flux-run-"), "base"), bin: fakeBin({ k3s: FAKE_K3S }) });

  test("applies, waits for every CRD, applies again", () => {
    const { state, base, bin } = setup();
    const result = runScript(applyScript(base, manifests!.sha256, manifests!.crds), manifests!.text, bin, { FAKE_STATE: state });
    expect(result.code).toBe(0);
    expect(result.stdout).toContain("APPLY first-incomplete");
    expect(result.stdout.trim().split("\n").at(-1)).toBe("APPLY done");
    const calls = readFileSync(join(state, "calls"), "utf8").trim().split("\n");
    expect(calls).toHaveLength(3);
    expect(calls[0]).toMatch(/^apply --server-side -f \S+\/flux-system\.yaml$/);
    expect(calls[1]).toBe(`wait --for=condition=established --timeout=120s ${manifests!.crds.map((c) => `crd/${c}`).join(" ")}`);
    expect(calls[2]).toBe(calls[0]);
    expect(readdirSync(base)).toEqual([]);
  });

  test("fails when the second apply fails, and never applies changed manifests", () => {
    const { state, base, bin } = setup();
    expect(runScript(applyScript(base, manifests!.sha256, manifests!.crds), manifests!.text, bin, { FAKE_STATE: state, FAKE_SECOND_FAIL: "1" }).code).not.toBe(0);
    const changed = setup();
    const result = runScript(applyScript(changed.base, manifests!.sha256, manifests!.crds), `${manifests!.text}\n# changed`, changed.bin, { FAKE_STATE: changed.state });
    expect(result.code).toBe(1);
    expect(existsSync(join(changed.state, "calls"))).toBe(false);
  });

  test("deletes Flux's objects without waiting", () => {
    const { state, base, bin } = setup();
    const result = runScript(deleteScript(base, manifests!.sha256), manifests!.text, bin, { FAKE_STATE: state });
    expect(result.code).toBe(0);
    expect(result.stdout).toContain("DELETE done");
    expect(readFileSync(join(state, "calls"), "utf8").trim()).toMatch(/^delete -f \S+\/flux-system\.yaml --ignore-not-found --wait=false$/);
  });
});

// --- The runs against a fake cluster ----------------------------------------------------------------------

const OPTIONS: CommonOptions = { sshHost: "demo", apply: false, confirm: "", lockHeld: false };
const TUNNEL = { active: "active", pid: "10", restarts: "0", started: "x" };
const ready = (extra: Record<string, unknown> = {}) => ({
  metadata: { generation: 1 },
  status: { observedGeneration: 1, conditions: [{ type: "Ready", status: "True", reason: "Succeeded", message: "ok" }], ...extra },
});
const notReady = { metadata: { generation: 1 }, status: { observedGeneration: 1, conditions: [{ type: "Ready", status: "False", reason: "Progressing", message: "working" }] } };

class FakeFlux implements FluxOps {
  calls: string[] = [];
  time = 0;
  lockValue: Awaited<ReturnType<FluxOps["lockState"]>> = { kind: "free" };
  sopsAgeValue: SopsAgeState;
  facts: ClusterFacts;
  /** Polls each object needs before it is ready, by resource name. */
  pollsUntilReady: Record<string, number> = {};
  stalled = new Set<string>();
  /** Flux drops the marker when it applies its own Namespace (the failure mode the run checks for). */
  dropMarkerOnReconcile = false;
  fluxRefs: string[] = [];
  /** Objects that no Kustomization applied (made by hand); every other one is the root Kustomization's. */
  unlabeled = new Set<string>();
  /** Polls a deleted object stays (helm-controller uninstalling). */
  deletePolls = 2;
  /** What Flux reports once it caught up: main's commit, the chart by its digest, the deployed chart. */
  revisions = { git: `main@sha1:${CLEAN.head}`, chart: DIGEST, release: `0.0.0-rc.9+${"b".repeat(12)}` };
  /** Polls an object stays Ready=True at an older revision (after its pollsUntilReady), by resource name. */
  pollsAtOldRevision: Record<string, number> = {};
  /** HelmRelease polls after a reconcile request that still show the request unhandled. */
  requestPolls = 0;
  private request?: { token: string; polls: number };
  private polls = new Map<string, number>();
  private deleting = new Map<string, number>();
  clusterReads = 0;
  onClusterRead?: (n: number, facts: ClusterFacts) => void;
  publicKey = DEPLOY_KEY_PUB;

  constructor(facts: ClusterFacts, sopsAge: SopsAgeState) {
    this.facts = facts;
    this.sopsAgeValue = sopsAge;
  }
  async tunnel() {
    return TUNNEL;
  }
  async lockState() {
    return this.lockValue;
  }
  async lock(step: string) {
    this.calls.push(`lock ${step}`);
    return async () => {
      this.calls.push("release");
    };
  }
  async cluster(_manifests: FluxManifests | undefined) {
    this.clusterReads += 1;
    this.onClusterRead?.(this.clusterReads, this.facts);
    return structuredClone(this.facts);
  }
  async sopsAge(_secret: string) {
    this.calls.push("sops-age check");
    return this.sopsAgeValue;
  }
  async createDeployKey(secret: string, _knownHosts: string) {
    this.calls.push(`create ${secret}`);
    return { publicKey: this.publicKey };
  }
  async markNamespace(resourceVersion: string) {
    this.calls.push(`mark ${resourceVersion}`);
    this.facts.namespace!.labels[MARKER] = MARKED_INSTALL;
  }
  async applyFlux(manifests: FluxManifests) {
    this.calls.push("apply");
    this.facts.fluxCrds = manifests.crds;
    this.facts.rootKustomization = true;
    return "namespace/flux-system serverside-applied\n";
  }
  async object(resource: string, namespace: string, name: string): Promise<Record<string, unknown> | undefined> {
    const key = `${resource}/${namespace}/${name}`;
    const n = (this.polls.get(key) ?? 0) + 1;
    this.polls.set(key, n);
    if (this.stalled.has(resource)) {
      return { ...notReady, status: { ...notReady.status, conditions: [...notReady.status.conditions, { type: "Stalled", status: "True", reason: "RetriesExceeded", message: "install failed" }] } };
    }
    if (resource === "kustomizations.kustomize.toolkit.fluxcd.io" && this.dropMarkerOnReconcile) delete this.facts.namespace!.labels[MARKER];
    if (n <= (this.pollsUntilReady[resource] ?? 0)) return notReady;
    if (resource === "deployments.apps") return { metadata: { generation: 1 }, spec: { replicas: 1, template: { spec: { containers: [{ image: `ghcr.io/fluxcd/${name}:v1` }] } } }, status: { observedGeneration: 1, availableReplicas: 1, updatedReplicas: 1 } };
    const old = n <= (this.pollsUntilReady[resource] ?? 0) + (this.pollsAtOldRevision[resource] ?? 0);
    const git = old ? `main@sha1:${"c".repeat(40)}` : this.revisions.git;
    if (resource.startsWith("gitrepositories")) return ready({ artifact: { revision: git } });
    if (resource.startsWith("kustomizations")) return ready({ lastAppliedRevision: git });
    if (resource.startsWith("ocirepositories")) return ready({ artifact: { revision: old ? `sha256:${"d".repeat(64)}` : this.revisions.chart } });
    if (resource.startsWith("helmreleases")) {
      const handled = this.request !== undefined && ++this.request.polls > this.requestPolls ? { lastHandledReconcileAt: this.request.token } : {};
      const chartVersion = old ? `0.0.0-rc.8+${"c".repeat(12)}` : this.revisions.release;
      return { ...ready({ history: [{ chartVersion, status: "deployed" }], ...handled }), spec: { suspend: false } };
    }
    return ready();
  }
  async requestReconcile(resource: string, namespace: string, name: string, token: string) {
    this.calls.push(`request reconcile ${resource} ${namespace}/${name}`);
    this.request = { token, polls: 0 };
  }
  async fluxObjects(_crds: readonly string[]) {
    for (const [ref, left] of this.deleting) {
      if (left <= 1) {
        this.deleting.delete(ref);
        this.fluxRefs = this.fluxRefs.filter((r) => r !== ref);
      } else this.deleting.set(ref, left - 1);
    }
    return this.fluxRefs.map((ref) => ({ ref, appliedBy: this.unlabeled.has(ref) ? "" : "flux-system/flux-system" }));
  }
  async pods(_namespace: string) {
    return ["trustiot-identity-abc Running ready=true restarts=0"];
  }
  async suspendRoot() {
    this.calls.push("suspend root");
  }
  async deleteObject(resource: string, namespace: string, name: string) {
    this.calls.push(`delete ${resource} ${namespace}/${name}`);
    const ref = this.fluxRefs.find((r) => {
      const [kind = "", ns, n] = r.split("/");
      return CRD_KINDS[kind] === resource && ns === namespace && n === name;
    });
    if (ref !== undefined) this.deleting.set(ref, this.deletePolls);
  }
  async deleteFlux(_manifests: FluxManifests) {
    this.calls.push("delete flux");
    delete this.facts.namespace;
    this.facts.fluxCrds = [];
    this.facts.fluxClusterObjects = [];
    return 'namespace "flux-system" deleted\n';
  }
  async kept() {
    return ["persistentvolumeclaim/trustiot-postgres-1", "cluster.postgresql.cnpg.io/trustiot-postgres"];
  }
  async clusterState() {
    return {};
  }
  now() {
    return this.time;
  }
  async sleep(ms: number) {
    this.time += ms;
  }
}

describe("runs", () => {
  let dir: string;
  let log: Log;
  let root: string;
  let inputs: Inputs;
  let repo: RepoInputs;
  beforeEach(async () => {
    dir = mkdtempSync(join(tmpdir(), "demo-flux-log-"));
    log = new Log("test", dir);
    root = mainReady();
    inputs = { repo: () => readRepo(root, CLEAN, build) };
    repo = await inputs.repo();
  });
  afterEach(() => rmSync(dir, { recursive: true, force: true }));

  const logText = () => readFileSync(log.file, "utf8");
  const codeOf = () => /--confirm ([0-9a-f]{12})/.exec(logText())?.[1] ?? "";
  const okAge = (): SopsAgeState => ({ kind: "ok", recipients: [repo.demoRecipient!] });

  describe("install", () => {
    test("a dry run prints the plan and its code and changes nothing", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      const plan = await runInstall(fake, log, OPTIONS, inputs);
      expect(plan.refusals).toEqual([]);
      expect(fake.calls).toEqual(["sops-age check"]);
      expect(codeOf()).toMatch(/^[0-9a-f]{12}$/);
      expect(logText()).toContain(`label namespace flux-system ${MARKER}=${MARKED_INSTALL}`);
      expect(logText()).toContain(`ingress ClusterIP ${CLUSTER_IP}`);
    });

    test("refusals stop a dry run and --apply alike, before anything changes", async () => {
      const fake = new FakeFlux(readyCluster(repo), { kind: "missing" });
      fake.lockValue = { kind: "held", owner: "Laysi/trustiot-v2#4670 2026-10-21T00:00:00Z" };
      await expect(runInstall(fake, log, OPTIONS, inputs)).rejects.toThrow("2 refusals above; nothing changed");
      await expect(runInstall(fake, log, { ...OPTIONS, apply: true, confirm: "x" }, inputs)).rejects.toThrow("refusals above");
      expect(fake.calls.filter((c) => c !== "sops-age check")).toEqual([]);
      expect(logText()).toContain("REFUSED: the lab lock is held by");
      expect(logText()).not.toContain("--confirm");
    });

    test("main as it is today is refused", async () => {
      const asIs = mainAsIs();
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await expect(runInstall(fake, log, OPTIONS, { repo: () => readRepo(asIs, CLEAN, build) })).rejects.toThrow("3 refusals above");
    });

    test("--apply needs the code of the plan", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await expect(runInstall(fake, log, { ...OPTIONS, apply: true }, inputs)).rejects.toThrow("--apply needs --confirm");
      await expect(runInstall(fake, log, { ...OPTIONS, apply: true, confirm: "000000000000" }, inputs)).rejects.toThrow("the plan changed since it was shown");
      expect(fake.calls.filter((c) => c !== "sops-age check")).toEqual([]);
    });

    test("--apply labels, applies, waits for each object and reports, holding the lock", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await runInstall(fake, log, OPTIONS, inputs);
      const code = codeOf();
      fake.pollsUntilReady = { "helmreleases.helm.toolkit.fluxcd.io": 3, "gitrepositories.source.toolkit.fluxcd.io": 1 };
      await runInstall(fake, log, { ...OPTIONS, apply: true, confirm: code }, inputs);
      expect(fake.calls).toEqual(["sops-age check", "sops-age check", "lock install", "sops-age check", "mark 100", "apply", "request reconcile helmreleases.helm.toolkit.fluxcd.io trustiot/trustiot", "release"]);
      const text = logText();
      expect(text).toContain("HelmRelease trustiot/trustiot at 0.0.0-rc.9+bbbbbbbbbbbb: Ready=False Progressing: working");
      expect(text).toContain("HelmRelease trustiot/trustiot at 0.0.0-rc.9+bbbbbbbbbbbb: Ready=True Succeeded: ok");
      expect(text).toContain(`requested a reconcile of HelmRelease trustiot/trustiot (${RECONCILE_ANNOTATION}=1970-01-01T00:00:`);
      expect(text).toContain("pulled:   the same");
      expect(text).toContain(`deployed: the same (0.0.0-rc.9+${"b".repeat(12)} (deployed))`);
      expect(text).toContain("trustiot-identity-abc Running");
      expect(fake.time).toBe(4 * POLL_MS);
    });

    test("a run after an interrupted one only waits and reports once Flux manages itself", async () => {
      const cluster = readyCluster(repo);
      cluster.namespace = withDeployKey(ageKeyNamespace({ labels: { [MARKER]: MARKED_INSTALL } }));
      cluster.fluxCrds = repo.manifests!.crds;
      cluster.fluxClusterObjects = repo.manifests!.clusterObjects;
      cluster.rootKustomization = true;
      cluster.appNamespace = { labels: { "kustomize.toolkit.fluxcd.io/name": "flux-system", "kustomize.toolkit.fluxcd.io/namespace": "flux-system" }, terminating: false };
      const fake = new FakeFlux(cluster, okAge());
      await runInstall(fake, log, OPTIONS, inputs);
      expect(logText()).not.toContain("label namespace flux-system");
      await runInstall(fake, log, { ...OPTIONS, apply: true, confirm: codeOf() }, inputs);
      expect(fake.calls).toEqual(["sops-age check", "sops-age check", "lock install", "sops-age check", "request reconcile helmreleases.helm.toolkit.fluxcd.io trustiot/trustiot", "release"]);
      expect(logText()).toContain("HelmRelease trustiot/trustiot at 0.0.0-rc.9+bbbbbbbbbbbb: Ready=True");
    });

    test("a checkout that is not origin/main is refused", async () => {
      const dirty = { repo: () => readRepo(root, { ...CLEAN, dirty: [" M deploy/flux/clusters/demo/trustiot/values-demo.yaml"] }, build) };
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await expect(runInstall(fake, log, OPTIONS, dirty)).rejects.toThrow("1 refusal above");
      expect(logText()).toContain("uncommitted changes");
    });

    test("a change between the plan and the run stops it under the lock", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await runInstall(fake, log, OPTIONS, inputs);
      const code = codeOf();
      fake.onClusterRead = (n, facts) => {
        if (n === 3) facts.namespace!.uid = "uid-2";
      };
      await expect(runInstall(fake, log, { ...OPTIONS, apply: true, confirm: code }, inputs)).rejects.toThrow("the plan changed since it was shown");
      expect(fake.calls).toEqual(["sops-age check", "sops-age check", "lock install", "sops-age check", "release"]);
    });

    test("a refusal that appears under the lock stops the run", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await runInstall(fake, log, OPTIONS, inputs);
      const code = codeOf();
      fake.onClusterRead = (n, facts) => {
        if (n === 3) facts.poc = ["poc/deployment.apps/bridge"];
      };
      await expect(runInstall(fake, log, { ...OPTIONS, apply: true, confirm: code }, inputs)).rejects.toThrow("1 refusal above");
      expect(fake.calls.at(-1)).toBe("release");
      expect(fake.calls).not.toContain("apply");
    });

    test("a wait that runs out names what it waited for, and the lock is released", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await runInstall(fake, log, OPTIONS, inputs);
      const code = codeOf();
      fake.pollsUntilReady = { "gitrepositories.source.toolkit.fluxcd.io": Number.POSITIVE_INFINITY };
      const run = runInstall(fake, log, { ...OPTIONS, apply: true, confirm: code }, inputs);
      await expect(run).rejects.toThrow(`timed out after ${WAITS.source / 60_000} min waiting for GitRepository flux-system/flux-system at main@sha1:${CLEAN.head}; last state: Ready=False Progressing: working. Check that GitHub has the deploy key ${sshFingerprint(DEPLOY_KEY_PUB)}`);
      expect(fake.calls.at(-1)).toBe("release");
    });

    test("Ready=True at an older commit or chart is waited out, not reported as done", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await runInstall(fake, log, OPTIONS, inputs);
      const code = codeOf();
      fake.pollsAtOldRevision = {
        "gitrepositories.source.toolkit.fluxcd.io": 2,
        "kustomizations.kustomize.toolkit.fluxcd.io": 1,
        "ocirepositories.source.toolkit.fluxcd.io": 1,
        "helmreleases.helm.toolkit.fluxcd.io": 1,
      };
      await runInstall(fake, log, { ...OPTIONS, apply: true, confirm: code }, inputs);
      const text = logText();
      expect(text).toContain(`GitRepository flux-system/flux-system at main@sha1:${CLEAN.head}: Ready=True Succeeded: ok; at main@sha1:${"c".repeat(40)}, waiting for main@sha1:${CLEAN.head}`);
      expect(text).toContain(`Kustomization flux-system/flux-system at main@sha1:${CLEAN.head}: Ready=True Succeeded: ok; at main@sha1:${"c".repeat(40)}`);
      expect(text).toContain(`OCIRepository trustiot/trustiot at ${DIGEST}: Ready=True Succeeded: ok; at sha256:${"d".repeat(64)}, waiting for ${DIGEST}`);
      expect(text).toContain(`HelmRelease trustiot/trustiot at 0.0.0-rc.9+bbbbbbbbbbbb: Ready=True Succeeded: ok; at 0.0.0-rc.8+cccccccccccc, waiting for 0.0.0-rc.9+bbbbbbbbbbbb`);
      expect(fake.time).toBe(5 * POLL_MS);
    });

    test("main moving on, or a chart Flux never reaches, ends in a timeout that names the wanted revision", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await runInstall(fake, log, OPTIONS, inputs);
      const code = codeOf();
      fake.revisions.git = `main@sha1:${"e".repeat(40)}`;
      const moved = runInstall(fake, log, { ...OPTIONS, apply: true, confirm: code }, inputs);
      await expect(moved).rejects.toThrow(
        `waiting for GitRepository flux-system/flux-system at main@sha1:${CLEAN.head}; last state: Ready=True Succeeded: ok; at main@sha1:${"e".repeat(40)}, waiting for main@sha1:${CLEAN.head}`,
      );
      await expect(moved).rejects.toThrow("If main moved on GitHub after this run read it, fetch and run install again (dry run first) from the new main");
      expect(fake.calls.at(-1)).toBe("release");

      const stuck = new FakeFlux(readyCluster(repo), okAge());
      await runInstall(stuck, log, OPTIONS, inputs);
      stuck.revisions.release = `0.0.0-rc.9+${"f".repeat(12)}`;
      await expect(runInstall(stuck, log, { ...OPTIONS, apply: true, confirm: codeOf() }, inputs)).rejects.toThrow(
        `timed out after ${WAITS.release / 60_000} min waiting for HelmRelease trustiot/trustiot at 0.0.0-rc.9+bbbbbbbbbbbb`,
      );
    });

    test("the release counts only after helm-controller handled the reconcile request of this run", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await runInstall(fake, log, OPTIONS, inputs);
      const code = codeOf();
      fake.requestPolls = 2;
      await runInstall(fake, log, { ...OPTIONS, apply: true, confirm: code }, inputs);
      expect(logText()).toContain("reconcile request 1970-01-01T00:00:00.000Z not handled yet");
      expect(fake.time).toBe(2 * POLL_MS);

      const never = new FakeFlux(readyCluster(repo), okAge());
      await runInstall(never, log, OPTIONS, inputs);
      never.requestPolls = Number.POSITIVE_INFINITY;
      await expect(runInstall(never, log, { ...OPTIONS, apply: true, confirm: codeOf() }, inputs)).rejects.toThrow("not handled yet");
      expect(never.calls.at(-1)).toBe("release");
    });

    test("a stalled HelmRelease ends the wait at once", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await runInstall(fake, log, OPTIONS, inputs);
      const code = codeOf();
      fake.stalled.add("helmreleases.helm.toolkit.fluxcd.io");
      await expect(runInstall(fake, log, { ...OPTIONS, apply: true, confirm: code }, inputs)).rejects.toThrow("gave up waiting for HelmRelease trustiot/trustiot at 0.0.0-rc.9+bbbbbbbbbbbb: Stalled RetriesExceeded: install failed");
      expect(fake.time).toBe(0);
    });

    test("losing the marker after Flux applies its own Namespace is an error", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await runInstall(fake, log, OPTIONS, inputs);
      const code = codeOf();
      fake.dropMarkerOnReconcile = true;
      await expect(runInstall(fake, log, { ...OPTIONS, apply: true, confirm: code }, inputs)).rejects.toThrow(`lost its label ${MARKER}=${MARKED_INSTALL} after Flux applied its own manifests`);
    });
  });

  describe("deploy-key", () => {
    const withoutKey = () => {
      const cluster = readyCluster(repo);
      cluster.namespace = ageKeyNamespace();
      delete cluster.deployKey;
      return cluster;
    };

    test("dry run, then --apply creates the key and prints the public key and its fingerprint", async () => {
      const fake = new FakeFlux(withoutKey(), okAge());
      await runDeployKey(fake, log, OPTIONS, inputs);
      expect(fake.calls).toEqual([]);
      await runDeployKey(fake, log, { ...OPTIONS, apply: true, confirm: codeOf() }, inputs);
      expect(fake.calls).toEqual(["lock deploy-key", "create flux-system", "release"]);
      expect(logText()).toContain(DEPLOY_KEY_PUB);
      expect(logText()).toContain(`fingerprint ${sshFingerprint(DEPLOY_KEY_PUB)}`);
      expect(logText()).toContain('leave "Allow write access" off');
    });

    test("an existing deploy-key Secret is printed again, not replaced", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      const plan = await runDeployKey(fake, log, { ...OPTIONS, apply: true, confirm: "x" }, inputs);
      expect(plan.create).toBe(false);
      expect(fake.calls).toEqual([]);
      expect(logText()).toContain(DEPLOY_KEY_PUB);
    });

    test("refuses a Secret it did not make, a missing or foreign namespace and another holder of the lock", async () => {
      const unmarked = readyCluster(repo);
      unmarked.namespace = withDeployKey(ageKeyNamespace(), "");
      await expect(runDeployKey(new FakeFlux(unmarked, okAge()), log, OPTIONS, inputs)).rejects.toThrow("refusal");
      expect(logText()).toContain("was not made by deploy-key");
      const absent = withoutKey();
      delete absent.namespace;
      await expect(runDeployKey(new FakeFlux(absent, okAge()), log, OPTIONS, inputs)).rejects.toThrow("1 refusal above");
      const locked = new FakeFlux(withoutKey(), okAge());
      locked.lockValue = { kind: "held", owner: "Laysi/trustiot-v2#466 2026-10-21T00:00:00Z" };
      await expect(runDeployKey(locked, log, OPTIONS, inputs)).rejects.toThrow("1 refusal above");
    });
  });

  describe("remove", () => {
    const installed = () => {
      const cluster = readyCluster(repo);
      cluster.namespace = withDeployKey(ageKeyNamespace({ labels: { [MARKER]: MARKED_INSTALL } }));
      cluster.fluxCrds = repo.manifests!.crds;
      cluster.fluxClusterObjects = repo.manifests!.clusterObjects;
      cluster.rootKustomization = true;
      const fake = new FakeFlux(cluster, okAge());
      fake.fluxRefs = ["GitRepository/flux-system/flux-system", "HelmRelease/trustiot/trustiot", "Kustomization/flux-system/flux-system", "OCIRepository/trustiot/trustiot"];
      return fake;
    };

    test("removes in the order that keeps the data", async () => {
      const fake = installed();
      await runRemove(fake, log, OPTIONS, inputs);
      expect(fake.calls).toEqual([]);
      await runRemove(fake, log, { ...OPTIONS, apply: true, confirm: codeOf() }, inputs);
      expect(fake.calls).toEqual([
        "lock remove",
        "suspend root",
        "delete helmreleases.helm.toolkit.fluxcd.io trustiot/trustiot",
        "delete ocirepositories.source.toolkit.fluxcd.io trustiot/trustiot",
        "delete gitrepositories.source.toolkit.fluxcd.io flux-system/flux-system",
        "delete kustomizations.kustomize.toolkit.fluxcd.io flux-system/flux-system",
        "delete flux",
        "release",
      ]);
      expect(logText()).toContain("kept in namespace trustiot: persistentvolumeclaim/trustiot-postgres-1");
    });

    test("refuses a Flux that is not this script's and a suspended HelmRelease", async () => {
      const foreign = installed();
      foreign.facts.namespace = ageKeyNamespace({ labels: { owner: "x" } });
      await expect(runRemove(foreign, log, OPTIONS, inputs)).rejects.toThrow("refusal");
      expect(logText()).toContain("is not this script's install, so remove leaves it alone");

      const suspended = installed();
      suspended.object = async () => ({ spec: { suspend: true } });
      await expect(runRemove(suspended, log, OPTIONS, inputs)).rejects.toThrow("1 refusal above");
      expect(logText()).toContain("HelmRelease trustiot/trustiot is suspended");

      const unknownKind = installed();
      unknownKind.fluxRefs.push("ImagePolicy/flux-system/x");
      await expect(runRemove(unknownKind, log, OPTIONS, inputs)).rejects.toThrow("1 refusal above");
      expect(logText()).toContain("kinds that main's Flux does not define exist (ImagePolicy/flux-system/x)");
    });

    test("after an install that stopped before its second apply, removes Flux without a root Kustomization to suspend", async () => {
      const partial = installed();
      partial.facts.rootKustomization = false;
      partial.fluxRefs = [];
      await runRemove(partial, log, OPTIONS, inputs);
      await runRemove(partial, log, { ...OPTIONS, apply: true, confirm: codeOf() }, inputs);
      expect(partial.calls).toEqual(["lock remove", "delete flux", "release"]);
    });

    test("refuses a flux-system that holds objects other than the install's", async () => {
      const extra = installed();
      extra.facts.namespace!.objects.push({ ref: "configmap/notes", owned: false, managed: false });
      await expect(runRemove(extra, log, OPTIONS, inputs)).rejects.toThrow("1 refusal above");
      expect(logText()).toContain("namespace flux-system holds objects that are not part of this install (configmap/notes)");
      expect(extra.calls).toEqual([]);

      // Added while the release is being uninstalled: checked again right before Flux is deleted.
      const late = installed();
      await runRemove(late, log, OPTIONS, inputs);
      const code = codeOf();
      late.onClusterRead = (n, facts) => {
        if (n === 4) facts.namespace!.objects.push({ ref: "configmap/notes", owned: false, managed: false });
      };
      await expect(runRemove(late, log, { ...OPTIONS, apply: true, confirm: code }, inputs)).rejects.toThrow("objects that are not part of this install appeared (configmap/notes)");
      expect(late.calls).not.toContain("delete flux");
      expect(late.calls.at(-1)).toBe("release");
    });

    test("refuses Flux CRDs and cluster roles that main's manifests do not define, before and while removing", async () => {
      const crd = "alerts.notification.toolkit.fluxcd.io";
      const role = "clusterrolebinding/extra-flux-system";
      const extra = installed();
      extra.facts.fluxCrds = [...extra.facts.fluxCrds, crd];
      extra.facts.fluxClusterObjects = [...extra.facts.fluxClusterObjects, role];
      await expect(runRemove(extra, log, OPTIONS, inputs)).rejects.toThrow("1 refusal above");
      expect(logText()).toContain(`Flux's CRDs or cluster-wide objects that main's manifests do not define exist (${crd}, ${role}): remove deletes only what main defines`);
      expect(extra.calls).toEqual([]);

      const late = installed();
      await runRemove(late, log, OPTIONS, inputs);
      const code = codeOf();
      late.onClusterRead = (n, facts) => {
        if (n === 4) facts.fluxCrds = [...facts.fluxCrds, crd];
      };
      await expect(runRemove(late, log, { ...OPTIONS, apply: true, confirm: code }, inputs)).rejects.toThrow(`objects that are not part of this install appeared (${crd})`);
      expect(late.calls).not.toContain("delete flux");
      expect(late.calls.at(-1)).toBe("release");
    });

    test("refuses Flux objects that the root Kustomization did not apply, before deleting anything", async () => {
      const handMade = installed();
      handMade.fluxRefs.push("HelmRelease/other/tool");
      handMade.unlabeled.add("HelmRelease/other/tool");
      await expect(runRemove(handMade, log, OPTIONS, inputs)).rejects.toThrow("1 refusal above");
      expect(logText()).toContain("Flux objects that the root Kustomization flux-system did not apply from main exist (HelmRelease/other/tool)");
      expect(handMade.calls).toEqual([]);

      // The same object appearing between the plan and the run stops it under the lock.
      const late = installed();
      await runRemove(late, log, OPTIONS, inputs);
      const code = codeOf();
      late.onClusterRead = (n) => {
        if (n === 3) {
          late.fluxRefs.push("HelmRelease/other/tool");
          late.unlabeled.add("HelmRelease/other/tool");
        }
      };
      await expect(runRemove(late, log, { ...OPTIONS, apply: true, confirm: code }, inputs)).rejects.toThrow("1 refusal above");
      expect(late.calls).toEqual(["lock remove", "release"]);
    });

    test("nothing to remove on the age key step's namespace", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      const plan = await runRemove(fake, log, { ...OPTIONS, apply: true, confirm: "x" }, inputs);
      expect(plan.remove).toBe(false);
      expect(fake.calls).toEqual([]);
    });
  });

  test("status reads and prints, and changes nothing", async () => {
    const fake = new FakeFlux(readyCluster(repo), okAge());
    fake.facts.fluxCrds = repo.manifests!.crds;
    await runStatus(fake, log, inputs);
    expect(fake.calls).toEqual([]);
    expect(logText()).toContain("namespace flux-system: only the age key step's");
    expect(logText()).toContain(`deploy key fingerprint ${sshFingerprint(DEPLOY_KEY_PUB)}`);
    expect(logText()).toContain("GitRepository flux-system/flux-system: Ready=True");
  });
});

// --- What boxOps sends ---------------------------------------------------------------------------------

/** Splits a command line that shq quoted back into its words. */
function words(line: string): string[] {
  const out: string[] = [];
  let word = "";
  let inWord = false;
  for (let i = 0; i < line.length; i += 1) {
    const c = line[i]!;
    if (c === "'") {
      const end = line.indexOf("'", i + 1);
      if (end < 0) throw new Error(`unbalanced quote in ${line}`);
      word += line.slice(i + 1, end);
      i = end;
      inWord = true;
    } else if (c === "\\") {
      word += line[i + 1] ?? "";
      i += 1;
      inWord = true;
    } else if (c === " ") {
      if (inWord) out.push(word);
      word = "";
      inWord = false;
    } else {
      word += c;
      inWord = true;
    }
  }
  if (inWord) out.push(word);
  return out;
}

describe("boxOps", () => {
  test("words splits what shq quotes", () => {
    const args = ["get", "x", "jsonpath={.data.identity\\.pub}", "it's", "a b"];
    expect(words(args.map(shq).join(" "))).toEqual(args);
  });

  test("never reads a Secret whole, reads only the deploy key's public halves, and sends inputs on stdin", async () => {
    const dir = mkdtempSync(join(tmpdir(), "demo-flux-box-"));
    try {
      const log = new Log("test", dir);
      const knownHosts = readFileSync(join(ROOT, KNOWN_HOSTS_FILE), "utf8");
      const kubectl: string[][] = [];
      const scripts: { script: string; stdin: string }[] = [];
      const ok = (stdout: string): RunResult => ({ stdout, stderr: "", exitCode: 0 });
      const notFound: RunResult = { stdout: "", stderr: 'Error from server (NotFound): namespaces "trustiot" not found', exitCode: 1 };
      const run = async (cmd: string[], stdin?: string | Uint8Array): Promise<RunResult> => {
        expect(cmd.slice(0, 4)).toEqual(["ssh", "-o", "BatchMode=yes", "demo"]);
        const line = words(cmd[4] ?? "");
        if (line.slice(0, 3).join(" ") === "sudo bash -c") {
          expect(line).toHaveLength(4);
          const script = line[3]!;
          scripts.push({ script, stdin: stdin === undefined ? "" : Buffer.from(stdin).toString() });
          if (script.includes("SOPS_AGE")) return ok("SOPS_AGE ok age1fake\n");
          if (script.includes("DEPLOY_KEY")) return ok(`PUBLIC ${DEPLOY_KEY_PUB}\nDEPLOY_KEY created\n`);
          return ok("APPLY done\n");
        }
        expect(line.slice(0, 3)).toEqual(["sudo", "k3s", "kubectl"]);
        const args = line.slice(3);
        kubectl.push(args);
        const joined = args.join(" ");
        if (joined === "get namespace flux-system -o json") return ok(JSON.stringify({ metadata: { uid: "u", resourceVersion: "5", labels: { "kubernetes.io/metadata.name": "flux-system" } } }));
        if (joined === "get namespace trustiot -o json") return notFound;
        if (args[0] === "api-resources") return ok("configmaps\nsecrets\nserviceaccounts\n");
        if (joined.startsWith("-n flux-system get configmaps,secrets,serviceaccounts --ignore-not-found -o jsonpath=")) return ok("v1\tSecret\tsops-age\t\tOpaque\t\t\t\nv1\tSecret\tflux-system\t\tOpaque\t\t\t\n");
        if (joined === `-n flux-system get secrets -o go-template=${SECRET_TEMPLATE}`) return ok("flux-system\tOpaque\tdeploy-key\tidentity,identity.pub,known_hosts,\nsops-age\tOpaque\t<no value>\tage.agekey,\n");
        if (joined === "-n flux-system get secret flux-system -o jsonpath={.data.identity\\.pub}") return ok(Buffer.from(`${DEPLOY_KEY_PUB}\n`).toString("base64"));
        if (joined === "-n flux-system get secret flux-system -o jsonpath={.data.known_hosts}") return ok(Buffer.from(knownHosts).toString("base64"));
        if (joined === "get customresourcedefinitions.apiextensions.k8s.io -o name") return ok("customresourcedefinition.apiextensions.k8s.io/clusters.postgresql.cnpg.io\n");
        if (joined.includes("-l app.kubernetes.io/part-of=flux -o name")) return ok("");
        if (joined.endsWith("--ignore-not-found -o name")) return ok("");
        if (args[0] === "label") return ok("namespace/flux-system labeled\n");
        if (joined === `get services -A -o ${SERVICES_JSONPATH}`) return ok("default/kubernetes\t10.43.0.1\t/\n");
        if (joined === "get servicecidrs.networking.k8s.io -o jsonpath={.items[*].spec.cidrs[*]}") return ok("10.43.0.0/16");
        if (args[2] === "annotate") return ok("helmrelease.helm.toolkit.fluxcd.io/trustiot annotated\n");
        if (args[0] === "get" && args[2] === "-A") return ok("HelmRelease/trustiot/trustiot\tflux-system/flux-system\nGitRepository/flux-system/flux-system\t/\n");
        throw new Error(`unexpected kubectl ${joined}`);
      };
      const box = new Box("demo", log, { run, sleep: async () => {} });
      const tool = new TextEncoder().encode("fake age-keygen");
      const ops = boxOps(box, OPTIONS, async () => tool);
      const manifests = parseManifests(DEMO_MANIFESTS).manifests!;
      const facts = await ops.cluster(manifests);
      expect(facts.namespace?.secrets.map((s) => s.name)).toEqual(["flux-system", "sops-age"]);
      expect(facts.namespace?.objects.map((o) => o.ref)).toEqual(["secret/flux-system", "secret/sops-age"]);
      expect(facts.deployKey).toEqual({ publicKey: DEPLOY_KEY_PUB, knownHostsSha256: sha256(knownHosts) });
      expect(facts.missingPlatformCrds).toHaveLength(4);
      expect(facts.appNamespace).toBeUndefined();
      expect(facts.services).toEqual([{ ref: "default/kubernetes", ips: ["10.43.0.1"], release: "/" }]);
      expect(facts.serviceCidrs).toEqual(["10.43.0.0/16"]);
      expect(SERVICES_JSONPATH).toBe(
        'jsonpath={range .items[*]}{.metadata.namespace}/{.metadata.name}{"\\t"}{.spec.clusterIPs[*]}{"\\t"}{.metadata.annotations.meta\\.helm\\.sh/release-namespace}/{.metadata.annotations.meta\\.helm\\.sh/release-name}{"\\n"}{end}',
      );
      expect(await ops.sopsAge("sops-age")).toEqual({ kind: "ok", recipients: ["age1fake"] });
      expect(await ops.createDeployKey("flux-system", knownHosts)).toEqual({ publicKey: DEPLOY_KEY_PUB });
      await ops.markNamespace("5");
      await ops.applyFlux(manifests);
      await ops.requestReconcile("helmreleases.helm.toolkit.fluxcd.io", "trustiot", "trustiot", "2026-10-21T00:00:00.000Z");
      await expect(ops.requestReconcile("helmreleases.helm.toolkit.fluxcd.io", "trustiot", "trustiot", "x; true")).rejects.toThrow("not a reconcile token");
      await expect(ops.object("secrets", "flux-system", "sops-age")).rejects.toThrow("never read");
      await expect(ops.object("secret", "flux-system", "flux-system")).rejects.toThrow("never read");
      expect(await ops.fluxObjects(manifests.crds)).toEqual([
        { ref: "GitRepository/flux-system/flux-system", appliedBy: "" },
        { ref: "HelmRelease/trustiot/trustiot", appliedBy: "flux-system/flux-system" },
      ]);
      const listed = kubectl.find((a) => a[2] === "-A" && a[1] !== "services")!;
      expect(listed[1]).toBe(manifests.crds.join(","));
      expect(listed.at(-1)).toBe(
        'jsonpath={range .items[*]}{.kind}/{.metadata.namespace}/{.metadata.name}{"\\t"}{.metadata.labels.kustomize\\.toolkit\\.fluxcd\\.io/namespace}/{.metadata.labels.kustomize\\.toolkit\\.fluxcd\\.io/name}{"\\n"}{end}',
      );

      // No Secret is read as a whole object, and the only Secret values read on the workstation
      // are the deploy key's public halves.
      for (const args of kubectl) {
        const output = args[args.indexOf("-o") + 1] ?? "";
        if (args.some((a) => /(^|,)secrets?(,|$)/.test(a))) expect(output).not.toMatch(/^(json|yaml)$/);
        if (output.includes(".data") && output !== `go-template=${SECRET_TEMPLATE}`) expect(output).toMatch(/^jsonpath=\{\.data\.(identity\\\.pub|known_hosts)\}$/);
      }
      expect(kubectl.filter((a) => a.join(" ").includes("{.data.")).map((a) => a.at(-1))).toEqual(["jsonpath={.data.identity\\.pub}", "jsonpath={.data.known_hosts}"]);
      expect(kubectl).toContainEqual(["label", "namespace", "flux-system", `${MARKER}=${MARKED_INSTALL}`, "--field-manager=trustiot-demo-flux", "--resource-version=5"]);
      expect(kubectl).toContainEqual([
        "-n",
        "trustiot",
        "annotate",
        "--overwrite",
        "--field-manager=trustiot-demo-flux",
        "helmreleases.helm.toolkit.fluxcd.io",
        "trustiot",
        `${RECONCILE_ANNOTATION}=2026-10-21T00:00:00.000Z`,
      ]);

      // The root scripts get their input on stdin: the tool, the host keys and the manifests.
      expect(scripts.map((s) => s.stdin)).toEqual(["fake age-keygen", knownHosts, manifests.text]);
      expect(scripts[0]!.script).toBe(ageCheckScript(REMOTE_BASE, sha256("fake age-keygen"), "sops-age"));
      expect(scripts[1]!.script).toBe(deployKeyScript(REMOTE_BASE, sha256(knownHosts), "flux-system"));
      expect(scripts[2]!.script).toBe(applyScript(REMOTE_BASE, manifests.sha256, manifests.crds));
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
