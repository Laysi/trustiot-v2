// Tests of how the nightly run carries the evidence of a failed ingress e2e install into the
// report (Laysi/trustiot-v2#399), with no cluster: the install failure is simulated with the e2e
// test's own `runInstall`, which writes the files this code reads.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { collectInstallEvidence } from "../../deploy/chart/tests/e2e/install-evidence.ts";
import { evidenceFile, runInstall } from "../../deploy/chart/tests/e2e/install.ts";
import { installEvidencePath, installEvidenceSection, takeInstallEvidence } from "./e2e.ts";
import { renderMarkdown, type Report } from "./report.ts";

let outDir: string;
let reportDir: string;
beforeEach(() => {
  outDir = mkdtempSync(join(tmpdir(), "nightly-e2e-"));
  reportDir = join(outDir, "e2e");
  mkdirSync(reportDir);
});
afterEach(() => rmSync(outDir, { recursive: true, force: true }));

// A value that stands for an environment value of the failing Pod; made at run time.
const ENV_VALUE = `${crypto.randomUUID()}${crypto.randomUUID()}`;

const CLUSTER: Record<string, unknown> = {
  deployments: {
    items: [
      {
        metadata: { name: "trustiot-identity", generation: 1 },
        spec: { replicas: 1 },
        status: {
          observedGeneration: 1,
          replicas: 1,
          conditions: [{ type: "Available", status: "False", reason: "MinimumReplicasUnavailable", message: "Deployment does not have minimum availability." }],
        },
      },
    ],
  },
  replicasets: { items: [{ metadata: { name: "trustiot-identity-7d9f" }, spec: { replicas: 1 }, status: { replicas: 1 } }] },
  pods: {
    items: [
      {
        metadata: { name: "trustiot-identity-7d9f-abcde" },
        spec: { containers: [{ name: "identity", env: [{ name: "X", value: ENV_VALUE }] }] },
        status: {
          phase: "Pending",
          containerStatuses: [{ name: "identity", ready: false, restartCount: 0, state: { waiting: { reason: "ErrImageNeverPull", message: "Container image is not present with pull policy of Never" } } }],
        },
      },
    ],
  },
  events: {
    items: [{ type: "Warning", reason: "ErrImageNeverPull", involvedObject: { kind: "Pod", name: "trustiot-identity-7d9f-abcde" }, message: "Container image is not present with pull policy of Never", lastTimestamp: "2026-10-14T07:12:29Z" }],
  },
};

/** The install of the ingress e2e that fails the way a release that does not become ready does. */
function failingInstall(): Error | undefined {
  let installed = false;
  try {
    runInstall({
      release: "trustiot",
      namespace: "nightly-e2e-saas",
      reportDir,
      releaseExists: () => installed,
      install: () => {
        installed = true;
        throw new Error("Error: INSTALLATION FAILED: resource Deployment/nightly-e2e-saas/trustiot-identity not ready. status: InProgress, message: Available: 0/1");
      },
      collectEvidence: () =>
        collectInstallEvidence(
          (args) => (args[0] === "get" ? JSON.stringify(CLUSTER[args[1] ?? ""]) : ""),
          () => "REVISION\tSTATUS\n1\tfailed\n",
        ),
      uninstall: () => {
        installed = false;
      },
    });
  } catch (error) {
    return error as Error;
  }
  return undefined;
}

describe("evidence of a failed install in the report", () => {
  test("moves the test's file out of the shared report directory and into the shape's diagnostics", () => {
    expect(failingInstall()).toBeInstanceOf(Error);
    expect(existsSync(evidenceFile(reportDir, "trustiot"))).toBe(true);

    const destination = installEvidencePath(outDir, "saas");
    const taken = takeInstallEvidence(reportDir, destination);

    expect(taken).toContain("### install-failure-trustiot.txt");
    expect(taken).toContain("Deployment/trustiot-identity: desired=1 replicas=1");
    expect(readFileSync(destination, "utf8")).toBe(taken ?? "");
    // The next shape's run starts from an empty directory, so it cannot overwrite or inherit it.
    expect(readdirSync(reportDir)).toEqual([]);

    const section = installEvidenceSection(outDir, "saas");
    expect(section).toContain("the release did not install");
    expect(section).toContain("ErrImageNeverPull");
    expect(installEvidenceSection(outDir, "onprem")).toBeUndefined();
  });

  test("the nightly report shows the events and the workload state of the failed install", () => {
    failingInstall();
    takeInstallEvidence(reportDir, installEvidencePath(outDir, "saas"));
    const report: Report = {
      startedAt: "2026-10-14T07:00:00Z",
      finishedAt: "2026-10-14T07:20:00Z",
      k3sImage: "rancher/k3s:test",
      ok: false,
      shapes: [
        {
          shape: "saas",
          ok: false,
          durationMs: 1000,
          checks: [{ name: "ingress e2e test", ok: false, detail: "the e2e test exited with 1", durationMs: 1000 }],
          diagnostics: `### objects\nnothing\n\n${installEvidenceSection(outDir, "saas")}`,
        },
      ],
    };
    const markdown = renderMarkdown(report);
    expect(markdown).toContain("Deployment/trustiot-identity: desired=1 replicas=1");
    expect(markdown).toContain("Available=False (MinimumReplicasUnavailable");
    expect(markdown).toContain("Warning ErrImageNeverPull Pod/trustiot-identity-7d9f-abcde: Container image is not present with pull policy of Never");
    expect(markdown).toContain("waiting (ErrImageNeverPull: Container image is not present with pull policy of Never)");
    // Public report: the Pod's environment is not in it.
    expect(markdown).not.toContain(ENV_VALUE);
  });

  test("a run without a failed install adds nothing", () => {
    writeFileSync(join(reportDir, "ingress-e2e-saas.json"), "[]");
    expect(takeInstallEvidence(reportDir, installEvidencePath(outDir, "saas"))).toBeUndefined();
    expect(existsSync(installEvidencePath(outDir, "saas"))).toBe(false);
    // The test's own report file stays.
    expect(readdirSync(reportDir)).toEqual(["ingress-e2e-saas.json"]);
    expect(takeInstallEvidence(join(outDir, "no-such-directory"), installEvidencePath(outDir, "saas"))).toBeUndefined();
  });
});
