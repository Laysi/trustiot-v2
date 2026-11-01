// The boot test's evaluation (src/boot-results.ts): what each boot's probe output must show for the
// data partition to count as unlocked or as locked (fail closed).
import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DEVICES, POWER_CUTS, UNITS, bootProblems, cutStateProblems, parseProbe, parseShow, powerCutPrograms, redactTpmFields, unitSeconds } from "../src/boot-results.ts";
import { REPO } from "../src/host.ts";
import { DATA, LUKS } from "../src/pins.ts";

const header = JSON.stringify({
  keyslots: { "1": { type: "luks2" } },
  tokens: { "0": { type: "systemd-tpm2", keyslots: ["1"], "tpm2-pcrs": [] } },
  segments: { "0": { type: "crypt", encryption: LUKS.cipher, sector_size: LUKS.sectorBytes } },
});

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

describe("power cuts in the first boot's setup", () => {
  const script = readFileSync(join(REPO, "edge/image/boot-test/power-cut.sh"), "utf8");
  const tpmToken = { type: "systemd-tpm2", keyslots: ["1"], "tpm2-pcrs": [] };
  const blkid = (label: string) => `DEVNAME=/x/data-part.img\nVERSION=2\nUUID=${LUKS.uuid}\nLABEL=${label}\nTYPE=crypto_LUKS\nUSAGE=crypto\n`;
  const json = (keyslots: string[], tokens: object[]) => JSON.stringify({ keyslots: Object.fromEntries(keyslots.map((k) => [k, { type: "luks2" }])), tokens: Object.fromEntries(tokens.map((t, i) => [String(i), t])) });

  test("the power cut stands in for the programs of the setup's steps, and knows every step the test cuts at", () => {
    expect([...powerCutPrograms(script).keys()].sort()).toEqual(["cryptsetup", "dd", "mke2fs", "systemd-cryptenroll"]);
    for (const step of [...POWER_CUTS.map((c) => c.step), "zero-header"]) expect(script).toMatch(new RegExp(`^#   ${step} `, "m"));
    const setup = readFileSync(join(REPO, "edge/image/files/data/trustiot-data-setup"), "utf8");
    for (const program of powerCutPrograms(script).keys()) expect(setup).toMatch(new RegExp(`^\\s+${program} `, "m"));
  });

  test("both scripts the test copy runs are valid bash", async () => {
    for (const file of ["power-cut.sh", "probe.sh"]) {
      const child = Bun.spawn(["bash", "-n", join(REPO, "edge/image/boot-test", file)], { stderr: "pipe" });
      expect(await child.exited).toBe(0);
    }
  });

  test("the state after a cut passes when label, keyslots and token match", () => {
    const enrolled = POWER_CUTS.find((c) => c.step === "tpm-enroll")?.state;
    if (!enrolled) throw new Error("no tpm-enroll cut");
    expect(cutStateProblems(enrolled, blkid(LUKS.labelNew), json(["0", "1"], [tpmToken]))).toEqual([]);
    const formatted = POWER_CUTS[0]?.state;
    if (!formatted) throw new Error("no cuts");
    expect(cutStateProblems(formatted, blkid(LUKS.labelNew), json(["0"], []))).toEqual([]);
  });

  test("the state after a cut fails on another label, keyslot set or token, and on a header that is not JSON", () => {
    const killed = POWER_CUTS.find((c) => c.step === "kill-slot")?.state;
    if (!killed) throw new Error("no kill-slot cut");
    expect(cutStateProblems(killed, blkid(LUKS.label), json(["1"], [tpmToken])).join()).toContain(`no LABEL=${LUKS.labelNew}`);
    expect(cutStateProblems(killed, blkid(LUKS.labelNew), json(["0", "1"], [tpmToken])).join()).toContain('keyslots ["0","1"], expected ["1"]');
    expect(cutStateProblems(killed, blkid(LUKS.labelNew), json(["1"], [])).join()).toContain("expected one systemd-tpm2 token on keyslot 1");
    expect(cutStateProblems(killed, blkid(LUKS.labelNew), json(["1"], [{ ...tpmToken, keyslots: ["0"] }])).join()).toContain("expected one systemd-tpm2 token");
    expect(cutStateProblems(killed, blkid(LUKS.labelNew), "{").join()).toContain("could not be read");
    expect(cutStateProblems(killed, "", json(["1"], [tpmToken])).join()).toContain("no TYPE=crypto_LUKS");
  });
});
