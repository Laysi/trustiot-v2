/**
 * Writes the SOPS-encrypted pull Secret of a cluster (deploy/flux/README.md, "GHCR 拉取 token"):
 * the read-only GHCR token of the machine account, as a kubernetes.io/dockerconfigjson Secret
 * that Flux uses to pull the chart and the kubelet uses to pull the images.
 *
 *   <command that prints the token> | bun scripts/flux/ghcr-pull-secret.ts --username <account>
 *     [--cluster saas|demo] [--out <path>] [--registry ghcr.io] [--namespace trustiot]
 *
 * --out defaults to deploy/flux/clusters/<cluster>/trustiot/ghcr-pull.sops.yaml (cluster saas).
 * The token is read from stdin only, never from an argument (docs/architecture/secrets.md,
 * rule 3), and the plain Secret only exists in memory and on sops's stdin; the file written is
 * the encrypted one, for the recipients in .sops.yaml.
 */

import { join } from "node:path";
import { parseOptions, runStage } from "../release/exec.ts";
import { APP_NAMESPACE, GHCR_PULL_NAME, appDir } from "../release/flux.ts";
import { parseCluster, renderPullSecret } from "./lib.ts";
import { kustomizationReminder, readPiped, writeEncrypted } from "./sops.ts";

if (import.meta.main) {
  await runStage("ghcr-pull-secret", async () => {
    const options = parseOptions(Bun.argv.slice(2), {
      username: {},
      cluster: { default: "saas" },
      out: { default: "" },
      registry: { default: "ghcr.io" },
      namespace: { default: APP_NAMESPACE },
    });
    const cluster = parseCluster(options.cluster);
    const token = (await readPiped("the token")).trim();
    const plain = renderPullSecret(options.namespace ?? "", options.registry ?? "", options.username ?? "", token);
    const file = `${GHCR_PULL_NAME}.sops.yaml`;
    const written = await writeEncrypted(plain, options.out || join(appDir(cluster), file));
    console.log(`wrote ${written} (encrypted for the recipients in .sops.yaml)`);
    const reminder = options.out ? undefined : kustomizationReminder(appDir(cluster), file);
    if (reminder !== undefined) console.log(reminder);
  });
}
