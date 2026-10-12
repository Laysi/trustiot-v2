/**
 * Regenerates Flux's own manifests in deploy/flux/clusters/saas/flux-system/ from the Flux CLI
 * pinned in .prototools (deploy/flux/README.md, "升級 Flux"):
 *
 *   bun scripts/flux/components.ts
 *
 * - gotk-components.yaml: `flux install --export` with the controllers the SaaS cluster runs.
 * - kustomization.yaml: pins every controller image by digest. The digest is the one that the
 *   image's keyless cosign signature by the fluxcd release workflow covers (cosign verify), so a
 *   tag moved in the registry cannot change what the cluster runs.
 *
 * `bun test deploy/flux` checks that both files are what this script writes for the pinned
 * Flux version, without network access.
 */

import { writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { protoTool, run, runStage } from "../release/exec.ts";
import { GITHUB_OIDC_ISSUER, ReleaseError, isDigest, isMap } from "../release/lib.ts";
import { FLUX_COMPONENTS, FLUX_SYSTEM_DIR, type PinnedImage, controllerImages, renderFluxSystemKustomization } from "./lib.ts";

const ROOT = resolve(import.meta.dir, "..", "..");

/**
 * The signer of ghcr.io/fluxcd/<controller>:<tag>: fluxcd's shared controller release workflow
 * (any of its tags), called by a push of <tag> in the repository fluxcd/<controller>.
 */
export function fluxSigningArgs(controller: string, tag: string): string[] {
  return [
    "--certificate-identity-regexp",
    "^https://github\\.com/fluxcd/gha-workflows/\\.github/workflows/controller-release\\.yaml@refs/tags/v[0-9.]+$",
    "--certificate-oidc-issuer",
    GITHUB_OIDC_ISSUER,
    "--certificate-github-workflow-repository",
    `fluxcd/${controller}`,
    "--certificate-github-workflow-ref",
    `refs/tags/${tag}`,
    "--certificate-github-workflow-trigger",
    "push",
  ];
}

async function verifiedDigest(cosign: string, image: string, tag: string): Promise<string> {
  const controller = image.split("/").at(-1) ?? "";
  const output = await run([
    cosign,
    "verify",
    `${image}:${tag}`,
    ...fluxSigningArgs(controller, tag),
  ]);
  const signatures: unknown = JSON.parse(output.stdout);
  const digests = new Set(
    (Array.isArray(signatures) ? signatures : []).map((signature: unknown) => {
      const critical = isMap(signature) && isMap(signature.critical) ? signature.critical : {};
      return isMap(critical.image) ? critical.image["docker-manifest-digest"] : undefined;
    }),
  );
  const [digest] = [...digests];
  if (digests.size !== 1 || !isDigest(digest)) {
    throw new ReleaseError(`cosign verify ${image}:${tag} did not name exactly one signed digest`);
  }
  return digest;
}

if (import.meta.main) {
  await runStage("flux-components", async () => {
    const flux = await protoTool("flux");
    const cosign = await protoTool("cosign");
    const components = (await run([flux, "install", "--export", `--components=${FLUX_COMPONENTS.join(",")}`], { cwd: ROOT })).stdout;
    const pinned: PinnedImage[] = [];
    for (const { name, tag } of controllerImages(components)) {
      const digest = await verifiedDigest(cosign, name, tag);
      console.log(`${name}:${tag} → ${digest} (signature verified)`);
      pinned.push({ name, tag, digest });
    }
    writeFileSync(join(ROOT, FLUX_SYSTEM_DIR, "gotk-components.yaml"), components);
    writeFileSync(join(ROOT, FLUX_SYSTEM_DIR, "kustomization.yaml"), renderFluxSystemKustomization(pinned));
    console.log(`wrote ${FLUX_SYSTEM_DIR}/gotk-components.yaml and kustomization.yaml`);
  });
}
