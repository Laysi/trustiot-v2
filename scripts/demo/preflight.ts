/**
 * The demo runbook's read-only pre-demo checks in one command (Laysi/trustiot-v2#1180;
 * docs/runbooks/demo-presentation.md, 「展示前檢查」), each with PASS, WARN or FAIL and its
 * evidence:
 *
 *   bun scripts/demo/preflight.ts --ssh-host <host> [--gateway-ssh-host <host>] [--log-dir <dir>]
 *
 * Items: Flux and services (the lab lock, the HelmRelease against main's chart pin, the pulled
 * chart, every pod), memory, the dashboard check (informational), data freshness and gateway
 * presence (one ClickHouse read), and the gateway itself (only with --gateway-ssh-host; skipped
 * and said so otherwise). Exit status 1 when any item is FAIL, 0 otherwise (WARN and SKIP do not
 * fail a run).
 *
 * Read only, always: no lab lock, no kubectl apply, patch, delete, scale, rollout, annotate or
 * label, no write on the gateway, no restart. The only kubectl verbs are get, top and exec, and the
 * exec runs one clickhouse-client with `--readonly 1` that selects aggregates (newest received time,
 * seconds ago, row count, the gateway's last presence state and time), never a tenant row. The
 * ClickHouse account and password file come from the container's own environment and the password
 * is passed to the client in its environment, so it is never on a command line (the host's, the
 * container's) or in the output.
 *
 * What is printed is meant for a public test report: Flux's part reuses flux.ts (FluxOps,
 * chartState, redact), every line goes through FluxLog (redact()) and ssh's own errors name the ssh
 * host only as <ssh-host>. The node's name (kubectl top node) is never printed.
 *
 * Every ssh command has a bound that names what it waited for (SSH_TIMEOUT_MS); a host that does
 * not answer is a FAIL of its items with the reason, and the other items still run.
 */

import { type RunResult, parseOptions, type tryRun } from "../release/exec.ts";
import { ReleaseError, errorMessage, isMap } from "../release/lib.ts";
import {
  FluxLog,
  type FluxOps,
  type Inputs,
  KINDS,
  RELEASE_NAME,
  type RepoInputs,
  boxOps,
  kustomizeFluxSystem,
  readCode,
  readRepo,
  chartState,
  cleanErrors,
  fluxReadiness,
  redact,
  revisionOf,
} from "./flux.ts";
import { Box, DEMO_NAMESPACE, ROOT, type SshRunner, commonOptions, hidingSshHost, lastLines, main } from "./lib.ts";

// --- Thresholds --------------------------------------------------------------------------------

/** FAIL when the newest telemetry row was received more than this many seconds ago: the runbook's bound (one row per data point per minute, so a few minutes of slack). */
export const FRESH_LIMIT_S = 180;
/**
 * WARN when the demo machine's available memory (`free -m`, the "available" column) is below this.
 * The runbook has no bound. The machine has 7.5 GiB and the 2026-11-07 reading was available about
 * 2.9 GiB with node memory at 42 to 44 %; turning on the dashboard check adds up to its 512 MiB
 * limit and a chart upgrade briefly runs old and new pods side by side. 2 GiB leaves that room with
 * about 0.9 GiB to spare and still warns well before the machine starts to swap hard. A WARN, not a
 * FAIL: low memory is a reason to look (the runbook's 「與 lab e2e 共用叢集」), not proof that the
 * demo cannot run.
 */
export const MEMORY_WARN_AVAILABLE_MIB = 2048;
/** WARN when the node's memory as `kubectl top node` reports it reaches this percent (the baseline is 42 to 44 %). */
export const MEMORY_WARN_NODE_PERCENT = 70;

/** One ssh command, from start to its end: the longest kubectl exec or top on this machine takes a few seconds; this is generous for a loaded machine, and fails naming what it waited for. */
export const SSH_TIMEOUT_MS = 90_000;
/** ssh's own connection bound (TCP connect and the handshake), so that an unreachable host fails fast. */
export const SSH_CONNECT_TIMEOUT_S = 20;
/** Each local command that reads main's chart pin (git fetch and the others, kubectl kustomize): bounded and killed with its children when it runs out. */
export const REPO_COMMAND_TIMEOUT_MS = 90_000;
/** After an ssh process ended or was killed, how long its output pipes may stay open (a ProxyCommand child can hold them). */
const PIPE_GRACE_MS = 5_000;

// --- Results -----------------------------------------------------------------------------------

export type Verdict = "PASS" | "WARN" | "FAIL" | "SKIP";

export interface Result {
  verdict: Verdict;
  /** The item's name, as the runbook calls it. */
  name: string;
  /** What was found: the first line is the evidence; more lines are details (pods). */
  evidence: string[];
}

export const NAMES = {
  flux: "Flux and services",
  memory: "memory",
  dashboard: "dashboard check",
  data: "data freshness",
  presence: "gateway presence",
  gateway: "the gateway itself",
} as const;

/** The lines of one result. */
export function resultLines(result: Result): string[] {
  const [first = "", ...rest] = result.evidence;
  return [`${result.verdict} ${result.name}: ${first}`, ...rest.map((line) => `  ${line}`)];
}

export function countVerdicts(results: readonly Result[]): Record<Verdict, number> {
  const counts: Record<Verdict, number> = { PASS: 0, WARN: 0, FAIL: 0, SKIP: 0 };
  for (const result of results) counts[result.verdict] += 1;
  return counts;
}

/** The last line of a run, and its exit status: 1 with any FAIL. */
export function summary(results: readonly Result[]): { line: string; exitCode: number } {
  const c = countVerdicts(results);
  const exitCode = c.FAIL > 0 ? 1 : 0;
  return { line: `summary: ${c.PASS} PASS, ${c.WARN} WARN, ${c.FAIL} FAIL, ${c.SKIP} SKIP; ${exitCode === 0 ? "no item failed" : "at least one item failed"} (exit ${exitCode})`, exitCode };
}

/**
 * Folds a problem list and a fact list into a result: FAIL with any failure, else WARN with any
 * warning, else PASS. With problems the first line is the problems and the facts follow as `ok:`.
 */
function judged(name: string, facts: readonly string[], warnings: readonly string[], failures: readonly string[], details: readonly string[] = []): Result {
  const verdict: Verdict = failures.length > 0 ? "FAIL" : warnings.length > 0 ? "WARN" : "PASS";
  const problems = [...failures, ...warnings];
  const lines = problems.length === 0 ? [facts.join("; ")] : [problems.join("; "), ...(facts.length > 0 ? [`ok: ${facts.join("; ")}`] : [])];
  return { verdict, name, evidence: [...lines, ...details] };
}

/** A step that reads the host: a thrown error is a FAIL of the item with the reason (`verdict` WARN for informational items). */
async function guarded(name: string, body: () => Promise<Result[]>, verdict: "FAIL" | "WARN" = "FAIL"): Promise<Result[]> {
  try {
    return await body();
  } catch (error) {
    return [{ verdict, name, evidence: [`could not read it: ${errorMessage(error)}`] }];
  }
}

// --- Bounded ssh -------------------------------------------------------------------------------

/** The process groups of the commands that are running now (runBounded starts each in its own). */
const running = new Set<number>();

/** Kills what is still running, with its children (ssh's ProxyCommand): at exit and on SIGINT or SIGTERM. */
export function killRunning(): void {
  for (const pid of running) {
    try {
      process.kill(-pid, "SIGKILL");
    } catch {
      // already gone
    }
  }
  running.clear();
}

/**
 * Runs one command with a bound, in its own process group: after `timeoutMs` the whole group is
 * killed (ssh and its ProxyCommand child) and the result is exit 124 with a line that says what
 * timed out. Output pipes get `graceMs` after the process ends, in case something outside the group
 * keeps them open.
 */
export async function runBounded(cmd: readonly string[], stdin: string | Uint8Array | undefined, timeoutMs: number, graceMs: number = PIPE_GRACE_MS): Promise<RunResult> {
  const proc = Bun.spawn([...cmd], { stdin: stdin === undefined ? "ignore" : new Blob([stdin]), stdout: "pipe", stderr: "pipe", detached: true });
  running.add(proc.pid);
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    try {
      process.kill(-proc.pid, "SIGKILL");
    } catch {
      proc.kill("SIGKILL");
    }
  }, timeoutMs);
  try {
    const reads = Promise.all([new Response(proc.stdout as ReadableStream).text(), new Response(proc.stderr as ReadableStream).text()]);
    const exitCode = await proc.exited;
    const [stdout, stderr] = await Promise.race([reads, Bun.sleep(graceMs).then((): [string, string] => ["", ""])]);
    if (timedOut) return { stdout, stderr: `${stderr}${stderr === "" || stderr.endsWith("\n") ? "" : "\n"}the command timed out after ${Math.round(timeoutMs / 1000)} s`, exitCode: 124 };
    return { stdout, stderr, exitCode };
  } finally {
    clearTimeout(timer);
    running.delete(proc.pid);
  }
}

/** The ssh runner of a run: ssh's connection bound added, every command bounded. */
export function boundedSsh(timeoutMs: number = SSH_TIMEOUT_MS): SshRunner {
  return (cmd, stdin) => {
    const bounded = cmd[0] === "ssh" ? ["ssh", "-o", `ConnectTimeout=${SSH_CONNECT_TIMEOUT_S}`, "-o", "ServerAliveInterval=10", "-o", "ServerAliveCountMax=3", ...cmd.slice(1)] : cmd;
    return runBounded(bounded, stdin, timeoutMs);
  };
}

/**
 * Whether the host answers ssh, with one attempt (no retry): undefined when it does, else why not
 * in one line. An unreachable host then costs one bound, not one per command.
 */
export async function unreachable(box: Box): Promise<string | undefined> {
  const result = await box.sh("true", { retry: false });
  if (result.exitCode === 0) return undefined;
  const why = lastLines(result.stderr || result.stdout, 2).replace(/\s+/g, " ").trim();
  return `ssh failed (exit ${result.exitCode})${why === "" ? "" : `: ${why}`}`;
}

// --- Flux and services -------------------------------------------------------------------------

export interface Pod {
  name: string;
  phase: string;
  /** The `ready` flag of each container. */
  ready: string[];
  restarts: number;
}

/** Parses a line of the pod list that FluxOps.pods reads: `<name> <phase> ready=<true true> restarts=<0 0>`. */
export function parsePod(line: string): Pod {
  const match = /^(\S+) (\S*) ready=(.*?) restarts=(.*)$/.exec(line.trim());
  if (match === null) throw new ReleaseError(`cannot read the pod line ${JSON.stringify(redact(line).slice(0, 80))}`);
  const restarts = match[4]!.split(/\s+/).filter((n) => n !== "");
  if (restarts.some((n) => !/^\d+$/.test(n))) throw new ReleaseError(`cannot read the restart counts of pod ${match[1]!}`);
  return { name: match[1]!, phase: match[2]!, ready: match[3]!.split(/\s+/).filter((r) => r !== ""), restarts: restarts.reduce((sum, n) => sum + Number(n), 0) };
}

/** A pod is fine when it is Running with every container ready, or finished (a Job's pod: Succeeded). */
export function podProblem(pod: Pod): string | undefined {
  if (pod.phase === "Succeeded") return undefined;
  if (pod.phase !== "Running") return `${pod.name} is ${pod.phase || "in no phase"}`;
  if (pod.ready.length === 0 || pod.ready.some((r) => r !== "true")) return `${pod.name} is Running but not ready (ready=${pod.ready.join(",") || "none"})`;
  return undefined;
}

/** What the Flux item reads: FluxOps' reads, and main's pin. */
export type FluxReader = Pick<FluxOps, "lockState" | "object" | "pods">;

export interface FluxSources {
  ops: FluxReader;
  /** The pods of namespace trustiot (one read, shared with the dashboard item). */
  pods: () => Promise<string[]>;
  /** What main says (the chart it pins); the same inputs flux.ts status reads. */
  repo: () => Promise<RepoInputs>;
}

export async function checkFlux(sources: FluxSources): Promise<Result> {
  const { ops } = sources;
  const facts: string[] = [];
  const warnings: string[] = [];
  const failures: string[] = [];
  const details: string[] = [];

  const lock = await ops.lockState();
  if (lock.kind === "free") facts.push("lab lock free");
  else if (lock.kind === "held") failures.push(`lab lock held by ${redact(lock.owner)} (its owner is using the cluster; ask them to release it before the demo)`);
  else failures.push("lab lock unreadable (a lock directory without an owner line)");

  let repo: RepoInputs | undefined;
  try {
    repo = await sources.repo();
  } catch (error) {
    warnings.push(`main's chart pin not read (${errorMessage(error)}), so the version is not compared`);
  }
  if (repo !== undefined && repo.code.length > 0) warnings.push(`the checkout is not a clean origin/main, so main's pin may differ from what Flux follows (${redact(repo.code.join(" | ")).slice(0, 200)})`);

  const oci = await ops.object(KINDS.ociRepository, DEMO_NAMESPACE, RELEASE_NAME);
  const helm = await ops.object(KINDS.helmRelease, DEMO_NAMESPACE, RELEASE_NAME);
  if (helm === undefined) {
    failures.push(`HelmRelease ${DEMO_NAMESPACE}/${RELEASE_NAME} does not exist`);
  } else {
    const deployedVersion = revisionOf("release", helm);
    const ready = fluxReadiness(helm);
    const suspended = isMap(helm.spec) && helm.spec.suspend === true;
    if (suspended) failures.push("HelmRelease is suspended");
    if (!ready.done) failures.push(`HelmRelease not ready (${ready.state})`);
    if (repo?.chart !== undefined) {
      const chart = chartState(oci, helm, repo.head, repo.chart);
      if (deployedVersion === "") failures.push(`HelmRelease has no deployed release (latest: ${chart.deployed === "" ? "no history" : redact(chart.deployed)})`);
      else if (chart.deployedSame) facts.push(`HelmRelease deployed ${redact(deployedVersion)} = main's pin ${repo.chart.version}`);
      else failures.push(`HelmRelease deployed ${redact(deployedVersion)}, but main pins ${repo.chart.version}`);
      if (oci === undefined) failures.push("chart OCIRepository does not exist");
      else if (chart.pulledSame) facts.push("pulled chart digest = main's pin");
      else failures.push(`pulled chart ${chart.pulled === "" ? "nothing yet" : redact(chart.pulled).slice(0, 80)} is not main's pin`);
    } else {
      if (deployedVersion === "") failures.push("HelmRelease has no deployed release");
      else facts.push(`HelmRelease deployed ${redact(deployedVersion)}`);
      if (repo !== undefined) warnings.push("main pins no chart for the demo cluster, so the version is not compared");
    }
  }

  const pods = (await sources.pods()).map(parsePod);
  const problems = pods.map(podProblem).filter((p): p is string => p !== undefined);
  if (pods.length === 0) failures.push(`no pods in namespace ${DEMO_NAMESPACE}`);
  failures.push(...problems);
  const finished = pods.filter((p) => p.phase === "Succeeded").length;
  const ready = pods.filter((p) => p.phase === "Running" && podProblem(p) === undefined).length;
  const restarted = pods.filter((p) => p.restarts > 0);
  if (restarted.length > 0) {
    warnings.push(`${restarted.length} pod(s) restarted (the runbook wants no restarts that keep growing; compare with an earlier reading)`);
    for (const pod of restarted) details.push(`${pod.name}: restarts=${pod.restarts}`);
  }
  facts.push(`${ready} of ${pods.length - finished} pods Running and ready${finished > 0 ? ` (${finished} finished Job pods)` : ""}, ${pods.reduce((sum, p) => sum + p.restarts, 0)} restarts in total`);
  return judged(NAMES.flux, facts, warnings, failures, details);
}

// --- Memory ------------------------------------------------------------------------------------

export interface Memory {
  totalMiB: number;
  availableMiB: number;
}

/** `free -m`: the Mem row's total and available columns, found by the header. */
export function parseFree(out: string): Memory {
  const lines = out.split("\n");
  const header = lines.find((l) => /\bavailable\b/.test(l) && /\btotal\b/.test(l));
  const mem = lines.find((l) => l.startsWith("Mem:"));
  if (header === undefined || mem === undefined) throw new ReleaseError("`free -m` printed no Mem row with an available column");
  const columns = header.trim().split(/\s+/);
  const values = mem.trim().split(/\s+/).slice(1);
  const at = (name: string) => Number(values[columns.indexOf(name)]);
  const memory = { totalMiB: at("total"), availableMiB: at("available") };
  if (!Number.isFinite(memory.totalMiB) || !Number.isFinite(memory.availableMiB) || memory.totalMiB <= 0) throw new ReleaseError("could not read total and available memory from `free -m`");
  return memory;
}

/** `kubectl top node`: the highest MEMORY(%) of its rows (the node's name is not kept). */
export function parseTopNodePercent(out: string): number {
  const rows = out.split("\n").filter((l) => l.trim() !== "" && !/^NAME\b/.test(l));
  const percents = rows.map((row) => /\s(\d+)%\s*$/.exec(row)?.[1]).map((p) => (p === undefined ? Number.NaN : Number(p)));
  if (percents.length === 0 || percents.some((p) => Number.isNaN(p))) throw new ReleaseError("`kubectl top node` printed no node with a memory percent (is metrics-server up?)");
  return Math.max(...percents);
}

export function judgeMemory(memory: Memory, nodePercent: number): Result {
  const availablePercent = Math.round((memory.availableMiB / memory.totalMiB) * 100);
  const facts = `available ${memory.availableMiB} MiB of ${memory.totalMiB} MiB (${availablePercent} %), node memory ${nodePercent} %`;
  const warnings: string[] = [];
  if (memory.availableMiB < MEMORY_WARN_AVAILABLE_MIB) warnings.push(`available memory is below ${MEMORY_WARN_AVAILABLE_MIB} MiB`);
  if (nodePercent >= MEMORY_WARN_NODE_PERCENT) warnings.push(`node memory is at or above ${MEMORY_WARN_NODE_PERCENT} %`);
  return judged(NAMES.memory, [facts], warnings, []);
}

export async function checkMemory(box: Box): Promise<Result> {
  const memory = parseFree(await box.mustSh("free -m", "reading free memory (free -m)"));
  const nodePercent = parseTopNodePercent(await box.kubectl(["top", "node"]));
  return judgeMemory(memory, nodePercent);
}

// --- Dashboard check ---------------------------------------------------------------------------

/** The dashboard check service's pods: the chart names them `<release>-dashboard-check` (Laysi/trustiot-v2#881). */
export function dashboardCheckResult(pods: readonly Pod[]): Result {
  const found = pods.filter((p) => /dashboard-check/.test(p.name));
  if (found.length === 0) {
    return { verdict: "PASS", name: NAMES.dashboard, evidence: ["off (no dashboard-check pod; informational: the assistant then writes component-mode dashboards only)"] };
  }
  const notReady = found.filter((p) => podProblem(p) !== undefined);
  if (notReady.length > 0) {
    return { verdict: "WARN", name: NAMES.dashboard, evidence: [`on, but ${notReady.map((p) => podProblem(p)).join("; ")} (informational: custom pages may fail)`] };
  }
  return { verdict: "PASS", name: NAMES.dashboard, evidence: [`on (${found.length} pod(s) ready; informational: the assistant may write custom pages)`] };
}

// --- Data freshness and gateway presence -------------------------------------------------------

/**
 * Why the freshness comes from `system.parts` and not `max(received_at)` of the table: the admin
 * account is named by no row policy, so a SELECT on `trustiot.telemetry` returns 0 rows for it
 * (and must never read tenant rows anyway); the parts' metadata needs no row access.
 *
 * `system.parts.max_time` is not the partition value (the month): it is the upper bound of the
 * minmax index that MergeTree keeps for the DateTime column inside the partition key expression,
 * to the second. For `PARTITION BY toYYYYMM(received_at)` (services/ingest/migrations/clickhouse/
 * 0002_telemetry.sql) that is the newest `received_at` of the active parts, which is what the
 * runbook's reading of 2026-11-06 09:01:26 UTC is (the gateway's last presence change, 09:02:13,
 * follows it). It is the newest received time only while the key is that expression: the first
 * query reads the key (table metadata, no row), and the freshness is judged only when it matches.
 */
export const PARTITION_KEY = "toYYYYMM(received_at)";

/** The selects of the runbook's ClickHouse heredoc plus the partition key, as aggregates in TSV (one line each). */
export const CLICKHOUSE_QUERIES = [
  "SELECT partition_key FROM system.tables WHERE database = 'trustiot' AND name = 'telemetry' FORMAT TabSeparated;",
  "SELECT toString(max(max_time), 'UTC'), dateDiff('second', max(max_time), now('UTC')), sum(rows)",
  "FROM system.parts WHERE database = 'trustiot' AND table = 'telemetry' AND active FORMAT TabSeparated;",
  "SELECT argMax(LogAttributes['state'], Timestamp), toString(max(Timestamp), 'UTC'), dateDiff('second', max(Timestamp), now('UTC'))",
  "FROM trustiot_logs.otel_logs WHERE ServiceName = 'gateway-ops' AND Body = 'gateway presence changed' FORMAT TabSeparated;",
  "",
].join("\n");

/**
 * The client, run in the ClickHouse container by `sh -c`: the account and the password file are
 * the container's own variables (`$CLICKHOUSE_USER`, `$CLICKHOUSE_PASSWORD_FILE`), the password
 * goes to the client as its CLICKHOUSE_PASSWORD variable (not a command-line argument), and
 * `--readonly 1` refuses anything but reads.
 */
export const CLICKHOUSE_SCRIPT = 'CLICKHOUSE_PASSWORD="$(cat "$CLICKHOUSE_PASSWORD_FILE")" clickhouse-client --user "$CLICKHOUSE_USER" --readonly 1 --multiquery';
export const CLICKHOUSE_POD = "trustiot-clickhouse-0";

export interface DataFacts {
  /** The telemetry table's partition key: what max_time of its parts is the maximum of. */
  partitionKey: string;
  newestUtc: string;
  secondsAgo: number;
  rows: number;
  /** "" when the log has no presence change. */
  state: string;
  sinceUtc: string;
  sinceSecondsAgo: number;
}

const EPOCH = /^1970-01-01/;

/** Parses the three TSV lines of CLICKHOUSE_QUERIES: the partition key, then 3 columns twice. */
export function parseDataFacts(out: string): DataFacts {
  const lines = out.split("\n").filter((l) => l.trim() !== "");
  const [key, first, second] = lines.map((l) => l.split("\t"));
  if (lines.length !== 3 || key?.length !== 1 || first?.length !== 3 || second?.length !== 3) {
    throw new ReleaseError(`ClickHouse printed ${lines.length} line(s), not the 3 lines (1, 3 and 3 columns) that were asked for`);
  }
  const number = (text: string | undefined, what: string) => {
    if (text === undefined || !/^-?\d+$/.test(text)) throw new ReleaseError(`ClickHouse's ${what} is not a number`);
    return Number(text);
  };
  return {
    partitionKey: key[0]!,
    newestUtc: first[0]!,
    secondsAgo: number(first[1], "seconds ago"),
    rows: number(first[2], "row count"),
    state: second[0]!,
    sinceUtc: second[1]!.replace(/\.\d+$/, ""),
    sinceSecondsAgo: number(second[2], "presence age"),
  };
}

/** Data freshness: FAIL above FRESH_LIMIT_S or without rows; WARN when the newest row is in the future. */
export function judgeFreshness(facts: DataFacts): Result {
  if (facts.partitionKey !== PARTITION_KEY) {
    return {
      verdict: "FAIL",
      name: NAMES.data,
      evidence: [`the telemetry table's partition key is ${JSON.stringify(redact(facts.partitionKey).slice(0, 60))}, not ${PARTITION_KEY}, so the parts' newest time is not the newest received time and the freshness cannot be judged`],
    };
  }
  if (facts.rows === 0 || EPOCH.test(facts.newestUtc)) {
    return { verdict: "FAIL", name: NAMES.data, evidence: ["the telemetry table has no rows"] };
  }
  const shown = `newest received ${facts.newestUtc} UTC, ${facts.secondsAgo} s ago (limit ${FRESH_LIMIT_S} s), ${facts.rows} rows`;
  if (facts.secondsAgo > FRESH_LIMIT_S) return { verdict: "FAIL", name: NAMES.data, evidence: [`${shown}: the data is stale`] };
  if (facts.secondsAgo < 0) return { verdict: "WARN", name: NAMES.data, evidence: [`${shown}: the newest time is in the future, so the clocks differ`] };
  return { verdict: "PASS", name: NAMES.data, evidence: [shown] };
}

/** Gateway presence: PASS only for `online`. */
export function judgePresence(facts: DataFacts): Result {
  if (facts.state === "" || EPOCH.test(facts.sinceUtc)) {
    return { verdict: "FAIL", name: NAMES.presence, evidence: ["gateway-ops has logged no presence change"] };
  }
  const shown = `last presence state ${JSON.stringify(redact(facts.state))} since ${facts.sinceUtc} UTC (${facts.sinceSecondsAgo} s ago)`;
  return facts.state === "online" ? { verdict: "PASS", name: NAMES.presence, evidence: [shown] } : { verdict: "FAIL", name: NAMES.presence, evidence: [`${shown}: not online`] };
}

export async function readDataFacts(box: Box): Promise<DataFacts> {
  const result = await box.tryKubectl(["-n", DEMO_NAMESPACE, "exec", "-i", CLICKHOUSE_POD, "-c", "clickhouse", "--", "sh", "-c", CLICKHOUSE_SCRIPT], CLICKHOUSE_QUERIES);
  if (result.exitCode !== 0) {
    throw new ReleaseError(`reading ClickHouse in ${CLICKHOUSE_POD} failed (exit ${result.exitCode}): ${lastLines(result.stderr || result.stdout, 3).replace(/\s+/g, " ")}`);
  }
  return parseDataFacts(result.stdout);
}

// --- The gateway -------------------------------------------------------------------------------

/** The runbook's command, read only. */
export const GATEWAY_COMMAND = "systemctl is-active trustiot-agent; uptime -p; test -e /run/systemd/timesync/synchronized && echo ntp-synchronized || echo ntp-NOT-synchronized";

export function judgeGateway(out: string): Result {
  const lines = out.split("\n").map((l) => l.trim()).filter((l) => l !== "");
  if (lines.length !== 3) return { verdict: "FAIL", name: NAMES.gateway, evidence: [`the gateway printed ${lines.length} line(s), not the 3 that were asked for`] };
  const [agent, uptime, ntp] = lines as [string, string, string];
  const failures: string[] = [];
  if (agent !== "active") failures.push(`trustiot-agent is ${redact(agent).slice(0, 40)}, not active`);
  if (ntp !== "ntp-synchronized") failures.push("the clock is not NTP-synchronized (readings stored now carry a wrong time for good: do not demo before it is)");
  return judged(NAMES.gateway, [`trustiot-agent active, ${redact(uptime).slice(0, 60)}, ${ntp}`], [], failures);
}

export async function checkGateway(box: Box): Promise<Result> {
  const out = await box.mustSh(GATEWAY_COMMAND, "reading the gateway's agent, uptime and NTP state");
  return judgeGateway(out);
}

// --- The run -----------------------------------------------------------------------------------

export interface PreflightInputs {
  demo: Box;
  /** Undefined without --gateway-ssh-host. */
  gateway?: Box;
  ops: FluxReader;
  repo: () => Promise<RepoInputs>;
}

/**
 * What flux.ts status reads from the checkout (main's chart pin), with every local command bounded
 * by REPO_COMMAND_TIMEOUT_MS: a command that runs out is killed with its children and fails with
 * "the command timed out after N s" (a fetch that times out shows as a checkout problem, a
 * kustomize that does as the failure of `kubectl kustomize`).
 */
export function boundedCheckoutInputs(root?: string, timeoutMs: number = REPO_COMMAND_TIMEOUT_MS): Inputs {
  const bounded: typeof tryRun = (cmd, options) => runBounded(cmd, options?.stdin, timeoutMs);
  const at = root ?? ROOT;
  return { repo: async () => readRepo(at, await readCode(at, bounded), (r) => kustomizeFluxSystem(r, bounded)) };
}

/** Runs every item and returns the results in the runbook's order; never throws for an item. */
export async function runPreflight(inputs: PreflightInputs): Promise<Result[]> {
  const results: Result[] = [];
  const down = await unreachable(inputs.demo);
  if (down !== undefined) {
    const why = `the demo machine is unreachable (${down})`;
    results.push(
      { verdict: "FAIL", name: NAMES.flux, evidence: [why] },
      { verdict: "FAIL", name: NAMES.memory, evidence: [why] },
      { verdict: "WARN", name: NAMES.dashboard, evidence: [`${why}, so it is not known (informational)`] },
      { verdict: "FAIL", name: NAMES.data, evidence: [why] },
      { verdict: "FAIL", name: NAMES.presence, evidence: [why] },
    );
  } else {
    let podsRead: Promise<string[]> | undefined;
    const pods = () => (podsRead ??= inputs.ops.pods(DEMO_NAMESPACE));
    results.push(
      ...(await guarded(NAMES.flux, async () => [
        await checkFlux({ ops: inputs.ops, pods, repo: inputs.repo }),
      ])),
      ...(await guarded(NAMES.memory, async () => [await checkMemory(inputs.demo)])),
      ...(await guarded(
        NAMES.dashboard,
        async () => [dashboardCheckResult((await pods()).map(parsePod))],
        "WARN",
      )),
    );
    try {
      const data = await readDataFacts(inputs.demo);
      results.push(judgeFreshness(data), judgePresence(data));
    } catch (error) {
      const why = `could not read it: ${errorMessage(error)}`;
      results.push({ verdict: "FAIL", name: NAMES.data, evidence: [why] }, { verdict: "FAIL", name: NAMES.presence, evidence: [why] });
    }
  }
  if (inputs.gateway === undefined) {
    results.push({ verdict: "SKIP", name: NAMES.gateway, evidence: ["skipped: no --gateway-ssh-host (or GATEWAY_SSH_HOST) given"] });
  } else {
    const gone = await unreachable(inputs.gateway);
    results.push(
      ...(gone === undefined
        ? await guarded(NAMES.gateway, async () => [await checkGateway(inputs.gateway!)])
        : [{ verdict: "FAIL" as const, name: NAMES.gateway, evidence: [`the gateway is unreachable (${gone}); it may be powered off`] }]),
    );
  }
  return results;
}

// --- Command line ------------------------------------------------------------------------------

const SPEC = { "ssh-host": { default: "" }, "gateway-ssh-host": { default: "" }, "log-dir": { default: "" } } as const;

let log: FluxLog | undefined;

if (import.meta.main) {
  await main(
    () => log,
    async () => {
      const raw = parseOptions(Bun.argv.slice(2), SPEC);
      const options = commonOptions({ apply: "false", ...raw });
      const gatewayHost = raw["gateway-ssh-host"] || process.env.GATEWAY_SSH_HOST || "";
      process.on("exit", killRunning);
      for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) {
        process.on(signal, () => {
          killRunning();
          process.exit(130);
        });
      }
      log = new FluxLog("demo-preflight", options.logDir);
      const current = log;
      const demo = new Box(options.sshHost, current, { run: hidingSshHost(boundedSsh(), options.sshHost), sleep: (ms) => Bun.sleep(Math.min(ms, 2_000)) });
      const gateway = gatewayHost === "" ? undefined : new Box(gatewayHost, current, { run: hidingSshHost(boundedSsh(), gatewayHost), sleep: (ms) => Bun.sleep(Math.min(ms, 2_000)) });
      const ops = boxOps(demo, options, async () => {
        throw new ReleaseError("preflight does not use age");
      });
      const inputs = boundedCheckoutInputs();
      current.note("demo pre-flight checks (read only: no lab lock, nothing changed)");
      const results = await runPreflight({ demo, ...(gateway === undefined ? {} : { gateway }), ops, repo: () => cleanErrors(current, inputs.repo) });
      for (const result of results) for (const line of resultLines(result)) current.note(line);
      const { line, exitCode } = summary(results);
      current.note(line);
      // Exit explicitly: a timed-out step may leave a local child (git, ssh) that would keep the process alive.
      process.exit(exitCode);
    },
  );
}
