// The boot test's results (README.md, section 「開機測試」): the sections boot-test/probe.sh writes
// inside the virtual machine, and what each boot must show. Pure functions, so the unit tests
// cover them without a virtual machine.
import { deviceUnit, luksHeaderProblems, markToken, ownerToken, parseEnrollList } from "./data.ts";
import { DATA, DATA_CHECK, LUKS } from "./pins.ts";

export interface ProbeSection {
  output: string;
  exit: number;
}

/** The probe's `=== name` ... `=== exit N` sections; complete when the closing `=== end` is there. */
export function parseProbe(text: string): { sections: Map<string, ProbeSection>; complete: boolean } {
  const sections = new Map<string, ProbeSection>();
  let name: string | null = null;
  let lines: string[] = [];
  let complete = false;
  for (const line of text.replace(/\0+$/, "").split("\n")) {
    const exit = /^=== exit (\d+)$/.exec(line);
    if (name !== null && exit) {
      sections.set(name, { output: lines.join("\n"), exit: Number(exit[1]) });
      name = null;
      lines = [];
    } else if (name === null && line === "=== end") {
      complete = true;
      break;
    } else if (name === null && line.startsWith("=== ")) {
      name = line.slice(4);
    } else if (name !== null) {
      lines.push(line);
    }
  }
  return { sections, complete };
}

/** `systemctl show --property=... <unit>...`: one block per unit, keyed by Id. */
export function parseShow(text: string): Map<string, Record<string, string>> {
  const units = new Map<string, Record<string, string>>();
  for (const block of text.split(/\n\s*\n/)) {
    const props: Record<string, string> = {};
    for (const line of block.split("\n")) {
      const eq = line.indexOf("=");
      if (eq > 0) props[line.slice(0, eq)] = line.slice(eq + 1);
    }
    if (props.Id) units.set(props.Id, props);
  }
  return units;
}

export const UNITS = {
  setup: "trustiot-data-setup.service",
  unlock: `systemd-cryptsetup@${LUKS.mapping}.service`,
  mount: "var-lib-trustiot\\x2dagent.mount",
  agent: "trustiot-agent.service",
  ssh: "ssh.service",
} as const;

/** The devices whose job time limit is DATA.deviceTimeout: the volume (crypttab) and the unlocked volume (fstab). */
export const DEVICES = [deviceUnit(`/dev/disk/by-uuid/${LUKS.uuid}`), deviceUnit(`/dev/mapper/${LUKS.mapping}`)] as const;

/** A unit's monotonic timestamp property from the probe's units section, in seconds since boot (0 when never). */
export function unitSeconds(probeText: string, unit: string, property: string): number {
  const output = parseProbe(probeText).sections.get("units")?.output ?? "";
  return Number(parseShow(output).get(unit)?.[property] ?? "0") / 1e6;
}

/**
 * The probe output with the TPM-sealed key blob and the TPM's public key left out of the LUKS2
 * header, for reports: neither opens anything without that TPM, but they are this volume's and
 * have no use outside it.
 */
export function redactTpmFields(text: string): string {
  return text.replace(/("(?:tpm2-blob|tpm2_srk)"\s*:\s*)"[^"]*"/g, '$1"(left out)"');
}

export interface BootExpectation {
  /** unlocked: the TPM opened the volume and it is mounted; locked: it stayed locked (fail closed). */
  volume: "unlocked" | "locked";
  /** What the setup unit must end as: success, failed (refused), or either. */
  setup: "success" | "failed";
  /** A line the setup must have logged this boot (first boot, already set up, refusing ...). */
  setupLogs?: string;
  /** The marker this boot writes on the volume, and the one it must read back. */
  writeMarker?: string;
  readMarker?: string;
  /** The volume holds no marker when the probe looks: it was made anew in this boot (a wipe). */
  markerGone?: boolean;
  /**
   * The probe started the agent by hand after the boot (trustiot_test.start-agent=1), once as
   * anyone would and once skipping its dependencies: neither may run it (locked boots only).
   */
  startAgent?: boolean;
  /**
   * The volume carries the mark of a wipe (Laysi/trustiot-v2#867): the header has the mark's
   * token, the setup left DATA_CHECK.factoryFlag, and the agent's condition kept it off (unlocked
   * boots only). Without it, neither the token nor the flag may be there.
   */
  mark?: boolean;
  /** What the setup's looks at a TPM that did not open the volume at once must show (tpmLookProblems). */
  tpmLooks?: TpmLooks;
}

/**
 * How the setup's looks at the TPM went (its "the TPM (look N, S s): answer" lines; README.md,
 * section 「TPM 永久解不開時」).
 */
export interface TpmLooks {
  /**
   * gave-up: no look opened the volume or found the same other owner hierarchy often enough, the
   * looks went on for the whole window and the setup did not wipe; opened-late: the TPM opened
   * the volume after one or more looks that did not; permanent: the last DATA_CHECK.sameAnswers
   * looks found the same other owner hierarchy and the setup wiped the volume.
   */
  outcome: "gave-up" | "opened-late" | "permanent";
  /** The answers the looks before the deciding ones may give (all of them for gave-up). */
  answers: readonly string[];
}

export interface TpmLook {
  look: number;
  /** Seconds since the first look, when this one started (whole seconds of the uptime). */
  at: number;
  answer: string;
}

/** The setup's looks at the TPM, from its journal. */
export function parseTpmLooks(journal: string): TpmLook[] {
  return [...journal.matchAll(/the TPM \(look (\d+), (\d+) s\): (.+)$/gm)].map((m) => ({ look: Number(m[1]), at: Number(m[2]), answer: (m[3] ?? "").trim() }));
}

/** Why the setup's looks at the TPM (its journal this boot) did not go as `expect` says, if they did not. */
export function tpmLookProblems(journal: string, expect: TpmLooks): string[] {
  const looks = parseTpmLooks(journal);
  if (looks.length === 0) return ["the setup logged no look at the TPM"];
  const problems: string[] = [];
  const { intervalS, windowS, sameAnswers } = DATA_CHECK;
  looks.forEach((l, i) => {
    if (l.look !== i + 1) problems.push(`look ${i + 1} is logged as look ${l.look}`);
    const before = looks[i - 1];
    if (before && l.at - before.at < intervalS) problems.push(`looks ${before.look} and ${l.look} are ${l.at - before.at} s apart, expected at least ${intervalS} s`);
  });
  const last = looks.at(-1) as TpmLook;
  if (last.at > windowS) problems.push(`the last look started ${last.at} s after the first, after the window of ${windowS} s`);
  const deciding = expect.outcome === "gave-up" ? 0 : expect.outcome === "opened-late" ? 1 : sameAnswers;
  const early = looks.slice(0, looks.length - deciding);
  for (const l of early) if (!expect.answers.includes(l.answer)) problems.push(`look ${l.look} answered "${l.answer}", expected one of ${expect.answers.join(", ")}`);
  const said = looks.map((l) => `${l.at} s: ${l.answer}`).join("; ");
  if (expect.outcome === "gave-up") {
    // The whole window, give or take one interval (the uptime is read in whole seconds).
    if (last.at < windowS - intervalS) problems.push(`the looks ended ${last.at} s after the first, before the window of ${windowS} s was over (${said})`);
    if (!journal.includes("not wiping it: it stays locked")) problems.push("the setup did not log that it gave up without wiping");
  } else if (expect.outcome === "opened-late") {
    if (last.answer !== "opens" || looks.length < 2) problems.push(`expected looks that did not open the volume, then one that did (${said})`);
  } else {
    const tail = looks.slice(-sameAnswers);
    const owner = /^other-owner (000b[0-9a-f]{64})$/.exec(tail[0]?.answer ?? "")?.[1];
    if (tail.length < sameAnswers || !owner || tail.some((l) => l.answer !== `other-owner ${owner}`)) problems.push(`expected the same other owner hierarchy in the last ${sameAnswers} looks (${said})`);
    if (!journal.includes("and making it anew for this TPM")) problems.push("the setup did not log the wipe");
  }
  return problems;
}

function started(unit: Record<string, string> | undefined): boolean {
  return Number(unit?.InactiveExitTimestampMonotonic ?? "0") > 0 || Number(unit?.ExecMainStartTimestampMonotonic ?? "0") > 0;
}

/** Why a boot's probe output does not show what `expect` says, if it does not. */
export function bootProblems(probeText: string, expect: BootExpectation): string[] {
  const { sections, complete } = parseProbe(probeText);
  const problems: string[] = [];
  if (!complete) problems.push("the probe's output is incomplete (no === end)");
  const section = (name: string): ProbeSection => {
    const found = sections.get(name);
    if (!found) problems.push(`the probe wrote no ${name} section`);
    return found ?? { output: "", exit: -1 };
  };
  const units = parseShow(section("units").output);
  const unit = (id: string) => units.get(id);
  const setup = unit(UNITS.setup);
  const unlock = unit(UNITS.unlock);
  const mount = unit(UNITS.mount);
  const agent = unit(UNITS.agent);

  // Every boot: the boot finished, the gateway stays reachable, nothing waits at a prompt, and
  // nothing is kept on the root partition (journal, swap file, core dumps).
  if (section("boot-finished").exit === 124) problems.push("the boot did not finish (systemctl is-system-running --wait timed out)");
  if (unit(UNITS.ssh)?.ActiveState !== "active") problems.push(`ssh.service is ${unit(UNITS.ssh)?.ActiveState ?? "missing"}, expected active`);
  if (section("ask-password").output.trim() !== "" && section("ask-password").exit === 0) {
    problems.push(`a password prompt is waiting: /run/systemd/ask-password has ${section("ask-password").output.trim()}`);
  }
  if (section("journal-prompts").output.split("\n").some((l) => /Please enter/.test(l))) problems.push("the journal shows a password prompt");
  // The source image ships an empty /var/log/journal; with Storage=volatile nothing appears in it.
  if (section("journal-files").exit === 0 && section("journal-files").output.trim() !== "") problems.push(`/var/log/journal is not empty: ${section("journal-files").output.trim()}`);
  const swaps = section("swap").output.split("\n").filter((l) => l.trim() !== "");
  for (const swap of swaps) if (!/^\/dev\/zram\d+\s/.test(swap)) problems.push(`swap on ${swap.trim()}, expected only zram`);
  if (section("core-files").output.trim() !== "") problems.push(`core dumps: ${section("core-files").output.trim()}`);

  const setupResult = setup?.Result ?? "missing";
  if (expect.setup === "success" && !(setup?.ActiveState === "active" && setupResult === "success")) {
    problems.push(`${UNITS.setup} is ${setup?.ActiveState}/${setupResult}, expected active/success`);
  }
  if (expect.setup === "failed" && !(setup?.ActiveState === "failed" && setupResult === "exit-code")) {
    problems.push(`${UNITS.setup} is ${setup?.ActiveState}/${setupResult}, expected failed/exit-code (refused)`);
  }
  if (expect.setupLogs && !section("journal-setup").output.includes(expect.setupLogs)) problems.push(`the setup did not log "${expect.setupLogs}"`);

  if (expect.volume === "unlocked") {
    if (unlock?.ActiveState !== "active") problems.push(`${UNITS.unlock} is ${unlock?.ActiveState}/${unlock?.Result}, expected active`);
    if (mount?.ActiveState !== "active") problems.push(`${UNITS.mount} is ${mount?.ActiveState}/${mount?.Result}, expected active`);
    const [source = "", fstype = "", options = ""] = section("findmnt").output.trim().split(/\s+/);
    if (source !== `/dev/mapper/${LUKS.mapping}` || fstype !== "ext4") problems.push(`${DATA.mountPoint} is mounted from "${source}" (${fstype}), expected /dev/mapper/${LUKS.mapping} (ext4)`);
    for (const option of ["noatime", "nodev", "nosuid", "noexec"]) if (!options.split(",").includes(option)) problems.push(`the mount has no ${option}: ${options}`);
    if (section("state-dir").output.trim() !== `trustiot-agent trustiot-agent 700 ${DATA.mountPoint}`) {
      problems.push(`the state directory is "${section("state-dir").output.trim()}", expected trustiot-agent trustiot-agent 700 on its own mount`);
    }
    if (expect.mark) {
      // After a wipe the agent stays off: its condition on the flag was evaluated and failed.
      if (started(agent)) problems.push(`${UNITS.agent} was started although the volume carries the mark of a wipe`);
      if (agent?.ConditionResult !== "no" || !(Number(agent?.ConditionTimestampMonotonic ?? "0") > 0)) {
        problems.push(`${UNITS.agent} has ConditionResult=${agent?.ConditionResult ?? "(not shown)"}, ConditionTimestampMonotonic=${agent?.ConditionTimestampMonotonic ?? "(not shown)"}: expected an evaluated ConditionPathExists=!${DATA_CHECK.factoryFlag} that failed`);
      }
    } else if (!started(agent)) problems.push(`${UNITS.agent} was not started`);
    const json = section("luks-json").output;
    try {
      problems.push(...luksHeaderProblems(json, { mark: expect.mark }));
      // The fingerprint the setup recorded is the name of the storage key systemd-cryptenroll
      // persisted: the setup's template is systemd's, on this image's tpm2-tools and systemd.
      const owner = ownerToken(json);
      const srk = section("srk-name").output.trim();
      if (owner !== null && owner !== srk) problems.push(`the recorded owner fingerprint ${owner} is not the name of the storage key at 0x81000001 (${srk || "none"})`);
      const mark = markToken(json);
      if (expect.mark && mark && (mark.reason !== "tpm-owner-changed" || mark.found_owner !== owner || typeof mark.sealed_owner !== "string" || mark.sealed_owner === owner)) {
        problems.push(`the mark ${JSON.stringify(mark)} does not say that the volume was wiped for the owner hierarchy it is now sealed under (${owner})`);
      }
    } catch (error) {
      problems.push(`the LUKS2 header could not be read: ${error instanceof Error ? error.message : String(error)}`);
    }
    const blkid = section("blkid-data").output.split("\n");
    for (const line of [`TYPE=crypto_LUKS`, `UUID=${LUKS.uuid}`, `LABEL=${LUKS.label}`]) if (!blkid.includes(line)) problems.push(`the data partition has no ${line} (blkid)`);
    const devices = parseShow(section("device-timeouts").output);
    for (const device of DEVICES) {
      const timeout = devices.get(device)?.JobRunningTimeoutUSec;
      if (timeout !== DATA.deviceTimeout) problems.push(`${device} waits ${timeout ?? "(not shown)"}, expected ${DATA.deviceTimeout}`);
    }
    try {
      const slots = parseEnrollList(section("enroll-list").output);
      if (JSON.stringify(slots.map((s) => s.type)) !== JSON.stringify(["tpm2"])) problems.push(`keyslots ${JSON.stringify(slots)}, expected only the TPM's`);
    } catch (error) {
      problems.push(String(error instanceof Error ? error.message : error));
    }
  } else {
    if (unlock?.ActiveState === "active") problems.push(`${UNITS.unlock} is active, expected the volume to stay locked`);
    if (unlock?.Result === "timeout") problems.push(`${UNITS.unlock} ran into its time limit instead of failing`);
    // After a successful setup the unlock must have run and failed on its own (the TPM refused or
    // was missing); after a refused setup it never runs (its Requires= failed).
    if (expect.setup === "success" && !(unlock?.ActiveState === "failed" && unlock.Result === "exit-code")) {
      problems.push(`${UNITS.unlock} is ${unlock?.ActiveState}/${unlock?.Result}, expected failed/exit-code: the TPM unlock was tried and failed`);
    }
    if (expect.setup === "failed" && started(unlock)) problems.push(`${UNITS.unlock} ran although the setup refused`);
    if (mount?.ActiveState === "active" || section("findmnt").output.trim() !== "") problems.push(`${DATA.mountPoint} is mounted: ${section("findmnt").output.trim()}`);
    if (started(agent)) problems.push(`${UNITS.agent} was started without its data partition`);
    if (section("state-dir-entries").output.trim() !== "") problems.push(`${DATA.mountPoint} on the root partition is not empty: ${section("state-dir-entries").output.trim()}`);
    if (!section("state-dir").output.trim().endsWith(" /")) problems.push(`${DATA.mountPoint} is "${section("state-dir").output.trim()}", expected the empty directory on the root file system`);
  }

  if (expect.startAgent) problems.push(...startAgentProblems(section));

  // The flag that keeps the agent off is there exactly when the volume carries the mark.
  const flag = section("factory-flag");
  if (expect.mark && (flag.exit !== 0 || !flag.output.split("\n").includes("reason=tpm-owner-changed"))) {
    problems.push(`${DATA_CHECK.factoryFlag} is ${flag.exit === 0 ? JSON.stringify(flag.output) : "missing"}, expected reason=tpm-owner-changed`);
  }
  if (!expect.mark && flag.exit === 0) problems.push(`${DATA_CHECK.factoryFlag} exists: ${JSON.stringify(flag.output)}`);
  if (expect.tpmLooks) problems.push(...tpmLookProblems(section("journal-setup").output, expect.tpmLooks));

  if (expect.writeMarker !== undefined && section("write-marker").exit !== 0) problems.push("the marker could not be written on the volume");
  if (expect.readMarker !== undefined && section("read-marker").output.trim() !== expect.readMarker) {
    problems.push(`the marker read back is "${section("read-marker").output.trim()}", expected ${expect.readMarker}`);
  }
  if (expect.markerGone && section("read-marker").exit === 0) problems.push(`the volume still holds a marker: "${section("read-marker").output.trim()}"`);
  return problems;
}

/**
 * Why a header made by a wipe (`json`, read on the build host) does not carry the mark of that
 * wipe, if it does not: the mark names the owner hierarchy the volume was sealed under before
 * (`sealedBefore`, the fingerprint in the header before the wipe) and the one the new volume is
 * sealed under, which is another one.
 */
export function wipeMarkProblems(json: string, sealedBefore: string | null): string[] {
  const owner = ownerToken(json);
  const mark = markToken(json);
  if (mark === null) return ["the header carries no mark of a wipe"];
  const problems: string[] = [];
  if (mark.reason !== "tpm-owner-changed") problems.push(`the mark's reason is ${JSON.stringify(mark.reason)}, expected "tpm-owner-changed"`);
  if (sealedBefore === null || mark.sealed_owner !== sealedBefore) problems.push(`the mark says the volume was sealed under ${JSON.stringify(mark.sealed_owner)}, expected ${sealedBefore ?? "(no fingerprint before the wipe)"}`);
  if (owner === null || mark.found_owner !== owner) problems.push(`the mark says it found ${JSON.stringify(mark.found_owner)}, but the volume is sealed under ${owner ?? "(no fingerprint)"}`);
  if (owner !== null && owner === sealedBefore) problems.push(`the volume is sealed under the owner hierarchy it was sealed under before the wipe (${owner})`);
  return problems;
}

/**
 * Why the agent's start by hand (probe.sh, trustiot_test.start-agent=1) did not fail closed, if it
 * did not: both starts fail without hanging, the one that skips the dependencies on the agent's
 * AssertPathIsMountPoint=; the agent never left "inactive" and no process runs its executable;
 * nothing is mounted and the directory on the root partition is still empty.
 */
function startAgentProblems(section: (name: string) => ProbeSection): string[] {
  const problems: string[] = [];
  const starts = [
    { name: "start-agent", command: "systemctl start trustiot-agent.service" },
    { name: "start-agent-ignoring-dependencies", command: "systemctl start --job-mode=ignore-dependencies trustiot-agent.service" },
  ];
  for (const { name, command } of starts) {
    const { exit } = section(name);
    if (exit === 0) problems.push(`${command} succeeded while the data partition is locked`);
    if (exit === 124) problems.push(`${command} did not return within 600 s`);
  }
  const plain = section("start-agent");
  if (plain.exit !== 0 && plain.exit !== 124 && !plain.output.includes("A dependency job for trustiot-agent.service failed")) {
    problems.push(`the plain start failed for another reason than a failed dependency: ${JSON.stringify(plain.output.trim())}`);
  }
  const bypass = section("start-agent-ignoring-dependencies");
  if (bypass.exit !== 0 && bypass.exit !== 124 && !bypass.output.includes("Assertion failed")) {
    problems.push(`the start that skips the dependencies failed for another reason than the assertion: ${JSON.stringify(bypass.output.trim())}`);
  }
  const prompts = section("start-agent-ask-password");
  if (prompts.exit === 0 && prompts.output.trim() !== "") problems.push(`a password prompt is waiting after the start: ${prompts.output.trim()}`);
  const units = parseShow(section("start-agent-units").output);
  const agent = units.get(UNITS.agent);
  // AssertResult is "no" also when the assertion was never evaluated: its timestamp says it was.
  if (agent?.AssertResult !== "no" || !(Number(agent?.AssertTimestampMonotonic ?? "0") > 0)) {
    problems.push(`${UNITS.agent} has AssertResult=${agent?.AssertResult ?? "(not shown)"}, AssertTimestampMonotonic=${agent?.AssertTimestampMonotonic ?? "(not shown)"}: expected an evaluated AssertPathIsMountPoint= that failed`);
  }
  if (started(agent) || agent?.ExecMainPID !== "0" || agent?.ActiveState !== "inactive") {
    problems.push(`${UNITS.agent} ran: ActiveState=${agent?.ActiveState}, ExecMainPID=${agent?.ExecMainPID}, InactiveExitTimestampMonotonic=${agent?.InactiveExitTimestampMonotonic}`);
  }
  const findmnt = section("start-agent-findmnt").output.trim();
  if (units.get(UNITS.mount)?.ActiveState === "active" || findmnt !== "") problems.push(`${DATA.mountPoint} is mounted after the start: ${findmnt}`);
  if (units.get(UNITS.unlock)?.ActiveState === "active") problems.push(`${UNITS.unlock} is active after the start`);
  const entries = section("start-agent-state-dir-entries").output.trim();
  if (entries !== "") problems.push(`${DATA.mountPoint} on the root partition is not empty after the start: ${entries}`);
  const processes = section("start-agent-processes");
  if (processes.exit !== 0 || processes.output.trim() !== "") problems.push(`processes run /usr/bin/trustiot-agent: ${processes.output.trim()} (exit ${processes.exit})`);
  return problems;
}

/**
 * The programs boot-test/power-cut.sh stands in for, and the real program it runs: its
 * `<name>) real=<path> ;;` lines.
 */
export function powerCutPrograms(script: string): Map<string, string> {
  return new Map([...script.matchAll(/^([a-z0-9-]+)\) real=(\/\S+) ;;$/gm)].map((m) => [m[1] ?? "", m[2] ?? ""]));
}

/** What the data partition holds after a power cut in the setup, when it holds a LUKS2 volume. */
export interface LuksCutState {
  /** The LUKS2 label: the setup had not finished (labelNew) or had (label). */
  label: string;
  /** The keyslots in the header: 0 is the setup's temporary key, 1 the TPM's. */
  keyslots: string[];
  /** Whether the header holds systemd's TPM2 token, on keyslot 1. */
  tpmToken: boolean;
  /**
   * Whether the header carries the mark of a wipe (LUKS.markToken). Every header the setup makes
   * holds its owner fingerprint token (LUKS.ownerToken) from the first write on.
   */
  mark?: boolean;
}

/** What the data partition holds after a power cut (boot-test/power-cut.sh): all zeros, or a LUKS2 volume. */
export type CutState = "zeros" | LuksCutState;

/** The data partition as the build host reads it after a power cut. */
export interface SeenState {
  /** `blkid --probe --output export` of the partition (empty when blkid finds nothing). */
  blkid: string;
  /** The LUKS2 header (`cryptsetup luksDump --dump-json-metadata`), when blkid finds one. */
  json: string | null;
  /** The whole partition is zeros. */
  allZero: boolean;
}

export interface PowerCut {
  /** The step of power-cut.sh after which the virtual machine loses its power. */
  step: string;
  /** What the data partition must hold after the cut. */
  state: CutState;
  /**
   * What the setup must log in the next boot, which must unlock and mount the volume with the
   * TPM's keyslot as its only one.
   */
  nextLogs: string;
}

const FIRST_BOOT = "is empty: first boot";
const UNFINISHED = "holds a setup that did not finish (no data was ever written to it): starting again";
const FINISHED = "is encrypted and opens only with the TPM";
/** What the setup logs when it wipes the volume (Laysi/trustiot-v2#867). */
export const WIPED = "and making it anew for this TPM";
/** The new header is on the partition: label LABEL_NEW and the temporary key's keyslot, no TPM token yet. */
const HEADER_WRITTEN: LuksCutState = { label: LUKS.labelNew, keyslots: ["0"], tpmToken: false };
/** From the TPM's enrollment until the temporary key's keyslot is wiped. */
const ENROLLED: LuksCutState = { label: LUKS.labelNew, keyslots: ["0", "1"], tpmToken: true };
const TPM_ONLY_NEW: LuksCutState = { label: LUKS.labelNew, keyslots: ["1"], tpmToken: true };

/**
 * The power cuts in the first boot's setup (on an all-zero partition), in the order of its steps.
 * luksFormat makes the header in a file on /run, so a cut right after it leaves the partition as
 * it was: the next boot is a first boot again.
 */
export const POWER_CUTS: PowerCut[] = [
  { step: "luks-format", state: "zeros", nextLogs: FIRST_BOOT },
  { step: "write-header", state: HEADER_WRITTEN, nextLogs: UNFINISHED },
  { step: "write-keyslots", state: HEADER_WRITTEN, nextLogs: UNFINISHED },
  { step: "tpm-enroll", state: ENROLLED, nextLogs: UNFINISHED },
  { step: "mkfs", state: ENROLLED, nextLogs: UNFINISHED },
  { step: "kill-slot", state: TPM_ONLY_NEW, nextLogs: UNFINISHED },
  { step: "before-relabel", state: TPM_ONLY_NEW, nextLogs: UNFINISHED },
  { step: "after-relabel", state: { label: LUKS.label, keyslots: ["1"], tpmToken: true }, nextLogs: FINISHED },
];

/**
 * The power cuts in a setup that starts again on the partition a cut at mkfs left (a LABEL_NEW
 * header in front of the earlier attempt's file system, so the partition is no longer zeros behind
 * the header), at each step until the new header and its keyslot area are on the partition. From
 * there on the restarted setup runs the first boot's steps on a partition like the one POWER_CUTS
 * covers. The step that changes the partition first replaces the old header with the new one,
 * never with zeros (Laysi/trustiot-v2#917).
 */
export const RESTART_CUTS: PowerCut[] = [
  { step: "luks-format", state: ENROLLED, nextLogs: UNFINISHED },
  { step: "write-header", state: HEADER_WRITTEN, nextLogs: UNFINISHED },
  { step: "write-keyslots", state: HEADER_WRITTEN, nextLogs: UNFINISHED },
];

/**
 * The power cuts in a wipe (Laysi/trustiot-v2#867): a finished volume whose TPM is another one
 * (the setup's looks found the same other owner hierarchy often enough), cut at the steps where
 * the mark could get lost. The wipe runs the first boot's steps, so the steps between these are
 * the ones POWER_CUTS covers. Every next boot, with the same other TPM, ends with the volume
 * sealed under that TPM and the mark in its header.
 */
export const WIPE_CUTS: PowerCut[] = [
  // luksFormat only writes /run: the partition is still the old volume, without the mark, and
  // the next boot looks at the TPM again and wipes it.
  { step: "luks-format", state: { label: LUKS.label, keyslots: ["1"], tpmToken: true }, nextLogs: WIPED },
  // The new header carries the mark from its first write on, and the setup that starts again
  // carries it on into the next header.
  { step: "write-header", state: { ...HEADER_WRITTEN, mark: true }, nextLogs: UNFINISHED },
  { step: "after-relabel", state: { label: LUKS.label, keyslots: ["1"], tpmToken: true, mark: true }, nextLogs: FINISHED },
];

/** A cut state in words, for the test's output. */
export function describeCutState(state: CutState): string {
  if (state === "zeros") return "all zeros";
  return `label ${state.label}, keyslots ${state.keyslots.join(" and ")}${state.tpmToken ? ", the TPM's token" : ""}${state.mark ? ", the mark of a wipe" : ""}`;
}

/**
 * Why the data partition after a power cut is not in the state its step leaves (so the cut did not
 * happen there), if it is not.
 */
export function cutStateProblems(expect: CutState, seen: SeenState): string[] {
  if (expect === "zeros") return seen.allZero ? [] : [`the data partition is not all zeros (blkid: ${JSON.stringify(seen.blkid.trim())})`];
  const problems: string[] = [];
  const lines = seen.blkid.split("\n");
  for (const line of ["TYPE=crypto_LUKS", `UUID=${LUKS.uuid}`, `LABEL=${expect.label}`]) if (!lines.includes(line)) problems.push(`the data partition has no ${line} (blkid)`);
  if (seen.json === null) return [...problems, "the data partition has no LUKS2 header to read"];
  let meta: { keyslots?: Record<string, unknown>; tokens?: Record<string, { type?: string; keyslots?: string[] }> };
  try {
    meta = JSON.parse(seen.json);
  } catch (error) {
    return [...problems, `the LUKS2 header could not be read: ${error instanceof Error ? error.message : String(error)}`];
  }
  const keyslots = Object.keys(meta.keyslots ?? {}).sort();
  if (JSON.stringify(keyslots) !== JSON.stringify(expect.keyslots)) problems.push(`keyslots ${JSON.stringify(keyslots)}, expected ${JSON.stringify(expect.keyslots)}`);
  // By id: systemd-cryptenroll takes the first free one, 0, beside the setup's own (1 and 2).
  const tokens = Object.entries(meta.tokens ?? {})
    .map(([id, t]) => `${id} ${t.type} ${JSON.stringify(t.keyslots)}`)
    .sort();
  const want = [
    ...(expect.tpmToken ? ['0 systemd-tpm2 ["1"]'] : []),
    `${LUKS.ownerToken.id} ${LUKS.ownerToken.type} []`,
    ...(expect.mark ? [`${LUKS.markToken.id} ${LUKS.markToken.type} []`] : []),
  ];
  if (JSON.stringify(tokens) !== JSON.stringify(want)) problems.push(`tokens ${JSON.stringify(tokens)}, expected ${JSON.stringify(want)}`);
  return problems;
}
