/**
 * Writes the demo environment's private values as the SOPS-encrypted Secret trustiot-demo-values
 * (deploy/flux/README.md, 「展示叢集」):
 *
 *   <command that prints the four lines> | bun scripts/flux/demo-values.ts
 *
 * stdin holds one `<key>=<value>` line for each key of DEMO_PRIVATE_VALUES (lib.ts), in any order:
 *
 *   app-host=<app host name>
 *   gateway-host=<gateway host name>
 *   ota-host=<ota host name>
 *   smtp-from=<sender, such as: TrustIoT Demo <no-reply@sending domain>>
 *
 * Each value is one line of text and every host a lowercase DNS name; errors name the key, never
 * the value. The Secret has exactly these four keys and the watch label, and the HelmRelease reads
 * each key into its one chart value with `literal: true`. The values are not secret, only private,
 * so they never go on a command line either. The file written is
 * deploy/flux/clusters/demo/trustiot/trustiot-demo-values.sops.yaml, encrypted for the recipients
 * of the demo rule in .sops.yaml; the whole file is replaced. To change one value, `sops edit` it
 * (deploy/flux/README.md).
 */

import { join } from "node:path";
import { runStage } from "../release/exec.ts";
import { appDir } from "../release/flux.ts";
import { ReleaseError } from "../release/lib.ts";
import { DEMO_VALUES_NAME, renderDemoValuesSecret, requireSopsSecret } from "./lib.ts";
import { kustomizationReminder, readPiped, writeEncrypted } from "./sops.ts";

if (import.meta.main) {
  await runStage("demo-values", async () => {
    if (Bun.argv.length > 2) {
      throw new ReleaseError("demo-values.ts takes no arguments: the values come from stdin");
    }
    requireSopsSecret("demo", DEMO_VALUES_NAME, "Opaque");
    const plain = renderDemoValuesSecret(await readPiped("the four <key>=<value> lines"));
    const file = `${DEMO_VALUES_NAME}.sops.yaml`;
    const written = await writeEncrypted(plain, join(appDir("demo"), file));
    console.log(`wrote ${written} (encrypted for the recipients in .sops.yaml)`);
    const reminder = kustomizationReminder(appDir("demo"), file);
    if (reminder !== undefined) console.log(reminder);
  });
}
