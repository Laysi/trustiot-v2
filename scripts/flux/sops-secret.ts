/**
 * Writes a SOPS-encrypted Opaque Secret of a cluster from a value on stdin
 * (deploy/flux/README.md, 「祕密」 and 「展示叢集」):
 *
 *   <command that prints the value> | bun scripts/flux/sops-secret.ts --cluster demo \
 *     --name <secret> --key <key> [--format token|text] [--set <key>=<plain value>] [--watch]
 *
 * - The secret value comes from stdin only, never from an argument (docs/architecture/secrets.md,
 *   rule 3); the plain Secret exists only in memory and on sops's stdin. The file written is
 *   deploy/flux/clusters/<cluster>/trustiot/<secret>.sops.yaml, encrypted for the recipients that
 *   .sops.yaml names for that cluster.
 * - --format token (default) takes one word such as an API key; --format text takes one line of
 *   text, spaces and `<>@` included (lib.ts, checkedTextValue).
 * - --set adds one key whose value is not secret, such as a user name (it is encrypted too).
 * - --watch labels the Secret so that Flux upgrades the HelmRelease as soon as it changes (for a
 *   Secret that the HelmRelease reads values from).
 *
 * The demo's private values have their own script, demo-values.ts, which writes exactly the keys
 * the HelmRelease reads.
 *
 * Examples, for the demo environment:
 *   <print the Resend API key> | bun scripts/flux/sops-secret.ts --cluster demo --name trustiot-smtp --key password --set username=resend
 *   <print the Anthropic API key> | bun scripts/flux/sops-secret.ts --cluster demo --name trustiot-anthropic --key api-key
 */

import { join } from "node:path";
import { parseOptions, runStage } from "../release/exec.ts";
import { ReleaseError } from "../release/lib.ts";
import { APP_NAMESPACE, appDir } from "../release/flux.ts";
import { DEMO_VALUES_NAME, type ValueFormat, parseCluster, renderOpaqueSecret } from "./lib.ts";
import { kustomizationReminder, readPiped, writeEncrypted } from "./sops.ts";

/** `key=value` of --set; the value is not secret. */
export function parsePlain(option: string | undefined): Record<string, string> {
  if (option === undefined || option === "") return {};
  const at = option.indexOf("=");
  if (at <= 0) {
    throw new ReleaseError("--set takes <key>=<value>");
  }
  return { [option.slice(0, at)]: option.slice(at + 1) };
}

export function parseFormat(option: string | undefined): ValueFormat {
  if (option !== "token" && option !== "text") {
    throw new ReleaseError("--format must be token or text");
  }
  return option;
}

/** The demo's private values Secret has its own keys and script; this one would write other keys. */
export function checkName(name: string): string {
  if (name === DEMO_VALUES_NAME) {
    throw new ReleaseError(`write ${DEMO_VALUES_NAME} with scripts/flux/demo-values.ts, which writes exactly the keys the HelmRelease reads`);
  }
  return name;
}

if (import.meta.main) {
  await runStage("sops-secret", async () => {
    const options = parseOptions(Bun.argv.slice(2), {
      cluster: {},
      name: {},
      key: {},
      format: { default: "token" },
      set: { default: "" },
      watch: { flag: true },
    });
    const cluster = parseCluster(options.cluster);
    const name = checkName(options.name ?? "");
    const plain = renderOpaqueSecret({
      namespace: APP_NAMESPACE,
      name,
      key: options.key ?? "",
      value: await readPiped("the value"),
      format: parseFormat(options.format),
      plain: parsePlain(options.set),
      labels: options.watch === "true" ? { "reconcile.fluxcd.io/watch": "Enabled" } : {},
    });
    const file = `${name}.sops.yaml`;
    const written = await writeEncrypted(plain, join(appDir(cluster), file));
    console.log(`wrote ${written} (encrypted for the recipients in .sops.yaml)`);
    const reminder = kustomizationReminder(appDir(cluster), file);
    if (reminder !== undefined) console.log(reminder);
  });
}
