// The boot test's evaluation (src/boot-results.ts): what each boot's probe output must show for the
// data partition to count as unlocked or as locked (fail closed).
import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DEVICES, UNITS, bootProblems, parseProbe, parseShow, redactTpmFields, unitSeconds } from "../src/boot-results.ts";
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
    const written = new Set([...script.matchAll(/^section ([a-z-]+) /gm)].map((m) => m[1]));
    for (const name of new Set([...parseProbe(unlocked).sections.keys(), ...parseProbe(locked).sections.keys(), "write-marker"])) {
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
