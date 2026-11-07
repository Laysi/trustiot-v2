/**
 * The dashboard check's isolation on the demo cluster (Laysi/trustiot-v2#881): the probes the
 * nightly runs on its k3d cluster (scripts/nightly/dashboard-check.ts, imported, not copied), run
 * over ssh against the demo cluster right after Flux deployed `services.dashboard-check` there.
 * The demo cluster is shared with the lab e2e tests, so its NetworkPolicy controller and Istio
 * ambient must be shown to isolate the service on this cluster too
 * (docs/runbooks/demo-environment.md, 「打開儀表板檢查」).
 *
 *   bun scripts/demo/dashboard-check.ts --ssh-host <host>                          # read only: facts, probes, confirmation code
 *   bun scripts/demo/dashboard-check.ts --ssh-host <host> --apply --confirm <code>
 *
 * Read only without --apply: the HelmRelease's latest history entry, the kubelet's podPidsLimit
 * (the dashboard check's node requirement), the release's Services and Deployments, the pods the
 * probes run from (the dashboard check's, the identity service's as the control, the assistant's)
 * and the addresses they probe, and the probe image. It prints every probe with what it expects,
 * the Deployments it restarts afterwards and a confirmation code.
 *
 * --apply takes the lab lock for this issue, reads everything again and stops when the plan
 * changed, then
 * 1. runs the probes in one ephemeral container per pod (`kubectl debug`, restricted profile,
 *    attached over ssh until it ends), exactly as the nightly does;
 * 2. restarts the three Deployments one at a time (`kubectl rollout restart`, the maintainer's
 *    decision of 2026-11-01): an ephemeral container can be neither removed nor reused, and a
 *    restart replaces the pod that holds it. Each wait ends when the Deployment has rolled out and
 *    the probed pod is gone. This step runs also when a probe failed, for every pod that got a probe
 *    container;
 * 3. prints one PASS or FAIL line per probe and ends with `PASS all <n> probes` (exit 0) or an
 *    error (exit 1).
 *
 * The probe image is the release's own Postgres image (deploy/images/postgres, the image the
 * nightly probes from): it has bash with /dev/tcp and /dev/udp, getent, timeout, head, dd and
 * base64, runs as a numeric non-root user (the restricted profile needs one), and is on the node
 * because the Postgres pod runs there. `--image-pull-policy=Never` keeps the node from pulling
 * anything.
 *
 * What it prints and logs is good to paste into a test report: the ssh host and node names are
 * masked, and everything passes through flux.ts's redact() (no LAN address, host name or token),
 * except the outside address and name the probes use (1.1.1.1 and example.com, both public). The
 * requests the probes send (the sandbox's `correct` fixture page) and pod JSON are never logged.
 */

import { RunLog, exec, type ExecOptions, type ExecResult } from "../nightly/exec.ts";
import type { K8sObject } from "../nightly/manifests.ts";
import { checkPodPidsLimit, nodeNamesOf } from "../nightly/pod-pids-limit.ts";
import {
  DASHBOARD_CHECK,
  type Expectation,
  type IsolationPods,
  type IsolationTargets,
  OUTSIDE,
  OUTSIDE_NAME,
  type PlannedProbe,
  type Probe,
  type ProbeResult,
  checkIsolation,
  describe,
  isolationPlan,
  isolationSetup,
  probeArguments,
  probeCommand,
  probeInPod,
  probeProblem,
} from "../nightly/dashboard-check.ts";
import { parseOptions } from "../release/exec.ts";
import { ReleaseError, errorMessage } from "../release/lib.ts";
import { redact } from "./flux.ts";
import {
  Box,
  COMMON_SPEC,
  type CommonOptions,
  DEMO_NAMESPACE,
  Log,
  type OpsTunnel,
  ROOT,
  checkConfirmation,
  commonOptions,
  hidingSshHost,
  lastLines,
  main,
  opsTunnel,
  sha256,
  shownPath,
  shq,
  sshRun,
  stableJson,
  takeLock,
  withOpsTunnel,
} from "./lib.ts";

export const ISSUE = "Laysi/trustiot-v2#881";
/** The lab lock's step name (`<issue> <UTC time> demo <step>`). */
export const STEP = "dashboard-check";
/** The release whose latest history entry must be `deployed` (deploy/flux/clusters/demo/trustiot/helm-release.yaml). */
export const HELM_RELEASE = "trustiot";

/** The pods the probes run from, in the order they run (nightly: checkIsolation). */
export const ROLES = ["checker", "control", "assistant"] as const;
export type Role = (typeof ROLES)[number];
/** Each role's `app.kubernetes.io/component`. */
export const COMPONENTS: Record<Role, string> = { checker: DASHBOARD_CHECK, control: "identity", assistant: "assistant" };
/** How the output names each role's pod. */
export const ROLE_NAMES: Record<Role, string> = { checker: "the dashboard check's pod", control: "the identity service's pod", assistant: "the assistant's pod" };

/**
 * The probe image: the release's bundled Postgres image (templates/postgres.yaml), pinned by digest.
 * Nothing else is accepted: another image may lack what the probe script needs or run as root.
 */
export const PROBE_IMAGE_SHAPE = /^ghcr\.io\/laysi\/trustiot-v2\/postgres(?::[A-Za-z0-9_][A-Za-z0-9._-]{0,127})?@sha256:[0-9a-f]{64}$/;
/** The CloudNativePG label on the Postgres instance pods, and their database container. */
export const POSTGRES_POD_LABEL = "cnpg.io/cluster";
export const POSTGRES_CONTAINER = "postgres";

/** Upper bound for one Deployment to roll out after its restart and for the probed pod to be gone. */
export const ROLLOUT_WAIT_MS = 600_000;
const POLL_MS = 5_000;
/**
 * The longest remote command line a probe container may need. sshd hands the whole line to the
 * remote shell as one argument, and Linux refuses an argument longer than 128 KiB (MAX_ARG_STRLEN).
 */
export const REMOTE_COMMAND_LIMIT = 100_000;
/**
 * ssh options for the attached probe containers: no prompt, and keepalives, so the connection to
 * the demo machine is not dropped while the probes wait out their limits with nothing to print.
 */
export const SSH_OPTIONS = ["-o", "BatchMode=yes", "-o", "ServerAliveInterval=30", "-o", "ServerAliveCountMax=6"] as const;
/** The program name probeInPod puts first; the ssh runner turns it into `sudo k3s kubectl` on the demo machine. */
export const KUBECTL = "kubectl";

// --- What may be printed --------------------------------------------------------------------------

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** The outside address and name the probes use, both public: kept as they are. */
const KEPT = new RegExp(String.raw`(?<![\w.-])(?:${escapeRegExp(OUTSIDE.host)}|${escapeRegExp(OUTSIDE_NAME.replace(/\.$/, ""))}\.?)(?![\w-]|\.\d)`, "g");

/**
 * Text as the run prints and logs it: the words in `hidden` (the ssh host, as `<ssh-host>`, and the
 * node names, as `<demo-node>`: neither has to have a dot, so redact() alone would keep them), then
 * flux.ts's redact() on everything but the probes' outside address and name.
 */
export function shownText(text: string, hidden: { sshHost?: string; nodes?: readonly string[] }): string {
  let masked = text;
  const words: [string, string][] = [...(hidden.nodes ?? []).map((node): [string, string] => [node, "<demo-node>"]), ...(hidden.sshHost ? [[hidden.sshHost, "<ssh-host>"] as [string, string]] : [])];
  for (const [word, mask] of words.filter(([word]) => word !== "")) {
    masked = masked.replace(new RegExp(String.raw`(?<![A-Za-z0-9_-])${escapeRegExp(word)}(?![A-Za-z0-9_-])`, "gi"), mask);
  }
  let out = "";
  let last = 0;
  for (const match of masked.matchAll(KEPT)) {
    out += redact(masked.slice(last, match.index)) + match[0];
    last = match.index + match[0].length;
  }
  return out + redact(masked.slice(last));
}

// --- Facts and plan -------------------------------------------------------------------------------

/** A Deployment's rollout, as `kubectl rollout status` judges it. */
export interface RolloutState {
  generation: number;
  observedGeneration: number;
  /** spec.replicas (1 when unset). */
  desired: number;
  updated: number;
  /** status.replicas: the pods of every ReplicaSet of the Deployment that are not terminating. */
  total: number;
  available: number;
}

export function rolloutStateOf(deployment: unknown): RolloutState {
  const object = (deployment ?? {}) as { metadata?: { generation?: number }; spec?: { replicas?: number }; status?: Record<string, number | undefined> };
  const status = object.status ?? {};
  return {
    generation: object.metadata?.generation ?? 0,
    observedGeneration: status.observedGeneration ?? 0,
    desired: object.spec?.replicas ?? 1,
    updated: status.updatedReplicas ?? 0,
    total: status.replicas ?? 0,
    available: status.availableReplicas ?? 0,
  };
}

/** Whether every pod runs the newest template and is available, and no older pod is left. */
export function rolledOut(state: RolloutState): boolean {
  return state.observedGeneration >= state.generation && state.updated >= state.desired && state.total === state.updated && state.available >= state.updated;
}

export function describeRollout(state: RolloutState): string {
  return `generation ${state.observedGeneration}/${state.generation}, ${state.updated} updated, ${state.available} available, ${state.total} in all, ${state.desired} desired`;
}

export interface Facts {
  /** The HelmRelease's latest history entry. */
  release: { chartVersion: string; status: string } | undefined;
  /** checkPodPidsLimit's lines (node names masked), or why the limit is not the one asked for. */
  podPids: { lines: string[]; problem?: string };
  /**
   * The pods and addresses the probes need, or why they cannot be found: before the merge the
   * release has no dashboard check, so the dry run shows this as a blocker next to the other facts.
   */
  setup: Setup | { problem: string };
  /** The running Postgres instance pods: the image of their `postgres` container and their node. */
  postgres: { image: string; node: string }[];
  /** The node names, for masking. */
  nodes: string[];
}

/** The pods the probes run from, the addresses they probe, and per role its Deployment, rollout and node. */
export interface Setup {
  pods: IsolationPods;
  targets: IsolationTargets;
  roles: Record<Role, { deployment: string | undefined; rollout: RolloutState | undefined; node: string }>;
}

export interface Plan {
  ok: string[];
  blockers: string[];
  pods: IsolationPods | undefined;
  image: string | undefined;
  probes: PlannedProbe[];
  /** The Deployments restarted after the probes, in the order of ROLES. */
  restarts: Partial<Record<Role, string>>;
}

/** What the confirmation code covers: the pods, the image, every probe with its target, and the restarts. */
export function changeOf(plan: Plan): unknown {
  return {
    pods: plan.pods ?? null,
    image: plan.image ?? null,
    probes: plan.probes.map((entry) => ({ from: entry.from, label: entry.label, expect: entry.expect, probe: sha256(probeArguments([entry.probe]).join("\0")) })),
    restarts: plan.restarts,
  };
}

/** What a probe expects, for the plan. */
export function expectationText(expect: Expectation): string {
  if (!expect.open) {
    if (expect.tlsRefusal === true) return "no answer, or only a TLS alert";
    if (expect.dnsRefusal === true) return "no answer, or a DNS error without a record";
    return "no answer";
  }
  if ("resolved" in expect) return "the name resolves";
  if ("dnsAnswer" in expect) return "a DNS answer with a record";
  if (expect.status === undefined) return "an answer";
  return `HTTP ${expect.status}${expect.verdict === undefined ? "" : ` with the verdict ${expect.verdict}`}`;
}

/** The remote command line that runs a probe container (`sudo k3s kubectl debug ...`), quoted for the demo machine's shell. */
export function remoteCommand(cmd: readonly string[]): string {
  if (cmd[0] !== KUBECTL) throw new ReleaseError(`only kubectl runs on the demo machine, not ${JSON.stringify(cmd[0])}`);
  return ["sudo", "k3s", "kubectl", ...cmd.slice(1)].map(shq).join(" ");
}

/** The probe containers' names: new in each pod for every run (an ephemeral container cannot be reused). */
export function containerName(role: Role, stamp: string): string {
  return `tq-probe-881-${role}-${stamp}`;
}

export function planProbe(facts: Facts): Plan {
  const ok: string[] = [];
  const blockers: string[] = [];
  if (facts.release === undefined) blockers.push(`HelmRelease ${DEMO_NAMESPACE}/${HELM_RELEASE} has no history yet`);
  else {
    const line = `HelmRelease ${DEMO_NAMESPACE}/${HELM_RELEASE}: latest ${facts.release.chartVersion} (${facts.release.status})`;
    if (facts.release.status === "deployed") ok.push(line);
    else blockers.push(`${line}: wait until Flux has deployed the release`);
  }
  for (const line of facts.podPids.lines) ok.push(`kubelet: ${line}`);
  if (facts.podPids.problem !== undefined) blockers.push(`the kubelet's podPidsLimit is not the dashboard check's node requirement: ${facts.podPids.problem} (docs/runbooks/demo-environment.md, 「展示機 k3s 的行程數上限」)`);

  const images = [...new Set(facts.postgres.map((pod) => pod.image))];
  let image: string | undefined;
  if (images.length !== 1) {
    blockers.push(`the probe image is the release's Postgres image, but ${images.length === 0 ? "no Postgres instance pod runs" : `the Postgres pods run ${images.length} images (${images.join(", ")})`}`);
  } else if (!PROBE_IMAGE_SHAPE.test(images[0]!)) {
    blockers.push(`the Postgres pods run ${images[0]}, not deploy/images/postgres pinned by digest: the probe script needs that image`);
  } else {
    image = images[0]!;
    ok.push(`probe image (the release's Postgres image, already on the node): ${image}`);
  }

  if ("problem" in facts.setup) {
    blockers.push(`the dashboard check cannot be probed: ${facts.setup.problem}`);
    return { ok, blockers, pods: undefined, image, probes: [], restarts: {} };
  }
  const { pods, targets, roles } = facts.setup;
  const restarts: Partial<Record<Role, string>> = {};
  for (const role of ROLES) {
    const { deployment, rollout, node } = roles[role];
    if (deployment !== undefined) restarts[role] = deployment;
    ok.push(`${ROLE_NAMES[role]}: ${pods[role]}`);
    if (deployment === undefined) blockers.push(`the release has no single Deployment of ${COMPONENTS[role]}`);
    else if (rollout === undefined || !rolledOut(rollout)) {
      blockers.push(`deployment/${deployment} is still rolling out (${rollout === undefined ? "no status" : describeRollout(rollout)}): run again when it has finished`);
    }
    if (image !== undefined && !facts.postgres.some((pod) => pod.image === image && pod.node === node)) {
      blockers.push(`${ROLE_NAMES[role]} ${pods[role]} runs on another node than the Postgres pod: the probe image may not be there, and the probe never pulls one`);
    }
  }

  const probes = isolationPlan(targets);
  if (image !== undefined) {
    for (const role of ROLES) {
      const own = probes.filter((entry) => entry.from === role).map((entry) => entry.probe);
      const length = Buffer.byteLength(remoteCommand(probeCommand(KUBECTL, DEMO_NAMESPACE, pods[role], containerName(role, "0000000"), own, image)));
      if (length > REMOTE_COMMAND_LIMIT) blockers.push(`the probe container in ${ROLE_NAMES[role]} needs a ${length}-byte command line, more than ${REMOTE_COMMAND_LIMIT}`);
    }
  }
  return { ok, blockers, pods, image, probes, restarts };
}

// --- Operations -----------------------------------------------------------------------------------

export interface ProbeOps {
  tunnel(): Promise<OpsTunnel>;
  facts(): Promise<Facts>;
  lock(): Promise<() => Promise<void>>;
  /** Runs `probes` in a new ephemeral container `name` of `pod` from `image` (probeInPod). */
  probe(pod: string, name: string, image: string, probes: Probe[]): Promise<Map<string, ProbeResult>>;
  restart(deployment: string): Promise<void>;
  rollout(deployment: string): Promise<RolloutState | undefined>;
  podExists(pod: string): Promise<boolean>;
  now(): number;
  sleep(ms: number): Promise<void>;
}

/** Prints through `shown` (shownText with this run's hidden words). */
type Shown = (text: string) => string;

function report(log: Log, shown: Shown, plan: Plan): void {
  for (const line of plan.ok) log.note(shown(`ok: ${line}`));
  for (const line of plan.blockers) log.note(shown(`BLOCKED (--apply refuses until it is gone): ${line}`));
  for (const entry of plan.probes) log.note(shown(`probe: from ${ROLE_NAMES[entry.from]}, ${entry.label}: expects ${expectationText(entry.expect)}`));
  const restarts = ROLES.flatMap((role) => (plan.restarts[role] === undefined ? [] : [`deployment/${plan.restarts[role]}`]));
  if (restarts.length > 0) log.note(shown(`then: rollout restart ${restarts.join(", ")}, one at a time, each until it has rolled out and its probed pod is gone (the probe containers go with it)`));
}

/** One line per planned probe: PASS or FAIL, where from, what, and what it saw. */
export function resultLines(plan: readonly PlannedProbe[], results: ReadonlyMap<Role, ReadonlyMap<string, ProbeResult>>): { lines: string[]; failed: number } {
  let failed = 0;
  const lines = plan.map((entry) => {
    const result = results.get(entry.from)?.get(entry.probe.id);
    const problem = probeProblem(entry, result);
    if (problem !== undefined) failed += 1;
    return `${problem === undefined ? "PASS" : "FAIL"} from ${ROLE_NAMES[entry.from]}, ${entry.label}: ${result === undefined ? "not run" : describe(result, entry.probe)} (expects ${expectationText(entry.expect)})`;
  });
  return { lines, failed };
}

async function waitForRollout(ops: ProbeOps, log: Log, shown: Shown, deployment: string, pod: string): Promise<void> {
  const deadline = ops.now() + ROLLOUT_WAIT_MS;
  for (;;) {
    const state = await ops.rollout(deployment);
    const gone = !(await ops.podExists(pod));
    if (state !== undefined && rolledOut(state) && gone) {
      log.note(shown(`deployment/${deployment} rolled out (${describeRollout(state)}); the probed pod ${pod} is gone`));
      return;
    }
    if (ops.now() > deadline) {
      throw new ReleaseError(`timed out after ${ROLLOUT_WAIT_MS / 1000} s waiting for deployment/${deployment} to roll out (${state === undefined ? "not found" : describeRollout(state)}; the probed pod ${pod} ${gone ? "is gone" : "still exists"})`);
    }
    await ops.sleep(POLL_MS);
  }
}

/**
 * Restarts the Deployments of `roles` one at a time and waits for each. Returns the problems; a
 * failed restart does not stop the next one, since each pod holds a probe container of its own.
 */
async function restartProbed(ops: ProbeOps, log: Log, shown: Shown, plan: Plan, pods: IsolationPods, roles: readonly Role[]): Promise<string[]> {
  const problems: string[] = [];
  for (const role of roles) {
    const deployment = plan.restarts[role];
    if (deployment === undefined) continue;
    try {
      log.note(shown(`restarting deployment/${deployment} to remove the probe container from ${pods[role]}`));
      await ops.restart(deployment);
      await waitForRollout(ops, log, shown, deployment, pods[role]);
    } catch (error) {
      problems.push(`deployment/${deployment}: ${errorMessage(error)}`);
      log.note(shown(`WARNING: deployment/${deployment}: ${errorMessage(error)}; restart it by hand: sudo k3s kubectl -n ${DEMO_NAMESPACE} rollout restart deployment/${deployment}`));
    }
  }
  return problems;
}

export async function runProbe(ops: ProbeOps, log: Log, options: CommonOptions, hidden: { sshHost?: string }, stamp: string): Promise<Plan> {
  let nodes: string[] = [];
  const shown: Shown = (text) => shownText(text, { ...hidden, nodes });
  try {
    return await withOpsTunnel(() => ops.tunnel(), log, () => probeBody(ops, log, options, stamp, shown, (found) => (nodes = found)));
  } catch (error) {
    // Errors of kubectl or ssh may name the node or the host: shown like everything else.
    throw new ReleaseError(shown(errorMessage(error)));
  }
}

async function probeBody(ops: ProbeOps, log: Log, options: CommonOptions, stamp: string, shown: Shown, setNodes: (nodes: string[]) => void): Promise<Plan> {
  const facts = await ops.facts();
  setNodes(facts.nodes);
  const plan = planProbe(facts);
  report(log, shown, plan);
  if (options.apply && plan.blockers.length > 0) throw new ReleaseError(`${plan.blockers.length} blockers above; nothing changed`);
  if (plan.blockers.length > 0) {
    log.note(`${plan.blockers.length} blockers above; nothing to run until they are settled`);
    return plan;
  }
  if (!checkConfirmation(log, changeOf(plan), options, "bun scripts/demo/dashboard-check.ts --ssh-host <host>")) return plan;

  const release = await ops.lock();
  try {
    const again = await ops.facts();
    setNodes(again.nodes);
    const replanned = planProbe(again);
    if (stableJson(changeOf(replanned)) !== stableJson(changeOf(plan)) || replanned.blockers.length > 0) {
      throw new ReleaseError(`the pods, their addresses or the release changed since the plan was made${replanned.blockers.length > 0 ? ` (${replanned.blockers.join("; ")})` : ""}; nothing changed, run again without --apply`);
    }
    // No blockers, so the plan has its image and pods, and the facts their setup.
    const setup = again.setup;
    if (plan.image === undefined || plan.pods === undefined || "problem" in setup) throw new ReleaseError("the plan has no probe image or no pods");
    const { image, pods } = { image: plan.image, pods: plan.pods };
    const byPod = new Map(ROLES.map((role) => [pods[role], role] as const));
    const attempted: Role[] = [];
    const results = new Map<Role, Map<string, ProbeResult>>();
    let failure: unknown;
    try {
      await checkIsolation(
        {
          probe: async (pod, probes) => {
            const role = byPod.get(pod);
            if (role === undefined) throw new ReleaseError(`no role for pod ${pod}`);
            // Counted before kubectl runs: a failed call may still have added the container.
            attempted.push(role);
            const seen = await ops.probe(pod, containerName(role, stamp), image, probes);
            results.set(role, seen);
            return seen;
          },
          // resultLines prints every probe afterwards, with its label and verdict.
          log: () => {},
        },
        pods,
        setup.targets,
      );
    } catch (error) {
      failure = error;
    }
    const { lines, failed } = resultLines(isolationPlan(setup.targets), results);
    for (const line of lines) log.note(shown(line));
    const problems = await restartProbed(ops, log, shown, plan, pods, attempted);
    if (failure !== undefined || failed > 0 || problems.length > 0) {
      const parts = [
        ...(failure !== undefined || failed > 0 ? [`the isolation check failed (${failed} of ${lines.length} probes): ${errorMessage(failure ?? "see the FAIL lines above")}`] : []),
        ...(problems.length > 0 ? [`restarting the probed Deployments failed: ${problems.join("; ")}`] : []),
      ];
      throw new ReleaseError(parts.join("; "));
    }
    log.note(`PASS all ${lines.length} probes; the probed Deployments rolled out again without the probe containers`);
  } finally {
    await release();
  }
  log.note(`done; records in ${shownPath(log.dir)}`);
  return plan;
}

// --- On the demo machine --------------------------------------------------------------------------

type Exec = (cmd: string[], options: ExecOptions) => Promise<ExecResult>;

/**
 * The exec probeInPod uses, over ssh: `kubectl ...` runs as `sudo k3s kubectl ...` on the demo
 * machine, with the standard input kept open as probeInPod asks (ssh forwards it and ends when the
 * remote kubectl ends). KUBECONFIG is not passed on: k3s's kubectl reads the node's own. ssh's
 * own errors name the host only as <ssh-host>.
 */
export function sshKubectlExec(host: string, run: Exec = exec): Exec {
  return async (cmd, options) => {
    const remote = remoteCommand(cmd);
    const bytes = Buffer.byteLength(remote);
    if (bytes > REMOTE_COMMAND_LIMIT) throw new ReleaseError(`the command line for the demo machine is ${bytes} bytes, more than ${REMOTE_COMMAND_LIMIT}`);
    const result = await run(["ssh", ...SSH_OPTIONS, host, remote], {
      ...(options.stdin === undefined ? {} : { stdin: options.stdin }),
      ...(options.keepStdinOpen === undefined ? {} : { keepStdinOpen: options.keepStdinOpen }),
      ...(options.timeoutMs === undefined ? {} : { timeoutMs: options.timeoutMs }),
    });
    return { ...result, stderr: shownText(result.stderr, { sshHost: host }) };
  };
}

/**
 * The run's Log, with every line it prints or saves passed through shownText: Box, takeLock and
 * withOpsTunnel note through it too, so ssh's and kubectl's own error lines (which may name the
 * host as ssh resolved it, or the node) are masked before they reach the console or the file. The
 * node names join once the facts have been read (hideNodes).
 */
export class ShownLog extends Log {
  private readonly nodes: string[] = [];

  constructor(
    step: string,
    dir: string | undefined,
    private readonly sshHost: string,
  ) {
    super(step, dir);
  }

  hideNodes(nodes: readonly string[]): void {
    for (const node of nodes) if (node !== "" && !this.nodes.includes(node)) this.nodes.push(node);
  }

  shown(text: string): string {
    return shownText(text, { sshHost: this.sshHost, nodes: this.nodes });
  }

  override note(message: string): void {
    super.note(this.shown(message));
  }

  override save(name: string, data: unknown): string {
    const clean = (value: unknown): unknown =>
      typeof value === "string"
        ? this.shown(value)
        : Array.isArray(value)
          ? value.map(clean)
          : value !== null && typeof value === "object"
            ? Object.fromEntries(Object.entries(value).map(([key, item]) => [this.shown(key), clean(item)]))
            : value;
    return super.save(name, clean(data));
  }
}

/** probeInPod's log, which writes into this run's Log through `shown`. */
class ShownRunLog extends RunLog {
  constructor(
    private readonly sink: Log,
    private readonly shown: Shown,
  ) {
    super();
  }

  override line(message: string): void {
    this.sink.note(this.shown(message));
  }

  override block(text: string): void {
    for (const line of text.trimEnd().split("\n").filter((l) => l !== "")) this.sink.note(this.shown(`    ${line}`));
  }
}

/** The facts of the pods list: each role's node, and the running Postgres instance pods. */
export function podFactsOf(podList: unknown, pods: IsolationPods): { nodes: Record<Role, string>; postgres: Facts["postgres"] } {
  const items = ((podList as { items?: unknown[] } | null)?.items ?? []) as {
    metadata?: { name?: string; labels?: Record<string, string>; deletionTimestamp?: string };
    spec?: { nodeName?: string; containers?: { name?: string; image?: string }[] };
    status?: { phase?: string };
  }[];
  const nodeOf = (name: string) => items.find((pod) => pod.metadata?.name === name)?.spec?.nodeName ?? "";
  const nodes = Object.fromEntries(ROLES.map((role) => [role, nodeOf(pods[role])])) as Record<Role, string>;
  const postgres = items
    .filter((pod) => pod.metadata?.labels?.[POSTGRES_POD_LABEL] !== undefined && pod.metadata.deletionTimestamp === undefined && pod.status?.phase === "Running")
    .flatMap((pod) => (pod.spec?.containers ?? []).filter((container) => container.name === POSTGRES_CONTAINER).map((container) => ({ image: container.image ?? "", node: pod.spec?.nodeName ?? "" })));
  return { nodes, postgres };
}

/** The Deployment of each role's component in the release's objects, when there is exactly one. */
export function deploymentsOf(objects: readonly K8sObject[]): Record<Role, K8sObject | undefined> {
  return Object.fromEntries(
    ROLES.map((role) => {
      const found = objects.filter((o) => o.kind === "Deployment" && o.metadata?.labels?.["app.kubernetes.io/component"] === COMPONENTS[role]);
      return [role, found.length === 1 ? found[0] : undefined];
    }),
  ) as Record<Role, K8sObject | undefined>;
}

async function boxFacts(box: Box): Promise<Facts> {
  // Pod, Service and Deployment JSON holds environment values: parsed, never logged.
  const json = async (args: string[]): Promise<unknown> => JSON.parse(await box.kubectl([...args, "--output", "json"])) as unknown;
  const list = (await json(["get", "services,deployments", "--namespace", DEMO_NAMESPACE])) as { items?: K8sObject[] };
  const objects = list.items ?? [];
  const podList = await json(["get", "pods", "--namespace", DEMO_NAMESPACE]);
  let setup: Facts["setup"];
  try {
    const { pods, targets } = await isolationSetup(json, DEMO_NAMESPACE, objects, ROOT);
    const nodesOfPods = podFactsOf(podList, pods).nodes;
    const deployments = deploymentsOf(objects);
    const roles = Object.fromEntries(
      ROLES.map((role) => {
        const deployment = deployments[role];
        return [role, { deployment: deployment?.metadata?.name, rollout: deployment === undefined ? undefined : rolloutStateOf(deployment), node: nodesOfPods[role] }];
      }),
    ) as Setup["roles"];
    setup = { pods, targets, roles };
  } catch (error) {
    // No dashboard check in the release yet, or a pod that is not ready: a blocker, not the end of the dry run.
    setup = { problem: errorMessage(error) };
  }
  const { postgres } = podFactsOf(podList, { checker: "", control: "", assistant: "" });

  const helmRelease = await box.getJson(["get", "helmreleases.helm.toolkit.fluxcd.io", HELM_RELEASE, "--namespace", DEMO_NAMESPACE]);
  const latest = ((helmRelease?.status as { history?: { chartVersion?: string; status?: string }[] } | undefined)?.history ?? [])[0];
  const release = latest === undefined ? undefined : { chartVersion: latest.chartVersion ?? "", status: latest.status ?? "" };

  const nodes = nodeNamesOf(await json(["get", "nodes"]));
  const mask = (text: string) => shownText(text, { nodes });
  let podPids: Facts["podPids"];
  try {
    podPids = { lines: (await checkPodPidsLimit((args) => box.kubectl(args))).map(mask) };
  } catch (error) {
    podPids = { lines: [], problem: mask(errorMessage(error)) };
  }
  return { release, podPids, setup, postgres, nodes };
}

export function boxOps(box: Box, options: CommonOptions, log: ShownLog): ProbeOps {
  const runLog = new ShownRunLog(log, (text) => log.shown(text));
  const execOverSsh = sshKubectlExec(box.host);
  return {
    tunnel: () => opsTunnel(box),
    async facts() {
      const facts = await boxFacts(box);
      log.hideNodes(facts.nodes);
      return facts;
    },
    lock: () => takeLock(box, ISSUE, STEP, options.lockHeld),
    probe: (pod, name, image, probes) => probeInPod({ kubeconfig: "", kubectl: KUBECTL, log: runLog, probeImage: image }, DEMO_NAMESPACE, pod, name, probes, { exec: execOverSsh }),
    async restart(deployment) {
      // Not retried: when ssh drops after the restart went through, a retry would roll the
      // Deployment a second time. The caller reports the failure with the command to run by hand.
      const command = ["sudo", "k3s", "kubectl", "rollout", "restart", `deployment/${deployment}`, "--namespace", DEMO_NAMESPACE].map(shq).join(" ");
      const result = await box.sh(command, { retry: false });
      if (result.exitCode !== 0) {
        const dropped = result.exitCode === 255 ? " (ssh dropped the connection: the restart may have gone through; check the Deployment's restartedAt before restarting it again)" : "";
        throw new ReleaseError(`kubectl rollout restart failed (exit ${result.exitCode})${dropped}: ${lastLines(result.stderr || result.stdout)}`);
      }
    },
    async rollout(deployment) {
      const object = await box.getJson(["get", "deployment", deployment, "--namespace", DEMO_NAMESPACE]);
      return object === undefined ? undefined : rolloutStateOf(object);
    },
    async podExists(pod) {
      return (await box.getJson(["get", "pod", pod, "--namespace", DEMO_NAMESPACE])) !== undefined;
    },
    now: () => Date.now(),
    sleep: (ms) => Bun.sleep(ms),
  };
}

if (import.meta.main) {
  let log: Log | undefined;
  await main(
    () => log,
    async () => {
      const options = commonOptions(parseOptions(Bun.argv.slice(2), COMMON_SPEC));
      const shownLog = new ShownLog(STEP, options.logDir, options.sshHost);
      log = shownLog;
      const box = new Box(options.sshHost, log, { run: hidingSshHost(sshRun, options.sshHost) });
      try {
        await runProbe(boxOps(box, options, shownLog), shownLog, options, { sshHost: options.sshHost }, Math.floor(Date.now() / 1000).toString(36));
      } catch (error) {
        throw new ReleaseError(shownText(errorMessage(error), { sshHost: options.sshHost }));
      }
    },
  );
}
