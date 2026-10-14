// Runs the ingress e2e test (deploy/chart/tests/e2e, Laysi/trustiot-v2#62) against the nightly
// cluster for one deployment shape. The test installs the chart itself, with test doubles, into a
// namespace that already exists, and removes only what it installed.
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { EVIDENCE_FILE_PATTERN } from "../../deploy/chart/tests/e2e/install.ts";
import { exec, type RunLog } from "./exec.ts";
import { e2eNamespaceOf, IDENTITY_DOUBLE_IMAGE, providerNameOf } from "./platform.ts";

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

/** Where the evidence of a failed install of one shape is kept for the shape's diagnostics. */
export function installEvidencePath(outDir: string, shape: string): string {
  return join(outDir, `e2e-install-failure-${shape}.txt`);
}

/** The diagnostics section for the evidence of a failed install, if the shape has any. */
export function installEvidenceSection(outDir: string, shape: string): string | undefined {
  const path = installEvidencePath(outDir, shape);
  if (!existsSync(path)) return undefined;
  return `### ingress e2e: the release did not install (events and workload status of its namespace)\n${readFileSync(path, "utf8").trim()}\n`;
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

/** Creates the namespace, runs the test as `shape`, and always deletes the namespace. */
export async function runIngressE2e(ctx: E2eContext, shape: string): Promise<string> {
  const namespace = e2eNamespaceOf(shape);
  const kube = { env: { KUBECONFIG: ctx.kubeconfig } };
  const kubectl = (args: string[], timeoutMs = 120_000) => exec([ctx.kubectl, ...args], { ...kube, timeoutMs });
  const reportDir = join(ctx.outDir, "e2e");
  mkdirSync(reportDir, { recursive: true });
  ctx.log.line(`$ ingress e2e as ${shape} in namespace ${namespace}`);
  try {
    const created = await kubectl(["create", "namespace", namespace]);
    if (created.code !== 0) throw new Error(`cannot create namespace ${namespace}: ${created.stderr.trim()}`);
    const labels = ["enforce", "warn", "audit"].map((mode) => `pod-security.kubernetes.io/${mode}=restricted`);
    const labelled = await kubectl(["label", "namespace", namespace, ...labels, "pod-security.kubernetes.io/enforce-version=latest"]);
    // Without the labels the test would pass on a namespace that does not enforce restricted.
    if (labelled.code !== 0) throw new Error(`cannot label namespace ${namespace}: ${labelled.stderr.trim()}`);
    const result = await exec(
      [process.execPath, "test", "--timeout", "600000", join(ctx.repoRoot, "deploy", "chart", "tests", "e2e", "ingress.e2e.ts")],
      {
        ...kube,
        cwd: ctx.repoRoot,
        timeoutMs: 20 * 60_000,
        env: {
          ...kube.env,
          E2E_NAMESPACE: namespace,
          E2E_SHAPE: shape,
          E2E_AUTHZ_PROVIDER: providerNameOf(shape),
          E2E_IDENTITY_IMAGE: IDENTITY_DOUBLE_IMAGE,
          // The image was loaded into the nodes; there is no registry to pull it from.
          E2E_IMAGE_PULL_POLICY: "Never",
          E2E_REPORT_DIR: reportDir,
        },
      },
    );
    const output = `${result.stdout}\n${result.stderr}`;
    ctx.log.block(output);
    if (result.code !== 0) {
      const evidence = takeInstallEvidence(reportDir, installEvidencePath(ctx.outDir, shape));
      if (evidence !== undefined) {
        ctx.log.line(`the release did not install; its events and workload status follow, and are kept in ${installEvidencePath(ctx.outDir, shape)}`);
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
