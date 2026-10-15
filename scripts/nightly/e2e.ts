// Runs the cluster e2e tests of deploy/chart/tests/e2e against the nightly cluster: the ingress
// e2e test (Laysi/trustiot-v2#62) for every deployment shape, and the service-to-service
// authorization e2e test (mesh.e2e.ts, Laysi/trustiot-v2#63) once, with the SaaS shape it is
// written for (Laysi/trustiot-v2#77). Each test installs the chart itself, with test doubles,
// into a namespace that already exists, and removes only what it installed.
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { EVIDENCE_FILE_PATTERN } from "../../deploy/chart/tests/e2e/install.ts";
import { exec, type RunLog } from "./exec.ts";
import { e2eNamespaceOf, IDENTITY_DOUBLE_IMAGE, providerNameOf } from "./platform.ts";
import { MIGRATE_IMAGE_NAME, serviceImageRef } from "./service-image.ts";

export interface E2eContext {
  repoRoot: string;
  outDir: string;
  kubeconfig: string;
  kubectl: string;
  log: RunLog;
}

/** Test results of a `bun test` run: its own summary lines, or the end of the output. */
export function summarizeBunTest(output: string): string {
  const pass = /^\s*(\d+) pass\s*$/m.exec(output)?.[1];
  const fail = /^\s*(\d+) fail\s*$/m.exec(output)?.[1];
  if (pass === undefined) return output.trim().split("\n").slice(-3).join(" ");
  return `${pass} passed, ${fail ?? "0"} failed`;
}

/** The last lines of an output, where a failed test names what went wrong. */
export function lastLines(output: string, count: number): string {
  return output.trim().split("\n").slice(-count).join("\n");
}

export type E2eSuite = "ingress" | "mesh";

export interface E2eSuiteSpec {
  /** The check's name in the report. */
  title: string;
  /** The test file under deploy/chart/tests/e2e. */
  file: string;
  /** The shapes it runs with; undefined means every shape. */
  shapes?: readonly string[];
  namespace(shape: string): string;
  /** The test's own variables (README.md in deploy/chart/tests/e2e). */
  env(shape: string): Record<string, string>;
}

/**
 * The service-to-service authorization e2e test installs values-saas.yaml: the services' policies
 * are the same in both shapes, and it reaches the ingress on the SaaS listener (port 80).
 */
export const MESH_E2E_SHAPE = "saas";

export const E2E_SUITES: Record<E2eSuite, E2eSuiteSpec> = {
  ingress: {
    title: "ingress e2e test (deploy/chart/tests/e2e)",
    file: "ingress.e2e.ts",
    namespace: e2eNamespaceOf,
    env: (shape) => ({ E2E_SHAPE: shape, E2E_AUTHZ_PROVIDER: providerNameOf(shape) }),
  },
  mesh: {
    title: "service-to-service authorization e2e test (deploy/chart/tests/e2e/mesh.e2e.ts)",
    file: "mesh.e2e.ts",
    shapes: [MESH_E2E_SHAPE],
    // It calls the ingress on public paths only, so it needs no ext_authz provider of its own.
    namespace: () => "nightly-e2e-mesh",
    env: () => ({}),
  },
};

/** The suites that run after the smoke checks of `shape`, in order. */
export function suitesOf(shape: string): E2eSuite[] {
  return (Object.keys(E2E_SUITES) as E2eSuite[]).filter((suite) => E2E_SUITES[suite].shapes?.includes(shape) ?? true);
}

/** Where the evidence of a failed install of one suite and shape is kept for the shape's diagnostics. */
export function installEvidencePath(outDir: string, suite: E2eSuite, shape: string): string {
  return join(outDir, suite === "ingress" ? `e2e-install-failure-${shape}.txt` : `e2e-install-failure-${suite}-${shape}.txt`);
}

/** The diagnostics section for the evidence of a failed install, if the suite left any for the shape. */
export function installEvidenceSection(outDir: string, suite: E2eSuite, shape: string): string | undefined {
  const path = installEvidencePath(outDir, suite, shape);
  if (!existsSync(path)) return undefined;
  return `### ${suite} e2e: the release did not install (events and workload status of its namespace)\n${readFileSync(path, "utf8").trim()}\n`;
}

/**
 * Moves the evidence files the test wrote into its report directory (install.ts of the e2e test)
 * to `destination`, so the next shape's run in the same directory cannot overwrite them. Returns
 * their text, or undefined when there was none. The files hold events and workload status only, never Secret data.
 */
export function takeInstallEvidence(reportDir: string, destination: string): string | undefined {
  if (!existsSync(reportDir)) return undefined;
  const files = readdirSync(reportDir)
    .filter((name) => EVIDENCE_FILE_PATTERN.test(name))
    .sort();
  if (files.length === 0) return undefined;
  const text = files.map((name) => `### ${name}\n${readFileSync(join(reportDir, name), "utf8").trimEnd()}\n`).join("\n");
  writeFileSync(destination, text);
  for (const name of files) rmSync(join(reportDir, name));
  return text;
}

/** Creates the suite's namespace, runs its test as `shape`, and always deletes the namespace. */
export async function runClusterE2e(ctx: E2eContext, suite: E2eSuite, shape: string): Promise<string> {
  const spec = E2E_SUITES[suite];
  const namespace = spec.namespace(shape);
  const kube = { env: { KUBECONFIG: ctx.kubeconfig } };
  const kubectl = (args: string[], timeoutMs = 120_000) => exec([ctx.kubectl, ...args], { ...kube, timeoutMs });
  const reportDir = join(ctx.outDir, "e2e");
  mkdirSync(reportDir, { recursive: true });
  ctx.log.line(`$ ${suite} e2e as ${shape} in namespace ${namespace}`);
  try {
    const created = await kubectl(["create", "namespace", namespace]);
    if (created.code !== 0) throw new Error(`cannot create namespace ${namespace}: ${created.stderr.trim()}`);
    const labels = ["enforce", "warn", "audit"].map((mode) => `pod-security.kubernetes.io/${mode}=restricted`);
    const labelled = await kubectl(["label", "namespace", namespace, ...labels, "pod-security.kubernetes.io/enforce-version=latest"]);
    // Without the labels the test would pass on a namespace that does not enforce restricted.
    if (labelled.code !== 0) throw new Error(`cannot label namespace ${namespace}: ${labelled.stderr.trim()}`);
    const result = await exec(
      [process.execPath, "test", "--timeout", "600000", join(ctx.repoRoot, "deploy", "chart", "tests", "e2e", spec.file)],
      {
        ...kube,
        cwd: ctx.repoRoot,
        timeoutMs: 20 * 60_000,
        env: {
          ...kube.env,
          ...spec.env(shape),
          E2E_NAMESPACE: namespace,
          E2E_IDENTITY_IMAGE: IDENTITY_DOUBLE_IMAGE,
          // The identity pod's init container generates the internal identity keys with the
          // migration image the smoke loaded into the nodes (Laysi/trustiot-v2#404).
          E2E_MIGRATE_IMAGE: serviceImageRef(MIGRATE_IMAGE_NAME),
          // The image was loaded into the nodes; there is no registry to pull it from.
          E2E_IMAGE_PULL_POLICY: "Never",
          E2E_REPORT_DIR: reportDir,
        },
      },
    );
    const output = `${result.stdout}\n${result.stderr}`;
    ctx.log.block(output);
    if (result.code !== 0) {
      const evidence = takeInstallEvidence(reportDir, installEvidencePath(ctx.outDir, suite, shape));
      if (evidence !== undefined) {
        ctx.log.line(`the release did not install; its events and workload status follow, and are kept in ${installEvidencePath(ctx.outDir, suite, shape)}`);
        ctx.log.block(evidence);
      }
      throw new Error(`${result.timedOut ? "the e2e test timed out" : `the e2e test exited with ${result.code}`}:\n${lastLines(output, 60)}`);
    }
    return summarizeBunTest(output);
  } finally {
    const deleted = await kubectl(["delete", "namespace", namespace, "--ignore-not-found", "--wait=true", "--timeout=180s"], 200_000);
    if (deleted.code !== 0) ctx.log.line(`deleting namespace ${namespace} failed: ${deleted.stderr.trim()}`);
  }
}
