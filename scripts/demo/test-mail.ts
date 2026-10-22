/**
 * The demo environment's test mail (Laysi/trustiot-v2#468, docs/runbooks/demo-environment.md
 * 「外寄郵件與 Anthropic 金鑰」): one message from inside the demo cluster, with the demo
 * environment's SMTP settings and Secrets, to show at the receiver that SPF and DKIM pass.
 *
 *   bun scripts/demo/test-mail.ts send --ssh-host <host> [--apply --confirm <code>] [--lock-held]
 *   bun scripts/demo/test-mail.ts headers < <the received message's raw headers>
 *
 * send reads the SMTP host, port and security from the checkout's chart values, the SaaS shape
 * and the demo overlay (deploy/flux/clusters/demo/trustiot/), and where the sender comes from
 * from the HelmRelease there (the private values' `smtp.from` target); a dirty checkout stops
 * it, and the confirm code covers the commit. It checks on the demo machine, read only, that the
 * deployed values say the same, that the Secrets have the keys, and what could shape the test
 * pod's traffic (NetworkPolicies, AuthorizationPolicies, the ambient mesh's ztunnel). Without
 * --apply it changes nothing and prints the plan with its code.
 *
 * With --apply it reads the recipient from stdin (a prompt on a terminal, not echoed), never from
 * the command line, takes the lab lock for this issue, plans again, and runs one short-lived pod in the
 * namespace trustiot under Pod Security `restricted`: curl from the image pinned below, with the
 * credentials and the sender from the Secrets as secretKeyRef env variables, and the recipient
 * only on the pod's stdin (kubectl attach). Inside the pod, curl's verbose SMTP transcript goes
 * straight into an awk filter that keeps only the reply codes, the command verbs and the TLS
 * version and cipher, so the container log (and anything this script prints or saves) never
 * holds the recipient, the sender, the credentials or the provider's message ID. The pod is
 * deleted at the end, whatever happened, and an earlier run's leftover pod first. Once the lab lock
 * is held, SIGINT, SIGTERM and SIGHUP delete the pod (if it was created) and release the lock
 * before the script exits with 128 + the signal's number; a second signal exits at once. Only
 * SIGKILL, a second signal or a lost ssh connection leave them behind: the kubelet stops the pod at
 * activeDeadlineSeconds and the next run deletes it as a leftover, and the lock is removed by hand
 * as lib.ts's LOCK_HELP says. --apply also needs the checkout's commit to be on origin/main.
 *
 * Unlike lib.ts's rule for the scripts here, it records no cluster state before and after: it only
 * creates one short-lived pod of its own and deletes it, and it changes nothing else.
 *
 * headers reads the raw headers of the received message and prints SPF, DKIM and DMARC results,
 * whether the DKIM and SPF domains align with the From domain, and the TLS of each Received hop,
 * with every domain as a placeholder and without addresses or IDs: ready for the test report.
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { Writable } from "node:stream";
import { parseOptions, tryRun } from "../release/exec.ts";
import { ReleaseError, errorMessage } from "../release/lib.ts";
import { Box, COMMON_SPEC, DEMO_NAMESPACE, Log, ROOT, checkConfirmation, commonOptions, lastLines, main, opsTunnel, shq, stableJson, takeLock, withOpsTunnel } from "./lib.ts";
import { LOCK_STATE_COMMAND, parseLockState, type Clock } from "./tunnel.ts";

/** The issue that owns this script's lab lock runs. */
export const ISSUE = "Laysi/trustiot-v2#468";

/**
 * curl's official image (github.com/curl/curl-container), published to quay.io/curl/curl and
 * Docker Hub as curlimages/curl, with the same index. Pinned by the digest of its multi-arch index,
 * which lists linux/arm64 (v8); Alpine with BusyBox sh and awk, curl built with SMTP, SMTPS and
 * OpenSSL. The image runs as curl_user, UID 100 and GID 101. Checked on 2026-10-21 with
 * `skopeo inspect --raw` on both registries (same index digest) and the arm64 config
 * (`--override-arch arm64 --override-variant v8 --config`).
 *
 * Upstream pushed 8.23.0 again on 2026-10-20 and quay.io then deleted the index this was pinned
 * to (its manifest answers 404), so on 2026-10-22 it was pinned to the new index: the same digest
 * on both registries, linux/arm64 v8 listed, User curl_user (UID and GID checked in the image).
 * A pin on quay.io lasts only while its tag points at it; when a pull fails with "manifest
 * unknown", pin the tag's current index after the same checks.
 */
export const IMAGE = "quay.io/curl/curl:8.23.0@sha256:11e64e28c5f58c8d93ee95fcf22ac055f48b0bc440e0ce7d8013bbb5ad6aa8ac";
export const IMAGE_UID = 100;
export const IMAGE_GID = 101;
/** The test pods' name prefix and label; a leftover with both is an earlier run's, deleted first. */
export const POD_PREFIX = "trustiot-test-mail-";
export const POD_LABELS: Readonly<Record<string, string>> = {
  "app.kubernetes.io/name": "trustiot-test-mail",
  "app.kubernetes.io/part-of": "trustiot-demo",
  // Like every service pod of the chart (templates/_service.tpl): its connections leave through
  // ztunnel, the way a service's SMTP connection would.
  "istio.io/dataplane-mode": "ambient",
};
const POD_SELECTOR = "app.kubernetes.io/name=trustiot-test-mail";
export const CONTAINER = "mail";
/**
 * Files whose state the plan binds: this script and the modules it imports (scripts/demo and
 * scripts/release), the demo cluster's values and the chart's defaults.
 */
export const CODE_PATHS = ["scripts/demo", "scripts/release", "deploy/flux/clusters/demo/trustiot", "deploy/chart/values.yaml"];
/** The branch whose history the commit must be in for --apply (the local remote-tracking ref). */
export const MAIN_REF = "origin/main";
const CHART_VALUES = "deploy/chart/values.yaml";
const SAAS_VALUES = "deploy/flux/clusters/demo/trustiot/values-saas.yaml";
const DEMO_VALUES = "deploy/flux/clusters/demo/trustiot/values-demo.yaml";
const HELM_RELEASE = "deploy/flux/clusters/demo/trustiot/helm-release.yaml";
/** The ConfigMap the HelmRelease reads values-saas.yaml and values-demo.yaml from (kustomization.yaml). */
const VALUES_CONFIGMAP = "trustiot-values";

/** How long the pod may live at all: the kubelet stops it after this, even if this script is gone. */
export const POD_DEADLINE_SECONDS = 600;
/** How long the pod waits for the recipient on stdin before it gives up. */
export const STDIN_WAIT_SECONDS = 120;
/** curl's own limits: the TCP connection, and the whole SMTP session. */
export const CONNECT_TIMEOUT_SECONDS = 20;
export const SESSION_TIMEOUT_SECONDS = 90;
/** Waiting here for the pod to run (image pull included) and then to end. */
const RUNNING_WAIT_MS = 5 * 60 * 1000;
const DONE_WAIT_MS = 4 * 60 * 1000;
const POLL_MS = 2_000;

const REAL_CLOCK: Clock = { now: () => Date.now(), sleep: (ms) => Bun.sleep(ms) };

// --- The SMTP settings of the checkout -------------------------------------------------------------

export type Security = "starttls" | "tls";

export interface SmtpSettings {
  host: string;
  port: number;
  security: Security;
  credentialsSecret: { name: string; usernameKey: string; passwordKey: string };
}

/** Where the sender (smtp.from) comes from: a key of a Secret the HelmRelease reads. */
export interface SenderSource {
  name: string;
  key: string;
}

const DNS_NAME = /^(?=.{1,253}$)[a-z0-9](?:[-a-z0-9]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[-a-z0-9]{0,61}[a-z0-9])?)+$/;
const OBJECT_NAME = /^(?=.{1,253}$)[a-z0-9](?:[-a-z0-9.]*[a-z0-9])?$/;
const SECRET_KEY = /^[-._a-zA-Z0-9]{1,253}$/;

function isMap(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** The `smtp` map of one values file (empty when the file has none). */
export function smtpLayer(values: unknown, file: string): Record<string, unknown> {
  if (!isMap(values)) throw new ReleaseError(`${file} is not a YAML mapping`);
  const smtp = values.smtp;
  if (smtp === undefined || smtp === null) return {};
  if (!isMap(smtp)) throw new ReleaseError(`${file}: smtp is not a mapping`);
  return smtp;
}

function mergeMaps(base: Record<string, unknown>, over: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = { ...base };
  for (const [key, value] of Object.entries(over)) {
    const current = out[key];
    out[key] = isMap(current) && isMap(value) ? mergeMaps(current, value) : value;
  }
  return out;
}

/**
 * The SMTP settings the demo release gets: the chart's defaults, then the SaaS shape, then the demo
 * overlay, merged as Helm merges values. Refuses a missing host, a port that is not one, any
 * security other than the chart's starttls and tls (never plain SMTP), and missing credentials.
 */
export function smtpSettings(layers: readonly Record<string, unknown>[]): SmtpSettings {
  const smtp = layers.reduce<Record<string, unknown>>((merged, layer) => mergeMaps(merged, layer), {});
  const host = smtp.host;
  if (typeof host !== "string" || host === "") throw new ReleaseError("smtp.host is not set in the demo values");
  if (!DNS_NAME.test(host)) throw new ReleaseError("smtp.host is not a lowercase DNS name");
  const port = smtp.port;
  if (typeof port !== "number" || !Number.isInteger(port) || port < 1 || port > 65_535) throw new ReleaseError("smtp.port is not a TCP port");
  const security = smtp.security;
  if (security !== "starttls" && security !== "tls") throw new ReleaseError(`smtp.security must be starttls or tls (the chart supports no plain SMTP), not ${JSON.stringify(security)}`);
  const credentials = isMap(smtp.credentialsSecret) ? smtp.credentialsSecret : {};
  const name = credentials.name;
  const usernameKey = credentials.usernameKey;
  const passwordKey = credentials.passwordKey;
  if (typeof name !== "string" || !OBJECT_NAME.test(name)) throw new ReleaseError("smtp.credentialsSecret.name is not set: the demo relay needs credentials");
  if (typeof usernameKey !== "string" || !SECRET_KEY.test(usernameKey) || typeof passwordKey !== "string" || !SECRET_KEY.test(passwordKey)) {
    throw new ReleaseError("smtp.credentialsSecret.usernameKey and passwordKey must be Secret keys");
  }
  return { host, port, security, credentialsSecret: { name, usernameKey, passwordKey } };
}

/** The Secret and key the HelmRelease writes into smtp.from (its valuesFrom entry with that targetPath). */
export function senderSource(helmRelease: unknown): SenderSource {
  const valuesFrom = isMap(helmRelease) && isMap(helmRelease.spec) && Array.isArray(helmRelease.spec.valuesFrom) ? helmRelease.spec.valuesFrom : [];
  const entries = valuesFrom.filter((entry): entry is Record<string, unknown> => isMap(entry) && entry.targetPath === "smtp.from");
  if (entries.length !== 1) throw new ReleaseError(`the HelmRelease must set smtp.from from exactly one valuesFrom entry (it has ${entries.length})`);
  const entry = entries[0]!;
  if (entry.kind !== "Secret" || typeof entry.name !== "string" || !OBJECT_NAME.test(entry.name) || typeof entry.valuesKey !== "string" || !SECRET_KEY.test(entry.valuesKey)) {
    throw new ReleaseError("the HelmRelease's smtp.from must come from a key of a Secret");
  }
  return { name: entry.name, key: entry.valuesKey };
}

function parseYaml(text: string, file: string): unknown {
  try {
    return Bun.YAML.parse(text);
  } catch {
    throw new ReleaseError(`${file} is not YAML`);
  }
}

/** Whether the commit is in origin/main's history: yes, no, or unknown (no such ref, git failed). */
export type OnMain = "yes" | "no" | "unknown";

export interface Checkout {
  /** The commit; empty in tests that pass files directly. */
  head: string;
  /** Whether `head` is in MAIN_REF (the local ref, as of the last fetch). */
  onMain: OnMain;
  smtp: SmtpSettings;
  sender: SenderSource;
  /** The overlay layers as committed, compared with what the cluster has. */
  layers: { saas: Record<string, unknown>; demo: Record<string, unknown> };
}

/** Reads the settings from a checkout's files (`read` gives a file's text by repository path). */
export function checkoutSettings(read: (path: string) => string, head: string, onMain: OnMain = "unknown"): Checkout {
  const chart = smtpLayer(parseYaml(read(CHART_VALUES), CHART_VALUES), CHART_VALUES);
  const saas = smtpLayer(parseYaml(read(SAAS_VALUES), SAAS_VALUES), SAAS_VALUES);
  const demo = smtpLayer(parseYaml(read(DEMO_VALUES), DEMO_VALUES), DEMO_VALUES);
  return { head, onMain, smtp: smtpSettings([chart, saas, demo]), sender: senderSource(parseYaml(read(HELM_RELEASE), HELM_RELEASE)), layers: { saas, demo } };
}

/**
 * The commit of this checkout and its settings. Anything uncommitted in CODE_PATHS stops the plan:
 * the confirm code then names the reviewed commit, not the files on this disk. Whether the commit
 * is on origin/main is read from the local ref, without fetching.
 */
export async function readCheckout(root: string = ROOT): Promise<Checkout> {
  const head = await tryRun(["git", "-C", root, "rev-parse", "HEAD"]);
  if (head.exitCode !== 0 || !/^[0-9a-f]{40}$/.test(head.stdout.trim())) throw new ReleaseError(`could not read the checkout's commit: ${lastLines(head.stderr)}`);
  const status = await tryRun(["git", "-C", root, "status", "--porcelain", "--", ...CODE_PATHS]);
  if (status.exitCode !== 0) throw new ReleaseError(`git status failed: ${lastLines(status.stderr)}`);
  if (status.stdout.trim() !== "") {
    throw new ReleaseError(`${CODE_PATHS.join(", ")} have uncommitted changes; run from a clean checkout of the reviewed commit:\n${lastLines(status.stdout, 10)}`);
  }
  // Exit 0: an ancestor (or the same commit); 1: not; anything else: no such ref or another error.
  const ancestor = await tryRun(["git", "-C", root, "merge-base", "--is-ancestor", "HEAD", MAIN_REF]);
  const onMain: OnMain = ancestor.exitCode === 0 ? "yes" : ancestor.exitCode === 1 ? "no" : "unknown";
  return checkoutSettings((path) => readFileSync(join(root, path), "utf8"), head.stdout.trim(), onMain);
}

/** --apply sends only from a commit that is on origin/main (reviewed and merged). */
export function requireOnMain(plan: Pick<SendPlan, "head" | "onMain">): void {
  if (plan.onMain === "yes") return;
  throw new ReleaseError(
    plan.onMain === "no"
      ? `commit ${plan.head} is not on ${MAIN_REF}: send only from a merged commit. Run git fetch origin main first if it was merged since the last fetch`
      : `could not tell whether commit ${plan.head} is on ${MAIN_REF} (no such ref here?): run git fetch origin main first`,
  );
}

// --- The recipient ---------------------------------------------------------------------------------

const LOCAL_PART = /^[A-Za-z0-9_%+-]+(?:\.[A-Za-z0-9_%+-]+)*$/;
const DOMAIN = /^(?=.{1,253}$)(?:[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?\.)+[A-Za-z]{2,63}$/;

/**
 * The recipient from what came on stdin: one address on one line. Only a plain address of letters,
 * digits and . _ % + - is accepted (no quotes, comments or display name), so the pod's shell and
 * curl see nothing they would interpret. Errors never quote the input.
 */
export function checkedRecipient(input: string): string {
  const lines = input.split(/\r?\n/).map((line) => line.trim()).filter((line) => line !== "");
  if (lines.length !== 1) throw new ReleaseError(`stdin must hold exactly one recipient address on one line (it has ${lines.length} non-empty lines)`);
  const address = lines[0]!;
  const at = address.lastIndexOf("@");
  const local = address.slice(0, at);
  const domain = address.slice(at + 1);
  if (at < 1 || address.length > 254 || local.length > 64 || !LOCAL_PART.test(local) || !DOMAIN.test(domain)) {
    throw new ReleaseError("the recipient is not a plain email address (letters, digits and . _ % + - before the @, a DNS name after it); it is not printed here");
  }
  return address;
}

// --- The pod ---------------------------------------------------------------------------------------

/**
 * The awk program that stands between curl's verbose output and the container's stdout. It prints
 * only `TESTMAIL ...` lines: the reply code (and the enhanced status code) of the last line of
 * each SMTP reply, the verb of each command (AUTH with its mechanism only, never its argument), the
 * TLS version and cipher, whether curl verified the certificate, and curl's error number. Reply
 * texts (the provider's message ID among them), addresses, the AUTH argument, host names and IP
 * addresses never pass it.
 */
export const OUTPUT_FILTER = String.raw`
function out(s) { print "TESTMAIL " s; fflush() }
{ sub(/\r$/, "") }
/^< [0-9][0-9][0-9] / {
  code = substr($0, 3, 3); enh = ""; rest = substr($0, 7)
  if (match(rest, /^[245]\.[0-9][0-9]?[0-9]?\.[0-9][0-9]?[0-9]?/)) {
    after = substr(rest, RLENGTH + 1, 1)
    if (after == "" || after == " ") enh = " " substr(rest, 1, RLENGTH)
  }
  out("S " code enh); next
}
/^< / { next }
/^> / {
  cmd = toupper(substr($0, 3))
  if (cmd ~ /^(EHLO|HELO)( |$)/) out("C EHLO")
  else if (cmd == "STARTTLS") out("C STARTTLS")
  else if (cmd ~ /^AUTH [A-Z0-9_-]+( |$)/) { split(cmd, w, " "); out("C AUTH " w[2]) }
  else if (cmd ~ /^MAIL FROM:/) out("C MAIL FROM")
  else if (cmd ~ /^RCPT TO:/) out("C RCPT TO")
  else if (cmd ~ /^(DATA|QUIT|RSET|NOOP)$/) out("C " cmd)
  else out("C (other)")
  next
}
/^\* SSL connection using / {
  s = $0; sub(/^\* SSL connection using /, "", s); split(s, p, " / ")
  v = p[1]; c = p[2]
  if (v !~ /^[A-Za-z0-9._-]+$/) v = "?"
  if (c !~ /^[A-Za-z0-9._-]+$/) c = "?"
  out("TLS " v " " c); next
}
/^\* *SSL certificate (verify ok|verified)/ { out("TLS certificate-verified"); next }
/^curl: \([0-9][0-9]*\)/ { s = $0; sub(/^curl: \(/, "", s); sub(/\).*$/, "", s); out("CURL_ERROR " s); next }
`.trim();

/**
 * The pod's shell script (BusyBox sh). Positional arguments: host, port, security, run ID and the
 * awk filter; the credentials and the sender arrive as env variables from Secrets, the recipient as
 * the first line of stdin. Only shell builtins handle the recipient and the sender before curl:
 * `read`, parameter expansion and a here-document piped to curl as the message. curl takes the
 * envelope addresses and the credentials through --variable (from the environment) and --expand-*,
 * so none of them is ever an argument of any process. --ssl-reqd makes curl stop unless the
 * session is encrypted (STARTTLS on smtp://, implicit TLS on smtps://), and it verifies the
 * server's certificate against the image's CA bundle.
 */
export const POD_SCRIPT = String.raw`
set -eu
set -o pipefail
host=$1; port=$2; security=$3; run=$4; filter=$5
say() { echo "TESTMAIL $*"; }
valid_address() {
  case "$1" in
    ''|*[!A-Za-z0-9._%+@-]*|*@*@*|@*|*@|*.@*|*@.*|*..*|.*|*.) return 1 ;;
    *@*.*) return 0 ;;
    *) return 1 ;;
  esac
}
case "$security" in
  starttls) url="smtp://$host:$port" ;;
  tls) url="smtps://$host:$port" ;;
  *) say "REFUSED security"; exit 64 ;;
esac
TESTMAIL_RCPT=
if ! IFS= read -r -t ${STDIN_WAIT_SECONDS} TESTMAIL_RCPT; then
  [ -n "$TESTMAIL_RCPT" ] || { say "REFUSED no-recipient"; exit 65; }
fi
valid_address "$TESTMAIL_RCPT" || { say "REFUSED recipient"; exit 65; }
nl='
'
cr=$(printf '\r')
from=${"$"}{TESTMAIL_FROM:-}
case "$from" in ''|*"$nl"*|*"$cr"*) say "REFUSED sender"; exit 66 ;; esac
case "$from" in
  *'<'*'>') envelope=${"$"}{from##*<}; envelope=${"$"}{envelope%>} ;;
  *) envelope=$from ;;
esac
valid_address "$envelope" || { say "REFUSED sender"; exit 66; }
[ -n "${"$"}{TESTMAIL_SMTP_USERNAME:-}" ] && [ -n "${"$"}{TESTMAIL_SMTP_PASSWORD:-}" ] || { say "REFUSED credentials"; exit 67; }
TESTMAIL_ENVELOPE_FROM=$envelope
export TESTMAIL_RCPT TESTMAIL_ENVELOPE_FROM
date=$(date -R)
message() {
  cat <<EOF
From: $from
To: $TESTMAIL_RCPT
Subject: TrustIoT demo SMTP test $run
Date: $date
MIME-Version: 1.0
Content-Type: text/plain; charset=us-ascii
Content-Transfer-Encoding: 7bit

A test message from the TrustIoT demo environment (Laysi/trustiot-v2#468).
It checks that SPF and DKIM pass for the demo sending domain; no reply is needed.
Run: $run
EOF
}
rc=0
message | curl -q -sS -v --crlf --proto =smtp,smtps --ssl-reqd --tlsv1.2 \
  --connect-timeout ${CONNECT_TIMEOUT_SECONDS} --max-time ${SESSION_TIMEOUT_SECONDS} \
  --variable %TESTMAIL_SMTP_USERNAME --variable %TESTMAIL_SMTP_PASSWORD \
  --expand-user '{{TESTMAIL_SMTP_USERNAME:trim}}:{{TESTMAIL_SMTP_PASSWORD:trim}}' \
  --variable %TESTMAIL_ENVELOPE_FROM --expand-mail-from '{{TESTMAIL_ENVELOPE_FROM}}' \
  --variable %TESTMAIL_RCPT --expand-mail-rcpt '{{TESTMAIL_RCPT}}' \
  --upload-file - --url "$url" 2>&1 | awk "$filter" || rc=$?
say "EXIT $rc"
exit "$rc"
`.trim();

export interface PodInput {
  name: string;
  run: string;
  smtp: SmtpSettings;
  sender: SenderSource;
}

/** The pod's command line: no secret, no address. */
export function podCommand(smtp: SmtpSettings, run: string): string[] {
  return ["/bin/sh", "-c", POD_SCRIPT, "test-mail", smtp.host, String(smtp.port), smtp.security, run, OUTPUT_FILTER];
}

const secretEnv = (name: string, secret: string, key: string) => ({ name, valueFrom: { secretKeyRef: { name: secret, key } } });

/**
 * The test pod. Pod Security `restricted`: non-root with the image's numeric UID and GID, no
 * privilege escalation, every capability dropped, RuntimeDefault seccomp, read-only root file
 * system (it writes nothing: the message is a pipe). No ServiceAccount token, no Service links.
 * stdin stays open for one attach (stdinOnce), so the recipient goes in through kubectl attach.
 * activeDeadlineSeconds bounds the pod's life even when this script dies.
 */
export function podManifest(input: PodInput): Record<string, unknown> {
  const { smtp, sender } = input;
  return {
    apiVersion: "v1",
    kind: "Pod",
    metadata: { name: input.name, namespace: DEMO_NAMESPACE, labels: { ...POD_LABELS } },
    spec: {
      restartPolicy: "Never",
      activeDeadlineSeconds: POD_DEADLINE_SECONDS,
      terminationGracePeriodSeconds: 5,
      automountServiceAccountToken: false,
      enableServiceLinks: false,
      securityContext: { runAsNonRoot: true, runAsUser: IMAGE_UID, runAsGroup: IMAGE_GID, seccompProfile: { type: "RuntimeDefault" } },
      containers: [
        {
          name: CONTAINER,
          image: IMAGE,
          imagePullPolicy: "IfNotPresent",
          command: podCommand(smtp, input.run),
          stdin: true,
          stdinOnce: true,
          tty: false,
          env: [
            secretEnv("TESTMAIL_SMTP_USERNAME", smtp.credentialsSecret.name, smtp.credentialsSecret.usernameKey),
            secretEnv("TESTMAIL_SMTP_PASSWORD", smtp.credentialsSecret.name, smtp.credentialsSecret.passwordKey),
            secretEnv("TESTMAIL_FROM", sender.name, sender.key),
          ],
          resources: { requests: { cpu: "10m", memory: "16Mi" }, limits: { cpu: "200m", memory: "64Mi" } },
          securityContext: {
            allowPrivilegeEscalation: false,
            privileged: false,
            readOnlyRootFilesystem: true,
            runAsNonRoot: true,
            runAsUser: IMAGE_UID,
            runAsGroup: IMAGE_GID,
            capabilities: { drop: ["ALL"] },
            seccompProfile: { type: "RuntimeDefault" },
          },
        },
      ],
    },
  };
}

/** A run ID: UTC time and four random hex digits, lowercase, so `<prefix><id>` is a valid pod name. */
export function runId(now: Date = new Date()): string {
  const stamp = now.toISOString().replace(/[-:]/g, "").replace(/\.\d+Z$/, "").replace("T", "t").toLowerCase();
  return `${stamp}-${crypto.getRandomValues(new Uint8Array(2)).reduce((s, b) => s + b.toString(16).padStart(2, "0"), "")}`;
}

// --- The pod's output ------------------------------------------------------------------------------

/** Every line the workstation accepts from the pod; anything else is dropped and only counted. */
const ALLOWED_LINE =
  /^TESTMAIL (?:S [0-9]{3}(?: [245]\.[0-9]{1,3}\.[0-9]{1,3})?|C (?:EHLO|STARTTLS|AUTH [A-Z0-9_-]{1,32}|MAIL FROM|RCPT TO|DATA|QUIT|RSET|NOOP|\(other\))|TLS (?:certificate-verified|(?:[A-Za-z0-9._-]{1,40}|\?) (?:[A-Za-z0-9._-]{1,80}|\?))|CURL_ERROR [0-9]{1,3}|EXIT [0-9]{1,3}|REFUSED [a-z-]{1,32})$/;

/** The pod's log, cut down to allowed lines (a second filter after the pod's own awk). */
export function filterOutput(text: string): { lines: string[]; dropped: number } {
  const all = text.split(/\r?\n/).filter((line) => line.trim() !== "");
  const lines = all.filter((line) => ALLOWED_LINE.test(line));
  return { lines, dropped: all.length - lines.length };
}

export interface Stage {
  step: string;
  reply: string;
}

export interface MailOutcome {
  stages: Stage[];
  tls: string | undefined;
  certificateVerified: boolean;
  exit: number | undefined;
  /** The container's exit code from the pod's terminated state. */
  podExit: number | undefined;
  curlError: number | undefined;
  refused: string | undefined;
  accepted: boolean;
}

/** What curl's error numbers mean here (curl's man page, EXIT CODES). */
export const CURL_ERRORS: Readonly<Record<number, string>> = {
  6: "could not resolve the SMTP host (DNS from the pod)",
  7: "could not connect to the SMTP host (the port may be blocked on the way out: try 2587 with starttls, or 465 with tls)",
  28: "timed out",
  35: "the TLS handshake failed",
  55: "sending failed",
  56: "receiving failed (the connection broke)",
  60: "the server's certificate did not verify",
  64: "the server does not offer TLS (--ssl-reqd)",
  67: "the server refused the login (username or API key)",
};

/**
 * Pairs each reply with the command it answers (the first with the greeting; the reply after the
 * 354 to DATA is the message's). The mail counts as accepted only when curl ended with 0, TLS was
 * in use with a verified certificate, the login got 235, MAIL FROM and RCPT TO got 250 or 251,
 * DATA 354 and the message 250, and the container itself ended with 0 (`podExit`, from the pod's
 * terminated state; the TESTMAIL EXIT line alone is the script's own report).
 */
export function mailOutcome(lines: readonly string[], security: Security, podExit: number | undefined): MailOutcome {
  const stages: Stage[] = [];
  let pending: string | undefined = "greeting";
  let tls: string | undefined;
  let certificateVerified = false;
  let exit: number | undefined;
  let curlError: number | undefined;
  let refused: string | undefined;
  for (const line of lines) {
    const [, kind = "", ...rest] = line.split(" ");
    const value = rest.join(" ");
    if (kind === "S") {
      stages.push({ step: pending ?? "(unsolicited)", reply: value });
      pending = pending === "DATA" && value.startsWith("354") ? "message" : undefined;
    } else if (kind === "C") {
      pending = value === "(other)" ? "AUTH (continued)" : value;
    } else if (kind === "TLS") {
      if (value === "certificate-verified") certificateVerified = true;
      else tls = value;
    } else if (kind === "EXIT") exit = Number(value);
    else if (kind === "CURL_ERROR") curlError = Number(value);
    else if (kind === "REFUSED") refused = value;
  }
  const replied = (step: string, ok: RegExp) => stages.some((stage) => stage.step === step && ok.test(stage.reply));
  const authenticated = stages.some((stage) => (stage.step.startsWith("AUTH ") || stage.step === "AUTH (continued)") && /^235\b/.test(stage.reply));
  const accepted =
    exit === 0 &&
    podExit === 0 &&
    tls !== undefined &&
    certificateVerified &&
    (security === "tls" || replied("STARTTLS", /^220\b/)) &&
    authenticated &&
    replied("MAIL FROM", /^250\b/) &&
    replied("RCPT TO", /^25[01]\b/) &&
    replied("DATA", /^354\b/) &&
    replied("message", /^250\b/);
  return { stages, tls, certificateVerified, exit, podExit, curlError, refused, accepted };
}

// --- The plan --------------------------------------------------------------------------------------

export interface NetworkPolicyFact {
  name: string;
  types: string[];
  /** Whether its podSelector selects the test pod: yes, no, or maybe (matchExpressions). */
  selectsTestPod: "yes" | "no" | "maybe";
}

export interface SendPlan {
  head: string;
  onMain: OnMain;
  namespace: string;
  image: string;
  smtp: SmtpSettings;
  sender: SenderSource;
  /** The pod as it will be created, with `<run>` for the run ID (in the name and the subject). */
  manifestSha256: string;
  podSecurityEnforce: string;
  networkPolicies: NetworkPolicyFact[];
  authorizationPolicies: string[];
  ztunnel: string[];
  leftovers: string[];
}

/** Whether a NetworkPolicy's podSelector selects a pod with these labels. */
export function selects(podSelector: unknown, labels: Readonly<Record<string, string>>): "yes" | "no" | "maybe" {
  if (!isMap(podSelector)) return "yes";
  const matchLabels = isMap(podSelector.matchLabels) ? podSelector.matchLabels : {};
  const labelsMatch = Object.entries(matchLabels).every(([key, value]) => labels[key] === value);
  if (!labelsMatch) return "no";
  return Array.isArray(podSelector.matchExpressions) && podSelector.matchExpressions.length > 0 ? "maybe" : "yes";
}

export function networkPolicyFacts(items: readonly Record<string, unknown>[]): NetworkPolicyFact[] {
  return items
    .map((item) => {
      const spec = isMap(item.spec) ? item.spec : {};
      const declared = Array.isArray(spec.policyTypes) ? spec.policyTypes.map(String) : [];
      // Without policyTypes, Kubernetes infers Egress from an egress section.
      const types = declared.length > 0 ? declared : ["Ingress", ...(spec.egress !== undefined ? ["Egress"] : [])];
      return { name: String(isMap(item.metadata) ? item.metadata.name : "?"), types: [...types].sort(), selectsTestPod: selects(spec.podSelector, POD_LABELS) };
    })
    .sort((a, b) => a.name.localeCompare(b.name));
}

/** Key names of a Secret only: the go-template never prints a value, so none crosses ssh. */
export const SECRET_KEYS_TEMPLATE = '{{range $k, $v := .data}}{{$k}}{{"\\n"}}{{end}}';

async function secretKeys(box: Box, name: string): Promise<string[] | undefined> {
  const result = await box.tryKubectl(["-n", DEMO_NAMESPACE, "get", "secret", name, "-o", `go-template=${SECRET_KEYS_TEMPLATE}`]);
  if (result.exitCode !== 0) {
    if (/\(NotFound\)/.test(result.stderr)) return undefined;
    throw new ReleaseError(`reading the key names of Secret ${name} failed: ${lastLines(result.stderr)}`);
  }
  return result.stdout.split("\n").filter((key) => key !== "").sort();
}

/** The deployed overlay layers (the ConfigMap Flux generates from this directory). */
async function deployedLayers(box: Box): Promise<{ saas: Record<string, unknown>; demo: Record<string, unknown> } | undefined> {
  const configMap = await box.getJson(["-n", DEMO_NAMESPACE, "get", "configmap", VALUES_CONFIGMAP]);
  if (configMap === undefined) return undefined;
  const data = isMap(configMap.data) ? configMap.data : {};
  const layer = (key: string) => smtpLayer(parseYaml(String(data[key] ?? ""), `ConfigMap ${VALUES_CONFIGMAP} key ${key}`) ?? {}, `ConfigMap ${VALUES_CONFIGMAP} key ${key}`);
  return { saas: layer("values-saas.yaml"), demo: layer("values-demo.yaml") };
}

export interface PlanEnv {
  checkout: () => Promise<Checkout>;
}

/**
 * The plan: the checkout's settings, the pod, and what the cluster has (read only). Refuses when
 * the namespace does not enforce Pod Security `restricted`, the deployed values differ from the
 * checkout's, or a Secret or key the pod needs is missing.
 */
export async function makePlan(box: Box, log: Log, env: PlanEnv): Promise<SendPlan> {
  const checkout = await env.checkout();
  const namespace = await box.getJson(["get", "namespace", DEMO_NAMESPACE]);
  if (namespace === undefined) throw new ReleaseError(`namespace ${DEMO_NAMESPACE} does not exist: deploy the demo environment first (Laysi/trustiot-v2#467)`);
  const labels = isMap(namespace.metadata) && isMap(namespace.metadata.labels) ? namespace.metadata.labels : {};
  const enforce = String(labels["pod-security.kubernetes.io/enforce"] ?? "");
  if (enforce !== "restricted") throw new ReleaseError(`namespace ${DEMO_NAMESPACE} enforces Pod Security "${enforce || "none"}", not "restricted"; the demo namespace must (deploy/flux/clusters/demo/trustiot/namespace.yaml)`);

  const deployed = await deployedLayers(box);
  if (deployed === undefined) throw new ReleaseError(`ConfigMap ${VALUES_CONFIGMAP} is not in ${DEMO_NAMESPACE}: Flux has not deployed the demo values yet`);
  for (const layer of ["saas", "demo"] as const) {
    if (stableJson(deployed[layer]) !== stableJson(checkout.layers[layer])) {
      throw new ReleaseError(`the deployed values-${layer}.yaml has other smtp settings than this checkout: Flux has not applied this commit yet, or the checkout is not the deployed one`);
    }
  }

  const smtpKeys = await secretKeys(box, checkout.smtp.credentialsSecret.name);
  const { usernameKey, passwordKey, name: smtpSecret } = checkout.smtp.credentialsSecret;
  if (smtpKeys === undefined) throw new ReleaseError(`Secret ${smtpSecret} is not in ${DEMO_NAMESPACE}: add ${smtpSecret}.sops.yaml (deploy/flux/README.md 「展示叢集」) and let Flux apply it`);
  for (const key of [usernameKey, passwordKey]) if (!smtpKeys.includes(key)) throw new ReleaseError(`Secret ${smtpSecret} has no key ${key}`);
  const senderKeys = await secretKeys(box, checkout.sender.name);
  if (senderKeys === undefined || !senderKeys.includes(checkout.sender.key)) throw new ReleaseError(`Secret ${checkout.sender.name} has no key ${checkout.sender.key} (the sender)`);

  const networkPolicies = networkPolicyFacts(await box.items(["-n", DEMO_NAMESPACE, "get", "networkpolicies.networking.k8s.io"]));
  const authorizationPolicies = (await box.items(["-n", DEMO_NAMESPACE, "get", "authorizationpolicies.security.istio.io"])).map((item) => String(isMap(item.metadata) ? item.metadata.name : "?")).sort();
  const ztunnel = (await box.items(["get", "daemonsets.apps", "-A", "--field-selector", "metadata.name=ztunnel"]))
    .map((item) => (isMap(item.metadata) ? `${String(item.metadata.namespace)}/${String(item.metadata.name)}` : "?"))
    .sort();
  const leftovers = (await box.items(["-n", DEMO_NAMESPACE, "get", "pods", "-l", POD_SELECTOR]))
    .map((item) => String(isMap(item.metadata) ? item.metadata.name : ""))
    .filter((name) => name.startsWith(POD_PREFIX))
    .sort();

  const template = podManifest({ name: `${POD_PREFIX}<run>`, run: "<run>", smtp: checkout.smtp, sender: checkout.sender });
  const plan: SendPlan = {
    head: checkout.head,
    onMain: checkout.onMain,
    namespace: DEMO_NAMESPACE,
    image: IMAGE,
    smtp: checkout.smtp,
    sender: checkout.sender,
    manifestSha256: new Bun.CryptoHasher("sha256").update(stableJson(template)).digest("hex"),
    podSecurityEnforce: enforce,
    networkPolicies,
    authorizationPolicies,
    ztunnel,
    leftovers,
  };
  describePlan(plan, log);
  log.save("plan", plan);
  return plan;
}

/** The plan as the operator reads it. */
export function describePlan(plan: SendPlan, log: Log): void {
  const { smtp, sender } = plan;
  log.note(`plan: commit ${plan.head}; one pod ${POD_PREFIX}<run> in namespace ${plan.namespace} (Pod Security ${plan.podSecurityEnforce}), image ${plan.image}`);
  log.note(
    `  the commit is on ${MAIN_REF} (local ref): ${plan.onMain}${plan.onMain === "yes" ? "" : `; --apply refuses until it is (git fetch origin main first${plan.onMain === "no" ? ", and send only from a merged commit" : ""})`}`,
  );
  log.note(`  SMTP ${smtp.host}:${smtp.port} with ${smtp.security === "starttls" ? "STARTTLS (smtp:// and --ssl-reqd)" : "implicit TLS (smtps://)"}; login from Secret ${smtp.credentialsSecret.name} keys ${smtp.credentialsSecret.usernameKey} and ${smtp.credentialsSecret.passwordKey}; sender from Secret ${sender.name} key ${sender.key}`);
  log.note("  the recipient comes from stdin when --apply runs, reaches the pod only on its stdin, and is never printed or saved");
  const egress = plan.networkPolicies.filter((policy) => policy.types.includes("Egress"));
  log.note(
    `  NetworkPolicies in ${plan.namespace}: ${plan.networkPolicies.length === 0 ? "none" : plan.networkPolicies.map((p) => `${p.name} (${p.types.join("+")}, selects the test pod: ${p.selectsTestPod})`).join(", ")}`,
  );
  log.note(
    egress.length === 0
      ? "  no NetworkPolicy restricts egress: the pod's connection leaves as a service's would (the chart's policies restrict ingress only)"
      : `  egress is restricted by: ${egress.map((p) => `${p.name} (selects the test pod: ${p.selectsTestPod})`).join(", ")}; the test follows the same rules as a service would, and this script changes none of them`,
  );
  log.note(`  AuthorizationPolicies in ${plan.namespace}: ${plan.authorizationPolicies.length} (they govern connections to the chart's workloads; none selects the test pod's labels)`);
  log.note(
    plan.ztunnel.length > 0
      ? `  Istio ambient: ztunnel runs (${plan.ztunnel.join(", ")}); the pod carries istio.io/dataplane-mode=ambient like the chart's services, so its connection goes out through ztunnel (passthrough to a host outside the mesh)`
      : "  Istio ambient: no ztunnel DaemonSet found, so the ambient label has no effect; the connection goes out directly, as a service's would",
  );
  log.note(plan.leftovers.length === 0 ? "  no leftover test pod" : `  leftover test pods, deleted before the run: ${plan.leftovers.join(", ")}`);
}

// --- Interrupts ------------------------------------------------------------------------------------

/** The signals that clean up before exiting, with their numbers (exit status 128 + number). */
export const SIGNAL_NUMBERS = { SIGHUP: 1, SIGINT: 2, SIGTERM: 15 } as const;
export type SignalName = keyof typeof SIGNAL_NUMBERS;

/** Where signals come from and how the process ends; the real one is `process`. */
export interface SignalSource {
  on(signal: SignalName, handler: () => void): void;
  off(signal: SignalName, handler: () => void): void;
  exit(code: number): void;
}

const PROCESS_SIGNALS: SignalSource = {
  on: (signal, handler) => void process.on(signal, handler),
  off: (signal, handler) => void process.off(signal, handler),
  exit: (code) => process.exit(code),
};

/** Runs `step` at most once; every later call gets the same promise. */
function once(step: () => Promise<void>): () => Promise<void> {
  let running: Promise<void> | undefined;
  return () => (running ??= step());
}

/**
 * Cleanup on SIGINT, SIGTERM and SIGHUP while the lab lock is held. The first signal runs the
 * guarded steps, the last guarded first (the pod before the lock), then exits with 128 + the
 * signal's number; a second signal exits at once. The normal path runs the same steps through the
 * functions guard() returns, so each step runs once, whichever path gets there first; after a
 * signal, check() stops the normal path from starting anything new.
 */
export class Interrupts {
  private readonly steps: (() => Promise<void>)[] = [];
  private readonly handlers = new Map<SignalName, () => void>();
  private signal: SignalName | undefined;
  private cleanup: Promise<void> | undefined;

  constructor(
    private readonly log: Log,
    private readonly source: SignalSource = PROCESS_SIGNALS,
  ) {
    for (const signal of Object.keys(SIGNAL_NUMBERS) as SignalName[]) {
      const handler = () => this.received(signal);
      this.handlers.set(signal, handler);
      source.on(signal, handler);
    }
  }

  /** Registers a cleanup step and returns it, wrapped to run once. */
  guard(step: () => Promise<void>): () => Promise<void> {
    const wrapped = once(step);
    this.steps.push(wrapped);
    return wrapped;
  }

  /** Throws once a signal came: the normal path starts nothing new while the cleanup runs. */
  check(): void {
    if (this.signal !== undefined) throw new ReleaseError(`interrupted by ${this.signal}`);
  }

  /** The normal path's end: after a signal, waits for its cleanup and exit; otherwise removes the handlers. */
  async finish(): Promise<void> {
    if (this.cleanup !== undefined) await this.cleanup;
    for (const [signal, handler] of this.handlers) this.source.off(signal, handler);
    this.handlers.clear();
  }

  private received(signal: SignalName): void {
    const code = 128 + SIGNAL_NUMBERS[signal];
    if (this.signal !== undefined) {
      this.log.note(`${signal} again: exiting at once (the pod stops at its activeDeadlineSeconds and the next run deletes it; remove the lab lock by hand if it is still this run's)`);
      this.source.exit(code);
      return;
    }
    this.signal = signal;
    this.log.note(`${signal}: deleting the test pod (if it was created) and releasing the lab lock before exiting; a second signal exits at once`);
    this.cleanup = (async () => {
      for (const step of [...this.steps].reverse()) {
        try {
          await step();
        } catch (error) {
          this.log.note(`WARNING: a cleanup step failed: ${errorMessage(error)}`);
        }
      }
      this.source.exit(code);
    })();
  }
}

// --- The run ---------------------------------------------------------------------------------------

interface PodState {
  phase: string;
  waiting: string;
  terminated: { exitCode: number; reason: string } | undefined;
}

export function podState(pod: Record<string, unknown> | undefined): PodState | undefined {
  if (pod === undefined) return undefined;
  const status = isMap(pod.status) ? pod.status : {};
  const statuses = Array.isArray(status.containerStatuses) ? status.containerStatuses.filter(isMap) : [];
  const container = statuses.find((s) => s.name === CONTAINER) ?? {};
  const state = isMap(container.state) ? container.state : {};
  const waiting = isMap(state.waiting) ? String(state.waiting.reason ?? "") : "";
  const terminated = isMap(state.terminated) ? { exitCode: Number(state.terminated.exitCode), reason: String(state.terminated.reason ?? "") } : undefined;
  return { phase: String(status.phase ?? ""), waiting, terminated };
}

/** Waiting reasons that will not go away by waiting longer. */
const STUCK = new Set(["ErrImagePull", "ImagePullBackOff", "InvalidImageName", "CreateContainerConfigError", "CreateContainerError", "RunContainerError"]);

async function readPod(box: Box, name: string): Promise<PodState | undefined> {
  return podState(await box.getJson(["-n", DEMO_NAMESPACE, "get", "pod", name]));
}

/** Deletes the test pod and checks that it is gone; never throws, says how to delete it by hand. */
async function deletePod(box: Box, log: Log, name: string): Promise<void> {
  const removed = await box.tryKubectl(["-n", DEMO_NAMESPACE, "delete", "pod", name, "--ignore-not-found", "--wait=true", "--timeout=60s"]);
  const gone = (await box.tryKubectl(["-n", DEMO_NAMESPACE, "get", "pod", name, "--ignore-not-found", "-o", "name"])).stdout.trim() === "";
  if (removed.exitCode === 0 && gone) log.note(`deleted pod ${name}`);
  else log.note(`WARNING: pod ${name} may still be there; delete it with: sudo k3s kubectl -n ${DEMO_NAMESPACE} delete pod ${name} (it holds no recipient; its log holds only the filtered lines)`);
}

/**
 * Creates the pod, gives it the recipient over attach, waits for it to end and reads its (already
 * filtered) log; deletes the pod in every case, also on a signal (`interrupts`, registered before
 * the pod is created, so a create that a signal cuts short is covered too). Returns the filtered
 * output and the container's exit code.
 */
export async function sendTestMail(
  box: Box,
  log: Log,
  plan: SendPlan,
  recipient: string,
  run: string,
  clock: Clock = REAL_CLOCK,
  interrupts?: Interrupts,
): Promise<{ lines: string[]; dropped: number; exitCode: number | undefined }> {
  const name = `${POD_PREFIX}${run}`;
  const check = () => interrupts?.check();
  for (const leftover of plan.leftovers) {
    check();
    await box.kubectl(["-n", DEMO_NAMESPACE, "delete", "pod", leftover, "--ignore-not-found", "--wait=true", "--timeout=60s"]);
    log.note(`deleted the leftover test pod ${leftover}`);
  }
  const manifest = podManifest({ name, run, smtp: plan.smtp, sender: plan.sender });
  check();
  // A signal can come while the create is on its way: the cleanup waits for it to settle (created
  // or not) before it deletes, so it never deletes before the pod exists and leaves it behind.
  let creating: Promise<unknown> | undefined;
  const cleanup = async () => {
    await creating?.catch(() => {});
    await deletePod(box, log, name);
  };
  const removePod = interrupts?.guard(cleanup) ?? once(cleanup);
  try {
    creating = box.kubectl(["create", "-f", "-"], JSON.stringify(manifest));
    await creating;
    log.note(`created pod ${name}; waiting for it to run (the first run pulls the image)`);
    const runningBy = clock.now() + RUNNING_WAIT_MS;
    for (;;) {
      check();
      const state = await readPod(box, name);
      if (state?.phase === "Running") break;
      if (state?.terminated !== undefined || state?.phase === "Failed" || state?.phase === "Succeeded") {
        throw new ReleaseError(`pod ${name} ended before it got the recipient (phase ${state.phase}, ${state.terminated?.reason ?? "no reason"})`);
      }
      if (state !== undefined && STUCK.has(state.waiting)) throw new ReleaseError(`pod ${name} cannot start: ${state.waiting} (kubectl -n ${DEMO_NAMESPACE} describe pod ${name} says why)`);
      if (clock.now() > runningBy) throw new ReleaseError(`pod ${name} did not run within ${RUNNING_WAIT_MS / 60_000} minutes (phase ${state?.phase || "unknown"}${state?.waiting ? `, ${state.waiting}` : ""})`);
      await clock.sleep(POLL_MS);
    }
    // The recipient only ever goes into this attach's stdin. No retry: the pod reads stdin once.
    check();
    const attach = await box.sh(["sudo", "k3s", "kubectl", "-n", DEMO_NAMESPACE, "attach", "-i", "-c", CONTAINER, name].map(shq).join(" "), { stdin: `${recipient}\n`, retry: false });
    if (attach.exitCode !== 0) log.note(`kubectl attach ended with exit ${attach.exitCode}; reading the pod's log`);
    const doneBy = clock.now() + DONE_WAIT_MS;
    let state = await readPod(box, name);
    while (state?.terminated === undefined) {
      check();
      if (clock.now() > doneBy) throw new ReleaseError(`pod ${name} did not end within ${DONE_WAIT_MS / 60_000} minutes of the attach`);
      await clock.sleep(POLL_MS);
      state = await readPod(box, name);
    }
    const logs = await box.kubectl(["-n", DEMO_NAMESPACE, "logs", name, "-c", CONTAINER]);
    return { ...filterOutput(logs), exitCode: state.terminated.exitCode };
  } finally {
    await removePod();
  }
}

/** Prints the outcome; throws unless the mail was accepted. */
export function report(log: Log, result: { lines: string[]; dropped: number; exitCode: number | undefined }, security: Security): MailOutcome {
  const outcome = mailOutcome(result.lines, security, result.exitCode);
  log.note("SMTP session (reply codes only; addresses, domains, message IDs and credentials are not in the pod's output):");
  for (const stage of outcome.stages) log.note(`  ${stage.step}: ${stage.reply}`);
  log.note(`  TLS: ${outcome.tls ?? "none seen"}${outcome.certificateVerified ? ", certificate verified" : ""}`);
  if (result.dropped > 0) log.note(`  (${result.dropped} other line(s) of the pod's log were dropped unread)`);
  if (outcome.refused !== undefined) log.note(`  the pod refused to send: ${outcome.refused}`);
  if (outcome.curlError !== undefined) log.note(`  curl error ${outcome.curlError}: ${CURL_ERRORS[outcome.curlError] ?? "see curl's EXIT CODES"}`);
  if (outcome.exit !== outcome.podExit) log.note(`  the container ended with ${outcome.podExit ?? "an unknown code"}, the script in it reported ${outcome.exit ?? "nothing"}`);
  log.save("result", { at: new Date().toISOString(), security, ...outcome, droppedLines: result.dropped });
  if (!outcome.accepted) throw new ReleaseError("the relay did not accept the test mail (see the stages above)");
  log.note("accepted: the relay took the message (250 after DATA). Read the received message's headers with: bun scripts/demo/test-mail.ts headers < <file>");
  return outcome;
}

// --- Received headers ------------------------------------------------------------------------------

export interface Header {
  name: string;
  value: string;
}

/** The header block of a raw message (up to the first empty line), unfolded. */
export function parseHeaders(raw: string): Header[] {
  const headers: Header[] = [];
  for (const line of raw.replace(/\r\n/g, "\n").split("\n")) {
    if (line.trim() === "") {
      if (headers.length > 0) break;
      continue;
    }
    if (/^[ \t]/.test(line)) {
      const last = headers.at(-1);
      if (last !== undefined) last.value += ` ${line.trim()}`;
      continue;
    }
    const colon = line.indexOf(":");
    if (colon <= 0) continue;
    headers.push({ name: line.slice(0, colon).trim().toLowerCase(), value: line.slice(colon + 1).trim() });
  }
  return headers;
}

function domainOf(text: string): string | undefined {
  const at = text.lastIndexOf("@");
  const domain = (at >= 0 ? text.slice(at + 1) : text).replace(/[>;,\s].*$/, "").replace(/\.$/, "").toLowerCase();
  return DOMAIN.test(domain) ? domain : undefined;
}

/** The From header's domain: the address in the last <...>, or the whole value. */
export function fromDomain(headers: readonly Header[]): string | undefined {
  const from = headers.find((h) => h.name === "from")?.value;
  if (from === undefined) return undefined;
  const angle = /<([^<>]*)>\s*$/.exec(from);
  return domainOf(angle?.[1] ?? from);
}

export type Relation = "same" | "subdomain" | "parent" | "same-parent" | "other";

/**
 * How a domain relates to the From domain. DMARC's relaxed alignment compares organizational
 * domains, which needs the Public Suffix List; this approximates it with the last two labels
 * (right for the demo's sending domain), and the receiver's own DMARC result is the authority.
 */
export function relation(domain: string, from: string): Relation {
  if (domain === from) return "same";
  if (domain.endsWith(`.${from}`)) return "subdomain";
  if (from.endsWith(`.${domain}`)) return "parent";
  const base = (name: string) => name.split(".").slice(-2).join(".");
  return base(domain) === base(from) ? "same-parent" : "other";
}

/** Placeholders for domains, numbered for the ones unrelated to From; the same domain gets the same one. */
export class Placeholders {
  private readonly others = new Map<string, string>();
  constructor(private readonly from: string | undefined) {}

  name(domain: string | undefined): string {
    if (domain === undefined) return "<none>";
    if (this.from === undefined) return this.other(domain);
    switch (relation(domain, this.from)) {
      case "same":
        return "<from-domain>";
      case "subdomain":
        return "<subdomain of from-domain>";
      case "parent":
        return "<parent of from-domain>";
      case "same-parent":
        return "<sibling of from-domain>";
      default:
        return this.other(domain);
    }
  }

  alignment(domain: string | undefined): string {
    if (domain === undefined || this.from === undefined) return "unknown";
    const r = relation(domain, this.from);
    return r === "same" ? "aligned (strict and relaxed)" : r === "other" ? "not aligned" : "aligned (relaxed only)";
  }

  private other(domain: string): string {
    let name = this.others.get(domain);
    if (name === undefined) {
      name = `<other-domain-${this.others.size + 1}>`;
      this.others.set(domain, name);
    }
    return name;
  }
}

export interface AuthResult {
  method: string;
  result: string;
  props: Record<string, string>;
  /** The DMARC policy from its comment, e.g. `p=NONE sp=NONE dis=NONE`. */
  policy: string | undefined;
}

const TOKEN = /^[A-Za-z0-9._-]{1,64}$/;

/**
 * The results of one Authentication-Results value (RFC 8601), after the authserv-id. Microsoft
 * writes no authserv-id (the value starts with a result); authserv is then "".
 */
export function parseAuthResults(value: string): { authserv: string; results: AuthResult[] } {
  let text = value.replace(/^\s*i=\d+\s*;\s*/, "");
  const parts = text.split(";");
  const startsWithResult = /^\s*[A-Za-z0-9_-]+=[A-Za-z]+(?:\s|$)/.test(parts[0] ?? "");
  const authserv = startsWithResult ? "" : ((parts.shift() ?? "").trim().split(/\s+/)[0] ?? "");
  text = parts.join(";");
  const results: AuthResult[] = [];
  for (const raw of text.split(";")) {
    const comments: string[] = [];
    let part = raw;
    for (let before = ""; before !== part; ) {
      before = part;
      part = part.replace(/\(([^()]*)\)/g, (_, inner: string) => {
        comments.push(inner);
        return " ";
      });
    }
    const words = part.trim().split(/\s+/).filter((w) => w !== "");
    const head = /^([A-Za-z0-9_-]+)=([A-Za-z]+)$/.exec(words[0] ?? "");
    if (head === null) continue;
    const props: Record<string, string> = {};
    for (const word of words.slice(1)) {
      const kv = /^([A-Za-z0-9_.-]+)=(.*)$/.exec(word);
      if (kv !== null) props[kv[1]!.toLowerCase()] = kv[2]!;
    }
    const policyComment = comments.find((c) => /\bp=[A-Za-z]+/.test(c));
    const policy = policyComment
      ?.split(/\s+/)
      .filter((w) => /^(?:p|sp|dis)=[A-Za-z]+$/.test(w))
      .join(" ");
    results.push({ method: head[1]!.toLowerCase(), result: head[2]!.toLowerCase(), props, policy: policy || undefined });
  }
  return { authserv, results };
}

/** The TLS a Received header records, in the forms Gmail, Postfix and Microsoft write. */
export function receivedTls(value: string): { protocol: string | undefined; version: string | undefined; cipher: string | undefined } {
  const protocol = /\bwith\s+(E?SMTPS?A?|LMTPS?A?|HTTPS?)\b/i.exec(value)?.[1]?.toUpperCase();
  const gmail = /version=([A-Za-z0-9_.]+)[ ,]+cipher=([A-Za-z0-9_-]+)/.exec(value);
  const postfix = /using\s+(TLSv[0-9.]+)\s+with\s+cipher\s+([A-Za-z0-9_-]+)/.exec(value);
  const match = gmail ?? postfix;
  const keep = (token: string | undefined) => (token !== undefined && TOKEN.test(token) ? token : undefined);
  return { protocol, version: keep(match?.[1]), cipher: keep(match?.[2]) };
}

/** A DKIM selector as printed: short plain ones as they are (Resend's is `resend`), others hidden. */
const SELECTOR = /^[a-z0-9]{1,32}$/;
export function shownSelector(selector: string | undefined): string | undefined {
  if (selector === undefined) return undefined;
  // A selector can carry a domain (Microsoft 365 writes selector1-<domain with dashes>).
  return SELECTOR.test(selector) ? selector : "<selector>";
}

/** What one Authentication-Results value says, for the verdict. */
interface AuthVerdict {
  /** spf=pass for a MAIL FROM domain related to From (relaxed alignment, approximated). */
  spfAligned: boolean;
  /** A dkim=pass whose d= aligns with From. */
  dkimAligned: boolean;
  /** The dmarc results the receiver wrote; none when it does not evaluate DMARC. */
  dmarc: string[];
}

/** PASS needs aligned SPF and DKIM passes, and dmarc=pass whenever the receiver reports DMARC. */
export function verdictOf(verdict: AuthVerdict | undefined): { pass: boolean; missing: string[] } {
  if (verdict === undefined) return { pass: false, missing: ["an Authentication-Results header"] };
  const missing: string[] = [];
  if (!verdict.spfAligned) missing.push("spf=pass for a MAIL FROM domain aligned with From");
  if (!verdict.dkimAligned) missing.push("dkim=pass with d= aligned with From");
  if (verdict.dmarc.length > 0 && !verdict.dmarc.every((result) => result === "pass")) missing.push("dmarc=pass");
  return { pass: missing.length === 0, missing };
}

/**
 * The redacted summary of a received message's headers. Prints results, the alignment of each
 * DKIM and SPF domain with From, placeholders for domains and the receiver, and the TLS of each
 * Received hop; never an address, a domain, an IP address or an ID. pass is judged on the first
 * (topmost) Authentication-Results, the receiver's own: spf=pass whose smtp.mailfrom domain is
 * related to From (same, subdomain, parent or the same organizational domain), a dkim=pass whose
 * d= aligns with From, and dmarc=pass when the receiver reports a dmarc result (without one, SPF
 * and DKIM decide, and a line says so). The organizational domain is approximated by the last two
 * labels (relation()); the receiver's dmarc result is the authority.
 */
export function headerSummary(raw: string): { lines: string[]; pass: boolean } {
  const headers = parseHeaders(raw);
  const from = fromDomain(headers);
  const names = new Placeholders(from);
  const receivers = new Map<string, string>();
  const receiver = (id: string) => {
    if (id === "") return "(no authserv-id)";
    if (!receivers.has(id)) receivers.set(id, `<receiver-${receivers.size + 1}>`);
    return receivers.get(id)!;
  };
  const lines: string[] = [];
  lines.push(`From domain: ${from === undefined ? "not found" : "<from-domain>"}`);
  const related = (domain: string | undefined) => domain !== undefined && from !== undefined && relation(domain, from) !== "other";
  /** Lists one Authentication-Results value and returns what it says for the verdict. */
  const describe = (label: string, value: string): AuthVerdict => {
    const { authserv, results } = parseAuthResults(value);
    lines.push(`${label} by ${receiver(authserv)}:`);
    const verdict: AuthVerdict = { spfAligned: false, dkimAligned: false, dmarc: [] };
    for (const r of results) {
      const result = TOKEN.test(r.result) ? r.result : "?";
      if (r.method === "spf") {
        const mailFrom = domainOf(r.props["smtp.mailfrom"] ?? "");
        const domain = mailFrom ?? domainOf(r.props["smtp.helo"] ?? "");
        lines.push(`  spf=${result} (${mailFrom !== undefined ? "MAIL FROM" : "HELO"} domain ${names.name(domain)})`);
        lines.push(`  SPF alignment with From: ${mailFrom !== undefined ? names.alignment(mailFrom) : "not aligned (no smtp.mailfrom)"}`);
        verdict.spfAligned ||= result === "pass" && related(mailFrom);
      } else if (r.method === "dkim") {
        const domain = domainOf(r.props["header.d"] ?? r.props["header.i"] ?? "");
        const selector = shownSelector(r.props["header.s"]);
        lines.push(`  dkim=${result} (d=${names.name(domain)}${selector !== undefined ? `, s=${selector}` : ""}: ${names.alignment(domain)})`);
        verdict.dkimAligned ||= result === "pass" && related(domain);
      } else if (r.method === "dmarc") {
        const domain = domainOf(r.props["header.from"] ?? "");
        lines.push(`  dmarc=${result} (header.from ${names.name(domain)}${r.policy !== undefined ? `, ${r.policy}` : ""})`);
        verdict.dmarc.push(result);
      } else if (/^[a-z-]{1,20}$/.test(r.method)) {
        lines.push(`  ${r.method}=${result}`);
      }
    }
    if (verdict.dmarc.length === 0) lines.push("  dmarc: no result from this receiver; judged by SPF and DKIM alone");
    return verdict;
  };
  const results = headers.filter((h) => h.name === "authentication-results");
  if (results.length === 0) lines.push("Authentication-Results: none");
  // The topmost one is the receiver's own; the ones below it came with the message.
  const verdicts = results.map((h, index) => describe(`Authentication-Results ${index + 1} (from the top)`, h.value));
  const { pass, missing } = verdictOf(verdicts[0]);
  for (const h of headers.filter((h) => h.name === "arc-authentication-results")) {
    describe(`ARC-Authentication-Results i=${/^\s*i=(\d+)/.exec(h.value)?.[1] ?? "?"}`, h.value);
  }
  headers.filter((h) => h.name === "received-spf").forEach((h, index) => {
    const result = /^\s*([A-Za-z]+)/.exec(h.value)?.[1]?.toLowerCase() ?? "?";
    lines.push(`Received-SPF ${index + 1}: ${result}`);
  });
  headers.filter((h) => h.name === "dkim-signature").forEach((h, index) => {
    const tags = Object.fromEntries(h.value.split(";").map((t) => t.trim().split("=", 2).map((s) => s.trim())).filter((kv) => kv.length === 2)) as Record<string, string>;
    const domain = domainOf(tags.d ?? "");
    const algorithm = tags.a !== undefined && TOKEN.test(tags.a) ? tags.a : "?";
    const selector = shownSelector(tags.s);
    lines.push(`DKIM-Signature ${index + 1}: d=${names.name(domain)}, a=${algorithm}${selector !== undefined ? `, s=${selector}` : ""}`);
  });
  headers.filter((h) => h.name === "received").forEach((h, index) => {
    const tls = receivedTls(h.value);
    if (tls.protocol === undefined && tls.version === undefined) return;
    lines.push(`Received ${index + 1} (from the top): with ${tls.protocol ?? "?"}${tls.version !== undefined ? `, ${tls.version} ${tls.cipher ?? "?"}` : ""}`);
  });
  const dmarcReported = (verdicts[0]?.dmarc.length ?? 0) > 0;
  lines.push(
    pass
      ? `verdict: PASS (the first Authentication-Results has spf=pass and dkim=pass, both aligned with From${dmarcReported ? ", and dmarc=pass" : "; the receiver reports no dmarc result"})`
      : `verdict: FAIL (the first Authentication-Results lacks: ${missing.join("; ")})`,
  );
  return { lines, pass };
}

// --- Command line ----------------------------------------------------------------------------------

export const RECIPIENT_PROMPT = "Recipient address for the test mail (not echoed, saved or printed): ";

/**
 * The recipient: a prompt on a terminal, otherwise all of stdin (one line). On a terminal the
 * typing is not echoed, so the address stays out of the scrollback and of a recorded session:
 * readline in terminal mode switches the terminal to raw mode and echoes into its output, which
 * here discards everything. Ctrl-C cancels.
 */
export async function readRecipient(): Promise<string> {
  if (!process.stdin.isTTY) return checkedRecipient(await Bun.stdin.text());
  const discard = new Writable({ write: (_chunk, _encoding, done) => done() });
  const lines = createInterface({ input: process.stdin, output: discard, terminal: true });
  // Only now, with the terminal in raw mode: what is typed after the prompt is never echoed, and
  // Ctrl-C reaches readline instead of killing the process.
  process.stderr.write(RECIPIENT_PROMPT);
  try {
    const line = await new Promise<string>((resolve, reject) => {
      lines.once("line", resolve);
      lines.once("SIGINT", () => reject(new ReleaseError("cancelled: no recipient was read")));
      lines.once("close", () => resolve(""));
    });
    return checkedRecipient(line);
  } finally {
    lines.close();
    process.stderr.write("\n");
  }
}

async function send(box: Box, log: Log, raw: Record<string, string>): Promise<void> {
  const options = commonOptions(raw);
  const env: PlanEnv = { checkout: () => readCheckout(ROOT) };
  const command = "bun scripts/demo/test-mail.ts send --ssh-host <host>";
  const plan = await makePlan(box, log, env);
  if (!options.apply) {
    const lock = parseLockState(await box.mustSh(LOCK_STATE_COMMAND, "reading the lab lock"));
    log.note(`the lab lock: ${lock.kind === "held" ? `held by ${lock.owner}` : lock.kind}; --apply takes it for ${ISSUE} (or needs --lock-held when you hold it for ${ISSUE})`);
    checkConfirmation(log, plan, options, command);
    return;
  }
  checkConfirmation(log, plan, options, command);
  requireOnMain(plan);
  const recipient = await readRecipient();
  const release = await takeLock(box, ISSUE, "test-mail", options.lockHeld);
  await whileLocked(log, release, async (interrupts) => {
    const again = await makePlan(box, log, env);
    checkConfirmation(log, again, options, command);
    requireOnMain(again);
    const result = await sendTestMail(box, log, again, recipient, runId(), REAL_CLOCK, interrupts);
    report(log, result, again.smtp.security);
  });
}

/**
 * Runs `body` while the lab lock is held, with the signal handlers on: a signal deletes the pod
 * (sendTestMail registers it) and then releases the lock, and the process exits with 128 + the
 * signal's number. Without a signal the lock is released when `body` ends, however it ends.
 */
export async function whileLocked(log: Log, release: () => Promise<void>, body: (interrupts: Interrupts) => Promise<void>, source: SignalSource = PROCESS_SIGNALS): Promise<void> {
  const interrupts = new Interrupts(log, source);
  const releaseLock = interrupts.guard(release);
  try {
    await body(interrupts);
  } finally {
    await releaseLock();
    await interrupts.finish();
  }
}

async function headers(): Promise<void> {
  const summary = headerSummary(await Bun.stdin.text());
  for (const line of summary.lines) console.log(line);
  if (!summary.pass) process.exit(1);
}

let log: Log | undefined;

if (import.meta.main) {
  await main(
    () => log,
    async () => {
      const [name, ...argv] = Bun.argv.slice(2);
      if (name === "headers") {
        parseOptions(argv, {});
        await headers();
        return;
      }
      if (name !== "send") throw new ReleaseError("usage: bun scripts/demo/test-mail.ts send --ssh-host <host> [--apply --confirm <code>] [--lock-held] | headers < <raw headers>");
      const raw = parseOptions(argv, COMMON_SPEC);
      const options = commonOptions(raw);
      log = new Log("test-mail", options.logDir);
      const current = log;
      const box = new Box(options.sshHost, current);
      await withOpsTunnel(() => opsTunnel(box), current, () => send(box, current, raw));
    },
  );
}
