// Shared harness of the cluster e2e tests (ingress.e2e.ts, mesh.e2e.ts): settings from the
// environment, the pinned helm and kubectl, and a release install that never touches an existing
// installation. README.md in this directory documents the variables.
import { existsSync } from "node:fs";
import { isAbsolute, join } from "node:path";

import { collectInstallEvidence } from "./install-evidence.ts";
import { runInstall, WAIT_ARGS } from "./install.ts";

export const CHART_DIR = join(import.meta.dir, "..", "..");
export const REPO_ROOT = join(CHART_DIR, "..", "..");

export function required(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required (see deploy/chart/tests/e2e/README.md)`);
  return value;
}

export const NAMESPACE = required("E2E_NAMESPACE");
export const IDENTITY_IMAGE = required("E2E_IDENTITY_IMAGE");
export const CONTEXT = process.env.E2E_KUBECONTEXT;
export const RELEASE = process.env.E2E_RELEASE ?? "trustiot";
export const PULL_POLICY = process.env.E2E_IMAGE_PULL_POLICY ?? "IfNotPresent";
// Directory for the per-request results (JSON); empty means no file.
export const REPORT_DIR = process.env.E2E_REPORT_DIR;
export const KEEP = process.env.E2E_KEEP === "1";
// Object name prefix of the release (trustiot.fullname in templates/_helpers.tpl).
export const OBJECT_PREFIX = RELEASE.includes("trustiot") ? RELEASE : `${RELEASE}-trustiot`;
export const GATEWAY = `${OBJECT_PREFIX}-ingress`;

function pinnedTool(tool: string): string {
  const result = Bun.spawnSync({ cmd: ["proto", "bin", tool, "--reporter", "text"], cwd: REPO_ROOT });
  const path = result.stdout.toString().trim().split("\n").at(-1)?.trim() ?? "";
  if (result.exitCode !== 0 || !isAbsolute(path) || !existsSync(path)) {
    throw new Error(`cannot find the pinned ${tool}; run \`proto install\``);
  }
  return path;
}
export const HELM = pinnedTool("helm");
export const KUBECTL = pinnedTool("kubectl");

export function run(cmd: string[], allowFailure = false, stdin?: string): string {
  const proc = Bun.spawnSync({
    cmd,
    cwd: CHART_DIR,
    stdin: stdin === undefined ? "ignore" : new TextEncoder().encode(stdin),
    stdout: "pipe",
    stderr: "pipe",
  });
  if (proc.exitCode !== 0 && !allowFailure) {
    throw new Error(`${cmd.slice(0, 3).join(" ")} failed (${proc.exitCode}):\n${proc.stderr.toString()}`);
  }
  return proc.stdout.toString();
}
export const kubectlArgs = (...args: string[]) => [KUBECTL, ...(CONTEXT ? ["--context", CONTEXT] : []), "--namespace", NAMESPACE, ...args];
export const kubectl = (...args: string[]) => run(kubectlArgs(...args));
// For the evidence of a failed install: a request that hangs because the API server does is cut.
const kubectlForEvidence = (args: string[]) => kubectl("--request-timeout=30s", ...args);
export const helm = (...args: string[]) =>
  run([HELM, ...(CONTEXT ? ["--kube-context", CONTEXT] : []), "--namespace", NAMESPACE, ...args]);

export function releaseExists(): boolean {
  const proc = Bun.spawnSync({
    cmd: [HELM, ...(CONTEXT ? ["--kube-context", CONTEXT] : []), "--namespace", NAMESPACE, "status", RELEASE],
    stdout: "ignore",
    stderr: "pipe",
  });
  if (proc.exitCode === 0) return true;
  if (proc.stderr.toString().includes("not found")) return false;
  throw new Error(`helm status ${RELEASE} failed:\n${proc.stderr.toString()}`);
}

export async function waitFor(what: string, check: () => boolean | Promise<boolean>, timeoutMs = 120_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await check()) return;
    await Bun.sleep(500);
  }
  throw new Error(`timed out waiting for ${what}`);
}

// A local port nothing listens on, for a tunnel process (ports.ts explains why a plain
// listen-and-stop is not enough on WSL2).
export { freePort } from "./ports.ts";

// Image values of one service from an image reference (repository[:tag][@sha256:digest]).
export function imageValues(service: string, image: string): string[] {
  const match = /^(?<repository>[^@]+?)(?::(?<tag>[\w][\w.-]*))?(?:@(?<digest>sha256:[a-f0-9]{64}))?$/.exec(image);
  if (!match?.groups?.repository) throw new Error(`not an image reference: ${image}`);
  const { repository, tag, digest } = match.groups;
  return [
    "--set-string",
    `services.${service}.image.repository=${repository}`,
    "--set-string",
    `services.${service}.image.tag=${tag ?? "latest"}`,
    "--set-string",
    `services.${service}.image.digest=${digest ?? ""}`,
    "--set-string",
    `services.${service}.image.pullPolicy=${PULL_POLICY}`,
  ];
}

// Installs the release and waits for it (install.ts has the rules and the reasons). Never touches an
// existing installation, and a failed install keeps its evidence and removes itself, so the caller
// counts the release as its own (and uninstalls it) only once this returns.
export function installRelease(valuesFiles: string[], extra: string[]): void {
  runInstall({
    release: RELEASE,
    namespace: NAMESPACE,
    releaseExists,
    install: () => {
      helm("install", RELEASE, ".", ...valuesFiles.flatMap((f) => ["--values", f]), ...extra, ...WAIT_ARGS);
    },
    collectEvidence: () => collectInstallEvidence(kubectlForEvidence, () => helm("history", RELEASE)),
    uninstall: uninstallRelease,
    reportDir: REPORT_DIR || undefined,
  });
}

export function uninstallRelease(): void {
  helm("uninstall", RELEASE, "--wait", "--timeout", "3m");
}
