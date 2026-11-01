// The data partition's encryption (Laysi/trustiot-v2#810): files/data agrees with the pins and the
// partition table, and the parsers the image check and the boot test read results with.
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import {
  ATTACH_OPTIONS,
  CRYPTTAB_OPTIONS,
  crypttabProblems,
  deviceTimeoutProblems,
  deviceUnit,
  effectiveSetting,
  generatedUnitProblems,
  luksHeaderProblems,
  parseCrypttab,
  parseShellAssignments,
  parseEnrollList,
  scriptConstants,
  syslogDaemons,
} from "../src/data.ts";
import { dataFstabLine, parseFstab } from "../src/files.ts";
import { REPO } from "../src/host.ts";
import { targetTable } from "../src/layout.ts";
import { DATA, DATA_FILES, IMAGE_PACKAGES, LUKS } from "../src/pins.ts";

const FILES = join(REPO, "edge/image/files/data");
const read = (name: string) => readFileSync(join(FILES, name), "utf8");
const script = read("trustiot-data-setup");
const setupUnit = read("trustiot-data-setup.service");
const cryptsetupDropIn = read("systemd-cryptsetup-trustiot-data.conf");
const agentDropIn = read("trustiot-agent-data.conf");

function lines(text: string): string[] {
  return text.split("\n").filter((l) => l !== "" && !l.startsWith("#"));
}

describe("files/data", () => {
  test("every file is installed, at a fixed place", () => {
    expect(readdirSync(FILES).toSorted()).toEqual(DATA_FILES.map((f) => f.repo).toSorted());
    for (const file of DATA_FILES) {
      expect(file.image).toMatch(/^\/(usr\/(libexec\/trustiot|lib\/systemd\/system(\/[a-z0-9_.@-]+\.d)?)\/[a-z0-9_.-]+|etc\/crypttab|etc\/systemd\/journald\.conf\.d\/[a-z0-9.-]+|etc\/rpi\/swap\.conf\.d\/[a-z0-9.-]+)$/);
      expect(file.mode).toBe(file.image.startsWith("/usr/libexec/") ? 0o755 : 0o644);
    }
  });

  test("the setup script uses the pinned volume and the partition the table gives it", () => {
    const data = targetTable().partitions.find((p) => p.number === DATA.number);
    expect(Object.fromEntries(scriptConstants(script))).toEqual({
      PART_NUMBER: String(DATA.number),
      PART_START: String(data?.start),
      PART_SECTORS: String(DATA.sizeSectors),
      LUKS_UUID: LUKS.uuid,
      LABEL_NEW: LUKS.labelNew,
      LABEL_READY: LUKS.label,
      FS_LABEL: LUKS.fsLabel,
      AGENT_USER: "trustiot-agent",
      HEADER_BYTES: String(LUKS.headerBytes),
      METADATA_BYTES: String(LUKS.metadataBytes),
      SETUP_MAPPING: `${LUKS.mapping}_setup`,
    });
    expect(script).toContain(`--cipher ${LUKS.cipher} --key-size ${LUKS.keyBits}`);
    expect(script).toContain(`--sector-size ${LUKS.sectorBytes}`);
    // The data partition leaves room for the header and more than the agent's buffer behind it.
    expect(LUKS.headerBytes % LUKS.sectorBytes).toBe(0);
    expect(DATA.sizeSectors * 512 - LUKS.headerBytes).toBeGreaterThan(1800 * 1024 * 1024);
  });

  // Laysi/trustiot-v2#917: a power cut at any point of format() must leave the partition as it was
  // (all zeros, or the unfinished setup's LABEL_NEW header) or with the new LABEL_NEW header; never
  // a partition without a header that is not all zeros, which main() refuses at every boot.
  test("the setup makes the new header on /run and replaces the old one with it, never with zeros", () => {
    const body = /^format\(\) \{\n([\s\S]*?)\n\}\n/m.exec(script)?.[1];
    if (!body) throw new Error("no format() in the setup script");
    // Nothing writes zeros to the partition, and luksFormat never gets it: its device is the
    // sparse stand-in on /run, its header the file it then copies.
    expect(body).not.toContain("/dev/zero");
    const luksFormat = /cryptsetup luksFormat ((?:.*\\\n)*.*)$/m.exec(body)?.[1]?.replace(/\\\n\s*/g, "");
    if (!luksFormat) throw new Error("no luksFormat in format()");
    expect(luksFormat.trim().split(/\s+/).at(-1)).toBe('"$stand_in"');
    expect(luksFormat).toContain('--header "$header" --offset "$((HEADER_BYTES / 512))"');
    expect(luksFormat).toContain('--luks2-metadata-size "$METADATA_BYTES" --luks2-keyslots-size "$((HEADER_BYTES - 2 * METADATA_BYTES))"');
    expect(luksFormat).not.toContain("$dev");
    expect(body).toContain("work=$(mktemp -d /run/");
    expect(body).toContain("header=$work/header");
    expect(body).toContain("stand_in=$work/stand-in");
    expect(body).toContain('truncate --size="$((PART_SECTORS * 512))" "$stand_in"');

    // In order: luksFormat into the file; the file is a whole LABEL_NEW header of this volume;
    // its two metadata copies go to the partition, written through; then its keyslot area; and
    // only then anything else touches the partition.
    const at = (text: string) => {
      const index = body.indexOf(text);
      if (index < 0) throw new Error(`format() has no ${text}`);
      return index;
    };
    const order = [
      at("cryptsetup luksFormat"),
      at('[[ $(stat -c %s "$header") == "$HEADER_BYTES" ]] || die'),
      at('[[ $(probe "$header" TYPE) == crypto_LUKS && $(probe "$header" UUID) == "$LUKS_UUID" && $(probe "$header" LABEL) == "$LABEL_NEW" ]] ||'),
      at('dd if="$header" of="$dev" bs="$((2 * METADATA_BYTES))" count=1 conv=fsync status=none'),
      at('dd if="$header" of="$dev" bs="$((2 * METADATA_BYTES))" skip=1 seek=1 conv=fsync status=none'),
      at("systemd-cryptenroll"),
    ];
    expect(order).toEqual(order.toSorted((a, b) => a - b));
    // The two dd lines are the only writes before the TPM's enrollment, and nothing before the
    // first one names the partition except as dd's output.
    const beforeEnroll = body.slice(0, at("systemd-cryptenroll"));
    expect([...beforeEnroll.matchAll(/"\$dev"/g)]).toHaveLength(2);
    expect(beforeEnroll.match(/^\s*dd /gm)).toHaveLength(2);
    // Every byte of the header file is written: the copies (2 * METADATA_BYTES in one block), then
    // the rest from that block on.
    expect((LUKS.headerBytes / (2 * LUKS.metadataBytes)) % 1).toBe(0);
  });

  test("the setup script never leaves a password: random key, TPM without PCRs, key slot wiped", () => {
    expect(script).toStartWith("#!/bin/bash\n");
    expect(script).toContain("set -euo pipefail");
    // The key comes from the kernel's random source and is never printed or stored on disk.
    expect(script).toContain('head -c 48 /dev/random | base64 -w 0 >"$key"');
    expect(script).toContain("mktemp -d /run/");
    // Explicitly no PCRs (systemd binds PCR 7 by default), no PIN, no signed or pcrlock policy.
    expect(script).toContain("--tpm2-device=auto --tpm2-pcrs= \\\n\t\t--tpm2-public-key= --tpm2-pcrlock= --tpm2-with-pin=no");
    expect(script).toContain('cryptsetup luksKillSlot --batch-mode "$dev" 0');
    for (const forbidden of ["--recovery-key", "--password", "--unlock-fido2", "ask-password", "--new-keyfile", "luksAddKey", "x-systemd.makefs"]) {
      expect(script).not.toContain(forbidden);
    }
    // Only a partition that is empty or holds an unfinished setup (LABEL_NEW) is ever formatted.
    const formats = [...script.matchAll(/^\t\tformat "\$dev"$/gm)];
    expect(formats).toHaveLength(2);
  });

  test("the script is valid bash", async () => {
    const child = Bun.spawn(["bash", "-n", join(FILES, "trustiot-data-setup")], { stderr: "pipe" });
    expect(await child.exited).toBe(0);
  });

  test("crypttab: the pinned volume by UUID, unlocked by the TPM only, never a prompt", () => {
    expect(crypttabProblems(read("crypttab"))).toEqual([]);
    expect(parseCrypttab(read("crypttab"))).toEqual([
      { name: LUKS.mapping, device: `UUID=${LUKS.uuid}`, key: "none", options: [...CRYPTTAB_OPTIONS] },
    ]);
  });

  test("crypttab problems are found", () => {
    const good = `${LUKS.mapping} UUID=${LUKS.uuid} none ${CRYPTTAB_OPTIONS.join(",")}`;
    expect(crypttabProblems(good)).toEqual([]);
    expect(crypttabProblems(good.replace("headless=true,", ""))).toHaveLength(1);
    expect(crypttabProblems(good.replace(" none ", " /root/key ")).join()).toContain("key file");
    expect(crypttabProblems(good.replace(`UUID=${LUKS.uuid}`, "/dev/mmcblk0p3")).join()).toContain("expected UUID=");
    expect(crypttabProblems(`${good}\nother /dev/sda1 none luks`).join()).toContain("2 entries");
    expect(crypttabProblems("").join()).toContain(`no ${LUKS.mapping}`);
  });

  test("fstab mounts the volume crypttab opens", () => {
    expect(parseFstab(dataFstabLine())[0]?.spec).toBe(`/dev/mapper/${parseCrypttab(read("crypttab"))[0]?.name}`);
  });

  test("the volume and the unlocked volume wait for the first boot's setup, with a bound", () => {
    const option = `x-systemd.device-timeout=${DATA.deviceTimeout}`;
    expect(parseCrypttab(read("crypttab"))[0]?.options).toContain(option);
    expect(parseFstab(dataFstabLine())[0]?.options).toContain(option);
    // Longer than the setup's own limit plus systemd's default wait for the TPM (90 s).
    const minutes = (text: string) => {
      const m = /^(\d+)min$/.exec(text);
      if (!m) throw new Error(`not whole minutes: ${text}`);
      return Number(m[1]);
    };
    const setupLimit = lines(setupUnit).find((l) => l.startsWith("TimeoutStartSec="))?.slice("TimeoutStartSec=".length) ?? "";
    expect(minutes(DATA.deviceTimeout) * 60).toBeGreaterThan(minutes(setupLimit) * 60 + 90);
    expect(ATTACH_OPTIONS).toEqual(["luks", "tpm2-device=auto", "headless=true", "nofail"]);
  });

  test("device unit names and their time limits", () => {
    expect(deviceUnit(`/dev/disk/by-uuid/${LUKS.uuid}`)).toBe("dev-disk-by\\x2duuid-9da7f680\\x2d5912\\x2d4d5c\\x2dbca6\\x2dd46c83e064f8.device");
    expect(deviceUnit(`/dev/mapper/${LUKS.mapping}`)).toBe("dev-mapper-trustiot_data.device");
    expect(() => deviceUnit("/dev/disk/by-label/a b")).toThrow("no simple device path");
    const dropIn = "# Automatically generated by systemd-cryptsetup-generator\n\n[Unit]\nJobRunningTimeoutSec=15min\n";
    expect(deviceTimeoutProblems("x.device", dropIn)).toEqual([]);
    expect(deviceTimeoutProblems("x.device", `${dropIn}\n[Unit]\nJobRunningTimeoutSec=90s\n`)).toEqual(["x.device has JobRunningTimeoutSec=90s, expected 15min"]);
    expect(deviceTimeoutProblems("x.device", "")).toEqual(["x.device has JobRunningTimeoutSec=(default), expected 15min"]);
  });

  test("the unlock waits for the setup and the TPM, with a bound, and keeps no core dump", () => {
    const unit = lines(cryptsetupDropIn);
    for (const line of ["Requires=trustiot-data-setup.service", "After=trustiot-data-setup.service", "Wants=dev-tpmrm0.device", "After=dev-tpmrm0.device", "LimitCORE=0"]) {
      expect(unit).toContain(line);
    }
    const timeout = unit.find((l) => l.startsWith("TimeoutStartSec="));
    expect(timeout).toBe("TimeoutStartSec=2min");
    // Its file name is the generated unit's name.
    const file = DATA_FILES.find((f) => f.repo === "systemd-cryptsetup-trustiot-data.conf");
    expect(file?.image).toBe(`/usr/lib/systemd/system/systemd-cryptsetup@${LUKS.mapping}.service.d/trustiot-data.conf`);
  });

  test("the setup runs before the unlock, on its own, without default dependencies", () => {
    const unit = lines(setupUnit);
    for (const line of [
      "DefaultDependencies=no",
      "Wants=dev-tpmrm0.device",
      "After=dev-tpmrm0.device",
      `Before=systemd-cryptsetup@${LUKS.mapping}.service`,
      "Type=oneshot",
      "RemainAfterExit=yes",
      "ExecStart=/usr/libexec/trustiot/trustiot-data-setup",
      "LimitCORE=0",
    ]) {
      expect(unit).toContain(line);
    }
    // Pulled in only by the unlock that requires it: no [Install] section.
    expect(setupUnit).not.toContain("[Install]");
  });

  test("the agent requires the mount and refuses an unmounted state directory", () => {
    const unit = lines(agentDropIn);
    expect(unit).toContain(`RequiresMountsFor=${DATA.mountPoint}`);
    expect(unit).toContain(`AssertPathIsMountPoint=${DATA.mountPoint}`);
    expect(unit).toContain("LimitCORE=0");
  });

  test("the journal stays in memory", () => {
    // Every drop-in the source image has, in the order journald applies them (file name order).
    const text = `# /usr/lib/systemd/journald.conf.d/40-rpi-volatile-storage.conf\n[Journal]\nStorage=persistent\n# /usr/lib/systemd/journald.conf.d/syslog.conf\n[Journal]\nForwardToSyslog=yes\n# /etc/systemd/journald.conf.d/zz-trustiot.conf\n${read("journald.conf")}`;
    expect(effectiveSetting(text, "Journal", "Storage")).toBe("volatile");
    expect(effectiveSetting(text, "Journal", "ForwardToSyslog")).toBe("no");
    // Drop-ins from every directory apply in file name order: ours must come after both vendor ones.
    const name = DATA_FILES.find((f) => f.repo === "journald.conf")?.image.split("/").at(-1) ?? "";
    expect([name, "40-rpi-volatile-storage.conf", "syslog.conf"].toSorted().at(-1)).toBe(name);
  });

  test("swap goes to compressed memory only, never to a file on the root partition", () => {
    const text = `[Main]\n#Mechanism=auto\n[File]\n#Path=/var/swap\n${read("rpi-swap.conf")}`;
    expect(effectiveSetting(text, "Main", "Mechanism")).toBe("zram");
  });

  test("the packages the setup and the unlock run are installed", () => {
    expect(Object.keys(IMAGE_PACKAGES)).toEqual(expect.arrayContaining(["cryptsetup-bin", "systemd-cryptsetup", "tpm2-tools"]));
  });
});

describe("parsers", () => {
  test("effective setting: the last one in the section wins", () => {
    const text = "[Journal]\nStorage=auto\n[Other]\nStorage=persistent\n# /etc/x.conf\n[Journal]\n#Storage=none\nStorage=volatile\n";
    expect(effectiveSetting(text, "Journal", "Storage")).toBe("volatile");
    expect(effectiveSetting(text, "Journal", "Compress")).toBeUndefined();
  });

  test("systemd-cryptenroll's keyslot list", () => {
    expect(parseEnrollList("SLOT TYPE\n   1 tpm2\n")).toEqual([{ slot: 1, type: "tpm2" }]);
    expect(parseEnrollList("SLOT TYPE\n   0 password\n   1 tpm2\n")).toEqual([
      { slot: 0, type: "password" },
      { slot: 1, type: "tpm2" },
    ]);
    expect(parseEnrollList("SLOT TYPE\n")).toEqual([]);
    expect(() => parseEnrollList("No slots\n")).toThrow("no SLOT TYPE header");
  });

  test("the generated unit attaches the pinned UUID with the crypttab options", () => {
    const unit =
      "[Unit]\nDefaultDependencies=no\n[Service]\n" +
      `ExecStart=/usr/bin/systemd-cryptsetup attach '${LUKS.mapping}' '/dev/disk/by-uuid/${LUKS.uuid}' 'none' '${ATTACH_OPTIONS.join(",")}'\n`;
    expect(generatedUnitProblems(unit)).toEqual([]);
    expect(generatedUnitProblems(unit.replace("headless=true,", ""))).toHaveLength(1);
    expect(generatedUnitProblems(unit.replace("DefaultDependencies=no\n", ""))).toHaveLength(1);
  });

  test("rpi-systemd-config's shell assignments", () => {
    expect(parseShellAssignments("CONF_MECHANISM=zram\nCONF_SWAPFILE='/var/swap'\n")).toEqual(
      new Map([
        ["CONF_MECHANISM", "zram"],
        ["CONF_SWAPFILE", "/var/swap"],
      ]),
    );
    expect(parseShellAssignments("")).toEqual(new Map());
    expect(() => parseShellAssignments("echo hi\n")).toThrow("unexpected line");
  });

  test("a LUKS2 header the setup leaves passes, and every other shape is found", () => {
    // The shape systemd-cryptenroll 257 and cryptsetup 2.7.5 wrote on the build host (2026-10-31).
    const good = {
      keyslots: { "1": { type: "luks2", key_size: 64, kdf: { type: "pbkdf2", hash: "sha512", iterations: 1000 } } },
      tokens: {
        "0": {
          type: "systemd-tpm2",
          keyslots: ["1"],
          "tpm2-blob": "AA==",
          "tpm2-pcrs": [],
          "tpm2-pcr-bank": "sha256",
          "tpm2-primary-alg": "ecc",
          "tpm2-policy-hash": "00",
          tpm2_srk: "AA==",
        },
      },
      segments: { "0": { type: "crypt", offset: "16777216", size: "dynamic", iv_tweak: "0", encryption: LUKS.cipher, sector_size: LUKS.sectorBytes } },
      digests: {},
      config: { json_size: "12288", keyslots_size: "16744448", label: LUKS.label },
    };
    const json = (change: (h: typeof good) => unknown) => {
      const copy = structuredClone(good);
      change(copy);
      return JSON.stringify(copy);
    };
    expect(luksHeaderProblems(JSON.stringify(good))).toEqual([]);
    // A second keyslot, such as the setup's random key left behind or a password added later.
    expect(luksHeaderProblems(json((h) => Object.assign(h.keyslots, { "0": { type: "luks2" } }))).join()).toContain("2 keyslots");
    expect(luksHeaderProblems(json((h) => Object.assign(h.tokens, { "1": { type: "systemd-recovery", keyslots: ["2"] } }))).join()).toContain("2 tokens");
    expect(luksHeaderProblems(json((h) => { h.tokens["0"].type = "systemd-fido2"; })).join()).toContain("expected systemd-tpm2");
    expect(luksHeaderProblems(json((h) => { h.tokens["0"]["tpm2-pcrs"] = [7] as never; })).join()).toContain("PCRs [7]");
    expect(luksHeaderProblems(json((h) => Object.assign(h.tokens["0"], { "tpm2-pin": true }))).join()).toContain("PIN");
    expect(luksHeaderProblems(json((h) => Object.assign(h.tokens["0"], { "tpm2-pcrlock": true }))).join()).toContain("pcrlock");
    expect(luksHeaderProblems(json((h) => { h.tokens["0"].keyslots = ["2"]; })).join()).toContain("uses keyslots");
    expect(luksHeaderProblems(json((h) => { (h.segments["0"] as { sector_size: number }).sector_size = 512; })).join()).toContain("4096-byte sectors");
    expect(luksHeaderProblems(json((h) => { h.keyslots = {} as never; })).join()).toContain("0 keyslots");
    // The layout the setup writes: data at 16 MiB, 16 KiB metadata copies, the keyslot area between.
    expect(luksHeaderProblems(json((h) => { h.segments["0"].offset = "33554432"; })).join()).toContain("the data starts at byte 33554432, expected 16777216");
    expect(luksHeaderProblems(json((h) => { h.config.json_size = "61440"; })).join()).toContain("header areas");
    expect(luksHeaderProblems(json((h) => { h.config.keyslots_size = "33521664"; })).join()).toContain('"keyslots_size":"16744448"');
  });

  test("syslog daemons among the installed packages", () => {
    const listing = [
      "rsyslog ii  system-log-daemon, linux-kernel-log-daemon",
      "systemd ii  systemd-sysusers, systemd-tmpfiles",
      "syslog-ng-core un  system-log-daemon",
      "other-logger ii  system-log-daemon",
    ].join("\n");
    expect(syslogDaemons(listing)).toEqual(["rsyslog", "other-logger"]);
    expect(syslogDaemons("systemd ii  \n")).toEqual([]);
  });
});
