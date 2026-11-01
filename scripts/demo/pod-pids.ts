/**
 * Sets the kubelet's process limit per pod on the demo machine (Laysi/trustiot-v2#881): the
 * dashboard check's node requirement (deploy/chart/README.md, 「儀表板檢查」), podPidsLimit
 * POD_PIDS_LIMIT. SaaS hosts get it from deploy/host's installer; the demo machine's k3s was not
 * installed by it (`--skip-k3s`), so its /etc/rancher/k3s/config.yaml needs `pod-max-pids=16384`
 * in `kubelet-arg` and a k3s restart, before the dashboard check is switched on there.
 *
 *   bun scripts/demo/pod-pids.ts --ssh-host <host>                          # check and plan, read only
 *   bun scripts/demo/pod-pids.ts --ssh-host <host> --apply --confirm <code>
 *   bun scripts/demo/pod-pids.ts --ssh-host <host> --rollback [--apply --confirm <code>]
 *
 * Read only without --apply: the kubelet's running podPidsLimit (its /configz through the API
 * server's node proxy, as scripts/nightly/pod-pids-limit.ts reads it), the config file, the other
 * places k3s reads kubelet arguments or another config file from, and the backup. It prints the
 * lines it would add, the file's sha256 before and after, and a confirmation code. It never prints
 * the file's other lines: a k3s config may hold a token.
 *
 * --apply (scripts/README.md, 「展示機（`demo/`）」): takes the lab lock for this issue, plans again
 * and stops when the plan changed, then
 * 1. backs the file up as config.yaml.pre-881 (kept; an existing backup must be the file as it is
 *    now), writes the new file next to it with the old one's mode, owner and SELinux label, checks
 *    its sha256 and renames it over the old one: a compare-and-swap on the sha256 the plan was
 *    confirmed against, so an edit in between stops the run with nothing changed;
 * 2. restarts k3s (`systemctl restart --no-block`; KillMode=process keeps the pods running, the
 *    API server is gone for tens of seconds) and waits until a new k3s process is active, the API
 *    server is ready and the node is Ready;
 * 3. reads the kubelet's podPidsLimit again and fails unless it is POD_PIDS_LIMIT.
 *
 * It refuses, changing nothing, when the existing `kubelet-arg` would be clobbered or cannot be
 * extended safely: a second `kubelet-arg` key, a `kubelet-arg+` key, a value that is not a block
 * list of plain items, a `pod-max-pids` item with another value; and when k3s reads kubelet
 * arguments or its config from anywhere else (the unit's command line, its environment files,
 * config.yaml.d). The new text is parsed back and must equal the old one with only the new item
 * added to `kubelet-arg`.
 *
 * --rollback puts the backup back and restarts k3s, only while the file is exactly what --apply
 * writes from that backup (nothing edited it since).
 *
 * Pods keep the limit they were created with: a dashboard check that already runs needs
 * `kubectl rollout restart` of its Deployment afterwards; the run lists such Deployments.
 */

import { ReleaseError } from "../release/lib.ts";
import { POD_PIDS_LIMIT } from "../nightly/dashboard-check.ts";
import { podPidsLimitOf } from "../nightly/pod-pids-limit.ts";
import { parseOptions } from "../release/exec.ts";
import {
  Box,
  COMMON_SPEC,
  type CommonOptions,
  Log,
  type OpsTunnel,
  checkConfirmation,
  commonOptions,
  lastLines,
  main,
  opsTunnel,
  sha256,
  shownPath,
  stableJson,
  takeLock,
  withOpsTunnel,
} from "./lib.ts";

export const ISSUE = "Laysi/trustiot-v2#881";
export const CONFIG_DIR = "/etc/rancher/k3s";
export const CONFIG_FILE = `${CONFIG_DIR}/config.yaml`;
/** The backup --apply keeps (`<file>.pre-<issue>`). */
export const BACKUP_FILE = `${CONFIG_FILE}.pre-881`;
export const KUBELET_ARG = `pod-max-pids=${POD_PIDS_LIMIT}`;
/**
 * The kubelet's podPidsLimit without pod-max-pids (KubeletConfiguration's default, no limit): what
 * --rollback expects back, since --apply only ever adds the item to a file that had none.
 */
export const KUBELET_DEFAULT_POD_PIDS_LIMIT = -1;
/** The block --apply appends when the file has no kubelet-arg yet. */
export const NEW_BLOCK = `kubelet-arg:\n  - ${KUBELET_ARG}\n`;
/** Upper bound for k3s to come back after the restart: a new process, the API server ready, the node Ready. */
export const RESTART_WAIT_MS = 600_000;
const POLL_MS = 5_000;

// --- The edit ---------------------------------------------------------------------------------

/** What happens to config.yaml: nothing (already there), an edit, or a refusal. */
export type ConfigEdit =
  | { kind: "present" }
  | { kind: "edit"; text: string; added: { line: number; text: string }[] }
  | { kind: "refuse"; reason: string };

type Yaml = Record<string, unknown>;

function parseYaml(text: string): { ok: true; value: Yaml } | { ok: false; reason: string } {
  let parsed: unknown;
  try {
    parsed = text.trim() === "" ? {} : Bun.YAML.parse(text);
  } catch (error) {
    return { ok: false, reason: `it is not valid YAML (${error instanceof Error ? error.message.split("\n")[0] : String(error)})` };
  }
  if (parsed === null || parsed === undefined) return { ok: true, value: {} };
  if (typeof parsed !== "object" || Array.isArray(parsed)) return { ok: false, reason: "it is not one YAML mapping" };
  return { ok: true, value: parsed as Yaml };
}

/** A top-level key line: the key at column 0, then a colon. */
const TOP_KEY = /^("?)(kubelet-arg\+?)\1\s*:(.*)$/;
const ITEM = /^(\s+)-\s+(\S.*?)\s*$/;

/**
 * config.yaml with KUBELET_ARG in its kubelet-arg list, by text, so comments and every other line
 * stay byte for byte. Only two shapes are edited: no kubelet-arg at all (NEW_BLOCK is appended),
 * or one `kubelet-arg:` key whose value is a block list of items (one item is added after the
 * last). The result is parsed back and must be the old mapping with only that item added.
 */
export function planConfigEdit(text: string): ConfigEdit {
  const before = parseYaml(text);
  if (!before.ok) return { kind: "refuse", reason: `config.yaml: ${before.reason}` };
  if (text.includes("\r")) return { kind: "refuse", reason: "config.yaml has CR line endings" };
  const lines = text.split("\n");
  const keyLines = lines.flatMap((line, index) => (TOP_KEY.test(line) ? [index] : []));
  if (keyLines.some((index) => TOP_KEY.exec(lines[index]!)?.[2] === "kubelet-arg+") || "kubelet-arg+" in before.value) {
    return { kind: "refuse", reason: "config.yaml has a kubelet-arg+ key; add the item by hand" };
  }
  if (keyLines.length > 1) {
    return { kind: "refuse", reason: `config.yaml has ${keyLines.length} kubelet-arg keys (lines ${keyLines.map((i) => i + 1).join(", ")}): YAML keeps only one; merge them by hand first` };
  }
  const current = before.value["kubelet-arg"];
  let next: string;
  let added: { line: number; text: string }[];
  if (keyLines.length === 0) {
    if (current !== undefined) return { kind: "refuse", reason: "config.yaml sets kubelet-arg in a form this script does not edit (not a plain top-level key); add the item by hand" };
    const base = text === "" || text.endsWith("\n") ? text : `${text}\n`;
    next = `${base}${NEW_BLOCK}`;
    const first = base.split("\n").length;
    added = NEW_BLOCK.trimEnd()
      .split("\n")
      .map((line, offset) => ({ line: first + offset, text: line }));
  } else {
    if (!Array.isArray(current) || current.some((item) => typeof item !== "string")) {
      return { kind: "refuse", reason: "config.yaml's kubelet-arg is not a list of strings; add the item by hand" };
    }
    const pids = (current as string[]).filter((item) => item.startsWith("pod-max-pids="));
    if (pids.length > 0) {
      if (pids.length === 1 && pids[0] === KUBELET_ARG) return { kind: "present" };
      return { kind: "refuse", reason: `config.yaml's kubelet-arg already has ${pids.join(", ")}: the script does not replace another value; settle it by hand` };
    }
    const keyIndex = keyLines[0]!;
    const rest = TOP_KEY.exec(lines[keyIndex]!)?.[3] ?? "";
    if (rest.replace(/\s+#.*$/, "").trim() !== "") {
      return { kind: "refuse", reason: `config.yaml's kubelet-arg (line ${keyIndex + 1}) is not a block list (it continues on the key's line); add the item by hand` };
    }
    // The items follow the key directly, one per line, at one indentation.
    let last = keyIndex;
    let indent: string | undefined;
    for (let index = keyIndex + 1; index < lines.length; index += 1) {
      const line = lines[index]!;
      const item = ITEM.exec(line);
      if (item === null) break;
      if (indent !== undefined && item[1] !== indent) return { kind: "refuse", reason: `config.yaml's kubelet-arg items are not at one indentation (line ${index + 1})` };
      indent = item[1]!;
      last = index;
    }
    if (indent === undefined || last - keyIndex !== current.length) {
      return { kind: "refuse", reason: "config.yaml's kubelet-arg is not a block list of one item per line right after the key; add the item by hand" };
    }
    const after = lines[last + 1];
    if (after !== undefined && after !== "" && /^\s/.test(after)) {
      return { kind: "refuse", reason: `config.yaml's kubelet-arg list continues in a form this script does not edit (line ${last + 2})` };
    }
    const line = `${indent}- ${KUBELET_ARG}`;
    next = [...lines.slice(0, last + 1), line, ...lines.slice(last + 1)].join("\n");
    added = [{ line: last + 2, text: line }];
  }
  const parsed = parseYaml(next);
  const expected = { ...before.value, "kubelet-arg": [...((current as string[] | undefined) ?? []), KUBELET_ARG] };
  if (!parsed.ok || stableJson(parsed.value) !== stableJson(expected)) {
    return { kind: "refuse", reason: "the edited config.yaml does not parse to the old one plus the new item; add the item by hand" };
  }
  return { kind: "edit", text: next, added };
}

// --- Facts and plan ---------------------------------------------------------------------------

export interface HostFacts {
  /** config.yaml's bytes, or undefined when there is none. */
  config: string | undefined;
  /** Files in config.yaml.d that mention kubelet-arg. */
  configD: string[];
  /** The k3s unit (with drop-ins) names kubelet-arg, --config or K3S_CONFIG_FILE. */
  unitMentions: string[];
  /** k3s's environment files that name kubelet-arg, --config or K3S_CONFIG_FILE. */
  envFiles: string[];
  /** sha256 of the backup, or undefined when there is none. */
  backupSha256: string | undefined;
  /** The backup's bytes, read only for --rollback. */
  backup?: string | undefined;
  k3s: { active: string; pid: string };
  /** The directory, the file or the backup is a symbolic link. */
  symlink?: boolean;
}

export interface Facts {
  host: HostFacts;
  /** The kubelet's running podPidsLimit per node (undefined: not readable). */
  limits: Record<string, number | undefined>;
  /** Deployments with the dashboard check's component label: they keep their old limit until restarted. */
  dashboardChecks: string[];
}

export interface Plan {
  mode: "apply" | "rollback";
  /** Lines for the operator: what is there now. */
  ok: string[];
  blockers: string[];
  /** The file change: sha256 before and after, and the added lines (apply) or the restored file (rollback). */
  write?: { fromSha256: string; toSha256: string; text: string; added: { line: number; text: string }[] };
  restart: boolean;
}

/** What the confirmation code covers: the file change and the restart, not the advice lines. */
export function changeOf(plan: Plan): unknown {
  return { mode: plan.mode, write: plan.write === undefined ? null : { from: plan.write.fromSha256, to: plan.write.toSha256 }, restart: plan.restart };
}

function elsewhere(host: HostFacts): string[] {
  const blockers: string[] = [];
  if (host.symlink === true) blockers.push(`${CONFIG_DIR}, its config.yaml or the backup is a symbolic link: this script writes only regular files; settle it by hand`);
  if (host.configD.length > 0) blockers.push(`${CONFIG_DIR}/config.yaml.d sets kubelet-arg too (${host.configD.join(", ")}): k3s would merge or override it; settle it by hand`);
  if (host.unitMentions.length > 0) blockers.push(`the k3s unit names ${host.unitMentions.join(", ")}: its command line would win over config.yaml; settle it by hand`);
  if (host.envFiles.length > 0) blockers.push(`k3s's environment files name kubelet-arg or another config (${host.envFiles.join(", ")}); settle it by hand`);
  return blockers;
}

/**
 * The limits without the node names: the demo machine's node is named after the host, which public
 * text does not name (docs/architecture/secrets.md, 「公開 repo 的資訊規則」), and what the run
 * prints goes into test reports.
 */
export function limitsLine(limits: Record<string, number | undefined>): string {
  const values = Object.values(limits);
  if (values.length === 0) return "no node could be read";
  return values.map((limit, index) => `${values.length === 1 ? "the node" : `node ${index + 1}`}: podPidsLimit ${limit ?? "unreadable"}`).join("; ");
}

export function planPodPids(facts: Facts): Plan {
  const plan: Plan = { mode: "apply", ok: [], blockers: [], restart: false };
  const nodes = Object.keys(facts.limits);
  if (nodes.length !== 1) plan.blockers.push(`the cluster has ${nodes.length} nodes; this script edits the demo machine's own k3s and expects it to be the only node`);
  const atLimit = nodes.length > 0 && Object.values(facts.limits).every((limit) => limit === POD_PIDS_LIMIT);
  plan.ok.push(`kubelet now: ${limitsLine(facts.limits)}`);
  if (Object.values(facts.limits).some((limit) => limit === undefined)) {
    plan.blockers.push("the kubelet's configuration could not be read (its /configz through the API server): without it the change cannot be checked afterwards");
  }
  if (facts.host.k3s.active !== "active") plan.blockers.push(`k3s.service is ${facts.host.k3s.active || "unknown"}, not active`);
  plan.blockers.push(...elsewhere(facts.host));
  if (facts.host.config === undefined) {
    plan.blockers.push(`${CONFIG_FILE} does not exist: this is not the demo machine's k3s as the runbook describes it`);
    return plan;
  }
  const edit = planConfigEdit(facts.host.config);
  const fromSha256 = sha256(facts.host.config);
  plan.ok.push(`${CONFIG_FILE}: sha256 ${fromSha256}`);
  if (edit.kind === "refuse") {
    plan.blockers.push(edit.reason);
    return plan;
  }
  if (edit.kind === "present") plan.ok.push(`${CONFIG_FILE} already has ${KUBELET_ARG} in kubelet-arg`);
  if (atLimit) {
    if (edit.kind === "edit") plan.ok.push(`the kubelet already runs with podPidsLimit ${POD_PIDS_LIMIT} although ${CONFIG_FILE} does not set it; nothing to change`);
    return plan;
  }
  if (edit.kind === "edit") {
    if (facts.host.backupSha256 !== undefined && facts.host.backupSha256 !== fromSha256) {
      plan.blockers.push(`${BACKUP_FILE} exists and is not the file as it is now (sha256 ${facts.host.backupSha256}); look at it and move it away first`);
    }
    plan.write = { fromSha256, toSha256: sha256(edit.text), text: edit.text, added: edit.added };
  }
  plan.restart = true;
  return plan;
}

/** --rollback: the backup back, only while the file is exactly what --apply wrote from it. */
export function planRollback(facts: Facts): Plan {
  const plan: Plan = { mode: "rollback", ok: [`kubelet now: ${limitsLine(facts.limits)}`], blockers: [], restart: false };
  const { config, backup } = facts.host;
  if (facts.dashboardChecks.length > 0) {
    plan.blockers.push(`the dashboard check runs (${facts.dashboardChecks.join(", ")}): switch it off before taking its node requirement away`);
  }
  if (backup === undefined) {
    plan.blockers.push(`${BACKUP_FILE} does not exist: --apply never changed this file`);
    return plan;
  }
  if (config === undefined) {
    plan.blockers.push(`${CONFIG_FILE} does not exist`);
    return plan;
  }
  if (config === backup) {
    plan.ok.push(`${CONFIG_FILE} is already the backup`);
    if (Object.values(facts.limits).some((limit) => limit !== KUBELET_DEFAULT_POD_PIDS_LIMIT)) {
      plan.ok.push(`the kubelet does not run with its default podPidsLimit ${KUBELET_DEFAULT_POD_PIDS_LIMIT} yet: k3s restarts to read the restored file`);
      plan.blockers.push(...elsewhere(facts.host));
      plan.restart = true;
    }
    return plan;
  }
  const edit = planConfigEdit(backup);
  if (edit.kind !== "edit" || edit.text !== config) {
    plan.blockers.push(`${CONFIG_FILE} is not what --apply writes from ${BACKUP_FILE} (it changed since): restoring would drop those changes; settle it by hand`);
    return plan;
  }
  plan.blockers.push(...elsewhere(facts.host));
  plan.write = { fromSha256: sha256(config), toSha256: sha256(backup), text: backup, added: [] };
  plan.restart = true;
  return plan;
}

// --- Operations -------------------------------------------------------------------------------

export interface PodPidsOps {
  tunnel(): Promise<OpsTunnel>;
  facts(withBackup: boolean): Promise<Facts>;
  /** k3s's state, its API server's readiness and the node's Ready condition; never throws. */
  health(): Promise<{ active: string; pid: string; apiReady: boolean; nodeReady: boolean }>;
  lock(step: string): Promise<() => Promise<void>>;
  /** The compare-and-swap of the file (the remote script of writeScript); returns its last line. */
  write(plan: NonNullable<Plan["write"]>, backup: boolean): Promise<string>;
  restartK3s(): Promise<void>;
  now(): number;
  sleep(ms: number): Promise<void>;
}

function report(log: Log, plan: Plan): void {
  for (const line of plan.ok) log.note(`ok: ${line}`);
  for (const line of plan.blockers) log.note(`BLOCKED (--apply refuses until it is gone): ${line}`);
  if (plan.write !== undefined) {
    log.note(`change: ${CONFIG_FILE}, sha256 ${plan.write.fromSha256} -> ${plan.write.toSha256}`);
    if (plan.mode === "apply") {
      log.note(`  backup first: ${BACKUP_FILE} (kept)`);
      for (const line of plan.write.added) log.note(`  + line ${line.line}: ${line.text}`);
    } else {
      log.note(`  restore: ${CONFIG_FILE} becomes ${BACKUP_FILE} byte for byte`);
    }
  }
  if (plan.restart) log.note("change: restart k3s (systemctl restart --no-block k3s.service): running pods keep running, the API server is unavailable for tens of seconds");
}

async function waitForK3s(ops: PodPidsOps, log: Log, oldPid: string): Promise<void> {
  const deadline = ops.now() + RESTART_WAIT_MS;
  let last = "";
  for (;;) {
    const health = await ops.health();
    last = `k3s ${health.active} pid ${health.pid === oldPid ? "unchanged" : "new"}, API server ${health.apiReady ? "ready" : "not ready"}, node ${health.nodeReady ? "Ready" : "not Ready"}`;
    if (health.active === "active" && health.pid !== oldPid && health.pid !== "0" && health.pid !== "" && health.apiReady && health.nodeReady) break;
    if (ops.now() > deadline) throw new ReleaseError(`timed out after ${RESTART_WAIT_MS / 1000} s waiting for k3s to come back (${last}); see journalctl -u k3s on the demo machine`);
    await ops.sleep(POLL_MS);
  }
  log.note(`k3s is back: ${last}`);
}

/** Polls the kubelet's limit until it reads `want` (the first reads after a restart may fail). */
async function waitForLimit(ops: PodPidsOps, want: (limit: number | undefined) => boolean): Promise<Record<string, number | undefined>> {
  const deadline = ops.now() + RESTART_WAIT_MS;
  for (;;) {
    // Right after the restart a read can still fail; that is waited out like a wrong value.
    let limits: Record<string, number | undefined> = {};
    try {
      ({ limits } = await ops.facts(false));
    } catch {
      limits = {};
    }
    if (Object.keys(limits).length > 0 && Object.values(limits).every(want)) return limits;
    if (ops.now() > deadline) return limits;
    await ops.sleep(POLL_MS);
  }
}

export async function runPodPids(ops: PodPidsOps, log: Log, options: CommonOptions, rollback: boolean): Promise<Plan> {
  return withOpsTunnel(
    () => ops.tunnel(),
    log,
    async () => {
      const planOf = (facts: Facts) => (rollback ? planRollback(facts) : planPodPids(facts));
      const facts = await ops.facts(rollback);
      const plan = planOf(facts);
      report(log, plan);
      log.save("plan", { ...plan, write: plan.write === undefined ? undefined : { ...plan.write, text: undefined } });
      if (!rollback && facts.dashboardChecks.length > 0) {
        log.note(`the dashboard check already runs (${facts.dashboardChecks.join(", ")}): after the change, restart it so its pod gets the limit: sudo k3s kubectl -n <namespace> rollout restart deployment/<name>`);
      }
      if (plan.write === undefined && !plan.restart) {
        log.note(plan.blockers.length > 0 ? `${plan.blockers.length} blockers above; nothing to change until they are settled` : "nothing to change");
        if (plan.blockers.length > 0 && options.apply) throw new ReleaseError(`${plan.blockers.length} blockers above; nothing changed`);
        return plan;
      }
      if (options.apply && plan.blockers.length > 0) throw new ReleaseError(`${plan.blockers.length} blockers above; nothing changed`);
      const command = `bun scripts/demo/pod-pids.ts --ssh-host <host>${rollback ? " --rollback" : ""}`;
      if (!checkConfirmation(log, changeOf(plan), options, command)) return plan;

      const release = await ops.lock(rollback ? "pod-pids-rollback" : "pod-pids");
      try {
        const again = planOf(await ops.facts(rollback));
        if (stableJson(changeOf(again)) !== stableJson(changeOf(plan))) {
          throw new ReleaseError("the file, the kubelet or k3s changed since the plan was made; nothing changed, run again without --apply");
        }
        if (again.blockers.length > 0) throw new ReleaseError(`blocked: ${again.blockers.join("; ")}; nothing changed`);
        const before = await ops.health();
        if (before.active !== "active" || before.pid === "" || before.pid === "0") {
          throw new ReleaseError(`k3s.service is ${before.active || "unreadable"} (pid ${before.pid || "unknown"}) under the lock; nothing changed`);
        }
        const oldPid = before.pid;
        if (plan.write !== undefined) {
          const result = await ops.write(plan.write, !rollback);
          log.note(`${CONFIG_FILE}: ${result} (sha256 ${plan.write.toSha256})`);
        }
        log.note("restarting k3s");
        await ops.restartK3s();
        await waitForK3s(ops, log, oldPid);
        const want = rollback ? KUBELET_DEFAULT_POD_PIDS_LIMIT : POD_PIDS_LIMIT;
        const limits = await waitForLimit(ops, (limit) => limit === want);
        log.note(`kubelet after: ${limitsLine(limits)}`);
        const done = Object.keys(limits).length > 0 && Object.values(limits).every((limit) => limit === want);
        if (!done) {
          throw new ReleaseError(
            rollback
              ? `the kubelet does not run with its default podPidsLimit ${KUBELET_DEFAULT_POD_PIDS_LIMIT} after the restart`
              : `the kubelet does not run with podPidsLimit ${POD_PIDS_LIMIT} after the restart; the file is changed and ${BACKUP_FILE} holds the old one: see journalctl -u k3s, and undo with --rollback`,
          );
        }
        log.note(rollback ? `${CONFIG_FILE} is back to ${BACKUP_FILE}` : `every node runs pods under podPidsLimit ${POD_PIDS_LIMIT}`);
      } finally {
        await release();
      }
      log.note(`done; records in ${shownPath(log.dir)}`);
      return plan;
    },
  );
}

// --- On the demo machine ----------------------------------------------------------------------

/** One `key=value` line of factsScript's output. */
function field(out: string, key: string): string[] {
  return out
    .split("\n")
    .filter((line) => line.startsWith(`${key}=`))
    .map((line) => line.slice(key.length + 1));
}

/**
 * The read-only script facts() runs as root: the file in base64 (it is never printed), and only
 * names for everything else. `dir` is CONFIG_DIR; tests pass a temporary one.
 */
export function factsScript(dir: string, withBackup: boolean): string {
  return `set -euo pipefail
d=${dir}
f="$d/config.yaml"
b="$f.pre-881"
if [ -L "$d" ] || [ -L "$f" ] || [ -L "$b" ]; then echo "symlink=yes"; fi
if [ -f "$f" ]; then echo "config=$(base64 -w0 < "$f")"; else echo "config=ABSENT"; fi
if [ -d "$d/config.yaml.d" ]; then
  for g in "$d"/config.yaml.d/*; do
    if [ -f "$g" ] && grep -q -- 'kubelet-arg' "$g"; then echo "configd=$(basename "$g")"; fi
  done
fi
unit=$(systemctl cat k3s.service 2>/dev/null || true)
for word in kubelet-arg --config K3S_CONFIG_FILE; do
  if printf '%s\\n' "$unit" | grep -v '^#' | grep -q -- "$word"; then echo "unit=$word"; fi
done
for e in /etc/default/k3s /etc/sysconfig/k3s /etc/systemd/system/k3s.service.env; do
  if [ -f "$e" ] && grep -q -E -- 'kubelet-arg|KUBELET_ARG|--config|K3S_CONFIG_FILE' "$e"; then echo "env=$e"; fi
done
if [ -f "$b" ]; then
  echo "backup=$(sha256sum < "$b" | cut -d' ' -f1)"
  ${withBackup ? `echo "backupbody=$(base64 -w0 < "$b")"` : ":"}
else
  echo "backup=ABSENT"
fi
echo "k3s=$(systemctl show k3s.service -p ActiveState --value) $(systemctl show k3s.service -p MainPID --value)"
echo END
`;
}

export function parseHostFacts(out: string): HostFacts {
  if (!out.split("\n").includes("END")) throw new ReleaseError("reading the k3s configuration on the demo machine stopped early");
  const decode = (value: string | undefined): string | undefined => (value === undefined || value === "ABSENT" ? undefined : Buffer.from(value, "base64").toString("utf8"));
  const [active = "", pid = ""] = (field(out, "k3s")[0] ?? "").split(" ");
  const backupSha = field(out, "backup")[0];
  return {
    config: decode(field(out, "config")[0]),
    configD: field(out, "configd"),
    unitMentions: field(out, "unit"),
    envFiles: field(out, "env"),
    backupSha256: backupSha === undefined || backupSha === "ABSENT" ? undefined : backupSha,
    backup: decode(field(out, "backupbody")[0]),
    k3s: { active, pid },
    symlink: field(out, "symlink").length > 0,
  };
}

/**
 * The compare-and-swap that writes config.yaml, run as root with this text on stdin. The new
 * bytes are in the script (stdin), never on a command line. A copy of the old file (cp -a keeps
 * its mode, owner and SELinux label) takes the new bytes, its sha256 is checked, and a rename puts
 * it in place. Exit 3: nothing changed because the file or the backup is not what the plan saw.
 */
export function writeScript(dir: string, write: NonNullable<Plan["write"]>, backup: boolean): string {
  const body = Buffer.from(write.text, "utf8").toString("base64");
  return `set -euo pipefail
d=${dir}
f="$d/config.yaml"
b="$f.pre-881"
n="$f.new-881"
sum() { sha256sum < "$1" | cut -d' ' -f1; }
# Root writes here: a symbolic link would redirect the write to its target.
for p in "$d" "$f" "$b" "$n"; do
  if [ -L "$p" ]; then echo "$p is a symbolic link; nothing changed" >&2; exit 3; fi
done
if [ ! -f "$f" ]; then echo "$f is not a regular file; nothing changed" >&2; exit 3; fi
now=$(sum "$f")
if [ "$now" = ${write.toSha256} ]; then echo "already written"; exit 0; fi
if [ "$now" != ${write.fromSha256} ]; then echo "config.yaml changed since the plan (sha256 $now); nothing changed" >&2; exit 3; fi
${
  backup
    ? `if [ -e "$b" ]; then
  if [ "$(sum "$b")" != ${write.fromSha256} ]; then echo "$b exists and is not the file as it is now; nothing changed" >&2; exit 3; fi
else
  cp -a "$f" "$b"
  if [ "$(sum "$b")" != ${write.fromSha256} ]; then rm -f "$b"; echo "the backup does not match; nothing changed" >&2; exit 3; fi
fi`
    : `if [ ! -f "$b" ] || [ "$(sum "$b")" != ${write.toSha256} ]; then echo "$b is not the file the plan restores; nothing changed" >&2; exit 3; fi`
}
rm -f "$n"
trap 'rm -f "$n"' EXIT
cp -a "$f" "$n"
printf '%s' '${body}' | base64 -d > "$n"
if [ "$(sum "$n")" != ${write.toSha256} ]; then echo "the new file does not have the planned content; nothing changed" >&2; exit 3; fi
sync "$n"
# Again right before the rename: an edit while the copy was made must not be overwritten.
if [ "$(sum "$f")" != ${write.fromSha256} ]; then echo "config.yaml changed while the new file was prepared; nothing changed" >&2; exit 3; fi
mv -f "$n" "$f"
trap - EXIT
if [ "$(sum "$f")" != ${write.toSha256} ]; then echo "config.yaml does not read back as written" >&2; exit 4; fi
echo "written"
`;
}

const DASHBOARD_CHECK_SELECTOR = "app.kubernetes.io/component=dashboard-check";

async function boxFacts(box: Box, withBackup: boolean): Promise<Facts> {
  const host = parseHostFacts(await box.mustSh("sudo bash -s", "reading the k3s configuration", { stdin: factsScript(CONFIG_DIR, withBackup) }));
  const limits: Record<string, number | undefined> = {};
  const nodes = await box.tryKubectl(["get", "nodes", "-o", "jsonpath={.items[*].metadata.name}"]);
  if (nodes.exitCode === 0) {
    for (const node of nodes.stdout.trim().split(/\s+/).filter((name) => /^[a-z0-9]([-a-z0-9.]*[a-z0-9])?$/.test(name))) {
      const configz = await box.tryKubectl(["get", "--raw", `/api/v1/nodes/${node}/proxy/configz`]);
      let limit: number | undefined;
      try {
        limit = configz.exitCode === 0 ? podPidsLimitOf(JSON.parse(configz.stdout) as unknown) : undefined;
      } catch {
        limit = undefined;
      }
      limits[node] = limit;
    }
  }
  // Not read means not known: a failure stops the run rather than counting as "no dashboard check".
  const checks = await box.kubectl(["get", "deployments", "-A", "-l", DASHBOARD_CHECK_SELECTOR, "-o", 'jsonpath={range .items[*]}{.metadata.namespace}/{.metadata.name}{"\\n"}{end}']);
  const dashboardChecks = checks.split("\n").filter((line) => line !== "");
  return { host, limits, dashboardChecks };
}

export function boxOps(box: Box, options: CommonOptions): PodPidsOps {
  return {
    tunnel: () => opsTunnel(box),
    facts: (withBackup) => boxFacts(box, withBackup),
    async health() {
      const state = await box.sh("systemctl show k3s.service -p ActiveState --value; systemctl show k3s.service -p MainPID --value");
      const [active = "", pid = ""] = state.exitCode === 0 ? state.stdout.trim().split("\n") : [];
      const ready = await box.tryKubectl(["get", "--raw", "/readyz"]);
      const nodes = await box.tryKubectl(["get", "nodes", "-o", 'jsonpath={range .items[*]}{.status.conditions[?(@.type=="Ready")].status}{"\\n"}{end}']);
      const statuses = nodes.exitCode === 0 ? nodes.stdout.split("\n").filter((line) => line !== "") : [];
      return { active, pid, apiReady: ready.exitCode === 0 && ready.stdout.trim() === "ok", nodeReady: statuses.length > 0 && statuses.every((s) => s === "True") };
    },
    lock: (step) => takeLock(box, ISSUE, step, options.lockHeld),
    async write(write, backup) {
      // Not repeated when ssh drops: the script is idempotent, but a drop leaves the outcome to the next dry run.
      const result = await box.sh("sudo bash -s", { stdin: writeScript(CONFIG_DIR, write, backup), retry: false });
      if (result.exitCode !== 0) {
        throw new ReleaseError(
          `writing ${CONFIG_FILE} failed (exit ${result.exitCode}): ${lastLines(result.stderr || result.stdout)}` +
            (result.exitCode === 255 ? "; ssh dropped: run again without --apply to see whether the file changed (k3s was not restarted)" : ""),
        );
      }
      return result.stdout.trim().split("\n").pop() ?? "";
    },
    async restartK3s() {
      const result = await box.sh("sudo systemctl restart --no-block k3s.service", { retry: false });
      if (result.exitCode !== 0) throw new ReleaseError(`systemctl restart k3s failed (exit ${result.exitCode}): ${lastLines(result.stderr || result.stdout)}`);
    },
    now: () => Date.now(),
    sleep: (ms) => Bun.sleep(ms),
  };
}

if (import.meta.main) {
  let log: Log | undefined;
  await main(
    () => log,
    async () => {
      const raw = parseOptions(Bun.argv.slice(2), { ...COMMON_SPEC, rollback: { flag: true } });
      const options = commonOptions(raw);
      const rollback = raw.rollback === "true";
      log = new Log(rollback ? "pod-pids-rollback" : "pod-pids", options.logDir);
      await runPodPids(boxOps(new Box(options.sshHost, log), options), log, options, rollback);
    },
  );
}
