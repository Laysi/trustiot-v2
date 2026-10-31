// The boot test's results (README.md, section 「開機測試」): the sections boot-test/probe.sh writes
// inside the virtual machine, and what each boot must show. Pure functions, so the unit tests
// cover them without a virtual machine.
import { deviceUnit, luksHeaderProblems, parseEnrollList } from "./data.ts";
import { DATA, LUKS } from "./pins.ts";

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
    if (!started(agent)) problems.push(`${UNITS.agent} was not started`);
    try {
      problems.push(...luksHeaderProblems(section("luks-json").output));
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

  if (expect.writeMarker !== undefined && section("write-marker").exit !== 0) problems.push("the marker could not be written on the volume");
  if (expect.readMarker !== undefined && section("read-marker").output.trim() !== expect.readMarker) {
    problems.push(`the marker read back is "${section("read-marker").output.trim()}", expected ${expect.readMarker}`);
  }
  return problems;
}
