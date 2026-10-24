// The first platform administrator in the smoke cluster (Laysi/trustiot-v2#582;
// docs/runbooks/platform-admin.md): the API check makes its administrator the way an operator
// does, through the chart's suspended CronJob <release>-platform-admin-bootstrap, never with SQL.
// It starts a Job from the CronJob, waits for its pod, writes an example.com address to the
// container's stdin with `kubectl attach -i` (the address is in no argument, no object and no log
// line), waits for the Job to end and reads the exit code from the pod and the user ID from what
// the attach printed (the container's log from then on; no `kubectl logs`). The address never
// reaches the run log: the commands carry it on stdin only, and the command prints only the user
// ID; the container's output stays out of the run log but for its fixed messages.
import { exec, run } from "./exec.ts";
import type { RunLog } from "./exec.ts";
import type { K8sObject } from "./manifests.ts";

type YamlMap = Record<string, unknown>;

function isMap(value: unknown): value is YamlMap {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function asMap(value: unknown): YamlMap {
  return isMap(value) ? value : {};
}

function asArray(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

/** The label that marks the CronJob and its pods (deploy/chart/templates/platform-admin-bootstrap.yaml). */
export const TASK_LABEL = "trustiot.io/task";
export const TASK = "platform-admin-bootstrap";
/** The CronJob's one container, which reads the address on stdin. */
export const BOOTSTRAP_CONTAINER = "bootstrap-platform-admin";
/** The command's exit codes (tools/migrate/README.md). */
export const EXIT_CREATED = 0;
export const EXIT_ALREADY_CREATED = 3;
/** The command's log messages (tools/migrate/src/main.rs). */
export const CREATED_MESSAGE = "created the first platform administrator";
export const ALREADY_CREATED_MESSAGE =
  "the first platform administrator was created before; nothing changed (docs/runbooks/platform-admin.md)";

/** How long a run may take: a pod start on a loaded runner, the attach, Postgres. */
export const BOOTSTRAP_TIMEOUT_MS = 300_000;
const POLL_MS = 2_000;

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** The environment the CronJob's container may have: the migration role and its CA, and the wait. */
const ALLOWED_ENV = new Set([
  "TRUSTIOT_POSTGRES_CA_FILE",
  "TRUSTIOT__POSTGRES__USERNAME",
  "TRUSTIOT__POSTGRES__PASSWORD",
  "TRUSTIOT__WAIT__DATABASES",
]);

export interface BootstrapCronJob {
  name: string;
  serviceAccount: string;
}

/**
 * The release's bootstrap CronJob and why it is not what the runbook describes: suspended, the
 * migration Job's ServiceAccount without a token, one container that runs only
 * `bootstrap-platform-admin` with stdin open once, and no setting but the migration role's.
 */
export function bootstrapCronJobOf(objects: K8sObject[]): { cronJob?: BootstrapCronJob; problems: string[] } {
  const found = objects.filter((o) => o.kind === "CronJob" && o.metadata?.labels?.[TASK_LABEL] === TASK);
  if (found.length !== 1) return { problems: [`the release should have one platform administrator bootstrap CronJob, found ${found.length}`] };
  const cronJob = found[0]!;
  const name = cronJob.metadata?.name ?? "";
  const spec = asMap(cronJob.spec);
  const pod = asMap(asMap(asMap(asMap(spec.jobTemplate).spec).template).spec);
  const problems: string[] = [];
  if (spec.suspend !== true) problems.push(`${name}: is not suspended, so it could run on its schedule`);
  const serviceAccount = asString(pod.serviceAccountName) ?? "";
  if (pod.automountServiceAccountToken !== false) problems.push(`${name}: mounts the default token`);
  const containers = asArray(pod.containers).filter(isMap);
  if (containers.length !== 1 || asArray(pod.initContainers).length > 0) problems.push(`${name}: has more than its one container`);
  const container = containers[0] ?? {};
  if (container.name !== BOOTSTRAP_CONTAINER) problems.push(`${name}: its container is not ${BOOTSTRAP_CONTAINER}`);
  if (JSON.stringify(container.args) !== JSON.stringify(["bootstrap-platform-admin"])) problems.push(`${name}: its container does not run only bootstrap-platform-admin`);
  if (container.stdin !== true || container.stdinOnce !== true) problems.push(`${name}: its container does not read stdin once`);
  const extra = asArray(container.env)
    .filter(isMap)
    .map((env) => asString(env.name) ?? "")
    .filter((env) => !ALLOWED_ENV.has(env));
  if (extra.length > 0) problems.push(`${name}: its container has settings beyond the migration role's: ${extra.join(", ")}`);
  return { cronJob: { name, serviceAccount }, problems };
}

/** The exit code of the bootstrap container in a Job's pod list (`kubectl get pods -o json`), once it ended. */
export function bootstrapExitCode(podList: unknown): number | undefined {
  for (const pod of asArray(asMap(podList).items).filter(isMap)) {
    for (const status of asArray(asMap(pod.status).containerStatuses).filter(isMap)) {
      if (status.name !== BOOTSTRAP_CONTAINER) continue;
      const code = asMap(asMap(status.state).terminated).exitCode;
      if (typeof code === "number") return code;
    }
  }
  return undefined;
}

/** The name of a pod of the list whose bootstrap container runs, so `kubectl attach` reaches it. */
export function runningBootstrapPod(podList: unknown): string | undefined {
  for (const pod of asArray(asMap(podList).items).filter(isMap)) {
    for (const status of asArray(asMap(pod.status).containerStatuses).filter(isMap)) {
      if (status.name === BOOTSTRAP_CONTAINER && isMap(asMap(status.state).running)) return asString(asMap(pod.metadata).name);
    }
  }
  return undefined;
}

/** The `user_id` of the command's JSON log line with `message`, or undefined. */
export function userIdOfLog(logs: string, message: string): string | undefined {
  for (const line of logs.split("\n")) {
    let parsed: unknown;
    try {
      parsed = JSON.parse(line);
    } catch {
      continue;
    }
    const fields = asMap(asMap(parsed).fields);
    const record = { ...asMap(parsed), ...fields };
    if (record.message !== message) continue;
    const id = asString(record.user_id);
    if (id !== undefined && UUID.test(id)) return id;
  }
  return undefined;
}

/**
 * The `message` of each JSON log line, and its level: the command's messages are fixed texts, so a
 * failure can say what happened without copying a field value (a database error's SQLSTATE is a
 * field) into the run log, which is a public artifact. Lines that are not JSON are counted only.
 */
export function logMessages(logs: string): string[] {
  const messages: string[] = [];
  let other = 0;
  for (const line of logs.split("\n")) {
    if (line.trim() === "") continue;
    let parsed: unknown;
    try {
      parsed = JSON.parse(line);
    } catch {
      other += 1;
      continue;
    }
    const message = asString(asMap(parsed).message);
    const level = asString(asMap(parsed).level) ?? "?";
    if (message === undefined) other += 1;
    else messages.push(`${level} ${message}`);
  }
  if (other > 0) messages.push(`(${other} other line${other === 1 ? "" : "s"})`);
  return messages;
}

/** A fresh address under example.com for one run (the repository is public: no real mailbox). */
export function exampleAddress(): string {
  return `nightly-admin-${crypto.randomUUID().slice(0, 8)}@example.com`;
}

export interface BootstrapRun {
  job: string;
  exitCode: number;
  /** The user the log line names: the new administrator, or the one made before. */
  userId: string;
}

export interface BootstrapContext {
  kubeconfig: string;
  kubectl: string;
  log: RunLog;
}

/**
 * Runs the bootstrap once from `cronJob` as the runbook says, with `address` on stdin; the Job is
 * deleted afterwards. Throws unless the pod ended with 0 or 3 and logged the user ID.
 */
export async function runBootstrap(ctx: BootstrapContext, namespace: string, cronJob: string, address: string): Promise<BootstrapRun> {
  const env = { KUBECONFIG: ctx.kubeconfig };
  const kubectl = (args: string[], quiet = true) => run(ctx.log, [ctx.kubectl, "--namespace", namespace, ...args], { env, timeoutMs: 60_000, quiet });
  const job = `${TASK}-${crypto.randomUUID().slice(0, 8)}`;
  const pods = async () => JSON.parse((await kubectl(["get", "pods", "--selector", `job-name=${job}`, "--output", "json"])).stdout) as unknown;
  await kubectl(["create", "job", job, `--from=cronjob/${cronJob}`], false);
  const started = Date.now();
  const waitFor = async <T>(what: string, found: (list: unknown) => T | undefined): Promise<T> => {
    for (;;) {
      const value = found(await pods());
      if (value !== undefined) return value;
      if (Date.now() - started > BOOTSTRAP_TIMEOUT_MS) throw new Error(`timed out after ${BOOTSTRAP_TIMEOUT_MS / 1000} s waiting for ${what} of Job ${job}`);
      await Bun.sleep(POLL_MS);
    }
  };
  try {
    const pod = await waitFor("the bootstrap container to run", runningBootstrapPod);
    // The address goes on stdin only. kubectl attach does not pass the container's exit code on,
    // so the pod's status says how it ended.
    const attach = [ctx.kubectl, "--namespace", namespace, "attach", "--quiet", "--stdin", `pod/${pod}`, "--container", BOOTSTRAP_CONTAINER];
    ctx.log.line(`$ ${attach.join(" ")}  (the address on stdin)`);
    const attached = await exec(attach, { env, stdin: `${address}\n`, timeoutMs: BOOTSTRAP_TIMEOUT_MS });
    // Its stdout is the container's log from the attach on: read here, never written to the run
    // log whole (the nightly code asks kubectl for no application's log; diagnostics.test.ts).
    // Only kubectl's own stderr goes to the run log.
    ctx.log.block(attached.stderr);
    if (attached.timedOut) throw new Error(`kubectl attach to ${pod} did not end within ${BOOTSTRAP_TIMEOUT_MS / 1000} s`);
    const exitCode = await waitFor("the bootstrap container to end", bootstrapExitCode);
    const output = attached.stdout;
    if (output.toLowerCase().includes(address.toLowerCase())) throw new Error(`Job ${job} logged the address it was given`);
    const messages = logMessages(output);
    const message = exitCode === EXIT_CREATED ? CREATED_MESSAGE : exitCode === EXIT_ALREADY_CREATED ? ALREADY_CREATED_MESSAGE : undefined;
    if (message === undefined) {
      throw new Error(`Job ${job} exited with ${exitCode}, expected ${EXIT_CREATED} or ${EXIT_ALREADY_CREATED}; it logged ${JSON.stringify(messages)}`);
    }
    const userId = userIdOfLog(output, message);
    if (userId === undefined) {
      throw new Error(`Job ${job} exited with ${exitCode} but did not log ${JSON.stringify(message)} with a user ID; it logged ${JSON.stringify(messages)}`);
    }
    return { job, exitCode, userId };
  } finally {
    await kubectl(["delete", "job", job, "--ignore-not-found", "--cascade=foreground", "--wait=true", "--timeout=120s"]);
  }
}
