// The boot test's evaluation (src/boot-results.ts): what each boot's probe output must show for the
// data partition to count as unlocked or as locked (fail closed).
import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  DEVICES,
  POWER_CUTS,
  RESTART_CUTS,
  UNITS,
  WIPED,
  WIPE_CUTS,
  type LuksCutState,
  bootProblems,
  cutStateProblems,
  describeCutState,
  parseProbe,
  parseShow,
  parseTpmLooks,
  powerCutPrograms,
  redactTpmFields,
  tpmLookProblems,
  unitSeconds,
  wipeMarkProblems,
} from "../src/boot-results.ts";
import { REPO } from "../src/host.ts";
import { DATA, DATA_CHECK, LUKS } from "../src/pins.ts";

/** Owner fingerprints (TPM 2.0 names with SHA-256) of two TPMs, made up. */
const OWNER_A = `000b${"a1".repeat(32)}`;
const OWNER_B = `000b${"b2".repeat(32)}`;
const ownerToken = (owner: string) => ({ type: LUKS.ownerToken.type, keyslots: [], owner });
const markToken = (sealed: string, found: string) => ({ type: LUKS.markToken.type, keyslots: [], reason: "tpm-owner-changed", sealed_owner: sealed, found_owner: found });
const headerWith = (tokens: Record<string, object>) =>
  JSON.stringify({
    keyslots: { "1": { type: "luks2" } },
    tokens: { "0": { type: "systemd-tpm2", keyslots: ["1"], "tpm2-pcrs": [] }, ...tokens },
    segments: { "0": { type: "crypt", offset: "16777216", encryption: LUKS.cipher, sector_size: LUKS.sectorBytes } },
    config: { json_size: "12288", keyslots_size: "16744448" },
  });
const header = headerWith({ "1": ownerToken(OWNER_A) });

function unit(id: string, props: Record<string, string>): string {
  return [`Id=${id}`, ...Object.entries(props).map(([k, v]) => `${k}=${v}`)].join("\n");
}

const NEVER = { InactiveExitTimestampMonotonic: "0", ExecMainStartTimestampMonotonic: "0" };

function probe(sections: Record<string, string | [string, number]>): string {
  const parts = Object.entries(sections).map(([name, value]) => {
    const [output, exit] = typeof value === "string" ? [value, 0] : value;
    return `=== ${name}\n${output === "" ? "" : `${output}\n`}=== exit ${exit}`;
  });
  return `${parts.join("\n")}\n=== end\n`;
}

const common = {
  "boot-finished": ["degraded", 1] as [string, number],
  "ask-password": "",
  "journal-prompts": ["-- No entries --", 1] as [string, number],
  "journal-files": ["ls: cannot access '/var/log/journal': No such file or directory", 2] as [string, number],
  swap: "/dev/zram0 partition",
  "core-files": "",
  "factory-flag": ["cat: /run/trustiot-back-to-factory: No such file or directory", 1] as [string, number],
};

const unlocked = probe({
  ...common,
  units: [
    unit(UNITS.setup, { ActiveState: "active", Result: "success", InactiveExitTimestampMonotonic: "5" }),
    unit(UNITS.unlock, { ActiveState: "active", Result: "success" }),
    unit(UNITS.mount, { ActiveState: "active", Result: "success" }),
    unit(UNITS.agent, { ActiveState: "activating", Result: "exit-code", InactiveExitTimestampMonotonic: "9", NRestarts: "3" }),
    unit(UNITS.ssh, { ActiveState: "active", Result: "success" }),
  ].join("\n\n"),
  findmnt: `/dev/mapper/${LUKS.mapping} ext4   rw,nosuid,nodev,noexec,noatime`,
  "state-dir": "trustiot-agent trustiot-agent 700 /var/lib/trustiot-agent",
  "state-dir-entries": "lost+found",
  "blkid-data": `DEVNAME=/dev/disk/by-id/virtio-trustiot-gateway-part3\nVERSION=2\nUUID=${LUKS.uuid}\nLABEL=${LUKS.label}\nTYPE=crypto_LUKS\nUSAGE=crypto`,
  "luks-json": header,
  "enroll-list": "SLOT TYPE\n   1 tpm2",
  "srk-name": OWNER_A,
  "device-timeouts": DEVICES.map((d) => `Id=${d}\nJobRunningTimeoutUSec=${DATA.deviceTimeout}`).join("\n\n"),
  "journal-setup": "/dev/vda3 is encrypted and opens only with the TPM",
  "read-marker": "0123456789abcdef0123456789abcdef",
});

const locked = probe({
  ...common,
  units: [
    unit(UNITS.setup, { ActiveState: "active", Result: "success" }),
    unit(UNITS.unlock, { ActiveState: "failed", Result: "exit-code" }),
    unit(UNITS.mount, { ActiveState: "inactive", Result: "success" }),
    unit(UNITS.agent, { ActiveState: "inactive", Result: "success", ...NEVER }),
    unit(UNITS.ssh, { ActiveState: "active", Result: "success" }),
  ].join("\n\n"),
  findmnt: ["", 1],
  "state-dir": "root root 700 /",
  "state-dir-entries": "",
  "read-marker": ["cat: /var/lib/trustiot-agent/boot-test-marker: No such file or directory", 1],
});

// The locked boot, then the agent started by hand (probe.sh, trustiot_test.start-agent=1), as the
// boot test saw it.
const startedByHand = locked.replace(
  "=== end\n",
  probe({
    "start-agent": ["A dependency job for trustiot-agent.service failed. See 'journalctl -xe' for details.", 1],
    "start-agent-ignoring-dependencies": ["Assertion failed on job for trustiot-agent.service.", 1],
    "start-agent-units": [
      unit(UNITS.agent, { ActiveState: "inactive", Result: "success", AssertResult: "no", AssertTimestampMonotonic: "16166684", ExecMainPID: "0", ...NEVER }),
      unit(UNITS.mount, { ActiveState: "inactive", Result: "success" }),
      unit(UNITS.unlock, { ActiveState: "failed", Result: "exit-code" }),
    ].join("\n\n"),
    "start-agent-ask-password": "",
    "start-agent-findmnt": ["", 1],
    "start-agent-state-dir-entries": "",
    "start-agent-processes": "",
    "start-agent-journal": "-- No entries --",
  }),
);

describe("probe output", () => {
  test("sections and their exit codes; incomplete output is noticed", () => {
    const parsed = parseProbe("=== a\nx\ny\n=== exit 0\n=== b\n=== exit 3\n=== end\n\0\0\0");
    expect(parsed.complete).toBe(true);
    expect(parsed.sections.get("a")).toEqual({ output: "x\ny", exit: 0 });
    expect(parsed.sections.get("b")).toEqual({ output: "", exit: 3 });
    expect(parseProbe("=== a\nx\n").complete).toBe(false);
  });

  test("systemctl show blocks by unit", () => {
    const units = parseShow("Id=a.service\nActiveState=active\n\nId=b.mount\nActiveState=failed\n");
    expect(units.get("b.mount")?.ActiveState).toBe("failed");
    expect(units.size).toBe(2);
  });

  test("the probe script writes the sections the evaluation reads", () => {
    const script = readFileSync(join(REPO, "edge/image/boot-test/probe.sh"), "utf8");
    const written = new Set([...script.matchAll(/^\s*section ([a-z-]+) /gm)].map((m) => m[1]));
    for (const name of new Set([...parseProbe(unlocked).sections.keys(), ...parseProbe(startedByHand).sections.keys(), "write-marker"])) {
      expect(written.has(name) || name === "write-marker").toBe(true);
    }
    expect(script).toContain('section write-marker sh -c');
  });

  test("the probe reads the LUKS2 header from a copy of the header area, never from the partition", () => {
    // cryptsetup writes one metadata copy over the other whenever it reads a header whose copies
    // differ (Laysi/trustiot-v2#918): the probe must leave the partition as the setup left it.
    const script = readFileSync(join(REPO, "edge/image/boot-test/probe.sh"), "utf8");
    const copy = /^section luks-header-copy dd if="\$data" of="\$header_copy" bs=1M count=(\d+) status=none$/m.exec(script);
    expect(Number(copy?.[1]) * 1024 * 1024).toBe(LUKS.headerBytes);
    expect(script).toContain('section luks-json cryptsetup luksDump --dump-json-metadata "$header_copy"');
    expect(script).toContain('section enroll-list systemd-cryptenroll "$header_copy"');
    // No other cryptsetup or systemd-cryptenroll call names the partition.
    for (const line of script.split("\n").filter((l) => /\b(cryptsetup|systemd-cryptenroll)\b/.test(l) && !l.trimStart().startsWith("#"))) expect(line).not.toContain('"$data"');
  });

  test("the probe puts every exit marker on a line of its own, also after output without a final newline", async () => {
    // cryptsetup luksDump --dump-json-metadata ends without a newline; the marker after it once
    // landed on the same line and the JSON could not be read.
    const script = readFileSync(join(REPO, "edge/image/boot-test/probe.sh"), "utf8");
    const fn = /^section\(\) \{\n[^]*?\n\}\n/m.exec(script)?.[0];
    expect(fn).toBeDefined();
    const dir = mkdtempSync(join(tmpdir(), "probe-"));
    try {
      const out = join(dir, "out.txt");
      const child = Bun.spawn(["bash", "-c", `${fn}\nout=$1\nsection json printf '{"a":1}'\nsection empty true\nsection failed sh -c 'echo no; exit 3'\n`, "bash", out]);
      expect(await child.exited).toBe(0);
      const parsed = parseProbe(`${readFileSync(out, "utf8")}=== end\n`);
      expect(parsed.sections.get("json")).toEqual({ output: '{"a":1}', exit: 0 });
      expect(parsed.sections.get("empty")).toEqual({ output: "", exit: 0 });
      expect(parsed.sections.get("failed")).toEqual({ output: "no", exit: 3 });
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("unit timestamps in seconds since boot", () => {
    expect(unitSeconds(unlocked, UNITS.setup, "InactiveExitTimestampMonotonic")).toBe(5 / 1e6);
    expect(unitSeconds(unlocked, UNITS.setup, "StateChangeTimestampMonotonic")).toBe(0);
  });

  test("reports leave out the TPM-sealed blob and the TPM's key, nothing else", () => {
    const pretty = '{\n  "tokens":{\n    "0":{\n      "type":"systemd-tpm2",\n      "tpm2-blob":"AJ4AIH+x/==",\n      "tpm2_srk":"gQAAAQAAAA==",\n      "tpm2-pcrs":[]\n    }\n  }\n}';
    const redacted = redactTpmFields(pretty);
    expect(redacted).not.toContain("AJ4AIH");
    expect(redacted).not.toContain("gQAAAQ");
    expect(JSON.parse(redacted).tokens["0"]).toEqual({ type: "systemd-tpm2", "tpm2-blob": "(left out)", tpm2_srk: "(left out)", "tpm2-pcrs": [] });
    expect(redactTpmFields('{"tpm2-blob": "x"}')).toBe('{"tpm2-blob": "(left out)"}');
  });
});

describe("an unlocked boot", () => {
  const expectUnlocked = { volume: "unlocked", setup: "success", setupLogs: "opens only with the TPM", readMarker: "0123456789abcdef0123456789abcdef" } as const;

  test("passes when the TPM opened and mounted the volume and the agent started", () => {
    expect(bootProblems(unlocked, expectUnlocked)).toEqual([]);
  });

  test("fails when the volume is mounted without its options, has a password keyslot, or the agent did not start", () => {
    expect(bootProblems(unlocked.replace("rw,nosuid,nodev,noexec,noatime", "rw,noatime"), expectUnlocked).join()).toContain("no nodev");
    expect(bootProblems(unlocked.replace("   1 tpm2", "   0 password\n   1 tpm2"), expectUnlocked).join()).toContain("expected only the TPM's");
    expect(bootProblems(unlocked.replace("InactiveExitTimestampMonotonic=9", "InactiveExitTimestampMonotonic=0"), expectUnlocked).join()).toContain("was not started");
    expect(bootProblems(unlocked.replace("0123456789abcdef0123456789abcdef", "x"), expectUnlocked).join()).toContain("marker read back");
  });

  test("fails, without throwing, when the LUKS2 header is not JSON or the volume is not the pinned one", () => {
    expect(bootProblems(unlocked.replace(header, `${header}=== exit 0`), expectUnlocked).join()).toContain("could not be read");
    expect(bootProblems(unlocked.replace(`LABEL=${LUKS.label}\n`, `LABEL=${LUKS.labelNew}\n`), expectUnlocked).join()).toContain(`no LABEL=${LUKS.label}`);
  });

  test("fails when the volume or the unlocked volume waits only systemd's default time", () => {
    const short = unlocked.replace(`Id=${DEVICES[1]}\nJobRunningTimeoutUSec=${DATA.deviceTimeout}`, `Id=${DEVICES[1]}\nJobRunningTimeoutUSec=1min 30s`);
    expect(bootProblems(short, expectUnlocked)).toEqual([`${DEVICES[1]} waits 1min 30s, expected ${DATA.deviceTimeout}`]);
  });

  test("fails when the journal or swap reaches the disk, or a prompt waits", () => {
    expect(bootProblems(unlocked.replace("ls: cannot access '/var/log/journal': No such file or directory", "0123abcd").replace("=== journal-files\n0123abcd\n=== exit 2", "=== journal-files\n0123abcd\n=== exit 0"), expectUnlocked).join()).toContain("/var/log/journal");
    expect(bootProblems(unlocked.replace("/dev/zram0 partition", "/var/swap file"), expectUnlocked).join()).toContain("expected only zram");
    expect(bootProblems(unlocked.replace("=== ask-password\n=== exit 0", "=== ask-password\nask.abc\n=== exit 0"), expectUnlocked).join()).toContain("password prompt");
  });
});

describe("a locked boot (fail closed)", () => {
  const expectLocked = { volume: "locked", setup: "success" } as const;

  test("passes when the unlock failed, nothing is mounted and the agent never started", () => {
    expect(bootProblems(locked, expectLocked)).toEqual([]);
  });

  test("fails when the agent started anyway, or wrote to the directory on the root partition", () => {
    expect(bootProblems(locked.replace(/(Id=trustiot-agent\.service\n)ActiveState=inactive/, "$1ActiveState=activating").replace(/(Id=trustiot-agent\.service[^]*?)InactiveExitTimestampMonotonic=0/, "$1InactiveExitTimestampMonotonic=7"), expectLocked).join()).toContain("started without its data partition");
    expect(bootProblems(locked.replace("=== state-dir-entries\n=== exit 0", "=== state-dir-entries\nstate.db\n=== exit 0"), expectLocked).join()).toContain("not empty");
  });

  test("fails when the unlock waited out its time limit (as at a prompt) or the volume opened", () => {
    expect(bootProblems(locked.replace("ActiveState=failed\nResult=exit-code", "ActiveState=failed\nResult=timeout"), expectLocked).join()).toContain("time limit");
    expect(bootProblems(locked.replace("ActiveState=failed\nResult=exit-code", "ActiveState=active\nResult=success"), expectLocked).join()).toContain("stay locked");
    // Never tried: inactive after a successful setup.
    expect(bootProblems(locked.replace("ActiveState=failed\nResult=exit-code", "ActiveState=inactive\nResult=success"), expectLocked).join()).toContain("expected failed/exit-code");
  });

  test("a refused setup must be a failed unit", () => {
    expect(bootProblems(locked, { volume: "locked", setup: "failed" }).join()).toContain("expected failed/exit-code");
  });

  test("the gateway must stay reachable", () => {
    expect(bootProblems(locked.replace(/(Id=ssh\.service\n)ActiveState=active/, "$1ActiveState=failed"), expectLocked).join()).toContain("ssh.service is failed");
  });

  test("a setup that could not tell whether the header carries the mark leaves the flag with every field unknown", () => {
    // Laysi/trustiot-v2#918: blkid and cryptsetup read different labels after a torn header write.
    const refused = locked
      .replace(/(Id=trustiot-data-setup\.service\n)ActiveState=active\nResult=success/, "$1ActiveState=failed\nResult=exit-code")
      .replace(/(Id=systemd-cryptsetup@trustiot_data\.service\n)ActiveState=failed\nResult=exit-code/, "$1ActiveState=inactive\nResult=success");
    const withFlag = (text: string) => refused.replace(/=== factory-flag\n[^]*?=== exit 1/, `=== factory-flag\n${text}\n=== exit 0`);
    const expectUnknown = { volume: "locked", setup: "failed", markUnknown: true } as const;
    expect(bootProblems(withFlag("reason=unknown\nsealed_owner=unknown\nfound_owner=unknown"), expectUnknown)).toEqual([]);
    expect(bootProblems(refused, expectUnknown).join()).toContain("missing, expected every field unknown");
    expect(bootProblems(withFlag(`reason=tpm-owner-changed\nsealed_owner=${OWNER_A}\nfound_owner=${OWNER_B}`), expectUnknown).join()).toContain("expected every field unknown");
    // Without it, the flag must not be there.
    expect(bootProblems(withFlag("reason=unknown\nsealed_owner=unknown\nfound_owner=unknown"), { volume: "locked", setup: "failed" }).join()).toContain("exists");
  });
});

describe("the agent started by hand while the volume is locked", () => {
  const expectStarted = { volume: "locked", setup: "success", startAgent: true } as const;
  const problems = (text: string) => bootProblems(text, expectStarted).join("\n");

  test("passes when both starts failed, the assertion stopped the one that skips the dependencies, and nothing ran", () => {
    expect(bootProblems(startedByHand, expectStarted)).toEqual([]);
  });

  test("the probe output must have the sections", () => {
    expect(problems(locked)).toContain("the probe wrote no start-agent section");
  });

  test("fails when a start succeeded or hung", () => {
    const plainOk = startedByHand.replace("details.\n=== exit 1", "details.\n=== exit 0");
    expect(problems(plainOk)).toContain("systemctl start trustiot-agent.service succeeded");
    const bypassOk = startedByHand.replace("Assertion failed on job for trustiot-agent.service.\n=== exit 1", "\n=== exit 0");
    expect(problems(bypassOk)).toContain("--job-mode=ignore-dependencies trustiot-agent.service succeeded");
    const hung = startedByHand.replace("details.\n=== exit 1", "details.\n=== exit 124");
    expect(problems(hung)).toContain("did not return within 600 s");
  });

  test("fails when the start that skips the dependencies was stopped by something else than the assertion", () => {
    expect(problems(startedByHand.replace("Assertion failed on job", "Job failed for"))).toContain("another reason than the assertion");
    expect(problems(startedByHand.replace("AssertResult=no", "AssertResult=yes"))).toContain("AssertResult=yes");
    // "no" is also what a unit shows whose assertion was never evaluated.
    expect(problems(startedByHand.replace("AssertTimestampMonotonic=16166684", "AssertTimestampMonotonic=0"))).toContain("expected an evaluated AssertPathIsMountPoint= that failed");
    expect(problems(startedByHand.replace("A dependency job for trustiot-agent.service failed.", "Failed to start trustiot-agent.service: Unit not found."))).toContain("another reason than a failed dependency");
  });

  test("fails when the start left a password prompt waiting", () => {
    expect(problems(startedByHand.replace("=== start-agent-ask-password\n=== exit 0", "=== start-agent-ask-password\nask.x1\n=== exit 0"))).toContain("password prompt is waiting after the start: ask.x1");
  });

  test("fails when the agent ran, something was mounted or written, or a process runs the agent", () => {
    expect(problems(startedByHand.replace("ExecMainPID=0", "ExecMainPID=812"))).toContain("ran: ActiveState=inactive, ExecMainPID=812");
    expect(problems(startedByHand.replace("=== start-agent-state-dir-entries\n=== exit 0", "=== start-agent-state-dir-entries\nstate.db\n=== exit 0"))).toContain("not empty after the start: state.db");
    expect(problems(startedByHand.replace("=== start-agent-processes\n=== exit 0", "=== start-agent-processes\n812\n=== exit 0"))).toContain("processes run /usr/bin/trustiot-agent: 812");
    const mounted = startedByHand.replace("=== start-agent-findmnt\n=== exit 1", `=== start-agent-findmnt\n/dev/mapper/${LUKS.mapping} ext4 rw\n=== exit 0`);
    expect(problems(mounted)).toContain("is mounted after the start");
  });
});

describe("an unlocked boot after a wipe (the mark)", () => {
  // TPM B's volume after a wipe: the mark names TPM A's owner hierarchy and TPM B's; the setup
  // left the flag, and the agent's condition on it kept the agent off.
  const markedHeader = headerWith({ "1": ownerToken(OWNER_B), "2": markToken(OWNER_A, OWNER_B) });
  const marked = unlocked
    .replace(header, markedHeader)
    .replace(`=== srk-name\n${OWNER_A}`, `=== srk-name\n${OWNER_B}`)
    .replace(/(Id=trustiot-agent\.service\n)[^]*?(\n\n)/, `$1ActiveState=inactive\nResult=success\nInactiveExitTimestampMonotonic=0\nExecMainStartTimestampMonotonic=0\nConditionResult=no\nConditionTimestampMonotonic=21000000$2`)
    .replace(/=== factory-flag\n.*\n=== exit 1/, `=== factory-flag\nreason=tpm-owner-changed\nsealed_owner=${OWNER_A}\nfound_owner=${OWNER_B}\n=== exit 0`)
    .replace(/=== read-marker\n.*\n=== exit 0/, "=== read-marker\ncat: /var/lib/trustiot-agent/boot-test-marker: No such file or directory\n=== exit 1");
  const expectMarked = { volume: "unlocked", setup: "success", mark: true, markerGone: true } as const;

  test("passes when the header has the mark, the flag is there and the agent's condition kept it off", () => {
    expect(bootProblems(marked, expectMarked)).toEqual([]);
  });

  test("fails when the agent started, or its condition was not what kept it off", () => {
    const startedAnyway = marked.replace("InactiveExitTimestampMonotonic=0\nExecMainStartTimestampMonotonic=0\nConditionResult", "InactiveExitTimestampMonotonic=9\nExecMainStartTimestampMonotonic=9\nConditionResult");
    expect(bootProblems(startedAnyway, expectMarked).join()).toContain("was started although the volume carries the mark");
    expect(bootProblems(marked.replace("ConditionResult=no", "ConditionResult=yes"), expectMarked).join()).toContain("expected an evaluated ConditionPathExists=!/run/trustiot-back-to-factory that failed");
    // "no" is also what a unit shows whose condition was never evaluated.
    expect(bootProblems(marked.replace("ConditionTimestampMonotonic=21000000", "ConditionTimestampMonotonic=0"), expectMarked).join()).toContain("expected an evaluated ConditionPathExists");
  });

  test("fails when the flag, the mark or the new volume is missing, or the mark names the wrong owner hierarchies", () => {
    const noFlag = marked.replace(/=== factory-flag\n[^]*?=== exit 0/, "=== factory-flag\ncat: /run/trustiot-back-to-factory: No such file or directory\n=== exit 1");
    expect(bootProblems(noFlag, expectMarked).join()).toContain("/run/trustiot-back-to-factory is missing, expected reason=tpm-owner-changed");
    const noMark = marked.replace(markedHeader, headerWith({ "1": ownerToken(OWNER_B) }));
    expect(bootProblems(noMark, expectMarked).join()).toContain("token 2 is missing, expected trustiot-back-to-factory");
    const wrongFound = marked.replace(markedHeader, headerWith({ "1": ownerToken(OWNER_B), "2": markToken(OWNER_A, OWNER_A) }));
    expect(bootProblems(wrongFound, expectMarked).join()).toContain("does not say that the volume was wiped for the owner hierarchy it is now sealed under");
    // The old data is still there: not a new volume.
    const oldData = marked.replace(/=== read-marker\n.*\n=== exit 1/, "=== read-marker\n0123456789abcdef0123456789abcdef\n=== exit 0");
    expect(bootProblems(oldData, expectMarked).join()).toContain("the volume still holds a marker");
  });

  test("without the mark expected, neither the flag nor the mark's token may be there", () => {
    const expectPlain = { volume: "unlocked", setup: "success" } as const;
    const flagOnly = unlocked.replace(/=== factory-flag\n.*\n=== exit 1/, "=== factory-flag\nreason=tpm-owner-changed\n=== exit 0");
    expect(bootProblems(flagOnly, expectPlain).join()).toContain("/run/trustiot-back-to-factory exists");
    expect(bootProblems(marked, expectPlain).join()).toContain("token 2 is the mark of a wipe, expected none");
    // And on a locked boot too.
    expect(bootProblems(locked.replace(/=== factory-flag\n.*\n=== exit 1/, "=== factory-flag\nreason=x\n=== exit 0"), { volume: "locked", setup: "success" }).join()).toContain("exists");
  });

  test("the recorded owner fingerprint must be the name of systemd's storage key", () => {
    const expectPlain = { volume: "unlocked", setup: "success" } as const;
    expect(bootProblems(unlocked.replace(`=== srk-name\n${OWNER_A}`, `=== srk-name\n${OWNER_B}`), expectPlain).join()).toContain(`the recorded owner fingerprint ${OWNER_A} is not the name of the storage key at 0x81000001 (${OWNER_B})`);
    expect(bootProblems(unlocked.replace(header, headerWith({})), expectPlain).join()).toContain("token 1 is missing, expected trustiot-tpm-owner");
  });

  test("a wipe's mark names the owner hierarchy before the wipe and the one after it (read on the build host)", () => {
    expect(wipeMarkProblems(markedHeader, OWNER_A)).toEqual([]);
    expect(wipeMarkProblems(headerWith({ "1": ownerToken(OWNER_B) }), OWNER_A)).toEqual(["the header carries no mark of a wipe"]);
    expect(wipeMarkProblems(headerWith({ "1": ownerToken(OWNER_B), "2": markToken(OWNER_B, OWNER_B) }), OWNER_A).join()).toContain(`the mark says the volume was sealed under "${OWNER_B}", expected ${OWNER_A}`);
    expect(wipeMarkProblems(headerWith({ "1": ownerToken(OWNER_B), "2": markToken(OWNER_A, OWNER_A) }), OWNER_A).join()).toContain(`the mark says it found "${OWNER_A}", but the volume is sealed under ${OWNER_B}`);
    expect(wipeMarkProblems(headerWith({ "1": ownerToken(OWNER_A), "2": markToken(OWNER_A, OWNER_A) }), OWNER_A).join()).toContain("the owner hierarchy it was sealed under before the wipe");
    expect(wipeMarkProblems(headerWith({ "1": ownerToken(OWNER_B), "2": { ...markToken(OWNER_A, OWNER_B), reason: "x" } }), OWNER_A).join()).toContain(`the mark's reason is "x"`);
    expect(wipeMarkProblems(markedHeader, null).join()).toContain("(no fingerprint before the wipe)");
  });
});

describe("the setup's looks at the TPM", () => {
  const { intervalS, windowS, sameAnswers } = DATA_CHECK;
  /** The setup's journal lines for these looks (each [seconds since the first, answer]). */
  const journal = (looks: [number, string][], tail: string) =>
    [...looks.map(([at, answer], i) => `[   ${(90 + at).toFixed(6)}] trustiot-data-setup[412]: the TPM (look ${i + 1}, ${at} s): ${answer}`), tail].join("\n");
  const every = (n: number, answer: string, from = 0, step: number = intervalS): [number, string][] => Array.from({ length: n }, (_, i) => [from + i * step, answer]);
  const GAVE_UP = "the TPM did not open /dev/vda3 within 180 s (last: absent); not wiping it: it stays locked";
  const gaveUp = journal(every(windowS / intervalS + 1, "absent"), GAVE_UP);

  test("the journal lines as the looks the setup logged", () => {
    expect(parseTpmLooks(journal([[0, "absent"], [12, `other-owner ${OWNER_B}`]], ""))).toEqual([
      { look: 1, at: 0, answer: "absent" },
      { look: 2, at: 12, answer: `other-owner ${OWNER_B}` },
    ]);
    expect(parseTpmLooks("nothing here")).toEqual([]);
  });

  test("gave up: looks for the whole window, every answer an allowed one, and no wipe", () => {
    expect(tpmLookProblems(gaveUp, { outcome: "gave-up", answers: ["absent"] })).toEqual([]);
    expect(tpmLookProblems(gaveUp, { outcome: "gave-up", answers: ["unrecorded"] }).join()).toContain('look 1 answered "absent", expected one of unrecorded');
    // Looks that ended early, or a setup that did not say it left the volume alone.
    expect(tpmLookProblems(journal(every(5, "absent"), GAVE_UP), { outcome: "gave-up", answers: ["absent"] }).join()).toContain("before the window of 180 s was over");
    expect(tpmLookProblems(journal(every(windowS / intervalS + 1, "absent"), ""), { outcome: "gave-up", answers: ["absent"] }).join()).toContain("did not log that it gave up without wiping");
  });

  test("looks are numbered in order, at least an interval apart, and never after the window", () => {
    const allowed = { outcome: "gave-up", answers: ["absent"] } as const;
    expect(tpmLookProblems(journal(every(windowS / 5 + 1, "absent", 0, 5), GAVE_UP), allowed).join()).toContain(`5 s apart, expected at least ${intervalS} s`);
    expect(tpmLookProblems(journal(every(windowS / intervalS + 2, "absent"), GAVE_UP), allowed).join()).toContain(`after the window of ${windowS} s`);
    expect(tpmLookProblems(gaveUp.replace("(look 3,", "(look 4,"), allowed).join()).toContain("look 3 is logged as look 4");
    expect(tpmLookProblems("", allowed)).toEqual(["the setup logged no look at the TPM"]);
  });

  test("opened late: looks that did not open the volume, then one that did", () => {
    const late = journal([...every(6, "absent"), [60, "opens"]], "/dev/vda3 is encrypted and opens only with the TPM");
    expect(tpmLookProblems(late, { outcome: "opened-late", answers: ["absent"] })).toEqual([]);
    expect(tpmLookProblems(journal([[0, "opens"]], ""), { outcome: "opened-late", answers: ["absent"] }).join()).toContain("expected looks that did not open the volume, then one that did");
    expect(tpmLookProblems(journal(every(6, "absent"), ""), { outcome: "opened-late", answers: ["absent"] }).join()).toContain("then one that did");
  });

  test("permanent: the same other owner hierarchy in the last looks, then the wipe", () => {
    const other = `other-owner ${OWNER_B}`;
    const wiped = journal(every(sameAnswers, other), `wiping /dev/vda3 ${WIPED}; the gateway goes back to the factory to be registered again`);
    expect(tpmLookProblems(wiped, { outcome: "permanent", answers: [] })).toEqual([]);
    // Fewer looks than it takes, another owner hierarchy among them, or no wipe in the journal.
    expect(tpmLookProblems(journal(every(sameAnswers - 1, other), WIPED), { outcome: "permanent", answers: [] }).join()).toContain("expected the same other owner hierarchy");
    const mixed = journal([[0, other], [10, `other-owner ${OWNER_A}`], [20, other]], WIPED);
    expect(tpmLookProblems(mixed, { outcome: "permanent", answers: [] }).join()).toContain("expected the same other owner hierarchy");
    expect(tpmLookProblems(journal(every(sameAnswers, other), "wiping the key's keyslot"), { outcome: "permanent", answers: [] }).join()).toContain("did not log the wipe");
    // Looks before the deciding ones must be allowed answers.
    const afterAbsent = journal([[0, "absent"], ...every(sameAnswers, other, 10)], WIPED);
    expect(tpmLookProblems(afterAbsent, { outcome: "permanent", answers: ["absent"] })).toEqual([]);
    expect(tpmLookProblems(afterAbsent, { outcome: "permanent", answers: [] }).join()).toContain('look 1 answered "absent"');
  });

  test("another gateway's TPM: the keyed answers are not a wipe, and a wipe after them is caught", () => {
    const keyed = "keyed 0x81000100,0x81010001";
    const kept = journal(every(windowS / intervalS + 1, keyed), GAVE_UP.replace("last: absent", `last: ${keyed}`));
    expect(parseTpmLooks(kept)[0]?.answer).toBe(keyed);
    expect(tpmLookProblems(kept, { outcome: "gave-up", answers: [keyed] })).toEqual([]);
    expect(tpmLookProblems(kept, { outcome: "gave-up", answers: ["keyed 0x81000100"] }).join()).toContain(`look 1 answered "${keyed}"`);
    // Keyed answers can never be the deciding ones of a wipe.
    expect(tpmLookProblems(journal(every(sameAnswers, keyed), WIPED), { outcome: "permanent", answers: [] }).join()).toContain("expected the same other owner hierarchy");
  });
});

describe("power cuts in the first boot's setup", () => {
  const script = readFileSync(join(REPO, "edge/image/boot-test/power-cut.sh"), "utf8");
  const tpmToken = { type: "systemd-tpm2", keyslots: ["1"], "tpm2-pcrs": [] };
  const blkid = (label: string) => `DEVNAME=/x/data-part.img\nVERSION=2\nUUID=${LUKS.uuid}\nLABEL=${label}\nTYPE=crypto_LUKS\nUSAGE=crypto\n`;
  // The setup's owner fingerprint is in every header it makes, at its own id; systemd's token takes id 0.
  const json = (keyslots: string[], tokens: Record<string, object>, owner = true) =>
    JSON.stringify({ keyslots: Object.fromEntries(keyslots.map((k) => [k, { type: "luks2" }])), tokens: { ...(owner ? { "1": ownerToken(OWNER_A) } : {}), ...tokens } });

  test("the power cut stands in for the programs of the setup's steps, and knows every step the test cuts at", () => {
    expect([...powerCutPrograms(script).keys()].sort()).toEqual(["cryptsetup", "dd", "mke2fs", "systemd-cryptenroll"]);
    for (const step of [...POWER_CUTS, ...RESTART_CUTS, ...WIPE_CUTS].map((c) => c.step)) {
      expect(script).toMatch(new RegExp(`^#   ${step} `, "m"));
      // Set from the program and its arguments, or cut around the relabel by name.
      expect(script).toMatch(new RegExp(`(step=${step} ;;|power_cut ${step}$)`, "m"));
    }
    const setup = readFileSync(join(REPO, "edge/image/files/data/trustiot-data-setup"), "utf8");
    for (const program of powerCutPrograms(script).keys()) expect(setup).toMatch(new RegExp(`^\\s+${program} `, "m"));
  });

  test("the power cut tells the setup's two header writes apart by their arguments", () => {
    // The dd lines of format(), as power-cut.sh's case patterns see them (" $* ").
    const setup = readFileSync(join(REPO, "edge/image/files/data/trustiot-data-setup"), "utf8");
    // A line ending in a backslash continues on the next one.
    const dds = [...setup.matchAll(/^\tdd ((?:.*\\\n)*.*)$/gm)].map((m) => ` ${m[1]?.replace(/\s*\\\n\s*/g, " ")} `);
    expect(dds).toHaveLength(2);
    expect(dds[0]).toContain(" count=1 ");
    expect(dds[0]).not.toContain(" seek=1 ");
    expect(dds[1]).toContain(" seek=1 ");
    expect(dds[1]).not.toContain(" count=1 ");
    // The second one's count is an arithmetic expansion, never the literal count=1.
    expect(dds[1]).toContain(' count="$((HEADER_BYTES / (2 * METADATA_BYTES) - 1))" ');
    expect(script).toContain('*" count=1 "*) step=write-header ;;');
    expect(script).toContain('*" seek=1 "*) step=write-keyslots ;;');
  });

  test("the first boot's cuts follow the setup's steps, and the restarted setup's cuts end where the first boot's take over", () => {
    expect(POWER_CUTS.map((c) => c.step)).toEqual(["luks-format", "write-header", "write-keyslots", "tpm-enroll", "mkfs", "kill-slot", "before-relabel", "after-relabel"]);
    expect(RESTART_CUTS.map((c) => c.step)).toEqual(POWER_CUTS.slice(0, 3).map((c) => c.step));
    // A cut right after luksFormat leaves the partition as it was: zeros on the first boot, the
    // earlier attempt (the state a cut at mkfs leaves) in a restarted setup.
    expect(POWER_CUTS[0]?.state).toBe("zeros");
    expect(RESTART_CUTS[0]?.state).toEqual(POWER_CUTS.find((c) => c.step === "mkfs")?.state);
    // From the first write on, the partition holds the new header: what the next boot starts over from.
    for (const cut of [...POWER_CUTS.slice(1, 3), ...RESTART_CUTS.slice(1)]) expect(cut.state).toEqual({ label: LUKS.labelNew, keyslots: ["0"], tpmToken: false });
    expect(POWER_CUTS[0]?.nextLogs).toContain("first boot");
    for (const cut of [...POWER_CUTS.slice(1, -1), ...RESTART_CUTS]) expect(cut.nextLogs).toContain("starting again");
  });

  test("the scripts the test copy runs are valid bash", async () => {
    for (const file of ["power-cut.sh", "probe.sh", "tpm-late.sh", "classify.sh"]) {
      const child = Bun.spawn(["bash", "-n", join(REPO, "edge/image/boot-test", file)], { stderr: "pipe" });
      expect(await child.exited).toBe(0);
    }
  });

  const seen = (blkidText: string, header: string | null, allZero = false) => ({ blkid: blkidText, json: header, allZero });
  const stateOf = (step: string) => {
    const state = POWER_CUTS.find((c) => c.step === step)?.state;
    if (!state) throw new Error(`no ${step} cut`);
    return state;
  };

  test("the state after a cut passes when label, keyslots and token match, or when the partition is still zeros", () => {
    expect(cutStateProblems(stateOf("tpm-enroll"), seen(blkid(LUKS.labelNew), json(["0", "1"], { "0": tpmToken })))).toEqual([]);
    expect(cutStateProblems(stateOf("write-header"), seen(blkid(LUKS.labelNew), json(["0"], {})))).toEqual([]);
    expect(cutStateProblems(stateOf("luks-format"), seen("", null, true))).toEqual([]);
  });

  test("the state after a cut fails on another label, keyslot set or token, and on a header that is not JSON", () => {
    const killed = stateOf("kill-slot");
    expect(cutStateProblems(killed, seen(blkid(LUKS.label), json(["1"], { "0": tpmToken }))).join()).toContain(`no LABEL=${LUKS.labelNew}`);
    expect(cutStateProblems(killed, seen(blkid(LUKS.labelNew), json(["0", "1"], { "0": tpmToken }))).join()).toContain('keyslots ["0","1"], expected ["1"]');
    expect(cutStateProblems(killed, seen(blkid(LUKS.labelNew), json(["1"], {}))).join()).toContain('tokens ["1 trustiot-tpm-owner []"], expected ["0 systemd-tpm2 [\\"1\\"]","1 trustiot-tpm-owner []"]');
    expect(cutStateProblems(killed, seen(blkid(LUKS.labelNew), json(["1"], { "0": { ...tpmToken, keyslots: ["0"] } }))).join()).toContain('"0 systemd-tpm2 [\\"0\\"]"');
    // Every header the setup makes has its owner fingerprint, from the first write on.
    expect(cutStateProblems(killed, seen(blkid(LUKS.labelNew), json(["1"], { "0": tpmToken }, false))).join()).toContain('tokens ["0 systemd-tpm2 [\\"1\\"]"], expected');
    expect(cutStateProblems(killed, seen(blkid(LUKS.labelNew), "{")).join()).toContain("could not be read");
    expect(cutStateProblems(killed, seen("", json(["1"], { "0": tpmToken }))).join()).toContain("no TYPE=crypto_LUKS");
    // blkid finds no volume (the partition is not zeros either): what the old setup left after
    // zeroing the header in front of an earlier attempt (Laysi/trustiot-v2#917).
    expect(cutStateProblems(stateOf("write-header"), seen("", null)).join()).toContain("no LUKS2 header to read");
  });

  test("a cut that should leave the partition untouched fails when anything was written", () => {
    expect(cutStateProblems("zeros", seen(blkid(LUKS.labelNew), json(["0"], {}))).join()).toContain("not all zeros");
    expect(cutStateProblems("zeros", seen("", null, false)).join()).toContain("not all zeros");
  });

  test("cut states in words", () => {
    expect(describeCutState("zeros")).toBe("all zeros");
    expect(describeCutState(stateOf("tpm-enroll"))).toBe(`label ${LUKS.labelNew}, keyslots 0 and 1, the TPM's token`);
    expect(describeCutState(WIPE_CUTS.at(-1)?.state ?? "zeros")).toBe(`label ${LUKS.label}, keyslots 1, the TPM's token, the mark of a wipe`);
  });

  test("a wipe's cuts: before its first write the old volume is unchanged, from then on the mark is in every header", () => {
    expect(WIPE_CUTS.map((c) => c.step)).toEqual(["luks-format", "write-header", "after-relabel"]);
    // luksFormat writes only /run: the next boot looks again and wipes.
    expect(WIPE_CUTS[0]?.state).toEqual(POWER_CUTS.at(-1)?.state);
    expect(WIPE_CUTS[0]?.nextLogs).toBe(WIPED);
    // The first write carries the mark; a setup that starts again keeps it.
    expect(WIPE_CUTS[1]?.state).toEqual({ ...(stateOf("write-header") as LuksCutState), mark: true });
    expect(WIPE_CUTS[1]?.nextLogs).toContain("starting again");
    expect(WIPE_CUTS[2]?.state).toEqual({ ...(stateOf("after-relabel") as LuksCutState), mark: true });
    const mark = markToken(OWNER_A, OWNER_B);
    expect(cutStateProblems(WIPE_CUTS[1]?.state ?? "zeros", seen(blkid(LUKS.labelNew), json(["0"], { "2": mark })))).toEqual([]);
    expect(cutStateProblems(WIPE_CUTS[2]?.state ?? "zeros", seen(blkid(LUKS.label), json(["1"], { "0": tpmToken, "2": mark })))).toEqual([]);
    // The mark lost, or there before the wipe wrote anything.
    expect(cutStateProblems(WIPE_CUTS[2]?.state ?? "zeros", seen(blkid(LUKS.label), json(["1"], { "0": tpmToken }))).join()).toContain("2 trustiot-back-to-factory []");
    expect(cutStateProblems(WIPE_CUTS[0]?.state ?? "zeros", seen(blkid(LUKS.label), json(["1"], { "0": tpmToken, "2": mark }))).join()).toContain("2 trustiot-back-to-factory []");
  });
});

describe("the late TPM (boot-test/tpm-late.sh)", () => {
  const unit = readFileSync(join(REPO, "edge/image/boot-test/trustiot-boot-test-tpm-late.service"), "utf8");
  const script = readFileSync(join(REPO, "edge/image/boot-test/tpm-late.sh"), "utf8");

  test("runs early, only when the kernel command line asks for it, and loads the driver by name", () => {
    const lines = unit.split("\n");
    for (const line of ["DefaultDependencies=no", "ConditionKernelCommandLine=trustiot_test.tpm-late", "Type=exec", "ExecStart=/usr/local/sbin/trustiot-boot-test-tpm-late", "WantedBy=sysinit.target"]) {
      expect(lines).toContain(line);
    }
    expect(script).toContain("trustiot_test\\.tpm-late=");
    expect(script).toContain("exec modprobe tpm_tis");
  });
});
