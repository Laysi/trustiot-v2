/**
 * Shared pieces of the demo environment's operations scripts (Laysi/trustiot-v2#457,
 * docs/runbooks/demo-environment.md). They run on a workstation with this repository and act on
 * the demo machine over ssh, the way deploy/flux/tests/e2e/lab.e2e.ts does: kubectl runs there as
 * `sudo k3s kubectl`, everything else here at the versions in .prototools.
 *
 * Every script that changes the cluster or the host:
 * - takes the lab lock (~/e2e.lock on the demo machine; docs/runbooks/demo-environment.md,
 *   「與 lab e2e 共用叢集」) for its own issue, or checks that the caller holds it for that issue
 *   (--lock-held);
 * - without --apply only reads and prints what it would do, with a confirmation code; --apply
 *   needs that code (--confirm), recomputes the plan and stops when the plan changed meanwhile;
 * - records the state before and after (with sha256 where it matters) and writes a log, under
 *   $XDG_STATE_HOME/trustiot-v2/demo/ (0700) or --log-dir;
 * - checks that the ops tunnel (the host's own cloudflared.service, which serves ssh to the demo
 *   machine) kept running with the same process, also when the run fails, and never touches it;
 * - is safe to run again: each step looks at the current state first.
 *
 * Nothing here prints a secret value, a private host name or an address: logs name objects only.
 * (ssh's own error lines, which lastLines() passes on, can name the host: filter them before a log
 * goes anywhere public.)
 */

import { appendFileSync, chmodSync, existsSync, mkdirSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { tryRun, type RunResult } from "../release/exec.ts";
import { ReleaseError, errorMessage } from "../release/lib.ts";

export const ROOT = resolve(import.meta.dir, "..", "..");
/** The namespace and Helm release of the demo environment (the SaaS default, deploy/istio). */
export const DEMO_NAMESPACE = "trustiot";
/** The lab lock on the demo machine, shared with every lab e2e test (CLAUDE.md). */
export const LOCK_DIR = "e2e.lock";
/**
 * The first release the demo environment may start with: it generates the internal identity
 * signing keys in the cluster (Laysi/trustiot-v2#404) and bootstraps Postgres with the PUBLIC
 * access to postgres and template1 revoked (Laysi/trustiot-v2#400), which only happens when the
 * Cluster is created. Earlier charts would need a person-made signing key and a manual revoke.
 */
export const DEMO_MIN_CHART_VERSION = "0.0.0-rc.8";
/** The host's own tunnel unit (ops ssh). Read only, never changed. */
export const OPS_TUNNEL_UNIT = "cloudflared.service";
/** The istiod ext_authz provider the chart refers to (deploy/istio/istiod-values.yaml). */
export const ISTIOD_PROVIDER = "trustiot-identity";
/**
 * What identifies each namespace of the 2026-10-09/10 feasibility PoC on the demo machine:
 * workloads that only the PoC had (cleanup.ts removes them; flux.ts refuses to install over them).
 */
export const POC_FINGERPRINTS: Record<string, { deployments: string[]; statefulsets: string[] }> = {
  [DEMO_NAMESPACE]: { deployments: ["app", "grafana", "mosquitto", "otel-collector"], statefulsets: ["clickhouse", "postgres", "qdrant"] },
  poc: { deployments: ["bridge", "consumer", "identity", "mosquitto-poc"], statefulsets: [] },
};

/**
 * Quotes one argument for the remote shell. The demo machine's login shell is POSIX (bash on
 * Fedora); a non-POSIX login shell such as fish would need other quoting.
 */
export function shq(arg: string): string {
  return /^[A-Za-z0-9_./:=@%+,-]+$/.test(arg) ? arg : `'${arg.replaceAll("'", `'\\''`)}'`;
}

/** A short code that names one plan exactly: --apply needs it, so what runs is what was shown. */
export function confirmCode(plan: unknown): string {
  return new Bun.CryptoHasher("sha256").update(stableJson(plan)).digest("hex").slice(0, 12);
}

/** JSON with sorted object keys, so equal plans give equal codes. */
export function stableJson(value: unknown): string {
  const sort = (v: unknown): unknown =>
    Array.isArray(v) ? v.map(sort) : v !== null && typeof v === "object" ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, sort((v as Record<string, unknown>)[k])])) : v;
  return JSON.stringify(sort(value));
}

export function sha256(text: string): string {
  return new Bun.CryptoHasher("sha256").update(text).digest("hex");
}

/**
 * A log file of one run, 0600 in a 0700 directory; lines also go to the console. A directory this
 * creates is made 0700; an existing one is never chmod-ed, and one that other users can read or
 * enter is refused (so `--log-dir .` cannot change the repository's permissions).
 */
export class Log {
  readonly dir: string;
  readonly file: string;

  constructor(step: string, dir?: string) {
    const base = process.env.XDG_STATE_HOME || join(homedir(), ".local", "state");
    this.dir = resolve(dir ?? join(base, "trustiot-v2", "demo"));
    if (existsSync(this.dir)) {
      const stat = statSync(this.dir);
      if (!stat.isDirectory()) throw new ReleaseError(`log directory ${this.dir} is not a directory`);
      if ((stat.mode & 0o077) !== 0) {
        throw new ReleaseError(`log directory ${this.dir} is open to other users (mode ${(stat.mode & 0o777).toString(8)}); use a private (0700) directory`);
      }
    } else {
      mkdirSync(this.dir, { recursive: true, mode: 0o700 });
      chmodSync(this.dir, 0o700);
    }
    const stamp = new Date().toISOString().replaceAll(":", "").replace(/\.\d+Z$/, "Z");
    this.file = join(this.dir, `${step}-${stamp}.log`);
    writeFileSync(this.file, "", { mode: 0o600 });
  }

  note(message: string): void {
    const line = `${new Date().toISOString()} ${message}`;
    console.log(line);
    appendFileSync(this.file, `${line}\n`);
  }

  /** Writes a JSON record next to the log (state before and after, plans). */
  save(name: string, data: unknown): string {
    const path = this.file.replace(/\.log$/, `-${name}.json`);
    writeFileSync(path, `${JSON.stringify(data, null, 2)}\n`, { mode: 0o600 });
    return path;
  }
}

export interface ShOptions {
  /** Bytes for the remote command's stdin. */
  stdin?: string | Uint8Array;
  /**
   * Repeat when ssh drops the connection before the command ran (exit 255). Only for commands
   * that are safe to run twice; off for anything that generates or consumes a secret.
   */
  retry?: boolean;
}

/** Runs one local command (ssh) and returns its result whatever its exit status. */
export type SshRunner = (cmd: string[], stdin?: string | Uint8Array) => Promise<RunResult>;

/** How a Box reaches the machine; tests replace both. */
export interface BoxIo {
  run?: SshRunner;
  sleep?: (ms: number) => Promise<void>;
}

/** The demo machine over ssh. */
export class Box {
  private readonly run: SshRunner;
  private readonly pause: (ms: number) => Promise<void>;

  constructor(
    readonly host: string,
    readonly log: Log,
    io: BoxIo = {},
  ) {
    if (!/^[A-Za-z0-9][A-Za-z0-9_.-]*$/.test(host)) {
      throw new ReleaseError("--ssh-host must be an ssh host name or alias");
    }
    this.run = io.run ?? sshRun;
    this.pause = io.sleep ?? ((ms) => Bun.sleep(ms));
  }

  /** Runs a shell command line on the box; returns the result whatever its exit status. */
  async sh(command: string, options: ShOptions = {}): Promise<RunResult> {
    const retry = options.retry ?? true;
    for (let attempt = 1; ; attempt += 1) {
      const result = await this.run(["ssh", "-o", "BatchMode=yes", this.host, command], options.stdin);
      if (result.exitCode !== 255 || !retry || attempt === 4) return result;
      this.log.note(`ssh dropped the connection (attempt ${attempt}); retrying`);
      await this.pause(3_000 * attempt);
    }
  }

  /** Like sh, but throws with the last lines of the output when the command fails. */
  async mustSh(command: string, what: string, options: ShOptions = {}): Promise<string> {
    const result = await this.sh(command, options);
    if (result.exitCode !== 0) {
      throw new ReleaseError(`${what} failed (exit ${result.exitCode}): ${lastLines(result.stderr || result.stdout)}`);
    }
    return result.stdout;
  }

  /** `sudo k3s kubectl` on the box; every argument is quoted for the remote shell. */
  async tryKubectl(args: readonly string[], stdin?: string): Promise<RunResult> {
    return this.sh(["sudo", "k3s", "kubectl", ...args].map(shq).join(" "), stdin === undefined ? {} : { stdin });
  }

  async kubectl(args: readonly string[], stdin?: string): Promise<string> {
    const result = await this.tryKubectl(args, stdin);
    if (result.exitCode !== 0) {
      throw new ReleaseError(`kubectl ${args.slice(0, 3).join(" ")} failed (exit ${result.exitCode}): ${lastLines(result.stderr || result.stdout)}`);
    }
    return result.stdout;
  }

  /** A JSON object from kubectl, or undefined when it does not exist (NotFound or no such kind). */
  async getJson(args: readonly string[]): Promise<Record<string, unknown> | undefined> {
    const result = await this.tryKubectl([...args, "-o", "json"]);
    if (result.exitCode === 0) return JSON.parse(result.stdout) as Record<string, unknown>;
    if (/\(NotFound\)|the server doesn't have a resource type|no matches for kind/.test(result.stderr)) return undefined;
    throw new ReleaseError(`kubectl get ${args.join(" ")} failed: ${lastLines(result.stderr)}`);
  }

  /** The items of a kubectl list; empty when the kind does not exist on this cluster. */
  async items(args: readonly string[]): Promise<Record<string, unknown>[]> {
    const list = await this.getJson(args);
    return Array.isArray(list?.items) ? (list.items as Record<string, unknown>[]) : [];
  }
}

async function sshRun(cmd: string[], stdin?: string | Uint8Array): Promise<RunResult> {
  if (stdin === undefined || typeof stdin === "string") {
    return tryRun(cmd, stdin === undefined ? {} : { stdin });
  }
  const proc = Bun.spawn(cmd, { stdin: new Blob([stdin]), stdout: "pipe", stderr: "pipe" });
  const [stdout, stderr, exitCode] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
  return { stdout, stderr, exitCode };
}

export function lastLines(text: string, count = 5): string {
  return text.trimEnd().split("\n").slice(-count).join("\n");
}

// --- The lab lock ------------------------------------------------------------------------------

/** An issue reference that may own the lab lock: `Laysi/trustiot-v2#<number>`. */
function requireIssue(issue: string): void {
  if (!/^Laysi\/trustiot-v2#\d+$/.test(issue)) throw new ReleaseError(`not an issue reference: ${JSON.stringify(issue)}`);
}

/**
 * The owner line a run writes into the lock: `<issue> <UTC time> demo <step>`, the format of
 * deploy/flux/README.md (「lab 測試」) with the step after the time.
 */
export function lockOwner(issue: string, step: string, now: Date = new Date()): string {
  requireIssue(issue);
  return `${issue} ${now.toISOString().replace(/\.\d+Z$/, "Z")} demo ${step}`;
}

/** Whether an owner line belongs to this issue: the exact issue, then a space. */
export function ownsLock(owner: string, issue: string): boolean {
  requireIssue(issue);
  return owner.startsWith(`${issue} `);
}

const LOCK_HELP = `on the demo machine, read ~/${LOCK_DIR}/owner and check that no kubectl of the interrupted run is still running (ps -ef | grep kubectl); only then, and only when the owner line is that run's, remove the lock with: rm -rf ~/${LOCK_DIR}`;

/**
 * Takes the lab lock for a step of `issue` and returns a release function, or, with `held`, checks
 * that the lock is already held for this issue (by the person or agent running the step) and
 * returns a no-op. Another holder stops the run before anything changes. The mkdir runs once (no
 * retry); when ssh drops during it, the owner line is read back to learn whether the lock is ours.
 * The release removes the lock only while the owner line is still this run's, and it never throws:
 * a failed release is logged with what to check and how to remove the lock by hand.
 */
export async function takeLock(box: Box, issue: string, step: string, held: boolean): Promise<() => Promise<void>> {
  requireIssue(issue);
  const readOwner = async (): Promise<string | undefined> => {
    const read = await box.sh(`cat ~/${LOCK_DIR}/owner 2>/dev/null || true`);
    return read.exitCode === 0 ? read.stdout.trim() : undefined;
  };
  if (held) {
    const owner = await readOwner();
    if (owner === undefined) throw new ReleaseError("could not read the lab lock (ssh failed); nothing changed");
    if (!ownsLock(owner, issue)) {
      throw new ReleaseError(`--lock-held, but the lab lock is ${owner === "" ? "free or has no owner line" : `held by: ${owner}`}; it must be held for ${issue}`);
    }
    box.log.note(`the lab lock is held for ${issue}: ${owner}`);
    return async () => {};
  }
  const owner = lockOwner(issue, step);
  const taken = await box.sh(`mkdir ~/${LOCK_DIR} 2>/dev/null && printf '%s\\n' ${shq(owner)} > ~/${LOCK_DIR}/owner && echo ACQUIRED || { printf 'HELD '; cat ~/${LOCK_DIR}/owner 2>/dev/null; }`, {
    retry: false,
  });
  if (taken.exitCode === 255) {
    const now = await readOwner();
    if (now !== owner) {
      throw new ReleaseError(`ssh dropped while taking the lab lock; it now reads: ${now === undefined ? "(unreadable)" : now || "(no owner line)"}. If that is "${owner}", this run took it: ${LOCK_HELP}`);
    }
  } else if (taken.stdout.trim() !== "ACQUIRED") {
    const holder = taken.stdout.trim().replace(/^HELD\s*/, "");
    throw new ReleaseError(`the lab lock is held by: ${holder || "(no owner line)"}; wait for it, or pass --lock-held if you hold it for ${issue}`);
  }
  box.log.note(`took the lab lock: ${owner}`);
  return async () => {
    const released = await box.sh(
      `if [ "$(cat ~/${LOCK_DIR}/owner 2>/dev/null)" = ${shq(owner)} ]; then rm -rf ~/${LOCK_DIR} && echo RELEASED; else printf 'NOT_OURS '; cat ~/${LOCK_DIR}/owner 2>/dev/null; fi`,
    );
    const out = released.stdout.trim();
    if (released.exitCode === 0 && out === "RELEASED") box.log.note("released the lab lock");
    else if (released.exitCode === 0 && out.startsWith("NOT_OURS")) box.log.note(`the lab lock is no longer this run's (owner: ${out.replace(/^NOT_OURS\s*/, "") || "none"}); left it as it is`);
    else box.log.note(`WARNING: releasing the lab lock failed (${released.exitCode === 255 ? "ssh" : `exit ${released.exitCode}`}); this run's owner line is "${owner}": ${LOCK_HELP}`);
  };
}

// --- State records -----------------------------------------------------------------------------

/** Cluster-scoped kinds whose objects a step records before and after. */
export const CLUSTER_KINDS = [
  "namespaces",
  "persistentvolumes",
  "customresourcedefinitions.apiextensions.k8s.io",
  "clusterroles.rbac.authorization.k8s.io",
  "clusterrolebindings.rbac.authorization.k8s.io",
  "validatingwebhookconfigurations.admissionregistration.k8s.io",
  "mutatingwebhookconfigurations.admissionregistration.k8s.io",
  "gatewayclasses.gateway.networking.k8s.io",
  "helmcharts.helm.cattle.io",
] as const;

export interface KindState {
  names: string[];
  sha256: string;
  /** ClusterRoleBindings only: each binding's subjects, `<Kind>:<namespace>/<name>` (`<Kind>:<name>` without a namespace). */
  subjects?: Record<string, string[]>;
}

/** A subject of a (Cluster)RoleBinding as `<Kind>:<namespace>/<name>`, or `<Kind>:<name>` without a namespace. */
export function subjectRef(subject: { kind?: string; namespace?: string; name?: string }): string {
  return `${subject.kind ?? "?"}:${subject.namespace ? `${subject.namespace}/` : ""}${subject.name ?? "?"}`;
}

/**
 * The objects of each cluster-scoped kind (helmcharts: the k3s HelmCharts in kube-system), without
 * the fields the API server changes by itself. uid and creationTimestamp stay, so an object deleted
 * and created again does not compare equal.
 */
export async function clusterState(box: Box): Promise<Record<string, KindState>> {
  const state: Record<string, KindState> = {};
  for (const kind of CLUSTER_KINDS) {
    const items = (await box.items(kind === "helmcharts.helm.cattle.io" ? ["-n", "kube-system", "get", kind] : ["get", kind]))
      .map((item) => {
        const metadata = { ...(item.metadata as Record<string, unknown>) };
        delete metadata.resourceVersion;
        delete metadata.managedFields;
        delete metadata.generation;
        const { status: _status, ...rest } = item;
        return { ...rest, metadata };
      })
      .sort((a, b) => String(a.metadata.name).localeCompare(String(b.metadata.name)));
    state[kind] = { names: items.map((item) => String(item.metadata.name)), sha256: sha256(JSON.stringify(items)) };
    if (kind === "clusterrolebindings.rbac.authorization.k8s.io") {
      state[kind].subjects = Object.fromEntries(
        items.map((item) => [String(item.metadata.name), ((item as { subjects?: { kind?: string; namespace?: string; name?: string }[] }).subjects ?? []).map(subjectRef)]),
      );
    }
  }
  return state;
}

/** Workloads and Services of every namespace, with uids: `<kind>/<namespace>/<name>/<uid>`. */
export async function workloads(box: Box): Promise<string[]> {
  const out = await box.kubectl([
    "get",
    "deployments,statefulsets,daemonsets,services",
    "-A",
    "-o",
    'jsonpath={range .items[*]}{.kind}/{.metadata.namespace}/{.metadata.name}/{.metadata.uid}{"\\n"}{end}',
  ]);
  return out
    .split("\n")
    .filter((line) => line !== "")
    .sort();
}

/** Which kinds changed between two records, and the names that came and went. */
export function stateDiff(before: Record<string, KindState>, after: Record<string, KindState>): Record<string, { added: string[]; removed: string[]; changed: boolean }> {
  const diff: Record<string, { added: string[]; removed: string[]; changed: boolean }> = {};
  for (const kind of Object.keys(before)) {
    const b = before[kind]!;
    const a = after[kind] ?? { names: [], sha256: "" };
    if (a.sha256 === b.sha256) continue;
    diff[kind] = { added: a.names.filter((n) => !b.names.includes(n)), removed: b.names.filter((n) => !a.names.includes(n)), changed: true };
  }
  return diff;
}

// --- The ops tunnel ----------------------------------------------------------------------------

export interface OpsTunnel {
  active: string;
  pid: string;
  /** systemd's restart counter and the main process's start time: a restart between two reads changes them even if the PID comes back. */
  restarts: string;
  started: string;
}

/** The ops tunnel's state, read only (systemctl show). */
export async function opsTunnel(box: Box): Promise<OpsTunnel> {
  const out = await box.mustSh(`systemctl show ${OPS_TUNNEL_UNIT} -p ActiveState -p MainPID -p NRestarts -p ExecMainStartTimestamp`, "reading the ops tunnel's state");
  const field = (name: string) => new RegExp(`^${name}=(.*)$`, "m").exec(out)?.[1]?.trim() ?? "";
  return { active: field("ActiveState"), pid: field("MainPID"), restarts: field("NRestarts"), started: field("ExecMainStartTimestamp") };
}

/** Stops the run when the ops tunnel is not active; it serves ssh to the demo machine. */
export function requireOpsTunnel(tunnel: OpsTunnel): void {
  if (tunnel.active !== "active") {
    throw new ReleaseError(`${OPS_TUNNEL_UNIT} is ${tunnel.active || "unknown"}: fix ssh access to the demo machine before changing anything`);
  }
}

/** A message for the end of a run: whether the ops tunnel kept its process. */
export function opsTunnelUnchanged(before: OpsTunnel, after: OpsTunnel): string {
  const same = before.pid === after.pid && before.restarts === after.restarts && before.started === after.started && after.active === "active";
  return same
    ? `${OPS_TUNNEL_UNIT} kept running (same process)`
    : `WARNING: ${OPS_TUNNEL_UNIT} changed (${before.active}/${before.pid}/restarts ${before.restarts}/${before.started} -> ${after.active}/${after.pid}/restarts ${after.restarts}/${after.started}); this run never touches it, check the host`;
}

/**
 * Runs `body` between two reads of the ops tunnel: refuses to start unless it is active, and logs
 * whether it kept its process when the body ends, also when the body fails.
 */
export async function withOpsTunnel<T>(read: () => Promise<OpsTunnel>, log: Log, body: () => Promise<T>): Promise<T> {
  const before = await read();
  requireOpsTunnel(before);
  try {
    return await body();
  } finally {
    let message: string;
    try {
      message = opsTunnelUnchanged(before, await read());
    } catch (error) {
      message = `WARNING: could not read ${OPS_TUNNEL_UNIT} after the run: ${errorMessage(error)}`;
    }
    log.note(message);
  }
}

// --- Command line --------------------------------------------------------------------------------

export interface CommonOptions {
  sshHost: string;
  apply: boolean;
  confirm: string;
  lockHeld: boolean;
  logDir?: string;
}

/** --ssh-host (or DEMO_SSH_HOST), --apply, --confirm, --lock-held and --log-dir. */
export function commonOptions(options: Record<string, string>): CommonOptions {
  const sshHost = options["ssh-host"] || process.env.DEMO_SSH_HOST || "";
  if (sshHost === "") {
    throw new ReleaseError("set --ssh-host or DEMO_SSH_HOST to the ssh host of the demo machine");
  }
  return {
    sshHost,
    apply: options.apply === "true",
    confirm: options.confirm ?? "",
    lockHeld: options["lock-held"] === "true",
    ...(options["log-dir"] ? { logDir: options["log-dir"] } : {}),
  };
}

export const COMMON_SPEC = {
  "ssh-host": { default: "" },
  apply: { flag: true },
  confirm: { default: "" },
  "lock-held": { flag: true },
  "log-dir": { default: "" },
} as const;

/** Stops --apply unless --confirm names the plan that was shown; prints the code without --apply. */
export function checkConfirmation(log: Log, plan: unknown, options: CommonOptions, command: string): boolean {
  const code = confirmCode(plan);
  if (!options.apply) {
    log.note(`dry run: nothing changed. To make these changes, run again with: ${command} --apply --confirm ${code}`);
    return false;
  }
  if (options.confirm !== code) {
    throw new ReleaseError(
      options.confirm === ""
        ? `--apply needs --confirm ${code} (the code of the plan above; run without --apply to review it)`
        : `the plan changed since it was shown (code ${code}, not ${options.confirm}); review it again without --apply`,
    );
  }
  return true;
}

/** Runs a script's body and turns errors into one line and exit status 1. */
export async function main(log: () => Log | undefined, body: () => Promise<void>): Promise<void> {
  try {
    await body();
  } catch (error) {
    const message = errorMessage(error);
    const current = log();
    if (current !== undefined) current.note(`ERROR ${message}`);
    else console.error(message);
    if (current !== undefined) console.error(`log: ${current.file}`);
    process.exit(1);
  }
}

export function fileExists(path: string): boolean {
  return existsSync(path);
}
