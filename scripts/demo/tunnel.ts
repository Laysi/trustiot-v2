/**
 * The demo environment's own Cloudflare Tunnel connector on the demo machine
 * (Laysi/trustiot-v2#466, docs/runbooks/demo-environment.md 「安裝 connector」):
 * deploy/host's installer with `--skip-k3s --unit cloudflared-demo`, next to the host's ops tunnel
 * (cloudflared.service), which this script only reads.
 *
 *   bun scripts/demo/tunnel.ts handoff-key --ssh-host <host>
 *   bun scripts/demo/tunnel.ts install --ssh-host <host> --ciphertext <file> --tunnel-id <uuid> [--allow-change "<line>"]... [--apply --confirm <code>]
 *   bun scripts/demo/tunnel.ts status --ssh-host <host>
 *   bun scripts/demo/tunnel.ts route-origin --ssh-host <host>
 *   bun scripts/demo/tunnel.ts check --ssh-host <host> [--stage 2]
 *   bun scripts/demo/tunnel.ts remove --ssh-host <host> [--apply --confirm <code>]
 *
 * The tunnel token never passes through a chat, a terminal or this workstation in clear text
 * (as in docs/test-reports/2026-10-11-64-saas-host.md): handoff-key makes an RSA-4096 key pair
 * on the demo machine, in root-only tmpfs (/run), and prints the public key and the SHA-256 of its
 * SPKI DER bytes; whoever holds the token encrypts it for that key with RSA-OAEP (SHA-256, MGF1
 * SHA-256) and hands over the base64 ciphertext; install decrypts it on the demo machine straight
 * into the installer's --token-stdin, and deletes the private key and the ciphertext after a
 * successful install. A reboot before that deletes the key, and handoff-key makes a new one.
 *
 * install runs the installer in a transient systemd unit on the demo machine
 * (trustiot-demo-install-<run id>), so it does not depend on this workstation or the ssh session,
 * and follows its log. The unit's ExecStopPost puts back, as root, the /usr/local/bin/cloudflared
 * link and the package manager marker that the old cloudflared package's %postun deletes, however
 * the installer ended. Without --apply it runs the installer's own --dry-run with the token, so the
 * plan is the installer's, and reports the cloudflared package change explicitly:
 * /usr/bin/cloudflared is shared with the ops tunnel. The plan names the commit the script runs
 * from (a clean checkout only), the tunnel the token belongs to (it must be --tunnel-id), and only
 * changes on the allow-list (allowedChanges) or named with --allow-change. With --apply it takes
 * the lab lock, plans again and stops when the plan differs from the confirmed one.
 *
 * What the installer changes on the host is in deploy/host/README.md: the firewalld trusted zone
 * (the Service network out, a rule that refuses pods' SSH in), Cloudflare's RPM repository and
 * the signed cloudflared package from it, the unit, its token and its config. install saves the
 * trusted zone first (the zone file if there is one, and firewalld's runtime and permanent view);
 * remove takes out the unit, its token and its config and puts back the two trusted-zone changes,
 * then compares the whole zone with the saved one.
 *
 * check sends the requests the acceptance needs, from this workstation and from the demo machine
 * (the lab network, where the demo gateway is). All three host names are behind Access; only the
 * gateway client's exact paths bypass it. So: a browser without an Access session on the app host
 * name, app paths and other paths without a session on the gateway and ota host names, all
 * stopped by Access; the gateway client's requests (no User-Agent, a WebSocket upgrade) on its
 * bypass paths, which must reach the tunnel; and a path below a bypass path, which the connector
 * must refuse. The host names come from DEMO_APP_HOST, DEMO_GATEWAY_HOST and DEMO_OTA_HOST and are
 * masked in everything it prints or saves.
 *
 * The stage-2 routes go to the ingress Service's fixed ClusterIP (ingress.tunnel.clusterIP,
 * Laysi/trustiot-v2#478). route-origin is the gate before the routes are switched, and check
 * --stage 2 runs the same gate before any probe (Laysi/trustiot-v2#483): three addresses must agree
 * (values-demo.yaml's at this checkout's commit, the deployed ingress-options ConfigMap's service
 * overlay, and the one Service Istio generated for the Gateway) and lie in the static band of the
 * cluster's Service range. Otherwise a route may point at an address no Service holds, which any
 * Service created later could take. route-origin exits 0 only when the gate passes, and its last
 * line names the origin for the routes; status prints the same comparison as a report only. All
 * three only read the cluster (kubectl get), and their reasons never quote ssh, kubectl or git.
 */

import { readdirSync, readFileSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { ipv4ToNumber, serviceCidrsOf, staticBandOf } from "../nightly/ingress-address.ts";
import { parseOptions, tryRun, type RunResult } from "../release/exec.ts";
import { ReleaseError } from "../release/lib.ts";
import {
  Box,
  COMMON_SPEC,
  DEMO_NAMESPACE,
  LOCK_DIR,
  Log,
  OPS_TUNNEL_UNIT,
  ROOT,
  checkConfirmation,
  commonOptions,
  lastLines,
  main,
  opsTunnel,
  ownsLock,
  shownPath,
  shq,
  stableJson,
  takeLock,
  withOpsTunnel,
} from "./lib.ts";

/** The issue that owns this script's lab lock runs. */
export const ISSUE = "Laysi/trustiot-v2#466";
export const DEMO_TUNNEL_UNIT = "cloudflared-demo";
/**
 * The demo connector's metrics listener on loopback (status reads /ready there). cloudflared picks
 * the first free port of 20241 to 20245 by itself; the ops tunnel holds 20241, so a fixed port
 * outside that range keeps the two apart.
 */
export const DEMO_METRICS_PORT = 20250;
/** The cloudflared tunnel the demo connector serves (the name check in the Cloudflare change list). */
export const DEMO_TUNNEL_NAME = "trustiot-demo";
/** Root-only, persistent: the installer, its runner, the run logs and the trusted zone backup. */
export const STATE_DIR = "/var/lib/trustiot-demo";
/** Root-only tmpfs: the hand-off key and the ciphertext live only in memory (and swap). */
export const RUN_DIR = "/run/trustiot-demo";
const HANDOFF_PEM = `${RUN_DIR}/handoff.pem`;
const CIPHERTEXT_FILE = `${RUN_DIR}/token.enc.b64`;
const BOX_INSTALLER = `${STATE_DIR}/trustiot-host-install`;
const BOX_RUNNER = `${STATE_DIR}/run-installer`;
/** The trusted zone as it was before the first install; outside /etc/firewalld, so firewalld never rotates it. */
export const ZONE_BACKUP = `${STATE_DIR}/firewalld`;
const INSTALLER_NAME = "trustiot-host-install-aarch64";
/** deploy/host/src/desired.ts: where the installer keeps the unit's token and config. */
export const CREDENTIAL = `/etc/credstore/${DEMO_TUNNEL_UNIT}.tunnel-token`;
export const CONFIG = `/etc/credstore/${DEMO_TUNNEL_UNIT}.cloudflared.yml`;
const TRUSTED_ZONE = "/etc/firewalld/zones/trusted.xml";
/** deploy/host/src/desired.ts: what the installer changes in the trusted zone. */
export const POD_SSH_RULE = 'rule family="ipv4" source address="10.42.0.0/16" port port="22" protocol="tcp" reject';
export const SERVICE_NETWORK = "10.43.0.0/16";
/** deploy/host/src/desired.ts: the repository the installer writes and the binary it runs. */
const PINNED_REPO_ID = "cloudflared-stable";
const PINNED_REPO_URL = "https://pkg.cloudflare.com/cloudflared/rpm";
const CLOUDFLARED_BINARY = "/usr/bin/cloudflared";
/** Created by the cloudflared package's %post and deleted by its %postun, also on upgrades. */
export const LOCAL_LINK = "/usr/local/bin/cloudflared";
export const PACKAGE_MARKER = "/usr/local/etc/cloudflared/.installedFromPackageManager";
/** What `cloudflared service install` adds unless told not to: a timer that replaces the binary. */
export const UPDATE_UNITS = ["cloudflared-update.timer", "cloudflared-update.service"] as const;
/** Transient units of installer runs: `<prefix><run id>`. */
export const RUN_UNIT_PREFIX = "trustiot-demo-install-";
/** RSA-4096: every OAEP ciphertext is exactly 512 bytes. */
const CIPHERTEXT_BYTES = 512;
/** Every remotely managed tunnel token starts with this (`eyJhIjoi`): base64 of `{"a":"`. */
export const TOKEN_PREFIX = Buffer.from('{"a":"').toString("base64");
/** The paths of this checkout whose state the plan records: the scripts and the installer they run. */
const CODE_PATHS = ["scripts/demo", "deploy/host"];
/**
 * The installer reads every firewalld zone and policy, at 0.5 to 10 seconds per call: 6 to 9
 * minutes for a run that changes nothing on a small host (deploy/host/README.md). This bound
 * only stops the waiting here; the run on the demo machine goes on, and the lab lock stays.
 */
const INSTALLER_WAIT_MS = 60 * 60 * 1000;
const POLL_MS = 10_000;
/** Polls that may find the run's unit gone without its .post file before the run counts as lost. */
const LOST_POLLS = 3;

// --- Input checks ---------------------------------------------------------------------------------

/** The ciphertext file's content as one base64 line, or an error that does not quote it. */
export function checkedCiphertext(text: string): string {
  const compact = text.replace(/\s+/g, "");
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(compact)) throw new ReleaseError("--ciphertext must hold base64 only");
  if (Buffer.from(compact, "base64").length !== CIPHERTEXT_BYTES) {
    throw new ReleaseError(`--ciphertext must decode to ${CIPHERTEXT_BYTES} bytes (RSA-OAEP with the RSA-4096 hand-off key)`);
  }
  return compact;
}

export function checkedTunnelId(value: string): string {
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(value)) {
    throw new ReleaseError("--tunnel-id <uuid> is required: the ID of the demo tunnel, from the Cloudflare change list (lower-case UUID)");
  }
  return value;
}

/** The checksum that deploy/host/dist/SHA256SUMS lists for a file. */
export function listedChecksum(sums: string, name: string): string {
  const line = sums.split("\n").find((l) => l.trim().endsWith(`  ${name}`));
  const sum = line?.split(/\s+/)[0] ?? "";
  if (!/^[0-9a-f]{64}$/.test(sum)) throw new ReleaseError(`deploy/host/dist/SHA256SUMS lists no ${name}: run bun run --cwd deploy/host build`);
  return sum;
}

/** The SHA-256 of a PEM public key's SPKI DER bytes, the form WebCrypto's exportKey("spki") gives. */
export function spkiFingerprint(pem: string): string {
  const der = Buffer.from(
    pem
      .split("\n")
      .filter((line) => line !== "" && !line.startsWith("-----"))
      .join(""),
    "base64",
  );
  return new Bun.CryptoHasher("sha256").update(der).digest("hex");
}

/**
 * Takes the repeatable --allow-change "<line>" out of the arguments (parseOptions keeps only the
 * last value of an option), so each one can go into the plan.
 */
export function splitAllowChanges(args: readonly string[]): { rest: string[]; allowed: string[] } {
  const rest: string[] = [];
  const allowed: string[] = [];
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index] ?? "";
    if (arg !== "--allow-change") {
      rest.push(arg);
      continue;
    }
    const value = args[index + 1];
    if (value === undefined || value === "" || value.startsWith("--")) throw new ReleaseError("--allow-change needs the exact change line, as the plan prints it");
    allowed.push(value);
    index += 1;
  }
  return { rest, allowed };
}

// --- The checkout the plan runs from (M4) ----------------------------------------------------------

export interface CodeState {
  head: string;
}

/**
 * The commit this checkout is at. Anything uncommitted under scripts/demo or deploy/host stops the
 * plan: the confirm code then names the reviewed commit, not the files on this disk.
 */
export async function codeState(root: string): Promise<CodeState> {
  const head = await tryRun(["git", "-C", root, "rev-parse", "HEAD"]);
  if (head.exitCode !== 0 || !/^[0-9a-f]{40}$/.test(head.stdout.trim())) throw new ReleaseError(`could not read the checkout's commit: ${lastLines(head.stderr)}`);
  const status = await tryRun(["git", "-C", root, "status", "--porcelain", "--", ...CODE_PATHS]);
  if (status.exitCode !== 0) throw new ReleaseError(`git status failed: ${lastLines(status.stderr)}`);
  if (status.stdout.trim() !== "") {
    throw new ReleaseError(`${CODE_PATHS.join(" and ")} have uncommitted changes; run from a clean checkout of the reviewed commit:\n${lastLines(status.stdout, 10)}`);
  }
  return { head: head.stdout.trim() };
}

// --- The installer run ----------------------------------------------------------------------------

/** The installer's arguments for the demo connector. */
export function installerArgs(dryRun: boolean): string[] {
  return ["--skip-k3s", "--unit", DEMO_TUNNEL_UNIT, "--metrics-port", String(DEMO_METRICS_PORT), "--token-stdin", "--reinstall-cloudflared", ...(dryRun ? ["--dry-run"] : [])];
}

export interface RunnerPaths {
  /** Persistent root-only directory: the staged installer and runner, the run's copies and logs. */
  state: string;
  /** tmpfs root-only directory: the hand-off key, the staged ciphertext and the run's copy of it. */
  run: string;
  /** Prefix for LOCAL_LINK and PACKAGE_MARKER; "" on the demo machine, a directory in tests. */
  root: string;
}

const BOX_PATHS: RunnerPaths = { state: STATE_DIR, run: RUN_DIR, root: "" };

/**
 * The root-only script on the demo machine that a run's transient unit executes.
 *
 * `run <id> <installer sha256> <ciphertext sha256> <installer arguments>` (ExecStart) copies the
 * staged installer and ciphertext to files of this run, checks the copies against the confirmed
 * plan and uses only those (another invocation may stage different files at any time), decrypts
 * the token straight into the installer's stdin, and writes the installer's output to <id>.log and
 * its exit status to <id>.exit. The token is never an argument, a file or output.
 *
 * The runner's own files are made under umask 077; only the installer runs under 022, sudo's
 * default umask. sudo combines that default with the caller's umask (unless sudoers sets
 * umask_override), so on a SaaS host a stricter caller umask reaches the installer and its dnf
 * too (deploy/host/README.md says to run it from a 0022 shell). Under 077 its dnf left dnf5's
 * state files and the repository's cache root-only (0600/0700) on the demo machine, so no other
 * user could query dnf (Laysi/trustiot-v2#466, 2026-10-21). deploy/host/src/system.ts never
 * relies on the caller's umask: writeFile opens with O_EXCL and the final mode and then chmods,
 * and makeDirectory mkdirs with the mode and then chmods, so 022 widens none of the installer's
 * secrets.
 *
 * `post <id> <restore link 0|1> <restore marker 0|1>` (ExecStopPost, after the run ended in any
 * way: exit, failure, a stop or a kill) puts back the link and the marker the plan said to keep,
 * deletes the run's copies and writes what it found to <id>.post, the run's last file.
 */
export function runnerScript(paths: RunnerPaths = BOX_PATHS): string {
  for (const dir of [paths.state, paths.run]) if (!/^\/[\w./-]+$/.test(dir)) throw new ReleaseError(`invalid directory ${dir}`);
  if (paths.root !== "" && !/^\/[\w./-]+$/.test(paths.root)) throw new ReleaseError(`invalid root ${paths.root}`);
  const link = `${paths.root}${LOCAL_LINK}`;
  const marker = `${paths.root}${PACKAGE_MARKER}`;
  return `#!/bin/bash
# Written by scripts/demo/tunnel.ts (Laysi/trustiot-v2#466); runs as root in a transient unit.
# Usage: run-installer run <run id> <installer sha256> <ciphertext sha256> <installer arguments>
#        run-installer post <run id> <restore link 0|1> <restore marker 0|1>
set -u
umask 077
mode=\${1:-}
run=\${2:-}
case "$run" in
  *[!a-z0-9-]* | "") echo "bad run id" >&2; exit 2 ;;
esac
state=${paths.state}
rundir=${paths.run}
ct="$rundir/$run.ciphertext"
installer="$state/$run.installer"
case "$mode" in
run)
  [ $# -ge 4 ] || { echo "usage: run <id> <installer sha256> <ciphertext sha256> <arguments>" >&2; exit 2; }
  want_installer=$3
  want_ciphertext=$4
  shift 4
  for sum in "$want_installer" "$want_ciphertext"; do
    case "$sum" in
      *[!0-9a-f]*) echo "bad sha256" >&2; exit 2 ;;
    esac
    [ \${#sum} -eq 64 ] || { echo "bad sha256" >&2; exit 2; }
  done
  (
    set -o pipefail
    install -m 0600 "$rundir/token.enc.b64" "$ct" && install -m 0700 "$state/trustiot-host-install" "$installer" || exit 4
    if [ "$(sha256sum < "$installer" | cut -d" " -f1)" != "$want_installer" ]; then
      echo "$state/trustiot-host-install is not the one in the confirmed plan; nothing ran"
      exit 3
    fi
    if [ "$(sha256sum < "$ct" | cut -d" " -f1)" != "$want_ciphertext" ]; then
      echo "$rundir/token.enc.b64 is not the one in the confirmed plan; nothing ran"
      exit 3
    fi
    # 022, sudo's default umask; safe: the installer sets each secret's mode itself (O_EXCL+chmod, mkdir+chmod).
    base64 -d "$ct" \\
      | openssl pkeyutl -decrypt -inkey "$rundir/handoff.pem" -pkeyopt rsa_padding_mode:oaep -pkeyopt rsa_oaep_md:sha256 -pkeyopt rsa_mgf1_md:sha256 \\
      | (umask 022 && exec "$installer" "$@")
  ) < /dev/null > "$state/$run.log" 2>&1
  rc=$?
  rm -f "$ct" "$installer"
  echo "$rc" > "$state/$run.exit.tmp" && mv "$state/$run.exit.tmp" "$state/$run.exit"
  exit "$rc"
  ;;
post)
  [ $# -eq 4 ] || { echo "usage: post <id> <restore link 0|1> <restore marker 0|1>" >&2; exit 2; }
  case "$3$4" in
    00 | 01 | 10 | 11) ;;
    *) echo "bad restore flags" >&2; exit 2 ;;
  esac
  rm -f "$ct" "$installer"
  link=not-asked
  if [ "$3" = 1 ]; then
    if [ -L ${link} ]; then
      if [ "$(readlink ${link})" = ${CLOUDFLARED_BINARY} ]; then link=present; else link=conflict; fi
    elif [ -e ${link} ]; then
      link=conflict
    elif ln -s ${CLOUDFLARED_BINARY} ${link}; then
      link=restored
    else
      link=failed
    fi
  fi
  marker=not-asked
  if [ "$4" = 1 ]; then
    if [ -L ${marker} ] || { [ -e ${marker} ] && [ ! -f ${marker} ]; }; then
      marker=conflict
    elif [ -f ${marker} ]; then
      marker=present
    elif install -d -m 0755 ${marker.replace(/\/[^/]+$/, "")} && install -m 0644 /dev/null ${marker}; then
      marker=restored
    else
      marker=failed
    fi
  fi
  printf 'service_result=%s\\nexit_code=%s\\nexit_status=%s\\nlink=%s\\nmarker=%s\\n' "\${SERVICE_RESULT:-unknown}" "\${EXIT_CODE:-unknown}" "\${EXIT_STATUS:-unknown}" "$link" "$marker" > "$state/$run.post.tmp" \\
    && mv "$state/$run.post.tmp" "$state/$run.post"
  rm -f "$state/$run.runner"
  ;;
*)
  echo "bad mode" >&2
  exit 2
  ;;
esac
`;
}

/**
 * A run id: lower-case letters, digits and dashes only (the runner refuses anything else), unique
 * per run, because the run's own copies of the staged files and its unit are named after it.
 */
export function runId(mode: "dry-run" | "apply", now: Date): string {
  const random = Buffer.from(crypto.getRandomValues(new Uint8Array(4))).toString("hex");
  return `${mode}-${now.toISOString().replace(/[-:]/g, "").replace(/\.\d+Z$/, "z").toLowerCase()}-${random}`;
}

export interface Staged {
  installer: string;
  runner: string;
  ciphertext: string;
}

export interface Restore {
  restoreLink: boolean;
  restoreMarker: boolean;
}

/**
 * Exit status of startCommand when root refused the staged runner: nothing started. Neither sudo
 * nor systemd-run exits with it.
 */
export const START_REFUSED = 97;

/**
 * The ssh command line that starts a run as a transient unit and returns once it started. Root
 * copies the runner to the run's own file, checks the copy against the plan and runs only the copy
 * (with bash, so SELinux sees a shell, not a file under /var/lib, as the unit's program); the
 * runner does the same with the installer and the ciphertext. ExecStopPost runs the same copy.
 */
export function startCommand(id: string, staged: Staged, args: readonly string[], restore: Restore): string {
  if (!/^[a-z0-9-]+$/.test(id)) throw new ReleaseError(`invalid run id ${id}`);
  for (const sum of [staged.installer, staged.runner, staged.ciphertext]) if (!/^[0-9a-f]{64}$/.test(sum)) throw new ReleaseError("invalid sha256 in the plan");
  const copy = `${STATE_DIR}/${id}.runner`;
  const refuse = `{ rm -f ${copy}; echo "${BOX_RUNNER} is not the one in the confirmed plan; nothing ran" >&2; exit ${START_REFUSED}; }`;
  const post = `ExecStopPost=/bin/bash ${copy} post ${id} ${restore.restoreLink ? 1 : 0} ${restore.restoreMarker ? 1 : 0}`;
  const start = [
    `install -m 0700 ${BOX_RUNNER} ${copy} || ${refuse}`,
    `printf '%s  %s\\n' ${staged.runner} ${copy} | sha256sum -c --status - || ${refuse}`,
    `exec systemd-run --unit=${RUN_UNIT_PREFIX}${id} --collect --quiet --setenv=HOME=/root -p ${shq(post)} /bin/bash ${copy} run ${[id, staged.installer, staged.ciphertext, ...args].map(shq).join(" ")}`,
  ].join("; ");
  return `sudo sh -c ${shq(start)} < /dev/null`;
}

/** Installer runs whose transient unit is still there, from `systemctl list-units --all --plain --no-legend`. */
export function runningInstallUnits(listUnits: string): string[] {
  return listUnits
    .split("\n")
    .map((line) => line.trim().split(/\s+/)[0] ?? "")
    .filter((name) => name.startsWith(RUN_UNIT_PREFIX) && name.endsWith(".service"));
}

/** The changes an installer run made or planned: its `+ ` lines, without the dry run's "would". */
export function installerChanges(output: string): string[] {
  return output
    .split("\n")
    .filter((line) => line.startsWith("+ "))
    .map((line) => line.slice(2).replace(/^would /, ""));
}

/** The installer's warnings: its `! ` lines. */
export function installerWarnings(output: string): string[] {
  return output
    .split("\n")
    .filter((line) => line.startsWith("! "))
    .map((line) => line.slice(2));
}

/** The tunnel the installer read from the token: its `  tunnel token: tunnel <id>` line (exactly one). */
export function tunnelIdFromInstallerOutput(output: string): string {
  const ids = [...output.matchAll(/^ {2}tunnel token: (?:kept the one in \S+ \()?tunnel ([0-9a-f-]{36})\)?$/gm)].map((m) => m[1] ?? "");
  if (ids.length !== 1) throw new ReleaseError(`the installer's dry run named ${ids.length} tunnel(s) for the token; expected exactly one "tunnel token: tunnel <id>" line`);
  return ids[0] as string;
}

/** The swap line of the installer: present when --reinstall-cloudflared replaces the package. */
export const SWAP_CHANGE = "replace the installed cloudflared with Cloudflare's signed package from the pinned repository (--reinstall-cloudflared)";

/**
 * The installer's change lines this script runs without a named exception: the host baseline of
 * deploy/host that the user accepted for the demo machine on 2026-10-20 (the trusted zone's two
 * changes, runtime and permanent), Cloudflare's repository and key, the cloudflared swap, and the
 * unit's own files, reload, enable, start and restart. Anything else needs --allow-change.
 */
export function allowedChanges(unit: string = DEMO_TUNNEL_UNIT): string[] {
  const files = [
    "/etc/pki/rpm-gpg/RPM-GPG-KEY-cloudflare-packaging: Cloudflare's package signing key",
    `/etc/yum.repos.d/cloudflared.repo: cloudflared repository (${PINNED_REPO_URL})`,
    `/etc/credstore/${unit}.tunnel-token: tunnel token (root only)`,
    `/etc/credstore/${unit}.cloudflared.yml: cloudflared config (root only)`,
    `/etc/systemd/system/${unit}.service: cloudflared unit (sandboxed, token through LoadCredential)`,
  ];
  return [
    ...["runtime", "permanent"].flatMap((scope) => [
      `firewalld (${scope}): remove the service network ${SERVICE_NETWORK} from the trusted zone (it is never a source)`,
      `firewalld (${scope}): in the trusted zone, ${POD_SSH_RULE}`,
    ]),
    ...files.flatMap((file) => [`write ${file}`, `replace ${file}`]),
    SWAP_CHANGE,
    "reload systemd's unit files",
    `enable ${unit}.service at boot`,
    `start ${unit}.service and wait for its first tunnel connection`,
    `restart ${unit}.service (open requests get up to 30 seconds to finish, then about 4 seconds without a connection)`,
  ];
}

/** Stops the plan at the first change line that is neither on the allow-list nor named with --allow-change. */
export function checkChanges(changes: readonly string[], extra: readonly string[], unit: string = DEMO_TUNNEL_UNIT): void {
  const allowed = new Set([...allowedChanges(unit), ...extra]);
  const other = changes.filter((change) => !allowed.has(change));
  if (other.length > 0) {
    throw new ReleaseError(
      `the installer plans change(s) outside what this script runs: ${other.map((c) => JSON.stringify(c)).join(", ")}. Look at why; to run them anyway, name each with --allow-change "<line>"`,
    );
  }
}

// --- The shared cloudflared binary ------------------------------------------------------------------

export interface LinkFacts {
  type: "absent" | "symlink" | "file" | "other";
  /** readlink's answer for a symbolic link, "" otherwise. */
  target: string;
}

/** /usr/local/bin/cloudflared from `stat -c %F` (or "absent") and `readlink`. */
export function linkFacts(statType: string, readlink: string): LinkFacts {
  const type = statType.trim();
  if (type === "absent") return { type: "absent", target: "" };
  if (type === "symbolic link") return { type: "symlink", target: readlink.trim() };
  if (type === "regular file" || type === "regular empty file") return { type: "file", target: "" };
  return { type: "other", target: "" };
}

/**
 * Where dnf says the installed cloudflared came from: its from_repo (a repository ID, or
 * @commandline for a downloaded RPM), or why it could not be read. An empty answer is a failed
 * read, never a value.
 */
export type FromRepo = { repo: string } | { unreadable: string };

/**
 * Reads from_repo as root. dnf5 keeps it in /usr/lib/sysimage/libdnf5/packages.toml; on the demo
 * machine a dnf transaction under the runner's old umask 077 left that file 0600 root:root (see
 * runnerScript; the modes there are put back by hand), and dnf5 5.4.6 run without root then
 * prints only "Loading system state TOML file ... failed" on stderr, nothing on stdout, and exits 0
 * (2026-10-21). --disablerepo='*': an installed package needs no repository metadata, so the read
 * downloads nothing. stderr is kept for the reason of a failed read.
 */
export const FROM_REPO_COMMAND = "sudo dnf -q repoquery --installed --disablerepo='*' --qf '%{from_repo}\\n' cloudflared";

/** FROM_REPO_COMMAND's answer: one repository ID on the last line of stdout and exit 0, or why not. */
export function fromRepoResult(result: RunResult): FromRepo {
  const repo = result.stdout.trim().split("\n").at(-1)?.trim() ?? "";
  if (result.exitCode === 0 && /^\S+$/.test(repo)) return { repo };
  const said = redactSecrets(mask(lastLines(result.stderr, 1), {})).trim();
  const what = result.exitCode !== 0 ? "failed" : repo === "" ? "printed no repository" : "printed more than a repository ID";
  return { unreadable: `dnf ${what} (exit ${result.exitCode})${said === "" ? "" : `: ${said}`}` };
}

/** A from_repo as the plan prints it: the repository, or that it could not be read and why. */
export function fromRepoText(fromRepo: FromRepo): string {
  return "repo" in fromRepo ? fromRepo.repo : `an unknown source (dnf's from_repo could not be read: ${fromRepo.unreadable})`;
}

/**
 * status's and install's line on the installed package: OK only when dnf says it came from the
 * pinned repository; a source that could not be read or is another one is a FAIL, like the other
 * FAIL lines of status (printed, and the command still exits 0: status reports, it does not stop).
 */
export function sourceLine(installed: string, fromRepo: FromRepo): string {
  if (installed === "") return `FAIL cloudflared is not installed`;
  if (!("repo" in fromRepo)) return `FAIL cloudflared ${installed}: could not read where it came from: ${fromRepo.unreadable}`;
  if (fromRepo.repo !== PINNED_REPO_ID) return `FAIL cloudflared ${installed} from ${fromRepo.repo}, not Cloudflare's repository ${PINNED_REPO_ID}`;
  return `OK cloudflared ${installed} from ${fromRepo.repo}`;
}

export interface CloudflaredFacts {
  /** `rpm -q` version-release, or "" when the package is not installed. */
  installed: string;
  /** Where dnf says the installed package came from (from_repo), or why that could not be read. */
  fromRepo: FromRepo;
  /** The newest version-release in Cloudflare's repository for this architecture; read only for a swap. */
  candidate: string;
  link: LinkFacts;
  /** The package manager marker: a regular file, absent, or something else. */
  marker: "file" | "absent" | "other";
}

export interface CloudflaredPlan {
  swap: boolean;
  installed: string;
  fromRepo: FromRepo;
  candidate: string;
  link: LinkFacts;
  marker: CloudflaredFacts["marker"];
  restoreLink: boolean;
  restoreMarker: boolean;
}

/**
 * What the installer's --reinstall-cloudflared does to the shared /usr/bin/cloudflared. Whether it
 * swaps the package is the installer's own decision (its dry run's SWAP_CHANGE line); dnf then
 * reinstalls the same version from the pinned repository, or takes the repository's version when
 * it has no such version (dnf distro-sync). The old package's %postun deletes the /usr/local/bin
 * link and the marker, which the run's ExecStopPost puts back as they were. A link that is not the
 * package's own (a file, or a link elsewhere) stops the plan: putting it back would be a guess.
 */
export function cloudflaredPlan(facts: CloudflaredFacts, swap: boolean): CloudflaredPlan {
  if (facts.installed === "") throw new ReleaseError("cloudflared is not installed on the demo machine; the ops tunnel needs it, so stop and check the host");
  if (facts.link.type === "file" || facts.link.type === "other") {
    throw new ReleaseError(`${LOCAL_LINK} is a ${facts.link.type === "file" ? "regular file" : "file of an unexpected type"}, not the package's link to ${CLOUDFLARED_BINARY}; look at where it came from before installing`);
  }
  if (facts.link.type === "symlink" && facts.link.target !== CLOUDFLARED_BINARY) {
    throw new ReleaseError(`${LOCAL_LINK} points to ${facts.link.target}, not ${CLOUDFLARED_BINARY}; look at where it came from before installing`);
  }
  if (facts.marker === "other") throw new ReleaseError(`${PACKAGE_MARKER} is not a regular file; look at it before installing`);
  if (swap && !/^\d[\w.+~-]*$/.test(facts.candidate)) {
    throw new ReleaseError(`could not read the cloudflared version in ${PINNED_REPO_URL}; the plan must name it, so stop here`);
  }
  return {
    swap,
    installed: facts.installed,
    fromRepo: facts.fromRepo,
    candidate: swap ? facts.candidate : facts.installed,
    link: facts.link,
    marker: facts.marker,
    restoreLink: swap && facts.link.type === "symlink",
    restoreMarker: swap && facts.marker === "file",
  };
}

/** The lines that tell the person what happens to the shared cloudflared binary. */
export function describeCloudflared(plan: CloudflaredPlan, ops: { pid: string }): string[] {
  if (!plan.swap) {
    return [`cloudflared: ${plan.installed} from ${fromRepoText(plan.fromRepo)} stays as it is (the installer does not replace it)`];
  }
  const lines = [
    `cloudflared: the installer replaces ${CLOUDFLARED_BINARY} ${plan.installed} (from ${fromRepoText(plan.fromRepo)}) with ${plan.candidate} from Cloudflare's signed repository (${PINNED_REPO_ID})`,
    plan.candidate === plan.installed ? "cloudflared: same version, now Cloudflare's signed package" : `cloudflared: this is a version change, ${plan.installed} -> ${plan.candidate}`,
    `cloudflared: the ops tunnel ${OPS_TUNNEL_UNIT} runs the same ${CLOUDFLARED_BINARY}; its running process (PID ${ops.pid}) keeps the binary it started with and runs ${plan.candidate} from its next restart (reboot); this script never restarts it`,
  ];
  if (plan.restoreLink || plan.restoreMarker) {
    lines.push(
      `cloudflared: the old package's %postun deletes ${[plan.restoreLink ? LOCAL_LINK : "", plan.restoreMarker ? PACKAGE_MARKER : ""].filter(Boolean).join(" and ")}; the run's ExecStopPost puts them back as root, however the installer ends`,
    );
  }
  return lines;
}

export interface UnitState {
  name: string;
  load: string;
  unitFile: string;
  active: string;
}

/** `systemctl show <units> -p Id -p LoadState -p UnitFileState -p ActiveState`: one block per unit. */
export function unitStates(show: string): UnitState[] {
  return show
    .trim()
    .split(/\n\s*\n/)
    .filter((block) => block.trim() !== "")
    .map((block) => {
      const field = (name: string) => new RegExp(`^${name}=(.*)$`, "m").exec(block)?.[1]?.trim() ?? "";
      return { name: field("Id"), load: field("LoadState"), unitFile: field("UnitFileState"), active: field("ActiveState") };
    });
}

/**
 * cloudflared's own updater (`cloudflared update`, run by cloudflared-update.timer) replaces the
 * binary unless the package manager marker is there; a dnf upgrade deletes the marker for a moment
 * (or for good, until it is put back). Either unit stops the plan.
 */
export function requireNoUpdateTimer(states: readonly UnitState[]): void {
  const present = states.filter((s) => s.load !== "not-found" && s.load !== "");
  if (present.length > 0 || states.length !== UPDATE_UNITS.length) {
    throw new ReleaseError(
      `${present.map((s) => `${s.name} (${s.load}, ${s.unitFile || "no unit file state"}, ${s.active})`).join(" and ") || "the cloudflared update units could not be read"}: cloudflared's self-update can replace the binary both tunnels run. Disable and remove it (it comes from \`cloudflared service install\` without --no-update-service) before installing`,
    );
  }
}

/** status: the link, the marker and the update timer, with OK, WARN or FAIL. */
export function linkReport(link: LinkFacts, marker: CloudflaredFacts["marker"], timers: readonly UnitState[]): string[] {
  const timerOn = timers.some((s) => s.load === "loaded" && (s.unitFile === "enabled" || s.active === "active"));
  const lines = [
    link.type === "symlink" && link.target === CLOUDFLARED_BINARY
      ? `OK ${LOCAL_LINK} -> ${CLOUDFLARED_BINARY}`
      : `WARN ${LOCAL_LINK} is ${link.type === "symlink" ? `a link to ${link.target}` : link.type}: after a dnf upgrade of cloudflared, put it back (docs/runbooks/demo-environment.md)`,
    marker === "file" ? `OK ${PACKAGE_MARKER} present` : `WARN ${PACKAGE_MARKER} is ${marker}: cloudflared's own updater may replace the binary; put it back (docs/runbooks/demo-environment.md)`,
  ];
  for (const s of timers) {
    if (s.load === "not-found") lines.push(`OK ${s.name} not installed`);
    else if (timerOn && marker !== "file") lines.push(`FAIL ${s.name} is ${s.unitFile}/${s.active} and the marker is missing: the next run replaces ${CLOUDFLARED_BINARY}; disable it now`);
    else lines.push(`WARN ${s.name} is ${s.load}/${s.unitFile}/${s.active}: remove it (cloudflared service install without --no-update-service added it)`);
  }
  return lines;
}

/** Who listens on the demo metrics port, from `ss -Htlnp 'sport = :<port>'`: "" when free or ours. */
export function metricsPortHolder(ss: string, demoPid: string): string {
  const others = ss
    .split("\n")
    .filter((line) => line.trim() !== "")
    .filter((line) => demoPid === "0" || demoPid === "" || !line.includes(`pid=${demoPid},`));
  return others.join("; ");
}

/** readyConnections from cloudflared's /ready, or undefined. */
export function readyConnections(body: string): number | undefined {
  try {
    const value = (JSON.parse(body) as { readyConnections?: unknown }).readyConnections;
    return typeof value === "number" ? value : undefined;
  } catch {
    return undefined;
  }
}

// --- The lab lock -----------------------------------------------------------------------------------

/** The lab lock as the dry run sees it: free, held with an owner line, or there without a readable owner. */
export type LockState = { kind: "free" } | { kind: "held"; owner: string } | { kind: "unreadable" };

/**
 * Reads the lock directory and its owner in one command. A directory without a readable, non-empty
 * owner is "unreadable": takeLock writes the owner right after mkdir, so that is a run taking the
 * lock right now, or one that died in between; both keep the dry run out.
 */
export const LOCK_STATE_COMMAND = [
  `if [ ! -e ~/${LOCK_DIR} ]; then echo LOCK_FREE`,
  `elif owner=$(cat ~/${LOCK_DIR}/owner 2>/dev/null) && [ -n "$owner" ]; then printf 'LOCK_HELD %s\\n' "$owner"`,
  "else echo LOCK_UNREADABLE; fi",
].join("; ");

/** Parses LOCK_STATE_COMMAND's output; anything else is "unreadable", so the dry run fails closed. */
export function parseLockState(out: string): LockState {
  const line = out.trim();
  if (line === "LOCK_FREE") return { kind: "free" };
  const held = /^LOCK_HELD ([^\n]+)$/.exec(line);
  if (held?.[1] !== undefined && held[1].trim() !== "") return { kind: "held", owner: held[1].trim() };
  return { kind: "unreadable" };
}

/**
 * A dry run changes nothing on the host but runs the installer, which calls firewall-cmd as root
 * many times; it runs while the lock is free or held for this issue, never under someone else's
 * run or a lock whose owner cannot be read, and does not take the lock itself.
 */
export function dryRunLockCheck(state: LockState): void {
  if (state.kind === "free") return;
  if (state.kind === "unreadable") {
    throw new ReleaseError(
      `the lab lock ~/${LOCK_DIR} is there but its owner cannot be read; the dry run waits (another run may be taking the lock right now, or one stopped while taking it: look at ~/${LOCK_DIR} on the demo machine)`,
    );
  }
  if (!ownsLock(state.owner, ISSUE)) {
    throw new ReleaseError(`the lab lock is held by: ${state.owner}; the dry run waits for it (it runs the installer as root, which reads firewalld many times)`);
  }
}

/**
 * Reads the lab lock and applies dryRunLockCheck. It takes no options on purpose: --lock-held
 * cannot skip it, since this issue's own lock passes anyway.
 */
export async function checkLockForDryRun(box: Box): Promise<void> {
  dryRunLockCheck(parseLockState(await box.mustSh(LOCK_STATE_COMMAND, "reading the lab lock")));
}

// --- Probes (check) ----------------------------------------------------------------------------

/**
 * What answered a probe. `refused` is the connector's own Access JWT check (originRequest.access)
 * turning away a request that came through a bypass without a JWT: a 403 that is neither Access
 * nor a Cloudflare block page, and in stage 2 also has no body and no content type, so the demo
 * ingress's own 403s count as `origin` (see classify).
 */
export type Outcome = "origin" | "access" | "challenge" | "block" | "tunnel-down" | "refused" | "unexpected";

/** What a probe must get: stopped by Access, through to the tunnel, or turned away by the connector. */
export type Expectation = "access" | "reached" | "refused";

export interface ProbeResponse {
  status: number;
  headers: Record<string, string>;
  body: string;
}

/** Splits curl -i output into the final status line, the headers (lower-case names) and the body. */
export function parseCurlOutput(output: string): ProbeResponse {
  let rest = output.replace(/\r\n/g, "\n");
  for (;;) {
    const end = rest.indexOf("\n\n");
    const head = end < 0 ? rest : rest.slice(0, end);
    const body = end < 0 ? "" : rest.slice(end + 2);
    const [statusLine = "", ...lines] = head.split("\n");
    const status = Number(/^HTTP\/[\d.]+ (\d{3})/.exec(statusLine)?.[1] ?? 0);
    if (status === 100 && end >= 0) {
      rest = body;
      continue;
    }
    const headers: Record<string, string> = {};
    for (const line of lines) {
      const colon = line.indexOf(":");
      if (colon > 0) headers[line.slice(0, colon).trim().toLowerCase()] = line.slice(colon + 1).trim();
    }
    return { status, headers, body };
  }
}

/** The tunnel's stage-1 route answer (http_status:418) for every routed path. */
export const STAGE1_STATUS = 418;

/**
 * What answered a probe. Cloudflare marks challenge pages with `cf-mitigated: challenge`
 * (https://developers.cloudflare.com/cloudflare-challenges/challenge-types/challenge-pages/detect-response/);
 * its block pages are 403s with an error code such as 1010 (browser signature) or 1020 (a rule);
 * Access sends a request without a session, browser or not, to <team>.cloudflareaccess.com with a
 * 302 (https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/managed-oauth/:
 * non-browser clients "receive a 302 redirect" unless managed OAuth is on, which this setup does
 * not use); 530 with error 1033 means no connector is up. Stage 1's routes answer 418, and 101 is a
 * WebSocket the origin accepted.
 *
 * Any other 403 is told apart by stage. In stage 1 nothing but the connector sits behind the tunnel
 * (every route is an http_status service), so it is the connector's refusal. In stage 2 the demo
 * ingress answers too, and its 403s carry a body or a content type (Envoy's "RBAC: access denied",
 * a service's JSON); the connector's refusal writes only the status, so only a 403 without either
 * is `refused`, and every other 403 is `origin`. Stage 1 keeps the refusal's content type and body
 * size (summary), which shows the stage-2 rule holds for the cloudflared that runs. Mistaking one
 * for the other only fails a probe, never passes one that should fail: an app-only path must
 * be `access`, which neither is.
 *
 * The WebSocket upgrade to /mqtt fails in stage 1 (`unexpected`). When the origin answers an upgrade
 * with anything but 101, the client gets a Cloudflare 502 page (`server: cloudflare`, 148 bytes of
 * HTML) while Cloudflare's HTTP request analytics and the connector's counters record the origin's
 * 418 (observed 2026-10-21; Cloudflare's documentation does not describe it). Whether the request
 * reached the connector is read by hand from cloudflared_tunnel_total_requests and
 * cloudflared_tunnel_response_by_code{status_code="418"} on 127.0.0.1:20250/metrics before and
 * after the probe (docs/runbooks/demo-environment.md); Laysi/trustiot-v2#473 makes check do that.
 * Until then stage 2 accepts that 502 without the counters, like any answer that is not Cloudflare
 * or the connector stopping the request (probePasses).
 */
export function classify(response: ProbeResponse, stage: 1 | 2): Outcome {
  const { status, headers, body } = response;
  if (headers["cf-mitigated"]?.toLowerCase() === "challenge" || /challenge-platform|Just a moment\.\.\./.test(body)) return "challenge";
  const location = headers.location ?? "";
  if (status >= 300 && status < 400 && /^https:\/\/[a-z0-9-]+\.cloudflareaccess\.com\//i.test(location)) return "access";
  if (status === 403 && /error code:?\s*10\d\d|Error 10\d\d|Sorry, you have been blocked|Attention Required/i.test(body)) return "block";
  if (status === 530 || /error code:?\s*1033/i.test(body)) return "tunnel-down";
  if (status === 403) return stage === 1 || (body.trim() === "" && headers["content-type"] === undefined) ? "refused" : "origin";
  if (status === 101 || status === STAGE1_STATUS) return "origin";
  return "unexpected";
}

/**
 * Whether an outcome is what the probe needs. A bypass path must reach the connector: in stage 1
 * that is the route's 418 (or 101); in stage 2 the demo ingress answers, so anything that is not
 * Cloudflare or the connector stopping the request counts.
 */
export function probePasses(expect: Expectation, outcome: Outcome, stage: 1 | 2): boolean {
  if (expect !== "reached") return outcome === expect;
  return stage === 1 ? outcome === "origin" : !["access", "challenge", "block", "tunnel-down", "refused"].includes(outcome);
}

/**
 * Replaces the private host names, the Access team domain and home directories (curl names files
 * in its errors), so a probe can be saved and shown.
 */
export function mask(text: string, hosts: Record<string, string>, home: string = homedir()): string {
  let out = text;
  for (const [name, host] of Object.entries(hosts)) {
    if (host !== "") out = out.split(host).join(`<${name}>`).split(encodeURIComponent(host)).join(`<${name}>`);
  }
  if (home.length > 1) out = out.split(home).join("~");
  return out.replace(/[a-z0-9-]+\.cloudflareaccess\.com/gi, "<team>.cloudflareaccess.com").replace(/\/(?:home|Users)\/[^/\s'"]+|\/root(?=[/\s'"]|$)/g, "~");
}

/**
 * What a probe keeps: the status, a few headers, cf-ray and the body's size, no body, no query
 * strings, no host names.
 */
export function summary(response: ProbeResponse, hosts: Record<string, string>): Record<string, string | number> {
  const location = response.headers.location?.replace(/\?.*$/, "") ?? "";
  const keep: Record<string, string | number> = { status: response.status, "body-bytes": new TextEncoder().encode(response.body).length };
  for (const name of ["server", "cf-mitigated", "content-type", "upgrade", "sec-websocket-protocol", "cf-ray"]) {
    const value = response.headers[name];
    if (value !== undefined) keep[name] = mask(value, hosts);
  }
  if (location !== "") keep.location = mask(location, hosts);
  const markers = [/cf-mitigated|challenge-platform|Just a moment/i, /error code:?\s*\d{4}|Error \d{4}/i, /Sorry, you have been blocked/i]
    .map((pattern) => pattern.exec(response.body)?.[0])
    .filter((m): m is string => m !== undefined);
  if (markers.length > 0) keep.body = markers.join(", ");
  return keep;
}

export interface Probe {
  name: string;
  host: "app" | "gateway" | "ota";
  path: string;
  expect: Expectation;
  args: string[];
}

/** The gateway's client (tokio-tungstenite): no User-Agent, no Accept, a WebSocket upgrade for MQTT. */
function websocketArgs(): string[] {
  const key = Buffer.from(crypto.getRandomValues(new Uint8Array(16))).toString("base64");
  return ["-H", "User-Agent:", "-H", "Accept:", "-H", "Connection: Upgrade", "-H", "Upgrade: websocket", "-H", "Sec-WebSocket-Version: 13", "-H", `Sec-WebSocket-Key: ${key}`, "-H", "Sec-WebSocket-Protocol: mqtt"];
}

/**
 * The paths that bypass Access: the chart's cookie-free paths on the gateway host name
 * (deploy/chart/templates/_ingress.tpl, the public path table; deploy/chart/values.yaml
 * ingress.publicPaths), each one exact. The ota host name has none in the chart, so all of it stays
 * behind Access until the chart gives the OTA client a cookie-free path.
 */
export const BYPASS_PATHS: Record<"gateway" | "ota", string[]> = { gateway: ["/certs/renew", "/mqtt"], ota: [] };

/** The chart's cookie-free paths on the app host name: app paths that must never pass Access elsewhere. */
export const APP_ONLY_PATHS = ["/auth/login", "/healthz"];

/**
 * The probes, the same in both stages; only what counts as reaching the tunnel differs
 * (probePasses). Every host name is behind Access; only the exact bypass paths get through without
 * a session, and a path below a bypass path gets through Access (whose path apps cover their
 * sub-paths) but must be turned away by the connector, whose bypass rules are anchored.
 */
export function probes(): Probe[] {
  const browser = ["-H", "User-Agent: Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36", "-H", "Accept: text/html,application/xhtml+xml"];
  const bare = ["-H", "User-Agent:", "-H", "Accept:"];
  return [
    { name: "app, browser without an Access session", host: "app", path: "/", expect: "access", args: browser },
    { name: "gateway, WebSocket upgrade to /mqtt (gateway client, bypass)", host: "gateway", path: "/mqtt", expect: "reached", args: websocketArgs() },
    { name: "gateway, POST /certs/renew without User-Agent (gateway client, bypass)", host: "gateway", path: "/certs/renew", expect: "reached", args: [...bare, "-X", "POST", "--data-binary", ""] },
    ...APP_ONLY_PATHS.flatMap((path) =>
      (["gateway", "ota"] as const).map((host): Probe => ({ name: `${host}, app path ${path} without User-Agent (behind Access)`, host, path, expect: "access", args: bare })),
    ),
    { name: "gateway, / without User-Agent (behind Access)", host: "gateway", path: "/", expect: "access", args: bare },
    { name: "ota, / without User-Agent (behind Access)", host: "ota", path: "/", expect: "access", args: bare },
    { name: "gateway, /mqtt/x below a bypass path (the connector refuses it)", host: "gateway", path: "/mqtt/x", expect: "refused", args: bare },
  ];
}

/** curl's arguments for one probe: HTTP/1.1 (WebSocket upgrades are HTTP/1.1), headers in the output. */
export function curlArgs(probe: Probe, host: string): string[] {
  return ["curl", "-sS", "-i", "--http1.1", "--max-time", "15", ...probe.args, `https://${host}${probe.path}`];
}

export interface ProbeIo {
  workstation: (args: string[]) => Promise<RunResult>;
  demoMachine: (args: string[]) => Promise<RunResult>;
  note: (line: string) => void;
}

/** Sends every probe from both places; returns the masked records and the number that failed. */
export async function runProbes(io: ProbeIo, hosts: Record<"app" | "gateway" | "ota", string>, stage: 1 | 2): Promise<{ results: Record<string, unknown>[]; failed: number }> {
  const results: Record<string, unknown>[] = [];
  let failed = 0;
  for (const where of ["workstation", "demo machine"] as const) {
    for (const probe of probes()) {
      const args = curlArgs(probe, hosts[probe.host]);
      const run = where === "workstation" ? await io.workstation(args) : await io.demoMachine(args);
      // A WebSocket the origin accepted stays open until --max-time (curl exit 28); its headers count.
      const response = parseCurlOutput(run.stdout);
      const outcome: Outcome = response.status === 0 ? "unexpected" : classify(response, stage);
      const pass = response.status !== 0 && probePasses(probe.expect, outcome, stage);
      if (!pass) failed += 1;
      const kept = summary(response, hosts);
      results.push({ from: where, probe: probe.name, expect: probe.expect, outcome, pass, ...kept, ...(response.status === 0 ? { curl: mask(lastLines(run.stderr, 2), hosts) } : {}) });
      io.note(`${pass ? "PASS" : "FAIL"} from the ${where}: ${probe.name}: ${outcome} (${JSON.stringify(kept)})${response.status === 0 ? ` curl: ${mask(lastLines(run.stderr, 1), hosts)}` : ""}`);
    }
  }
  return { results, failed };
}


function hostFromEnv(name: string): string {
  const value = process.env[name] ?? "";
  if (!/^(?=.{1,253}$)([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$/.test(value)) {
    throw new ReleaseError(`set ${name} to the host name (the values are in the private Cloudflare change list, never in the repo)`);
  }
  return value;
}


// --- The ingress address (route-origin, check --stage 2, status) -----------------------------------

/**
 * The demo overlay that pins the ingress Service's ClusterIP (ingress.tunnel.clusterIP,
 * Laysi/trustiot-v2#478 and #480), relative to the repository root.
 */
export const DEMO_VALUES = "deploy/flux/clusters/demo/trustiot/values-demo.yaml";
/**
 * The chart's names for the demo release (DEMO_NAMESPACE is also the release name, see
 * deploy/flux/clusters/demo/trustiot/helm-release.yaml). trustiot.fullname is the release name when
 * it contains the chart's name (deploy/chart/templates/_helpers.tpl); the Gateway is
 * `<fullname>-ingress` (trustiot.ingress.gatewayName) and its options ConfigMap
 * `<fullname>-ingress-options` (deploy/chart/templates/ingress.yaml).
 */
export const INGRESS_GATEWAY = `${DEMO_NAMESPACE}-ingress`;
export const INGRESS_OPTIONS = `${DEMO_NAMESPACE}-ingress-options`;
/** The Gateway's gatewayClassName in deploy/chart/templates/ingress.yaml. */
export const GATEWAY_CLASS = "istio";
/**
 * Istio names what it generates for a Gateway `<gateway>-<gatewayClassName>`
 * (deploy/chart/templates/_ingress.tpl; scripts/nightly/api.ts port-forwards to `<gateway>-istio`).
 */
export const INGRESS_SERVICE = `${INGRESS_GATEWAY}-${GATEWAY_CLASS}`;
/**
 * The label Istio puts on the Service it generates for a Gateway: the selector the nightly smoke
 * uses for the same check (scripts/nightly/smoke.ts, scripts/nightly/ingress-address.ts).
 */
export const GATEWAY_NAME_LABEL = "gateway.networking.k8s.io/gateway-name";
/** The tunnel listener's port (trustiot.ingress.listenerPort in deploy/chart/templates/_ingress.tpl). */
export const INGRESS_PORT = 80;
/** The ServiceCIDR object of the cluster's default Service range (read for its static band). */
export const SERVICE_CIDR = "kubernetes";
/** The chart's IPv4 form for the address (trustiot.ingress.ipv4Pattern): a dotted quad without leading zeros. */
const IPV4 = /^(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])(\.(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])){3}$/;
const FIXED_ADDRESS_DOC = "deploy/chart/README.md 「固定的入口 ClusterIP」";

/**
 * One of the three addresses, or why there is none. A reason never quotes ssh, kubectl, git or a
 * parser: their messages can name the demo machine, its network or a home directory, and the
 * comparison's output goes into test reports. It names the command and its exit status instead;
 * values the script validated (an IPv4 address, a commit hash, a CIDR) and object names in the
 * demo namespace may appear.
 */
export type AddressRead = { address: string } | { missing: string };

/** The static band of the cluster's IPv4 Service range, or why it could not be read. */
export type BandRead = { cidr: string; first: string; last: string } | { missing: string };

export interface IngressAddresses {
  /** The checkout's commit whose values-demo.yaml was read; "" when it could not be read. */
  commit: string;
  /** values-demo.yaml's ingress.tunnel.clusterIP at that commit. */
  values: AddressRead;
  /** The clusterIP of the `service` overlay in the chart's ingress-options ConfigMap in the cluster. */
  overlay: AddressRead;
  /** spec.clusterIP of the one Service Istio generated for the ingress Gateway. */
  service: AddressRead;
  /** The static band the address must be in. */
  band: BandRead;
}

/** Reads values-demo.yaml at the checkout's commit; valuesAddress(ROOT), or a test's own. */
export type ValuesReader = () => Promise<{ commit: string; values: AddressRead }>;

function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
}

/** An address as the chart accepts it; anything else (a number, a CIDR, "None") is a reason that does not quote it. */
function addressOf(raw: unknown, what: string, unset: string): AddressRead {
  if (raw === undefined || raw === null || raw === "") return { missing: unset };
  if (typeof raw !== "string") return { missing: `${what} is not a string` };
  if (!IPV4.test(raw)) return { missing: `${what} is not one IPv4 address (a dotted quad without leading zeros)` };
  return { address: raw };
}

/** ingress.tunnel.clusterIP in the text of values-demo.yaml. */
export function valuesClusterIp(text: string): AddressRead {
  let values: unknown;
  try {
    values = Bun.YAML.parse(text);
  } catch {
    return { missing: `${DEMO_VALUES} is not valid YAML` };
  }
  const raw = asRecord(asRecord(asRecord(values).ingress).tunnel).clusterIP;
  return addressOf(raw, `ingress.tunnel.clusterIP in ${DEMO_VALUES}`, `${DEMO_VALUES} does not set ingress.tunnel.clusterIP (Laysi/trustiot-v2#480 sets it)`);
}

/** A git command for valuesAddress: its result, or a fixed reason (never git's own words). */
async function gitRead(root: string, args: string[], shown: string): Promise<RunResult | { failed: string }> {
  let result: RunResult;
  try {
    result = await tryRun(["git", "-C", root, ...args]);
  } catch {
    return { failed: `${shown} could not run` };
  }
  return result.exitCode === 0 ? result : { failed: `${shown} failed (exit ${result.exitCode})` };
}

/**
 * values-demo.yaml as the checkout's commit has it: an uncommitted change to the file is no
 * address, so what is compared is the reviewed commit, not the file on this disk. Never throws.
 */
export async function valuesAddress(root: string): Promise<{ commit: string; values: AddressRead }> {
  const head = await gitRead(root, ["rev-parse", "HEAD"], "git rev-parse HEAD");
  if ("failed" in head) return { commit: "", values: { missing: `could not read the checkout's commit: ${head.failed}` } };
  const commit = head.stdout.trim();
  if (!/^[0-9a-f]{40}$/.test(commit)) return { commit: "", values: { missing: "could not read the checkout's commit: git rev-parse HEAD printed no commit hash" } };
  const status = await gitRead(root, ["status", "--porcelain", "--", DEMO_VALUES], `git status --porcelain -- ${DEMO_VALUES}`);
  if ("failed" in status) return { commit, values: { missing: status.failed } };
  if (status.stdout.trim() !== "") return { commit, values: { missing: `${DEMO_VALUES} has uncommitted changes; run from a clean checkout of the reviewed commit` } };
  const shown = await gitRead(root, ["show", `HEAD:${DEMO_VALUES}`], `git show HEAD:${DEMO_VALUES}`);
  if ("failed" in shown) return { commit, values: { missing: `${shown.failed}: the file is not in commit ${commit}, or git could not read it` } };
  return { commit, values: valuesClusterIp(shown.stdout) };
}

/** The clusterIP of the `service` overlay, from `kubectl get configmap <release>-ingress-options -o json` (undefined: not found). */
export function overlayClusterIp(configMap: Record<string, unknown> | undefined): AddressRead {
  if (configMap === undefined) return { missing: `ConfigMap ${INGRESS_OPTIONS} is not in namespace ${DEMO_NAMESPACE}: the demo release is not deployed` };
  const service = asRecord(configMap.data).service;
  if (typeof service !== "string") return { missing: `ConfigMap ${INGRESS_OPTIONS} has no service overlay` };
  let overlay: unknown;
  try {
    overlay = Bun.YAML.parse(service);
  } catch {
    return { missing: `the service overlay of ConfigMap ${INGRESS_OPTIONS} is not valid YAML` };
  }
  return addressOf(
    asRecord(asRecord(overlay).spec).clusterIP,
    `the clusterIP of ConfigMap ${INGRESS_OPTIONS}'s service overlay`,
    `ConfigMap ${INGRESS_OPTIONS}'s service overlay has no clusterIP: the deployed release does not set ingress.tunnel.clusterIP (Flux has not applied the values yet, or the chart is older than Laysi/trustiot-v2#478)`,
  );
}

/**
 * The ClusterIP of the Service Istio generated for the ingress Gateway, from `kubectl get services
 * --selector <GATEWAY_NAME_LABEL>=<gateway> -o json`. There must be exactly one, as in the nightly
 * smoke (generatedClusterIpProblems), and it must be the one Istio made for this Gateway: Istio's
 * name for it, owned by the Gateway, of type ClusterIP (the chart's tunnel overlay) with the
 * listener's port, and its clusterIP an IPv4 address that is also the first of its clusterIPs.
 */
export function serviceClusterIp(list: Record<string, unknown> | undefined): AddressRead {
  const items = Array.isArray(list?.items) ? list.items.map(asRecord) : [];
  const selector = `${GATEWAY_NAME_LABEL}=${INGRESS_GATEWAY}`;
  if (items.length !== 1) {
    const names = items.map((item) => String(asRecord(item.metadata).name));
    return {
      missing:
        items.length === 0
          ? `no Service in namespace ${DEMO_NAMESPACE} has the label ${selector}: Istio has not generated the ingress Service (the release is not deployed, or its Gateway is not programmed)`
          : `${items.length} Services in namespace ${DEMO_NAMESPACE} have the label ${selector} (${names.join(", ")}), expected exactly one: find out where the others came from before routing to any of them`,
    };
  }
  const service = items[0] ?? {};
  const metadata = asRecord(service.metadata);
  const name = String(metadata.name);
  const spec = asRecord(service.spec);
  const problems: string[] = [];
  if (name !== INGRESS_SERVICE) problems.push(`the Service with the label ${selector} is ${name}, not ${INGRESS_SERVICE}, the name Istio gives the Service of Gateway ${INGRESS_GATEWAY}`);
  const owners = Array.isArray(metadata.ownerReferences) ? metadata.ownerReferences.map(asRecord) : [];
  const ownedByGateway = owners.some((owner) => owner.kind === "Gateway" && owner.name === INGRESS_GATEWAY && typeof owner.apiVersion === "string" && owner.apiVersion.startsWith("gateway.networking.k8s.io/"));
  if (!ownedByGateway) problems.push(`Service ${name} is not owned by Gateway ${INGRESS_GATEWAY} (no ownerReference of kind Gateway in gateway.networking.k8s.io): Istio did not make it`);
  if (spec.type !== "ClusterIP") problems.push(`Service ${name} is not of type ClusterIP, which the chart's tunnel overlay sets`);
  const ports = Array.isArray(spec.ports) ? spec.ports.map(asRecord) : [];
  if (!ports.some((port) => port.port === INGRESS_PORT)) problems.push(`Service ${name} has no port ${INGRESS_PORT}, the tunnel listener's`);
  const read = addressOf(spec.clusterIP, `the ClusterIP of Service ${name}`, `Service ${name} has no ClusterIP`);
  if ("missing" in read) problems.push(read.missing);
  else if (!Array.isArray(spec.clusterIPs) || spec.clusterIPs[0] !== read.address) problems.push(`the clusterIPs of Service ${name} do not start with its ClusterIP ${read.address}`);
  return problems.length > 0 ? { missing: problems.join("; ") } : read;
}

/**
 * The static band of the cluster's Service range, from `kubectl get servicecidr kubernetes -o json`
 * (undefined: not found), as the nightly smoke computes it (staticBandOf). Without exactly one
 * valid IPv4 range there is no band, and the gate fails closed.
 */
export function serviceRangeBand(serviceCidr: Record<string, unknown> | undefined): BandRead {
  if (serviceCidr === undefined) return { missing: `ServiceCIDR ${SERVICE_CIDR} is not in the cluster` };
  const ipv4 = serviceCidrsOf(serviceCidr).filter((cidr) => cidr.includes("."));
  if (ipv4.length !== 1) return { missing: `ServiceCIDR ${SERVICE_CIDR} lists ${ipv4.length} IPv4 ranges, expected exactly one` };
  const cidr = ipv4[0] ?? "";
  let band: { first: string; last: string } | undefined;
  try {
    band = staticBandOf(cidr);
  } catch {
    return { missing: `ServiceCIDR ${SERVICE_CIDR}'s IPv4 range is not a valid CIDR` };
  }
  if (band === undefined) return { missing: `the Service range ${cidr} is too small to have a static band` };
  return { cidr, ...band };
}

/** A kubectl get for the comparison: the object, NotFound, or a fixed reason (never kubectl's or ssh's own words). */
type KubectlRead = { found: Record<string, unknown> } | { notFound: true } | { failed: string };

async function kubectlRead(box: Box, args: readonly string[]): Promise<KubectlRead> {
  const shown = `sudo k3s kubectl ${args.join(" ")}`;
  let result: RunResult;
  try {
    result = await box.tryKubectl([...args, "-o", "json"]);
  } catch {
    return { failed: `${shown} could not run; run it on the demo machine to see why` };
  }
  if (result.exitCode !== 0) {
    if (/\(NotFound\)/.test(result.stderr)) return { notFound: true };
    return { failed: `${shown} failed (exit ${result.exitCode}${result.exitCode === 255 ? ", ssh" : ""}); run it on the demo machine to see why` };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(result.stdout);
  } catch {
    return { failed: `${shown} printed no JSON` };
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return { failed: `${shown} printed no JSON object` };
  return { found: parsed as Record<string, unknown> };
}

async function readCluster<T>(box: Box, args: readonly string[], parse: (value: Record<string, unknown> | undefined) => T): Promise<T | { missing: string }> {
  const read = await kubectlRead(box, args);
  return "failed" in read ? { missing: read.failed } : parse("found" in read ? read.found : undefined);
}

/**
 * The three addresses and the static band, read only (git on this checkout, kubectl get on the
 * demo machine). Never throws: a read that fails, also one that could not run, is a reason.
 */
export async function readIngressAddresses(box: Box, values: ValuesReader): Promise<IngressAddresses> {
  let checkout: { commit: string; values: AddressRead };
  try {
    checkout = await values();
  } catch {
    checkout = { commit: "", values: { missing: `${DEMO_VALUES} at the checkout's commit could not be read (git could not run)` } };
  }
  return {
    commit: checkout.commit,
    values: checkout.values,
    overlay: await readCluster(box, ["-n", DEMO_NAMESPACE, "get", "configmap", INGRESS_OPTIONS], overlayClusterIp),
    service: await readCluster(box, ["-n", DEMO_NAMESPACE, "get", "services", "--selector", `${GATEWAY_NAME_LABEL}=${INGRESS_GATEWAY}`], serviceClusterIp),
    band: await readCluster(box, ["get", "servicecidr", SERVICE_CIDR], serviceRangeBand),
  };
}

/** The tunnel routes' origin in stage 2. */
export function ingressOrigin(address: string): string {
  return `http://${address}:${INGRESS_PORT}`;
}

/** The comparison: the stage-2 origin when the gate passes, otherwise each problem. */
export type AddressComparison = { origin: string } | { problems: string[] };

/**
 * What stops the stage-2 routes from going to the ingress: a missing address or band, addresses
 * that differ, or an address outside the static band. The likely cause of each difference
 * (deploy/chart/README.md 「固定的入口 ClusterIP」): values against the overlay, the deployed release
 * is not this commit; the overlay against the Service, the Service was created before the address
 * was set or an address change is half done, and Istio no longer updates the Service while they
 * differ. Outside the band, any Service created while the ingress Service is gone may take it.
 */
export function compareAddresses(addresses: IngressAddresses): AddressComparison {
  const { values, overlay, service, band } = addresses;
  const problems: string[] = [];
  if ("missing" in values) problems.push(`no values address: ${values.missing}`);
  if ("missing" in overlay) problems.push(`no ConfigMap overlay address: ${overlay.missing}`);
  if ("missing" in service) problems.push(`no Service address: ${service.missing}`);
  if ("missing" in band) problems.push(`no static band of the Service range: ${band.missing}`);
  if (!("address" in values) || !("address" in overlay) || !("address" in service)) return { problems };
  if (values.address !== overlay.address) {
    problems.push(
      `${DEMO_VALUES} at ${addresses.commit.slice(0, 12)} pins ${values.address}, but the deployed ConfigMap ${INGRESS_OPTIONS} has ${overlay.address}: the release in the cluster is not from this commit (Flux has not applied it yet, or this checkout is not the deployed commit)`,
    );
  }
  if (overlay.address !== service.address) {
    problems.push(
      `ConfigMap ${INGRESS_OPTIONS} asks for ${overlay.address}, but the ingress Service has ${service.address}: the Service was created before the address was set, or an address change is half done; Istio no longer updates this Service while they differ`,
    );
  }
  if (problems.length === 0 && "cidr" in band) {
    const value = ipv4ToNumber(service.address);
    if (value < ipv4ToNumber(band.first) || value > ipv4ToNumber(band.last)) {
      problems.push(
        `${service.address} is outside the static band ${band.first}-${band.last} of the Service range ${band.cidr}: a Service created while the ingress Service is gone could take it; pin an address in the band (${FIXED_ADDRESS_DOC}, 位址怎麼選)`,
      );
    }
  }
  return problems.length > 0 ? { problems } : { origin: ingressOrigin(service.address) };
}

/** The comparison's lines: the three addresses, the band, and OK or FAIL with the problems. */
export function addressReport(addresses: IngressAddresses): string[] {
  const text = (read: AddressRead) => ("address" in read ? read.address : `none (${read.missing})`);
  const compared = compareAddresses(addresses);
  return [
    `ingress address in ${DEMO_VALUES}${addresses.commit === "" ? "" : ` at ${addresses.commit.slice(0, 12)}`} (ingress.tunnel.clusterIP): ${text(addresses.values)}`,
    `ingress address in ConfigMap ${INGRESS_OPTIONS} (service overlay): ${text(addresses.overlay)}`,
    `ingress address of the Service with ${GATEWAY_NAME_LABEL}=${INGRESS_GATEWAY}: ${text(addresses.service)}`,
    `static band of the cluster's Service range (ServiceCIDR ${SERVICE_CIDR}): ${"cidr" in addresses.band ? `${addresses.band.first}-${addresses.band.last} of ${addresses.band.cidr}` : `none (${addresses.band.missing})`}`,
    "origin" in compared
      ? "OK the three ingress addresses agree and are in the static band"
      : `FAIL the ingress addresses do not allow the stage-2 routes: ${compared.problems.join("; ")}; see ${FIXED_ADDRESS_DOC}`,
  ];
}

/** The gate's refusal, the same for route-origin and check --stage 2. */
function gateError(problems: readonly string[]): ReleaseError {
  return new ReleaseError(
    `the stage-2 routes must not go to the ingress, nothing was probed: ${problems.join("; ")}. Do not switch the routes, and set back to http_status:418 any that go to the ingress; follow ${FIXED_ADDRESS_DOC} (deploy the reviewed values first, and change an address only as its 「改位址」 steps say)`,
  );
}

/** The stage-2 origin when the gate passes, otherwise the gate's refusal. */
export function requireIngressAddress(addresses: IngressAddresses): string {
  const compared = compareAddresses(addresses);
  if ("problems" in compared) throw gateError(compared.problems);
  return compared.origin;
}

/**
 * The gate before the stage-2 routes, the one code path of route-origin and check --stage 2: reads
 * the addresses (never throws), prints the comparison, and on a refusal saves `record` with the
 * addresses and the problems (and `refused`, check's `probed: false`) before throwing. A route to an
 * address no Service holds would publish whatever Service takes it later on the two paths that
 * bypass Access (Laysi/trustiot-v2#483).
 */
export async function ingressGate(box: Box, log: Log, values: ValuesReader, record: string, refused: Record<string, unknown>): Promise<{ addresses: IngressAddresses; origin: string }> {
  const addresses = await readIngressAddresses(box, values);
  for (const line of addressReport(addresses)) log.note(line);
  const compared = compareAddresses(addresses);
  if ("problems" in compared) {
    log.save(record, { at: new Date().toISOString(), ...refused, addresses, problems: compared.problems });
    throw gateError(compared.problems);
  }
  return { addresses, origin: compared.origin };
}

/** The line that names the stage-2 origin: route-origin's last line, and check's. */
export function originLine(origin: string): string {
  return `the stage-2 routes' origin: ${origin}`;
}

/**
 * route-origin: the gate on its own, read only and without the lab lock, run right before the
 * routes are switched. It exits 0 only when the gate passes; its last line then names the origin
 * the routes get, taken from its output, never typed from values-demo.yaml.
 */
export async function routeOrigin(box: Box, log: Log, values: ValuesReader = () => valuesAddress(ROOT)): Promise<string> {
  const { addresses, origin } = await ingressGate(box, log, values, "route-origin", {});
  log.save("route-origin", { at: new Date().toISOString(), addresses, origin });
  return originLine(origin);
}

// --- The trusted zone (install's backup, remove's restore) ---------------------------------------

/**
 * firewall-cmd's options that read the configuration as a whole (the `options_config` group of
 * firewall-cmd 2.x: every --info-*, --get-zones, --get-services, --get-icmptypes, --get-policies,
 * --get-ipsets, --get-helpers, --get-zone-of-*, --list-all-zones, --list-all-policies).
 * firewall-cmd refuses each of them next to --zone: it exits 2 with "Wrong usage of --get-zones |
 * --get-services | --get-icmptypes | --get-policies." (--info-zone=<zone> names the zone itself).
 */
const FIREWALL_CONFIG_OPTION = /^--(info-[a-z]+=.+|get-(zones|services|icmptypes|policies|ipsets|helpers)|get-zone-of-(interface|source)=.+|list-all-(zones|policies))$/;
/** The firewall-cmd arguments these scripts use. */
const FIREWALL_ARGUMENT = /^(-q|--permanent|--zone=trusted|--info-zone=trusted|--(query|add|remove)-(rich-rule|source)=.+)$/;
/** Options that act on one zone: without --zone they act on the default zone, never what is meant here. */
const FIREWALL_ZONE_ACTION = /^--(query|add|remove)-/;

/**
 * A firewall-cmd command line for the scripts that run on the demo machine, with each argument
 * shell-quoted. Every firewall-cmd call these scripts make is built here, so firewall-cmd's usage
 * rules are checked when the script is built, on this workstation, not first on the demo machine:
 * only the arguments above, exactly one action, no --zone next to a configuration-wide option
 * (FIREWALL_CONFIG_OPTION), and an explicit --zone=trusted for an action on a zone.
 */
export function firewallCmd(...args: string[]): string {
  for (const arg of args) if (!FIREWALL_ARGUMENT.test(arg)) throw new ReleaseError(`firewall-cmd argument these scripts do not use: ${arg}`);
  const zone = args.includes("--zone=trusted");
  const config = args.filter((arg) => FIREWALL_CONFIG_OPTION.test(arg));
  const actions = args.filter((arg) => FIREWALL_ZONE_ACTION.test(arg));
  if (zone && config.length > 0) throw new ReleaseError(`firewall-cmd refuses --zone next to ${config.join(" ")} (it exits 2: "Wrong usage of --get-zones | ...")`);
  if (actions.length > 0 && !zone) throw new ReleaseError(`firewall-cmd ${actions.join(" ")} needs --zone=trusted, or it acts on the default zone`);
  if (actions.length + config.length !== 1) throw new ReleaseError(`firewall-cmd takes exactly one action here, not: ${args.join(" ")}`);
  return ["firewall-cmd", ...args.map((arg) => (/^[\w=./:-]+$/.test(arg) ? arg : shq(arg)))].join(" ");
}

/** firewalld's runtime and permanent view of the trusted zone. */
export const TRUSTED_ZONE_VIEWS = {
  runtime: firewallCmd("--info-zone=trusted"),
  permanent: firewallCmd("--permanent", "--info-zone=trusted"),
} as const;

/** The root script that records firewalld's state for the host records before and after an install. */
export function firewalldRecordScript(): string {
  return `sha256sum /etc/firewalld/zones/*.xml /etc/firewalld/policies/*.xml 2>/dev/null; ${TRUSTED_ZONE_VIEWS.runtime}; ${TRUSTED_ZONE_VIEWS.permanent}`;
}

/**
 * `firewall-cmd [--permanent] --info-zone=trusted`, as sorted values per field (rich rules and
 * forward ports as written). A field line is indented by exactly two spaces (`  sources: a b`);
 * fields that list one entry per line (rich rules, forward ports) put each entry on its own line
 * indented further, with a tab in firewall-cmd, and any deeper indentation, tabs or spaces, counts
 * the same. A rich rule must start with `rule`. Anything else, or an output without the zone name,
 * its sources or its rich rules, fails: remove never decides on a misread zone.
 */
export function parseZoneInfo(text: string): Record<string, string[]> {
  const fields: Record<string, string[]> = {};
  let current = "";
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (trimmed === "") continue;
    if (/^\S/.test(line)) {
      if (fields.zone !== undefined) throw new ReleaseError(`the trusted zone view (firewall-cmd's --info-zone) printed a second zone: ${trimmed}`);
      fields.zone = [trimmed.replace(/\s*\(.*\)$/, "")];
      continue;
    }
    const field = /^ {2}([a-z0-9][a-z0-9 -]*):(?:\s+(.*))?$/.exec(line);
    if (field !== null) {
      current = field[1] ?? "";
      const values = (field[2] ?? "").split(/\s+/).filter(Boolean);
      if (current === "rich rules" && values.length > 0) throw new ReleaseError(`cannot read this line of the trusted zone view (firewall-cmd's --info-zone): ${trimmed}`);
      fields[current] = values;
      continue;
    }
    if (current === "" || (current === "rich rules" && !/^rule(\s|$)/.test(trimmed))) {
      throw new ReleaseError(`cannot read this line of the trusted zone view (firewall-cmd's --info-zone): ${trimmed}`);
    }
    fields[current]?.push(trimmed);
  }
  for (const required of ["zone", "sources", "rich rules"]) {
    if (fields[required] === undefined) throw new ReleaseError(`the trusted zone view (firewall-cmd's --info-zone) printed no ${required === "zone" ? "zone name" : `"${required}:" line`}`);
  }
  for (const key of Object.keys(fields)) fields[key]?.sort();
  return fields;
}

export interface ZoneItems {
  rule: boolean;
  serviceNetwork: boolean;
}

/** Whether a trusted zone view holds the installer's two items: its exact pod-SSH rule and the Service network as a source. */
export function zoneItems(info: Record<string, string[]>): ZoneItems {
  return { rule: (info["rich rules"] ?? []).includes(POD_SSH_RULE), serviceNetwork: (info.sources ?? []).includes(SERVICE_NETWORK) };
}

/** The fields in which two views of the zone differ. */
export function zoneDiff(before: Record<string, string[]>, now: Record<string, string[]>): string[] {
  return [...new Set([...Object.keys(before), ...Object.keys(now)])].filter((key) => stableJson(before[key] ?? []) !== stableJson(now[key] ?? [])).sort();
}

export interface ZoneBackup {
  /** Whether /etc/firewalld/zones/trusted.xml existed before the first install. */
  fileExisted: boolean;
  runtime: string;
  permanent: string;
}

export interface Reverts {
  removeRule: boolean;
  addServiceNetwork: boolean;
}

export interface ZoneRestore {
  runtime: { revert: Reverts; want: ZoneItems };
  permanent: { revert: Reverts; want: ZoneItems };
}

/** Undo exactly the installer's two items where the zone differs from the backup in them, per scope. */
export function zoneRestore(backup: ZoneBackup, now: { runtime: string; permanent: string }): ZoneRestore {
  const one = (before: string, current: string) => {
    const want = zoneItems(parseZoneInfo(before));
    const has = zoneItems(parseZoneInfo(current));
    return { revert: { removeRule: has.rule && !want.rule, addServiceNetwork: !has.serviceNetwork && want.serviceNetwork }, want };
  };
  return { runtime: one(backup.runtime, now.runtime), permanent: one(backup.permanent, now.permanent) };
}

/** After the restore: the backup goes only when the whole zone equals it again, in both views. */
export function backupDecision(backup: ZoneBackup, now: { runtime: string; permanent: string }): { delete: boolean; differences: string[] } {
  const differences = [
    ...zoneDiff(parseZoneInfo(backup.runtime), parseZoneInfo(now.runtime)).map((field) => `runtime ${field}`),
    ...zoneDiff(parseZoneInfo(backup.permanent), parseZoneInfo(now.permanent)).map((field) => `permanent ${field}`),
  ];
  return { delete: differences.length === 0, differences };
}

/**
 * The root script that saves the trusted zone once, before the first install: the zone file (or a
 * note that there was none) and firewalld's runtime and permanent view. A complete backup is never
 * overwritten, so a later install (a token rotation) keeps the state from before the first one.
 */
export function zoneBackupScript(dir: string = ZONE_BACKUP, zoneFile: string = TRUSTED_ZONE): string {
  return [
    "set -eu",
    "umask 077",
    `d=${shq(dir)}`,
    `z=${shq(zoneFile)}`,
    'if [ -e "$d/complete" ]; then echo kept; exit 0; fi',
    'install -d -m 0700 "$d"',
    `rm -f "$d/trusted.xml" "$d/trusted.xml.absent"`,
    `if [ -e "$z" ]; then cp -p "$z" "$d/trusted.xml"; else : > "$d/trusted.xml.absent"; fi`,
    `${TRUSTED_ZONE_VIEWS.runtime} > "$d/runtime.txt"`,
    `${TRUSTED_ZONE_VIEWS.permanent} > "$d/permanent.txt"`,
    ': > "$d/complete"',
    "echo saved",
  ].join("\n");
}

/**
 * The root shell script that puts the trusted zone's two installer changes back as the backup had
 * them, in the runtime and the permanent configuration, then checks both. Any firewall-cmd error
 * or a state other than the backup's stops it with a non-zero status.
 */
export function restoreTrustedZoneScript(plan: ZoneRestore): string {
  const scopes = [
    { flags: ["--zone=trusted"], label: "runtime", ...plan.runtime },
    { flags: ["--zone=trusted", "--permanent"], label: "permanent", ...plan.permanent },
  ];
  const lines = [
    "set -eu",
    // firewall-cmd --query-*: 0 yes, 1 no, anything else an error.
    `q() { rc=0; "$@" > /dev/null 2>&1 || rc=$?; case $rc in 0) echo present ;; 1) echo absent ;; *) echo "$* failed (exit $rc)" >&2; return 1 ;; esac; }`,
  ];
  for (const { flags, revert } of scopes) {
    if (revert.removeRule) lines.push(firewallCmd("-q", ...flags, `--remove-rich-rule=${POD_SSH_RULE}`));
    if (revert.addServiceNetwork) lines.push(firewallCmd("-q", ...flags, `--add-source=${SERVICE_NETWORK}`));
  }
  for (const { flags, label, want } of scopes) {
    const rule = want.rule ? "present" : "absent";
    const net = want.serviceNetwork ? "present" : "absent";
    lines.push(`rule=$(q ${firewallCmd(...flags, `--query-rich-rule=${POD_SSH_RULE}`)})`);
    lines.push(`net=$(q ${firewallCmd(...flags, `--query-source=${SERVICE_NETWORK}`)})`);
    lines.push(
      `if [ "$rule" != ${rule} ] || [ "$net" != ${net} ]; then echo "the trusted zone (${label}) is not as in the backup: pod-SSH rule $rule (want ${rule}), ${SERVICE_NETWORK} $net (want ${net})" >&2; exit 1; fi`,
    );
  }
  return lines.join("\n");
}

// --- Remove -------------------------------------------------------------------------------------

export interface RemovePlan {
  unit: string;
  trustedZone: ZoneRestore | string;
}

/** What remove does, from the unit's state, the trusted zone backup (null when there is none) and the zone now. */
export function removePlan(unitState: string, backup: ZoneBackup | null, now: { runtime: string; permanent: string }): RemovePlan {
  return {
    unit: /LoadState=loaded/.test(unitState)
      ? `stop, disable and delete ${DEMO_TUNNEL_UNIT}.service, its token, its config and ${RUN_DIR}`
      : `${DEMO_TUNNEL_UNIT}.service is gone; delete its token, its config and ${RUN_DIR} if they are there`,
    trustedZone: backup === null ? "no backup: left as it is" : zoneRestore(backup, now),
  };
}

/**
 * The root shell script that takes the connector out. It deletes the token, the config and the
 * hand-off files only once the unit is stopped (or was never there): a connector that failed to
 * stop keeps its tunnel connected, so the run stops instead and leaves everything for a look by
 * hand. /etc is btrfs (copy on write): the deleted token's blocks may stay on disk, so the token's
 * end is its rotation or the tunnel's deletion in Cloudflare (docs/runbooks/demo-environment.md).
 */
export function removeScript(): string {
  const unit = `${DEMO_TUNNEL_UNIT}.service`;
  return [
    "set -u",
    `if [ "$(systemctl show ${unit} -p LoadState --value)" != not-found ]; then systemctl disable --now ${unit} || { echo "could not stop ${unit}; nothing deleted" >&2; exit 1; }; fi`,
    `state=$(systemctl show ${unit} -p ActiveState --value) || { echo "could not read ${unit}'s state; nothing deleted" >&2; exit 1; }`,
    `case "$state" in inactive | failed) ;; *) echo "${unit} is still \${state:-in an unknown state}; nothing deleted" >&2; exit 1 ;; esac`,
    `rm -f ${CREDENTIAL} ${CONFIG} /etc/systemd/system/${unit} || exit 1`,
    `systemctl daemon-reload || { echo "systemctl daemon-reload failed: systemd may still have ${unit} loaded" >&2; exit 1; }`,
    `rm -rf ${RUN_DIR} || exit 1`,
  ].join("\n");
}

// --- The token audit (status) ---------------------------------------------------------------------

export interface AuditPaths {
  credential: string;
  dirs: string[];
  /** Where root writes the 0600 pattern file (tmpfs on the demo machine), outside the searched dirs. */
  patternDir: string;
}

/**
 * The root script that looks for the token, and for the secret `s` inside it, in the whole journal
 * (every field, -o verbose, so command lines too) and in the hand-off and state directories. Root
 * decodes both into a 0600 pattern file and grep reads them only with -f: neither value is ever an
 * argument, a variable on a command line or output. Only counts come back.
 */
export function auditScript(paths: AuditPaths = { credential: CREDENTIAL, dirs: [STATE_DIR, RUN_DIR], patternDir: "/run" }): string {
  const dirs = paths.dirs.map(shq).join(" ");
  return [
    "set -u",
    "umask 077",
    `f=${shq(paths.credential)}`,
    `[ -s "$f" ] || { echo "no token file"; exit 0; }`,
    `p=$(mktemp -p ${shq(paths.patternDir)} trustiot-demo-audit.XXXXXXXX) || exit 1`,
    `trap 'rm -f "$p"' EXIT`,
    `tr -d '\\n' < "$f" > "$p" && echo >> "$p"`,
    `base64 -d < "$f" 2>/dev/null | sed -n 's/.*"s"[[:space:]]*:[[:space:]]*"\\([^"]\\{16,\\}\\)".*/\\1/p' | head -n 1 >> "$p"`,
    `if grep -qx '' "$p"; then echo "empty pattern; nothing searched"; exit 1; fi`,
    `echo "patterns: $(grep -c . "$p") (the token, and its secret when it could be read)"`,
    `echo "journal lines with the token or its secret: $(journalctl --no-pager -o verbose 2>/dev/null | grep -cFf "$p")"`,
    `echo "files in ${paths.dirs.join(" and ")} with the token or its secret: $(grep -rlFf "$p" ${dirs} 2>/dev/null | wc -l)"`,
  ].join("\n");
}

/** Files under a directory whose content contains a string. */
export function filesWith(dir: string, needle: string): string[] {
  const found: string[] = [];
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    const info = statSync(path);
    if (info.isDirectory()) found.push(...filesWith(path, needle));
    else if (info.isFile() && readFileSync(path, "utf8").includes(needle)) found.push(path);
  }
  return found;
}

/** A path argument as the person may paste it back from the output: a leading `~/` is the home directory. */
export function expandHome(path: string): string {
  const home = (process.env.HOME || homedir()).replace(/\/+$/, "");
  return path === "~" ? home : path.startsWith("~/") ? join(home, path.slice(2)) : path;
}

/** The --ciphertext file's content; errors name the path as shownPath prints it. */
export function readCiphertextFile(path: string): string {
  try {
    return readFileSync(expandHome(path), "utf8");
  } catch {
    throw new ReleaseError(`could not read --ciphertext ${shownPath(expandHome(path))}`);
  }
}

// --- The install flow -------------------------------------------------------------------------------

export interface InstallPlan {
  unit: string;
  tunnelId: string;
  head: string;
  installer: string;
  runner: string;
  ciphertext: string;
  args: string[];
  changes: string[];
  allowedExtra: string[];
  cloudflared: CloudflaredPlan;
  updateUnits: UnitState[];
  metricsPort: number;
}

/** A run that may still be going on the demo machine: the lab lock must stay. */
export class StillRunningError extends ReleaseError {}

export interface RunOutcome {
  id: string;
  /** The installer's exit status; null when the runner never wrote one (stopped, killed, or failed to start). */
  exitCode: number | null;
  output: string;
  post: Record<string, string>;
}

export interface Clock {
  now: () => number;
  sleep: (ms: number) => Promise<void>;
}

const REAL_CLOCK: Clock = { now: () => Date.now(), sleep: (ms) => Bun.sleep(ms) };

/** The `.post` file's key=value lines. */
export function parsePost(text: string): Record<string, string> {
  return Object.fromEntries(
    text
      .split("\n")
      .map((line) => /^([a-z_]+)=(.*)$/.exec(line.trim()))
      .filter((m): m is RegExpExecArray => m !== null)
      .map((m) => [m[1] ?? "", m[2] ?? ""]),
  );
}

/** What to do when a run outlives the wait or the workstation lost sight of it. */
export function stillRunningHelp(id: string): string {
  const unit = `${RUN_UNIT_PREFIX}${id}`;
  return [
    `on the demo machine: systemctl status ${unit} (gone means the run ended; its result is in ${STATE_DIR}/${id}.post)`,
    `sudo tail ${STATE_DIR}/${id}.log`,
    `cat ~/${LOCK_DIR}/owner (the lab lock stays held for ${ISSUE} until the run has ended)`,
    `to stop the run: sudo systemctl stop ${unit} (this interrupts dnf and firewall-cmd mid-way; its ExecStopPost still puts back the cloudflared link and marker)`,
    `once ${STATE_DIR}/${id}.post exists, run status, then remove the lock with rm -rf ~/${LOCK_DIR} if its owner line is this run's`,
  ].join("; ");
}

/**
 * Follows a started run until its .post file appears (the run's last file, written by
 * ExecStopPost however the run ended). Running past the wait, or losing sight of the run, throws
 * StillRunningError: the run may still change the host, so the caller keeps the lab lock.
 */
export async function waitForRun(box: Box, id: string, log: Log, clock: Clock = REAL_CLOCK): Promise<RunOutcome> {
  const unit = `${RUN_UNIT_PREFIX}${id}.service`;
  const deadline = clock.now() + INSTALLER_WAIT_MS;
  let shown = 0;
  let gone = 0;
  for (;;) {
    const post = await box.sh(`sudo cat ${STATE_DIR}/${id}.post 2>/dev/null || true`);
    const logRead = await box.sh(`sudo cat ${STATE_DIR}/${id}.log 2>/dev/null || true`);
    const output = logRead.exitCode === 0 ? logRead.stdout : "";
    const done = post.exitCode === 0 && post.stdout.trim() !== "";
    const lines = output.split("\n");
    const complete = done ? lines : lines.slice(0, -1);
    for (const line of complete.slice(shown)) if (line !== "") log.note(`  | ${redactSecrets(line)}`);
    shown = Math.max(shown, complete.length);
    if (done) {
      const exit = (await box.sh(`sudo cat ${STATE_DIR}/${id}.exit 2>/dev/null || true`)).stdout.trim();
      return { id, exitCode: /^\d+$/.test(exit) ? Number(exit) : null, output, post: parsePost(post.stdout) };
    }
    const state = await box.sh(`systemctl show ${unit} -p LoadState -p ActiveState -p SubState`);
    if (state.exitCode === 0 && /^LoadState=not-found$/m.test(state.stdout)) gone += 1;
    else gone = 0;
    if (gone >= LOST_POLLS) {
      throw new StillRunningError(`installer run ${id}: its unit is gone but it wrote no ${STATE_DIR}/${id}.post; check the host by hand before anything else: ${stillRunningHelp(id)}`);
    }
    if (clock.now() > deadline) {
      throw new StillRunningError(`installer run ${id} did not finish within ${INSTALLER_WAIT_MS / 60_000} minutes and may still be running: ${stillRunningHelp(id)}`);
    }
    await clock.sleep(POLL_MS);
  }
}

/**
 * What ExecStopPost did not put back, from its .post fields: a requested link or marker must be
 * `restored` or `present`; anything else (conflict, failed, not-asked, missing) is a problem.
 */
export function restoreProblems(post: Record<string, string>, requested: { link: boolean; marker: boolean }): string[] {
  return (["link", "marker"] as const).filter((name) => requested[name] && !["restored", "present"].includes(post[name] ?? "")).map((name) => `${name === "link" ? LOCAL_LINK : PACKAGE_MARKER} (${name}=${post[name] ?? "missing"})`);
}

export interface ApplySteps {
  /** Takes the lab lock (or checks --lock-held) and returns its release. */
  lock: () => Promise<() => Promise<void>>;
  /** Stages the files and plans again, under the lock. */
  plan: () => Promise<InstallPlan>;
  /** Throws unless --confirm names this plan. */
  confirm: (plan: InstallPlan) => void;
  backupZone: () => Promise<void>;
  record: (name: "host-before" | "host-after") => Promise<void>;
  run: (plan: InstallPlan) => Promise<RunOutcome>;
  /** Deletes the hand-off key and the ciphertext. */
  cleanup: () => Promise<void>;
  note: (line: string) => void;
}

/**
 * install --apply: the order of its steps and what happens on each way out. The plan is made again
 * under the lock; the hand-off files go only after a run that succeeded; the lock is released
 * unless a run may still be going (StillRunningError), and then the error says what to check.
 */
export async function applyInstall(steps: ApplySteps): Promise<RunOutcome> {
  const release = await steps.lock();
  let keepLock = false;
  try {
    // The plan's own dry run and the install both run on the demo machine; either may outlive us.
    const plan = await steps.plan();
    steps.confirm(plan);
    await steps.backupZone();
    await steps.record("host-before");
    const outcome = await steps.run(plan);
    await steps.record("host-after");
    const notBack = restoreProblems(outcome.post, { link: plan.cloudflared?.restoreLink === true, marker: plan.cloudflared?.restoreMarker === true });
    if (notBack.length > 0) steps.note(`FAIL the run's ExecStopPost did not put back: ${notBack.join(", ")}; see docs/runbooks/demo-environment.md 「cloudflared 的更新與退回」`);
    if (outcome.exitCode !== 0) {
      throw new ReleaseError(
        `the installer failed (exit ${outcome.exitCode ?? `none, ${outcome.post.service_result ?? "unknown"}`}): ${redactSecrets(lastLines(outcome.output))}; the hand-off key and the ciphertext stay for another run`,
      );
    }
    if (notBack.length > 0) {
      throw new ReleaseError(
        `the installer finished, but ${notBack.join(", ")} was not put back: the install is not done. Put them back by hand (docs/runbooks/demo-environment.md 「cloudflared 的更新與退回」) and run status; the hand-off key and the ciphertext stay`,
      );
    }
    await steps.cleanup();
    return outcome;
  } catch (error) {
    if (error instanceof StillRunningError) keepLock = true;
    throw error;
  } finally {
    if (keepLock) steps.note(`the lab lock stays held for ${ISSUE}: the installer run may still be going`);
    else await release();
  }
}

// --- Demo machine ---------------------------------------------------------------------------------

/**
 * A path's file type as `stat -c %F` names it ("symbolic link", "regular file", ...), or "absent".
 * stat names types in the session's language (an ssh session may carry the workstation's), and
 * linkFacts compares the English names, so it runs with LC_ALL=C.
 */
export function fileTypeCommand(path: string): string {
  return `LC_ALL=C stat -c '%F' ${path} 2>/dev/null || echo absent`;
}

/**
 * The installed cloudflared's version-release, or nothing. `rpm -q` prints "package ... is not
 * installed" on stdout, in the session's language, so the exit status of `rpm -q --quiet` decides.
 */
export const INSTALLED_VERSION_COMMAND = "if rpm -q --quiet cloudflared; then rpm -q --qf '%{version}-%{release}\\n' cloudflared; fi";

/** What the installer may change, recorded before and after (names, versions and sha256 only). */
async function hostRecord(box: Box): Promise<Record<string, string>> {
  const read = async (command: string) => (await box.sh(command)).stdout.trim();
  return {
    unit: await read(`systemctl show ${DEMO_TUNNEL_UNIT}.service -p LoadState -p ActiveState -p MainPID -p NRestarts`),
    // Never ExecStart: a unit may carry its tunnel token on the command line.
    ops: await read(`systemctl show ${OPS_TUNNEL_UNIT} -p ActiveState -p MainPID -p NRestarts`),
    cloudflared: await read(`LC_ALL=C rpm -q cloudflared; sha256sum ${CLOUDFLARED_BINARY}; ${FROM_REPO_COMMAND} 2>&1`),
    links: await read(`${fileTypeCommand(LOCAL_LINK)}; readlink ${LOCAL_LINK}; ${fileTypeCommand(PACKAGE_MARKER)}`),
    firewalld: await read(`sudo sh -c ${shq(firewalldRecordScript())}`),
  };
}

async function readLinkFacts(box: Box): Promise<{ link: LinkFacts; marker: CloudflaredFacts["marker"] }> {
  const read = async (command: string) => (await box.sh(command)).stdout.trim();
  const link = linkFacts(await read(fileTypeCommand(LOCAL_LINK)), await read(`readlink ${LOCAL_LINK} || true`));
  const markerType = await read(`if [ -L ${PACKAGE_MARKER} ]; then echo other; elif [ -f ${PACKAGE_MARKER} ]; then echo file; elif [ -e ${PACKAGE_MARKER} ]; then echo other; else echo absent; fi`);
  return { link, marker: markerType === "file" ? "file" : markerType === "absent" ? "absent" : "other" };
}

/** The installed package and, only for a swap, the version the pinned repository offers. */
export async function readCloudflaredFacts(box: Box, swap: boolean): Promise<CloudflaredFacts> {
  const read = async (command: string) => (await box.sh(command)).stdout.trim();
  // Read only, without root: dnf reads the repository's metadata into a throwaway cache on tmpfs.
  const candidate = swap
    ? await read(
        [
          'd=$(mktemp -d -p "${XDG_RUNTIME_DIR:?}" trustiot-cf.XXXXXXXX)',
          `timeout 300 dnf repoquery -q --setopt=cachedir="$d" --repofrompath=trustiot-cf-probe,${PINNED_REPO_URL} --repo=trustiot-cf-probe --arch="$(uname -m)" --latest-limit=1 --qf '%{version}-%{release}\\n' cloudflared`,
          'rm -rf "$d"',
        ].join("; "),
      )
    : "";
  return {
    installed: await read(INSTALLED_VERSION_COMMAND),
    fromRepo: fromRepoResult(await box.sh(FROM_REPO_COMMAND)),
    candidate: candidate.split("\n").at(-1) ?? "",
    ...(await readLinkFacts(box)),
  };
}

async function readUpdateUnits(box: Box): Promise<UnitState[]> {
  return unitStates(await box.mustSh(`systemctl show ${UPDATE_UNITS.join(" ")} -p Id -p LoadState -p UnitFileState -p ActiveState`, "reading cloudflared's update units"));
}

/**
 * Token-shaped text (base64 JSON, which starts with eyJ) and long base64 runs, such as the
 * ciphertext, replaced before remote output reaches the log or an error. The installer never
 * prints the token, but a failure on a path that carries it must not be the first time it does.
 * sha256 sums (64 hex digits) and PEM lines (64 characters) stay.
 */
export function redactSecrets(text: string): string {
  return text.replace(/eyJ[A-Za-z0-9+/_=-]{16,}/g, "<redacted>").replace(/[A-Za-z0-9+/_-]{80,}={0,2}/g, "<redacted>");
}

/**
 * A remote step on the token's or the ciphertext's path: sent once (ssh never repeats it; after a
 * dropped connection the caller reads the state back or stops), and its output redacted in errors.
 */
export async function quietSh(box: Box, command: string, what: string, stdin?: Uint8Array | string): Promise<string> {
  const result = await box.sh(command, { retry: false, ...(stdin === undefined ? {} : { stdin }) });
  if (result.exitCode !== 0) {
    throw new ReleaseError(`${what} failed (exit ${result.exitCode}${result.exitCode === 255 ? ", ssh dropped; not sent again" : ""}): ${redactSecrets(lastLines(result.stderr || result.stdout))}`);
  }
  return result.stdout;
}

/** Copies bytes to a root-only file on the demo machine and checks them there as root. */
export async function stage(box: Box, bytes: Uint8Array | string, target: string, mode: string): Promise<string> {
  const tmp = (await box.mustSh('mktemp -p "${XDG_RUNTIME_DIR:?}" trustiot-demo.XXXXXXXX', "making a temporary file")).trim();
  const want = new Bun.CryptoHasher("sha256").update(bytes).digest("hex");
  try {
    await quietSh(box, `cat > ${shq(tmp)}`, `copying ${target}`, bytes);
    const out = await quietSh(box, `sudo install -m ${mode} -o root -g root ${shq(tmp)} ${shq(target)} && sudo sha256sum ${shq(target)}`, `installing ${target}`);
    if (out.split(" ")[0] !== want) throw new ReleaseError(`${target} arrived changed on the demo machine`);
  } finally {
    await box.sh(`rm -f ${shq(tmp)}`);
  }
  return want;
}

/**
 * Starts the installer in its transient unit on the demo machine. Only two answers are certain:
 * exit 0 (the unit started) and START_REFUSED (nothing started). Anything else, such as ssh
 * dropping after systemd-run was sent, may have started the run: the unit and its files tell, and
 * when they cannot, StillRunningError keeps the lab lock.
 */
export async function startRun(box: Box, log: Log, dryRun: boolean, staged: Staged, restore: Restore, now: Date = new Date()): Promise<string> {
  const running = runningInstallUnits(await box.mustSh(`systemctl list-units --all --plain --no-legend '${RUN_UNIT_PREFIX}*'`, "listing installer runs"));
  if (running.length > 0) {
    throw new ReleaseError(`an installer run is still there on the demo machine (${running.join(", ")}); wait for it and read its log in ${STATE_DIR}`);
  }
  const id = runId(dryRun ? "dry-run" : "apply", now);
  const started = await box.sh(startCommand(id, staged, installerArgs(dryRun), restore), { retry: false });
  if (started.exitCode === START_REFUSED) throw new ReleaseError(`starting the installer was refused: ${redactSecrets(lastLines(started.stderr || started.stdout))}`);
  if (started.exitCode !== 0) {
    const unit = await box.sh(`systemctl show ${RUN_UNIT_PREFIX}${id}.service -p LoadState`);
    const files = await box.sh(`sudo sh -c ${shq(`ls ${STATE_DIR}/${id}.log ${STATE_DIR}/${id}.post 2>/dev/null || true`)}`);
    const loaded = unit.exitCode === 0 && /^LoadState=loaded$/m.test(unit.stdout);
    const wrote = files.exitCode === 0 && files.stdout.trim() !== "";
    if (!loaded && !wrote) {
      throw new StillRunningError(
        `starting installer run ${id} failed with exit ${started.exitCode} (${redactSecrets(lastLines(started.stderr || started.stdout, 2))}) and the demo machine could not show whether it started: ${stillRunningHelp(id)}`,
      );
    }
    log.note(`starting installer run ${id} returned exit ${started.exitCode}, but its unit or files are there: following it`);
  }
  log.note(`installer run ${id} started on the demo machine as ${RUN_UNIT_PREFIX}${id}.service (log ${STATE_DIR}/${id}.log, root only); it prints nothing while it reads firewalld zone by zone, which is normal`);
  return id;
}

export interface InstallInputs {
  tunnelId: string;
  allowedExtra: string[];
  installer: Uint8Array;
  ciphertext: string;
}

export interface PlanEnv {
  /** The checkout's state (codeState(ROOT)); tests pass their own. */
  code: () => Promise<CodeState>;
  clock?: Clock;
}

/** The checks, the installer's own dry run and the cloudflared facts: what --apply will do, and its code. */
export async function makePlan(box: Box, log: Log, inputs: InstallInputs, ops: { pid: string }, env: PlanEnv): Promise<InstallPlan> {
  const { head } = await env.code();
  const updateUnits = await readUpdateUnits(box);
  requireNoUpdateTimer(updateUnits);
  // Checked before anything is staged; the plan checks the link again after the dry run.
  cloudflaredPlan({ installed: "x", fromRepo: { unreadable: "not read for this check" }, candidate: "", ...(await readLinkFacts(box)) }, false);
  const demoPid = /^MainPID=(\d+)$/m.exec(await box.mustSh(`systemctl show ${DEMO_TUNNEL_UNIT}.service -p MainPID`, "reading the demo unit"))?.[1] ?? "0";
  const holder = metricsPortHolder(await box.mustSh(`sudo ss -Htlnp 'sport = :${DEMO_METRICS_PORT}'`, "reading the metrics port"), demoPid);
  if (holder !== "") throw new ReleaseError(`127.0.0.1:${DEMO_METRICS_PORT}, the demo connector's metrics port, is taken: ${holder}`);

  // The only writes of a dry run: the installer and its runner into the root-only state directory
  // and the ciphertext into the root-only tmpfs one, each checked there by root. The installer's
  // --dry-run changes nothing. Every run checks them again against the plan on the demo machine
  // (startCommand, runnerScript), so files staged by another invocation in the meantime stop the
  // run instead of being used.
  await box.mustSh(`sudo install -d -m 0700 -o root -g root ${STATE_DIR}`, `creating ${STATE_DIR}`);
  const staged: Staged = {
    installer: await stage(box, inputs.installer, BOX_INSTALLER, "0700"),
    runner: await stage(box, runnerScript(), BOX_RUNNER, "0700"),
    ciphertext: await stage(box, `${inputs.ciphertext}\n`, CIPHERTEXT_FILE, "0600"),
  };
  log.note(`staged the installer and its runner in ${STATE_DIR} and the ciphertext in ${RUN_DIR} (root only)`);
  log.note("the installer's own dry run (with the token, which it checks and never prints; it calls firewall-cmd as root many times):");
  const dry = await waitForRun(box, await startRun(box, log, true, staged, { restoreLink: false, restoreMarker: false }), log, env.clock);
  if (dry.exitCode !== 0) throw new ReleaseError(`the installer's dry run failed (exit ${dry.exitCode ?? dry.post.service_result ?? "none"}): ${redactSecrets(lastLines(dry.output))}`);

  const tokenTunnel = tunnelIdFromInstallerOutput(dry.output);
  if (tokenTunnel !== inputs.tunnelId) throw new ReleaseError(`the token belongs to tunnel ${tokenTunnel}, not --tunnel-id ${inputs.tunnelId}; nothing changed`);
  const changes = installerChanges(dry.output);
  checkChanges(changes, inputs.allowedExtra);
  const cloudflared = cloudflaredPlan(await readCloudflaredFacts(box, changes.includes(SWAP_CHANGE)), changes.includes(SWAP_CHANGE));
  const plan: InstallPlan = {
    unit: DEMO_TUNNEL_UNIT,
    tunnelId: tokenTunnel,
    head,
    ...staged,
    args: installerArgs(false),
    changes,
    allowedExtra: [...inputs.allowedExtra].sort(),
    cloudflared,
    updateUnits,
    metricsPort: DEMO_METRICS_PORT,
  };
  log.note(`plan: commit ${head}, tunnel ${tokenTunnel}, ${plan.changes.length} change(s) by the installer:`);
  for (const change of plan.changes) log.note(`  - ${change}`);
  if (changes.some((c) => c.startsWith("firewalld ("))) {
    log.note("  (the firewalld lines are deploy/host's host baseline, accepted for the demo machine on 2026-10-20: the trusted zone loses the Service network as a source and refuses pods' SSH, in the runtime and the permanent configuration; remove puts both back)");
  }
  for (const extra of plan.allowedExtra) log.note(`  allowed with --allow-change: ${extra}`);
  for (const warning of installerWarnings(dry.output)) log.note(`  ! ${warning}`);
  for (const line of describeCloudflared(cloudflared, ops)) log.note(line);
  log.note(`cloudflared update units: ${updateUnits.map((u) => `${u.name} ${u.load}`).join(", ")}`);
  log.save("plan", plan);
  return plan;
}

async function handoffKey(box: Box, log: Log): Promise<void> {
  const script = [
    `install -d -m 0700 -o root -g root ${RUN_DIR}`,
    `if [ ! -e ${HANDOFF_PEM} ]; then (umask 077 && openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:4096 -out ${HANDOFF_PEM} 2>/dev/null); fi`,
    `openssl pkey -in ${HANDOFF_PEM} -pubout`,
  ].join(" && ");
  const pem = await box.mustSh(`sudo bash -c ${shq(script)}`, "making the hand-off key", { retry: false });
  const fingerprint = spkiFingerprint(pem);
  if (Buffer.from(pem.replace(/-----[^-]+-----|\s/g, ""), "base64").length < 512) throw new ReleaseError("the hand-off public key is not an RSA-4096 key");
  log.save("handoff-public-key", { pem, spkiSha256: fingerprint });
  log.note(
    `hand-off public key (SPKI, RSA-4096; encrypt the tunnel token for it with RSA-OAEP, SHA-256), SHA-256 of its SPKI DER bytes ${fingerprint}; the private key stays root-only in ${HANDOFF_PEM} (tmpfs: a reboot deletes it, then run handoff-key again):\n${pem.trimEnd()}`,
  );
}

function readInstaller(): Uint8Array {
  const dist = join(ROOT, "deploy/host/dist");
  let installer: Uint8Array;
  try {
    installer = new Uint8Array(readFileSync(join(dist, INSTALLER_NAME)));
  } catch {
    throw new ReleaseError(`deploy/host/dist/${INSTALLER_NAME} is missing: run bun run --cwd deploy/host build`);
  }
  if (new Bun.CryptoHasher("sha256").update(installer).digest("hex") !== listedChecksum(readFileSync(join(dist, "SHA256SUMS"), "utf8"), INSTALLER_NAME)) {
    throw new ReleaseError(`${INSTALLER_NAME} does not match deploy/host/dist/SHA256SUMS; build it again`);
  }
  return installer;
}

async function install(box: Box, log: Log, raw: Record<string, string>, ops: { pid: string }, allowedExtra: string[]): Promise<void> {
  const options = commonOptions(raw);
  if (!raw.ciphertext) throw new ReleaseError("--ciphertext <file> is required: the base64 RSA-OAEP ciphertext of the tunnel token");
  const inputs: InstallInputs = {
    tunnelId: checkedTunnelId(raw["tunnel-id"] ?? ""),
    allowedExtra,
    ciphertext: checkedCiphertext(readCiphertextFile(raw.ciphertext)),
    installer: readInstaller(),
  };
  if ((await box.sh(`sudo test -f ${HANDOFF_PEM} && echo yes`)).stdout.trim() !== "yes") {
    throw new ReleaseError(`no hand-off key on the demo machine (${HANDOFF_PEM}; a reboot deletes it); run handoff-key and have the token encrypted for it`);
  }
  const planEnv: PlanEnv = { code: () => codeState(ROOT) };
  const command = `bun scripts/demo/tunnel.ts install --ssh-host <host> --ciphertext <file> --tunnel-id ${inputs.tunnelId}${allowedExtra.map((line) => ` --allow-change ${shq(line)}`).join("")}`;
  if (!options.apply) {
    await checkLockForDryRun(box);
    checkConfirmation(log, await makePlan(box, log, inputs, ops, planEnv), options, command);
    return;
  }
  const outcome = await applyInstall({
    lock: () => takeLock(box, ISSUE, "tunnel-install", options.lockHeld),
    plan: () => makePlan(box, log, inputs, ops, planEnv),
    confirm: (plan) => void checkConfirmation(log, plan, options, command),
    backupZone: async () => {
      const result = (await box.mustSh(`sudo sh -c ${shq(zoneBackupScript())}`, "saving the trusted zone")).trim();
      log.note(result === "kept" ? `the trusted zone backup from the first install stays in ${ZONE_BACKUP}` : `saved the trusted zone in ${ZONE_BACKUP}`);
    },
    record: async (name) => void log.save(name, await hostRecord(box)),
    run: async (plan) => waitForRun(box, await startRun(box, log, false, plan, plan.cloudflared), log),
    cleanup: async () => {
      await quietSh(box, `sudo rm -f ${HANDOFF_PEM} ${CIPHERTEXT_FILE}`, "deleting the hand-off key and the ciphertext");
      log.note(`deleted the hand-off key and the ciphertext from ${RUN_DIR} (tmpfs); a new token needs a new handoff-key`);
    },
    note: (line) => log.note(line),
  });
  log.note(`run ${outcome.id}: ${Object.entries(outcome.post).map(([k, v]) => `${k}=${v}`).join(" ")}`);
  const after = await readCloudflaredFacts(box, false);
  log.note(`cloudflared now: ${sourceLine(after.installed, after.fromRepo)}; ${linkReport(after.link, after.marker, await readUpdateUnits(box)).join("; ")}`);
}

async function status(box: Box, log: Log): Promise<void> {
  const read = async (command: string) => (await box.sh(command)).stdout.trim();
  const unit = await read(`systemctl show ${DEMO_TUNNEL_UNIT}.service -p LoadState -p ActiveState -p MainPID -p NRestarts`);
  log.note(`${DEMO_TUNNEL_UNIT}: ${unit.replaceAll("\n", " ")}`);
  const ready = readyConnections(await read(`curl -s --max-time 5 http://127.0.0.1:${DEMO_METRICS_PORT}/ready`));
  log.note(`${DEMO_TUNNEL_UNIT}: ready connections to Cloudflare (127.0.0.1:${DEMO_METRICS_PORT}/ready): ${ready ?? "unknown (nothing answered there)"}`);
  const ops = await read(`systemctl show ${OPS_TUNNEL_UNIT} -p ActiveState -p MainPID -p ActiveEnterTimestamp`);
  const opsPid = /^MainPID=(\d+)$/m.exec(ops)?.[1] ?? "0";
  const opsExe = opsPid === "0" ? "" : await read(`sudo readlink /proc/${opsPid}/exe`);
  log.note(`ops tunnel: ${ops.replaceAll("\n", " ")} exe=${opsExe}${opsExe.endsWith("(deleted)") ? " (runs a replaced binary until its next restart)" : ""}`);
  const facts = await readCloudflaredFacts(box, false);
  log.note(sourceLine(facts.installed, facts.fromRepo));
  for (const line of linkReport(facts.link, facts.marker, await readUpdateUnits(box))) log.note(line);
  log.note(`token and config files: ${(await read(`sudo stat -c '%U:%G %a %n' /etc/credstore ${CREDENTIAL} ${CONFIG} 2>&1`)).replaceAll("\n", "; ")}`);
  for (const dir of [RUN_DIR, STATE_DIR]) log.note(`${dir}: ${(await read(`sudo sh -c 'ls -A ${dir} 2>/dev/null | tr "\\n" " "' || true`)) || "(absent or empty)"}`);
  const runs = runningInstallUnits(await read(`systemctl list-units --all --plain --no-legend '${RUN_UNIT_PREFIX}*'`));
  log.note(`installer runs still there: ${runs.length === 0 ? "none" : runs.join(", ")}`);
  log.note(`demo machine: ${(await read(`sudo sh -c ${shq(auditScript())}`)).replaceAll("\n", "; ")}`);
  log.note(`this workstation: files in ${shownPath(log.dir)} with a tunnel token's prefix: ${filesWith(log.dir, TOKEN_PREFIX).length}`);
  for (const line of await statusAddressLines(box, () => valuesAddress(ROOT))) log.note(line);
}

/** status's lines on the ingress address: only a report, like its other FAIL lines; route-origin is the gate. */
export async function statusAddressLines(box: Box, values: ValuesReader): Promise<string[]> {
  return [
    ...addressReport(await readIngressAddresses(box, values)),
    "(status only reports the ingress addresses; before switching the routes run route-origin, and take the origin from its output)",
  ];
}

/** What check reaches besides the demo machine; tests pass their own. */
export interface CheckDeps {
  /** Runs a probe's curl on this workstation. */
  workstation: (args: string[]) => Promise<RunResult>;
  /** values-demo.yaml's address at the checkout's commit (valuesAddress(ROOT)). */
  values: ValuesReader;
}

const CHECK_DEPS: CheckDeps = { workstation: (args) => tryRun(args), values: () => valuesAddress(ROOT) };

/**
 * The probes, from this workstation and the demo machine. Stage 2 first runs route-origin's gate
 * (ingressGate) and stops before any probe unless it passes, saving `probed: false`; stage 1 does
 * not, its routes are still the connector's own 418.
 */
export async function check(box: Box, log: Log, raw: Record<string, string>, deps: CheckDeps = CHECK_DEPS): Promise<void> {
  const hosts = { app: hostFromEnv("DEMO_APP_HOST"), gateway: hostFromEnv("DEMO_GATEWAY_HOST"), ota: hostFromEnv("DEMO_OTA_HOST") };
  const stage = raw.stage === "2" ? 2 : raw.stage === "1" || raw.stage === undefined ? 1 : undefined;
  if (stage === undefined) throw new ReleaseError("--stage is 1 (routes answer 418) or 2 (routes go to the demo ingress)");
  let ingress: { addresses: IngressAddresses; origin: string } | undefined;
  if (stage === 2) {
    ingress = await ingressGate(box, log, deps.values, "check", { stage, probed: false });
    log.note(`${originLine(ingress.origin)} (the routes that go to the ingress must use exactly this)`);
  }
  const { results, failed } = await runProbes(
    { workstation: deps.workstation, demoMachine: (args) => box.sh(args.map(shq).join(" ")), note: (line) => log.note(line) },
    hosts,
    stage,
  );
  log.save("check", { at: new Date().toISOString(), stage, ...(ingress === undefined ? {} : { addresses: ingress.addresses, origin: ingress.origin }), results });
  if (failed > 0) {
    throw new ReleaseError(
      `${failed} probe(s) did not get the expected answer. challenge or block on the gateway or ota host name: Cloudflare's bot protection stopped a non-browser client (Bot Fight Mode cannot be skipped per host on the Free plan; turning it off for the zone is the user's decision; the record keeps cf-ray). access on a bypass path: a bypass application is missing or does not match; an app path that is not access: Access does not cover that host name. refused on a bypass path, or a path below it that is not refused: the tunnel's path rules are wrong. tunnel-down: no connector is up. In stage 1 the WebSocket upgrade to /mqtt fails with Cloudflare's 502 even when it reached the connector (the route answers 418, not 101): compare the connector's counters before and after it as docs/runbooks/demo-environment.md says (Laysi/trustiot-v2#473 automates this).`,
    );
  }
}

async function readZoneBackup(box: Box): Promise<ZoneBackup | null> {
  const complete = (await box.sh(`sudo test -e ${ZONE_BACKUP}/complete && echo yes`)).stdout.trim() === "yes";
  if (!complete) return null;
  return {
    fileExisted: (await box.sh(`sudo test -e ${ZONE_BACKUP}/trusted.xml && echo yes`)).stdout.trim() === "yes",
    runtime: await box.mustSh(`sudo cat ${ZONE_BACKUP}/runtime.txt`, "reading the trusted zone backup"),
    permanent: await box.mustSh(`sudo cat ${ZONE_BACKUP}/permanent.txt`, "reading the trusted zone backup"),
  };
}

async function readZoneNow(box: Box): Promise<{ runtime: string; permanent: string }> {
  return {
    runtime: await box.mustSh(`sudo ${TRUSTED_ZONE_VIEWS.runtime}`, "reading the trusted zone"),
    permanent: await box.mustSh(`sudo ${TRUSTED_ZONE_VIEWS.permanent}`, "reading the trusted zone"),
  };
}

async function remove(box: Box, log: Log, raw: Record<string, string>): Promise<void> {
  const options = commonOptions(raw);
  const plan = async (): Promise<{ plan: RemovePlan; backup: ZoneBackup | null }> => {
    const unit = (await box.sh(`systemctl show ${DEMO_TUNNEL_UNIT}.service -p LoadState`)).stdout;
    const backup = await readZoneBackup(box);
    return { plan: removePlan(unit, backup, await readZoneNow(box)), backup };
  };
  const command = "bun scripts/demo/tunnel.ts remove --ssh-host <host>";
  if (!options.apply) {
    const shown = await plan();
    log.note(`plan: ${JSON.stringify(shown.plan)}`);
    log.note(`kept: Cloudflare's RPM repository, its key and the cloudflared package from it (the ops tunnel ${OPS_TUNNEL_UNIT} runs that binary), and the firewalld zones other than trusted`);
    checkConfirmation(log, shown.plan, options, command);
    return;
  }
  const release = await takeLock(box, ISSUE, "tunnel-remove", options.lockHeld);
  try {
    const current = await plan();
    checkConfirmation(log, current.plan, options, command);
    log.save("host-before", await hostRecord(box));
    await quietSh(box, `sudo sh -c ${shq(removeScript())}`, "removing the unit");
    log.note(`removed ${DEMO_TUNNEL_UNIT}.service, its token, its config and ${RUN_DIR}`);
    if (typeof current.plan.trustedZone === "object" && current.backup !== null) {
      // Fails on any firewall-cmd error and on a runtime or permanent state that is not the
      // backup's in the two items; the backup then stays for a look by hand.
      await box.mustSh(`sudo sh -c ${shq(restoreTrustedZoneScript(current.plan.trustedZone))}`, "restoring the trusted zone", { retry: false });
      log.note("the trusted zone holds the backup's pod-SSH rule and Service network state again (runtime and permanent)");
      const decision = backupDecision(current.backup, await readZoneNow(box));
      if (decision.delete) {
        await box.mustSh(`sudo rm -rf ${STATE_DIR}`, `deleting ${STATE_DIR}`);
        log.note(`the whole trusted zone equals the backup again (runtime and permanent); deleted ${STATE_DIR} with the backup`);
        if (!current.backup.fileExisted) log.note(`${TRUSTED_ZONE} did not exist before the install; it now holds the same settings as firewalld's defaults and stays`);
      } else {
        log.note(`WARNING: the trusted zone differs from the backup in: ${decision.differences.join(", ")}; compare with ${ZONE_BACKUP} by hand (it stays, with ${STATE_DIR})`);
      }
    }
    log.save("host-after", await hostRecord(box));
  } finally {
    await release();
  }
}

const HOST_ONLY = { "ssh-host": { default: "" }, "log-dir": { default: "" } } as const;
const SUBCOMMANDS = {
  "handoff-key": { spec: HOST_ONLY, run: (box: Box, log: Log) => handoffKey(box, log) },
  install: { spec: { ...COMMON_SPEC, ciphertext: { default: "" }, "tunnel-id": { default: "" } }, run: install },
  status: { spec: HOST_ONLY, run: (box: Box, log: Log) => status(box, log) },
  // Read only, no lab lock; its result is the run's last line (see below).
  "route-origin": { spec: HOST_ONLY, run: (box: Box, log: Log) => routeOrigin(box, log) },
  check: { spec: { ...HOST_ONLY, stage: { default: "1" } }, run: (box: Box, log: Log, raw: Record<string, string>) => check(box, log, raw) },
  remove: { spec: COMMON_SPEC, run: (box: Box, log: Log, raw: Record<string, string>) => remove(box, log, raw) },
} as const;

let log: Log | undefined;

if (import.meta.main) {
  await main(
    () => log,
    async () => {
      const [name, ...argv] = Bun.argv.slice(2);
      const sub = SUBCOMMANDS[name as keyof typeof SUBCOMMANDS];
      if (sub === undefined) throw new ReleaseError(`usage: bun scripts/demo/tunnel.ts <${Object.keys(SUBCOMMANDS).join("|")}> --ssh-host <host> [options]`);
      const { rest, allowed } = name === "install" ? splitAllowChanges(argv) : { rest: argv, allowed: [] };
      const raw = parseOptions(rest, sub.spec);
      const options = commonOptions({ apply: "false", ...raw });
      log = new Log(`tunnel-${name}`, options.logDir);
      const current = log;
      const box = new Box(options.sshHost, current);
      const result: unknown = await withOpsTunnel(
        () => opsTunnel(box),
        current,
        async () => {
          const ops = await opsTunnel(box);
          return sub.run(box, current, raw, ops, allowed);
        },
      );
      // route-origin's origin line comes last, after the ops tunnel's line.
      if (typeof result === "string") current.note(result);
    },
  );
}
