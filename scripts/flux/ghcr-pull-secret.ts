/**
 * Writes the SOPS-encrypted pull Secret of a cluster (deploy/flux/README.md, "GHCR 拉取 token"):
 * a read-only GHCR token (a classic token with only read:packages; the README says who creates
 * it), as a kubernetes.io/dockerconfigjson Secret that Flux uses to pull the chart and the
 * kubelet uses to pull the images.
 *
 *   <command that prints the token> | bun scripts/flux/ghcr-pull-secret.ts --username <account>
 *     [--cluster saas|demo] [--registry ghcr.io]
 *
 * It writes deploy/flux/clusters/<cluster>/trustiot/ghcr-pull.sops.yaml (cluster saas by default),
 * Secret ghcr-pull in namespace trustiot, the one place SOPS_SECRETS (lib.ts) allows for it.
 * The token is read from stdin only, never from an argument (docs/architecture/secrets.md,
 * rule 3), and the plain Secret only exists in memory and on sops's stdin; the file written is
 * the encrypted one, for the recipients in .sops.yaml.
 */

import { join } from "node:path";
import { parseOptions, runStage } from "../release/exec.ts";
import { APP_NAMESPACE, GHCR_PULL_NAME, appDir } from "../release/flux.ts";
import { parseCluster, renderPullSecret, requireSopsSecret } from "./lib.ts";
import { kustomizationReminder, readPiped, writeEncrypted } from "./sops.ts";

if (import.meta.main) {
  await runStage("ghcr-pull-secret", async () => {
    const options = parseOptions(Bun.argv.slice(2), {
      username: {},
      cluster: { default: "saas" },
      registry: { default: "ghcr.io" },
    });
    const cluster = parseCluster(options.cluster);
    requireSopsSecret(cluster, GHCR_PULL_NAME, "kubernetes.io/dockerconfigjson");
    const token = (await readPiped("the token")).trim();
    const plain = renderPullSecret(APP_NAMESPACE, options.registry ?? "", options.username ?? "", token);
    const file = `${GHCR_PULL_NAME}.sops.yaml`;
    const written = await writeEncrypted(plain, join(appDir(cluster), file));
    console.log(`wrote ${written} (encrypted for the recipients in .sops.yaml)`);
    const reminder = kustomizationReminder(appDir(cluster), file);
    if (reminder !== undefined) console.log(reminder);
  });
}
