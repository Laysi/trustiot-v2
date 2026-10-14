// Unit tests of the release install of the cluster e2e tests (Laysi/trustiot-v2#399). They need no
// cluster: a fake `helm install` fails the way helm 4.3.0's watcher did on 2026-10-14. Runs with
// `bun test deploy/chart`.
import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { EVIDENCE_FILE_PATTERN, evidenceFile, type InstallDeps, NAME_IN_USE, runInstall, WAIT_ARGS } from "./install.ts";

const WATCHER_FAILURE =
  "helm install failed (1):\nError: INSTALLATION FAILED: resource Deployment/nightly-e2e-saas/trustiot-identity not ready. status: InProgress, message: Available: 0/1";
const EVIDENCE = "### Deployments\nDeployment/trustiot-identity: desired=1 replicas=1 updated=1 ready=0 available=0\n";

const dirs: string[] = [];
const tempDir = () => {
  const dir = mkdtempSync(join(tmpdir(), "install-test-"));
  dirs.push(dir);
  return dir;
};
afterEach(() => {
  for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

interface Fake {
  deps: InstallDeps;
  calls: string[];
}

/** A cluster that has the release when `exists` says so, and an install that does `install`. */
function fake(options: { exists?: () => boolean; install?: () => void; reportDir?: string; evidence?: () => string; uninstall?: () => void }): Fake {
  const calls: string[] = [];
  const deps: InstallDeps = {
    release: "trustiot",
    namespace: "nightly-e2e-saas",
    reportDir: options.reportDir,
    releaseExists: () => {
      calls.push("exists");
      return options.exists?.() ?? false;
    },
    install: () => {
      calls.push("install");
      options.install?.();
    },
    collectEvidence: () => {
      calls.push("evidence");
      return options.evidence?.() ?? EVIDENCE;
    },
    uninstall: () => {
      calls.push("uninstall");
      options.uninstall?.();
    },
  };
  return { deps, calls };
}

function failure(run: () => void): Error {
  try {
    run();
  } catch (error) {
    return error as Error;
  }
  throw new Error("expected the install to fail");
}

describe("the wait", () => {
  test("is helm's legacy strategy, bounded at 5 minutes, and never a rollback", () => {
    // `--wait` alone is the watcher strategy; the legacy one needs the `=` form of the flag.
    expect([...WAIT_ARGS]).toEqual(["--wait=legacy", "--timeout", "5m"]);
    expect(WAIT_ARGS).not.toContain("--rollback-on-failure");
  });
});

describe("evidenceFile", () => {
  test("stays inside the report directory whatever the release name", () => {
    const dir = join(tmpdir(), "report");
    expect(evidenceFile(dir, "trustiot")).toBe(join(dir, "install-failure-trustiot.txt"));
    for (const release of ["../../etc/x", "a/b", "..", "/abs", "a\\b"]) {
      const file = evidenceFile(dir, release);
      expect(file.startsWith(`${dir}/`)).toBe(true);
      expect(file.slice(dir.length + 1)).not.toContain("/");
      expect(EVIDENCE_FILE_PATTERN.test(file.slice(dir.length + 1))).toBe(true);
    }
  });
});

describe("runInstall", () => {
  test("returns once the install worked, without evidence or cleanup", () => {
    const { deps, calls } = fake({});
    runInstall(deps);
    expect(calls).toEqual(["exists", "install"]);
  });

  test("never installs over an existing release", () => {
    const { deps, calls } = fake({ exists: () => true });
    expect(failure(() => runInstall(deps)).message).toContain("release trustiot already exists in nightly-e2e-saas");
    expect(calls).toEqual(["exists"]);
  });

  test("a failed install saves the evidence in the report directory, then removes its own release", () => {
    const reportDir = join(tempDir(), "report");
    // The release exists once helm has recorded it as failed.
    let installed = false;
    const { deps, calls } = fake({
      reportDir,
      exists: () => installed,
      install: () => {
        installed = true;
        throw new Error(WATCHER_FAILURE);
      },
      uninstall: () => {
        installed = false;
      },
    });
    const error = failure(() => runInstall(deps));
    // The evidence is read while the objects still exist: before the uninstall.
    expect(calls).toEqual(["exists", "install", "evidence", "exists", "uninstall"]);
    expect(error.message).toContain("status: InProgress, message: Available: 0/1");
    expect(error.message).toContain(`evidence of the namespace nightly-e2e-saas saved in ${evidenceFile(reportDir, "trustiot")}`);
    expect(error.message).toContain("release trustiot removed");
    expect(readFileSync(evidenceFile(reportDir, "trustiot"), "utf8")).toBe(EVIDENCE);
    expect(readdirSync(reportDir).every((name) => EVIDENCE_FILE_PATTERN.test(name))).toBe(true);
  });

  test("without a report directory the evidence is in the error", () => {
    let installed = false;
    const { deps } = fake({
      exists: () => installed,
      install: () => {
        installed = true;
        throw new Error(WATCHER_FAILURE);
      },
    });
    const error = failure(() => runInstall(deps));
    expect(error.message).toContain("evidence of the namespace nightly-e2e-saas:\n### Deployments");
  });

  test("a release name that is taken is rethrown and nothing is collected or removed", () => {
    // A concurrent run installed the release between the check and the install: it is not ours.
    const { deps, calls } = fake({
      install: () => {
        throw new Error(`Error: INSTALLATION FAILED: ${NAME_IN_USE}`);
      },
    });
    expect(failure(() => runInstall(deps)).message).toContain(NAME_IN_USE);
    expect(calls).toEqual(["exists", "install"]);
  });

  test("a chart that never rendered has no release to remove", () => {
    const { deps, calls } = fake({
      install: () => {
        throw new Error("Error: INSTALLATION FAILED: template: ... nil pointer");
      },
    });
    const error = failure(() => runInstall(deps));
    expect(calls).toEqual(["exists", "install", "evidence", "exists"]);
    expect(error.message).toContain("release trustiot was not created");
  });

  test("a failed removal is reported next to the install failure, which stays first", () => {
    let installed = false;
    const { deps } = fake({
      exists: () => installed,
      install: () => {
        installed = true;
        throw new Error(WATCHER_FAILURE);
      },
      uninstall: () => {
        throw new Error("helm uninstall failed (1): timed out");
      },
    });
    const error = failure(() => runInstall(deps));
    expect(error.message.startsWith("helm install of trustiot failed:")).toBe(true);
    expect(error.message).toContain("removing release trustiot failed: helm uninstall failed (1): timed out");
  });

  test("a failure of the evidence collection or the file does not hide the install failure", () => {
    const blocked = join(tempDir(), "file");
    writeFileSync(blocked, "x");
    let installed = false;
    const { deps } = fake({
      // A report directory that cannot be created, because a file is in the way.
      reportDir: join(blocked, "report"),
      exists: () => installed,
      install: () => {
        installed = true;
        throw new Error(WATCHER_FAILURE);
      },
      evidence: () => {
        throw new Error("kubectl blew up");
      },
    });
    const error = failure(() => runInstall(deps));
    expect(error.message).toContain("status: InProgress");
    expect(error.message).toContain("evidence could not be saved");
    expect(error.message).toContain("(evidence collection failed: kubectl blew up)");
    expect(existsSync(join(blocked, "report"))).toBe(false);
  });
});

