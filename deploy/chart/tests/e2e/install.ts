// The release install of the cluster e2e tests (Laysi/trustiot-v2#399), apart from the settings
// in cluster.ts so that it is tested without a cluster or the environment (install.test.ts).
//
// Waiting. The install waits with helm's `legacy` strategy, not the default `watcher` one. Helm
// 4.3.0 turns on concurrent status computation in the CLI (`cliDefaultStatusComputeWorkers = 8`,
// pkg/cmd/flags.go), where the informer handlers compute each object's status in goroutines and
// send them on a channel; a status computed for an older version of an object can still be sent
// after the status of a newer one. The watcher stops at the first moment every resource is
// Current and reports the last status it collected, so a late `InProgress` after the `Current`
// ends the wait with "not ready. status: InProgress, message: Available: 0/1" within seconds, not
// at the timeout. The legacy waiter polls every 2 s with a plain readiness check of the
// Deployments, with no event stream to reorder. What it does not wait for is what the tests wait
// for themselves: the Gateway (`kubectl wait ... Programmed`) and the gateway's own Deployment
// (`kubectl rollout status`), and no test installs a Job or a database cluster. The full account
// is in docs/architecture/ci-release.md (nightly.yml, ingress e2e).
//
// Failing. There is no `--rollback-on-failure`: a rollback deletes the objects and with them the
// evidence of why they were not ready. A failed install keeps them; this module collects the
// events and the state of the namespace's workloads, then removes the release itself.
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

/** Helm's message when the release name is taken (pkg/action/install.go, `availableName`). */
export const NAME_IN_USE = "cannot reuse a name that is still in use";

/** The helm arguments that wait for the release; see the header for why `legacy`. */
export const WAIT_ARGS = ["--wait=legacy", "--timeout", "5m"] as const;

export interface InstallDeps {
  release: string;
  namespace: string;
  /** Whether the release exists now; throws when that cannot be told. */
  releaseExists(): boolean;
  /** Runs `helm install ... --wait`; throws with helm's message when it fails. */
  install(): void;
  /** The text of `collectInstallEvidence`; never throws. */
  collectEvidence(): string;
  /** Removes the release, waiting for its objects to go; throws when that fails. */
  uninstall(): void;
  /** Directory of the nightly report files (`E2E_REPORT_DIR`); undefined means none. */
  reportDir: string | undefined;
}

/** The file that holds the evidence of a failed install, in the report directory. */
export function evidenceFile(reportDir: string, release: string): string {
  // E2E_RELEASE is checked by helm only after this runs, so it must not choose a path.
  return join(reportDir, `install-failure-${release.replace(/[^A-Za-z0-9._-]/g, "_")}.txt`);
}

/** The file name pattern the nightly harness reads (scripts/nightly/e2e.ts). */
export const EVIDENCE_FILE_PATTERN = /^install-failure-.+\.txt$/;

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Installs the release and waits for it. Never touches an existing installation: it fails before
 * installing when the release exists, and when helm reports the name as taken (a concurrent
 * installation) the failure is rethrown without touching the release. Any other failure is this
 * run's own release: the evidence is saved, the release removed, and the error says where the
 * evidence is. Returns only when the release is installed and waited for, so the caller counts
 * the release as its own (and uninstalls it) only then.
 */
export function runInstall(deps: InstallDeps): void {
  if (deps.releaseExists()) {
    throw new Error(`release ${deps.release} already exists in ${deps.namespace}; use another E2E_RELEASE or namespace`);
  }
  try {
    deps.install();
    return;
  } catch (error) {
    if (message(error).includes(NAME_IN_USE)) throw error;
    throw failedInstall(deps, error);
  }
}

function failedInstall(deps: InstallDeps, cause: unknown): Error {
  const notes: string[] = [];
  let evidence: string;
  try {
    evidence = deps.collectEvidence();
  } catch (error) {
    // The original failure matters more than the evidence of it.
    evidence = `(evidence collection failed: ${message(error)})`;
  }
  if (deps.reportDir === undefined) {
    notes.push(`evidence of the namespace ${deps.namespace}:\n${evidence}`);
  } else {
    try {
      mkdirSync(deps.reportDir, { recursive: true });
      const file = evidenceFile(deps.reportDir, deps.release);
      writeFileSync(file, evidence);
      notes.push(`evidence of the namespace ${deps.namespace} saved in ${file}`);
    } catch (error) {
      notes.push(`evidence could not be saved (${message(error)}):\n${evidence}`);
    }
  }
  // The release exists only when helm got as far as recording it; a chart that does not render
  // leaves nothing to remove.
  try {
    if (deps.releaseExists()) {
      deps.uninstall();
      notes.push(`release ${deps.release} removed`);
    } else {
      notes.push(`release ${deps.release} was not created`);
    }
  } catch (error) {
    notes.push(`removing release ${deps.release} failed: ${message(error)}`);
  }
  return new Error(`helm install of ${deps.release} failed: ${message(cause)}\n${notes.join("\n")}`);
}
