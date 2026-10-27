// The rules and the runs of scripts/demo/flux.ts without ssh (Laysi/trustiot-v2#484): every
// refusal, the plan and confirmation flow against a fake cluster, the root scripts run with bash
// against a fake k3s (the private keys must never reach their output), and the commands boxOps
// sends (no Secret is ever read whole).
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { appendFileSync, chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { COMMAND_TIMEOUT_MS, SLOW_TEST_MS, throwIfTimedOut, useProcessTestTimeouts } from "../../deploy/chart/tests/timing.ts";
import type { RunResult } from "../release/exec.ts";
import { appDir, renderChartSource } from "../release/flux.ts";
import { CHART_REGISTRY } from "../release/lib.ts";
import { sopsFileProblems } from "../flux/lib.ts";
import { Box, type CommonOptions, DEMO_MIN_CHART_VERSION, ROOT, sha256, shq } from "./lib.ts";
import {
  type Bootstrap,
  type ClusterFacts,
  type CodeFacts,
  type DeployKeyFacts,
  type FluxManifests,
  FluxLog,
  type FluxOps,
  type GitRef,
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
  REMOTE_LOCK,
  type RepoInputs,
  SECRET_TEMPLATE,
  type SopsAgeState,
  WAITS,
  ageCheckScript,
  applyScript,
  boxOps,
  cleanErrors,
  clusterIpProblems,
  codeProblems,
  deleteScript,
  deployKeyProblem,
  deployKeyScript,
  deploymentReadiness,
  fluxReadiness,
  followsMain,
  foreignFluxObjects,
  gitRefOf,
  hidingSshHost,
  isIpv4,
  isObjectName,
  lockProblem,
  namespaceVerdict,
  parseAgeCheck,
  parseFluxObjects,
  parseManifests,
  parseSecrets,
  parseServices,
  pinDocument,
  pinProblem,
  pinnedBootstrap,
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
  suspendedProblems,
  unexpectedNamespaceObjects,
  wantedRevisions,
} from "./flux.ts";
import { StillRunningError } from "./tunnel.ts";

// These tests start git, ssh-keygen and bash; bun's 5 s default is too short on a loaded machine.
useProcessTestTimeouts();

/** Runs a local command to its end, bounded by COMMAND_TIMEOUT_MS, and names it when it is killed. */
function spawn(cmd: string[], options: { cwd?: string; stdin?: string | Uint8Array; env?: Record<string, string | undefined> } = {}): { code: number; stdout: string; stderr: string } {
  const result = Bun.spawnSync({
    cmd,
    stdout: "pipe",
    stderr: "pipe",
    timeout: COMMAND_TIMEOUT_MS,
    ...(options.cwd === undefined ? {} : { cwd: options.cwd }),
    ...(options.stdin === undefined ? {} : { stdin: new Blob([options.stdin]) }),
    ...(options.env === undefined ? {} : { env: options.env }),
  });
  throwIfTimedOut(cmd, result);
  return { code: result.exitCode ?? -1, stdout: result.stdout.toString(), stderr: result.stderr.toString() };
}

const DEMO = appDir("demo");
const SAAS_FLUX = "deploy/flux/clusters/saas/flux-system";
const COMPONENTS = readFileSync(join(ROOT, SAAS_FLUX, "gotk-components.yaml"), "utf8");
const SYNC = readFileSync(join(ROOT, SAAS_FLUX, "gotk-sync.yaml"), "utf8");
/** What `kubectl kustomize deploy/flux/clusters/demo/flux-system` renders, without kubectl: the demo patch applied by hand. */
const DEMO_MANIFESTS = `${COMPONENTS}\n---\n${SYNC.replace("path: ./deploy/flux/clusters/saas", "path: ./deploy/flux/clusters/demo")}`;
const build = async () => DEMO_MANIFESTS;
/** Kind to CRD name, as main's manifests define them. */
const CRD_KINDS = parseManifests(DEMO_MANIFESTS).manifests?.crdKinds ?? {};
const CLEAN: CodeFacts = { head: "a".repeat(40), originMain: "a".repeat(40), dirty: [], hidden: [], originUrl: "git@github.com:Laysi/trustiot-v2.git" };
const HEAD = CLEAN.head;
const DIGEST = `sha256:${"b".repeat(64)}`;
const CLUSTER_IP = "10.43.0.50";
/** A fake age identity of the real length, built at run time so that gitleaks' age-secret-key rule does not report the source. */
const PRIVATE_AGE_KEY = `AGE-SECRET-KEY-1${"Q".repeat(58)}`;
/**
 * The armour lines of an OpenSSH private key, built at run time: written out in this file, two of
 * them a few lines apart in one diff hunk match gitleaks' private-key rule.
 */
const PEM = {
  begin: `-----BEGIN OPENSSH ${"PRIVATE"} KEY-----`,
  end: `-----END OPENSSH ${"PRIVATE"} KEY-----`,
};
/** Credential-shaped values built at run time, so that no scanner reports the source: a GitHub token of the classic length, and others. */
const FAKE = {
  github: `ghp_${"A1b2".repeat(9)}`,
  githubPat: `github_pat_${"Ab12".repeat(6)}`,
  resend: `re_${"Xy34".repeat(6)}`,
  openai: `sk-${"Pq56".repeat(6)}`,
  random: ["Zk3", "Qw9", "Ln2", "Rt7", "Vb5", "Hc8", "Mx4"].join(""),
};

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

/** What readRepo reads outside the demo directory; mainAsIs copies the demo directory whole, SOPS files included. */
const FILES = [KNOWN_HOSTS_FILE, ".sops.yaml"];

/** main as it is: since Laysi/trustiot-v2#480 the chart, the ingress ClusterIP and ghcr-pull.sops.yaml are in place. */
function mainAsIs(): string {
  const root = tempDir("demo-flux-repo-");
  for (const file of [...FILES, ...readdirSync(join(ROOT, DEMO)).map((name) => `${DEMO}/${name}`)]) {
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

/** main before Laysi/trustiot-v2#480: the placeholder chart, no ingress ClusterIP, no ghcr-pull.sops.yaml. */
function mainUnready(): string {
  const root = mainAsIs();
  pinChart(root, "0.0.0-rc.7");
  const values = join(root, DEMO, "values-demo.yaml");
  writeFileSync(values, readFileSync(values, "utf8").replace(/^ingress:\n(?: .*\n)*\n?/m, ""));
  rmSync(join(root, DEMO, "ghcr-pull.sops.yaml"));
  const kustomization = join(root, DEMO, "kustomization.yaml");
  writeFileSync(kustomization, readFileSync(kustomization, "utf8").replace("  - ghcr-pull.sops.yaml\n", ""));
  return root;
}

/** main as the install needs it: a chart after the minimum, the ClusterIP pinned, the pull Secret on main. */
function mainReady(version = "0.0.0-rc.10"): string {
  const root = mainUnready();
  pinChart(root, version);
  setClusterIp(root, CLUSTER_IP);
  addPullSecret(root);
  return root;
}

// --- Redaction and small parsers ---------------------------------------------------------------------

describe("redact", () => {
  test("removes private keys, tokens and long base64", () => {
    const pem = `${PEM.begin}\nb3BlbnNzaC1rZXktdjEAAAAA\n${PEM.end}`;
    expect(redact(`before ${pem} after`)).toBe("before <redacted private key> after");
    expect(redact(`key ${PRIVATE_AGE_KEY}`)).toBe("key <redacted age key>");
    expect(redact(`token eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc`)).not.toContain("eyJ");
    expect(redact(`blob ${"A".repeat(100)}`)).toBe("blob <redacted>");
  });

  test("replaces host names and addresses that are not public names or cluster addresses", () => {
    expect(redact("hostnames[0]: Invalid value: \"app.example.com\"")).toBe('hostnames[0]: Invalid value: "<host>"');
    expect(redact("from TrustIoT <no-reply@mail.example.com>")).toBe("from TrustIoT <<email>>");
    // Addresses at names that are shown on their own are a person's too; the clone user is not.
    expect(redact("by someone@github.com, ops@networking.k8s.io and x@trustiot-identity.trustiot.svc")).toBe("by <email>, <email> and <email>");
    expect(redact("origin git@github.com:Laysi/trustiot-v2.git, ssh://git@github.com/Laysi/trustiot-v2")).toBe("origin git@github.com:Laysi/trustiot-v2.git, ssh://git@github.com/Laysi/trustiot-v2");
    expect(redact("image ghcr.io/laysi/trustiot/identity@sha256:" + "a".repeat(64))).toBe(`image ghcr.io/laysi/trustiot/identity@sha256:${"a".repeat(64)}`);
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
      "trustiot-0.0.0-rc.10.tgz",
      "deployment.apps/source-controller, job.batch/trustiot-migrate, configmap/kube-root-ca.crt",
      "keys identity, identity.pub, known_hosts; age.agekey",
      "Helm install succeeded for release trustiot/trustiot.v1 with chart trustiot@0.0.0-rc.10+bbbbbbbbbbbb",
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

  test("replaces a host name wherever it starts: after a wildcard, a dot, dashes or an empty label", () => {
    expect(redact("cert for *.app.example.com")).toBe("cert for *.<host>");
    expect(redact("suffix .app.example.com")).toBe("suffix .<host>");
    expect(redact("--app.example.com")).toBe("--<host>");
    expect(redact("dial foo..example.com:443")).toBe("dial foo..<host>:443");
  });

  test("replaces an IPv4 address after a dot and before a final one", () => {
    expect(redact("the address is 192.0.2.5.")).toBe("the address is <address>.");
    expect(redact("v1.192.0.2.5 and v1.198.51.100.7.")).toBe("v1.<address> and v1.<address>.");
    expect(redact("10.43.0.50.")).toBe("10.43.0.50.");
  });

  test("shows only exact public names and API groups; label prefixes only before the slash of a key", () => {
    for (const name of ["internal.trustiot.io", "trustiot.io", "k8s.io", "foo.github.com", "github.com.example.com", "evil.source.toolkit.fluxcd.io.example.com"]) {
      expect(redact(`at ${name} now`)).toBe("at <host> now");
    }
    expect(redact("internal.k8s.io/x")).toBe("<host>/x");
    const kept = "trustiot.io/demo-flux app.kubernetes.io/name networking.k8s.io/v1 gitrepositories.source.toolkit.fluxcd.io github.com ghcr.io";
    expect(redact(kept)).toBe(kept);
  });

  test("removes tokens, API keys, URL user information and random-looking runs", () => {
    for (const token of [FAKE.github, FAKE.githubPat, FAKE.resend, FAKE.openai, FAKE.random, `AKIA${"ABCD2345".repeat(2)}`]) {
      expect(redact(`value ${token} end`)).toBe("value <redacted> end");
    }
    expect(redact("postgres://app:pass@db.example.com:5432/x")).toBe("postgres://<redacted>@<host>:5432/x");
    expect(redact(`https://${FAKE.github}@github.com/Laysi/trustiot-v2`)).toBe("https://<redacted>@github.com/Laysi/trustiot-v2");
    expect(redact("Authorization: Bearer abcdefgh12345")).toBe("Authorization: Bearer <redacted>");
    expect(redact("password=hunter2x")).toBe("password=<redacted>");
  });

  test("keeps what the script prints: digests, commits, revisions, the public key, its fingerprint and age recipients", () => {
    const head = "0123456789abcdef0123456789abcdef01234567";
    const kept = [
      `main@sha1:${head} ${head} ${DIGEST}`,
      "ssh://git@github.com/Laysi/trustiot-v2.git",
      `ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI${"Bm9xQ2Rs".repeat(5)}Z1 trustiot-demo-flux`,
      `SHA256:${"Ab3dE5gH7jK9mN1pQ3sT5vX7zA9cE1gI3kM5oQ7sU9w"}`,
      "age1ql3z7hjy54pw3hyww5ayyfg7zqgvc7w3j2elw8zmrj2kg5sfn9aqmcac8p",
      "Deployment/trustiot/trustiot-k8s-proxy ReconciliationSucceeded HelmChartPullSucceeded",
    ].join("\n");
    expect(redact(kept)).toBe(kept);
  });

  test("shortens local paths: the checkout, the home directory and other home directories", () => {
    expect(redact(`open ${join(ROOT, "deploy/flux")}: denied`)).toBe("open <repo>/deploy/flux: denied");
    expect(redact(`open ${join(homedir(), ".kube/config")}`)).toBe("open ~/.kube/config");
    expect(redact("error: open /home/someone/x/deploy: no such file; /Users/someone/repo; /root/.ssh")).toBe("error: open ~/x/deploy: no such file; ~/repo; ~/.ssh");
  });

  test("hidingSshHost hides the ssh host in ssh's errors and leaves stdout as it is", async () => {
    const run = hidingSshHost(async () => ({ exitCode: 255, stdout: "LOCK demo-box\n", stderr: "ssh: Could not resolve hostname demo-box: Name or service not known\nmy-demo-box stays\n" }), "demo-box");
    const result = await run(["ssh", "demo-box", "true"]);
    expect(result.stderr).toBe("ssh: Could not resolve hostname <ssh-host>: Name or service not known\nmy-demo-box stays\n");
    expect(result.stdout).toBe("LOCK demo-box\n");
  });
});

describe("small parsers", () => {
  test("isIpv4 accepts dotted quads only", () => {
    for (const ok of ["10.43.0.50", "0.0.0.0", "255.255.255.255"]) expect(isIpv4(ok)).toBe(true);
    for (const bad of ["10.43.0.256", "10.43.0", "10.43.0.050", " 10.43.0.5", "fd00::1", "ten.0.0.1"]) expect(isIpv4(bad)).toBe(false);
  });

  test("parseSecrets reads name, type, marker, key names and the size of each value", () => {
    const out = "sops-age\tOpaque\t<no value>\tage.agekey:100,\nflux-system\tOpaque\tdeploy-key\tknown_hosts:900,identity:548,identity.pub:136,\nodd\tOpaque\t\tx:,y,\n";
    expect(parseSecrets(out)).toEqual([
      { name: "flux-system", type: "Opaque", marker: MARKED_DEPLOY_KEY, keys: ["identity", "identity.pub", "known_hosts"], sizes: { known_hosts: 900, identity: 548, "identity.pub": 136 } },
      { name: "odd", type: "Opaque", marker: "", keys: ["x", "y"], sizes: { x: Number.NaN, y: Number.NaN } },
      { name: "sops-age", type: "Opaque", marker: "", keys: ["age.agekey"], sizes: { "age.agekey": 100 } },
    ]);
  });

  test("SECRET_TEMPLATE prints key names and the length of each value, never a value", () => {
    expect(SECRET_TEMPLATE).toContain("{{range $k, $v := .data}}{{$k}}:{{len $v}},{{end}}");
    expect(SECRET_TEMPLATE.match(/\{\{[^}]*\$v[^}]*\}\}/g)).toEqual(["{{range $k, $v := .data}}", "{{len $v}}"]);
  });

  test("isObjectName accepts DNS-1123 subdomains only, as the API server does for a Secret name", () => {
    // IsDNS1123Subdomain limits the whole name to 253 characters and no label to 63.
    for (const ok of ["flux-system", "sops-age", "a", "a.b-c", "a".repeat(253), `${"a".repeat(70)}.b`]) expect(isObjectName(ok)).toBe(true);
    for (const bad of ["", "Flux", "-a", "a-", "a b", "a\nb", "a'b", "$(id)", "a".repeat(254)]) expect(isObjectName(bad)).toBe(false);
  });

  test(
    "sshFingerprint matches ssh-keygen -l",
    () => {
      const dir = tempDir("demo-flux-key-");
      expect(spawn(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "test", "-f", join(dir, "key")]).code).toBe(0);
      const listed = spawn(["ssh-keygen", "-l", "-E", "sha256", "-f", join(dir, "key.pub")]).stdout.split(" ")[1];
      expect(sshFingerprint(readFileSync(join(dir, "key.pub"), "utf8"))).toBe(listed!);
      expect(redact(`${readFileSync(join(dir, "key.pub"), "utf8").trim()} ${listed}`)).toBe(`${readFileSync(join(dir, "key.pub"), "utf8").trim()} ${listed}`);
      expect(() => sshFingerprint("not a key")).toThrow("not an OpenSSH public key");
      const blob = readFileSync(join(dir, "key.pub"), "utf8").split(" ")[1]!;
      expect(() => sshFingerprint(`ssh-rsa ${blob} test`)).toThrow("not an OpenSSH public key");
      expect(() => sshFingerprint(`ssh-ed25519 ${blob.slice(0, 8)} test`)).toThrow("not an OpenSSH public key");
    },
    SLOW_TEST_MS,
  );

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

  test("codeProblems refuses files hidden from git status", () => {
    const problems = codeProblems({ ...CLEAN, hidden: ["S scripts/demo/flux.ts", "h .proto/plugins/kubeconform.toml"] });
    expect(problems).toHaveLength(1);
    expect(problems[0]).toContain("assume-unchanged or skip-worktree");
    expect(problems[0]).toContain("S scripts/demo/flux.ts; h .proto/plugins/kubeconform.toml");
    expect(problems[0]).toContain("git update-index --no-assume-unchanged --no-skip-worktree <file>");
  });

  test(
    "readCode fetches main and sees a moved origin, uncommitted files and files hidden from status",
    async () => {
      const base = tempDir("demo-flux-git-");
      const git = (cwd: string, ...args: string[]) => {
        const result = spawn(["git", "-c", "user.email=t@example.com", "-c", "user.name=t", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main", ...args], { cwd });
        if (result.code !== 0) throw new Error(result.stderr);
        return result.stdout.trim();
      };
      git(base, "init", "--bare", "origin.git");
      git(base, "clone", join(base, "origin.git"), "work");
      const work = join(base, "work");
      mkdirSync(join(work, "scripts"));
      mkdirSync(join(work, ".proto", "plugins"), { recursive: true });
      writeFileSync(join(work, "scripts", "a.ts"), "a\n");
      writeFileSync(join(work, ".proto", "plugins", "tool.toml"), "a\n");
      writeFileSync(join(work, "other.txt"), "a\n");
      git(work, "add", ".");
      git(work, "commit", "-m", "one");
      git(work, "push", "origin", "HEAD:main");
      const head = git(work, "rev-parse", "HEAD");
      expect(await readCode(work)).toEqual({ head, originMain: head, dirty: [], hidden: [], originUrl: join(base, "origin.git") });

      writeFileSync(join(work, "scripts", "b.ts"), "b\n");
      writeFileSync(join(work, ".proto", "plugins", "tool.toml"), "changed\n");
      expect((await readCode(work)).dirty).toEqual([" M .proto/plugins/tool.toml", "?? scripts/b.ts"]);
      rmSync(join(work, "scripts", "b.ts"));
      git(work, "checkout", "--", ".proto/plugins/tool.toml");

      // A file outside CODE_PATHS may carry the marks; one inside may not, and its change is invisible to status.
      git(work, "update-index", "--skip-worktree", "scripts/a.ts");
      git(work, "update-index", "--assume-unchanged", ".proto/plugins/tool.toml");
      git(work, "update-index", "--assume-unchanged", "other.txt");
      writeFileSync(join(work, "scripts", "a.ts"), "changed\n");
      const hidden = await readCode(work);
      expect(hidden.dirty).toEqual([]);
      expect(hidden.hidden).toEqual(["h .proto/plugins/tool.toml", "S scripts/a.ts"]);
      expect(codeProblems({ ...hidden, originUrl: CLEAN.originUrl })).toHaveLength(1);
      git(work, "update-index", "--no-skip-worktree", "scripts/a.ts");
      git(work, "update-index", "--no-assume-unchanged", ".proto/plugins/tool.toml");
      git(work, "checkout", "--", "scripts/a.ts");

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
    },
    SLOW_TEST_MS,
  );
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

  test("needs exactly one GitRepository and exactly one Kustomization, counted apart", () => {
    const documents = DEMO_MANIFESTS.split(/^---[ \t]*$/m);
    const gitDoc = documents.find((doc) => /^kind: GitRepository$/m.test(doc))!;
    const rootDoc = documents.find((doc) => /^kind: Kustomization$/m.test(doc))!;
    const secondGit = gitDoc.replace(/^  name: flux-system$/m, "  name: second");
    const secondRoot = rootDoc.replace(/^  name: flux-system$/m, "  name: second");
    // Two GitRepositories and no Kustomization: two documents of the two kinds, as before, but not one of each.
    const noRoot = documents.filter((doc) => doc !== rootDoc).join("---");
    const counted = parseManifests(`${noRoot}\n---\n${secondGit}`).problems;
    expect(counted).toContain("Flux's manifests hold 2 GitRepositories, not exactly one (the sync's)");
    expect(counted).toContain("Flux's manifests hold 0 Kustomizations, not exactly one (the root)");
    expect(parseManifests(`${DEMO_MANIFESTS}\n---\n${secondRoot}`).problems).toContain("Flux's manifests hold 2 Kustomizations, not exactly one (the root)");
  });

  test("needs the GitRepository to follow exactly main, under a source.toolkit apiVersion", () => {
    const pinned = DEMO_MANIFESTS.replace(/^    branch: main$/m, `    branch: main\n    commit: ${HEAD}`);
    expect(pinned).not.toBe(DEMO_MANIFESTS);
    expect(parseManifests(pinned).problems).toEqual(["the GitRepository's spec.ref is not exactly {branch: main}"]);
    expect(parseManifests(DEMO_MANIFESTS.replace(/^    branch: main$/m, "    tag: v1")).problems).toEqual(["the GitRepository's spec.ref is not exactly {branch: main}"]);
    const other = DEMO_MANIFESTS.replace(/^apiVersion: source\.toolkit\.fluxcd\.io\/(v\w+)\nkind: GitRepository$/m, "apiVersion: example.com/$1\nkind: GitRepository");
    expect(other).not.toBe(DEMO_MANIFESTS);
    expect(parseManifests(other).problems[0]).toContain("not source.toolkit.fluxcd.io/<version>");
    expect(parseManifests(DEMO_MANIFESTS).manifests?.gitApiVersion).toMatch(/^source\.toolkit\.fluxcd\.io\/v1$/);
  });

  test("refuses Secret names that are not Kubernetes object names: they go into YAML and shell text", () => {
    const deployKey = DEMO_MANIFESTS.replace(/^  secretRef:\n    name: flux-system$/m, "  secretRef:\n    name: \"x\\n  evil: 1\"");
    expect(deployKey).not.toBe(DEMO_MANIFESTS);
    expect(parseManifests(deployKey).problems).toEqual([`the GitRepository's secretRef "x\\n  evil: 1" is not a Kubernetes object name`]);
    const sops = DEMO_MANIFESTS.replace(/^      name: sops-age$/m, "      name: Sops_Age");
    expect(sops).not.toBe(DEMO_MANIFESTS);
    expect(parseManifests(sops).problems).toEqual(['the root Kustomization\'s decryption secretRef "Sops_Age" is not a Kubernetes object name']);
  });
});

describe("the pinned bootstrap", () => {
  const manifests = parseManifests(DEMO_MANIFESTS).manifests!;
  const isKind = (kind: string) => (doc: unknown) => (doc as { kind?: string } | null)?.kind === kind;
  const docs = (text: string) => {
    const parsed: unknown = Bun.YAML.parse(text);
    return (Array.isArray(parsed) ? parsed : [parsed]).filter((doc) => doc !== null && doc !== undefined);
  };

  test("pins the GitRepository to the checked commit and moves the root Kustomization out, to be applied last", () => {
    const bootstrap = pinnedBootstrap(manifests, HEAD);
    expect(bootstrap.head).toBe(HEAD);
    expect(bootstrap.sha256).toBe(sha256(bootstrap.text));
    expect(bootstrap.apiVersion).toBe(manifests.gitApiVersion);
    const applied = docs(bootstrap.text);
    expect(applied.filter(isKind("Kustomization"))).toEqual([]);
    const git = applied.filter(isKind("GitRepository")) as Record<string, unknown>[];
    expect(git).toHaveLength(1);
    expect(gitRefOf(git[0])).toEqual({ kind: "pinned", commit: HEAD });
    const root = JSON.parse(bootstrap.root) as Record<string, unknown>;
    expect(root.kind).toBe("Kustomization");
    expect((root.spec as { path: string }).path).toBe("./deploy/flux/clusters/demo");
    // Everything else is as main has it, byte for byte.
    const original = docs(DEMO_MANIFESTS);
    expect(applied.filter((doc) => !isKind("GitRepository")(doc))).toEqual(original.filter((doc) => !isKind("GitRepository")(doc) && !isKind("Kustomization")(doc)));
    expect(bootstrap.text).toContain(COMPONENTS.slice(0, 2000));
  });

  test("refuses a commit that is not a full hash", () => {
    for (const head of ["", "a".repeat(39), "A".repeat(40), `${"a".repeat(39)}'`]) expect(() => pinnedBootstrap(manifests, head)).toThrow("not a full commit hash");
  });

  test("pinDocument names only the GitRepository and its commit", () => {
    expect(JSON.parse(pinDocument(manifests.gitApiVersion, HEAD))).toEqual({
      apiVersion: manifests.gitApiVersion,
      kind: "GitRepository",
      metadata: { name: "flux-system", namespace: "flux-system" },
      spec: { ref: { commit: HEAD } },
    });
    expect(() => pinDocument("v1", HEAD)).toThrow();
    expect(() => pinDocument(manifests.gitApiVersion, "main")).toThrow();
  });

  test("gitRefOf tells following main, pinned to a commit and anything else apart", () => {
    const ref = (value: unknown) => gitRefOf({ spec: { ref: value } });
    expect(ref({ branch: "main" })).toEqual({ kind: "main" });
    expect(ref({ branch: "main", commit: HEAD })).toEqual({ kind: "pinned", commit: HEAD });
    expect(ref({ branch: "main", commit: "abc" }).kind).toBe("other");
    expect(ref({ branch: "dev" })).toEqual({ kind: "other", ref: '{"branch":"dev"}' });
    expect(ref({ branch: "main", tag: "v1" }).kind).toBe("other");
    expect(gitRefOf(undefined)).toEqual({ kind: "other", ref: "null" });
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
      ai: { providers: { anthropic: { apiKeySecret: { name: "trustiot-anthropic" } } } },
      services: { identity: { enabled: true, credentials: ["smtp"] }, "ai-gateway": { enabled: false, credentials: ["ai"] } },
    };
    const release = { spec: { valuesFrom: [{ kind: "Secret", name: "optional-one", optional: true }, { kind: "ConfigMap", name: "c" }] } };
    const { secrets, problems } = requiredSecrets({}, release, values);
    expect(problems).toEqual([]);
    expect(Object.keys(secrets)).toEqual(["trustiot-smtp"]);
    // The AI gateway, once the demo values turn it on (deploy/flux/README.md, 「展示叢集」), needs the
    // Anthropic key's Secret.
    const gatewayOn = { ...values, services: { ...values.services, "ai-gateway": { enabled: true, credentials: ["ai"] } } };
    expect(Object.keys(requiredSecrets({}, release, gatewayOn).secrets)).toEqual(["trustiot-smtp", "trustiot-anthropic"]);
    expect(requiredSecrets({}, {}, { services: { x: { enabled: true, credentials: ["unknown"] } } }).problems[0]).toContain("does not know");
  });
});

describe("readRepo", () => {
  test("main as it is today is ready for the install", async () => {
    const repo = await readRepo(mainAsIs(), CLEAN, build);
    expect([...repo.code, ...repo.flux, ...repo.deploy]).toEqual([]);
    expect(Object.keys(repo.secrets)).toContain("ghcr-pull");
  });

  test("main before Laysi/trustiot-v2#480: the placeholder chart, no ClusterIP and no pull Secret are refused", async () => {
    const repo = await readRepo(mainUnready(), CLEAN, build);
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
    expect(repo.chart).toEqual({ version: "0.0.0-rc.10", digest: DIGEST });
    expect(repo.clusterIp).toBe(CLUSTER_IP);
    expect(Object.keys(repo.secrets).sort()).toEqual(["ghcr-pull", "trustiot-demo-values"]);
    expect(repo.manifests?.objects).toHaveLength(27);
  });

  test("(a) orders release candidates by number: rc.9 is before rc.10, rc.11 is after", async () => {
    expect((await readRepo(mainReady("0.0.0-rc.11"), CLEAN, build)).deploy).toEqual([]);
    expect((await readRepo(mainReady(DEMO_MIN_CHART_VERSION), CLEAN, build)).deploy).toEqual([]);
    expect((await readRepo(mainReady("0.0.0-rc.9"), CLEAN, build)).deploy[0]).toContain("pins chart 0.0.0-rc.9, older than");
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
      const root = mainUnready();
      pinChart(root, "0.0.0-rc.10");
      addPullSecret(root);
      setClusterIp(root, bad);
      const deploy = (await readRepo(root, CLEAN, build)).deploy;
      expect(deploy).toHaveLength(1);
      expect(deploy[0]).toContain("to an IPv4 address (it is");
    }
  });

  test("(e) a needed SOPS file that kustomization.yaml does not list, or that is not encrypted, is refused", async () => {
    const unlisted = mainUnready();
    pinChart(unlisted, "0.0.0-rc.10");
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
  secrets: [{ name: "sops-age", type: "Opaque", marker: "", keys: ["age.agekey"], sizes: { "age.agekey": 100 } }],
  ...overrides,
});

const withDeployKey = (ns: NamespaceState, marker = MARKED_DEPLOY_KEY): NamespaceState => ({
  ...ns,
  objects: [...ns.objects, { ref: "secret/flux-system", owned: false, managed: false }],
  secrets: [...ns.secrets, { name: "flux-system", type: "Opaque", marker, keys: ["identity", "identity.pub", "known_hosts"], sizes: { identity: 548, "identity.pub": 136, known_hosts: 900 } }],
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

  test("a deploy key Secret with an empty or unreadable value is incomplete (a deploy-key run that stopped while making it)", () => {
    const cases: Record<string, number>[] = [{ identity: 0, "identity.pub": 136, known_hosts: 900 }, { identity: 548, "identity.pub": Number.NaN, known_hosts: 900 }, { identity: 548, known_hosts: 900 }];
    for (const sizes of cases) {
      const ns = withDeployKey(ageKeyNamespace());
      ns.secrets[1]!.sizes = sizes;
      const problem = deployKeyProblem(ns, key, "flux-system", knownHosts);
      expect(problem).toContain("has an empty");
      expect(problem).toContain("a deploy-key run stopped while making it");
      expect(problem).toContain("`sudo k3s kubectl -n flux-system delete secret flux-system`");
    }
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
  const wanted = wantedRevisions(CLEAN.head, { version: "0.0.0-rc.10", digest: DIGEST });

  test("wantedRevisions names main's commit, the chart's digest and the version helm-controller records", () => {
    expect(wanted).toEqual({ git: `main@sha1:${CLEAN.head}`, chartDigest: DIGEST, release: `0.0.0-rc.10+${"b".repeat(12)}` });
  });

  test("revisionOf reads each kind's revision, the chart digest from either form, and only a deployed release", () => {
    expect(revisionOf("git", at({ artifact: { revision: wanted.git } }))).toBe(wanted.git);
    expect(revisionOf("kustomization", at({ lastAppliedRevision: wanted.git }))).toBe(wanted.git);
    expect(revisionOf("chart", at({ artifact: { revision: DIGEST } }))).toBe(DIGEST);
    expect(revisionOf("chart", at({ artifact: { revision: `0.0.0-rc.10@${DIGEST}` } }))).toBe(DIGEST);
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

  const other = "c".repeat(40);
  const git = (ref: Record<string, unknown>, revision: string, generation = 2, observed = 2, ready = "True") => ({
    metadata: { generation },
    spec: { ref },
    status: { observedGeneration: observed, conditions: [{ type: "Ready", status: ready, reason: "Succeeded", message: "stored artifact" }], artifact: { revision } },
  });

  test("pinProblem: nothing while the pin and the artifact are this run's, or the artifact is of an older generation", () => {
    expect(pinProblem(undefined, HEAD)).toBeUndefined();
    expect(pinProblem(git({ branch: "main", commit: HEAD }, `main@sha1:${HEAD}`), HEAD)).toBeUndefined();
    expect(pinProblem(git({ branch: "main", commit: HEAD }, "", 1, 0), HEAD)).toBeUndefined();
    // An earlier run's pin, until source-controller observes the new generation.
    expect(pinProblem(git({ branch: "main", commit: HEAD }, `main@sha1:${other}`, 3, 2), HEAD)).toBeUndefined();
  });

  test("pinProblem: another commit for the current generation, or spec.ref changed, stops the wait", () => {
    const moved = pinProblem(git({ branch: "main", commit: HEAD }, `main@sha1:${other}`), HEAD);
    expect(moved).toContain(`is pinned to ${HEAD} but reports main@sha1:${other} for its current generation`);
    expect(moved).toContain("run install again (dry run first)");
    expect(pinProblem(git({ branch: "main" }, `main@sha1:${other}`), HEAD)).toContain(`is no longer pinned to ${HEAD}, which this run checked (spec.ref is now {branch: main})`);
    expect(pinProblem(git({ branch: "main", commit: other }, `main@sha1:${other}`), HEAD)).toContain(`(spec.ref is now {branch: main, commit: ${other}})`);
    expect(pinProblem(git({ tag: "v1" }, ""), HEAD)).toContain('(spec.ref is now {"tag":"v1"})');
  });

  test("followsMain: spec.ref exactly main, Ready for the generation, at a commit of main", () => {
    expect(followsMain(git({ branch: "main" }, `main@sha1:${other}`))).toEqual({ done: true, state: `Ready=True Succeeded: stored artifact; at main@sha1:${other}` });
    expect(followsMain(git({ branch: "main" }, `main@sha1:${HEAD}`, 3, 2)).done).toBe(false);
    expect(followsMain(git({ branch: "main" }, "", 2, 2)).state).toContain("waiting for a commit of main");
    expect(followsMain(git({ branch: "main" }, `main@sha1:${other}`, 2, 2, "False")).done).toBe(false);
    expect(followsMain(git({ branch: "main", commit: HEAD }, `main@sha1:${HEAD}`)).failed).toBe(`spec.ref is {branch: main, commit: ${HEAD}}, not {branch: main}`);
    expect(followsMain(undefined).failed).toBe("it is gone");
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
    spawn(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "trustiot-demo-flux", "-f", join(dir, "key")]);
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

  test("everything in place: label, apply pinned to the checked commit, no refusals", () => {
    const plan = planInstall(repo, readyCluster(repo), { kind: "free" }, okAge);
    expect(plan.refusals).toEqual([]);
    expect(plan).toMatchObject({ namespace: "age-key", namespaceUid: "uid-1", mark: true, apply: true, pin: { step: "apply" }, clusterIp: CLUSTER_IP, deployKeyFingerprint: sshFingerprint(DEPLOY_KEY_PUB) });
    const bootstrap = pinnedBootstrap(repo.manifests!, repo.head);
    expect(plan.bootstrapSha256).toBe(sha256(`${bootstrap.text}\n---\n${bootstrap.root}`));
  });

  /** This script's install as a re-run finds it: Flux's objects there, the GitRepository as `ref` says. */
  const installed = (ref: GitRef, change: (cluster: ClusterFacts) => void = () => {}) => {
    const cluster = readyCluster(repo);
    cluster.namespace = withDeployKey(ageKeyNamespace({ labels: { [MARKER]: MARKED_INSTALL } }));
    cluster.fluxCrds = repo.manifests!.crds;
    cluster.rootKustomization = { suspended: false, prune: true };
    cluster.gitRepository = { ref, suspended: false };
    cluster.helmRelease = { suspended: false };
    cluster.appNamespace = { labels: { "kustomize.toolkit.fluxcd.io/name": "flux-system", "kustomize.toolkit.fluxcd.io/namespace": "flux-system" }, terminating: false };
    change(cluster);
    return planInstall(repo, cluster, { kind: "free" }, okAge);
  };

  test("a re-run over this script's install does not label or apply again once Flux manages itself", () => {
    const plan = installed({ kind: "main" });
    expect(plan.refusals).toEqual([]);
    expect(plan).toMatchObject({ namespace: "ours", mark: false, apply: false, pin: { step: "none" }, bootstrapSha256: "", notes: [] });
  });

  test("a re-run after an interruption while pinned finishes the pin, and moves it to the new head first if main moved", () => {
    expect(installed({ kind: "pinned", commit: repo.head })).toMatchObject({ refusals: [], apply: false, pin: { step: "finish" } });
    expect(installed({ kind: "pinned", commit: repo.head }).pin.from).toBeUndefined();
    const older = "c".repeat(40);
    expect(installed({ kind: "pinned", commit: older })).toMatchObject({ refusals: [], apply: false, pin: { step: "finish", from: older } });
    // Stopped before the root Kustomization existed: the bootstrap apply runs again, after moving the pin.
    const early = planInstall(repo, { ...readyCluster(repo), gitRepository: { ref: { kind: "pinned", commit: older }, suspended: false } }, { kind: "free" }, okAge);
    expect(early).toMatchObject({ refusals: [], apply: true, pin: { step: "apply", from: older } });
  });

  test("a GitRepository that is neither main nor a pin, or missing beside the root Kustomization, is refused", () => {
    expect(installed({ kind: "other", ref: '{"tag":"v1"}' }).refusals).toEqual([
      'GitRepository flux-system/flux-system has spec.ref {"tag":"v1"}: neither main nor a pin of an earlier install, so this script does not know what Flux applies; look at it by hand',
    ]);
    expect(installed({ kind: "main" }, (c) => delete c.gitRepository).refusals[0]).toContain("the root Kustomization flux-system exists but GitRepository flux-system/flux-system does not");
  });

  test("(L2) a suspended root Kustomization, GitRepository or HelmRelease, or prune off, is refused", () => {
    const remove = installed({ kind: "main" }, (c) => (c.rootKustomization = { suspended: true, prune: false })).refusals;
    expect(remove).toEqual([
      "the root Kustomization flux-system has spec.suspend: true, spec.prune: false, as a remove that stopped halfway leaves it, so Flux does not apply main: run remove again (dry run first) to finish it, or resume Flux as deploy/flux/README.md 「在展示機安裝 Flux」 (中斷之後) describes",
    ]);
    expect(installed({ kind: "main" }, (c) => (c.rootKustomization = { suspended: false, prune: false })).refusals[0]).toContain("has spec.prune: false, as");
    expect(installed({ kind: "main" }, (c) => (c.gitRepository = { ref: { kind: "main" }, suspended: true })).refusals[0]).toContain("GitRepository flux-system/flux-system is suspended");
    expect(installed({ kind: "main" }, (c) => (c.helmRelease = { suspended: true })).refusals[0]).toContain("HelmRelease trustiot/trustiot is suspended");
    expect(suspendedProblems(readyCluster(repo))).toEqual([]);
  });

  const refusalsWith = (change: (cluster: ClusterFacts) => void, lock: Parameters<typeof planInstall>[2] = { kind: "free" }, age: SopsAgeState = okAge) => {
    const cluster = readyCluster(repo);
    change(cluster);
    return planInstall(repo, cluster, lock, age).refusals;
  };

  test("(a), (e) and the ClusterIP: main's problems are refusals", async () => {
    const unready = await readRepo(mainUnready(), CLEAN, build);
    const refusals = planInstall(unready, readyCluster(unready), { kind: "free" }, okAge).refusals;
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

  test("the pinned ClusterIP must be in the static band of the cluster's one IPv4 Service range, and free or the release's own", () => {
    const value = `ingress.tunnel.clusterIP ${CLUSTER_IP} of values-demo.yaml`;
    const how = "pin an address of the static band (deploy/chart/README.md 「固定的入口 ClusterIP」, Laysi/trustiot-v2#478)";
    expect(clusterIpProblems(CLUSTER_IP, readyCluster(repo))).toEqual([]);
    expect(refusalsWith((c) => (c.serviceCidrs = ["10.96.0.0/12"]))).toEqual([
      `${value} is outside the static band 10.96.0.1-10.96.1.0 of the Service range 10.96.0.0/12: a Service created before the ingress one could take it, and the first install would fail; ${how}`,
    ]);
    expect(refusalsWith((c) => delete c.serviceCidrs)).toEqual([`could not read the cluster's Service range (ServiceCIDR kubernetes, networking.k8s.io/v1), so ${value} cannot be checked against its static band`]);
    expect(refusalsWith((c) => (c.serviceCidrs = []))[0]).toContain("expected one IPv4 range in ServiceCIDR kubernetes, found []");
    expect(refusalsWith((c) => (c.serviceCidrs = ["10.43.0.0/16", "10.44.0.0/16"]))[0]).toContain("expected one IPv4 range");
    // A dual-stack range: the IPv4 one counts.
    expect(refusalsWith((c) => (c.serviceCidrs = ["10.43.0.0/16", "fd00:43::/112"]))).toEqual([]);
    expect(refusalsWith((c) => (c.serviceCidrs = ["10.43.0.5/16"]))[0]).toContain("the cluster's Service range is not an IPv4 CIDR (10.43.0.5/16 has host bits set)");
    expect(refusalsWith((c) => (c.serviceCidrs = ["10.43.0.48/28"]))[0]).toContain("is too small to have a static band");
    expect(refusalsWith((c) => c.services.push({ ref: "lab/proxy", ips: ["10.43.0.9", CLUSTER_IP], release: "/" }))).toEqual([
      `${value} is already the ClusterIP of Service lab/proxy: the release could not create its ingress Service with it; pin another address (Laysi/trustiot-v2#478)`,
    ]);
    // Another release, or the right release name in another namespace, is someone else's.
    expect(refusalsWith((c) => c.services.push({ ref: "trustiot/other", ips: [CLUSTER_IP], release: "trustiot/other" }))).toHaveLength(1);
    expect(refusalsWith((c) => c.services.push({ ref: "e2e-1/trustiot-ingress", ips: [CLUSTER_IP], release: "trustiot/trustiot" }))).toHaveLength(1);
    expect(refusalsWith((c) => c.services.push({ ref: "trustiot/trustiot-ingress", ips: [CLUSTER_IP], release: "trustiot/trustiot" }))).toEqual([]);
  });

  test("the band's edges: not the network or broadcast address, first and last of the band only", () => {
    // Without Service default/kubernetes, which holds the band's first address on a real cluster.
    const cluster = { ...readyCluster(repo), services: [] };
    const value = (ip: string) => `ingress.tunnel.clusterIP ${ip} of values-demo.yaml`;
    expect(clusterIpProblems("10.43.0.0", cluster)).toEqual([`${value("10.43.0.0")} is the network address of the Service range 10.43.0.0/16; pin an address of the static band (deploy/chart/README.md 「固定的入口 ClusterIP」, Laysi/trustiot-v2#478)`]);
    expect(clusterIpProblems("10.43.255.255", cluster)[0]).toContain("is the broadcast address of the Service range 10.43.0.0/16");
    expect(clusterIpProblems("10.43.0.1", cluster)).toEqual([]);
    expect(clusterIpProblems("10.43.1.0", cluster)).toEqual([]);
    expect(clusterIpProblems("10.43.1.1", cluster)[0]).toContain("is outside the static band 10.43.0.1-10.43.1.0");
    expect(clusterIpProblems("10.43.2.5", cluster)[0]).toContain("is outside the static band");
    expect(clusterIpProblems("10.44.0.5", cluster)[0]).toContain("is outside the static band");
  });
});

describe("Service addresses", () => {
  test("parseServices reads the lines boxOps asks for", () => {
    expect(parseServices("default/kubernetes\t10.43.0.1\t/\nlab/dual\t10.43.0.7 fd00::7\ttrustiot/trustiot\nlab/name\t\t/\n\n")).toEqual([
      { ref: "default/kubernetes", ips: ["10.43.0.1"], release: "/" },
      { ref: "lab/dual", ips: ["10.43.0.7", "fd00::7"], release: "trustiot/trustiot" },
      { ref: "lab/name", ips: [], release: "/" },
    ]);
    expect(() => parseServices("no-namespace\t10.43.0.1\t/\n")).toThrow("unexpected Service line");
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
  return spawn(["bash", "-c", script], { stdin, env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, ...env } });
}

/** What a root script leaves in its base directory: nothing but the flock file. */
const leftovers = (base: string) => readdirSync(base).filter((name) => name !== ".lock");

/** Holds the root scripts' flock on `base` from another process, as a running root script does, until the returned function is called. */
async function holdScriptLock(base: string): Promise<() => void> {
  mkdirSync(base, { recursive: true, mode: 0o700 });
  const holder = Bun.spawn(["bash", "-c", 'exec 9>"$1/.lock"; flock -w 60 9 || { echo "could not take the lock within 60 s"; exit 1; }; echo held; exec sleep 600', "hold", base], { stdout: "pipe", stderr: "pipe" });
  const { value } = await holder.stdout.getReader().read();
  expect(new TextDecoder().decode(value)).toBe("held\n");
  return () => holder.kill();
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
  const bin = () => fakeBin({ k3s: FAKE_K3S, uname: 'echo "${FAKE_ARCH:-aarch64}"' });

  const check = (env: Record<string, string>, bytes: Uint8Array = tool, base = join(tempDir("demo-flux-run-"), "base")) => {
    const result = runScript(ageCheckScript(base, toolSha, "sops-age"), bytes, bin(), env);
    // The run's directory is gone afterwards, whatever happened.
    expect(leftovers(base)).toEqual([]);
    expect(`${result.stdout}${result.stderr}`).not.toContain(PRIVATE_AGE_KEY);
    return result;
  };
  const b64 = (text: string) => Buffer.from(text).toString("base64");

  test(
    "derives the public key of a valid identity and prints nothing else",
    () => {
      const result = check({ FAKE_STORED: "present", FAKE_KEYS: "age.agekey", FAKE_VALUE: b64(`# created: x\n${PRIVATE_AGE_KEY}\n`) });
      expect(result.code).toBe(0);
      expect(result.stdout).toBe("SOPS_AGE ok age1fakerecipient\n");
      expect(parseAgeCheck(result.stdout)).toEqual({ kind: "ok", recipients: ["age1fakerecipient"] });
    },
    SLOW_TEST_MS,
  );

  test(
    "reports missing, other keys, empty and invalid without the key's text",
    () => {
      expect(check({}).stdout).toBe("SOPS_AGE missing\n");
      expect(check({ FAKE_STORED: "present", FAKE_KEYS: "other.key" }).stdout).toBe("SOPS_AGE keys other.key\n");
      expect(check({ FAKE_STORED: "present", FAKE_KEYS: "" }).stdout).toBe("SOPS_AGE keys\n");
      expect(check({ FAKE_STORED: "present", FAKE_KEYS: "age.agekey", FAKE_VALUE: "" }).stdout).toBe("SOPS_AGE empty\n");
      const invalid = check({ FAKE_STORED: "present", FAKE_KEYS: "age.agekey", FAKE_VALUE: b64(`${PRIVATE_AGE_KEY.slice(0, -1)}X`) });
      expect(invalid.stdout).toBe("SOPS_AGE invalid\n");
      expect(invalid.stderr).toBe("");
    },
    SLOW_TEST_MS,
  );

  test(
    "checks the tool's hash and the machine's architecture before reading the Secret",
    () => {
      const changed = check({ FAKE_STORED: "present", FAKE_KEYS: "age.agekey" }, new TextEncoder().encode(`${FAKE_AGE_KEYGEN}\n# changed`));
      expect(changed.code).toBe(1);
      expect(changed.stderr).toContain("age-keygen arrived changed");
      expect(check({ FAKE_ARCH: "x86_64" }).stdout).toBe("SOPS_AGE arch x86_64\n");
    },
    SLOW_TEST_MS,
  );

  test(
    "(L5) shreds the run directories a stopped run left behind, like every root script",
    () => {
      const base = join(tempDir("demo-flux-run-"), "base");
      mkdirSync(join(base, "deploy-key.AbCd1234"), { recursive: true });
      writeFileSync(join(base, "deploy-key.AbCd1234", "key"), "left by a killed run");
      const result = check({}, tool, base);
      expect(result.stdout).toBe("STALE removed a run directory that a stopped run left behind\nSOPS_AGE missing\n");
      expect(readdirSync(base)).toEqual([".lock"]);
    },
    SLOW_TEST_MS,
  );

  test(
    "(L5) refuses to run beside another root script of this file, and leaves that one's directory alone",
    async () => {
      const base = join(tempDir("demo-flux-run-"), "base");
      const release = await holdScriptLock(base);
      try {
        mkdirSync(join(base, "apply.AbCd1234"));
        writeFileSync(join(base, "apply.AbCd1234", "flux-system.yaml"), "in use");
        const result = runScript(ageCheckScript(base, toolSha, "sops-age"), tool, bin(), {});
        expect(result.code).toBe(1);
        expect(result.stderr).toBe("another root script of scripts/demo/flux.ts is running on the demo machine; wait until it ends\n");
        expect(result.stdout).toBe("");
        expect(readFileSync(join(base, "apply.AbCd1234", "flux-system.yaml"), "utf8")).toBe("in use");
      } finally {
        release();
      }
    },
    SLOW_TEST_MS,
  );

  test("refuses a Secret name that is not a Kubernetes object name", () => {
    expect(() => ageCheckScript(REMOTE_BASE, toolSha, "x'; rm -rf /; '")).toThrow("is not a Kubernetes object name");
    expect(() => deployKeyScript(REMOTE_BASE, toolSha, "Flux")).toThrow("is not a Kubernetes object name");
  });
});

describe("deployKeyScript", () => {
  const knownHosts = readFileSync(join(ROOT, KNOWN_HOSTS_FILE), "utf8");
  const FAKE_K3S = [
    'shift; args="$*"; echo "$args" >> "$FAKE_STATE/calls"',
    'case "$args" in',
    '  *"get secret flux-system --ignore-not-found -o name"*) if [ -f "$FAKE_STATE/created.yaml" ]; then echo secret/flux-system; fi; exit 0 ;;',
    '  "create --field-manager=trustiot-demo-flux -f -") cat > "$FAKE_STATE/created.yaml"; if [ -n "${FAKE_CREATE_FAIL:-}" ]; then printf "%s\\n" "$FAKE_CREATE_FAIL" >&2; rm "$FAKE_STATE/created.yaml"; exit 1; fi; exit 0 ;;',
    '  *"jsonpath={.data.identity}"*) if [ -n "${FAKE_READBACK:-}" ]; then printf "%s" "$FAKE_READBACK"; exit 0; fi; sed -n "s/^  identity: //p" "$FAKE_STATE/created.yaml" | tr -d "\\n"; exit 0 ;;',
    '  *"jsonpath={.data.identity\\.pub}"*) sed -n "s/^  identity\\.pub: //p" "$FAKE_STATE/created.yaml" | tr -d "\\n"; exit 0 ;;',
    '  *"jsonpath={.data.known_hosts}"*) sed -n "s/^  known_hosts: //p" "$FAKE_STATE/created.yaml" | tr -d "\\n"; exit 0 ;;',
    '  *"delete secret flux-system --ignore-not-found"*) rm -f "$FAKE_STATE/created.yaml"; exit 0 ;;',
    "esac",
    'echo "unexpected k3s $args" >&2; exit 2',
  ].join("\n");
  // base64 that fails, or prints nothing, for one file; the real one (next on PATH) otherwise.
  const FAKE_BASE64 = [
    'if [ "$1" = -w0 ] && [ "${2##*/}" = "${FAKE_BASE64_FILE:-none}" ]; then',
    '  case "${FAKE_BASE64:-}" in fail) echo "base64: read error" >&2; exit 1 ;; empty) exit 0 ;; esac',
    "fi",
    'PATH="${PATH#*:}" exec base64 "$@"',
  ].join("\n");

  const setup = () => {
    const state = tempDir("demo-flux-state-");
    const base = join(tempDir("demo-flux-run-"), "base");
    const bin = fakeBin({ k3s: FAKE_K3S, base64: FAKE_BASE64 });
    return { state, base, bin, run: (env: Record<string, string> = {}) => runScript(deployKeyScript(base, sha256(knownHosts), "flux-system"), knownHosts, bin, { FAKE_STATE: state, ...env }) };
  };
  const calls = (state: string) => (existsSync(join(state, "calls")) ? readFileSync(join(state, "calls"), "utf8").trim().split("\n") : []);

  test(
    "generates the key, stores it marked with the host keys, reads it back, prints only the public key and shreds the files",
    () => {
      const { state, base, run } = setup();
      mkdirSync(join(base, "deploy-key.stale"), { recursive: true });
      writeFileSync(join(base, "deploy-key.stale", "key"), "left by a killed run");
      const result = run();
      expect(result.code).toBe(0);
      expect(result.stderr).toBe("");
      const lines = result.stdout.trim().split("\n");
      expect(lines[0]).toBe("STALE removed a run directory that a stopped run left behind");
      expect(lines[1]).toMatch(/^PUBLIC ssh-ed25519 [A-Za-z0-9+/=]+ trustiot-demo-flux$/);
      expect(lines[2]).toBe("DEPLOY_KEY created");
      expect(leftovers(base)).toEqual([]);

      const secret = Bun.YAML.parse(readFileSync(join(state, "created.yaml"), "utf8")) as { metadata: { name: string; namespace: string; labels: Record<string, string> }; type: string; data: Record<string, string> };
      expect(secret.metadata).toEqual({ name: "flux-system", namespace: "flux-system", labels: { [MARKER]: MARKED_DEPLOY_KEY } });
      expect(secret.type).toBe("Opaque");
      expect(Object.keys(secret.data).sort()).toEqual(["identity", "identity.pub", "known_hosts"]);
      const identity = Buffer.from(secret.data.identity!, "base64").toString();
      expect(identity).toStartWith(PEM.begin);
      expect(Buffer.from(secret.data["identity.pub"]!, "base64").toString().trim()).toBe(lines[1]!.slice("PUBLIC ".length));
      expect(Buffer.from(secret.data.known_hosts!, "base64").toString()).toBe(knownHosts);
      expect(result.stdout).not.toContain("PRIVATE");
      expect(result.stdout).not.toContain(secret.data.identity!);
      // kubectl got the Secret on stdin, never on its command line; each value was read back.
      expect(readFileSync(join(state, "calls"), "utf8")).not.toContain(secret.data.identity!);
      expect(calls(state)).toEqual([
        "-n flux-system get secret flux-system --ignore-not-found -o name",
        "create --field-manager=trustiot-demo-flux -f -",
        "-n flux-system get secret flux-system -o jsonpath={.data.identity}",
        "-n flux-system get secret flux-system -o jsonpath={.data.identity\\.pub}",
        "-n flux-system get secret flux-system -o jsonpath={.data.known_hosts}",
      ]);
    },
    SLOW_TEST_MS,
  );

  test(
    "leaves an existing Secret alone",
    () => {
      const { state, base, run } = setup();
      writeFileSync(join(state, "created.yaml"), "existing");
      const result = run();
      expect(result.code).toBe(0);
      expect(result.stdout).toBe("DEPLOY_KEY exists\n");
      expect(readFileSync(join(state, "created.yaml"), "utf8")).toBe("existing");
      expect(leftovers(base)).toEqual([]);
    },
    SLOW_TEST_MS,
  );

  test(
    "passes kubectl's errors on without lines that look like key material",
    () => {
      const { state, base, run } = setup();
      const result = run({ FAKE_CREATE_FAIL: `error: bad data ${"QUJD".repeat(20)}\n${PEM.begin}\nError from server (Forbidden): secrets is forbidden` });
      expect(result.code).toBe(1);
      expect(result.stderr).toBe("Error from server (Forbidden): secrets is forbidden\n");
      expect(result.stdout).toBe("");
      expect(leftovers(base)).toEqual([]);
      // kubectl did not create it, so there is nothing to delete.
      expect(calls(state).some((call) => call.includes("delete"))).toBe(false);
    },
    SLOW_TEST_MS,
  );

  test(
    "refuses host keys that arrived changed",
    () => {
      const { base, bin, state } = setup();
      const result = runScript(deployKeyScript(base, sha256(knownHosts), "flux-system"), `${knownHosts}#`, bin, { FAKE_STATE: state });
      expect(result.code).toBe(1);
      expect(result.stderr).toContain("known_hosts arrived changed");
      expect(existsSync(join(state, "calls"))).toBe(false);
    },
    SLOW_TEST_MS,
  );

  test(
    "(L5) stores nothing when a value cannot be read or comes out empty",
    () => {
      for (const [mode, file] of [
        ["fail", "key"],
        ["empty", "key"],
        ["fail", "known_hosts"],
        ["empty", "key.pub"],
      ] as const) {
        const { state, base, run } = setup();
        const result = run({ FAKE_BASE64: mode, FAKE_BASE64_FILE: file });
        expect(result.code).toBe(1);
        expect(result.stderr).toBe(mode === "fail" ? "base64: read error\n" : "the deploy key files are empty; nothing was stored\n");
        expect(result.stdout).toBe("");
        expect(calls(state)).toEqual(["-n flux-system get secret flux-system --ignore-not-found -o name"]);
        expect(existsSync(join(state, "created.yaml"))).toBe(false);
        expect(leftovers(base)).toEqual([]);
      }
    },
    SLOW_TEST_MS,
  );

  test(
    "(L5) deletes the Secret again when what was stored differs from the generated files",
    () => {
      const { state, base, run } = setup();
      const result = run({ FAKE_READBACK: Buffer.from("something else").toString("base64") });
      expect(result.code).toBe(1);
      expect(result.stderr).toBe("the stored Secret differs from the generated files\ndeleted the incomplete Secret flux-system/flux-system\n");
      expect(result.stdout).not.toContain("PUBLIC");
      expect(existsSync(join(state, "created.yaml"))).toBe(false);
      expect(calls(state).at(-1)).toBe("-n flux-system delete secret flux-system --ignore-not-found");
      expect(leftovers(base)).toEqual([]);
    },
    SLOW_TEST_MS,
  );

  test(
    "(L5) two runs never share a key directory: the second one waits for nothing and stops",
    async () => {
      const { state, base, run } = setup();
      const release = await holdScriptLock(base);
      try {
        const result = run();
        expect(result.code).toBe(1);
        expect(result.stderr).toContain("another root script of scripts/demo/flux.ts is running");
        expect(existsSync(join(state, "calls"))).toBe(false);
      } finally {
        release();
      }
    },
    SLOW_TEST_MS,
  );
});

describe("applyScript and deleteScript", () => {
  const manifests = parseManifests(DEMO_MANIFESTS).manifests!;
  const bootstrap = pinnedBootstrap(manifests, HEAD);
  const FAKE_K3S = [
    'shift; args="$*"; echo "$args" >> "$FAKE_STATE/calls"',
    'case "$args" in',
    '  "apply --server-side -f -") cat > "$FAKE_STATE/root.json"; echo "kustomization.kustomize.toolkit.fluxcd.io/flux-system serverside-applied"; exit 0 ;;',
    '  "apply --server-side --field-manager=trustiot-demo-flux -f -") cat > "$FAKE_STATE/pin.json"; if [ -n "${FAKE_PIN_FAIL:-}" ]; then echo "error: Apply failed with 1 conflict" >&2; exit 1; fi; echo "gitrepository.source.toolkit.fluxcd.io/flux-system serverside-applied"; exit 0 ;;',
    '  "apply --server-side -f "*) cp "$4" "$FAKE_STATE/applied.yaml"; n=$(cat "$FAKE_STATE/applies" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$FAKE_STATE/applies"; if [ $n -eq 1 ]; then echo "error: no matches for kind GitRepository" >&2; exit 1; fi; [ -z "${FAKE_SECOND_FAIL:-}" ] || exit 1; echo "namespace/flux-system serverside-applied"; exit 0 ;;',
    '  "wait --for=condition=established --timeout=120s "*) exit 0 ;;',
    '  "delete -f "*" --ignore-not-found --wait=false") echo "namespace \\"flux-system\\" deleted"; exit 0 ;;',
    "esac",
    'echo "unexpected k3s $args" >&2; exit 2',
  ].join("\n");
  const setup = () => ({ state: tempDir("demo-flux-state-"), base: join(tempDir("demo-flux-run-"), "base"), bin: fakeBin({ k3s: FAKE_K3S }) });
  const calls = (state: string) => (existsSync(join(state, "calls")) ? readFileSync(join(state, "calls"), "utf8").trim().split("\n") : []);

  test(
    "(M1) applies the pinned manifests, waits for every CRD, applies again, then the pin as trustiot-demo-flux, and creates the root Kustomization last",
    () => {
      const { state, base, bin } = setup();
      const result = runScript(applyScript(base, bootstrap, manifests.crds), bootstrap.text, bin, { FAKE_STATE: state });
      expect(result.code).toBe(0);
      expect(result.stdout).toContain("APPLY first-incomplete");
      const lines = result.stdout.trim().split("\n");
      expect(lines.at(-1)).toBe("APPLY done");
      expect(lines.indexOf(`APPLY pinned ${HEAD}`)).toBeGreaterThan(lines.indexOf("APPLY second"));
      const made = calls(state);
      expect(made).toHaveLength(5);
      expect(made[0]).toMatch(/^apply --server-side -f \S+\/flux-system\.yaml$/);
      expect(made[1]).toBe(`wait --for=condition=established --timeout=120s ${manifests.crds.map((c) => `crd/${c}`).join(" ")}`);
      expect(made[2]).toBe(made[0]);
      expect(made.slice(3)).toEqual(["apply --server-side --field-manager=trustiot-demo-flux -f -", "apply --server-side -f -"]);
      expect(readFileSync(join(state, "applied.yaml"), "utf8")).toBe(bootstrap.text);
      expect(readFileSync(join(state, "pin.json"), "utf8")).toBe(`${pinDocument(manifests.gitApiVersion, HEAD)}\n`);
      expect(readFileSync(join(state, "root.json"), "utf8")).toBe(`${bootstrap.root}\n`);
      expect(leftovers(base)).toEqual([]);
    },
    SLOW_TEST_MS,
  );

  test(
    "(M1) never creates the root Kustomization when the pin, the second apply or the staged manifests fail",
    () => {
      const pin = setup();
      const pinFailed = runScript(applyScript(pin.base, bootstrap, manifests.crds), bootstrap.text, pin.bin, { FAKE_STATE: pin.state, FAKE_PIN_FAIL: "1" });
      expect(pinFailed.code).not.toBe(0);
      expect(pinFailed.stdout).not.toContain("APPLY pinned");
      expect(calls(pin.state)).not.toContain("apply --server-side -f -");

      const second = setup();
      expect(runScript(applyScript(second.base, bootstrap, manifests.crds), bootstrap.text, second.bin, { FAKE_STATE: second.state, FAKE_SECOND_FAIL: "1" }).code).not.toBe(0);
      expect(calls(second.state)).toHaveLength(3);

      const changed = setup();
      const result = runScript(applyScript(changed.base, bootstrap, manifests.crds), `${bootstrap.text}\n# changed`, changed.bin, { FAKE_STATE: changed.state });
      expect(result.code).toBe(1);
      expect(existsSync(join(changed.state, "calls"))).toBe(false);
      // Flux's manifests as main has them are not what the apply takes: only the pinned text is.
      expect(runScript(applyScript(setup().base, bootstrap, manifests.crds), manifests.text, changed.bin, { FAKE_STATE: changed.state }).code).toBe(1);
    },
    SLOW_TEST_MS,
  );

  test(
    "deletes Flux's objects without waiting",
    () => {
      const { state, base, bin } = setup();
      const result = runScript(deleteScript(base, manifests.sha256), manifests.text, bin, { FAKE_STATE: state });
      expect(result.code).toBe(0);
      expect(result.stdout).toContain("DELETE done");
      expect(readFileSync(join(state, "calls"), "utf8").trim()).toMatch(/^delete -f \S+\/flux-system\.yaml --ignore-not-found --wait=false$/);
      expect(leftovers(base)).toEqual([]);
    },
    SLOW_TEST_MS,
  );
});

// --- The runs against a fake cluster ----------------------------------------------------------------------

const OPTIONS: CommonOptions = { sshHost: "demo", apply: false, confirm: "", lockHeld: false };
const TUNNEL = { active: "active", pid: "10", restarts: "0", started: "x" };
const GIT = "gitrepositories.source.toolkit.fluxcd.io";
const KUSTOMIZATION = "kustomizations.kustomize.toolkit.fluxcd.io";
const HELM_RELEASE = "helmreleases.helm.toolkit.fluxcd.io";
/** A commit this run did not check: an earlier run's, or main's after it moved on. */
const OLD = "c".repeat(40);
const NEWER = "e".repeat(40);
const ready = (extra: Record<string, unknown> = {}) => ({
  metadata: { generation: 1 },
  status: { observedGeneration: 1, conditions: [{ type: "Ready", status: "True", reason: "Succeeded", message: "ok" }], ...extra },
});
const notReady = { metadata: { generation: 1 }, status: { observedGeneration: 1, conditions: [{ type: "Ready", status: "False", reason: "Progressing", message: "working" }] } };

class FakeFlux implements FluxOps {
  calls: string[] = [];
  time = 0;
  lockValue: Awaited<ReturnType<FluxOps["lockState"]>> = { kind: "free" };
  lockReads = 0;
  onLockRead?: (n: number) => void;
  sopsAgeValue: SopsAgeState;
  /** Everything but the GitRepository, which gitRef holds. */
  facts: ClusterFacts;
  /** spec.ref of GitRepository flux-system; undefined while it does not exist. */
  gitRef?: Record<string, unknown>;
  gitGeneration = 0;
  gitSuspended = false;
  /** main's newest commit on GitHub: what the GitRepository fetches while it follows main. */
  mainCommit = HEAD;
  /** A commit the GitRepository reports whatever spec.ref says (a source-controller gone wrong). */
  reportedCommit?: string;
  /** What the bootstrap apply got. */
  bootstrap?: Bootstrap;
  applyError?: Error;
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
  /** What Flux reports for the chart once it caught up: its digest, the deployed chart. */
  revisions = { chart: DIGEST, release: `0.0.0-rc.10+${"b".repeat(12)}` };
  /** Polls an object stays Ready=True at an older revision (after its pollsUntilReady), by resource name. */
  pollsAtOldRevision: Record<string, number> = {};
  /** HelmRelease polls after a reconcile request that still show the request unhandled. */
  requestPolls = 0;
  private request?: { token: string; polls: number };
  private polls = new Map<string, number>();
  private deleting = new Map<string, number>();
  clusterReads = 0;
  onClusterRead?: (n: number, facts: ClusterFacts) => void;
  /** Called on each object read, with the read's number for that object. */
  onObject?: (resource: string, n: number) => void;
  onSuspend?: () => void;
  publicKey = DEPLOY_KEY_PUB;

  constructor(facts: ClusterFacts, sopsAge: SopsAgeState) {
    this.facts = facts;
    this.sopsAgeValue = sopsAge;
  }
  /** Sets spec.ref as an API server would: a new generation when it changes. */
  setGitRef(ref: Record<string, unknown>) {
    if (JSON.stringify(ref) === JSON.stringify(this.gitRef)) return;
    this.gitRef = structuredClone(ref);
    this.gitGeneration += 1;
  }
  async tunnel() {
    return TUNNEL;
  }
  async lockState() {
    this.lockReads += 1;
    this.onLockRead?.(this.lockReads);
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
    const facts = structuredClone(this.facts);
    if (this.gitRef === undefined) delete facts.gitRepository;
    else facts.gitRepository = { ref: gitRefOf({ spec: { ref: this.gitRef } }), suspended: this.gitSuspended };
    return facts;
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
  async applyFlux(manifests: FluxManifests, bootstrap: Bootstrap) {
    this.calls.push("apply");
    if (this.applyError !== undefined) throw this.applyError;
    this.bootstrap = bootstrap;
    // What applyScript makes: the CRDs, the GitRepository as the applied text has it, and the root Kustomization.
    const docs = Bun.YAML.parse(bootstrap.text) as { kind?: string; spec?: { ref?: Record<string, unknown> } }[];
    this.setGitRef(docs.find((doc) => doc?.kind === "GitRepository")?.spec?.ref ?? {});
    this.facts.fluxCrds = manifests.crds;
    this.facts.rootKustomization = { suspended: false, prune: true };
    this.facts.helmRelease = { suspended: false };
    return "namespace/flux-system serverside-applied\n";
  }
  async pinGit(_apiVersion: string, head: string, from?: string) {
    this.calls.push(`pin ${head}${from === undefined ? "" : ` from ${from}`}`);
    // The JSON patch tests the commit it moves the pin from.
    if (from !== undefined && this.gitRef?.commit !== from && this.gitRef?.commit !== head) throw new Error("the JSON patch's test failed");
    this.setGitRef({ branch: "main", commit: head });
  }
  async unpinGit(head: string) {
    this.calls.push(`unpin ${head}`);
    if (this.gitRef?.commit !== head) throw new Error("the JSON patch's test failed");
    this.setGitRef({ branch: "main" });
  }
  /** The commit source-controller has: the pinned one, or main's newest. */
  private gitRevision(): string {
    const pinned = this.gitRef?.commit;
    return `main@sha1:${this.reportedCommit ?? (typeof pinned === "string" ? pinned : this.mainCommit)}`;
  }
  private gitObject(n: number): Record<string, unknown> | undefined {
    if (this.gitRef === undefined) return undefined;
    const generation = this.gitGeneration;
    const spec = { ref: structuredClone(this.gitRef), suspend: this.gitSuspended };
    const until = this.pollsUntilReady[GIT] ?? 0;
    if (n <= until) return { metadata: { generation }, spec, status: { ...notReady.status, observedGeneration: generation } };
    // An artifact of an older commit belongs to an older generation, which source-controller has not caught up with.
    const old = n <= until + (this.pollsAtOldRevision[GIT] ?? 0);
    return {
      metadata: { generation },
      spec,
      status: { observedGeneration: old ? generation - 1 : generation, conditions: ready().status.conditions, artifact: { revision: old ? `main@sha1:${OLD}` : this.gitRevision() } },
    };
  }
  async object(resource: string, namespace: string, name: string): Promise<Record<string, unknown> | undefined> {
    const key = `${resource}/${namespace}/${name}`;
    const n = (this.polls.get(key) ?? 0) + 1;
    this.polls.set(key, n);
    this.onObject?.(resource, n);
    if (resource === GIT) return this.gitObject(n);
    if (this.stalled.has(resource)) {
      return { ...notReady, status: { ...notReady.status, conditions: [...notReady.status.conditions, { type: "Stalled", status: "True", reason: "RetriesExceeded", message: "install failed" }] } };
    }
    if (resource === KUSTOMIZATION && this.dropMarkerOnReconcile) delete this.facts.namespace!.labels[MARKER];
    if (n <= (this.pollsUntilReady[resource] ?? 0)) return notReady;
    if (resource === "deployments.apps") return { metadata: { generation: 1 }, spec: { replicas: 1, template: { spec: { containers: [{ image: `ghcr.io/fluxcd/${name}:v1` }] } } }, status: { observedGeneration: 1, availableReplicas: 1, updatedReplicas: 1 } };
    const old = n <= (this.pollsUntilReady[resource] ?? 0) + (this.pollsAtOldRevision[resource] ?? 0);
    if (resource === KUSTOMIZATION) return ready({ lastAppliedRevision: old ? `main@sha1:${OLD}` : this.gitRevision() });
    if (resource.startsWith("ocirepositories")) return ready({ artifact: { revision: old ? `sha256:${"d".repeat(64)}` : this.revisions.chart } });
    if (resource === HELM_RELEASE) {
      const handled = this.request !== undefined && ++this.request.polls > this.requestPolls ? { lastHandledReconcileAt: this.request.token } : {};
      const chartVersion = old ? `0.0.0-rc.8+${"c".repeat(12)}` : this.revisions.release;
      return { ...ready({ history: [{ chartVersion, status: "deployed" }], ...handled }), spec: { suspend: this.facts.helmRelease?.suspended ?? false } };
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
    if (this.facts.rootKustomization !== undefined) this.facts.rootKustomization = { suspended: true, prune: false };
    this.onSuspend?.();
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
    delete this.facts.rootKustomization;
    delete this.facts.helmRelease;
    this.gitRef = undefined;
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

/** Every file a run wrote into its log directory, as one text. */
function written(dir: string): string {
  return readdirSync(dir)
    .map((name) => readFileSync(join(dir, name), "utf8"))
    .join("\n");
}

/** Private names and local paths as ssh, kubectl and kustomize may print them; none may reach a run's output. */
const PRIVATE_TEXT = ["192.0.2.10", "198.51.100.7", "demo-box", "access.example.com", "/home/someone", ROOT];

describe("runs", () => {
  let dir: string;
  let log: FluxLog;
  let root: string;
  let inputs: Inputs;
  let repo: RepoInputs;
  beforeEach(async () => {
    dir = mkdtempSync(join(tmpdir(), "demo-flux-log-"));
    log = new FluxLog("test", dir);
    root = mainReady();
    inputs = { repo: () => readRepo(root, CLEAN, build) };
    repo = await inputs.repo();
  });
  afterEach(() => rmSync(dir, { recursive: true, force: true }));

  const logText = () => readFileSync(log.file, "utf8");
  /** The code of the latest dry run in the log. */
  const codeOf = () => [...logText().matchAll(/--confirm ([0-9a-f]{12})/g)].at(-1)?.[1] ?? "";
  const okAge = (): SopsAgeState => ({ kind: "ok", recipients: [repo.demoRecipient!] });
  /** This script's install as a later run finds it, the GitRepository as `ref` says. */
  const installedCluster = (ref: Record<string, unknown> | undefined) => {
    const cluster = readyCluster(repo);
    cluster.namespace = withDeployKey(ageKeyNamespace({ labels: { [MARKER]: MARKED_INSTALL } }));
    cluster.fluxCrds = repo.manifests!.crds;
    cluster.fluxClusterObjects = repo.manifests!.clusterObjects;
    cluster.rootKustomization = { suspended: false, prune: true };
    cluster.helmRelease = { suspended: false };
    cluster.appNamespace = { labels: { "kustomize.toolkit.fluxcd.io/name": "flux-system", "kustomize.toolkit.fluxcd.io/namespace": "flux-system" }, terminating: false };
    const fake = new FakeFlux(cluster, okAge());
    if (ref !== undefined) fake.setGitRef(ref);
    return fake;
  };
  /** A dry run, then --apply with its code. */
  const dryRunThenApply = async (fake: FakeFlux, prepare: () => void = () => {}) => {
    await runInstall(fake, log, OPTIONS, inputs);
    const code = codeOf();
    prepare();
    return runInstall(fake, log, { ...OPTIONS, apply: true, confirm: code }, inputs);
  };

  describe("install", () => {
    test("a dry run prints the plan and its code and changes nothing", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      const plan = await runInstall(fake, log, OPTIONS, inputs);
      expect(plan.refusals).toEqual([]);
      expect(fake.calls).toEqual(["sops-age check"]);
      expect(codeOf()).toMatch(/^[0-9a-f]{12}$/);
      expect(logText()).toContain(`label namespace flux-system ${MARKER}=${MARKED_INSTALL}`);
      expect(logText()).toContain(`ingress ClusterIP ${CLUSTER_IP}`);
      expect(logText()).toContain(`with GitRepository flux-system/flux-system pinned to ${HEAD} (spec.ref.commit;`);
      expect(logText()).toContain("remove the pin (a JSON patch that tests it), so Flux follows main again");
    });

    test("(L4) refusals stop a dry run and --apply alike, before anything changes, and the root age check waits for the lock", async () => {
      const fake = new FakeFlux(readyCluster(repo), { kind: "missing" });
      fake.lockValue = { kind: "held", owner: "Laysi/trustiot-v2#4670 2026-10-21T00:00:00Z" };
      await expect(runInstall(fake, log, OPTIONS, inputs)).rejects.toThrow("1 refusal above; nothing changed");
      await expect(runInstall(fake, log, { ...OPTIONS, apply: true, confirm: "x" }, inputs)).rejects.toThrow("refusal above");
      // The age check reads the private key as root: never while another issue holds the lab.
      expect(fake.calls).toEqual([]);
      expect(logText()).toContain("REFUSED: the lab lock is held by");
      expect(logText()).not.toContain("--confirm");
    });

    test("(L4) the root age check runs only with the lock free or this issue's and a namespace this script may use", async () => {
      const unreadable = new FakeFlux(readyCluster(repo), okAge());
      unreadable.lockValue = { kind: "unreadable" };
      await expect(runInstall(unreadable, log, OPTIONS, inputs)).rejects.toThrow("1 refusal above");
      expect(unreadable.calls).toEqual([]);

      const foreign = readyCluster(repo);
      foreign.namespace = ageKeyNamespace({ labels: { owner: "x" } });
      const foreignFake = new FakeFlux(foreign, okAge());
      await expect(runInstall(foreignFake, log, OPTIONS, inputs)).rejects.toThrow("refusal");
      expect(foreignFake.calls).toEqual([]);

      const ours = new FakeFlux(readyCluster(repo), okAge());
      ours.lockValue = { kind: "held", owner: "Laysi/trustiot-v2#467 2026-10-21T00:00:00Z demo install" };
      await runInstall(ours, log, OPTIONS, inputs);
      expect(ours.calls).toEqual(["sops-age check"]);
    });

    test("(L4) under the lock the fresh lock and namespace decide too, before any root script", async () => {
      const taken = new FakeFlux(readyCluster(repo), okAge());
      await expect(
        dryRunThenApply(taken, () => {
          taken.onLockRead = (n) => {
            if (n === 3) taken.lockValue = { kind: "held", owner: "Laysi/trustiot-v2#4670 2026-10-21T00:00:00Z" };
          };
        }),
      ).rejects.toThrow("1 refusal above");
      expect(taken.calls).toEqual(["sops-age check", "sops-age check", "lock install", "release"]);

      const turned = new FakeFlux(readyCluster(repo), okAge());
      await expect(
        dryRunThenApply(turned, () => {
          turned.onClusterRead = (n, facts) => {
            if (n === 3) facts.namespace!.labels.owner = "x";
          };
        }),
      ).rejects.toThrow("refusal above");
      expect(turned.calls).toEqual(["sops-age check", "sops-age check", "lock install", "release"]);
    });

    test("main before Laysi/trustiot-v2#480 is refused", async () => {
      const unready = mainUnready();
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await expect(runInstall(fake, log, OPTIONS, { repo: () => readRepo(unready, CLEAN, build) })).rejects.toThrow("3 refusals above");
    });

    test("--apply needs the code of the plan", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await expect(runInstall(fake, log, { ...OPTIONS, apply: true }, inputs)).rejects.toThrow("--apply needs --confirm");
      await expect(runInstall(fake, log, { ...OPTIONS, apply: true, confirm: "000000000000" }, inputs)).rejects.toThrow("the plan changed since it was shown");
      expect(fake.calls.filter((c) => c !== "sops-age check")).toEqual([]);
    });

    test("(M1) --apply labels, applies pinned to the checked commit, waits for each object, removes the pin and reports, holding the lock", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await dryRunThenApply(fake, () => {
        // The guard reads the GitRepository once per controller poll (3); its own wait then sees it not ready once.
        fake.pollsUntilReady = { [HELM_RELEASE]: 3, [GIT]: 4 };
      });
      expect(fake.calls).toEqual(["sops-age check", "sops-age check", "lock install", "sops-age check", "mark 100", "apply", `request reconcile ${HELM_RELEASE} trustiot/trustiot`, `unpin ${HEAD}`, "release"]);
      expect(fake.bootstrap).toEqual(pinnedBootstrap(repo.manifests!, HEAD));
      expect(fake.gitRef).toEqual({ branch: "main" });
      const text = logText();
      expect(text).toContain("HelmRelease trustiot/trustiot at 0.0.0-rc.10+bbbbbbbbbbbb: Ready=False Progressing: working");
      expect(text).toContain("HelmRelease trustiot/trustiot at 0.0.0-rc.10+bbbbbbbbbbbb: Ready=True Succeeded: ok");
      expect(text).toContain(`requested a reconcile of HelmRelease trustiot/trustiot (${RECONCILE_ANNOTATION}=1970-01-01T00:00:`);
      expect(text).toContain(`removed the pin of GitRepository flux-system/flux-system to ${HEAD}: Flux follows main again`);
      expect(text).toContain(`GitRepository flux-system/flux-system following main: Ready=True Succeeded: ok; at main@sha1:${HEAD}`);
      expect(text).not.toContain("main moved on");
      expect(text).toContain("pulled:   the same");
      expect(text).toContain(`deployed: the same (0.0.0-rc.10+${"b".repeat(12)} (deployed))`);
      expect(text).toContain("trustiot-identity-abc Running");
      expect(fake.time).toBe(4 * POLL_MS);
    });

    test("(M1) main moving on during the install: the first deploy is still the checked commit, then Flux follows the new main", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await dryRunThenApply(fake, () => {
        fake.mainCommit = NEWER;
      });
      const text = logText();
      expect(text).toContain(`Kustomization flux-system/flux-system at main@sha1:${HEAD}: Ready=True Succeeded: ok`);
      expect(text).toContain(`GitRepository flux-system/flux-system following main: Ready=True Succeeded: ok; at main@sha1:${NEWER}`);
      expect(text).toContain(`main moved on since this run checked ${HEAD}: Flux now follows main at main@sha1:${NEWER}, which this run did not check; status shows how it goes`);
      expect(fake.calls.slice(-2)).toEqual([`unpin ${HEAD}`, "release"]);
    });

    test("(M1) the GitRepository reporting another commit while pinned stops the run at once", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      const run = dryRunThenApply(fake, () => {
        fake.reportedCommit = NEWER;
      });
      await expect(run).rejects.toThrow(`GitRepository flux-system/flux-system is pinned to ${HEAD} but reports main@sha1:${NEWER} for its current generation`);
      await expect(run).rejects.toThrow("nothing more is changed; look at it and at the Kustomization, then run install again (dry run first)");
      expect(fake.time).toBe(0);
      expect(fake.calls.filter((c) => c.startsWith("unpin"))).toEqual([]);
      expect(fake.calls.at(-1)).toBe("release");
    });

    test("(M1) a pin that changes while the run waits stops it at once", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      const run = dryRunThenApply(fake, () => {
        fake.pollsUntilReady = { [KUSTOMIZATION]: 5 };
        fake.onObject = (resource, n) => {
          if (resource === KUSTOMIZATION && n === 1) fake.setGitRef({ branch: "main" });
        };
      });
      await expect(run).rejects.toThrow(`GitRepository flux-system/flux-system is no longer pinned to ${HEAD}, which this run checked (spec.ref is now {branch: main}): Flux may apply a commit this run did not check`);
      expect(fake.time).toBe(POLL_MS);
      expect(fake.calls.at(-1)).toBe("release");
    });

    test("(M1) the wait for the controllers is guarded too: a pin that changes while they start stops the run", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      const run = dryRunThenApply(fake, () => {
        fake.pollsUntilReady = { "deployments.apps": 3 };
        fake.onObject = (resource, n) => {
          if (resource === "deployments.apps" && n === 1) fake.setGitRef({ branch: "main", commit: NEWER });
        };
      });
      await expect(run).rejects.toThrow(`GitRepository flux-system/flux-system is no longer pinned to ${HEAD}, which this run checked (spec.ref is now {branch: main, commit: ${NEWER}})`);
      expect(fake.time).toBe(POLL_MS);
      expect(logText()).not.toContain("GitRepository flux-system/flux-system at");
      expect(fake.calls.at(-1)).toBe("release");
    });

    test("(M1) a pin that is gone once the first deploy was verified is not patched again; Flux must still follow main", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await dryRunThenApply(fake, () => {
        // Dropped during the read that shows the release deployed, after the guard of that poll.
        fake.onObject = (resource, n) => {
          if (resource === HELM_RELEASE && n === 1) fake.setGitRef({ branch: "main" });
        };
      });
      expect(fake.calls.filter((c) => c.startsWith("unpin"))).toEqual([]);
      expect(fake.calls.at(-1)).toBe("release");
      const text = logText();
      expect(text).toContain(`the pin of GitRepository flux-system/flux-system to ${HEAD} was gone already after the first deploy was verified: Flux follows main again`);
      expect(text).toContain(`GitRepository flux-system/flux-system following main: Ready=True Succeeded: ok; at main@sha1:${HEAD}`);
      expect(text).toContain("done; records in");
    });

    test("(M1) a run after one that stopped while pinned keeps the pin, waits for the first deploy and removes it", async () => {
      const fake = installedCluster({ branch: "main", commit: HEAD });
      await runInstall(fake, log, OPTIONS, inputs);
      expect(logText()).toContain(`an earlier run stopped while GitRepository flux-system/flux-system was pinned; keep it pinned to ${HEAD}`);
      await runInstall(fake, log, { ...OPTIONS, apply: true, confirm: codeOf() }, inputs);
      expect(fake.calls).toEqual(["sops-age check", "sops-age check", "lock install", "sops-age check", `pin ${HEAD}`, `request reconcile ${HELM_RELEASE} trustiot/trustiot`, `unpin ${HEAD}`, "release"]);
      expect(fake.gitRef).toEqual({ branch: "main" });
    });

    test("(M1) a re-run after main moved checks the new head and moves the pin to it before anything else", async () => {
      // Stopped after the root Kustomization existed.
      const later = installedCluster({ branch: "main", commit: OLD });
      await runInstall(later, log, OPTIONS, inputs);
      expect(logText()).toContain(`move the pin of GitRepository flux-system/flux-system from ${OLD} (an earlier run's; main moved on since) to ${HEAD}`);
      await runInstall(later, log, { ...OPTIONS, apply: true, confirm: codeOf() }, inputs);
      expect(later.calls).toEqual(["sops-age check", "sops-age check", "lock install", "sops-age check", `pin ${HEAD} from ${OLD}`, `request reconcile ${HELM_RELEASE} trustiot/trustiot`, `unpin ${HEAD}`, "release"]);

      // Stopped before it: the pin moves first, then the bootstrap apply runs again.
      const early = installedCluster({ branch: "main", commit: OLD });
      delete early.facts.rootKustomization;
      await dryRunThenApply(early);
      expect(early.calls).toEqual(["sops-age check", "sops-age check", "lock install", "sops-age check", `pin ${HEAD} from ${OLD}`, "apply", `request reconcile ${HELM_RELEASE} trustiot/trustiot`, `unpin ${HEAD}`, "release"]);
    });

    test("a run after an install that finished only waits and reports", async () => {
      const fake = installedCluster({ branch: "main" });
      await runInstall(fake, log, OPTIONS, inputs);
      expect(logText()).not.toContain("label namespace flux-system");
      expect(logText()).not.toContain("remove the pin");
      await runInstall(fake, log, { ...OPTIONS, apply: true, confirm: codeOf() }, inputs);
      expect(fake.calls).toEqual(["sops-age check", "sops-age check", "lock install", "sops-age check", `request reconcile ${HELM_RELEASE} trustiot/trustiot`, "release"]);
      expect(logText()).toContain("HelmRelease trustiot/trustiot at 0.0.0-rc.10+bbbbbbbbbbbb: Ready=True");
    });

    test("(L2) a suspended root Kustomization, GitRepository or HelmRelease is refused", async () => {
      const removed = installedCluster({ branch: "main" });
      removed.facts.rootKustomization = { suspended: true, prune: false };
      await expect(runInstall(removed, log, OPTIONS, inputs)).rejects.toThrow("1 refusal above");
      expect(logText()).toContain("REFUSED: the root Kustomization flux-system has spec.suspend: true, spec.prune: false, as a remove that stopped halfway leaves it");
      const git = installedCluster({ branch: "main" });
      git.gitSuspended = true;
      await expect(runInstall(git, log, OPTIONS, inputs)).rejects.toThrow("1 refusal above");
      expect(logText()).toContain("REFUSED: GitRepository flux-system/flux-system is suspended");
      const release = installedCluster({ branch: "main" });
      release.facts.helmRelease = { suspended: true };
      await expect(runInstall(release, log, OPTIONS, inputs)).rejects.toThrow("1 refusal above");
      expect(logText()).toContain("REFUSED: HelmRelease trustiot/trustiot is suspended");
    });

    test("(L2) an install is not done while anything of Flux is suspended", async () => {
      const fake = installedCluster({ branch: "main" });
      const run = dryRunThenApply(fake, () => {
        fake.onObject = (resource, n) => {
          if (resource === HELM_RELEASE && n === 1) fake.facts.helmRelease = { suspended: true };
        };
      });
      await expect(run).rejects.toThrow("the install is not done: HelmRelease trustiot/trustiot is suspended");
      expect(logText()).not.toContain("done; records in");
      expect(fake.calls.at(-1)).toBe("release");
    });

    test("(L3) a root script that may still run keeps the lab lock, and says what to check", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      const run = dryRunThenApply(fake, () => {
        fake.applyError = new StillRunningError("applying Flux's manifests: ssh ended with exit 255 and the root script may still be running on the demo machine");
      });
      await expect(run).rejects.toBeInstanceOf(StillRunningError);
      expect(fake.calls).toEqual(["sops-age check", "sops-age check", "lock install", "sops-age check", "mark 100", "apply"]);
      expect(logText()).toContain("the lab lock stays held for Laysi/trustiot-v2#467: a root script of this run may still be running on the demo machine");

      // Any other failure gives the lock back.
      const failed = new FakeFlux(readyCluster(repo), okAge());
      await expect(
        dryRunThenApply(failed, () => {
          failed.applyError = new Error("applying Flux's manifests failed (exit 1)");
        }),
      ).rejects.toThrow("exit 1");
      expect(failed.calls.at(-1)).toBe("release");
    });

    test("a checkout that is not origin/main is refused", async () => {
      const dirty = { repo: () => readRepo(root, { ...CLEAN, dirty: [" M deploy/flux/clusters/demo/trustiot/values-demo.yaml"] }, build) };
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await expect(runInstall(fake, log, OPTIONS, dirty)).rejects.toThrow("1 refusal above");
      expect(logText()).toContain("uncommitted changes");
    });

    test("a change between the plan and the run stops it under the lock", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await expect(
        dryRunThenApply(fake, () => {
          fake.onClusterRead = (n, facts) => {
            if (n === 3) facts.namespace!.uid = "uid-2";
          };
        }),
      ).rejects.toThrow("the plan changed since it was shown");
      expect(fake.calls).toEqual(["sops-age check", "sops-age check", "lock install", "sops-age check", "release"]);
    });

    test("(M1) the confirmed code binds the pin step: a GitRepository whose spec.ref changed after the dry run stops the run before any pin", async () => {
      for (const [before, after] of [
        [{ branch: "main" }, { branch: "main", commit: HEAD }],
        [{ branch: "main", commit: HEAD }, { branch: "main", commit: OLD }],
        [{ branch: "main", commit: OLD }, { branch: "main", commit: NEWER }],
      ]) {
        const fake = installedCluster(before);
        await runInstall(fake, log, OPTIONS, inputs);
        const code = codeOf();
        fake.setGitRef(after!);
        await runInstall(fake, log, OPTIONS, inputs);
        expect(codeOf()).not.toBe(code);
        fake.setGitRef(before!);
        fake.calls = [];
        fake.clusterReads = 0;
        // Changed again after the confirmation (read 1), before the plan under the lock (read 2): nothing is pinned.
        fake.onClusterRead = (n) => {
          if (n === 2) fake.setGitRef(after!);
        };
        await expect(runInstall(fake, log, { ...OPTIONS, apply: true, confirm: code }, inputs)).rejects.toThrow("the plan changed since it was shown");
        expect(fake.calls).toEqual(["sops-age check", "lock install", "sops-age check", "release"]);
      }
    });

    test("a refusal that appears under the lock stops the run", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await expect(
        dryRunThenApply(fake, () => {
          fake.onClusterRead = (n, facts) => {
            if (n === 3) facts.poc = ["poc/deployment.apps/bridge"];
          };
        }),
      ).rejects.toThrow("1 refusal above");
      expect(fake.calls.at(-1)).toBe("release");
      expect(fake.calls).not.toContain("apply");
    });

    test("a wait that runs out names what it waited for, and the lock is released", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      const run = dryRunThenApply(fake, () => {
        fake.pollsUntilReady = { [GIT]: Number.POSITIVE_INFINITY };
      });
      await expect(run).rejects.toThrow(
        `timed out after ${WAITS.source / 60_000} min waiting for GitRepository flux-system/flux-system at main@sha1:${HEAD}; last state: Ready=False Progressing: working. Check that GitHub has the deploy key ${sshFingerprint(DEPLOY_KEY_PUB)}`,
      );
      await expect(run).rejects.toThrow(`The GitRepository is pinned to ${HEAD}; if that commit is no longer on main, run install again (dry run first) from the new main`);
      expect(fake.calls.at(-1)).toBe("release");
    });

    test("Ready=True at an older commit or chart is waited out, not reported as done", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await dryRunThenApply(fake, () => {
        // The GitRepository: 3 reads by the guard while the controllers start, then 2 in its own wait.
        fake.pollsAtOldRevision = { [GIT]: 5, [KUSTOMIZATION]: 1, "ocirepositories.source.toolkit.fluxcd.io": 1, [HELM_RELEASE]: 1 };
      });
      const text = logText();
      expect(text).toContain(`GitRepository flux-system/flux-system at main@sha1:${HEAD}: Ready=True Succeeded: ok (generation 1 not observed yet)`);
      expect(text).toContain(`Kustomization flux-system/flux-system at main@sha1:${HEAD}: Ready=True Succeeded: ok; at main@sha1:${OLD}`);
      expect(text).toContain(`OCIRepository trustiot/trustiot at ${DIGEST}: Ready=True Succeeded: ok; at sha256:${"d".repeat(64)}, waiting for ${DIGEST}`);
      expect(text).toContain(`HelmRelease trustiot/trustiot at 0.0.0-rc.10+bbbbbbbbbbbb: Ready=True Succeeded: ok; at 0.0.0-rc.8+cccccccccccc, waiting for 0.0.0-rc.10+bbbbbbbbbbbb`);
      expect(fake.time).toBe(5 * POLL_MS);
    });

    test("a chart Flux never reaches ends in a timeout that names the wanted revision", async () => {
      const stuck = new FakeFlux(readyCluster(repo), okAge());
      await expect(
        dryRunThenApply(stuck, () => {
          stuck.revisions.release = `0.0.0-rc.10+${"f".repeat(12)}`;
        }),
      ).rejects.toThrow(`timed out after ${WAITS.release / 60_000} min waiting for HelmRelease trustiot/trustiot at 0.0.0-rc.10+bbbbbbbbbbbb`);
      expect(stuck.calls.at(-1)).toBe("release");
    });

    test("the release counts only after helm-controller handled the reconcile request of this run", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await dryRunThenApply(fake, () => {
        fake.requestPolls = 2;
      });
      expect(logText()).toContain("reconcile request 1970-01-01T00:00:00.000Z not handled yet");
      expect(fake.time).toBe(2 * POLL_MS);

      const never = new FakeFlux(readyCluster(repo), okAge());
      await expect(
        dryRunThenApply(never, () => {
          never.requestPolls = Number.POSITIVE_INFINITY;
        }),
      ).rejects.toThrow("not handled yet");
      expect(never.calls.at(-1)).toBe("release");
    });

    test("a stalled HelmRelease ends the wait at once", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await expect(
        dryRunThenApply(fake, () => {
          fake.stalled.add(HELM_RELEASE);
        }),
      ).rejects.toThrow("gave up waiting for HelmRelease trustiot/trustiot at 0.0.0-rc.10+bbbbbbbbbbbb: Stalled RetriesExceeded: install failed");
      expect(fake.time).toBe(0);
    });

    test("losing the marker after Flux applies its own Namespace is an error", async () => {
      const fake = new FakeFlux(readyCluster(repo), okAge());
      await expect(
        dryRunThenApply(fake, () => {
          fake.dropMarkerOnReconcile = true;
        }),
      ).rejects.toThrow(`lost its label ${MARKER}=${MARKED_INSTALL} after Flux applied its own manifests`);
    });

    test("(M2) a failed kustomize build reaches the output, the records and the error without local paths or private names", async () => {
      const stderr = [
        `Error: accumulating resources: accumulation err='accumulating resources from '${ROOT}/deploy/flux/clusters/saas/flux-system': open /home/someone/x/deploy/flux/clusters/demo/flux-system/kustomization.yaml: no such file or directory'`,
        "dial tcp 192.0.2.10:443 via access.example.com (198.51.100.7)",
      ].join("\n");
      const failing = { repo: () => readRepo(root, CLEAN, async () => Promise.reject(new Error(stderr))) };
      const fake = new FakeFlux(readyCluster(repo), okAge());
      const error = await runInstall(fake, log, OPTIONS, failing).then(
        () => new Error("no error"),
        (thrown: Error) => thrown,
      );
      expect(error.message).toBe("1 refusal above; nothing changed");
      const output = written(dir);
      expect(output).toContain("kubectl kustomize deploy/flux/clusters/demo/flux-system failed: Error: accumulating resources");
      expect(output).toContain("'<repo>/deploy/flux/clusters/saas/flux-system': open ~/x/deploy/flux/clusters/demo/flux-system/kustomization.yaml");
      expect(output).toContain("dial tcp <address>:443 via <host> (<address>)");
      for (const text of PRIVATE_TEXT) {
        expect(output).not.toContain(text);
        expect(error.message).not.toContain(text);
      }
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

    test("(L5) a deploy-key Secret with an empty value is refused, with how to start over", async () => {
      const cluster = readyCluster(repo);
      cluster.namespace!.secrets[1]!.sizes = { identity: 0, "identity.pub": 136, known_hosts: 900 };
      await expect(runDeployKey(new FakeFlux(cluster, okAge()), log, OPTIONS, inputs)).rejects.toThrow("1 refusal above");
      expect(logText()).toContain("REFUSED: Secret flux-system/flux-system has an empty identity (a deploy-key run stopped while making it): delete it");
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
      const fake = installedCluster({ branch: "main" });
      delete fake.facts.appNamespace;
      fake.fluxRefs = ["GitRepository/flux-system/flux-system", "HelmRelease/trustiot/trustiot", "Kustomization/flux-system/flux-system", "OCIRepository/trustiot/trustiot"];
      return fake;
    };
    const ORDER = [
      "lock remove",
      "suspend root",
      `delete ${HELM_RELEASE} trustiot/trustiot`,
      "delete ocirepositories.source.toolkit.fluxcd.io trustiot/trustiot",
      `delete ${GIT} flux-system/flux-system`,
      `delete ${KUSTOMIZATION} flux-system/flux-system`,
      "delete flux",
      "release",
    ];

    test("removes in the order that keeps the data", async () => {
      const fake = installed();
      await runRemove(fake, log, OPTIONS, inputs);
      expect(fake.calls).toEqual([]);
      await runRemove(fake, log, { ...OPTIONS, apply: true, confirm: codeOf() }, inputs);
      expect(fake.calls).toEqual(ORDER);
      expect(logText()).toContain("kept in namespace trustiot: persistentvolumeclaim/trustiot-postgres-1");
    });

    test("(L2) a remove that stopped halfway, with the root Kustomization suspended, runs again to the end", async () => {
      const fake = installed();
      fake.facts.rootKustomization = { suspended: true, prune: false };
      await runRemove(fake, log, OPTIONS, inputs);
      await runRemove(fake, log, { ...OPTIONS, apply: true, confirm: codeOf() }, inputs);
      expect(fake.calls).toEqual(ORDER);
    });

    test("(L7) Flux objects that appear before the suspend took hold stop remove before it deletes anything", async () => {
      const fake = installed();
      await runRemove(fake, log, OPTIONS, inputs);
      const code = codeOf();
      fake.onSuspend = () => fake.fluxRefs.push("OCIRepository/trustiot/extra");
      await expect(runRemove(fake, log, { ...OPTIONS, apply: true, confirm: code }, inputs)).rejects.toThrow(
        "the Flux objects changed after the plan was made (now: GitRepository/flux-system/flux-system, HelmRelease/trustiot/trustiot, Kustomization/flux-system/flux-system, OCIRepository/trustiot/extra, OCIRepository/trustiot/trustiot; planned: GitRepository/flux-system/flux-system, HelmRelease/trustiot/trustiot, Kustomization/flux-system/flux-system, OCIRepository/trustiot/trustiot); nothing was deleted. The root Kustomization stays suspended with prune off: run remove again (dry run first), or resume Flux as deploy/flux/README.md 「在展示機安裝 Flux」 (中斷之後) describes",
      );
      expect(fake.calls).toEqual(["lock remove", "suspend root", "release"]);
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
      delete partial.facts.rootKustomization;
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
    fake.setGitRef({ branch: "main" });
    await runStatus(fake, log, inputs);
    expect(fake.calls).toEqual([]);
    expect(logText()).toContain("namespace flux-system: only the age key step's");
    expect(logText()).toContain(`deploy key fingerprint ${sshFingerprint(DEPLOY_KEY_PUB)}`);
    expect(logText()).toContain(`GitRepository flux-system/flux-system: Ready=True Succeeded: ok, revision main@sha1:${HEAD}`);
    expect(logText()).not.toContain("WARNING");
  });

  test("(L2) status warns about a pin left by an install that has not finished and about anything suspended", async () => {
    const fake = installedCluster({ branch: "main", commit: HEAD });
    fake.facts.rootKustomization = { suspended: true, prune: false };
    await runStatus(fake, log, inputs);
    expect(fake.calls).toEqual([]);
    expect(logText()).toContain(`  spec.ref is {branch: main, commit: ${HEAD}}: pinned by an install that has not finished; run install again (dry run first) to finish it`);
    expect(logText()).toContain("WARNING: the root Kustomization flux-system has spec.suspend: true, spec.prune: false");
  });
});

describe("FluxLog", () => {
  test("(M2) cleans everything it prints and saves, keys included", () => {
    const dir = tempDir("demo-flux-log-");
    const log = new FluxLog("test", dir);
    // What withOpsTunnel and Box note from ssh's own errors, and what a run saves.
    log.note("WARNING: the operations tunnel could not be read: ssh: connect to host 192.0.2.10 port 22: No route to host");
    log.note("ssh dropped the connection (attempt 1); retrying: kex_exchange_identification: read: Connection reset; ProxyCommand cloudflared access ssh --hostname access.example.com");
    const saved = log.save("record", { stderr: `open /home/someone/x and ${ROOT}/scripts`, list: ["198.51.100.7"], "192.0.2.10": { deep: "access.example.com" } });
    const output = written(dir);
    for (const text of PRIVATE_TEXT) expect(output).not.toContain(text);
    expect(output).toContain("ssh: connect to host <address> port 22");
    expect(output).toContain("--hostname <host>");
    expect(JSON.parse(readFileSync(saved, "utf8"))).toEqual({ stderr: "open ~/x and <repo>/scripts", list: ["<address>"], "<address>": { deep: "<host>" } });
  });

  test("cleanErrors rewrites the message and keeps a StillRunningError one", async () => {
    const log = new FluxLog("test", tempDir("demo-flux-log-"));
    await expect(cleanErrors(log, async () => Promise.reject(new Error("dial 192.0.2.10 for /home/someone/x")))).rejects.toThrow("dial <address> for ~/x");
    await expect(cleanErrors(log, async () => Promise.reject(new StillRunningError("still at 192.0.2.10")))).rejects.toBeInstanceOf(StillRunningError);
    expect(await cleanErrors(log, async () => "fine")).toBe("fine");
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

const ok = (stdout: string): RunResult => ({ stdout, stderr: "", exitCode: 0 });

describe("boxOps", () => {
  test("words splits what shq quotes", () => {
    const args = ["get", "x", "jsonpath={.data.identity\\.pub}", "it's", "a b"];
    expect(words(args.map(shq).join(" "))).toEqual(args);
  });

  test("never reads a Secret whole, reads only the deploy key's public halves, and sends inputs on stdin", async () => {
    const dir = tempDir("demo-flux-box-");
    const log = new FluxLog("test", dir);
    const knownHosts = readFileSync(join(ROOT, KNOWN_HOSTS_FILE), "utf8");
    const kubectl: string[][] = [];
    const scripts: { script: string; stdin: string }[] = [];
    const notFound: RunResult = { stdout: "", stderr: 'Error from server (NotFound): namespaces "trustiot" not found', exitCode: 1 };
    const run = async (cmd: string[], stdin?: string | Uint8Array): Promise<RunResult> => {
      expect(cmd.slice(0, 4)).toEqual(["ssh", "-o", "BatchMode=yes", "demo"]);
      const line = words(cmd[4] ?? "");
      if (line.slice(0, 3).join(" ") === "sudo bash -c") {
        expect(line).toHaveLength(4);
        const script = line[3]!;
        scripts.push({ script, stdin: stdin === undefined ? "" : Buffer.from(stdin).toString() });
        if (script.includes("SOPS_AGE")) return ok("STALE removed a run directory that a stopped run left behind\nSOPS_AGE ok age1fake\n");
        if (script.includes("DEPLOY_KEY")) return ok(`PUBLIC ${DEPLOY_KEY_PUB}\nDEPLOY_KEY created\n`);
        return ok(`APPLY pinned ${HEAD}\nAPPLY done\n`);
      }
      expect(line.slice(0, 3)).toEqual(["sudo", "k3s", "kubectl"]);
      const args = line.slice(3);
      kubectl.push(args);
      const joined = args.join(" ");
      if (joined === "get namespace flux-system -o json") return ok(JSON.stringify({ metadata: { uid: "u", resourceVersion: "5", labels: { "kubernetes.io/metadata.name": "flux-system" } } }));
      if (joined === "get namespace trustiot -o json") return notFound;
      if (args[0] === "api-resources") return ok("configmaps\nsecrets\nserviceaccounts\n");
      if (joined.startsWith("-n flux-system get configmaps,secrets,serviceaccounts --ignore-not-found -o jsonpath=")) return ok("v1\tSecret\tsops-age\t\tOpaque\t\t\t\nv1\tSecret\tflux-system\t\tOpaque\t\t\t\n");
      if (joined === `-n flux-system get secrets -o go-template=${SECRET_TEMPLATE}`) return ok("flux-system\tOpaque\tdeploy-key\tidentity:548,identity.pub:136,known_hosts:900,\nsops-age\tOpaque\t<no value>\tage.agekey:100,\n");
      if (joined === "-n flux-system get secret flux-system -o jsonpath={.data.identity\\.pub}") return ok(Buffer.from(`${DEPLOY_KEY_PUB}\n`).toString("base64"));
      if (joined === "-n flux-system get secret flux-system -o jsonpath={.data.known_hosts}") return ok(Buffer.from(knownHosts).toString("base64"));
      if (joined === "get customresourcedefinitions.apiextensions.k8s.io -o name") return ok("customresourcedefinition.apiextensions.k8s.io/clusters.postgresql.cnpg.io\n");
      if (joined.includes("-l app.kubernetes.io/part-of=flux -o name")) return ok("");
      if (joined.endsWith("--ignore-not-found -o name")) return ok("");
      if (args[0] === "label") return ok("namespace/flux-system labeled\n");
      if (joined === `get services -A -o ${SERVICES_JSONPATH}`) return ok("default/kubernetes\t10.43.0.1\t/\n");
      if (joined === "get servicecidrs.networking.k8s.io kubernetes -o json") return ok(JSON.stringify({ metadata: { name: "kubernetes" }, spec: { cidrs: ["10.43.0.0/16"] } }));
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
    expect(facts.namespace?.secrets[0]?.sizes).toEqual({ identity: 548, "identity.pub": 136, known_hosts: 900 });
    expect(facts.namespace?.objects.map((o) => o.ref)).toEqual(["secret/flux-system", "secret/sops-age"]);
    expect(facts.deployKey).toEqual({ publicKey: DEPLOY_KEY_PUB, knownHostsSha256: sha256(knownHosts) });
    expect(facts.missingPlatformCrds).toHaveLength(4);
    expect(facts.appNamespace).toBeUndefined();
    expect(facts.rootKustomization).toBeUndefined();
    expect(facts.gitRepository).toBeUndefined();
    expect(facts.services).toEqual([{ ref: "default/kubernetes", ips: ["10.43.0.1"], release: "/" }]);
    expect(facts.serviceCidrs).toEqual(["10.43.0.0/16"]);
    expect(SERVICES_JSONPATH).toBe(
      'jsonpath={range .items[*]}{.metadata.namespace}/{.metadata.name}{"\\t"}{.spec.clusterIPs[*]}{"\\t"}{.metadata.annotations.meta\\.helm\\.sh/release-namespace}/{.metadata.annotations.meta\\.helm\\.sh/release-name}{"\\n"}{end}',
    );
    expect(await ops.sopsAge("sops-age")).toEqual({ kind: "ok", recipients: ["age1fake"] });
    expect(readFileSync(log.file, "utf8")).toContain("removed a run directory that a stopped run left behind");
    expect(await ops.createDeployKey("flux-system", knownHosts)).toEqual({ publicKey: DEPLOY_KEY_PUB });
    await ops.markNamespace("5");
    const bootstrap = pinnedBootstrap(manifests, HEAD);
    await ops.applyFlux(manifests, bootstrap);
    await ops.requestReconcile(HELM_RELEASE, "trustiot", "trustiot", "2026-10-21T00:00:00.000Z");
    await expect(ops.requestReconcile(HELM_RELEASE, "trustiot", "trustiot", "x; true")).rejects.toThrow("not a reconcile token");
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
    expect(kubectl).toContainEqual(["-n", "trustiot", "annotate", "--overwrite", "--field-manager=trustiot-demo-flux", HELM_RELEASE, "trustiot", `${RECONCILE_ANNOTATION}=2026-10-21T00:00:00.000Z`]);

    // The root scripts get their input on stdin: the tool, the host keys and the pinned manifests.
    expect(scripts.map((s) => s.stdin)).toEqual(["fake age-keygen", knownHosts, bootstrap.text]);
    expect(scripts[0]!.script).toBe(ageCheckScript(REMOTE_BASE, sha256("fake age-keygen"), "sops-age"));
    expect(scripts[1]!.script).toBe(deployKeyScript(REMOTE_BASE, sha256(knownHosts), "flux-system"));
    expect(scripts[2]!.script).toBe(applyScript(REMOTE_BASE, bootstrap, manifests.crds));
  });

  test("(L2) the root Kustomization prunes only with spec.prune exactly true; suspend only when exactly true", async () => {
    const manifests = parseManifests(DEMO_MANIFESTS).manifests!;
    const factsWith = async (rootSpec: Record<string, unknown>) => {
      const run = async (cmd: string[]): Promise<RunResult> => {
        const joined = words(cmd[4] ?? "").slice(3).join(" ");
        if (joined === "get namespace flux-system -o json" || joined === "get namespace trustiot -o json") return { stdout: "", stderr: "Error from server (NotFound): not found", exitCode: 1 };
        if (joined === "get customresourcedefinitions.apiextensions.k8s.io -o name") return ok(manifests.crds.map((crd) => `customresourcedefinition.apiextensions.k8s.io/${crd}\n`).join(""));
        if (joined.includes("-l app.kubernetes.io/part-of=flux -o name") || joined.endsWith("--ignore-not-found -o name")) return ok("");
        if (joined === `-n flux-system get ${KUSTOMIZATION} flux-system -o json`) return ok(JSON.stringify({ spec: rootSpec }));
        if (joined.startsWith("-n flux-system get ") || joined.startsWith("-n trustiot get ")) return { stdout: "", stderr: "Error from server (NotFound): not found", exitCode: 1 };
        if (joined === `get services -A -o ${SERVICES_JSONPATH}`) return ok("");
        if (joined === "get servicecidrs.networking.k8s.io kubernetes -o json") return ok(JSON.stringify({ spec: { cidrs: ["10.43.0.0/16"] } }));
        throw new Error(`unexpected kubectl ${joined}`);
      };
      const ops = boxOps(new Box("demo", new FluxLog("test", tempDir("demo-flux-box-")), { run, sleep: async () => {} }), OPTIONS, async () => new Uint8Array());
      return (await ops.cluster(manifests)).rootKustomization;
    };
    expect(await factsWith({ prune: true })).toEqual({ suspended: false, prune: true });
    // An omitted boolean is false: a root without spec.prune does not prune.
    expect(await factsWith({})).toEqual({ suspended: false, prune: false });
    expect(await factsWith({ prune: "true", suspend: "true" })).toEqual({ suspended: false, prune: false });
    expect(await factsWith({ prune: false, suspend: true })).toEqual({ suspended: true, prune: false });
  });

  /** A box whose kubectl serves GitRepository flux-system from `state`, with patch and server-side apply. */
  const gitBox = (state: { ref: Record<string, unknown>; dropAfterPatch?: boolean }) => {
    const dir = tempDir("demo-flux-box-");
    const log = new FluxLog("test", dir);
    const sent: { args: string[]; stdin: string }[] = [];
    const run = async (cmd: string[], stdin?: string | Uint8Array): Promise<RunResult> => {
      const args = words(cmd[4] ?? "").slice(3);
      sent.push({ args, stdin: stdin === undefined ? "" : Buffer.from(stdin).toString() });
      const joined = args.join(" ");
      if (joined === `-n flux-system get ${GIT} flux-system -o json`) return ok(JSON.stringify({ metadata: { generation: 2 }, spec: { ref: state.ref } }));
      if (joined.startsWith(`-n flux-system patch ${GIT} flux-system --type=json --field-manager=trustiot-demo-flux -p `)) {
        const patch = JSON.parse(args.at(-1)!) as { op: string; path: string; value?: string }[];
        const test = patch.find((op) => op.op === "test");
        if (test?.value !== state.ref.commit) return { stdout: "", stderr: `The request is invalid: the server rejected our request due to an error in our request (test operation failed at ${test?.path})`, exitCode: 1 };
        const change = patch.find((op) => op.op !== "test")!;
        const { commit: _commit, ...rest } = state.ref;
        state.ref = change.op === "remove" ? rest : { ...rest, commit: change.value };
        return state.dropAfterPatch === true ? { stdout: "", stderr: "Connection to <ssh-host> closed by remote host.", exitCode: 255 } : ok("gitrepository.source.toolkit.fluxcd.io/flux-system patched\n");
      }
      if (joined === "apply --server-side --field-manager=trustiot-demo-flux -f -") {
        const doc = JSON.parse(Buffer.from(stdin ?? "").toString()) as { spec: { ref: { commit: string } } };
        state.ref = { ...state.ref, commit: doc.spec.ref.commit };
        return ok("gitrepository.source.toolkit.fluxcd.io/flux-system serverside-applied\n");
      }
      throw new Error(`unexpected kubectl ${joined}`);
    };
    const ops = boxOps(new Box("demo", log, { run, sleep: async () => {} }), OPTIONS, async () => new Uint8Array());
    return { ops, sent, state };
  };
  const apiVersion = parseManifests(DEMO_MANIFESTS).manifests!.gitApiVersion;

  test("(M1) pinGit applies the pin as trustiot-demo-flux, moves an earlier run's pin with a tested JSON patch first, and reads it back", async () => {
    const finish = gitBox({ ref: { branch: "main", commit: HEAD } });
    await finish.ops.pinGit(apiVersion, HEAD);
    expect(finish.sent.map((s) => s.args.slice(0, 3).join(" "))).toEqual(["apply --server-side --field-manager=trustiot-demo-flux", `-n flux-system get`]);
    expect(finish.sent[0]!.stdin).toBe(pinDocument(apiVersion, HEAD));

    const move = gitBox({ ref: { branch: "main", commit: OLD } });
    await move.ops.pinGit(apiVersion, HEAD, OLD);
    expect(JSON.parse(move.sent[0]!.args.at(-1)!)).toEqual([
      { op: "test", path: "/spec/ref/commit", value: OLD },
      { op: "replace", path: "/spec/ref/commit", value: HEAD },
    ]);
    expect(move.state.ref).toEqual({ branch: "main", commit: HEAD });

    // Someone else moved it meanwhile: the test fails, and the error says where it points.
    const other = gitBox({ ref: { branch: "main", commit: NEWER } });
    await expect(other.ops.pinGit(apiVersion, HEAD, OLD)).rejects.toThrow(`moving the pin of GitRepository flux-system from ${OLD} to ${HEAD} failed (exit 1: `);
    await expect(gitBox({ ref: { branch: "main", commit: NEWER } }).ops.pinGit(apiVersion, HEAD, OLD)).rejects.toThrow(`spec.ref is {branch: main, commit: ${NEWER}}`);
    expect(other.sent.some((s) => s.args[0] === "apply")).toBe(false);
  });

  test("(M1) unpinGit removes exactly this run's pin with a tested JSON patch, and an ssh drop after it landed is fine", async () => {
    const unpin = gitBox({ ref: { branch: "main", commit: HEAD } });
    await unpin.ops.unpinGit(HEAD);
    expect(JSON.parse(unpin.sent[0]!.args.at(-1)!)).toEqual([
      { op: "test", path: "/spec/ref/commit", value: HEAD },
      { op: "remove", path: "/spec/ref/commit" },
    ]);
    expect(unpin.sent).toHaveLength(1);
    expect(unpin.state.ref).toEqual({ branch: "main" });

    const dropped = gitBox({ ref: { branch: "main", commit: HEAD }, dropAfterPatch: true });
    await dropped.ops.unpinGit(HEAD);
    // Sent once (retry: false), then read back.
    expect(dropped.sent.map((s) => s.args[2])).toEqual(["patch", "get"]);

    await expect(gitBox({ ref: { branch: "main", commit: OLD } }).ops.unpinGit(HEAD)).rejects.toThrow(`removing the pin of GitRepository flux-system to ${HEAD} failed (exit 1: `);
    await expect(gitBox({ ref: { branch: "main" } }).ops.unpinGit("main")).rejects.toThrow("not a full commit hash");
  });

  test("(L3) exit 255 from a root script is StillRunningError with what to check; other failures are the script's, redacted", async () => {
    const dir = tempDir("demo-flux-box-");
    const log = new FluxLog("test", dir);
    let exit = 255;
    const run = hidingSshHost(
      async (cmd: string[]): Promise<RunResult> => {
        if (!(cmd[4] ?? "").startsWith("sudo bash -c")) throw new Error(`unexpected command ${cmd[4]}`);
        return exit === 255
          ? { stdout: "APPLY first\n", stderr: "Connection to demo-box closed by remote host.\nclient_loop: send disconnect: Broken pipe", exitCode: 255 }
          : { stdout: "", stderr: "error: dial tcp 192.0.2.10:6443: connect: no route to host", exitCode: 1 };
      },
      "demo-box",
    );
    const ops = boxOps(new Box("demo-box", log, { run, sleep: async () => {} }), OPTIONS, async () => new Uint8Array());
    const manifests = parseManifests(DEMO_MANIFESTS).manifests!;
    const still = await ops.applyFlux(manifests, pinnedBootstrap(manifests, HEAD)).then(
      () => new Error("no error"),
      (thrown: Error) => thrown,
    );
    expect(still).toBeInstanceOf(StillRunningError);
    expect(still.message).toContain("applying Flux's manifests: ssh ended with exit 255 and the root script may still be running on the demo machine (Connection to <ssh-host> closed by remote host.");
    expect(still.message).toContain(`\`sudo flock --nonblock ${REMOTE_LOCK} true\` fails while a root script of this file still runs`);
    await expect(ops.deleteFlux(manifests)).rejects.toBeInstanceOf(StillRunningError);
    await expect(ops.createDeployKey("flux-system", "x")).rejects.toBeInstanceOf(StillRunningError);
    // The age check reads the private key as root: it may still run too, so underLock keeps the lock.
    await expect(ops.sopsAge("sops-age")).rejects.toBeInstanceOf(StillRunningError);
    await expect(ops.sopsAge("sops-age")).rejects.toThrow("checking Secret flux-system/sops-age: ssh ended with exit 255 and the root script may still be running");
    exit = 1;
    expect(await ops.sopsAge("sops-age")).toEqual({ kind: "unreadable", detail: "exit 1: error: dial tcp <address>:6443: connect: no route to host" });
    const failed = await ops.applyFlux(manifests, pinnedBootstrap(manifests, HEAD)).then(
      () => new Error("no error"),
      (thrown: Error) => thrown,
    );
    expect(failed).not.toBeInstanceOf(StillRunningError);
    expect(failed.message).toBe("applying Flux's manifests failed (exit 1): error: dial tcp <address>:6443: connect: no route to host");
  });

  test("(M2) ssh's and kubectl's errors reach neither the output nor the error with a private name, address or path", async () => {
    const dir = tempDir("demo-flux-box-");
    const log = new FluxLog("test", dir);
    const stderr = [
      "ssh: Could not resolve hostname demo-box: Name or service not known",
      "ssh: connect to host 192.0.2.10 port 22: Connection timed out",
      "websocket: bad handshake from access.example.com (198.51.100.7) reading /home/someone/.cloudflared/config.yml",
    ].join("\n");
    const run = hidingSshHost(async (): Promise<RunResult> => ({ stdout: "", stderr, exitCode: 255 }), "demo-box");
    const ops = boxOps(new Box("demo-box", log, { run, sleep: async () => {} }), OPTIONS, async () => new Uint8Array());
    const root = mainReady();
    const error = await runStatus(ops, log, { repo: () => readRepo(root, CLEAN, build) }).then(
      () => new Error("no error"),
      (thrown: Error) => thrown,
    );
    expect(error.message).toContain("ssh: Could not resolve hostname <ssh-host>: Name or service not known");
    expect(error.message).toContain("ssh: connect to host <address> port 22");
    expect(error.message).toContain("websocket: bad handshake from <host> (<address>) reading ~/.cloudflared/config.yml");
    const output = written(dir);
    for (const text of PRIVATE_TEXT) {
      expect(error.message).not.toContain(text);
      expect(output).not.toContain(text);
    }
    // The lock owner line and kubectl's output, on stdout, stay as they are for the run to parse.
    const plain = await hidingSshHost(async () => ({ stdout: "Laysi/trustiot-v2#467 demo-box\n", stderr: "", exitCode: 0 }), "demo-box")(["ssh", "demo-box", "cat"]);
    expect(plain.stdout).toBe("Laysi/trustiot-v2#467 demo-box\n");
  });
});
