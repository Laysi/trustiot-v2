/**
 * Shared pieces of the demo environment's operations scripts (Laysi/trustiot-v2#457,
 * docs/runbooks/demo-environment.md). They run on a workstation with this repository and act on
 * the demo machine over ssh, the way deploy/flux/tests/e2e/lab.e2e.ts does: kubectl runs there as
 * `sudo k3s kubectl`, everything else here at the versions in .prototools.
 *
 * Every script that changes the cluster or the host:
 * - takes the lab lock (~/e2e.lock on the demo machine; docs/runbooks/demo-environment.md,
 *   「與 lab e2e 共用叢集」), or checks that the caller holds it for this issue (--lock-held);
 * - without --apply only reads and prints what it would do, with a confirmation code; --apply
 *   needs that code (--confirm), recomputes the plan and stops when the plan changed meanwhile;
 * - records the state before and after (with sha256 where it matters) and writes a log, under
 *   $XDG_STATE_HOME/trustiot-v2/demo/ (0700) or --log-dir;
 * - checks that the ops tunnel (the host's own cloudflared.service, which serves ssh to the demo
 *   machine) kept running with the same process, and never touches it;
 * - is safe to run again: each step looks at the current state first.
 *
 * Nothing here prints a secret value, a private host name or an address: logs name objects only.
 */

import { appendFileSync, chmodSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { tryRun, type RunResult } from "../release/exec.ts";
import { ReleaseError, errorMessage } from "../release/lib.ts";

export const ROOT = resolve(import.meta.dir, "..", "..");
export const ISSUE = "Laysi/trustiot-v2#457";
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

/** Quotes one argument for the remote POSIX shell. */
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

/** A log file of one run, 0600 in a 0700 directory; lines also go to the console. */
export class Log {
  readonly dir: string;
  readonly file: string;

  constructor(step: string, dir?: string) {
    const base = process.env.XDG_STATE_HOME || join(homedir(), ".local", "state");
    this.dir = resolve(dir ?? join(base, "trustiot-v2", "demo"));
    mkdirSync(this.dir, { recursive: true, mode: 0o700 });
    chmodSync(this.dir, 0o700);
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

/** The demo machine over ssh. */
export class Box {
  constructor(
    readonly host: string,
    readonly log: Log,
  ) {
    if (!/^[A-Za-z0-9][A-Za-z0-9_.-]*$/.test(host)) {
      throw new ReleaseError("--ssh-host must be an ssh host name or alias");
    }
  }

  /** Runs a shell command line on the box; returns the result whatever its exit status. */
  async sh(command: string, options: ShOptions = {}): Promise<RunResult> {
    const retry = options.retry ?? true;
    for (let attempt = 1; ; attempt += 1) {
      const result = await sshRun(["ssh", "-o", "BatchMode=yes", this.host, command], options.stdin);
      if (result.exitCode !== 255 || !retry || attempt === 4) return result;
      this.log.note(`ssh dropped the connection (attempt ${attempt}); retrying`);
      await Bun.sleep(3_000 * attempt);
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

  /** `sudo k3s kubectl` on the box. */
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

/** The owner line this run writes into the lock. */
export function lockOwner(step: string): string {
  return `${ISSUE} demo ${step} ${new Date().toISOString().replace(/\.\d+Z$/, "Z")}`;
}

/**
 * Takes the lab lock for a step and returns a release function, or, with `held`, checks that the
 * lock is already held for this issue (by the person or agent running the step) and returns a
 * no-op. Another holder stops the run before anything changes.
 */
export async function takeLock(box: Box, step: string, held: boolean): Promise<() => Promise<void>> {
  if (held) {
    const owner = (await box.sh(`cat ~/${LOCK_DIR}/owner 2>/dev/null`)).stdout.trim();
    if (!owner.startsWith(ISSUE)) {
      throw new ReleaseError(`--lock-held, but the lab lock is ${owner === "" ? "free" : `held by: ${owner}`}`);
    }
    box.log.note(`the lab lock is held for ${ISSUE}: ${owner}`);
    return async () => {};
  }
  const owner = lockOwner(step);
  const taken = await box.sh(`mkdir ~/${LOCK_DIR} 2>/dev/null && printf '%s\\n' ${shq(owner)} > ~/${LOCK_DIR}/owner && echo ACQUIRED || cat ~/${LOCK_DIR}/owner`, { retry: false });
  if (taken.stdout.trim() !== "ACQUIRED") {
    throw new ReleaseError(`the lab lock is held by: ${taken.stdout.trim() || "(unknown)"}; wait for it, or pass --lock-held if you hold it for ${ISSUE}`);
  }
  box.log.note(`took the lab lock: ${owner}`);
  return async () => {
    // Only our own lock: the owner line must still be the one this run wrote.
    const released = await box.sh(`[ "$(cat ~/${LOCK_DIR}/owner 2>/dev/null)" = ${shq(owner)} ] && rm -rf ~/${LOCK_DIR} && echo RELEASED`);
    box.log.note(released.stdout.trim() === "RELEASED" ? "released the lab lock" : "the lab lock was not ours any more; left it as it is");
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

export interface OpsTunnel {
  active: string;
  pid: string;
}

/** The ops tunnel's state, read only (systemctl show). */
export async function opsTunnel(box: Box): Promise<OpsTunnel> {
  const out = await box.mustSh(`systemctl show ${OPS_TUNNEL_UNIT} -p ActiveState -p MainPID`, "reading the ops tunnel's state");
  const field = (name: string) => new RegExp(`^${name}=(.*)$`, "m").exec(out)?.[1]?.trim() ?? "";
  return { active: field("ActiveState"), pid: field("MainPID") };
}

/** Stops the run when the ops tunnel is not active; it serves ssh to the demo machine. */
export function requireOpsTunnel(tunnel: OpsTunnel): void {
  if (tunnel.active !== "active") {
    throw new ReleaseError(`${OPS_TUNNEL_UNIT} is ${tunnel.active || "unknown"}: fix ssh access to the demo machine before changing anything`);
  }
}

/** A message for the end of a run: whether the ops tunnel kept its process. */
export function opsTunnelUnchanged(before: OpsTunnel, after: OpsTunnel): string {
  return before.pid === after.pid && after.active === "active"
    ? `${OPS_TUNNEL_UNIT} kept running (same process)`
    : `WARNING: ${OPS_TUNNEL_UNIT} changed (${before.active}/${before.pid} -> ${after.active}/${after.pid}); this run never touches it, check the host`;
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
