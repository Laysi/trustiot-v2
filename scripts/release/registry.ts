/**
 * Registry access shared by publish.ts and verify.ts: logins, digests, cosign arguments.
 *
 * In GitHub Actions the registry is GHCR, reached with the job's GITHUB_TOKEN, and cosign signs
 * keyless with the job's OIDC token. `--local-test-key <prefix>` switches both scripts to a
 * plain-HTTP registry on this machine and the cosign key pair `<prefix>.key`/`<prefix>.pub`, so
 * the whole publish and verify flow can be exercised on a workstation (scripts/README.md). It is
 * refused inside GitHub Actions, so a release is always signed keyless, and it refuses a plan
 * for any registry that is not on this machine, so a test never takes a release's tags in GHCR.
 */

import { existsSync } from "node:fs";
import { run, systemTool, tryRun } from "./exec.ts";
import { type ReleasePlan, ReleaseError, requireLoopbackRegistry, requireRegistry, sha256Digest } from "./lib.ts";

export interface RegistryAccess {
  /** Local test: cosign key pair and plain-HTTP registry; undefined for GHCR with keyless signing. */
  localKey?: { privateKey: string; publicKey: string };
}

export function registryAccess(localTestKey: string): RegistryAccess {
  if (localTestKey === "") {
    if (process.env.GITHUB_ACTIONS !== "true") {
      throw new ReleaseError(
        "keyless signing needs the OIDC token of a GitHub Actions job; to test locally pass " +
          "--local-test-key <prefix> with a cosign key pair and a local registry",
      );
    }
    return {};
  }
  if (process.env.GITHUB_ACTIONS === "true") {
    throw new ReleaseError("--local-test-key is for local tests only; releases are signed keyless");
  }
  const privateKey = `${localTestKey}.key`;
  const publicKey = `${localTestKey}.pub`;
  if (!existsSync(privateKey) || !existsSync(publicKey)) {
    throw new ReleaseError(`--local-test-key: ${privateKey} and ${publicKey} must exist`);
  }
  return { localKey: { privateKey, publicKey } };
}

/**
 * Throws unless the plan publishes where this mode may publish: GHCR for a release signed
 * keyless, a registry on this machine for a local test.
 */
export function requirePlanRegistry(access: RegistryAccess, plan: ReleasePlan): void {
  if (access.localKey === undefined) {
    requireRegistry(plan);
  } else {
    requireLoopbackRegistry(plan);
  }
}

/** skopeo flags for the registry's transport security. */
export function skopeoTls(access: RegistryAccess, direction: "src" | "dest" | "inspect"): string[] {
  if (access.localKey === undefined) {
    return [];
  }
  return [direction === "inspect" ? "--tls-verify=false" : `--${direction}-tls-verify=false`];
}

/** cosign flags shared by sign, attest and verify. */
export function cosignRegistryFlags(access: RegistryAccess): string[] {
  return access.localKey === undefined ? [] : ["--allow-http-registry"];
}

/** cosign sign and attest flags: keyless in GitHub Actions, the test key locally. */
export function cosignSigningFlags(access: RegistryAccess): string[] {
  if (access.localKey === undefined) {
    return ["--yes", "--oidc-provider", "github-actions"];
  }
  return [
    "--yes",
    "--key",
    access.localKey.privateKey,
    // A local test must not write to the public transparency log.
    "--use-signing-config=false",
    "--tlog-upload=false",
    ...cosignRegistryFlags(access),
  ];
}

/** Logs skopeo, cosign and helm in to the registry host with GITHUB_TOKEN (stdin only). */
export async function login(access: RegistryAccess, host: string, tools: { skopeo: string; cosign: string; helm: string }): Promise<void> {
  if (access.localKey !== undefined) {
    return;
  }
  const token = process.env.GITHUB_TOKEN;
  const user = process.env.GITHUB_ACTOR;
  if (!token || !user) {
    throw new ReleaseError("GITHUB_TOKEN and GITHUB_ACTOR must be set to log in to the registry");
  }
  await run([tools.skopeo, "login", host, "--username", user, "--password-stdin"], { stdin: token });
  await run([tools.cosign, "login", host, "--username", user, "--password-stdin"], { stdin: token });
  await run([tools.helm, "registry", "login", host, "--username", user, "--password-stdin"], { stdin: token });
}

/** Errors that mean "no such manifest" (or, on GHCR, no package yet that this token can see). */
const ABSENT = /manifest unknown|name unknown|not found|denied|unauthorized/i;

/**
 * Returns the manifest that `ref` (repository:tag or repository@digest) points at and its
 * digest, or undefined when the registry has no such manifest. Any other failure throws.
 */
export async function registryManifest(
  access: RegistryAccess,
  ref: string,
): Promise<{ manifest: unknown; digest: string } | undefined> {
  const result = await tryRun([systemTool("skopeo"), "inspect", "--raw", ...skopeoTls(access, "inspect"), `docker://${ref}`]);
  if (result.exitCode !== 0) {
    if (ABSENT.test(result.stderr)) {
      return undefined;
    }
    throw new ReleaseError(`cannot read ${ref}: ${result.stderr.trim()}`);
  }
  return { manifest: JSON.parse(result.stdout), digest: sha256Digest(result.stdout) };
}

/** The host part of a registry reference, for logins. */
export function registryHost(ref: string): string {
  return ref.split("/")[0] ?? "";
}
