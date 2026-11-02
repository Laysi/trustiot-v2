/**
 * Installs Flux on the demo machine, and through it the TrustIoT release of the demo environment
 * (Laysi/trustiot-v2#484; the install on the demo machine is Laysi/trustiot-v2#467, part of
 * Laysi/trustiot-v2#457; deploy/flux/README.md, 「在展示機安裝 Flux」). It runs on a workstation
 * with a clean checkout of origin/main and acts on the demo machine over ssh, under the rules of
 * lib.ts (lab lock, dry run with a confirmation code, logs, the ops tunnel left alone):
 *
 *   bun scripts/demo/flux.ts deploy-key --ssh-host <host> [--apply --confirm <code>]
 *   bun scripts/demo/flux.ts install    --ssh-host <host> [--apply --confirm <code>]
 *   bun scripts/demo/flux.ts status     --ssh-host <host>
 *   bun scripts/demo/flux.ts remove     --ssh-host <host> [--apply --confirm <code>]
 *
 * - deploy-key generates the cluster's own SSH deploy key on the demo machine (ssh-keygen as
 *   root, in a root-only directory under /run) and stores it with GitHub's host keys
 *   (deploy/flux/github-known-hosts) as the Secret that the GitRepository reads. Only the public
 *   key and its fingerprint are printed: a person adds them on GitHub as a read-only deploy key.
 * - install applies `kubectl kustomize deploy/flux/clusters/demo/flux-system` the way
 *   deploy/flux/README.md 「建立叢集」 step 6 does (server-side apply, wait for the CRDs, apply
 *   again), with the GitRepository pinned to the commit of main that this run checked
 *   (pinnedBootstrap), then waits for the controllers, the GitRepository, the Kustomization, the
 *   chart's OCIRepository and the HelmRelease, removes the pin so that Flux follows main again,
 *   and reports each one.
 * - status only reads.
 * - remove takes Flux and the release away in an order that keeps the workload data (see
 *   runRemove).
 *
 * Every refusal is collected first and printed together; --apply runs only when there is none.
 * The private halves of the deploy key and of the sops-age key never leave the demo machine:
 * they only pass through pipes, a root-only file that is shredded, and the Kubernetes Secrets.
 * Nothing here reads a Secret with `-o json` or `-o yaml`; Secrets are read by name, type, labels,
 * key names and value sizes (SECRET_TEMPLATE), and only public values (the deploy key's public
 * half and known_hosts) are read back to the workstation. Everything a run prints, logs, saves or
 * throws goes through FluxLog.clean (redact() and the ssh host hidden).
 */

import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { RUN_LABEL } from "../../deploy/flux/tests/e2e/preflight.ts";
import { ageToolFor } from "../flux/age.ts";
import { FLUX_COMPONENTS, SOPS_SECRETS, sopsFileProblems, sopsRuleTexts } from "../flux/lib.ts";
import { ipv4ToNumber, serviceCidrsOf, staticBandOf } from "../nightly/ingress-address.ts";
import { parseOptions, protoTool, tryRun } from "../release/exec.ts";
import { appDir, chartSourceFile, clusterDir, parseChartSource } from "../release/flux.ts";
import { CHART_REGISTRY, ReleaseError, compareReleaseTags, errorMessage, isMap } from "../release/lib.ts";
import { NAMESPACE_DEFAULTS, type ObjectFact, INVENTORY_JSONPATH, inventoryResources, parseInventory } from "./cleanup.ts";
import {
  Box,
  COMMON_SPEC,
  type CommonOptions,
  DEMO_MIN_CHART_VERSION,
  DEMO_NAMESPACE,
  type KindState,
  LOCK_DIR,
  Log,
  type OpsTunnel,
  POC_FINGERPRINTS,
  ROOT,
  checkConfirmation,
  clusterState,
  commonOptions,
  hidingSshHost,
  lastLines,
  main,
  opsTunnel,
  ownsLock,
  sha256,
  shownPath,
  shq,
  sshRun,
  stableJson,
  stateDiff,
  takeLock,
  withOpsTunnel,
} from "./lib.ts";
import { mergeValues } from "./platform.ts";
import { LOCK_STATE_COMMAND, type LockState, StillRunningError, parseLockState, redactSecrets } from "./tunnel.ts";

/** The issue whose lab lock this script takes: the install on the demo machine. */
export const ISSUE = "Laysi/trustiot-v2#467";
export const FLUX_NAMESPACE = "flux-system";
/**
 * The label that marks what this script made: `installed` on the namespace flux-system (set by
 * install before it applies anything), `deploy-key` on the deploy key Secret. Later runs, remove
 * and a person (or a lab e2e test, which refuses any flux-system) can tell this install apart.
 */
export const MARKER = "trustiot.io/demo-flux";
export const MARKED_INSTALL = "installed";
export const MARKED_DEPLOY_KEY = "deploy-key";
/**
 * The field manager of the marker. kustomize-controller takes over (and then drops, when its own
 * manifests lack them) fields of the managers `kubectl` and `before-first-apply`, and of its own
 * name; fields of any other manager stay (kustomize-controller v1.9.6, ApplyCleanupOptions). So
 * the label survives Flux applying its own Namespace, and the bootstrap apply, as `kubectl`, hands
 * every Flux object over to Flux.
 */
export const FIELD_MANAGER = "trustiot-demo-flux";
/** The Secret and key that the root Kustomization decrypts SOPS files with (deploy/flux/README.md, 「展示叢集的 age 金鑰」). */
export const SOPS_AGE_KEY = "age.agekey";
/** The keys of the deploy key Secret, as source-controller reads them for SSH. */
export const DEPLOY_KEY_KEYS = ["identity", "identity.pub", "known_hosts"] as const;
/** The comment of the generated deploy key: no host or user name. */
export const DEPLOY_KEY_COMMENT = "trustiot-demo-flux";
/** Root-only working directories on the demo machine (tmpfs); each run uses its own and removes it. */
export const REMOTE_BASE = "/run/trustiot-demo-flux";
export const KNOWN_HOSTS_FILE = "deploy/flux/github-known-hosts";
/**
 * The chart value that pins the ingress Service's ClusterIP in tunnel mode
 * (Laysi/trustiot-v2#478). One constant, so a different name in the chart is one edit here.
 */
export const INGRESS_CLUSTER_IP_PATH = ["ingress", "tunnel", "clusterIP"] as const;
/** The CRDs of the platform components the chart's objects need (deploy/istio, deploy/cloudnative-pg, platform.ts). */
export const PLATFORM_CRDS = [
  "clusters.postgresql.cnpg.io",
  "imagecatalogs.postgresql.cnpg.io",
  "gateways.gateway.networking.k8s.io",
  "httproutes.gateway.networking.k8s.io",
  "authorizationpolicies.security.istio.io",
] as const;
/** The demo release: the OCIRepository of chart.yaml and the HelmRelease, both `trustiot` in namespace trustiot. */
export const RELEASE_NAME = "trustiot";
export const REPOSITORY = "Laysi/trustiot-v2";
const GIT_URL = "ssh://git@github.com/Laysi/trustiot-v2.git";
const SYNC_PATH = `./${clusterDir("demo")}`;
const FLUX_SYSTEM = `${clusterDir("demo")}/flux-system`;
/**
 * What the plan is built from, the tools it runs included (.prototools and the schema plugins that
 * verify their checksums); anything uncommitted there, or hidden from git status, stops it.
 */
const CODE_PATHS = ["deploy/flux", "scripts", ".sops.yaml", ".prototools", ".proto/plugins"];
const ORIGIN_URL = /^(https:\/\/github\.com\/|git@github\.com:|ssh:\/\/git@github\.com\/)Laysi\/trustiot-v2(\.git)?\/?$/i;
/**
 * A Kubernetes object name (DNS-1123 subdomain): names read from main go into YAML and shell text
 * only after this check. Exactly the API server's rule for a Secret name, no stricter: Kubernetes
 * validates it with ValidateSecretName = apimachineryvalidation.NameIsDNSSubdomain
 * (kubernetes/kubernetes pkg/apis/core/validation/validation.go), which is IsDNS1123Subdomain
 * (kubernetes/apimachinery pkg/util/validation/validation.go): at most 253 characters and the
 * pattern dns1123LabelFmt("." dns1123LabelFmt)*, with no limit of 63 per label (only
 * IsDNS1123Label has that). Either way the pattern alone keeps quotes, spaces and newlines out.
 */
const DNS_1123 = /^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$/;

export function isObjectName(name: string): boolean {
  return name.length <= 253 && DNS_1123.test(name);
}

const KINDS = {
  crd: "customresourcedefinitions.apiextensions.k8s.io",
  deployment: "deployments.apps",
  gitRepository: "gitrepositories.source.toolkit.fluxcd.io",
  kustomization: "kustomizations.kustomize.toolkit.fluxcd.io",
  ociRepository: "ocirepositories.source.toolkit.fluxcd.io",
  helmRelease: "helmreleases.helm.toolkit.fluxcd.io",
} as const;

/** Poll interval of every wait: each poll is a short read, because ssh cuts a command that prints nothing for about 3 minutes (scripts/README.md). */
export const POLL_MS = 10_000;
/** Bounds of the waits; each names what it waits for when it runs out. */
export const WAITS = {
  /** Three controller images pulled and started on the arm64 machine. */
  controllers: 10 * 60_000,
  /** The first clone of main over SSH, after the deploy key is on GitHub. */
  source: 10 * 60_000,
  /** The first apply of the demo tree, with SOPS decryption. */
  kustomization: 10 * 60_000,
  /** The chart pull and its keyless signature check. */
  chart: 15 * 60_000,
  /** The HelmRelease: timeout 10m per attempt, 3 install retries with remediation in between. */
  release: 50 * 60_000,
  /** remove: helm-controller uninstalls the release (timeout 10m). */
  uninstall: 20 * 60_000,
  /** remove: Flux objects, CRDs and the namespace going away. */
  deleted: 10 * 60_000,
} as const;

type Values = Record<string, unknown>;

// --- Redaction ---------------------------------------------------------------------------------

/**
 * Anything shaped like a host name: dotted labels whose last label is letters (every top-level
 * domain is) or punycode. Fail closed: every match is replaced unless shownName restores it,
 * wherever it starts (after `*.`, `--` or a second dot too), so private names such as db.internal
 * or box.lan never print. Where a longer dotted word does not end in letters
 * (sh.helm.release.v1.trustiot.v1), its leading part still matches and is replaced.
 */
const HOST_CORE = String.raw`(?:[a-z0-9_](?:[a-z0-9_-]{0,61}[a-z0-9_])?\.)+(?:[a-z]{2,63}|xn--[a-z0-9-]{1,59})`;
const HOST_NAME = new RegExp(String.raw`${HOST_CORE}(?![\w-])`, "gi");
/**
 * An e-mail address at any host-shaped domain, shown names included (someone@github.com is a
 * person's address): replaced before shownName can keep its domain. The clone user of
 * `git@github.com:Laysi/trustiot-v2` is no person and stays.
 */
const EMAIL = new RegExp(String.raw`(?<![\w.%+-])(?!git@)[A-Za-z0-9._%+-]+@${HOST_CORE}(?![\w-])`, "gi");
const HOST_SHAPE = new RegExp(`^${HOST_CORE}$`, "i");
/**
 * Public names printed exactly as they are: the repository's host and the registry. Other public
 * names (another registry, the signature issuer) print as <host> too: that costs some detail, and
 * keeps these lists within scripts/demo/public-text.test.ts's.
 */
const SHOWN_NAMES = new Set(["github.com", "ghcr.io"]);
/**
 * Kubernetes API groups that status lines and records carry, shown alone (`networking.k8s.io/v1`)
 * or after one resource label (`gitrepositories.source.toolkit.fluxcd.io`,
 * `lease.coordination.k8s.io`). No other name in these zones is shown.
 */
const API_GROUPS = new Set([
  "source.toolkit.fluxcd.io",
  "kustomize.toolkit.fluxcd.io",
  "helm.toolkit.fluxcd.io",
  "notification.toolkit.fluxcd.io",
  "image.toolkit.fluxcd.io",
  "apiextensions.k8s.io",
  "admissionregistration.k8s.io",
  "apiregistration.k8s.io",
  "certificates.k8s.io",
  "coordination.k8s.io",
  "discovery.k8s.io",
  "events.k8s.io",
  "flowcontrol.apiserver.k8s.io",
  "gateway.networking.k8s.io",
  "metrics.k8s.io",
  "networking.k8s.io",
  "node.k8s.io",
  "rbac.authorization.k8s.io",
  "resource.k8s.io",
  "scheduling.k8s.io",
  "storage.k8s.io",
  "postgresql.cnpg.io",
  "security.istio.io",
  "networking.istio.io",
  "telemetry.istio.io",
  "extensions.istio.io",
  "helm.cattle.io",
  "k3s.cattle.io",
]);
/** Label and annotation key prefixes, shown only right before the `/` of a key (`trustiot.io/demo-flux`), never as names of their own. */
const LABEL_PREFIXES = new Set([
  "trustiot.io",
  "e2e.trustiot.io",
  "k8s.io",
  "kubernetes.io",
  "app.kubernetes.io",
  "kubectl.kubernetes.io",
  "pod-security.kubernetes.io",
  "fluxcd.io",
  "reconcile.fluxcd.io",
  "istio.io",
  "sidecar.istio.io",
  "topology.istio.io",
  "cnpg.io",
  "helm.sh",
  "meta.helm.sh",
]);
/** A Service's or a pod's name in the cluster's DNS (<name>.<namespace>.svc[.cluster.local]): cluster addresses may be shown (CLAUDE.md). */
const CLUSTER_DNS = /^[\w-]+\.[\w-]+\.(?:svc|pod)(?:\.cluster\.local)?$/i;
/**
 * Endings of dotted words that are no host names: file extensions that are no top-level domain (a
 * manifest, a chart archive, a clone URL, a certificate, a script, a lock, a unit) and the
 * Kubernetes API groups without a dot (deployment.apps, job.batch). The rest of the word must not
 * look like a host name itself, so that db.internal.yaml is still replaced.
 */
const NOT_HOST_ENDING = /\.(?:sops\.yaml|crt|git|json|tgz|yaml|yml|ts|lock|log|toml|service|apps|batch|policy|autoscaling)$/i;
/** Dotted words this script prints that are no host names: Secret keys, a file and the fields its messages name. */
const SHOWN_WORDS = new Set(["identity.pub", "age.agekey", "readme.md", "metadata.name", "spec.ref", "spec.ref.commit", "spec.ref.branch", "spec.suspend", "spec.prune", INGRESS_CLUSTER_IP_PATH.join(".").toLowerCase()]);
/** IPv4 addresses, also right after a dot (`v1.192.0.2.5`) and before a final one (`at 192.0.2.5.`). */
const IPV4_TEXT = /(?<!\d)(\d{1,3}(?:\.\d{1,3}){3})(?!\d|\.\d)/g;
/** Pod and Service networks of k3s and loopback: cluster addresses may be shown (CLAUDE.md). */
const SHOWN_ADDRESS = /^(10\.42\.|10\.43\.|127\.)/;
/**
 * IPv6 addresses, full (eight groups) or compressed (with ::). The demo cluster runs IPv4 only,
 * so every IPv6 address but loopback is outside it. Runs after IPV4_TEXT, so that the IPv4 tail
 * of an IPv4-mapped address is already replaced.
 */
const IPV6_TEXT = /(?<![\w:])((?:[0-9a-f]{1,4}:){7}[0-9a-f]{1,4}|(?:[0-9a-f]{1,4}(?::[0-9a-f]{1,4}){0,6})?::(?:[0-9a-f]{1,4}(?::[0-9a-f]{1,4}){0,6})?)(?![\w:])/gi;
/** Public values this script prints as they are: an OpenSSH public key and a SHA256 fingerprint. */
const PUBLIC_VALUE = /(?:ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(?:256|384|521)) AAAA[A-Za-z0-9+/]+={0,3}|SHA256:[A-Za-z0-9+/]{43}(?![A-Za-z0-9+/=])/g;
/**
 * Credentials by their shape: GitHub tokens, Resend and OpenAI-style API keys, AWS access key ids,
 * HTTP authorization values, the user information of a URL (a password, or a token as the user
 * name; the clone URL's `ssh://git@` stays), and key=value settings of a secret.
 */
const CREDENTIALS: readonly (readonly [RegExp, string])[] = [
  [/\bgh[pousr]_[A-Za-z0-9]{16,}/g, "<redacted>"],
  [/\bgithub_pat_[A-Za-z0-9_]{16,}/g, "<redacted>"],
  [/\bre_[A-Za-z0-9_]{16,}/g, "<redacted>"],
  [/\bsk-[A-Za-z0-9_-]{16,}/g, "<redacted>"],
  [/\bAKIA[0-9A-Z]{16}\b/g, "<redacted>"],
  [/\b(Bearer|Basic)\s+[A-Za-z0-9._~+/=-]{8,}/gi, "$1 <redacted>"],
  [/\b([a-z][a-z0-9+.-]*:\/\/)(?!git@)[^\s/@]+@/gi, "$1<redacted>@"],
  [/\b(password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key)(["']?\s*[:=]\s*)("[^"]*"|'[^']*'|[^\s"',;&]+)/gi, "$1$2<redacted>"],
];
/** A run of base64 or base64url characters long enough to be a key or a password. */
const LONG_RUN = /(?<![A-Za-z0-9+/_-])[A-Za-z0-9+/_-]{20,}={0,2}(?![A-Za-z0-9+/_=-])/g;
/** Words or lower-case names joined by `/` (`Deployment/trustiot/trustiot-k8s-proxy`, `com/Laysi/trustiot-v2`): a path, not a secret. */
const NAME_PATH = /^(?:[A-Za-z]+|[a-z0-9][a-z0-9_-]*)(?:\/(?:[A-Za-z]+|[a-z0-9][a-z0-9_-]*))+$/;

/**
 * Whether a long run looks random: upper case, lower case and digits mixed, and no path of names.
 * Hex digests and commits (lower case and digits), age recipients, Kubernetes names and condition
 * reasons (letters only) do not.
 */
function looksRandom(run: string): boolean {
  return /[A-Z]/.test(run) && /[a-z]/.test(run) && /[0-9]/.test(run) && !NAME_PATH.test(run.replace(/=+$/, ""));
}

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** Local paths as they may be printed: this checkout as <repo>, home directories as ~ (local tool errors name them). */
function hidePaths(text: string): string {
  const end = String.raw`(?=$|[/\s'"\x60:;,)])`;
  let out = text;
  const root = ROOT.replace(/\/+$/, "");
  if (root.length > 1) out = out.replace(new RegExp(`${escapeRegExp(root)}${end}`, "g"), "<repo>");
  const home = (process.env.HOME || homedir()).replace(/\/+$/, "");
  if (home.length > 1) out = out.replace(new RegExp(`${escapeRegExp(home)}${end}`, "g"), "~");
  return out.replace(/(?<![\w.~-])\/(?:home|Users)\/[^/\s'"`:;,)]+/g, "~").replace(new RegExp(String.raw`(?<![\w.~-])/root${end}`, "g"), "~");
}

function hideCredentials(text: string): string {
  let out = text;
  for (const [pattern, replacement] of CREDENTIALS) out = out.replace(pattern, replacement);
  return redactSecrets(out.replace(LONG_RUN, (run) => (looksRandom(run) ? "<redacted>" : run)));
}

function shownName(name: string, next: string): boolean {
  const lower = name.toLowerCase();
  if (SHOWN_NAMES.has(lower) || API_GROUPS.has(lower) || SHOWN_WORDS.has(lower) || CLUSTER_DNS.test(lower)) return true;
  const dot = lower.indexOf(".");
  if (/^[a-z0-9-]+$/.test(lower.slice(0, dot)) && API_GROUPS.has(lower.slice(dot + 1))) return true;
  if (next === "/" && LABEL_PREFIXES.has(lower)) return true;
  const ending = NOT_HOST_ENDING.exec(lower);
  return ending !== null && !HOST_SHAPE.test(lower.slice(0, ending.index));
}

/**
 * Text as a run may print, log or save it: local paths shortened; private keys, tokens and other
 * credentials removed (tunnel.ts redactSecrets for JWTs and very long base64, plus the shapes
 * above and random-looking runs), except the public key and fingerprint this script prints; host
 * names other than the public ones above, and e-mail addresses at them, replaced; addresses other
 * than cluster addresses replaced. Status messages can quote values, such as a host name from the
 * private values in a failed install, and local tool errors name local paths.
 */
export function redact(text: string): string {
  const keys = hidePaths(text)
    .replace(/-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?(-----END [A-Z0-9 ]*PRIVATE KEY-----|$)/g, "<redacted private key>")
    .replace(/AGE-SECRET-KEY-1[0-9A-Z]+/gi, "<redacted age key>")
    // A whole JSON Web Token: redactSecrets takes its segments one by one and keeps short ones.
    .replace(/eyJ[A-Za-z0-9_-]*\.[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]*)?/g, "<redacted>");
  let out = "";
  let last = 0;
  for (const match of keys.matchAll(PUBLIC_VALUE)) {
    out += hideCredentials(keys.slice(last, match.index)) + match[0];
    last = match.index + match[0].length;
  }
  out += hideCredentials(keys.slice(last));
  return out
    .replace(EMAIL, "<email>")
    .replace(HOST_NAME, (name: string, offset: number, whole: string) => (shownName(name, whole.charAt(offset + name.length)) ? name : "<host>"))
    .replace(IPV4_TEXT, (address: string) => (SHOWN_ADDRESS.test(address) ? address : "<address>"))
    .replace(IPV6_TEXT, (address: string) => (address === "::1" ? address : "<address>"));
}

/** Names read from the cluster (object refs, label keys, Secret keys), joined as they may be printed. */
export function names(items: readonly string[]): string {
  return redact(items.join(", "));
}

/**
 * The log of a run: lib.ts' Log, with everything it prints and saves passed through redact().
 * Box, takeLock and withOpsTunnel note through it too, so ssh's and kubectl's own error lines are
 * cleaned before they reach the console or a file.
 */
export class FluxLog extends Log {
  clean(text: string): string {
    return redact(text);
  }

  override note(message: string): void {
    super.note(this.clean(message));
  }

  override save(name: string, data: unknown): string {
    return super.save(name, cleanValue(data, (text) => this.clean(text)));
  }
}

function cleanValue(value: unknown, clean: (text: string) => string): unknown {
  if (typeof value === "string") return clean(value);
  if (Array.isArray(value)) return value.map((item) => cleanValue(item, clean));
  if (isMap(value)) return Object.fromEntries(Object.entries(value).map(([key, item]) => [clean(key), cleanValue(item, clean)]));
  return value;
}

/**
 * Runs a step and rethrows its error with the message cleaned, so that the error lib.ts' main
 * prints, and any caller sees, holds what the log may hold. A StillRunningError stays one: the lab
 * lock stays held for it.
 */
export async function cleanErrors<T>(log: FluxLog, body: () => Promise<T>): Promise<T> {
  try {
    return await body();
  } catch (error) {
    const message = log.clean(errorMessage(error));
    throw error instanceof StillRunningError ? new StillRunningError(message) : new ReleaseError(message);
  }
}

// --- The checkout --------------------------------------------------------------------------------

export interface CodeFacts {
  head: string;
  originMain: string;
  /** `git status --porcelain` lines under CODE_PATHS. */
  dirty: string[];
  /** `git ls-files -v` lines under CODE_PATHS of files marked assume-unchanged or skip-worktree: git status does not compare them. */
  hidden: string[];
  originUrl: string;
  fetchError?: string;
}

/** Why the checkout cannot be the source of the plan: it must be a clean origin/main of this repository on GitHub, fetched just now. */
export function codeProblems(code: CodeFacts): string[] {
  const problems: string[] = [];
  if (code.fetchError !== undefined) problems.push(`git fetch origin main failed (${code.fetchError}); the plan is made from main as GitHub has it`);
  if (!ORIGIN_URL.test(code.originUrl.trim())) problems.push(`the remote origin is not ${REPOSITORY} on GitHub; run from a clone of it`);
  if (code.head === "" || code.head !== code.originMain) {
    problems.push(`HEAD (${code.head.slice(0, 12) || "unknown"}) is not origin/main (${code.originMain.slice(0, 12) || "unknown"}); run from origin/main, for example after \`git switch --detach origin/main\``);
  }
  if (code.dirty.length > 0) problems.push(`${CODE_PATHS.join(", ")} have uncommitted changes: ${code.dirty.slice(0, 10).join("; ")}`);
  if (code.hidden.length > 0) {
    problems.push(
      `files under ${CODE_PATHS.join(", ")} are marked assume-unchanged or skip-worktree, so git status cannot show whether they differ from main (${code.hidden.slice(0, 10).join("; ")}); clear the marks with \`git update-index --no-assume-unchanged --no-skip-worktree <file>\``,
    );
  }
  return problems;
}

/** Fetches main from origin and reads what codeProblems checks. */
export async function readCode(root: string = ROOT): Promise<CodeFacts> {
  const git = (...args: string[]) => tryRun(["git", "-C", root, ...args]);
  const fetched = await git("fetch", "--quiet", "origin", "+refs/heads/main:refs/remotes/origin/main");
  const head = await git("rev-parse", "--verify", "HEAD");
  const originMain = await git("rev-parse", "--verify", "refs/remotes/origin/main");
  const status = await git("status", "--porcelain", "--untracked-files=all", "--", ...CODE_PATHS);
  // -v tags each tracked file: H when git compares it with the work tree; S (skip-worktree) or a
  // lower-case tag (assume-unchanged) when it does not, so its changes are missing from status.
  const listed = await git("ls-files", "-v", "--", ...CODE_PATHS);
  const url = await git("remote", "get-url", "origin");
  if (status.exitCode !== 0) throw new ReleaseError(`git status failed: ${redact(lastLines(status.stderr))}`);
  if (listed.exitCode !== 0) throw new ReleaseError(`git ls-files failed: ${redact(lastLines(listed.stderr))}`);
  return {
    head: head.exitCode === 0 ? head.stdout.trim() : "",
    originMain: originMain.exitCode === 0 ? originMain.stdout.trim() : "",
    dirty: status.stdout.split("\n").filter((line) => line.trim() !== ""),
    hidden: listed.stdout.split("\n").filter((line) => line.trim() !== "" && !line.startsWith("H ")),
    originUrl: url.exitCode === 0 ? url.stdout.trim() : "",
    // git's message names the remote's URL, which may carry a credential: only the exit status.
    ...(fetched.exitCode === 0 ? {} : { fetchError: `exit ${fetched.exitCode}` }),
  };
}

// --- What main says --------------------------------------------------------------------------------

interface Manifest {
  apiVersion?: string;
  kind?: string;
  metadata?: { name?: string; namespace?: string };
  spec?: Record<string, unknown>;
}

/** Flux's own objects as `kubectl kustomize deploy/flux/clusters/demo/flux-system` renders them. */
export interface FluxManifests {
  text: string;
  sha256: string;
  /** `<Kind>/<name>` of every object, in order. */
  objects: string[];
  crds: string[];
  /** Each CRD's kind and its name: `HelmRelease` → `helmreleases.helm.toolkit.fluxcd.io`. */
  crdKinds: Record<string, string>;
  /** `clusterrole/<name>` and `clusterrolebinding/<name>`. */
  clusterObjects: string[];
  deployments: string[];
  /** The GitRepository's apiVersion, for the document that pins it (pinDocument). */
  gitApiVersion: string;
  /** The Secret the GitRepository reads its deploy key from. */
  deployKeySecret: string;
  /** The Secret the root Kustomization decrypts SOPS files with. */
  sopsSecret: string;
  /** The objects in namespace flux-system as the namespace inventory names them: `kind.group/name`. */
  namespaceObjects: string[];
}

const CLUSTER_SCOPED = new Set(["Namespace", "CustomResourceDefinition", "ClusterRole", "ClusterRoleBinding"]);
const NAMESPACED = new Set(["ResourceQuota", "ServiceAccount", "Service", "Deployment", "NetworkPolicy", "GitRepository", "Kustomization"]);

function parseDocuments(text: string): unknown[] {
  const parsed: unknown = Bun.YAML.parse(text);
  return (Array.isArray(parsed) ? parsed : [parsed]).filter((doc) => doc !== null && doc !== undefined);
}

/** The value at a path of map keys and list indexes, or undefined. */
function at(value: unknown, path: readonly string[]): unknown {
  let current = value;
  for (const key of path) {
    if (Array.isArray(current) && /^\d+$/.test(key)) current = current[Number(key)];
    else if (isMap(current)) current = current[key];
    else return undefined;
  }
  return current;
}

function text(value: unknown): string {
  return typeof value === "string" ? value : "";
}

/**
 * Reads Flux's manifests and checks that they are what this script knows how to install and
 * remove: one Namespace flux-system holding every namespaced object, only Flux toolkit CRDs, the
 * controllers, and the sync of this repository's main into the demo tree with SOPS decryption.
 * Anything else is a problem, so an unexpected change to the SaaS Flux stops the install.
 */
export function parseManifests(manifestText: string): { manifests?: FluxManifests; problems: string[] } {
  let docs: unknown[];
  try {
    docs = parseDocuments(manifestText);
  } catch (error) {
    return { problems: [`Flux's manifests are not YAML: ${errorMessage(error)}`] };
  }
  const problems: string[] = [];
  const objects: string[] = [];
  const crds: string[] = [];
  const crdKinds: Record<string, string> = {};
  const clusterObjects: string[] = [];
  const deployments: string[] = [];
  const namespaceObjects: string[] = [];
  let namespaces = 0;
  let gits = 0;
  let roots = 0;
  let git: Manifest | undefined;
  let root: Manifest | undefined;
  for (const doc of docs) {
    if (!isMap(doc)) {
      problems.push("Flux's manifests hold a document that is not a mapping");
      continue;
    }
    const manifest = doc as Manifest;
    const kind = text(manifest.kind);
    const name = text(manifest.metadata?.name);
    const namespace = text(manifest.metadata?.namespace);
    objects.push(`${kind}/${name}`);
    if (CLUSTER_SCOPED.has(kind)) {
      if (namespace !== "") problems.push(`${kind}/${name} is cluster-scoped but names namespace ${namespace}`);
    } else if (NAMESPACED.has(kind)) {
      if (namespace !== FLUX_NAMESPACE) problems.push(`${kind}/${name} is not in namespace ${FLUX_NAMESPACE}`);
      const apiVersion = text(manifest.apiVersion);
      const group = apiVersion.includes("/") ? apiVersion.slice(0, apiVersion.indexOf("/")) : "";
      namespaceObjects.push(`${kind.toLowerCase()}${group === "" ? "" : `.${group}`}/${name}`);
    } else {
      problems.push(`Flux's manifests hold a ${kind || "(no kind)"}, which this script does not know how to install and remove`);
    }
    if (kind === "Namespace") {
      namespaces += 1;
      if (name !== FLUX_NAMESPACE) problems.push(`Flux's manifests declare namespace ${name}`);
    }
    if (kind === "CustomResourceDefinition") {
      if (!name.endsWith(".toolkit.fluxcd.io")) problems.push(`Flux's manifests hold the CRD ${name}, which is not a Flux toolkit CRD`);
      crds.push(name);
      const crdKind = text(at(manifest.spec, ["names", "kind"]));
      if (crdKind === "") problems.push(`the CRD ${name} names no kind`);
      else crdKinds[crdKind] = name;
    }
    if (kind === "ClusterRole" || kind === "ClusterRoleBinding") clusterObjects.push(`${kind.toLowerCase()}/${name}`);
    if (kind === "Deployment") deployments.push(name);
    if (kind === "GitRepository") {
      gits += 1;
      git = manifest;
    }
    if (kind === "Kustomization") {
      roots += 1;
      root = manifest;
    }
  }
  if (namespaces !== 1) problems.push(`Flux's manifests declare ${namespaces} Namespaces, not exactly ${FLUX_NAMESPACE}`);
  for (const component of FLUX_COMPONENTS) {
    if (!deployments.includes(component)) problems.push(`Flux's manifests have no Deployment ${component}`);
  }
  if (gits !== 1) problems.push(`Flux's manifests hold ${gits} GitRepositories, not exactly one (the sync's)`);
  if (roots !== 1) problems.push(`Flux's manifests hold ${roots} Kustomizations, not exactly one (the root)`);
  const deployKeySecret = text(at(git?.spec, ["secretRef", "name"]));
  const sopsSecret = text(at(root?.spec, ["decryption", "secretRef", "name"]));
  const gitApiVersion = text(git?.apiVersion);
  if (git !== undefined) {
    if (git.metadata?.name !== FLUX_NAMESPACE) problems.push(`the GitRepository is ${text(git.metadata?.name)}, not ${FLUX_NAMESPACE}`);
    if (!/^source\.toolkit\.fluxcd\.io\/v[0-9a-z]+$/.test(gitApiVersion)) problems.push(`the GitRepository has the apiVersion ${JSON.stringify(gitApiVersion)}, not source.toolkit.fluxcd.io/<version>`);
    if (at(git.spec, ["url"]) !== GIT_URL) problems.push(`the GitRepository does not read ${GIT_URL}`);
    // install pins spec.ref.commit for the first deploy and removes it afterwards (pinnedBootstrap).
    if (stableJson(at(git.spec, ["ref"])) !== stableJson({ branch: "main" })) problems.push("the GitRepository's spec.ref is not exactly {branch: main}");
    if (deployKeySecret === "") problems.push("the GitRepository has no secretRef (the deploy key)");
    else if (!isObjectName(deployKeySecret)) problems.push(`the GitRepository's secretRef ${JSON.stringify(deployKeySecret)} is not a Kubernetes object name`);
  }
  if (root !== undefined) {
    if (root.metadata?.name !== FLUX_NAMESPACE) problems.push(`the Kustomization is ${text(root.metadata?.name)}, not ${FLUX_NAMESPACE}`);
    // The demo tree only: the SaaS tree must never be applied on the demo machine.
    if (at(root.spec, ["path"]) !== SYNC_PATH) problems.push(`the root Kustomization applies ${text(at(root.spec, ["path"])) || "(no path)"}, not ${SYNC_PATH}`);
    if (at(root.spec, ["sourceRef", "kind"]) !== "GitRepository" || at(root.spec, ["sourceRef", "name"]) !== FLUX_NAMESPACE) {
      problems.push(`the root Kustomization does not read GitRepository ${FLUX_NAMESPACE}`);
    }
    if (at(root.spec, ["decryption", "provider"]) !== "sops" || sopsSecret === "") problems.push("the root Kustomization does not decrypt SOPS files with a secretRef");
    else if (!isObjectName(sopsSecret)) problems.push(`the root Kustomization's decryption secretRef ${JSON.stringify(sopsSecret)} is not a Kubernetes object name`);
  }
  if (problems.length > 0) return { problems };
  return {
    manifests: {
      text: manifestText,
      sha256: sha256(manifestText),
      objects,
      crds: crds.sort(),
      crdKinds,
      clusterObjects: clusterObjects.sort(),
      deployments: deployments.sort(),
      gitApiVersion,
      deployKeySecret,
      sopsSecret,
      namespaceObjects: namespaceObjects.sort(),
    },
    problems,
  };
}

/** What install's bootstrap applies (applyScript): Flux's manifests, the GitRepository pinned to the checked commit. */
export interface Bootstrap {
  /** Every document but the root Kustomization, the GitRepository's with spec.ref.commit = head. */
  text: string;
  sha256: string;
  /** The root Kustomization's document (JSON), applied last. */
  root: string;
  /** The commit of main the GitRepository is pinned to. */
  head: string;
  /** The GitRepository's apiVersion (pinDocument). */
  apiVersion: string;
}

/**
 * Flux's manifests with spec.ref.commit of the GitRepository set to `head`, the commit of main
 * that this run checked. Without it, source-controller's first clone takes whatever main is by
 * then, and kustomize-controller (cluster-admin) would apply a commit whose checks never ran,
 * irreversible first-deploy parts included (the Postgres bootstrap, the ingress ClusterIP).
 * source-controller checks out the commit from the branch and names it `main@sha1:<commit>`
 * (source-controller v1.9.6 clones with github.com/fluxcd/pkg/git v0.52.0, whose gogit Clone
 * gives a commit precedence over the branch: cloneCommit in gogit/clone.go), so the waits stay
 * the same; a commit that is not on main fails the clone.
 *
 * The root Kustomization comes apart, so that applyScript creates it only after FIELD_MANAGER
 * co-owns the pin: kustomize-controller applies main's GitRepository, which has no commit, and
 * drops the fields of `kubectl` it takes over, but keeps fields another manager owns too. With the
 * Kustomization created last, nothing of Flux applies anything before the pin is safe. Only the
 * two documents change form (JSON, which is YAML); every other byte stays, and the result must
 * parse to the same documents but for the pin.
 */
export function pinnedBootstrap(manifests: FluxManifests, head: string): Bootstrap {
  if (!/^[0-9a-f]{40}$/.test(head)) throw new ReleaseError("the commit to pin is not a full commit hash");
  const isKind = (kind: string) => (doc: unknown): doc is Record<string, unknown> => isMap(doc) && doc.kind === kind;
  const isGit = isKind("GitRepository");
  const isRoot = isKind("Kustomization");
  let pinned = 0;
  const roots: Record<string, unknown>[] = [];
  const parts = manifests.text.split(/^(---[ \t]*)$/m).map((part, index) => {
    if (index % 2 === 1) return part;
    let doc: unknown;
    try {
      doc = Bun.YAML.parse(part);
    } catch {
      return part;
    }
    if (isRoot(doc)) {
      roots.push(doc);
      return "\n";
    }
    if (!isGit(doc)) return part;
    pinned += 1;
    const spec = isMap(doc.spec) ? doc.spec : {};
    const ref = isMap(spec.ref) ? spec.ref : {};
    return `\n${JSON.stringify({ ...doc, spec: { ...spec, ref: { ...ref, commit: head } } })}\n`;
  });
  if (pinned !== 1 || roots.length !== 1) throw new ReleaseError(`found ${pinned} GitRepository and ${roots.length} Kustomization documents in Flux's manifests, not one each`);
  const text = parts.join("");
  const root = JSON.stringify(roots[0]);
  const unpin = (doc: unknown) => {
    if (!isGit(doc) || !isMap(doc.spec) || !isMap(doc.spec.ref)) return doc;
    const { commit: _commit, ...ref } = doc.spec.ref;
    return { ...doc, spec: { ...doc.spec, ref } };
  };
  const after = [...parseDocuments(text), ...parseDocuments(root)];
  const original = parseDocuments(manifests.text);
  const expected = [...original.filter((doc) => !isRoot(doc)), ...original.filter(isRoot)];
  if (stableJson(after.map(unpin)) !== stableJson(expected) || stableJson(after.filter(isGit).map((doc) => at(doc, ["spec", "ref", "commit"]))) !== stableJson([head])) {
    throw new ReleaseError("pinning the GitRepository would change more of Flux's manifests than its spec.ref.commit");
  }
  return { text, sha256: sha256(text), root, head, apiVersion: manifests.gitApiVersion };
}

/** The document that makes FIELD_MANAGER a co-owner of the pin (applyScript, pinGit). */
export function pinDocument(apiVersion: string, head: string): string {
  if (!/^source\.toolkit\.fluxcd\.io\/v[0-9a-z]+$/.test(apiVersion) || !/^[0-9a-f]{40}$/.test(head)) throw new ReleaseError("not a GitRepository apiVersion and a full commit hash");
  return JSON.stringify({ apiVersion, kind: "GitRepository", metadata: { name: FLUX_NAMESPACE, namespace: FLUX_NAMESPACE }, spec: { ref: { commit: head } } });
}

/** spec.ref of the GitRepository: following main, pinned to a commit of main, or anything else. */
export type GitRef = { kind: "main" } | { kind: "pinned"; commit: string } | { kind: "other"; ref: string };

export function gitRefOf(object: Record<string, unknown> | undefined): GitRef {
  const ref = at(object, ["spec", "ref"]);
  if (isMap(ref) && ref.branch === "main") {
    const keys = Object.keys(ref).sort().join(",");
    if (keys === "branch") return { kind: "main" };
    if (keys === "branch,commit" && typeof ref.commit === "string" && /^[0-9a-f]{40}$/.test(ref.commit)) return { kind: "pinned", commit: ref.commit };
  }
  return { kind: "other", ref: redact(JSON.stringify(ref ?? null)).slice(0, 200) };
}

function describeRef(ref: GitRef): string {
  return ref.kind === "main" ? "{branch: main}" : ref.kind === "pinned" ? `{branch: main, commit: ${ref.commit}}` : ref.ref;
}

/** A dotted-quad IPv4 address without leading zeros. */
export function isIpv4(value: string): boolean {
  return /^(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(\.(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/.test(value);
}

/**
 * Where the chart reads each credential of services.<name>.credentials (the chart's
 * trustiot.credentialCatalog in deploy/chart/templates/_helpers.tpl).
 */
const CREDENTIAL_SECRETS: Record<string, string[][]> = {
  smtp: [["smtp", "credentialsSecret", "name"]],
  "object-storage": [["objectStorage", "credentialsSecret", "name"]],
  "qdrant-read": [["qdrant", "external", "credentialsSecret", "name"]],
  "qdrant-write": [["qdrant", "external", "credentialsSecret", "name"]],
  ai: [
    ["ai", "providers", "anthropic", "apiKeySecret", "name"],
    ["ai", "providers", "openai", "apiKeySecret", "name"],
  ],
};

/**
 * The Secrets in namespace trustiot that the demo release needs before it can start, each with
 * what needs it: the chart's pull Secret (OCIRepository secretRef), every Secret the HelmRelease
 * reads values from unless optional, the image pull Secrets, and the credentials that enabled
 * services request (merged values-saas.yaml and values-demo.yaml; the chart's own defaults request
 * none). A Secret only some later feature uses, such as trustiot-smtp before a service requests
 * the smtp credential, is not one of them.
 */
export function requiredSecrets(chartSource: unknown, helmRelease: unknown, values: Values): { secrets: Record<string, string>; problems: string[] } {
  const secrets: Record<string, string> = {};
  const problems: string[] = [];
  const add = (name: unknown, why: string) => {
    if (typeof name === "string" && name !== "") secrets[name] ??= why;
  };
  add(at(chartSource, ["spec", "secretRef", "name"]), "the OCIRepository pulls the chart with it");
  const valuesFrom = at(helmRelease, ["spec", "valuesFrom"]);
  for (const entry of Array.isArray(valuesFrom) ? valuesFrom : []) {
    if (isMap(entry) && entry.kind === "Secret" && entry.optional !== true) add(entry.name, "the HelmRelease reads values from it");
  }
  const pull = at(values, ["global", "imagePullSecrets"]);
  for (const entry of Array.isArray(pull) ? pull : []) add(isMap(entry) ? entry.name : entry, "global.imagePullSecrets: the pods pull their images with it");
  const services = at(values, ["services"]);
  for (const [service, settings] of Object.entries(isMap(services) ? services : {})) {
    if (!isMap(settings) || settings.enabled !== true) continue;
    for (const credential of Array.isArray(settings.credentials) ? settings.credentials : []) {
      const paths = CREDENTIAL_SECRETS[String(credential)];
      if (paths === undefined) {
        problems.push(`service ${service} requests the credential ${String(credential)}, which flux.ts does not know (CREDENTIAL_SECRETS)`);
        continue;
      }
      for (const path of paths) add(at(values, path), `service ${service} requests the credential ${String(credential)}`);
    }
  }
  return { secrets, problems };
}

/** What install, deploy-key and remove read from the checkout. */
export interface RepoInputs {
  head: string;
  manifests?: FluxManifests;
  knownHosts: string;
  chart?: { version: string; digest: string };
  /** The demo cluster's own age recipient in .sops.yaml (`# cluster: demo`). */
  demoRecipient?: string;
  /** The SOPS Secrets the release needs, with why. */
  secrets: Record<string, string>;
  clusterIp?: string;
  /** Problems with the checkout itself (codeProblems). */
  code: string[];
  /** Problems with Flux's manifests or GitHub's host keys: every subcommand that changes something stops. */
  flux: string[];
  /** Problems with what the release needs: install stops. */
  deploy: string[];
}

function readText(root: string, path: string): string | undefined {
  const file = join(root, path);
  return existsSync(file) ? readFileSync(file, "utf8") : undefined;
}

function parseYaml(root: string, path: string, problems: string[]): unknown {
  const content = readText(root, path);
  if (content === undefined) {
    problems.push(`${path} is missing`);
    return undefined;
  }
  try {
    return Bun.YAML.parse(content);
  } catch (error) {
    problems.push(`${path} is not YAML: ${errorMessage(error)}`);
    return undefined;
  }
}

/**
 * Reads the inputs from a checkout at `root` (the repository, or a test's copy): Flux's manifests
 * from `build` (kubectl kustomize with the pinned kubectl), GitHub's host keys, and what the
 * release needs, each checked: the chart at DEMO_MIN_CHART_VERSION or later, the pinned ingress
 * ClusterIP, the SOPS file of every Secret the release needs, and the demo cluster's recipient.
 */
export async function readRepo(root: string, code: CodeFacts, build: (root: string) => Promise<string>): Promise<RepoInputs> {
  const flux: string[] = [];
  const deploy: string[] = [];
  let manifests: FluxManifests | undefined;
  try {
    const parsed = parseManifests(await build(root));
    manifests = parsed.manifests;
    flux.push(...parsed.problems);
  } catch (error) {
    flux.push(`kubectl kustomize ${FLUX_SYSTEM} failed: ${errorMessage(error)}`);
  }
  const knownHosts = readText(root, KNOWN_HOSTS_FILE) ?? "";
  if (!knownHosts.split("\n").some((line) => line.startsWith("github.com ssh-ed25519 "))) flux.push(`${KNOWN_HOSTS_FILE} has no ed25519 host key of github.com`);

  const inputs: RepoInputs = { head: code.originMain, knownHosts, secrets: {}, code: codeProblems(code), flux, deploy };
  if (manifests !== undefined) inputs.manifests = manifests;

  // (a) the chart version.
  const chartFile = chartSourceFile("demo");
  const chartText = readText(root, chartFile);
  if (chartText === undefined) deploy.push(`${chartFile} is missing`);
  else {
    try {
      const chart = parseChartSource(chartText, REPOSITORY, `${CHART_REGISTRY}/trustiot`, chartFile);
      inputs.chart = { version: chart.version, digest: chart.digest };
      if (compareReleaseTags(`v${chart.version}`, `v${DEMO_MIN_CHART_VERSION}`) < 0) {
        deploy.push(
          `${chartFile} pins chart ${chart.version}, older than ${DEMO_MIN_CHART_VERSION} (DEMO_MIN_CHART_VERSION in scripts/demo/lib.ts): the demo's first deploy must run a release that sets everything up in the cluster; pin one with scripts/flux/pin-chart.ts --cluster demo (deploy/flux/README.md, 「在展示機安裝 Flux」)`,
        );
      }
    } catch (error) {
      deploy.push(errorMessage(error));
    }
  }

  // The pinned ingress ClusterIP.
  const app = appDir("demo");
  const demoValues = parseYaml(root, `${app}/values-demo.yaml`, deploy);
  const shapeValues = parseYaml(root, `${app}/values-saas.yaml`, deploy);
  const ip = at(demoValues, INGRESS_CLUSTER_IP_PATH);
  if (typeof ip === "string" && isIpv4(ip)) inputs.clusterIp = ip;
  else {
    deploy.push(
      `${app}/values-demo.yaml does not set ${INGRESS_CLUSTER_IP_PATH.join(".")} to an IPv4 address (${ip === undefined ? "not set" : `it is ${redact(JSON.stringify(ip)).slice(0, 40)}`}): ` +
        "the ingress Service's ClusterIP cannot change after the first deploy creates it, and the demo tunnel's route (Laysi/trustiot-v2#469) points at that address, " +
        "so the first deploy must already pin it (Laysi/trustiot-v2#478)",
    );
  }

  // (e) the SOPS files of the Secrets the release needs, on main and applied.
  const chartSource = chartText === undefined ? undefined : (() => {
    try {
      return Bun.YAML.parse(chartText);
    } catch {
      return undefined;
    }
  })();
  const helmRelease = parseYaml(root, `${app}/helm-release.yaml`, deploy);
  if (at(helmRelease, ["metadata", "name"]) !== RELEASE_NAME || at(helmRelease, ["metadata", "namespace"]) !== DEMO_NAMESPACE) {
    deploy.push(`${app}/helm-release.yaml is not HelmRelease ${DEMO_NAMESPACE}/${RELEASE_NAME}`);
  }
  const values = mergeValues(isMap(shapeValues) ? shapeValues : {}, isMap(demoValues) ? demoValues : {});
  const required = requiredSecrets(chartSource, helmRelease, values);
  inputs.secrets = required.secrets;
  deploy.push(...required.problems);
  const kustomization = parseYaml(root, `${app}/kustomization.yaml`, deploy);
  const resources = at(kustomization, ["resources"]);
  const listed = Array.isArray(resources) ? resources.map(String) : [];
  for (const [name, why] of Object.entries(required.secrets).sort(([a], [b]) => a.localeCompare(b))) {
    const file = `${name}.sops.yaml`;
    const content = readText(root, `${app}/${file}`);
    if (!Object.hasOwn(SOPS_SECRETS.demo, name)) {
      deploy.push(`the release needs Secret ${name} (${why}), but the demo cluster holds no such SOPS Secret (SOPS_SECRETS in scripts/flux/lib.ts)`);
    } else if (content === undefined) {
      deploy.push(`${app}/${file} is not on main, and the release needs Secret ${name} (${why}): create it as deploy/flux/README.md 「展示叢集」 describes, list it in kustomization.yaml and merge it first`);
    } else {
      deploy.push(...sopsFileProblems("demo", file, content));
      if (!listed.includes(file)) deploy.push(`${app}/kustomization.yaml does not list ${file}, so Flux does not apply Secret ${name} (${why})`);
    }
  }

  // The demo cluster's recipient, which the sops-age Secret must match.
  const sopsText = readText(root, ".sops.yaml");
  if (sopsText === undefined) deploy.push(".sops.yaml is missing");
  else {
    try {
      const probe = `${app}/x.sops.yaml`;
      const rule = sopsRuleTexts(sopsText).find((r) => new RegExp(r.pathRegex).test(probe));
      const own = rule?.recipients.filter((r) => r.role === "cluster: demo").map((r) => r.key) ?? [];
      if (own.length === 1 && own[0] !== undefined) inputs.demoRecipient = own[0];
      else deploy.push(`.sops.yaml's rule for ${app} lists ${own.length} keys with the role "cluster: demo", not exactly one`);
    } catch (error) {
      deploy.push(errorMessage(error));
    }
  }
  return inputs;
}

/** Flux's manifests from the checkout, built with the pinned kubectl (deploy/flux/README.md, 「建立叢集」 step 6). */
export async function kustomizeFluxSystem(root: string): Promise<string> {
  const kubectl = await protoTool("kubectl");
  const result = await tryRun([kubectl, "kustomize", join(root, FLUX_SYSTEM)]);
  // kustomize names absolute paths of this workstation: redact() shortens them.
  if (result.exitCode !== 0) throw new ReleaseError(redact(lastLines(result.stderr)));
  return result.stdout;
}

// --- What the cluster has --------------------------------------------------------------------------

export interface SecretFacts {
  name: string;
  type: string;
  /** MARKER's value, "" without it. */
  marker: string;
  keys: string[];
  /** The length of each key's base64 value (never the value): an empty key is 0. */
  sizes: Record<string, number>;
}

export interface NamespaceState {
  uid: string;
  resourceVersion: string;
  labels: Record<string, string>;
  annotations: Record<string, string>;
  terminating: boolean;
  objects: ObjectFact[];
  secrets: SecretFacts[];
}

/** The deploy key Secret as it can be read without its private half. */
export interface DeployKeyFacts {
  publicKey: string;
  knownHostsSha256: string;
}

export interface ClusterFacts {
  namespace?: NamespaceState;
  /** Flux toolkit CRDs that exist. */
  fluxCrds: string[];
  /** ClusterRoles and ClusterRoleBindings labeled as Flux's (`app.kubernetes.io/part-of=flux`). */
  fluxClusterObjects: string[];
  /** The root Kustomization flux-system, when it exists: then Flux manages its own objects. */
  rootKustomization?: { suspended: boolean; prune: boolean };
  /** The GitRepository flux-system, when it exists. */
  gitRepository?: { ref: GitRef; suspended: boolean };
  /** HelmRelease trustiot/trustiot, when it exists. */
  helmRelease?: { suspended: boolean };
  /** PoC workloads still there (`<namespace>/<kind>/<name>`). */
  poc: string[];
  /** Namespace trustiot, when it exists. */
  appNamespace?: { labels: Record<string, string>; terminating: boolean };
  missingPlatformCrds: string[];
  deployKey?: DeployKeyFacts;
  /** The ranges of the ServiceCIDR `kubernetes` (networking.k8s.io/v1, the API server's own); undefined when it could not be read. */
  serviceCidrs?: string[];
  /** Every Service with its ClusterIPs. */
  services: ServiceFacts[];
}

export interface ServiceFacts {
  /** `<namespace>/<name>` */
  ref: string;
  ips: string[];
  /** The Helm release that made it (`<namespace>/<name>` from its meta.helm.sh annotations), or "/". */
  release: string;
}

/** One line per Service: `<namespace>/<name>\t<ClusterIPs>\t<release namespace>/<release name>`; parseServices reads it. */
export const SERVICES_JSONPATH =
  'jsonpath={range .items[*]}{.metadata.namespace}/{.metadata.name}{"\\t"}{.spec.clusterIPs[*]}{"\\t"}{.metadata.annotations.meta\\.helm\\.sh/release-namespace}/{.metadata.annotations.meta\\.helm\\.sh/release-name}{"\\n"}{end}';

export function parseServices(out: string): ServiceFacts[] {
  return out
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line) => {
      const [ref = "", ips = "", release = ""] = line.split("\t");
      if (!/^[^/\s]+\/[^/\s]+$/.test(ref)) throw new ReleaseError(`unexpected Service line: ${redact(line).slice(0, 120)}`);
      return { ref, ips: ips.split(/\s+/).filter((ip) => ip !== ""), release: release.trim() };
    });
}

/**
 * Why the ClusterIP that main pins cannot be the ingress Service's on this cluster; [] when it
 * can. The address must be in the static band of the cluster's one IPv4 Service range (the
 * ServiceCIDR `kubernetes`): Kubernetes hands out dynamic addresses from the rest first, so any
 * Service created before the ingress one could take an address outside the band, and the first
 * install would fail (deploy/chart/README.md 「固定的入口 ClusterIP」; the same rule as the smoke
 * deployment's, scripts/nightly/ingress-address.ts). It must not be held by another Service either.
 */
export function clusterIpProblems(ip: string, cluster: ClusterFacts): string[] {
  const value = `${INGRESS_CLUSTER_IP_PATH.join(".")} ${ip} of values-demo.yaml`;
  const how = "pin an address of the static band (deploy/chart/README.md 「固定的入口 ClusterIP」, Laysi/trustiot-v2#478)";
  const problems: string[] = [];
  const ipv4 = (cluster.serviceCidrs ?? []).filter((cidr) => cidr.includes("."));
  if (cluster.serviceCidrs === undefined) {
    problems.push(`could not read the cluster's Service range (ServiceCIDR kubernetes, networking.k8s.io/v1), so ${value} cannot be checked against its static band`);
  } else if (ipv4.length !== 1) {
    problems.push(`expected one IPv4 range in ServiceCIDR kubernetes, found ${JSON.stringify(cluster.serviceCidrs)}, so ${value} cannot be checked against its static band`);
  } else {
    const cidr = ipv4[0]!;
    let band: { first: string; last: string } | undefined;
    let error = "";
    try {
      band = staticBandOf(cidr);
    } catch (thrown) {
      error = errorMessage(thrown);
    }
    if (error !== "") problems.push(`the cluster's Service range is not an IPv4 CIDR (${error}), so ${value} cannot be checked against its static band`);
    else if (band === undefined) problems.push(`the cluster's Service range ${cidr} is too small to have a static band, so ${value} cannot be pinned safely`);
    else {
      const address = ipv4ToNumber(ip);
      const network = ipv4ToNumber(band.first) - 1;
      const broadcast = network + 2 ** (32 - Number(cidr.split("/")[1])) - 1;
      if (address === network) problems.push(`${value} is the network address of the Service range ${cidr}; ${how}`);
      else if (address === broadcast) problems.push(`${value} is the broadcast address of the Service range ${cidr}; ${how}`);
      else if (address < ipv4ToNumber(band.first) || address > ipv4ToNumber(band.last)) {
        problems.push(`${value} is outside the static band ${band.first}-${band.last} of the Service range ${cidr}: a Service created before the ingress one could take it, and the first install would fail; ${how}`);
      }
    }
  }
  for (const service of cluster.services) {
    if (!service.ips.includes(ip)) continue;
    // A run after an earlier install: the release's own Service holds it.
    if (service.ref.startsWith(`${DEMO_NAMESPACE}/`) && service.release === `${DEMO_NAMESPACE}/${RELEASE_NAME}`) continue;
    problems.push(`${value} is already the ClusterIP of Service ${redact(service.ref)}: the release could not create its ingress Service with it; pin another address (Laysi/trustiot-v2#478)`);
  }
  return problems;
}

/**
 * The go-template that lists the Secrets of a namespace: name, type, MARKER, and each key's name
 * with the length of its base64 value. Never a value: `$v` is bound by range and only measured.
 */
export const SECRET_TEMPLATE =
  `{{range .items}}{{.metadata.name}}{{"\\t"}}{{.type}}{{"\\t"}}{{with .metadata.labels}}{{index . "${MARKER}"}}{{end}}{{"\\t"}}{{range $k, $v := .data}}{{$k}}:{{len $v}},{{end}}{{"\\n"}}{{end}}`;

/** Parses SECRET_TEMPLATE's output (kubectl prints a missing map entry as `<no value>`); a size that does not parse is NaN. */
export function parseSecrets(out: string): SecretFacts[] {
  const clean = (field: string | undefined) => (field === undefined || field === "<no value>" ? "" : field);
  return out
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line) => {
      const [name, type, marker, keys] = line.split("\t");
      const sizes: Record<string, number> = {};
      for (const entry of clean(keys).split(",").filter((k) => k !== "")) {
        const colon = entry.lastIndexOf(":");
        const key = colon < 0 ? entry : entry.slice(0, colon);
        sizes[key] = colon < 0 || !/^\d+$/.test(entry.slice(colon + 1)) ? Number.NaN : Number(entry.slice(colon + 1));
      }
      return { name: clean(name), type: clean(type), marker: clean(marker), keys: Object.keys(sizes).sort(), sizes };
    })
    .sort((a, b) => a.name.localeCompare(b.name));
}

/** The SHA256 fingerprint of an OpenSSH public key line, as ssh-keygen -l and GitHub show it. */
export function sshFingerprint(publicKey: string): string {
  const [type = "", blob = ""] = publicKey.trim().split(/\s+/);
  const bytes = /^[A-Za-z0-9+/]+={0,2}$/.test(blob) ? Buffer.from(blob, "base64") : Buffer.alloc(0);
  // The key blob starts with its own type as an SSH string (RFC 4253, section 6.6).
  const named = bytes.length >= 4 ? bytes.subarray(4, 4 + bytes.readUInt32BE(0)).toString("latin1") : "";
  if (!/^(ssh|ecdsa|sk)-[a-z0-9@.-]+$/.test(type) || named !== type) throw new ReleaseError("not an OpenSSH public key");
  const digest = new Bun.CryptoHasher("sha256").update(bytes).digest("base64");
  return `SHA256:${digest.replace(/=+$/, "")}`;
}

// --- Verdicts and plans (no I/O) ---------------------------------------------------------------------

export type NamespaceVerdict =
  | { kind: "absent" }
  | { kind: "ours"; uid: string }
  | { kind: "age-key"; uid: string }
  | { kind: "foreign"; why: string[] };

/**
 * Who made the namespace flux-system: this script (MARKER=installed), only the age key step of
 * deploy/flux/README.md (「展示叢集的 age 金鑰」: no labels but the name, no annotations, and
 * nothing in it but sops-age, deploy-key's own Secret and what Kubernetes and Istio put into every
 * namespace), or something else, such as a lab e2e run or another Flux, which nothing here touches.
 */
export function namespaceVerdict(ns: NamespaceState | undefined, deployKeySecret: string, sopsSecret: string): NamespaceVerdict {
  if (ns === undefined) return { kind: "absent" };
  const why: string[] = [];
  if (ns.terminating) why.push("it is being deleted");
  if (ns.labels[RUN_LABEL] !== undefined) why.push(`it carries ${RUN_LABEL} (a lab e2e run made it)`);
  if (ns.labels[MARKER] === MARKED_INSTALL) return why.length === 0 ? { kind: "ours", uid: ns.uid } : { kind: "foreign", why };
  if (ns.labels[MARKER] !== undefined) why.push(`it carries ${MARKER}=${redact(ns.labels[MARKER] ?? "")}`);
  const labels = Object.keys(ns.labels).filter((key) => key !== "kubernetes.io/metadata.name" && key !== RUN_LABEL);
  if (labels.length > 0) why.push(`it has labels ${names(labels.sort())}`);
  if (Object.keys(ns.annotations).length > 0) why.push(`it has annotations ${names(Object.keys(ns.annotations).sort())}`);
  const ourKey = ns.secrets.find((s) => s.name === deployKeySecret)?.marker === MARKED_DEPLOY_KEY;
  const allowed = new Set<string>([...NAMESPACE_DEFAULTS, `secret/${sopsSecret}`, ...(ourKey ? [`secret/${deployKeySecret}`] : [])]);
  for (const object of ns.objects) {
    if (object.managed) why.push(`Helm or Flux manages ${redact(object.ref)} in it`);
    else if (!object.owned && !allowed.has(object.ref)) why.push(`it holds ${redact(object.ref)}`);
  }
  return why.length === 0 ? { kind: "age-key", uid: ns.uid } : { kind: "foreign", why };
}

/** Why a run must wait for the lab lock: free and this issue's are fine for a dry run, --apply takes it. */
export function lockProblem(state: LockState): string | undefined {
  if (state.kind === "free") return undefined;
  if (state.kind === "unreadable") return `the lab lock ~/${LOCK_DIR} is there but its owner cannot be read (a run may be taking it right now, or one stopped while taking it)`;
  return ownsLock(state.owner, ISSUE) ? undefined : `the lab lock is held by: ${redact(state.owner)}; this script runs only while it is free or held for ${ISSUE}`;
}

/** The sops-age Secret as the check on the demo machine found it (ageCheckScript). */
export type SopsAgeState =
  | { kind: "ok"; recipients: string[] }
  | { kind: "missing" | "empty" | "invalid" | "tool-mismatch" | "not-checked" }
  | { kind: "keys"; keys: string[] }
  | { kind: "arch"; arch: string }
  | { kind: "unreadable"; detail: string };

/** Why the sops-age Secret cannot decrypt the demo's SOPS files; undefined when it can. */
export function sopsAgeProblem(state: SopsAgeState, recipient: string | undefined, secret: string): string | undefined {
  const step = "deploy/flux/README.md 「展示叢集的 age 金鑰」";
  switch (state.kind) {
    case "ok":
      if (recipient === undefined) return undefined; // .sops.yaml's problem is reported on its own
      if (state.recipients.length === 1 && state.recipients[0] === recipient) return undefined;
      return `the key in Secret ${FLUX_NAMESPACE}/${secret} is for ${state.recipients.join(", ") || "nothing"}, not the demo cluster's recipient ${recipient} in .sops.yaml: Flux could not decrypt the demo's SOPS files (${step})`;
    case "missing":
      return `Secret ${FLUX_NAMESPACE}/${secret} does not exist: make it first (${step})`;
    case "keys":
      return `Secret ${FLUX_NAMESPACE}/${secret} has the keys ${names(state.keys) || "(none)"}, not exactly ${SOPS_AGE_KEY} (${step})`;
    case "empty":
      return `Secret ${FLUX_NAMESPACE}/${secret} holds an empty ${SOPS_AGE_KEY} (${step})`;
    case "invalid":
      return `Secret ${FLUX_NAMESPACE}/${secret} does not hold an age identity that age-keygen -y reads (${step})`;
    case "tool-mismatch":
      return "age-keygen arrived changed on the demo machine; nothing was checked";
    case "arch":
      return `the demo machine is ${state.arch}, not aarch64; age-keygen was built for linux/arm64`;
    case "not-checked":
      return undefined;
    case "unreadable":
      return `could not check Secret ${FLUX_NAMESPACE}/${secret}: ${state.detail}`;
  }
}

/**
 * Why the deploy key Secret that deploy-key made is incomplete: other keys than DEPLOY_KEY_KEYS, or
 * one of them empty (a run that stopped while making it). undefined when it is complete.
 */
function incompleteDeployKey(found: SecretFacts, secret: string): string | undefined {
  const again = `delete it (\`sudo k3s kubectl -n ${FLUX_NAMESPACE} delete secret ${secret}\`) and run deploy-key again`;
  if (stableJson(found.keys) !== stableJson([...DEPLOY_KEY_KEYS].sort())) return `Secret ${FLUX_NAMESPACE}/${secret} has the keys ${names(found.keys)}, not ${DEPLOY_KEY_KEYS.join(", ")}: ${again}`;
  const empty = DEPLOY_KEY_KEYS.filter((key) => !((found.sizes[key] ?? 0) > 0));
  if (empty.length > 0) return `Secret ${FLUX_NAMESPACE}/${secret} has an empty ${empty.join(", ")} (a deploy-key run stopped while making it): ${again}`;
  return undefined;
}

/** Why the deploy key Secret cannot serve the GitRepository; undefined when it can. */
export function deployKeyProblem(ns: NamespaceState | undefined, key: DeployKeyFacts | undefined, secret: string, knownHosts: string): string | undefined {
  const found = ns?.secrets.find((s) => s.name === secret);
  if (found === undefined) return `Secret ${FLUX_NAMESPACE}/${secret} (the deploy key) does not exist: run \`bun scripts/demo/flux.ts deploy-key\` first and add its public key on GitHub as a read-only deploy key`;
  if (found.marker !== MARKED_DEPLOY_KEY) return `Secret ${FLUX_NAMESPACE}/${secret} was not made by deploy-key (no ${MARKER}=${MARKED_DEPLOY_KEY}); nothing here replaces it`;
  const incomplete = incompleteDeployKey(found, secret);
  if (incomplete !== undefined) return incomplete;
  if (key === undefined) return `could not read the public half of Secret ${FLUX_NAMESPACE}/${secret}`;
  if (key.knownHostsSha256 !== sha256(knownHosts)) {
    return `the known_hosts in Secret ${FLUX_NAMESPACE}/${secret} differs from main's ${KNOWN_HOSTS_FILE}: delete the Secret and its key on GitHub, then run deploy-key again`;
  }
  return undefined;
}

/**
 * How install keeps Flux at commits it checked until the first deploy is verified
 * (pinnedBootstrap). `from` is a commit an earlier run pinned the GitRepository to that is not
 * this run's head (main moved on since): install pins head, which it checked, before anything
 * else. `step`:
 * - apply: the bootstrap apply creates the GitRepository pinned to head;
 * - finish: an earlier run stopped after it created the root Kustomization, with the GitRepository
 *   still pinned: install makes sure the pin is FIELD_MANAGER's too, waits for the first deploy at
 *   head and removes the pin;
 * - none: Flux follows main already (an install finished before); install only checks.
 */
export interface PinPlan {
  step: "apply" | "finish" | "none";
  from?: string;
}

export interface InstallPlan {
  head: string;
  chart?: { version: string; digest: string };
  clusterIp?: string;
  manifestsSha256: string;
  /** What the bootstrap apply applies (pinnedBootstrap: its text and the root Kustomization), "" when it applies nothing. */
  bootstrapSha256: string;
  namespace: "age-key" | "ours" | "unknown";
  namespaceUid: string;
  /** Label flux-system with MARKER=installed (the age key step's namespace). */
  mark: boolean;
  /** Apply Flux's manifests: the root Kustomization does not exist yet, so nothing of Flux manages them. */
  apply: boolean;
  pin: PinPlan;
  deployKeyFingerprint: string;
  notes: string[];
  refusals: string[];
}

/** Where the README says how to resume Flux by hand, after an interrupted remove among others. */
const RESUME = "deploy/flux/README.md 「在展示機安裝 Flux」 (中斷之後)";

/**
 * Why Flux would not follow main or deploy the release even when everything is Ready: the root
 * Kustomization suspended or without prune (what an interrupted remove leaves), the GitRepository
 * or the HelmRelease suspended. install refuses to start, and to report done, with any of them.
 */
export function suspendedProblems(cluster: ClusterFacts): string[] {
  const problems: string[] = [];
  const root = cluster.rootKustomization;
  if (root !== undefined && (root.suspended || !root.prune)) {
    const state = [root.suspended ? "spec.suspend: true" : "", root.prune ? "" : "spec.prune: false"].filter((part) => part !== "").join(", ");
    problems.push(
      `the root Kustomization ${FLUX_NAMESPACE} has ${state}, as a remove that stopped halfway leaves it, so Flux does not apply main: run remove again (dry run first) to finish it, or resume Flux as ${RESUME} describes`,
    );
  }
  if (cluster.gitRepository?.suspended === true) {
    problems.push(`GitRepository ${FLUX_NAMESPACE}/${FLUX_NAMESPACE} is suspended, so Flux fetches nothing from main: resume it as ${RESUME} describes`);
  }
  if (cluster.helmRelease?.suspended === true) {
    problems.push(`HelmRelease ${DEMO_NAMESPACE}/${RELEASE_NAME} is suspended, so Flux does not deploy the release: resume it as ${RESUME} describes`);
  }
  return problems;
}

/**
 * The pin step for this cluster (PinPlan), or why install cannot tell what to do with the
 * GitRepository. `apply` is whether the root Kustomization is missing.
 */
function planPin(cluster: ClusterFacts, head: string, apply: boolean): { pin: PinPlan; refusal?: string } {
  const git = cluster.gitRepository;
  const pin: PinPlan = { step: apply ? "apply" : "none" };
  if (git === undefined) {
    if (apply) return { pin };
    return { pin, refusal: `the root Kustomization ${FLUX_NAMESPACE} exists but GitRepository ${FLUX_NAMESPACE}/${FLUX_NAMESPACE} does not: run remove (dry run first), then install again` };
  }
  if (git.ref.kind === "other") {
    return { pin, refusal: `GitRepository ${FLUX_NAMESPACE}/${FLUX_NAMESPACE} has spec.ref ${git.ref.ref}: neither main nor a pin of an earlier install, so this script does not know what Flux applies; look at it by hand` };
  }
  if (git.ref.kind === "pinned") {
    if (!apply) pin.step = "finish";
    if (git.ref.commit !== head) pin.from = git.ref.commit;
  }
  return { pin };
}

/** Everything install checks, in one plan; it runs only without refusals. */
export function planInstall(repo: RepoInputs, cluster: ClusterFacts, lock: LockState, sopsAge: SopsAgeState): InstallPlan {
  const refusals = [...repo.code, ...repo.flux, ...repo.deploy];
  const notes: string[] = [];
  const lockIssue = lockProblem(lock);
  if (lockIssue !== undefined) refusals.push(lockIssue);
  const deployKeySecret = repo.manifests?.deployKeySecret ?? FLUX_NAMESPACE;
  const sopsSecret = repo.manifests?.sopsSecret ?? "sops-age";
  const verdict = namespaceVerdict(cluster.namespace, deployKeySecret, sopsSecret);
  if (verdict.kind === "absent") {
    refusals.push(`namespace ${FLUX_NAMESPACE} does not exist: make it with Secret ${sopsSecret} first (deploy/flux/README.md 「展示叢集的 age 金鑰」), then run deploy-key`);
  } else if (verdict.kind === "foreign") {
    refusals.push(`namespace ${FLUX_NAMESPACE} was made neither by this script nor by the age key step, so nothing here touches it: ${verdict.why.join("; ")}`);
  }
  if (verdict.kind !== "ours") {
    if (cluster.fluxCrds.length > 0) refusals.push(`Flux CRDs exist (${names(cluster.fluxCrds)}) but ${FLUX_NAMESPACE} is not this script's install: another Flux runs here, or one was left behind`);
    if (cluster.fluxClusterObjects.length > 0) refusals.push(`Flux's cluster-wide objects exist (${names(cluster.fluxClusterObjects)}) but ${FLUX_NAMESPACE} is not this script's install`);
  }
  if (verdict.kind === "age-key" || verdict.kind === "ours") {
    const sopsIssue = sopsAgeProblem(sopsAge, repo.demoRecipient, sopsSecret);
    if (sopsIssue !== undefined) refusals.push(sopsIssue);
    const keyIssue = deployKeyProblem(cluster.namespace, cluster.deployKey, deployKeySecret, repo.knownHosts);
    if (keyIssue !== undefined) refusals.push(keyIssue);
  }
  if (cluster.poc.length > 0) refusals.push(`the feasibility PoC is still there (${cluster.poc.join(", ")}): run scripts/demo/cleanup.ts first`);
  if (cluster.appNamespace !== undefined) {
    const labels = cluster.appNamespace.labels;
    if (cluster.appNamespace.terminating) refusals.push(`namespace ${DEMO_NAMESPACE} is being deleted; wait until it is gone`);
    else if (labels[KUSTOMIZE_LABEL_NAME] !== FLUX_NAMESPACE || labels[KUSTOMIZE_LABEL_NAMESPACE] !== FLUX_NAMESPACE) {
      refusals.push(`namespace ${DEMO_NAMESPACE} exists and is not Flux's: run scripts/demo/cleanup.ts, or remove what is in it, first`);
    } else {
      // Expected once Flux runs; before that, it is what an earlier remove left.
      if (cluster.rootKustomization === undefined) {
        notes.push(`namespace ${DEMO_NAMESPACE} is left from an earlier install of this script: Helm takes over the objects it kept there (Postgres, its Secrets and the volumes), which is not verified on a cluster yet`);
      }
    }
  }
  refusals.push(...suspendedProblems(cluster));
  const apply = cluster.rootKustomization === undefined;
  const { pin, refusal: pinIssue } = planPin(cluster, repo.head, apply);
  if (pinIssue !== undefined) refusals.push(pinIssue);
  let bootstrapSha256 = "";
  if (apply && repo.manifests !== undefined && /^[0-9a-f]{40}$/.test(repo.head)) {
    try {
      const bootstrap = pinnedBootstrap(repo.manifests, repo.head);
      bootstrapSha256 = sha256(`${bootstrap.text}\n---\n${bootstrap.root}`);
    } catch (error) {
      refusals.push(errorMessage(error));
    }
  }
  if (repo.clusterIp !== undefined) refusals.push(...clusterIpProblems(repo.clusterIp, cluster));
  if (cluster.missingPlatformCrds.length > 0) {
    refusals.push(`the platform CRDs ${cluster.missingPlatformCrds.join(", ")} are missing: install Istio and CloudNativePG first (deploy/istio, deploy/cloudnative-pg, scripts/demo/platform.ts)`);
  }
  let fingerprint = "";
  try {
    if (cluster.deployKey !== undefined) fingerprint = sshFingerprint(cluster.deployKey.publicKey);
  } catch (error) {
    refusals.push(`the deploy key's public half is not an OpenSSH key: ${errorMessage(error)}`);
  }
  return {
    head: repo.head,
    ...(repo.chart === undefined ? {} : { chart: repo.chart }),
    ...(repo.clusterIp === undefined ? {} : { clusterIp: repo.clusterIp }),
    manifestsSha256: repo.manifests?.sha256 ?? "",
    bootstrapSha256,
    namespace: verdict.kind === "ours" || verdict.kind === "age-key" ? verdict.kind : "unknown",
    namespaceUid: verdict.kind === "ours" || verdict.kind === "age-key" ? verdict.uid : "",
    mark: verdict.kind === "age-key",
    apply,
    pin,
    deployKeyFingerprint: fingerprint,
    notes,
    refusals,
  };
}

/** The part of the plan that the confirmation code names. */
/**
 * What --confirm binds and the plan under the lock must equal: every field of the plan but its
 * refusals, so the pin step and the commit it moves from (`pin`), the bootstrap text with the pin
 * (`bootstrapSha256`), the label and the apply all change the code when the cluster changed.
 */
function installChange(plan: InstallPlan): unknown {
  const { refusals: _refusals, ...change } = plan;
  return change;
}

export interface DeployKeyPlan {
  namespaceUid: string;
  knownHostsSha256: string;
  secret: string;
  /** Create the Secret; false when deploy-key made it already (its public key is printed again). */
  create: boolean;
  existing?: { publicKey: string; fingerprint: string };
  refusals: string[];
}

export function planDeployKey(repo: RepoInputs, cluster: ClusterFacts, lock: LockState): DeployKeyPlan {
  const refusals = [...repo.code, ...repo.flux];
  const lockIssue = lockProblem(lock);
  if (lockIssue !== undefined) refusals.push(lockIssue);
  const secret = repo.manifests?.deployKeySecret ?? FLUX_NAMESPACE;
  const verdict = namespaceVerdict(cluster.namespace, secret, repo.manifests?.sopsSecret ?? "sops-age");
  if (verdict.kind === "absent") refusals.push(`namespace ${FLUX_NAMESPACE} does not exist: make it with the age key first (deploy/flux/README.md 「展示叢集的 age 金鑰」)`);
  if (verdict.kind === "foreign") refusals.push(`namespace ${FLUX_NAMESPACE} was made neither by this script nor by the age key step: ${verdict.why.join("; ")}`);
  const found = cluster.namespace?.secrets.find((s) => s.name === secret);
  let existing: DeployKeyPlan["existing"];
  if (found !== undefined) {
    const incomplete = found.marker === MARKED_DEPLOY_KEY ? incompleteDeployKey(found, secret) : undefined;
    if (found.marker !== MARKED_DEPLOY_KEY) refusals.push(`Secret ${FLUX_NAMESPACE}/${secret} exists and was not made by deploy-key; nothing here replaces it`);
    else if (incomplete !== undefined) refusals.push(incomplete);
    else if (cluster.deployKey !== undefined) {
      try {
        existing = { publicKey: cluster.deployKey.publicKey.trim(), fingerprint: sshFingerprint(cluster.deployKey.publicKey) };
      } catch (error) {
        refusals.push(`Secret ${FLUX_NAMESPACE}/${secret} holds no OpenSSH public key: ${errorMessage(error)}`);
      }
    } else refusals.push(`could not read the public half of Secret ${FLUX_NAMESPACE}/${secret}`);
  }
  return {
    namespaceUid: verdict.kind === "ours" || verdict.kind === "age-key" ? verdict.uid : "",
    knownHostsSha256: sha256(repo.knownHosts),
    secret,
    create: found === undefined,
    ...(existing === undefined ? {} : { existing }),
    refusals,
  };
}

export interface RemovePlan {
  head: string;
  manifestsSha256: string;
  namespaceUid: string;
  /** False when there is no Flux of this script's to remove. */
  remove: boolean;
  /** The Flux objects in the cluster when the plan was made, `<Kind>/<namespace>/<name>`. */
  fluxObjects: string[];
  refusals: string[];
}

/** A Flux custom object in the cluster and the Kustomization that applied it, if any. */
export interface FluxObject {
  /** `<Kind>/<namespace>/<name>`. */
  ref: string;
  /** `<namespace>/<name>` from kustomize-controller's labels, or "" when no Kustomization applied it. */
  appliedBy: string;
}

/** The labels kustomize-controller puts on every object a Kustomization applies. */
export const KUSTOMIZE_LABEL_NAME = "kustomize.toolkit.fluxcd.io/name";
export const KUSTOMIZE_LABEL_NAMESPACE = "kustomize.toolkit.fluxcd.io/namespace";

/** Parses boxOps' fluxObjects lines: `<Kind>/<namespace>/<name>\t<namespace>/<name>` (the second part from the labels). */
export function parseFluxObjects(out: string): FluxObject[] {
  return out
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line) => {
      const [ref = "", owner = ""] = line.trim().split("\t");
      if (!/^[A-Za-z]+\/[^/\s]+\/[^/\s]+$/.test(ref)) throw new ReleaseError(`unexpected Flux object line: ${redact(line).slice(0, 120)}`);
      return { ref, appliedBy: /^[^/\s]+\/[^/\s]+$/.test(owner) ? owner : "" };
    })
    .sort((a, b) => a.ref.localeCompare(b.ref));
}

/** The root sync objects: in the demo tree too, but there before Flux has reconciled and labeled them. */
export const ROOT_SYNC_OBJECTS = [`GitRepository/${FLUX_NAMESPACE}/${FLUX_NAMESPACE}`, `Kustomization/${FLUX_NAMESPACE}/${FLUX_NAMESPACE}`] as const;

/**
 * Flux objects that remove must not delete: neither the root sync objects nor applied by the root
 * Kustomization from main's demo tree. Someone else's HelmRelease, made by hand or by another
 * Kustomization, is refused instead of being uninstalled, and so is any object that deleting
 * Flux's CRDs would take with it.
 */
export function foreignFluxObjects(objects: readonly FluxObject[]): string[] {
  const root = `${FLUX_NAMESPACE}/${FLUX_NAMESPACE}`;
  return objects.filter((o) => !(ROOT_SYNC_OBJECTS as readonly string[]).includes(o.ref) && o.appliedBy !== root).map((o) => o.ref);
}

/**
 * Objects in this script's flux-system that deleting the namespace would take with it but that
 * are not part of the install: anything besides the Kubernetes defaults, the two key Secrets,
 * Flux's own objects from main's manifests and the controllers' leader election Leases. Objects
 * that another object owns (Pods, ReplicaSets, service account tokens) go with their owners.
 */
export function unexpectedNamespaceObjects(ns: NamespaceState, manifests: FluxManifests): string[] {
  const keySecret = ns.secrets.find((s) => s.name === manifests.deployKeySecret);
  const allowed = new Set<string>([
    ...NAMESPACE_DEFAULTS,
    `secret/${manifests.sopsSecret}`,
    ...(keySecret?.marker === MARKED_DEPLOY_KEY ? [`secret/${manifests.deployKeySecret}`] : []),
    ...manifests.namespaceObjects,
    // controller-runtime's leader election, `<controller>-leader-election` in each Flux controller.
    ...manifests.deployments.map((name) => `lease.coordination.k8s.io/${name}-leader-election`),
  ]);
  return ns.objects.filter((o) => !o.owned && !allowed.has(o.ref)).map((o) => o.ref);
}

/** Flux CRDs and Flux-labeled ClusterRoles and ClusterRoleBindings on the cluster that main's manifests do not define. */
export function unexpectedClusterObjects(cluster: ClusterFacts, manifests: FluxManifests): string[] {
  return [...cluster.fluxCrds.filter((crd) => !manifests.crds.includes(crd)), ...cluster.fluxClusterObjects.filter((object) => !manifests.clusterObjects.includes(object))].sort();
}

export function planRemove(repo: RepoInputs, cluster: ClusterFacts, lock: LockState, objects: readonly FluxObject[], suspendedReleases: string[]): RemovePlan {
  const fluxObjects = objects.map((o) => o.ref);
  const refusals = [...repo.code, ...repo.flux];
  const lockIssue = lockProblem(lock);
  if (lockIssue !== undefined) refusals.push(lockIssue);
  const verdict = namespaceVerdict(cluster.namespace, repo.manifests?.deployKeySecret ?? FLUX_NAMESPACE, repo.manifests?.sopsSecret ?? "sops-age");
  let remove = false;
  if (verdict.kind === "ours") {
    remove = true;
    const extra = cluster.namespace !== undefined && repo.manifests !== undefined ? unexpectedNamespaceObjects(cluster.namespace, repo.manifests) : [];
    if (extra.length > 0) {
      refusals.push(`namespace ${FLUX_NAMESPACE} holds objects that are not part of this install (${names(extra)}): deleting the namespace would delete them too; move or delete them by hand first`);
    }
    const leftOver = repo.manifests === undefined ? [] : unexpectedClusterObjects(cluster, repo.manifests);
    if (leftOver.length > 0) {
      refusals.push(
        `Flux's CRDs or cluster-wide objects that main's manifests do not define exist (${names(leftOver)}): remove deletes only what main defines, so it would stop halfway with them left; delete them by hand first`,
      );
    }
  } else if (verdict.kind === "foreign") refusals.push(`namespace ${FLUX_NAMESPACE} is not this script's install, so remove leaves it alone: ${verdict.why.join("; ")}`);
  else if (cluster.fluxCrds.length > 0 || cluster.fluxClusterObjects.length > 0) {
    refusals.push(`Flux's CRDs or cluster-wide objects exist (${names([...cluster.fluxCrds, ...cluster.fluxClusterObjects])}) without this script's ${FLUX_NAMESPACE}; remove them by hand`);
  }
  for (const release of suspendedReleases) {
    refusals.push(`HelmRelease ${redact(release)} is suspended: deleting it would leave its Helm release installed with nothing managing it; resume it first (spec.suspend false)`);
  }
  const known = repo.manifests?.crdKinds ?? {};
  const unknown = fluxObjects.filter((ref) => !Object.hasOwn(known, ref.split("/")[0] ?? ""));
  if (repo.manifests !== undefined && unknown.length > 0) {
    refusals.push(`Flux objects of kinds that main's Flux does not define exist (${names(unknown)}); delete them by hand first`);
  }
  const foreign = foreignFluxObjects(objects).filter((ref) => !unknown.includes(ref));
  if (foreign.length > 0) {
    refusals.push(
      `Flux objects that the root Kustomization ${FLUX_NAMESPACE} did not apply from main exist (${names(foreign)}): remove would delete them, and deleting Flux's CRDs takes them along; delete or move them by hand first`,
    );
  }
  return {
    head: repo.head,
    manifestsSha256: repo.manifests?.sha256 ?? "",
    namespaceUid: verdict.kind === "ours" ? verdict.uid : "",
    remove,
    fluxObjects: [...fluxObjects].sort(),
    refusals,
  };
}

function removeChange(plan: RemovePlan): unknown {
  const { refusals: _refusals, ...change } = plan;
  return change;
}

// --- Scripts that run as root on the demo machine -------------------------------------------------

/** The file every root script holds with flock(1) while it runs (scriptStart). */
export const REMOTE_LOCK = `${REMOTE_BASE}/.lock`;

/** Removes a run directory's files with shred(1), then the directory. */
const SHRED_DIR = (dir: string) => `find "${dir}" -type f -exec shred -u {} + 2>/dev/null || true; rm -rf "${dir}"`;

/**
 * The start of every root script: strict mode; flock on REMOTE_LOCK, so that root scripts of this
 * file never run side by side (two --lock-held runs, or a run beside one whose ssh dropped; the
 * lock goes when the script and the commands it started have ended); every run directory a killed
 * run left behind shredded (only a running script uses one, and none runs); a root-only directory
 * of its own under `base` (removed on exit, also after SIGHUP when ssh drops); stdin saved into
 * `$d/<file>` and checked against `sha`. Prints `STALE ...` for each directory it removed.
 * Commands are written for bash (Fedora's /bin/bash on the demo machine).
 */
function scriptStart(base: string, prefix: string, file: string, sha: string, cleanup = 'rm -rf "$d"'): string[] {
  if (!/^[0-9a-f]{64}$/.test(sha)) throw new ReleaseError("a staged file needs its sha256");
  if (!/^\/[A-Za-z0-9/_-]+$/.test(base)) throw new ReleaseError("the root scripts' base directory must be a plain absolute path");
  return [
    "set -eu",
    "set -o pipefail",
    "umask 077",
    `base=${shq(base)}`,
    'mkdir -p -m 0700 "$base"',
    'chmod 0700 "$base"',
    'exec 9>"$base/.lock"',
    'flock -n 9 || { echo "another root script of scripts/demo/flux.ts is running on the demo machine; wait until it ends" >&2; exit 1; }',
    'for old in "$base"/*.*; do',
    '  [ -d "$old" ] || continue',
    `  ${SHRED_DIR("$old")}`,
    '  echo "STALE removed a run directory that a stopped run left behind"',
    "done",
    `d=$(mktemp -d "$base/${prefix}.XXXXXXXX")`,
    `cleanup() { ${cleanup}; }`,
    "trap cleanup EXIT",
    "trap 'exit 129' HUP INT TERM",
    `cat > "$d/${file}"`,
    `[ "$(sha256sum "$d/${file}" | cut -d' ' -f1)" = ${sha} ] || { echo "${file} arrived changed on the demo machine" >&2; exit 1; }`,
  ];
}

function requireObjectName(name: string): void {
  if (!isObjectName(name)) throw new ReleaseError(`${JSON.stringify(name).slice(0, 80)} is not a Kubernetes object name`);
}

/**
 * Checks the sops-age Secret with age-keygen (stdin): the private key goes from kubectl through
 * base64 -d into `age-keygen -y` by pipes only, and only the public keys it derives are printed.
 * age-keygen's errors are dropped, since a parse error could quote its input. Prints one line,
 * `SOPS_AGE <state> [...]`.
 */
export function ageCheckScript(base: string, toolSha256: string, secret: string): string {
  requireObjectName(secret);
  const get = `k3s kubectl -n ${FLUX_NAMESPACE} get secret ${shq(secret)}`;
  const value =`${get} -o 'jsonpath={.data.${SOPS_AGE_KEY.replaceAll(".", "\\.")}}' | base64 -d`;
  return [
    ...scriptStart(base, "age", "age-keygen", toolSha256),
    'arch=$(uname -m)',
    'if [ "$arch" != aarch64 ]; then echo "SOPS_AGE arch $arch"; exit 0; fi',
    'chmod 0700 "$d/age-keygen"',
    `keys=$(${get} --ignore-not-found -o 'go-template=FOUND{{range $k, $v := .data}} {{$k}}{{end}}')`,
    'case "$keys" in',
    '  "") echo "SOPS_AGE missing"; exit 0 ;;',
    `  "FOUND ${SOPS_AGE_KEY}") ;;`,
    '  *) echo "SOPS_AGE keys${keys#FOUND}"; exit 0 ;;',
    "esac",
    `size=$(${value} | wc -c)`,
    'if [ "$size" -eq 0 ]; then echo "SOPS_AGE empty"; exit 0; fi',
    `if ! pub=$(${value} | "$d/age-keygen" -y 2>/dev/null); then echo "SOPS_AGE invalid"; exit 0; fi`,
    // grep finds nothing for a file without identities; that is "invalid", not a failed script.
    "pub=$(printf '%s\\n' \"$pub\" | grep -E '^age1[0-9a-z]+$' | tr '\\n' ' ' || true)",
    'if [ -z "$pub" ]; then echo "SOPS_AGE invalid"; exit 0; fi',
    'echo "SOPS_AGE ok ${pub% }"',
  ].join("\n");
}

export function parseAgeCheck(stdout: string): SopsAgeState {
  const line = stdout.split("\n").find((l) => l.startsWith("SOPS_AGE ")) ?? "";
  const [, state = "", ...rest] = line.trim().split(/\s+/);
  switch (state) {
    case "ok":
      return rest.every((key) => /^age1[0-9a-z]+$/.test(key)) && rest.length > 0 ? { kind: "ok", recipients: rest } : { kind: "unreadable", detail: "unexpected output" };
    case "missing":
    case "empty":
    case "invalid":
    case "tool-mismatch":
      return { kind: state };
    case "keys":
      return { kind: "keys", keys: rest };
    case "arch":
      return { kind: "arch", arch: rest.join(" ") };
    default:
      return { kind: "unreadable", detail: "unexpected output" };
  }
}

/**
 * Generates the deploy key and stores it (stdin: known_hosts). The private key exists only as a
 * file in the run's root-only directory, which is shredded on exit; it reaches kubectl through
 * bash's builtin printf and a pipe, never through a command line. The three values are read into
 * variables first (an assignment fails the script under set -e, a command substitution inside
 * printf's arguments would not) and must not be empty. The Secret is read back and compared with
 * the files; when anything fails after kubectl created it, the exit trap deletes it again, so no
 * half-made deploy key stays (deployKeyProblem refuses an empty key too). kubectl's errors are
 * passed on without lines that look like key material. Prints `DEPLOY_KEY exists`, or
 * `PUBLIC <key>` and `DEPLOY_KEY created`.
 */
export function deployKeyScript(base: string, knownHostsSha256: string, secret: string): string {
  requireObjectName(secret);
  const get = `k3s kubectl -n ${FLUX_NAMESPACE} get secret ${shq(secret)}`;
  const undo = `if [ "\${created:-0}" = 1 ] && [ "\${verified:-0}" != 1 ]; then k3s kubectl -n ${FLUX_NAMESPACE} delete secret ${shq(secret)} --ignore-not-found >/dev/null 2>&1 && echo "deleted the incomplete Secret ${FLUX_NAMESPACE}/${secret}" >&2 || echo "could not delete the incomplete Secret ${FLUX_NAMESPACE}/${secret}: delete it by hand" >&2; fi`;
  return [
    ...scriptStart(base, "deploy-key", "known_hosts", knownHostsSha256, `${undo}; ${SHRED_DIR("$d")}`),
    `if [ -n "$(${get} --ignore-not-found -o name)" ]; then echo "DEPLOY_KEY exists"; exit 0; fi`,
    `ssh-keygen -q -t ed25519 -N '' -C ${DEPLOY_KEY_COMMENT} -f "$d/key" </dev/null >/dev/null`,
    'identity=$(base64 -w0 "$d/key")',
    'public=$(base64 -w0 "$d/key.pub")',
    'hosts=$(base64 -w0 "$d/known_hosts")',
    '[ -n "$identity" ] && [ -n "$public" ] && [ -n "$hosts" ] || { echo "the deploy key files are empty; nothing was stored" >&2; exit 1; }',
    "{",
    `  printf 'apiVersion: v1\\nkind: Secret\\nmetadata:\\n  name: %s\\n  namespace: %s\\n  labels:\\n    %s: %s\\ntype: Opaque\\ndata:\\n' ${shq(secret)} ${FLUX_NAMESPACE} ${MARKER} ${MARKED_DEPLOY_KEY}`,
    `  printf '  identity: %s\\n' "$identity"`,
    `  printf '  identity.pub: %s\\n' "$public"`,
    `  printf '  known_hosts: %s\\n' "$hosts"`,
    `} | k3s kubectl create --field-manager=${FIELD_MANAGER} -f - >/dev/null 2>"$d/create.err" || {`,
    `  grep -Ev '[A-Za-z0-9+/=]{40,}|PRIVATE' "$d/create.err" >&2 || true`,
    "  exit 1",
    "}",
    "created=1",
    // Each stored value, decoded, must be the file it came from.
    `same() { [ "$(${get} -o "jsonpath={.data.$1}" | base64 -d | sha256sum | cut -d' ' -f1)" = "$(sha256sum "$d/$2" | cut -d' ' -f1)" ]; }`,
    `same identity key && same 'identity\\.pub' key.pub && same known_hosts known_hosts || { echo "the stored Secret differs from the generated files" >&2; exit 1; }`,
    "verified=1",
    `printf 'PUBLIC %s\\n' "$(cat "$d/key.pub")"`,
    'echo "DEPLOY_KEY created"',
  ].join("\n");
}

/**
 * Applies Flux's manifests as deploy/flux/README.md 「建立叢集」 step 6 does, with the GitRepository
 * pinned (pinnedBootstrap; stdin: its text): server-side apply as `kubectl`, so Flux takes every
 * object over; the first apply cannot create the GitRepository before its CRD is established, so
 * it waits for the CRDs (120 s, well within ssh's silence limit) and applies again, which must
 * succeed. Then FIELD_MANAGER applies the pin itself (pinDocument), so that it co-owns
 * spec.ref.commit and kustomize-controller's apply of main's GitRepository keeps it; only then the
 * root Kustomization is created, and with it Flux starts applying main's tree.
 */
export function applyScript(base: string, bootstrap: Bootstrap, crds: readonly string[]): string {
  if (crds.length === 0) throw new ReleaseError("no CRDs to wait for");
  return [
    ...scriptStart(base, "apply", "flux-system.yaml", bootstrap.sha256),
    'echo "APPLY first"',
    'k3s kubectl apply --server-side -f "$d/flux-system.yaml" || echo "APPLY first-incomplete (expected before the CRDs exist)"',
    `k3s kubectl wait --for=condition=established --timeout=120s ${crds.map((crd) => shq(`crd/${crd}`)).join(" ")}`,
    'echo "APPLY second"',
    'k3s kubectl apply --server-side -f "$d/flux-system.yaml"',
    `printf '%s\\n' ${shq(pinDocument(bootstrap.apiVersion, bootstrap.head))} | k3s kubectl apply --server-side --field-manager=${FIELD_MANAGER} -f -`,
    `echo "APPLY pinned ${bootstrap.head}"`,
    `printf '%s\\n' ${shq(bootstrap.root)} | k3s kubectl apply --server-side -f -`,
    'echo "APPLY done"',
  ].join("\n");
}

/** Deletes Flux's own objects (stdin: the manifests): the namespace with both keys, the CRDs, the cluster roles and bindings. */
export function deleteScript(base: string, manifestsSha256: string): string {
  return [
    ...scriptStart(base, "delete", "flux-system.yaml", manifestsSha256),
    'k3s kubectl delete -f "$d/flux-system.yaml" --ignore-not-found --wait=false',
    'echo "DELETE done"',
  ].join("\n");
}

/** A root script on the demo machine: one ssh call, never repeated (retry: false). */
export function rootCommand(script: string): string {
  return `sudo bash -c ${shq(script)}`;
}

// --- Readiness ---------------------------------------------------------------------------------------

export interface Readiness {
  done: boolean;
  /** Set when waiting longer cannot help (Stalled=True). */
  failed?: string;
  state: string;
}

interface Condition {
  type?: string;
  status?: string;
  reason?: string;
  message?: string;
}

function conditionsOf(object: Record<string, unknown> | undefined): Condition[] {
  const conditions = at(object, ["status", "conditions"]);
  return Array.isArray(conditions) ? (conditions as Condition[]) : [];
}

function oneLine(message: string | undefined): string {
  return redact((message ?? "").replace(/\s+/g, " ").trim()).slice(0, 300);
}

/** A Flux object is ready when Ready=True for its current generation; Stalled=True ends the wait. */
export function fluxReadiness(object: Record<string, unknown> | undefined): Readiness {
  if (object === undefined) return { done: false, state: "not there yet" };
  const conditions = conditionsOf(object);
  const ready = conditions.find((c) => c.type === "Ready");
  const stalled = conditions.find((c) => c.type === "Stalled" && c.status === "True");
  const generation = at(object, ["metadata", "generation"]);
  const observed = at(object, ["status", "observedGeneration"]);
  const current = generation === undefined || observed === generation;
  const state = ready === undefined ? "no Ready condition yet" : `Ready=${ready.status} ${ready.reason ?? ""}: ${oneLine(ready.message)}`;
  if (ready?.status === "True" && current) return { done: true, state };
  if (stalled !== undefined) return { done: false, failed: `Stalled ${stalled.reason ?? ""}: ${oneLine(stalled.message)}`, state };
  return { done: false, state: current ? state : `${state} (generation ${String(generation)} not observed yet)` };
}

/** A Deployment is ready when every replica of its current generation is available. */
export function deploymentReadiness(object: Record<string, unknown> | undefined): Readiness {
  if (object === undefined) return { done: false, state: "not there yet" };
  const want = Number(at(object, ["spec", "replicas"]) ?? 1);
  const available = Number(at(object, ["status", "availableReplicas"]) ?? 0);
  const updated = Number(at(object, ["status", "updatedReplicas"]) ?? 0);
  const current = at(object, ["status", "observedGeneration"]) === at(object, ["metadata", "generation"]);
  const state = `${available}/${want} available, ${updated} updated`;
  return { done: current && available >= want && updated >= want, state };
}

/** One status line of a Flux object: readiness and the revision it has. */
export function describeFlux(label: string, object: Record<string, unknown> | undefined): string {
  if (object === undefined) return `${label}: not there`;
  const revision = text(at(object, ["status", "artifact", "revision"])) || text(at(object, ["status", "lastAppliedRevision"]));
  const history = at(object, ["status", "history"]);
  const latest = Array.isArray(history) && isMap(history[0]) ? history[0] : undefined;
  const release = latest === undefined ? "" : `, chart ${text(latest.chartVersion)} (${text(latest.status)})`;
  const suspended = at(object, ["spec", "suspend"]) === true ? ", SUSPENDED" : "";
  return `${label}: ${fluxReadiness(object).state}${revision === "" ? "" : `, revision ${redact(revision)}`}${release}${suspended}`;
}

/** The revisions install waits for: the commit of origin/main it checked and the chart that commit pins. */
export interface WantedRevisions {
  /** GitRepository status.artifact.revision and Kustomization status.lastAppliedRevision. */
  git: string;
  /** The digest in OCIRepository status.artifact.revision. */
  chartDigest: string;
  /** HelmRelease status.history[0].chartVersion of a deployed release. */
  release: string;
}

export function wantedRevisions(head: string, chart: { version: string; digest: string }): WantedRevisions {
  return {
    // source-controller names a branch's revision <branch>@sha1:<commit>, and kustomize-controller
    // records the same string (docs/test-reports/data/2026-10-11-70-flux-gitops/timeline.json).
    git: `main@sha1:${head}`,
    chartDigest: chart.digest,
    // helm-controller appends the first 12 hex digits of an OCIRepository chart's digest to the
    // chart version (mutateChartWithSourceRevision in helm-controller v1.6.5, and the same
    // timeline: chart 0.0.1+5062550e75fe).
    release: `${chart.version}+${chart.digest.replace(/^sha256:/, "").slice(0, 12)}`,
  };
}

export type RevisionKind = "git" | "kustomization" | "chart" | "release";

/** The revision a Flux object reports, in the form of WantedRevisions; "" when it has none yet. */
export function revisionOf(kind: RevisionKind, object: Record<string, unknown> | undefined): string {
  switch (kind) {
    case "git":
      return text(at(object, ["status", "artifact", "revision"]));
    case "kustomization":
      return text(at(object, ["status", "lastAppliedRevision"]));
    case "chart": {
      // With spec.ref.digest set, source-controller v1.9.6 pulls by digest and the revision is the
      // digest alone (getArtifactRef and getRevision); <tag>@<digest> is what a tag alone gives.
      const revision = text(at(object, ["status", "artifact", "revision"]));
      return revision.slice(revision.lastIndexOf("@") + 1);
    }
    case "release": {
      const history = at(object, ["status", "history"]);
      const latest = Array.isArray(history) && isMap(history[0]) ? history[0] : undefined;
      return latest !== undefined && text(latest.status) === "deployed" ? text(latest.chartVersion) : "";
    }
  }
}

/**
 * Ready for the current generation and at the wanted revision. Ready=True alone is not enough: an
 * object reconciled before (a run that stopped, or an older main) stays Ready=True at its old
 * revision until its controller catches up.
 */
export function readyAt(kind: RevisionKind, object: Record<string, unknown> | undefined, want: string): Readiness {
  const ready = fluxReadiness(object);
  const have = revisionOf(kind, object);
  if (!ready.done || have === want) return ready;
  return { done: false, state: `${ready.state}; at ${have === "" ? "no revision yet" : redact(have)}, waiting for ${want}` };
}

/** The annotation `flux reconcile` sets; a controller copies its value to status.lastHandledReconcileAt when it has handled it. */
export const RECONCILE_ANNOTATION = "reconcile.fluxcd.io/requestedAt";

/** The release is ready at the wanted chart, by a reconcile that began after the request `token`. */
export function releaseReadiness(object: Record<string, unknown> | undefined, want: string, token: string): Readiness {
  const ready = readyAt("release", object, want);
  if (!ready.done) return ready;
  if (text(at(object, ["status", "lastHandledReconcileAt"])) === token) return ready;
  return { done: false, state: `${ready.state}; reconcile request ${token} not handled yet` };
}

/**
 * Why install must stop at once while it keeps the GitRepository pinned to `head`: its spec.ref
 * changed (by hand, or the pin got lost), or source-controller reports another commit for the
 * current generation. Either way Flux may apply a commit this run did not check, and waiting out
 * the bound would only let it. undefined while all is as install left it (an older generation's
 * artifact, such as an earlier run's pin, is fine until source-controller observes the new one).
 */
export function pinProblem(git: Record<string, unknown> | undefined, head: string): string | undefined {
  if (git === undefined) return undefined;
  const label = `GitRepository ${FLUX_NAMESPACE}/${FLUX_NAMESPACE}`;
  const again = "nothing more is changed; look at it and at the Kustomization, then run install again (dry run first)";
  const ref = gitRefOf(git);
  if (ref.kind !== "pinned" || ref.commit !== head) {
    return `${label} is no longer pinned to ${head}, which this run checked (spec.ref is now ${describeRef(ref)}): Flux may apply a commit this run did not check; ${again}`;
  }
  const generation = at(git, ["metadata", "generation"]);
  const revision = revisionOf("git", git);
  if (generation !== undefined && at(git, ["status", "observedGeneration"]) === generation && revision !== "" && revision !== `main@sha1:${head}`) {
    return `${label} is pinned to ${head} but reports ${redact(revision)} for its current generation (${fluxReadiness(git).state}); ${again}`;
  }
  return undefined;
}

/**
 * The GitRepository after install removed the pin: spec.ref exactly {branch: main}, Ready for the
 * current generation, with an artifact of main (`main@sha1:<commit>`; the commit may be newer than
 * the checked one, since Flux now follows main). Any other spec.ref ends the wait at once.
 */
export function followsMain(git: Record<string, unknown> | undefined): Readiness {
  if (git === undefined) return { done: false, failed: "it is gone", state: "not there" };
  const ref = gitRefOf(git);
  const ready = fluxReadiness(git);
  if (ref.kind !== "main") return { done: false, failed: `spec.ref is ${describeRef(ref)}, not {branch: main}`, state: ready.state };
  const revision = revisionOf("git", git);
  if (!ready.done) return ready;
  if (!/^main@sha1:[0-9a-f]{40}$/.test(revision)) return { done: false, state: `${ready.state}; at ${redact(revision) || "no revision yet"}, waiting for a commit of main` };
  return { done: true, state: `${ready.state}; at ${revision}` };
}

// --- Talking to the cluster --------------------------------------------------------------------------

/** What the runs do on the demo machine; boxOps does it over ssh, the tests with a fake cluster. */
export interface FluxOps {
  tunnel(): Promise<OpsTunnel>;
  lockState(): Promise<LockState>;
  lock(step: string): Promise<() => Promise<void>>;
  cluster(manifests: FluxManifests | undefined): Promise<ClusterFacts>;
  sopsAge(secret: string): Promise<SopsAgeState>;
  createDeployKey(secret: string, knownHosts: string): Promise<{ publicKey: string } | "exists">;
  markNamespace(resourceVersion: string): Promise<void>;
  /** The bootstrap apply (applyScript); returns its output. */
  applyFlux(manifests: FluxManifests, bootstrap: Bootstrap): Promise<string>;
  /**
   * Pins GitRepository flux-system to `head` with FIELD_MANAGER co-owning the pin: when `from` is
   * given, first moves an earlier run's pin from that commit (a JSON patch that tests it).
   */
  pinGit(apiVersion: string, head: string, from?: string): Promise<void>;
  /** Removes the pin to `head` with a JSON patch that tests it, so that Flux follows main again. */
  unpinGit(head: string): Promise<void>;
  /** A non-Secret object by its resource name (KINDS), or undefined when it does not exist. */
  object(resource: string, namespace: string, name: string): Promise<Record<string, unknown> | undefined>;
  /** Flux custom objects in every namespace, with the Kustomization that applied each. */
  fluxObjects(crds: readonly string[]): Promise<FluxObject[]>;
  pods(namespace: string): Promise<string[]>;
  /** Sets RECONCILE_ANNOTATION on a Flux object, like `flux reconcile`. */
  requestReconcile(resource: string, namespace: string, name: string, token: string): Promise<void>;
  suspendRoot(): Promise<void>;
  deleteObject(resource: string, namespace: string, name: string): Promise<void>;
  deleteFlux(manifests: FluxManifests): Promise<string>;
  /** What stays in namespace trustiot after remove: PersistentVolumeClaims, Postgres Clusters and Secrets, by name. */
  kept(): Promise<string[]>;
  clusterState(): Promise<Record<string, KindState>>;
  now(): number;
  sleep(ms: number): Promise<void>;
}

/** What a run reads from the workstation; tests replace both. */
export interface Inputs {
  repo(): Promise<RepoInputs>;
}

/** Waits for `read` to be done, polling every POLL_MS up to `bound`; logs each new state. */
export async function waitFor(ops: FluxOps, log: Log, what: string, bound: number, read: () => Promise<Readiness>, hint = ""): Promise<void> {
  const start = ops.now();
  let last = "";
  for (;;) {
    const now = await read();
    if (now.state !== last) {
      log.note(`${what}: ${now.state}`);
      last = now.state;
    }
    if (now.done) return;
    if (now.failed !== undefined) throw new ReleaseError(`gave up waiting for ${what}: ${now.failed}${hint === "" ? "" : `. ${hint}`}`);
    if (ops.now() - start >= bound) {
      throw new ReleaseError(`timed out after ${Math.round(bound / 60_000)} min waiting for ${what}; last state: ${now.state}${hint === "" ? "" : `. ${hint}`}`);
    }
    await ops.sleep(POLL_MS);
  }
}

async function readPlanInputs(ops: FluxOps, inputs: Inputs): Promise<{ repo: RepoInputs; cluster: ClusterFacts; lock: LockState }> {
  const repo = await inputs.repo();
  const cluster = await ops.cluster(repo.manifests);
  const lock = await ops.lockState();
  return { repo, cluster, lock };
}

function printRefusals(log: Log, refusals: readonly string[]): void {
  for (const line of refusals) log.note(`REFUSED: ${line}`);
}

function refused(refusals: readonly string[]): ReleaseError {
  return new ReleaseError(`${refusals.length} refusal${refusals.length === 1 ? "" : "s"} above; nothing changed`);
}

/**
 * Runs `body` under the lab lock that `release` gives back: released afterwards, also on an
 * error, but kept when a root script may still be running on the demo machine (StillRunningError),
 * as tunnel.ts applyInstall does; the error says what to check.
 */
async function underLock<T>(release: () => Promise<void>, log: Log, body: () => Promise<T>): Promise<T> {
  let keep = false;
  try {
    return await body();
  } catch (error) {
    if (error instanceof StillRunningError) keep = true;
    throw error;
  } finally {
    if (keep) log.note(`the lab lock stays held for ${ISSUE}: a root script of this run may still be running on the demo machine`);
    else await release();
  }
}

/**
 * The sops-age check (ageCheckScript) runs as root and reads the age private key, so it runs only
 * when the lock read just now is free or this issue's, and only in a namespace this script may
 * use by the same fresh read (tunnel.ts dryRunLockCheck, for the dry run too). Otherwise it is
 * `not-checked`, and the plan refuses for the lock or the namespace.
 */
async function checkSopsAge(ops: FluxOps, repo: RepoInputs, cluster: ClusterFacts, lock: LockState): Promise<SopsAgeState> {
  if (lockProblem(lock) !== undefined) return { kind: "not-checked" };
  const sopsSecret = repo.manifests?.sopsSecret ?? "sops-age";
  const verdict = namespaceVerdict(cluster.namespace, repo.manifests?.deployKeySecret ?? FLUX_NAMESPACE, sopsSecret);
  if (verdict.kind !== "ours" && verdict.kind !== "age-key") return { kind: "not-checked" };
  return ops.sopsAge(sopsSecret);
}

const COMMAND = "bun scripts/demo/flux.ts";

// --- deploy-key ------------------------------------------------------------------------------------

export async function runDeployKey(ops: FluxOps, log: FluxLog, options: CommonOptions, inputs: Inputs): Promise<DeployKeyPlan> {
  return cleanErrors(log, async () => {
    const { repo, cluster, lock } = await readPlanInputs(ops, inputs);
    const plan = planDeployKey(repo, cluster, lock);
    log.save("deploy-key-plan", plan);
    if (plan.existing !== undefined) {
      log.note(`Secret ${FLUX_NAMESPACE}/${plan.secret} was made by deploy-key already; nothing to do. Its public key, for GitHub (Settings → Deploy keys, read-only):`);
      log.note(`  ${plan.existing.publicKey}`);
      log.note(`  fingerprint ${plan.existing.fingerprint}`);
    }
    printRefusals(log, plan.refusals);
    if (plan.refusals.length > 0) throw refused(plan.refusals);
    if (!plan.create) return plan;
    log.note(`plan: generate an ed25519 key on the demo machine (root, ${REMOTE_BASE}, shredded afterwards) and store it as Secret ${FLUX_NAMESPACE}/${plan.secret} (keys ${DEPLOY_KEY_KEYS.join(", ")}; known_hosts from ${KNOWN_HOSTS_FILE}; label ${MARKER}=${MARKED_DEPLOY_KEY}); only the public key is printed`);
    if (!checkConfirmation(log, plan, options, `${COMMAND} deploy-key --ssh-host <host>`)) return plan;
    await underLock(await ops.lock("deploy-key"), log, async () => {
      // Everything read again under the lock, the lock itself too.
      const now = await readPlanInputs(ops, inputs);
      const again = planDeployKey(now.repo, now.cluster, now.lock);
      if (again.refusals.length > 0) {
        printRefusals(log, again.refusals);
        throw refused(again.refusals);
      }
      if (stableJson(again) !== stableJson(plan)) throw new ReleaseError("the plan changed since it was shown; nothing changed, run again without --apply");
      const made = await ops.createDeployKey(plan.secret, repo.knownHosts);
      if (made === "exists") throw new ReleaseError(`Secret ${FLUX_NAMESPACE}/${plan.secret} appeared meanwhile; nothing changed, run again without --apply`);
      log.note(`created Secret ${FLUX_NAMESPACE}/${plan.secret}. Add this public key on GitHub (${REPOSITORY}: Settings → Deploy keys → Add deploy key; leave "Allow write access" off):`);
      log.note(`  ${made.publicKey}`);
      log.note(`  fingerprint ${sshFingerprint(made.publicKey)}`);
    });
    return plan;
  });
}

// --- install -----------------------------------------------------------------------------------------

function describeInstall(log: Log, plan: InstallPlan, repo: RepoInputs): void {
  const manifests = repo.manifests;
  log.note(`plan, from main ${plan.head.slice(0, 12)}: chart ${plan.chart?.version ?? "?"} (${plan.chart?.digest ?? "?"}), ingress ClusterIP ${plan.clusterIp ?? "?"}, Flux's manifests sha256 ${plan.manifestsSha256.slice(0, 12)}`);
  let step = 1;
  const git = `GitRepository ${FLUX_NAMESPACE}/${FLUX_NAMESPACE}`;
  if (plan.mark) log.note(`  ${step++}. label namespace ${FLUX_NAMESPACE} ${MARKER}=${MARKED_INSTALL} (field manager ${FIELD_MANAGER}), so later runs, remove and lab e2e can tell this install`);
  if (plan.pin.from !== undefined) {
    log.note(`  ${step++}. move the pin of ${git} from ${plan.pin.from} (an earlier run's; main moved on since) to ${plan.head}, which this run checked`);
  }
  if (plan.apply && manifests !== undefined) {
    log.note(
      `  ${step++}. server-side apply ${manifests.objects.length - 1} objects of Flux with ${git} pinned to ${plan.head} (spec.ref.commit; bootstrap sha256 ${plan.bootstrapSha256.slice(0, 12)}), wait for its ${manifests.crds.length} CRDs, apply again; apply the pin as ${FIELD_MANAGER} too, so Flux keeps it; then create the root Kustomization. Cluster-wide: CRDs ${manifests.crds.join(", ")}; ${manifests.clusterObjects.join(", ")}`,
    );
  } else if (plan.pin.step === "finish") {
    log.note(`  ${step++}. nothing to apply: the root Kustomization exists, and an earlier run stopped while ${git} was pinned; keep it pinned to ${plan.head} (as ${FIELD_MANAGER} too)`);
  } else if (!plan.apply) {
    log.note(`  ${step++}. nothing to apply: the root Kustomization exists, so Flux manages its own objects and follows main`);
  }
  const wanted = plan.chart === undefined ? undefined : wantedRevisions(plan.head, plan.chart);
  const pinned = plan.pin.step === "none" ? "" : ", stopping at once if the pin changes or Flux reports another commit";
  log.note(
    `  ${step++}. wait for the controllers (${manifests?.deployments.join(", ") ?? "?"}); ${git} and Kustomization ${FLUX_NAMESPACE} at ${wanted?.git ?? "?"} (the deploy key ${plan.deployKeyFingerprint || "?"} must be on GitHub, read-only)${pinned}; OCIRepository ${DEMO_NAMESPACE}/${RELEASE_NAME} at ${wanted?.chartDigest ?? "?"}`,
  );
  log.note(
    `  ${step++}. request a reconcile of HelmRelease ${DEMO_NAMESPACE}/${RELEASE_NAME} (annotation ${RECONCILE_ANNOTATION}, field manager ${FIELD_MANAGER}) and wait until it is handled and the release is deployed at chart ${wanted?.release ?? "?"}`,
  );
  if (plan.pin.step !== "none") {
    log.note(`  ${step++}. remove the pin (a JSON patch that tests it), so Flux follows main again, and wait until ${git} is Ready at a commit of main`);
  }
  log.note(`  ${step}. check that nothing of Flux is suspended; report everything`);
  for (const note of plan.notes) log.note(`NOTE: ${note}`);
}

/** Whether namespace flux-system still carries MARKER=installed. */
async function requireMarker(ops: FluxOps, manifests: FluxManifests | undefined, when: string): Promise<void> {
  const labels = (await ops.cluster(manifests)).namespace?.labels ?? {};
  if (labels[MARKER] !== MARKED_INSTALL) {
    throw new ReleaseError(
      `namespace ${FLUX_NAMESPACE} lost its label ${MARKER}=${MARKED_INSTALL} ${when}; later runs of this script will refuse it. Flux keeps running; remove it by hand as deploy/flux/README.md 「在展示機安裝 Flux」 describes, or label the namespace again`,
    );
  }
}

export async function runInstall(ops: FluxOps, log: FluxLog, options: CommonOptions, inputs: Inputs): Promise<InstallPlan> {
  return cleanErrors(log, async () => {
    const { repo, cluster, lock } = await readPlanInputs(ops, inputs);
    const plan = planInstall(repo, cluster, lock, await checkSopsAge(ops, repo, cluster, lock));
    log.save("install-plan", plan);
    describeInstall(log, plan, repo);
    printRefusals(log, plan.refusals);
    if (plan.refusals.length > 0) throw refused(plan.refusals);
    if (!checkConfirmation(log, installChange(plan), options, `${COMMAND} install --ssh-host <host>`)) return plan;

    await underLock(await ops.lock("install"), log, async () => {
      // Everything read again under the lock, the lock itself too, before any root script runs.
      const now = await readPlanInputs(ops, inputs);
      const again = planInstall(now.repo, now.cluster, now.lock, await checkSopsAge(ops, now.repo, now.cluster, now.lock));
      if (again.refusals.length > 0) {
        printRefusals(log, again.refusals);
        throw refused(again.refusals);
      }
      if (stableJson(installChange(again)) !== stableJson(installChange(plan))) throw new ReleaseError("the plan changed since it was shown; nothing changed, run again without --apply");
      const manifests = now.repo.manifests;
      if (manifests === undefined) throw new ReleaseError("Flux's manifests are missing");
      const before = await ops.clusterState();
      log.save("state-before", before);
      if (plan.mark) {
        await ops.markNamespace(now.cluster.namespace?.resourceVersion ?? "");
        log.note(`labeled namespace ${FLUX_NAMESPACE} ${MARKER}=${MARKED_INSTALL}`);
      }
      if (plan.pin.from !== undefined || plan.pin.step === "finish") {
        await ops.pinGit(manifests.gitApiVersion, plan.head, plan.pin.from);
        log.note(`GitRepository ${FLUX_NAMESPACE}/${FLUX_NAMESPACE} is pinned to ${plan.head}${plan.pin.from === undefined ? "" : ` (was ${plan.pin.from})`}, with ${FIELD_MANAGER} co-owning the pin`);
      }
      if (plan.apply) {
        const bootstrap = pinnedBootstrap(manifests, plan.head);
        for (const line of (await ops.applyFlux(manifests, bootstrap)).split("\n").filter((l) => l.trim() !== "")) log.note(`  ${line}`);
      }
      await requireMarker(ops, manifests, "after the bootstrap apply");
      await waitForFlux(ops, log, manifests, plan);
      await report(ops, log, now.repo);
      // Ready objects at the right revisions are not enough: a suspended one stops following main.
      const stuck = suspendedProblems(await ops.cluster(manifests));
      if (stuck.length > 0) throw new ReleaseError(`the install is not done: ${stuck.join("; ")}`);
      const after = await ops.clusterState();
      log.save("state", { before, after, diff: stateDiff(before, after) });
      log.note(`cluster-scoped changes: ${JSON.stringify(stateDiff(before, after))}`);
    });
    log.note(`done; records in ${shownPath(log.dir)}`);
    return plan;
  });
}

/**
 * Waits until Flux runs what this run checked: each object Ready for its current generation and at
 * the plan's revision (main's commit, the chart's digest and version), and the HelmRelease
 * reconciled once more after the Kustomization applied that commit. While the GitRepository is
 * pinned (every step but `none`), each poll also reads it and stops at once on a pinProblem. Then
 * the pin goes, and the GitRepository must be Ready at a commit of main with spec.ref {branch: main}.
 */
async function waitForFlux(ops: FluxOps, log: Log, manifests: FluxManifests, plan: InstallPlan): Promise<void> {
  if (plan.chart === undefined) throw new ReleaseError("the plan has no chart");
  const wanted = wantedRevisions(plan.head, plan.chart);
  const pinned = plan.pin.step !== "none";
  const moved = pinned
    ? `The GitRepository is pinned to ${plan.head}; if that commit is no longer on main, run install again (dry run first) from the new main`
    : `If main moved on GitHub after this run read it, fetch and run install again (dry run first) from the new main`;
  const readGit = () => ops.object(KINDS.gitRepository, FLUX_NAMESPACE, FLUX_NAMESPACE);
  const checkPin = (git: Record<string, unknown> | undefined) => {
    const problem = pinned ? pinProblem(git, plan.head) : undefined;
    if (problem !== undefined) throw new ReleaseError(problem);
  };
  const guarded = (read: () => Promise<Readiness>) => async (): Promise<Readiness> => {
    if (pinned) checkPin(await readGit());
    return read();
  };
  // Every wait while pinned is guarded, the controllers' too: once they run, Flux may apply.
  for (const name of manifests.deployments) {
    await waitFor(
      ops,
      log,
      `Deployment ${FLUX_NAMESPACE}/${name}`,
      WAITS.controllers,
      guarded(async () => deploymentReadiness(await ops.object(KINDS.deployment, FLUX_NAMESPACE, name))),
    );
  }
  await waitFor(
    ops,
    log,
    `GitRepository ${FLUX_NAMESPACE}/${FLUX_NAMESPACE} at ${wanted.git}`,
    WAITS.source,
    async () => {
      const git = await readGit();
      checkPin(git);
      return readyAt("git", git, wanted.git);
    },
    `Check that GitHub has the deploy key ${plan.deployKeyFingerprint} of Secret ${FLUX_NAMESPACE}/${manifests.deployKeySecret} as a read-only deploy key of ${REPOSITORY}. ${moved}`,
  );
  await waitFor(
    ops,
    log,
    `Kustomization ${FLUX_NAMESPACE}/${FLUX_NAMESPACE} at ${wanted.git}`,
    WAITS.kustomization,
    guarded(async () => readyAt("kustomization", await ops.object(KINDS.kustomization, FLUX_NAMESPACE, FLUX_NAMESPACE), wanted.git)),
    moved,
  );
  // The Kustomization applied the tree, Flux's own Namespace included: the marker must still be there.
  await requireMarker(ops, manifests, "after Flux applied its own manifests");
  await waitFor(
    ops,
    log,
    `OCIRepository ${DEMO_NAMESPACE}/${RELEASE_NAME} at ${wanted.chartDigest}`,
    WAITS.chart,
    guarded(async () => readyAt("chart", await ops.object(KINDS.ociRepository, DEMO_NAMESPACE, RELEASE_NAME), wanted.chartDigest)),
  );
  // The chart version alone does not show the values: values-demo.yaml or a SOPS-encrypted Secret
  // that changed with main reach helm-controller only through a watch, so a release that is Ready
  // at the right chart may not have seen them yet. A reconcile requested now, after the
  // Kustomization applied main's commit, reads them; helm-controller records the request it
  // handled when that reconcile ends (Reconcile in helm-controller v1.6.5).
  const token = new Date(ops.now()).toISOString();
  await ops.requestReconcile(KINDS.helmRelease, DEMO_NAMESPACE, RELEASE_NAME, token);
  log.note(`requested a reconcile of HelmRelease ${DEMO_NAMESPACE}/${RELEASE_NAME} (${RECONCILE_ANNOTATION}=${token})`);
  await waitFor(
    ops,
    log,
    `HelmRelease ${DEMO_NAMESPACE}/${RELEASE_NAME} at ${wanted.release}`,
    WAITS.release,
    guarded(async () => releaseReadiness(await ops.object(KINDS.helmRelease, DEMO_NAMESPACE, RELEASE_NAME), wanted.release, token)),
    `Look at \`sudo k3s kubectl -n ${DEMO_NAMESPACE} describe helmrelease ${RELEASE_NAME}\` and the pods of namespace ${DEMO_NAMESPACE}`,
  );
  if (!pinned) return;

  // The first deploy runs the checked commit and chart: Flux may follow main again. The pin goes
  // with an explicit JSON patch; a pin that is gone already (spec.ref exactly {branch: main}) went
  // after that deploy was verified, which is what the patch would do, so it is left as it is.
  const git = await readGit();
  if (git !== undefined && gitRefOf(git).kind === "main") {
    log.note(`the pin of GitRepository ${FLUX_NAMESPACE}/${FLUX_NAMESPACE} to ${plan.head} was gone already after the first deploy was verified: Flux follows main again`);
  } else {
    checkPin(git);
    await ops.unpinGit(plan.head);
    log.note(`removed the pin of GitRepository ${FLUX_NAMESPACE}/${FLUX_NAMESPACE} to ${plan.head}: Flux follows main again`);
  }
  let following = "";
  await waitFor(ops, log, `GitRepository ${FLUX_NAMESPACE}/${FLUX_NAMESPACE} following main`, WAITS.source, async () => {
    const now = await readGit();
    following = revisionOf("git", now);
    return followsMain(now);
  });
  if (following !== wanted.git) {
    log.note(`main moved on since this run checked ${plan.head}: Flux now follows main at ${redact(following)}, which this run did not check; status shows how it goes`);
  }
}

/** What the cluster runs, compared with main: the lines of status. */
async function report(ops: FluxOps, log: Log, repo: RepoInputs | undefined): Promise<void> {
  const deployments = repo?.manifests?.deployments ?? [...FLUX_COMPONENTS];
  for (const name of deployments) {
    const deployment = await ops.object(KINDS.deployment, FLUX_NAMESPACE, name);
    const image = text(at(deployment, ["spec", "template", "spec", "containers", "0", "image"]));
    log.note(`Deployment ${FLUX_NAMESPACE}/${name}: ${deploymentReadiness(deployment).state}${image === "" ? "" : `, ${redact(image)}`}`);
  }
  const git = await ops.object(KINDS.gitRepository, FLUX_NAMESPACE, FLUX_NAMESPACE);
  log.note(describeFlux(`GitRepository ${FLUX_NAMESPACE}/${FLUX_NAMESPACE}`, git));
  const ref = gitRefOf(git);
  if (git !== undefined && ref.kind !== "main") {
    log.note(
      `  spec.ref is ${describeRef(ref)}: ${ref.kind === "pinned" ? "pinned by an install that has not finished; run install again (dry run first) to finish it" : "neither main nor an install's pin"}`,
    );
  }
  const revision = text(at(git, ["status", "artifact", "revision"]));
  if (repo !== undefined && revision !== "" && !revision.endsWith(repo.head)) {
    log.note(`  that is not this checkout's origin/main (${repo.head.slice(0, 12)}); Flux follows main on GitHub`);
  }
  log.note(describeFlux(`Kustomization ${FLUX_NAMESPACE}/${FLUX_NAMESPACE}`, await ops.object(KINDS.kustomization, FLUX_NAMESPACE, FLUX_NAMESPACE)));
  const oci = await ops.object(KINDS.ociRepository, DEMO_NAMESPACE, RELEASE_NAME);
  log.note(describeFlux(`OCIRepository ${DEMO_NAMESPACE}/${RELEASE_NAME}`, oci));
  const helm = await ops.object(KINDS.helmRelease, DEMO_NAMESPACE, RELEASE_NAME);
  log.note(describeFlux(`HelmRelease ${DEMO_NAMESPACE}/${RELEASE_NAME}`, helm));
  if (repo?.chart !== undefined) {
    const wanted = wantedRevisions(repo.head, repo.chart);
    const pulled = revisionOf("chart", oci);
    const history = at(helm, ["status", "history"]);
    const deployed = Array.isArray(history) && isMap(history[0]) ? `${text(history[0].chartVersion)} (${text(history[0].status)})` : "";
    log.note(`main's ${chartSourceFile("demo")}: ${repo.chart.version}@${repo.chart.digest}`);
    log.note(`  pulled:   ${pulled === "" ? "nothing yet" : pulled === wanted.chartDigest ? "the same" : redact(pulled)}`);
    log.note(`  deployed: ${deployed === "" ? "nothing yet" : revisionOf("release", helm) === wanted.release ? `the same (${deployed})` : redact(deployed)}`);
  }
  const pods = await ops.pods(DEMO_NAMESPACE);
  log.note(`pods in ${DEMO_NAMESPACE}: ${pods.length === 0 ? "none" : pods.length}`);
  for (const pod of pods) log.note(`  ${redact(pod)}`);
}

// --- status ------------------------------------------------------------------------------------------

/** Reads and prints; changes nothing and takes no lock. Main's files are read from this checkout as it is. */
export async function runStatus(ops: FluxOps, log: FluxLog, inputs: Inputs): Promise<void> {
  return cleanErrors(log, () => status(ops, log, inputs));
}

async function status(ops: FluxOps, log: FluxLog, inputs: Inputs): Promise<void> {
  const repo = await inputs.repo();
  const cluster = await ops.cluster(repo.manifests);
  const lock = await ops.lockState();
  log.note(`lab lock: ${lock.kind === "held" ? `held by ${redact(lock.owner)}` : lock.kind}`);
  const verdict = namespaceVerdict(cluster.namespace, repo.manifests?.deployKeySecret ?? FLUX_NAMESPACE, repo.manifests?.sopsSecret ?? "sops-age");
  log.note(
    `namespace ${FLUX_NAMESPACE}: ${
      verdict.kind === "ours" ? `installed by this script (${MARKER}=${MARKED_INSTALL})` : verdict.kind === "age-key" ? "only the age key step's (Flux not installed)" : verdict.kind === "absent" ? "absent" : `not this script's: ${verdict.why.join("; ")}`
    }`,
  );
  for (const secret of cluster.namespace?.secrets ?? []) {
    log.note(redact(`  Secret ${secret.name}: type ${secret.type || "?"}, keys ${secret.keys.join(", ") || "(none)"}${secret.marker === "" ? "" : `, ${MARKER}=${secret.marker}`}`));
  }
  if (cluster.deployKey !== undefined) {
    try {
      log.note(`  deploy key fingerprint ${sshFingerprint(cluster.deployKey.publicKey)}`);
    } catch {
      log.note("  the deploy key's public half is not an OpenSSH key");
    }
  }
  log.note(`Flux CRDs: ${cluster.fluxCrds.length === 0 ? "none" : names(cluster.fluxCrds)}`);
  log.note(`Flux cluster roles and bindings: ${cluster.fluxClusterObjects.length === 0 ? "none" : names(cluster.fluxClusterObjects)}`);
  if (cluster.fluxCrds.length > 0) await report(ops, log, repo);
  else log.note(`namespace ${DEMO_NAMESPACE}: ${cluster.appNamespace === undefined ? "absent" : "exists"}`);
  for (const problem of suspendedProblems(cluster)) log.note(`WARNING: ${problem}`);
  if (repo.code.length > 0) log.note(`(main's files were read from this checkout, which is not a clean origin/main: ${repo.code.join("; ")})`);
}

// --- remove --------------------------------------------------------------------------------------------

/**
 * Removes the release and Flux in the order that keeps the data (deploy/flux/README.md,
 * 「在展示機安裝 Flux」, removal):
 * 1. the root Kustomization is suspended with prune off: deleting it then garbage-collects
 *    nothing (kustomize-controller v1.9.6 skips the finalizer's prune for a suspended
 *    Kustomization), so namespace trustiot and its volumes stay;
 * 2. every HelmRelease is deleted while helm-controller runs: a HelmRelease that is not suspended
 *    uninstalls its Helm release (helm-controller v1.6.5, reconcileDelete). What the chart keeps
 *    (helm.sh/resource-policy: keep: the Postgres Cluster, the generated password and signing key
 *    Secrets) and every PersistentVolumeClaim stay;
 * 3. every other Flux object, then the root Kustomization and GitRepository, while their
 *    controllers still run their finalizers;
 * 4. Flux itself from main's manifests: namespace flux-system with sops-age and the deploy key,
 *    the CRDs, the ClusterRoles and ClusterRoleBindings; only when no Flux object is left, so no
 *    finalizer waits for a controller that is gone.
 * Each wait is a poll of short reads (ssh cuts a silent command after about 3 minutes).
 */
export async function runRemove(ops: FluxOps, log: FluxLog, options: CommonOptions, inputs: Inputs): Promise<RemovePlan> {
  return cleanErrors(log, () => remove(ops, log, options, inputs));
}

async function remove(ops: FluxOps, log: FluxLog, options: CommonOptions, inputs: Inputs): Promise<RemovePlan> {
  const makePlan = async () => {
    const { repo, cluster, lock } = await readPlanInputs(ops, inputs);
    const objects = cluster.fluxCrds.length > 0 ? await ops.fluxObjects(cluster.fluxCrds) : [];
    const suspended: string[] = [];
    for (const { ref } of objects.filter((o) => o.ref.startsWith("HelmRelease/"))) {
      const [, namespace = "", name = ""] = ref.split("/");
      if (at(await ops.object(KINDS.helmRelease, namespace, name), ["spec", "suspend"]) === true) suspended.push(`${namespace}/${name}`);
    }
    return { repo, plan: planRemove(repo, cluster, lock, objects, suspended) };
  };
  const { repo, plan } = await makePlan();
  log.save("remove-plan", plan);
  printRefusals(log, plan.refusals);
  if (plan.refusals.length > 0) throw refused(plan.refusals);
  if (!plan.remove) {
    log.note("nothing to remove: this script's Flux is not installed");
    return plan;
  }
  log.note(`plan: suspend Kustomization ${FLUX_NAMESPACE} with prune off; delete ${names(plan.fluxObjects.filter((o) => o.startsWith("HelmRelease/"))) || "no HelmRelease"} (helm-controller uninstalls the release; kept: the Postgres Cluster, its Secrets and every volume)`);
  log.note(`  then delete the other Flux objects (${names(plan.fluxObjects.filter((o) => !o.startsWith("HelmRelease/"))) || "none"}), then Flux itself: ${repo.manifests?.objects.length ?? "?"} objects from main's manifests, namespace ${FLUX_NAMESPACE} with Secrets sops-age and ${repo.manifests?.deployKeySecret ?? "?"} among them`);
  log.note(`  a later install needs a new age key (a .sops.yaml PR and sops updatekeys) and a new deploy key on GitHub; namespace ${DEMO_NAMESPACE} stays`);
  if (!checkConfirmation(log, removeChange(plan), options, `${COMMAND} remove --ssh-host <host>`)) return plan;

  await underLock(await ops.lock("remove"), log, async () => {
    // Everything read again under the lock, the lock itself too.
    const now = await makePlan();
    if (now.plan.refusals.length > 0) {
      printRefusals(log, now.plan.refusals);
      throw refused(now.plan.refusals);
    }
    if (stableJson(removeChange(now.plan)) !== stableJson(removeChange(plan))) throw new ReleaseError("the plan changed since it was shown; nothing changed, run again without --apply");
    const manifests = now.repo.manifests;
    if (manifests === undefined) throw new ReleaseError("Flux's manifests are missing");
    const before = await ops.clusterState();
    log.save("state-before", before);
    const facts = await ops.cluster(manifests);
    const crds = facts.fluxCrds;
    // Absent after an install that stopped before its second apply, or a remove that stopped later.
    if (facts.rootKustomization !== undefined) {
      await ops.suspendRoot();
      log.note(`suspended Kustomization ${FLUX_NAMESPACE} with prune off`);
    }
    // Until the suspend, the root Kustomization could still create Flux objects (a new main): only
    // what the plan named is deleted, so anything else stops remove before it deletes anything.
    const listed = crds.length === 0 ? [] : (await ops.fluxObjects(crds)).map((o) => o.ref).sort();
    if (stableJson(listed) !== stableJson(plan.fluxObjects)) {
      throw new ReleaseError(
        `the Flux objects changed after the plan was made (now: ${names(listed) || "none"}; planned: ${names(plan.fluxObjects) || "none"}); nothing was deleted. ${
          facts.rootKustomization === undefined ? "" : "The root Kustomization stays suspended with prune off: "
        }run remove again (dry run first)${facts.rootKustomization === undefined ? "" : `, or resume Flux as ${RESUME} describes`}`,
      );
    }
    const gone = (refs: string[]) => async (): Promise<Readiness> => {
      const left = (await ops.fluxObjects(crds)).map((o) => o.ref).filter((ref) => refs.includes(ref));
      return { done: left.length === 0, state: left.length === 0 ? "gone" : `left: ${names(left)}` };
    };
    const deleteAll = async (refs: string[]) => {
      for (const ref of refs) {
        const [kind = "", namespace = "", name = ""] = ref.split("/");
        // planRemove refused any kind that main's manifests do not define.
        const resource = manifests.crdKinds[kind];
        if (resource === undefined) throw new ReleaseError(`${redact(ref)} has a kind that main's Flux does not define`);
        await ops.deleteObject(resource, namespace, name);
        log.note(`deleting ${redact(ref)}`);
      }
    };
    const releases = plan.fluxObjects.filter((o) => o.startsWith("HelmRelease/"));
    await deleteAll(releases);
    await waitFor(ops, log, `the uninstall of ${names(releases) || "no HelmRelease"}`, WAITS.uninstall, gone(releases));
    const root: readonly string[] = ROOT_SYNC_OBJECTS;
    const others = plan.fluxObjects.filter((o) => !releases.includes(o) && !root.includes(o));
    await deleteAll(others);
    await waitFor(ops, log, `the deletion of ${names(others) || "no other Flux object"}`, WAITS.deleted, gone(others));
    const roots = plan.fluxObjects.filter((o) => root.includes(o));
    await deleteAll(roots);
    await waitFor(ops, log, `the deletion of ${names(roots) || "the sync objects"}`, WAITS.deleted, gone(roots));
    const left = (await ops.fluxObjects(crds)).map((o) => o.ref);
    if (left.length > 0) throw new ReleaseError(`Flux objects appeared meanwhile (${names(left)}); Flux's controllers and CRDs stay. Delete them, then run remove again`);
    // The uninstall can take many minutes: read flux-system again right before deleting it.
    const late = await ops.cluster(manifests);
    const extra = [...(late.namespace === undefined ? [] : unexpectedNamespaceObjects(late.namespace, manifests)), ...unexpectedClusterObjects(late, manifests)];
    if (extra.length > 0) {
      throw new ReleaseError(`objects that are not part of this install appeared (${names(extra)}); Flux's controllers and CRDs stay. Move or delete them, then run remove again`);
    }
    for (const line of (await ops.deleteFlux(manifests)).split("\n").filter((l) => l.trim() !== "")) log.note(`  ${redact(line)}`);
    await waitFor(ops, log, `namespace ${FLUX_NAMESPACE}, Flux's CRDs and cluster roles to go`, WAITS.deleted, async () => {
      const facts = await ops.cluster(manifests);
      const remaining = [...(facts.namespace === undefined ? [] : [`namespace/${FLUX_NAMESPACE}`]), ...facts.fluxCrds, ...facts.fluxClusterObjects];
      return { done: remaining.length === 0, state: remaining.length === 0 ? "gone" : `left: ${names(remaining)}` };
    });
    const kept = await ops.kept();
    log.note(`kept in namespace ${DEMO_NAMESPACE}: ${kept.length === 0 ? "nothing" : names(kept)}`);
    const after = await ops.clusterState();
    log.save("state", { before, after, diff: stateDiff(before, after) });
    log.note(`cluster-scoped changes: ${JSON.stringify(stateDiff(before, after))}`);
  });
  log.note(`done; records in ${shownPath(log.dir)}`);
  return plan;
}

// --- FluxOps on the demo machine -----------------------------------------------------------------------

/** The clock boxOps waits with; tests replace it. */
export interface Clock {
  now(): number;
  sleep(ms: number): Promise<void>;
}

const SYSTEM_CLOCK: Clock = { now: () => Date.now(), sleep: (ms) => Bun.sleep(ms) };

function labelsOf(object: Record<string, unknown> | undefined, field: "labels" | "annotations"): Record<string, string> {
  const map = at(object, ["metadata", field]);
  return isMap(map) ? Object.fromEntries(Object.entries(map).map(([k, v]) => [k, String(v)])) : {};
}

/** What to check on the demo machine before anything else, when a root script may still be running. */
export const STILL_RUNNING_HELP =
  `on the demo machine, \`sudo flock --nonblock ${REMOTE_LOCK} true\` fails while a root script of this file still runs (\`ps -ef | grep '[s]udo bash -c'\` shows it); ` +
  `once it succeeds, run the dry run to see where things stand, and continue with --lock-held, or release the lab lock with \`rm -rf ~/${LOCK_DIR}\` only when ~/${LOCK_DIR}/owner is this run's line`;

/**
 * A root script (rootCommand), sent once (retry: false), its output returned. Exit 255 is ssh's:
 * the connection dropped or never came up, and `sudo bash -c` without a terminal keeps running
 * when ssh goes, so the script may still be changing the cluster: StillRunningError keeps the lab
 * lock (underLock). Every other failure is the script's own, with its last lines redacted.
 */
async function rootScript(box: Box, script: string, what: string, stdin: string): Promise<string> {
  const result = await box.sh(rootCommand(script), { retry: false, stdin });
  if (result.exitCode === 255) {
    throw new StillRunningError(`${what}: ssh ended with exit 255 and the root script may still be running on the demo machine (${redact(lastLines(result.stderr, 2))}); ${STILL_RUNNING_HELP}`);
  }
  if (result.exitCode !== 0) throw new ReleaseError(`${what} failed (exit ${result.exitCode}): ${redact(lastLines(result.stderr || result.stdout))}`);
  for (const line of result.stdout.split("\n").filter((l) => l.startsWith("STALE "))) box.log.note(line.slice("STALE ".length));
  return result.stdout;
}

/**
 * A JSON patch of GitRepository flux-system as FIELD_MANAGER, sent once: its `test` makes a second
 * copy fail, so after an error the object is read back and `landed` tells whether it went through.
 */
async function patchGit(box: Box, patch: unknown[], what: string, landed: (ref: GitRef) => boolean): Promise<void> {
  const result = await box.sh(kubectlLine(["-n", FLUX_NAMESPACE, "patch", KINDS.gitRepository, FLUX_NAMESPACE, "--type=json", `--field-manager=${FIELD_MANAGER}`, "-p", JSON.stringify(patch)]), { retry: false });
  if (result.exitCode === 0) return;
  const ref = gitRefOf(await box.getJson(["-n", FLUX_NAMESPACE, "get", KINDS.gitRepository, FLUX_NAMESPACE]));
  if (landed(ref)) return;
  throw new ReleaseError(`${what} failed (exit ${result.exitCode}: ${redact(lastLines(result.stderr, 2))}); spec.ref is ${describeRef(ref)}`);
}

function kubectlLine(args: readonly string[]): string {
  return ["sudo", "k3s", "kubectl", ...args].map(shq).join(" ");
}

/** FluxOps on the demo machine, over ssh. `ageTool` gives age-keygen for linux/arm64. */
export function boxOps(box: Box, options: CommonOptions, ageTool: () => Promise<Uint8Array>, clock: Clock = SYSTEM_CLOCK): FluxOps {
  let resources: string[] | undefined;
  const object = async (resource: string, namespace: string, name: string) => {
    if (/^secrets?(\.|$)/.test(resource)) throw new ReleaseError("Secrets are never read as objects");
    return box.getJson(["-n", namespace, "get", resource, name]);
  };
  return {
    tunnel: () => opsTunnel(box),
    lockState: async () => parseLockState(await box.mustSh(LOCK_STATE_COMMAND, "reading the lab lock")),
    lock: (step) => takeLock(box, ISSUE, step, options.lockHeld),
    async cluster(manifests) {
      const facts: ClusterFacts = { fluxCrds: [], fluxClusterObjects: [], poc: [], missingPlatformCrds: [], services: [] };
      const ns = await box.getJson(["get", "namespace", FLUX_NAMESPACE]);
      if (ns !== undefined) {
        resources ??= inventoryResources(await box.kubectl(["api-resources", "--namespaced", "--verbs=list,delete", "-o", "name"]));
        const inventory = await box.kubectl(["-n", FLUX_NAMESPACE, "get", resources.join(","), "--ignore-not-found", "-o", `jsonpath=${INVENTORY_JSONPATH}`]);
        const secrets = parseSecrets(await box.kubectl(["-n", FLUX_NAMESPACE, "get", "secrets", "-o", `go-template=${SECRET_TEMPLATE}`]));
        facts.namespace = {
          uid: text(at(ns, ["metadata", "uid"])),
          resourceVersion: text(at(ns, ["metadata", "resourceVersion"])),
          labels: labelsOf(ns, "labels"),
          annotations: labelsOf(ns, "annotations"),
          terminating: at(ns, ["metadata", "deletionTimestamp"]) !== undefined || at(ns, ["status", "phase"]) === "Terminating",
          objects: parseInventory(inventory),
          secrets,
        };
        const keySecret = manifests?.deployKeySecret ?? FLUX_NAMESPACE;
        if (secrets.some((s) => s.name === keySecret && s.marker === MARKED_DEPLOY_KEY)) {
          // Only the public halves: identity.pub and known_hosts.
          const read = async (key: string) =>
            Buffer.from((await box.kubectl(["-n", FLUX_NAMESPACE, "get", "secret", keySecret, "-o", `jsonpath={.data.${key.replaceAll(".", "\\.")}}`])).trim(), "base64").toString("utf8");
          facts.deployKey = { publicKey: (await read("identity.pub")).trim(), knownHostsSha256: sha256(await read("known_hosts")) };
        }
      }
      const crds = (await box.kubectl(["get", KINDS.crd, "-o", "name"]))
        .split("\n")
        .map((line) => line.trim().replace(/^customresourcedefinition\.apiextensions\.k8s\.io\//, ""))
        .filter((name) => name !== "");
      facts.fluxCrds = crds.filter((name) => name.endsWith(".toolkit.fluxcd.io")).sort();
      facts.missingPlatformCrds = PLATFORM_CRDS.filter((crd) => !crds.includes(crd));
      facts.fluxClusterObjects = (await box.kubectl(["get", "clusterroles.rbac.authorization.k8s.io,clusterrolebindings.rbac.authorization.k8s.io", "-l", "app.kubernetes.io/part-of=flux", "-o", "name"]))
        .split("\n")
        .map((line) => line.trim().replace(/^(clusterrole|clusterrolebinding)\.rbac\.authorization\.k8s\.io\//, "$1/"))
        .filter((line) => line !== "")
        .sort();
      const suspended = (object: Record<string, unknown>) => at(object, ["spec", "suspend"]) === true;
      if (facts.fluxCrds.includes(KINDS.kustomization)) {
        const root = await object(KINDS.kustomization, FLUX_NAMESPACE, FLUX_NAMESPACE);
        // Pruning only when spec.prune is exactly true: an omitted boolean is false.
        if (root !== undefined) facts.rootKustomization = { suspended: suspended(root), prune: at(root, ["spec", "prune"]) === true };
      }
      if (facts.fluxCrds.includes(KINDS.gitRepository)) {
        const git = await object(KINDS.gitRepository, FLUX_NAMESPACE, FLUX_NAMESPACE);
        if (git !== undefined) facts.gitRepository = { ref: gitRefOf(git), suspended: suspended(git) };
      }
      if (facts.fluxCrds.includes(KINDS.helmRelease)) {
        const release = await object(KINDS.helmRelease, DEMO_NAMESPACE, RELEASE_NAME);
        if (release !== undefined) facts.helmRelease = { suspended: suspended(release) };
      }
      for (const [namespace, fingerprint] of Object.entries(POC_FINGERPRINTS)) {
        const names = [...fingerprint.deployments.map((d) => `deployment.apps/${d}`), ...fingerprint.statefulsets.map((s) => `statefulset.apps/${s}`)];
        const found = await box.tryKubectl(["-n", namespace, "get", ...names, "--ignore-not-found", "-o", "name"]);
        if (found.exitCode !== 0) throw new ReleaseError(`reading namespace ${namespace} failed: ${redact(lastLines(found.stderr))}`);
        facts.poc.push(...found.stdout.split("\n").filter((l) => l.trim() !== "").map((l) => `${namespace}/${l.trim()}`));
      }
      facts.services = parseServices(await box.kubectl(["get", "services", "-A", "-o", SERVICES_JSONPATH]));
      // The API server's own ServiceCIDR, served from Kubernetes 1.33 (GA); without it, install
      // refuses (clusterIpProblems). Ranges added later by other ServiceCIDRs have their own bands.
      const serviceCidr = await box.getJson(["get", "servicecidrs.networking.k8s.io", "kubernetes"]);
      if (serviceCidr !== undefined) facts.serviceCidrs = serviceCidrsOf(serviceCidr);
      const app = await box.getJson(["get", "namespace", DEMO_NAMESPACE]);
      if (app !== undefined) facts.appNamespace = { labels: labelsOf(app, "labels"), terminating: at(app, ["metadata", "deletionTimestamp"]) !== undefined };
      return facts;
    },
    async sopsAge(secret) {
      const tool = await ageTool();
      const script = ageCheckScript(REMOTE_BASE, new Bun.CryptoHasher("sha256").update(tool).digest("hex"), secret);
      // Exit 255 is ssh's, and the root script (it reads the age private key) may still run:
      // StillRunningError, as for every root script, so that underLock keeps the lab lock. Any
      // other failure leaves the Secret unreadable, and the plan refuses.
      const result = await box.sh(rootCommand(script), { retry: false, stdin: tool });
      if (result.exitCode === 255) {
        throw new StillRunningError(`checking Secret ${FLUX_NAMESPACE}/${secret}: ssh ended with exit 255 and the root script may still be running on the demo machine (${redact(lastLines(result.stderr, 2))}); ${STILL_RUNNING_HELP}`);
      }
      if (result.exitCode !== 0) return { kind: "unreadable", detail: `exit ${result.exitCode}: ${redact(lastLines(result.stderr || result.stdout, 3))}` };
      for (const line of result.stdout.split("\n").filter((l) => l.startsWith("STALE "))) box.log.note(line.slice("STALE ".length));
      return parseAgeCheck(result.stdout);
    },
    async createDeployKey(secret, knownHosts) {
      const out = await rootScript(box, deployKeyScript(REMOTE_BASE, sha256(knownHosts), secret), "generating the deploy key", knownHosts);
      if (out.split("\n").includes("DEPLOY_KEY exists")) return "exists";
      const publicKey = /^PUBLIC (ssh-ed25519 [A-Za-z0-9+/=]+ \S+)$/m.exec(out)?.[1];
      if (publicKey === undefined || !out.split("\n").includes("DEPLOY_KEY created")) throw new ReleaseError("deploy-key's script ended without the public key; run deploy-key again to see the Secret");
      return { publicKey };
    },
    async markNamespace(resourceVersion) {
      if (!/^\d+$/.test(resourceVersion)) throw new ReleaseError("namespace flux-system has no resourceVersion");
      const result = await box.sh(
        kubectlLine(["label", "namespace", FLUX_NAMESPACE, `${MARKER}=${MARKED_INSTALL}`, `--field-manager=${FIELD_MANAGER}`, `--resource-version=${resourceVersion}`]),
        { retry: false },
      );
      if (result.exitCode === 0) return;
      // ssh dropped or the update conflicted: the label on the cluster tells whether it went through.
      const labels = labelsOf(await box.getJson(["get", "namespace", FLUX_NAMESPACE]), "labels");
      if (labels[MARKER] === MARKED_INSTALL) return;
      throw new ReleaseError(`labeling namespace ${FLUX_NAMESPACE} failed (exit ${result.exitCode}): ${redact(lastLines(result.stderr))}; nothing else changed, run the dry run again`);
    },
    async applyFlux(manifests, bootstrap) {
      const out = await rootScript(box, applyScript(REMOTE_BASE, bootstrap, manifests.crds), "applying Flux's manifests", bootstrap.text);
      if (!out.split("\n").includes("APPLY done")) throw new ReleaseError("the apply script ended without APPLY done; run the dry run to see where it stands");
      return out;
    },
    async pinGit(apiVersion, head, from) {
      const document = pinDocument(apiVersion, head);
      if (from !== undefined && from !== head) {
        if (!/^[0-9a-f]{40}$/.test(from)) throw new ReleaseError("the commit to move the pin from is not a full commit hash");
        const patch = [
          { op: "test", path: "/spec/ref/commit", value: from },
          { op: "replace", path: "/spec/ref/commit", value: head },
        ];
        await patchGit(box, patch, `moving the pin of GitRepository ${FLUX_NAMESPACE} from ${from} to ${head}`, (ref) => ref.kind === "pinned" && ref.commit === head);
      }
      // Idempotent: an apply of the same value only makes FIELD_MANAGER a co-owner.
      await box.kubectl(["apply", "--server-side", `--field-manager=${FIELD_MANAGER}`, "-f", "-"], document);
      const ref = gitRefOf(await object(KINDS.gitRepository, FLUX_NAMESPACE, FLUX_NAMESPACE));
      if (ref.kind !== "pinned" || ref.commit !== head) throw new ReleaseError(`GitRepository ${FLUX_NAMESPACE} is not pinned to ${head} after pinning it: spec.ref is ${describeRef(ref)}`);
    },
    async unpinGit(head) {
      if (!/^[0-9a-f]{40}$/.test(head)) throw new ReleaseError("the pinned commit is not a full commit hash");
      const patch = [
        { op: "test", path: "/spec/ref/commit", value: head },
        { op: "remove", path: "/spec/ref/commit" },
      ];
      await patchGit(box, patch, `removing the pin of GitRepository ${FLUX_NAMESPACE} to ${head}`, (ref) => ref.kind === "main");
    },
    object,
    async fluxObjects(crds) {
      if (crds.length === 0) return [];
      const out = await box.kubectl([
        "get",
        crds.join(","),
        "-A",
        "-o",
        `jsonpath={range .items[*]}{.kind}/{.metadata.namespace}/{.metadata.name}{"\\t"}{.metadata.labels.${KUSTOMIZE_LABEL_NAMESPACE.replaceAll(".", "\\.")}}/{.metadata.labels.${KUSTOMIZE_LABEL_NAME.replaceAll(".", "\\.")}}{"\\n"}{end}`,
      ]);
      return parseFluxObjects(out);
    },
    async pods(namespace) {
      const out = await box.kubectl([
        "-n",
        namespace,
        "get",
        "pods",
        "-o",
        'jsonpath={range .items[*]}{.metadata.name}{" "}{.status.phase}{" ready="}{.status.containerStatuses[*].ready}{" restarts="}{.status.containerStatuses[*].restartCount}{"\\n"}{end}',
      ]);
      return out.split("\n").filter((l) => l.trim() !== "");
    },
    async requestReconcile(resource, namespace, name, token) {
      if (!/^[0-9TZ:.-]+$/.test(token)) throw new ReleaseError(`not a reconcile token: ${token}`);
      // Our own field manager: kustomize-controller v1.9.6 takes over only the fields of kubectl,
      // before-first-apply and its own Update manager when it applies (kustomization_controller.go),
      // so the annotation stays until helm-controller has read it.
      await box.kubectl(["-n", namespace, "annotate", "--overwrite", `--field-manager=${FIELD_MANAGER}`, resource, name, `${RECONCILE_ANNOTATION}=${token}`]);
    },
    async suspendRoot() {
      await box.kubectl(["-n", FLUX_NAMESPACE, "patch", KINDS.kustomization, FLUX_NAMESPACE, "--type=merge", "-p", '{"spec":{"suspend":true,"prune":false}}']);
    },
    async deleteObject(resource, namespace, name) {
      await box.kubectl(["-n", namespace, "delete", resource, name, "--ignore-not-found", "--wait=false"]);
    },
    deleteFlux: (manifests) => rootScript(box, deleteScript(REMOTE_BASE, manifests.sha256), "deleting Flux's objects", manifests.text),
    async kept() {
      if ((await box.getJson(["get", "namespace", DEMO_NAMESPACE])) === undefined) return [];
      const crds = await box.kubectl(["get", KINDS.crd, "-o", "name"]);
      const postgres = crds.includes("/clusters.postgresql.cnpg.io") ? ["clusters.postgresql.cnpg.io"] : [];
      const kinds = ["persistentvolumeclaims", "secrets", ...postgres];
      const out = await box.kubectl(["-n", DEMO_NAMESPACE, "get", kinds.join(","), "--ignore-not-found", "-o", "name"]);
      return out.split("\n").map((l) => l.trim()).filter((l) => l !== "");
    },
    clusterState: () => clusterState(box),
    now: () => clock.now(),
    sleep: (ms) => clock.sleep(ms),
  };
}

/** The inputs from this checkout: main fetched and checked, then read and built. */
export function checkoutInputs(root: string = ROOT): Inputs {
  return { repo: async () => readRepo(root, await readCode(root), kustomizeFluxSystem) };
}

/** age-keygen for the demo machine, built from source with the pinned Go (scripts/flux/age.ts). */
async function demoAgeKeygen(): Promise<Uint8Array> {
  return new Uint8Array(await Bun.file(await ageToolFor("age-keygen", { os: "linux", arch: "arm64" })).arrayBuffer());
}

const HOST_ONLY = { "ssh-host": { default: "" }, "log-dir": { default: "" } } as const;
const SUBCOMMANDS = {
  "deploy-key": COMMON_SPEC,
  install: COMMON_SPEC,
  status: HOST_ONLY,
  remove: COMMON_SPEC,
} as const;

let log: FluxLog | undefined;

if (import.meta.main) {
  await main(
    () => log,
    async () => {
      const [name, ...argv] = Bun.argv.slice(2);
      const spec = SUBCOMMANDS[name as keyof typeof SUBCOMMANDS];
      if (spec === undefined) throw new ReleaseError(`usage: ${COMMAND} <${Object.keys(SUBCOMMANDS).join("|")}> --ssh-host <host> [--apply --confirm <code>] [--lock-held]`);
      const raw = parseOptions(argv, spec);
      const options = commonOptions({ apply: "false", ...raw });
      log = new FluxLog(`flux-${name}`, options.logDir);
      const current = log;
      // Every line a run prints, logs or saves goes through FluxLog (redact()), and ssh's own
      // errors name the ssh host only as <ssh-host>.
      const ops = boxOps(new Box(options.sshHost, current, { run: hidingSshHost(sshRun, options.sshHost) }), options, demoAgeKeygen);
      const inputs = checkoutInputs();
      await cleanErrors(current, () =>
        withOpsTunnel(
          () => ops.tunnel(),
          current,
          async () => {
            if (name === "deploy-key") await runDeployKey(ops, current, options, inputs);
            else if (name === "install") await runInstall(ops, current, options, inputs);
            else if (name === "remove") await runRemove(ops, current, options, inputs);
            else await runStatus(ops, current, inputs);
          },
        ),
      );
    },
  );
}
