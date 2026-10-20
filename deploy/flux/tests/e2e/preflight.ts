// The decisions lab.e2e.ts makes before and after it changes the cluster, as code that
// `bun test deploy/flux` covers without a cluster (preflight.test.ts): whether the cluster already
// has Flux, whether the lab lock is this run's, whether the test namespace and flux-system (and
// with it Flux's cluster-wide objects) belong to this run, and what teardown deletes.

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

// --- Owning a namespace ------------------------------------------------------------------------

/** The label that marks a namespace (the test namespace, flux-system) as created by one run of this test. */
export const RUN_LABEL = "e2e.trustiot.io/run";

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
 * Whether this run owns a namespace after trying to create it: when the create succeeded, or when
 * its result is unknown (an error other than AlreadyExists, such as a dropped connection) and the
 * namespace carries this run's label. AlreadyExists means someone else's.
 */
export function ownsNamespace(result: CreateResult): boolean {
  if (result.createExitCode === 0) return true;
  if (/\bAlreadyExists\b|already exists/.test(result.createOutput)) return false;
  return result.labelOnCluster === result.runId;
}

/** What RunNamespace needs of the cluster. */
export interface NamespaceApi {
  /** `kubectl create -f -` with the manifest, exactly one attempt (no retry). */
  create(manifest: string): Promise<{ exitCode: number; output: string }>;
  /** RUN_LABEL of namespace `name`; undefined when the namespace or the label is absent or unreadable. */
  runLabel(name: string): Promise<string | undefined>;
}

/**
 * Why a namespace this run created no longer counts as its own (undefined while it does): its
 * RUN_LABEL must still be this run's ID. `cleanup` says what a person removes by hand, because
 * teardown then leaves everything in it alone.
 */
export function runLabelProblem(name: string, labelOnCluster: string | undefined, runId: string, cleanup: string): string | undefined {
  if (labelOnCluster === runId) return undefined;
  const found = labelOnCluster === undefined ? "the label is gone (or the namespace)" : "the label holds another run's ID";
  return `namespace ${name} no longer carries ${RUN_LABEL}=${runId} (${found}), so teardown will not delete it or anything that depends on it; ${cleanup}`;
}

/**
 * A namespace of one run. `claim` creates it with the run label and decides ownership from the
 * outcome (ownsNamespace); only an owned namespace is used. `mayTearDown` reads the label again
 * before teardown, and `requireStillOurs` right after something else may have applied the
 * namespace (Flux's own manifests): what depends on the namespace is deleted only while the label
 * still holds this run's ID.
 */
export class RunNamespace {
  owned = false;

  constructor(
    readonly name: string,
    readonly runId: string,
  ) {}

  manifest(): string {
    return ["apiVersion: v1", "kind: Namespace", "metadata:", `  name: ${this.name}`, "  labels:", `    ${RUN_LABEL}: ${this.runId}`, ""].join("\n");
  }

  async claim(api: NamespaceApi): Promise<void> {
    let created: { exitCode: number; output: string };
    try {
      created = await api.create(this.manifest());
    } catch (error) {
      created = { exitCode: -1, output: String(error) };
    }
    const labelOnCluster = created.exitCode === 0 ? this.runId : await api.runLabel(this.name).catch(() => undefined);
    this.owned = ownsNamespace({ createExitCode: created.exitCode, createOutput: created.output, labelOnCluster, runId: this.runId });
    if (!this.owned) {
      throw new Error(
        `namespace ${this.name} is not this run's (create exited ${created.exitCode}: ${created.output.trim().split("\n").at(-1) ?? ""}); ` +
          "nothing is installed in it or removed from it",
      );
    }
  }

  /** Stops the run when the namespace lost this run's label; `cleanup` is what a person then removes. */
  async requireStillOurs(api: NamespaceApi, cleanup: string): Promise<void> {
    const problem = runLabelProblem(this.name, await api.runLabel(this.name).catch(() => undefined), this.runId, cleanup);
    if (problem !== undefined) throw new Error(problem);
  }

  async mayTearDown(api: NamespaceApi): Promise<boolean> {
    if (!this.owned) return false;
    return (await api.runLabel(this.name).catch(() => undefined)) === this.runId;
  }
}

// --- Teardown -----------------------------------------------------------------------------------

/** One deletion of teardown: kubectl arguments, and whether Flux's manifests go on its stdin. */
export interface TeardownStep {
  what: string;
  args: string[];
  /** `kubectl delete -f -` reads the Flux manifests that the run applied. */
  fluxManifests?: true;
}

export interface Ownership {
  /** flux-system still carries this run's label (RunNamespace.mayTearDown): Flux is this run's. */
  fluxOurs: boolean;
  /** The test namespace still carries this run's label. */
  testOurs: boolean;
  /** The test namespace. */
  testNamespace: string;
  /** Whether the run rendered Flux's manifests (it got as far as installing Flux). */
  fluxManifests: boolean;
}

/**
 * The deletions of teardown, in the order Flux needs: stop syncing, uninstall the release while
 * helm-controller runs, delete the custom resources while their controllers remove the finalizers,
 * then Flux (its CRDs, ClusterRoles and ClusterRoleBindings) and the namespaces. Only what this run
 * still owns: nothing of Flux, its CRDs or flux-system unless flux-system carries this run's label,
 * and not the test namespace unless it does. lab.e2e.ts runs exactly this list.
 */
export function teardownSteps(own: Ownership): TeardownStep[] {
  const ns = own.testNamespace;
  const steps: TeardownStep[] = [];
  if (own.fluxOurs) {
    steps.push({ what: "suspend the sync, no pruning", args: ["-n", "flux-system", "patch", "kustomization", "flux-system", "--type=merge", "-p", '{"spec":{"suspend":true,"prune":false}}'] });
  }
  if (own.fluxOurs && own.testOurs) {
    steps.push({ what: "uninstall the release", args: ["-n", ns, "delete", "helmrelease", "--all", "--wait", "--timeout=180s"] });
    steps.push({ what: "delete the sources", args: ["-n", ns, "delete", "ocirepository", "--all", "--wait", "--timeout=120s"] });
  }
  if (own.fluxOurs) {
    steps.push({ what: "delete the sync", args: ["-n", "flux-system", "delete", "kustomization,gitrepository", "--all", "--wait", "--timeout=120s"] });
  }
  if (own.fluxOurs && own.fluxManifests) {
    steps.push({ what: "delete Flux", args: ["delete", "--ignore-not-found", "--wait", "--timeout=300s", "-f", "-"], fluxManifests: true });
  }
  if (own.testOurs) {
    steps.push({ what: `delete namespace ${ns}`, args: ["delete", "namespace", ns, "--ignore-not-found", "--wait", "--timeout=300s"] });
  }
  if (own.fluxOurs) {
    steps.push({ what: "delete namespace flux-system", args: ["delete", "namespace", "flux-system", "--ignore-not-found", "--wait", "--timeout=300s"] });
  }
  return steps;
}
