/**
 * Shared logic of the SaaS GitOps tooling (deploy/flux/README.md): what the scripts in this
 * directory render and parse, without tools or network, so `bun test deploy/flux` and
 * scripts/flux/lib.test.ts can check it.
 */

import { CLUSTER_DIR, GHCR_PULL_NAME } from "../release/flux.ts";
import { ReleaseError, isDigest } from "../release/lib.ts";

export const FLUX_SYSTEM_DIR = `${CLUSTER_DIR}/flux-system`;
/**
 * The controllers the SaaS cluster runs. No notification-controller (alerts go through the
 * observability stack, and no webhook receiver is exposed: the host opens no ports) and no
 * image automation (releases are proposed by release.yml, not by the cluster, which only reads).
 */
export const FLUX_COMPONENTS = ["source-controller", "kustomize-controller", "helm-controller"] as const;

export interface ControllerImage {
  /** `ghcr.io/fluxcd/<controller>` */
  name: string;
  tag: string;
}

export interface PinnedImage extends ControllerImage {
  digest: string;
}

/** The controller images that `flux install --export` refers to, in order of appearance. */
export function controllerImages(components: string): ControllerImage[] {
  const images: ControllerImage[] = [];
  for (const match of components.matchAll(/^\s+image: (ghcr\.io\/fluxcd\/[a-z-]+):(v[0-9][^\s@]*)\s*$/gm)) {
    const name = match[1] ?? "";
    const tag = match[2] ?? "";
    if (!images.some((image) => image.name === name)) {
      images.push({ name, tag });
    }
  }
  const unknown = [...components.matchAll(/^\s+image: (\S+)\s*$/gm)]
    .map((match) => match[1] ?? "")
    .filter((ref) => !images.some((image) => ref === `${image.name}:${image.tag}`));
  if (unknown.length > 0) {
    throw new ReleaseError(`unexpected images in the Flux manifests: ${unknown.join(", ")}`);
  }
  if (images.length !== FLUX_COMPONENTS.length) {
    throw new ReleaseError(`expected ${FLUX_COMPONENTS.length} controller images, found ${images.length}`);
  }
  return images;
}

/** flux-system/kustomization.yaml: Flux's manifests with every controller pinned by digest. */
export function renderFluxSystemKustomization(images: readonly PinnedImage[]): string {
  const lines = [
    "# Flux itself. The bootstrap applies this directory once (deploy/flux/README.md); after that Flux",
    "# manages its own controllers from here. Written by scripts/flux/components.ts: the images are",
    "# pinned to the digests that their keyless cosign signatures by the fluxcd release workflows",
    "# cover. `bun test deploy/flux` checks this file against gotk-components.yaml.",
    "apiVersion: kustomize.config.k8s.io/v1beta1",
    "kind: Kustomization",
    "resources:",
    "  - gotk-components.yaml",
    "  - gotk-sync.yaml",
    "images:",
  ];
  for (const image of images) {
    if (!isDigest(image.digest)) {
      throw new ReleaseError(`${image.name}:${image.tag} has no sha256 digest`);
    }
    lines.push(`  - name: ${image.name}`, `    newTag: ${image.tag}`, `    digest: ${image.digest}`);
  }
  return `${lines.join("\n")}\n`;
}

/** A Docker registry login as kubernetes.io/dockerconfigjson wants it. */
export function dockerConfigJson(registry: string, username: string, token: string): string {
  if (!/^[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})$/.test(username)) {
    throw new ReleaseError("the GitHub user name is not valid");
  }
  if (!/^[\x21-\x7e]+$/.test(token)) {
    throw new ReleaseError("the token is empty or has spaces or control characters");
  }
  const auth = Buffer.from(`${username}:${token}`).toString("base64");
  return JSON.stringify({ auths: { [registry]: { username, password: token, auth } } });
}

/**
 * The pull Secret as plain YAML, before sops encrypts it. Only ever written to sops's stdin,
 * never to a file or a command line (docs/architecture/secrets.md, rule 3).
 */
export function renderPullSecret(namespace: string, registry: string, username: string, token: string): string {
  const config = Buffer.from(dockerConfigJson(registry, username, token)).toString("base64");
  return [
    "apiVersion: v1",
    "kind: Secret",
    "metadata:",
    `  name: ${GHCR_PULL_NAME}`,
    `  namespace: ${namespace}`,
    "type: kubernetes.io/dockerconfigjson",
    "data:",
    `  .dockerconfigjson: ${config}`,
    "",
  ].join("\n");
}
