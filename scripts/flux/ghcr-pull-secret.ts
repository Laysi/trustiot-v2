/**
 * Writes the SOPS-encrypted pull Secret for the SaaS cluster (deploy/flux/README.md, "GHCR 拉取
 * token"): the read-only GHCR token of the machine account, as a kubernetes.io/dockerconfigjson
 * Secret that Flux uses to pull the chart and the kubelet uses to pull the images.
 *
 *   <command that prints the token> | bun scripts/flux/ghcr-pull-secret.ts --username <account>
 *     [--out deploy/flux/clusters/saas/trustiot/ghcr-pull.sops.yaml] [--registry ghcr.io]
 *     [--namespace trustiot]
 *
 * The token is read from stdin only, never from an argument (docs/architecture/secrets.md,
 * rule 3), and the plain Secret only exists in memory and on sops's stdin; the file written is
 * the encrypted one, for the recipients in .sops.yaml.
 */

import { existsSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { parseOptions, protoTool, run, runStage } from "../release/exec.ts";
import { ReleaseError } from "../release/lib.ts";
import { APP_DIR, APP_NAMESPACE, GHCR_PULL_NAME } from "../release/flux.ts";
import { renderPullSecret } from "./lib.ts";

const ROOT = resolve(import.meta.dir, "..", "..");

if (import.meta.main) {
  await runStage("ghcr-pull-secret", async () => {
    const options = parseOptions(Bun.argv.slice(2), {
      username: {},
      out: { default: join(APP_DIR, `${GHCR_PULL_NAME}.sops.yaml`) },
      registry: { default: "ghcr.io" },
      namespace: { default: APP_NAMESPACE },
    });
    if (process.stdin.isTTY) {
      throw new ReleaseError("pipe the token into stdin; it is never taken from an argument or a prompt that echoes");
    }
    const token = (await Bun.stdin.text()).trim();
    const plain = renderPullSecret(options.namespace ?? "", options.registry ?? "", options.username ?? "", token);
    const out = resolve(options.out ?? "");
    if (!existsSync(dirname(out))) {
      throw new ReleaseError(`${dirname(out)} does not exist`);
    }
    const sops = await protoTool("sops");
    // --filename-override picks the creation rule (recipients) in .sops.yaml for the target path.
    const encrypted = await run(
      [sops, "encrypt", "--input-type", "yaml", "--output-type", "yaml", "--filename-override", relative(ROOT, out)],
      { cwd: ROOT, stdin: plain },
    );
    const temporary = `${out}.tmp-${process.pid}`;
    writeFileSync(temporary, encrypted.stdout, { mode: 0o644 });
    renameSync(temporary, out);
    console.log(`wrote ${relative(ROOT, out)} (encrypted for the recipients in .sops.yaml)`);
  });
}
