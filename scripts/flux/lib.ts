/**
 * Shared logic of the SaaS GitOps tooling (deploy/flux/README.md): what the scripts in this
 * directory render and parse, without tools or network, so `bun test deploy/flux` and
 * scripts/flux/lib.test.ts can check it.
 */

import { FLUX_WATCH_LABEL, sopsDocumentProblems, sopsMetadataValid, sopsValueValid } from "../ci/check-secret-handling.ts";
import { APP_NAMESPACE, CLUSTER_DIR, CLUSTERS, type ClusterName, GHCR_PULL_NAME, appDir } from "../release/flux.ts";
import { ReleaseError, isDigest, isMap } from "../release/lib.ts";

/** The value of a `--cluster` option: one of the clusters in deploy/flux/clusters. */
export function parseCluster(value: string | undefined): ClusterName {
  if (!CLUSTERS.includes(value as ClusterName)) {
    throw new ReleaseError(`--cluster must be one of ${CLUSTERS.join(", ")}`);
  }
  return value as ClusterName;
}

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

/** How the stdin value of an Opaque Secret is checked (sops-secret.ts). */
export type ValueFormat = "token" | "text";

/** A DNS-1123 label: a namespace name, and each dot-separated part of an object name. */
const DNS_LABEL = /^[a-z0-9](?:[-a-z0-9]{0,61}[a-z0-9])?$/;

/** A Kubernetes object name (DNS-1123 subdomain): at most 253 characters, DNS labels joined by dots. */
export function isK8sName(name: string): boolean {
  return name.length <= 253 && name.split(".").every((label) => DNS_LABEL.test(label));
}
/** A Secret key. */
const SECRET_KEY = /^[-._a-zA-Z0-9]{1,253}$/;

export interface OpaqueSecret {
  namespace: string;
  name: string;
  /** The key whose value is secret: read from stdin, never from an argument. */
  key: string;
  value: string;
  format: ValueFormat;
  /** Keys whose values are not secret, such as a user name. */
  plain?: Record<string, string>;
  /**
   * Whether the Secret carries the label that makes Flux upgrade a HelmRelease that reads it, the
   * only label a SOPS Secret may have (check-secret-handling.ts, sopsDocumentProblems).
   */
  watch?: boolean;
}

/**
 * Checks the secret value of an Opaque Secret and returns it as it goes into the Secret. Errors
 * never quote the value.
 * - token: one printable ASCII word (an API key, a password); surrounding whitespace, such as the
 *   newline of `echo` or a password manager, is dropped.
 * - text: one line of text (checkedTextValue), such as a mail sender.
 */
export function checkedSecretValue(value: string, format: ValueFormat): string {
  if (format === "token") {
    const token = value.trim();
    if (!/^[\x21-\x7e]+$/.test(token)) {
      throw new ReleaseError("the value is empty or has spaces or control characters inside it");
    }
    return token;
  }
  return checkedTextValue(value);
}

/**
 * One line of text, such as a host name or a mail sender (`Name <address>`): spaces and any
 * printable character, `<`, `>` and `@` included. Exactly one trailing newline (from `echo` or a
 * file) is dropped. Refused: an empty value, any other carriage return or line feed, any control
 * or invisible format character (bidi overrides, zero-width characters, line and paragraph
 * separators), and spaces at either end. Errors never quote the value; `what` names it.
 */
export function checkedTextValue(value: string, what = "the value"): string {
  const line = value.endsWith("\n") ? value.slice(0, -1) : value;
  if (line === "") {
    throw new ReleaseError(`${what} is empty`);
  }
  if (/[\r\n]/.test(line)) {
    throw new ReleaseError(`${what} has more than one line or a carriage return`);
  }
  if (/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/u.test(line)) {
    throw new ReleaseError(`${what} has a control or invisible format character`);
  }
  if (line.trim() !== line) {
    throw new ReleaseError(`${what} starts or ends with whitespace`);
  }
  return line;
}

/**
 * An Opaque Secret as plain YAML, before sops encrypts it (only its stringData values are
 * encrypted, .sops.yaml). Only ever written to sops's stdin, never to a file or a command line
 * (docs/architecture/secrets.md, rule 3). Every value is a JSON string, which YAML reads as is.
 */
export function renderOpaqueSecret(secret: OpaqueSecret): string {
  const plain = secret.plain ?? {};
  if (secret.key in plain) {
    throw new ReleaseError(`the key ${secret.key} is both the secret value and a plain one`);
  }
  for (const [key, value] of Object.entries(plain)) {
    if (!/^[\x20-\x7e]*$/.test(value)) {
      throw new ReleaseError(`the plain value of ${key} must be one line of printable ASCII`);
    }
  }
  checkSecretNames(secret.namespace, secret.name, [secret.key, ...Object.keys(plain)]);
  return renderSecretData(secret.namespace, secret.name, { ...plain, [secret.key]: checkedSecretValue(secret.value, secret.format) }, secret.watch === true);
}

function checkSecretNames(namespace: string, name: string, keys: readonly string[]): void {
  if (!isK8sName(name) || !DNS_LABEL.test(namespace)) {
    throw new ReleaseError("the Secret name must be a Kubernetes name and the namespace a DNS label");
  }
  for (const key of keys) {
    if (!SECRET_KEY.test(key)) {
      throw new ReleaseError(`the Secret key ${JSON.stringify(key)} is not a valid key`);
    }
  }
}

/** The Secret's YAML; `data` holds values that are already checked, as they go into stringData. */
function renderSecretData(namespace: string, name: string, data: Record<string, string>, watch: boolean): string {
  const entries = Object.entries(data).sort(([a], [b]) => a.localeCompare(b));
  return [
    "apiVersion: v1",
    "kind: Secret",
    "metadata:",
    `  name: ${name}`,
    `  namespace: ${namespace}`,
    ...(watch ? ["  labels:", ...Object.entries(FLUX_WATCH_LABEL).map(([k, v]) => `    ${k}: ${JSON.stringify(v)}`)] : []),
    "type: Opaque",
    "stringData:",
    ...entries.map(([key, v]) => `  ${JSON.stringify(key)}: ${JSON.stringify(v)}`),
    "",
  ].join("\n");
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

// --- The demo environment's private values ------------------------------------------------------

/** The SOPS-encrypted Secret with the demo's private values (deploy/flux/README.md, 「展示叢集」). */
export const DEMO_VALUES_NAME = "trustiot-demo-values";

/**
 * The demo's private values: each key of Secret trustiot-demo-values and the one chart value it
 * sets. The HelmRelease reads every key on its own with `targetPath` and `literal: true`, so the
 * encrypted file, which no reviewer sees in clear text, can set these four values and nothing else
 * (`bun test deploy/flux` checks the HelmRelease against this list). Not secret, only private: the
 * public repository does not name the demo's hosts or its sending domain.
 */
export const DEMO_PRIVATE_VALUES: Readonly<Record<string, string>> = {
  "app-host": "hosts.app",
  "gateway-host": "hosts.gateway",
  "ota-host": "hosts.ota",
  "smtp-from": "smtp.from",
};

/** A lowercase DNS name with at least two labels, as the chart's schema wants a host. */
const DNS_NAME = /^(?=.{1,253}$)[a-z0-9](?:[-a-z0-9]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[-a-z0-9]{0,61}[a-z0-9])?)+$/;

/**
 * The demo's private values from `<key>=<value>` lines (demo-values.ts): exactly the keys of
 * DEMO_PRIVATE_VALUES, each once, every value one line of text (checkedTextValue) and every
 * `*-host` value a DNS name. Errors name keys and line numbers, never values.
 */
export function parseDemoValues(text: string): Record<string, string> {
  const keys = Object.keys(DEMO_PRIVATE_VALUES);
  const lines = (text.endsWith("\n") ? text.slice(0, -1) : text).split("\n");
  const values: Record<string, string> = {};
  lines.forEach((line, index) => {
    const at = line.indexOf("=");
    const key = at > 0 ? line.slice(0, at) : "";
    if (!keys.includes(key)) {
      throw new ReleaseError(`line ${index + 1} is not <key>=<value> with one of the keys ${keys.join(", ")}`);
    }
    if (key in values) {
      throw new ReleaseError(`${key} is given more than once`);
    }
    const value = checkedTextValue(line.slice(at + 1), key);
    if (key.endsWith("-host") && !DNS_NAME.test(value)) {
      throw new ReleaseError(`${key} is not a lowercase DNS name`);
    }
    values[key] = value;
  });
  const missing = keys.filter((key) => !(key in values));
  if (missing.length > 0) {
    throw new ReleaseError(`missing ${missing.join(", ")}`);
  }
  return values;
}

/**
 * Secret trustiot-demo-values as plain YAML, before sops encrypts it: the four keys of
 * DEMO_PRIVATE_VALUES and the label that makes Flux upgrade the release when it changes.
 */
export function renderDemoValuesSecret(text: string): string {
  const values = parseDemoValues(text);
  checkSecretNames(APP_NAMESPACE, DEMO_VALUES_NAME, Object.keys(values));
  return renderSecretData(APP_NAMESPACE, DEMO_VALUES_NAME, values, true);
}

// --- Which SOPS Secrets a cluster may hold ------------------------------------------------------

/** The Secret types the SOPS files use. */
export type SopsSecretType = "Opaque" | "kubernetes.io/dockerconfigjson";

/**
 * The SOPS-encrypted Secrets each cluster may hold, `deploy/flux/clusters/<cluster>/trustiot/<name>.sops.yaml`
 * in namespace trustiot, with their type (deploy/flux/README.md; docs/architecture/secrets.md, the
 * inventory). Nothing else: a name the chart reserves for a Secret it generates in the cluster
 * (`trustiot-identity-signing-keys`, the password Secrets) must never come from git, where Helm
 * would adopt it and keep its data. A new Secret is added here, in the README and in the inventory
 * in the same PR. `bun test deploy/flux` checks every SOPS file against this list, and the scripts
 * that write SOPS files refuse any other name or type.
 */
export const SOPS_SECRETS: Readonly<Record<ClusterName, Readonly<Record<string, SopsSecretType>>>> = {
  saas: {
    [GHCR_PULL_NAME]: "kubernetes.io/dockerconfigjson",
    "trustiot-qdrant": "Opaque",
    "trustiot-sentry": "Opaque",
  },
  demo: {
    [GHCR_PULL_NAME]: "kubernetes.io/dockerconfigjson",
    "trustiot-smtp": "Opaque",
    "trustiot-anthropic": "Opaque",
    [DEMO_VALUES_NAME]: "Opaque",
  },
};

/** Stops unless `cluster` may hold a SOPS Secret `name` of `type` (SOPS_SECRETS). */
export function requireSopsSecret(cluster: ClusterName, name: string, type: string): void {
  const allowed = SOPS_SECRETS[cluster];
  if (!Object.hasOwn(allowed, name)) {
    throw new ReleaseError(
      `cluster ${cluster} holds no SOPS Secret ${name}; it may hold ${Object.keys(allowed).join(", ")} (SOPS_SECRETS in scripts/flux/lib.ts)`,
    );
  }
  if (allowed[name] !== type) {
    throw new ReleaseError(`the SOPS Secret ${name} of cluster ${cluster} has type ${allowed[name]}, not ${type}`);
  }
}

/**
 * Why a file in the release directory of `cluster` is not a SOPS Secret it may hold; empty when it
 * is: one document, SOPS-encrypted (check-secret-handling.ts: the values, the sops block, and
 * sopsDocumentProblems), a Secret of SOPS_SECRETS with its type, in namespace trustiot, in the file
 * `<name>.sops.yaml`. Messages never quote values.
 */
export function sopsFileProblems(cluster: ClusterName, file: string, text: string): string[] {
  let parsed: unknown;
  try {
    parsed = Bun.YAML.parse(text);
  } catch {
    return [`${file} is not YAML`];
  }
  if (!isMap(parsed)) return [`${file} is not one YAML mapping`];
  const problems = sopsDocumentProblems(parsed).map((problem) => `${file}: ${problem}`);
  const values = [parsed.data, parsed.stringData].flatMap((block) => (isMap(block) ? Object.values(block) : block === undefined ? [] : [block]));
  if (!sopsMetadataValid(parsed.sops) || values.length === 0 || !values.every(sopsValueValid)) {
    problems.push(`${file} is not a SOPS-encrypted Secret with values`);
  }
  const metadata = isMap(parsed.metadata) ? parsed.metadata : {};
  const name = typeof metadata.name === "string" ? metadata.name : "";
  try {
    requireSopsSecret(cluster, name, typeof parsed.type === "string" ? parsed.type : "(none)");
  } catch (error) {
    problems.push(`${file}: ${error instanceof Error ? error.message : String(error)}`);
  }
  if (metadata.namespace !== APP_NAMESPACE) problems.push(`${file}: the Secret is not in namespace ${APP_NAMESPACE}`);
  if (file !== `${name}.sops.yaml`) problems.push(`${file} is not named after its Secret (<name>.sops.yaml)`);
  return problems;
}

// --- Who can decrypt (.sops.yaml) ---------------------------------------------------------------

/** One recipient of a .sops.yaml rule and the role its comment records. */
export interface SopsRecipient {
  key: string;
  /** `cluster: <name>` for a cluster's own key, `operator` for a person's. */
  role: string;
}

/** A creation rule of .sops.yaml as its text lists it. */
export interface SopsRuleText {
  pathRegex: string;
  recipients: SopsRecipient[];
}

const AGE_PUBLIC_KEY = /^age1[02-9ac-hj-np-z]{58}$/;
const RULE_START = /^ {2}- path_regex: (\S+)$/;
const AGE_EMPTY = /^ {4}age: \[\]$/;
const AGE_LIST = /^ {4}age:$/;
const RECIPIENT_LINE = /^ {6}- (\S+) # (cluster: [a-z]+|operator)$/;

/**
 * Reads the recipients of every rule from the text of .sops.yaml, with their roles. Every
 * recipient is one line `      - <age public key> # cluster: <name>` or `... # operator`: the
 * comment records the role only, never a person's or a machine's name. Throws on any other way of
 * writing a recipient, so that a key cannot sit in a rule without a role.
 */
export function sopsRuleTexts(text: string): SopsRuleText[] {
  const rules: SopsRuleText[] = [];
  const lines = text.split("\n");
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index] ?? "";
    const start = RULE_START.exec(line);
    if (start !== null) {
      rules.push({ pathRegex: start[1] ?? "", recipients: [] });
      continue;
    }
    if (line.trimStart().startsWith("#") || (!/age1/.test(line) && !/^\s*-?\s*age\s*:/.test(line))) continue;
    const rule = rules.at(-1);
    if (rule === undefined) throw new ReleaseError(`.sops.yaml line ${index + 1}: a recipient outside a creation rule`);
    if (AGE_EMPTY.test(line)) continue;
    if (AGE_LIST.test(line)) {
      while (index + 1 < lines.length && /^ {6}- /.test(lines[index + 1] ?? "")) {
        index += 1;
        const recipient = RECIPIENT_LINE.exec(lines[index] ?? "");
        if (recipient === null || !AGE_PUBLIC_KEY.test(recipient[1] ?? "")) {
          throw new ReleaseError(`.sops.yaml line ${index + 1}: write a recipient as "- <age public key> # cluster: <name>" or "- <age public key> # operator"`);
        }
        rule.recipients.push({ key: recipient[1] ?? "", role: recipient[2] ?? "" });
      }
      continue;
    }
    throw new ReleaseError(`.sops.yaml line ${index + 1}: write age as "age: []" or as a list with one recipient per line`);
  }
  return rules;
}

/**
 * What is wrong with the recipients of .sops.yaml (empty when nothing): every rule belongs to one
 * cluster; a rule is either empty (sops refuses to encrypt for it) or holds its own cluster's key
 * or keys and every operator key, at least one of each, and no other cluster's key; a key has one
 * role in the whole file and appears once per rule.
 */
export function sopsRecipientProblems(rules: readonly SopsRuleText[], clusters: readonly ClusterName[] = CLUSTERS): string[] {
  const problems: string[] = [];
  const roles = new Map<string, string>();
  for (const { recipients } of rules) {
    for (const { key, role } of recipients) {
      const known = roles.get(key);
      if (known !== undefined && known !== role) problems.push(`${key} is listed as ${known} and as ${role}`);
      roles.set(key, role);
    }
  }
  const operators = [...new Set(rules.flatMap((rule) => rule.recipients.filter((r) => r.role === "operator").map((r) => r.key)))];
  for (const rule of rules) {
    const owners = clusters.filter((cluster) => new RegExp(rule.pathRegex).test(`${appDir(cluster)}/${GHCR_PULL_NAME}.sops.yaml`));
    const cluster = owners[0];
    if (owners.length !== 1 || cluster === undefined) {
      problems.push(`the rule ${rule.pathRegex} matches ${owners.length === 0 ? "no cluster" : `the files of ${owners.join(", ")}`}, not exactly one cluster`);
      continue;
    }
    const keys = rule.recipients.map((r) => r.key);
    if (new Set(keys).size !== keys.length) problems.push(`the ${cluster} rule lists a recipient twice`);
    for (const { key, role } of rule.recipients) {
      if (role === "operator" || role === `cluster: ${cluster}`) continue;
      problems.push(
        clusters.includes(role.slice("cluster: ".length) as ClusterName)
          ? `the ${cluster} rule holds ${key}, the key of ${role}`
          : `${key} has the role ${role}, which names no cluster`,
      );
    }
    if (keys.length === 0) continue;
    if (!rule.recipients.some((r) => r.role === `cluster: ${cluster}`)) problems.push(`the ${cluster} rule has recipients but not the ${cluster} cluster's key`);
    if (operators.length === 0) problems.push(`the ${cluster} rule has no operator key, so no person could edit its files`);
    for (const operator of operators) {
      if (!keys.includes(operator)) problems.push(`the ${cluster} rule lacks the operator key ${operator}`);
    }
  }
  return problems;
}
