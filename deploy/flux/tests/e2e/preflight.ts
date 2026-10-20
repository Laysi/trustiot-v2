// The decisions lab.e2e.ts makes before and after it changes the cluster, as code that
// `bun test deploy/flux` covers without a cluster (preflight.test.ts): whether the cluster already
// has Flux, whether the lab lock is this run's, and whether the namespace flux-system, and with it
// Flux's cluster-wide objects, belongs to this run.

/** What the cluster already has of Flux. */
export interface ExistingFlux {
  /** Whether the namespace flux-system exists. */
  fluxSystemNamespace: boolean;
  /** `kubectl get crd -o name` lines of the Flux toolkit's CRDs. */
  fluxCrds: string[];
}

const REASON =
  "this test installs Flux cluster-wide and removes it, CRDs included, at the end, which would remove the Flux that runs there " +
  "(on the lab: the demo environment's, deploy/flux/README.md 「展示叢集」), so it refuses to run";

/** The Flux toolkit's CRDs among `kubectl get crd -o name` lines. */
export function fluxCrdNames(lines: string): string[] {
  return lines
    .split("\n")
    .map((line) => line.trim())
    .filter((name) => name.endsWith(".toolkit.fluxcd.io"));
}

/** Why the test must not run on a cluster that already has Flux; undefined when it may. */
export function existingFluxProblem(state: ExistingFlux): string | undefined {
  if (state.fluxSystemNamespace) return `namespace flux-system already exists: ${REASON}`;
  if (state.fluxCrds.length > 0) return `Flux CRDs already exist (${state.fluxCrds.join(", ")}): ${REASON}`;
  return undefined;
}

// --- The lab lock -------------------------------------------------------------------------------

/** The issue a test namespace belongs to: `e2e-<N>` gives `Laysi/trustiot-v2#<N>`. */
export function issueOfNamespace(namespace: string): string {
  const number = /^e2e-([1-9][0-9]*)$/.exec(namespace)?.[1];
  if (number === undefined) throw new Error(`E2E_NAMESPACE must be e2e-<issue number>, not ${namespace}`);
  return `Laysi/trustiot-v2#${number}`;
}

/**
 * Why the run may not use the lab (undefined when it may): the lock file `~/e2e.lock/owner` must be
 * one line `<issue> <UTC time>` (deploy/flux/README.md, 「lab 測試」) naming this run's issue.
 * The issue is compared with the space after it, so that #4570 does not pass for #457. `owner` is
 * undefined when the file could not be read (no lock, or ssh failed).
 */
export function lockProblem(owner: string | undefined, issue: string): string | undefined {
  if (owner === undefined) return `the lab lock ~/e2e.lock is not held (or its owner file cannot be read); take it for ${issue} first`;
  const line = owner.endsWith("\n") ? owner.slice(0, -1) : owner;
  if (line.includes("\n") || !line.startsWith(`${issue} `)) {
    return `the lab lock is held by someone else (its owner is not "${issue} <time>"); wait for it to be released`;
  }
  return undefined;
}

// --- Owning flux-system -------------------------------------------------------------------------

/** The label that marks namespace flux-system as created by one run of this test. */
export const RUN_LABEL = "e2e.trustiot.io/flux-run";

/** A new run ID: a label value, unguessable, so that only this run can match it. */
export function newRunId(): string {
  return `run-${crypto.randomUUID().replaceAll("-", "")}`;
}

export interface CreateResult {
  /** The exit code of `kubectl create`, one attempt; -1 when the command could not be run or its answer was lost. */
  createExitCode: number;
  createOutput: string;
  /** RUN_LABEL on the namespace as read afterwards; undefined when the namespace or the label is absent or unreadable. */
  labelOnCluster: string | undefined;
  runId: string;
}

/**
 * Whether this run owns namespace flux-system after trying to create it: when the create
 * succeeded, or when its result is unknown (an error other than AlreadyExists, such as a dropped
 * connection) and the namespace carries this run's label. AlreadyExists means someone else's.
 */
export function ownsFluxNamespace(result: CreateResult): boolean {
  if (result.createExitCode === 0) return true;
  if (/\bAlreadyExists\b|already exists/.test(result.createOutput)) return false;
  return result.labelOnCluster === result.runId;
}

/** What FluxNamespace needs of the cluster. */
export interface NamespaceApi {
  /** `kubectl create -f -` with the manifest, exactly one attempt (no retry). */
  create(manifest: string): Promise<{ exitCode: number; output: string }>;
  /** RUN_LABEL of namespace flux-system; undefined when absent or unreadable. */
  runLabel(): Promise<string | undefined>;
}

/**
 * The namespace flux-system of one run. `claim` creates it with the run label and decides
 * ownership from the outcome (ownsFluxNamespace); only an owned namespace lets the run install
 * Flux. `mayTearDown` reads the label again before teardown: Flux's sync objects, its manifests
 * (CRDs, ClusterRoles, ClusterRoleBindings) and the namespace are deleted only while the label
 * still holds this run's ID.
 */
export class FluxNamespace {
  owned = false;

  constructor(readonly runId: string) {}

  manifest(): string {
    return ["apiVersion: v1", "kind: Namespace", "metadata:", "  name: flux-system", "  labels:", `    ${RUN_LABEL}: ${this.runId}`, ""].join("\n");
  }

  async claim(api: NamespaceApi): Promise<void> {
    let created: { exitCode: number; output: string };
    try {
      created = await api.create(this.manifest());
    } catch (error) {
      created = { exitCode: -1, output: String(error) };
    }
    const labelOnCluster = created.exitCode === 0 ? this.runId : await api.runLabel().catch(() => undefined);
    this.owned = ownsFluxNamespace({ createExitCode: created.exitCode, createOutput: created.output, labelOnCluster, runId: this.runId });
    if (!this.owned) {
      throw new Error(
        `namespace flux-system is not this run's (create exited ${created.exitCode}: ${created.output.trim().split("\n").at(-1) ?? ""}); ` +
          "nothing of Flux is installed or removed",
      );
    }
  }

  async mayTearDown(api: NamespaceApi): Promise<boolean> {
    if (!this.owned) return false;
    return (await api.runLabel().catch(() => undefined)) === this.runId;
  }
}
