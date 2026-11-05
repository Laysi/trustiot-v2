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
  markToken,
  ownerToken,
  parseCrypttab,
  parseShellAssignments,
  parseEnrollList,
  scriptConstants,
  syslogDaemons,
} from "../src/data.ts";
import { dataFstabLine, parseFstab } from "../src/files.ts";
import { REPO } from "../src/host.ts";
import { targetTable } from "../src/layout.ts";
import { DATA, DATA_CHECK, DATA_FILES, IMAGE_PACKAGES, LUKS } from "../src/pins.ts";

const FILES = join(REPO, "edge/image/files/data");
const read = (name: string) => readFileSync(join(FILES, name), "utf8");
const script = read("trustiot-data-setup");
const setupUnit = read("trustiot-data-setup.service");
const cryptsetupDropIn = read("systemd-cryptsetup-trustiot-data.conf");
/** Owner fingerprints (TPM 2.0 names with SHA-256) of two TPMs, made up. */
const OWNER_A = `000b${"a1".repeat(32)}`;
const OWNER_B = `000b${"b2".repeat(32)}`;
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
      OWNER_TOKEN_ID: LUKS.ownerToken.id,
      OWNER_TOKEN_TYPE: LUKS.ownerToken.type,
      MARK_TOKEN_ID: LUKS.markToken.id,
      MARK_TOKEN_TYPE: LUKS.markToken.type,
      FACTORY_FLAG: DATA_CHECK.factoryFlag,
      CHECK_INTERVAL_S: String(DATA_CHECK.intervalS),
      CHECK_WINDOW_S: String(DATA_CHECK.windowS),
      CHECK_SAME_ANSWERS: String(DATA_CHECK.sameAnswers),
      TPM_COMMAND_LIMIT_S: "60",
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
    // The setup's own tokens (Laysi/trustiot-v2#867) go into the header file too, so the first
    // write already carries them: the fingerprint is made before luksFormat, and a wipe's mark
    // never reaches the partition without the header it belongs to.
    const order = [
      at('fingerprint=$(owner_fingerprint "$work") || die'),
      at("cryptsetup luksFormat"),
      at('cryptsetup token import --token-id "$OWNER_TOKEN_ID" --json-file "$work/owner.json" "$header"'),
      at('cryptsetup token import --token-id "$MARK_TOKEN_ID" --json-file "$work/mark.json" "$header"'),
      at('[[ $(stat -c %s "$header") == "$HEADER_BYTES" ]] || die'),
      at('[[ $(probe "$header" TYPE) == crypto_LUKS && $(probe "$header" UUID) == "$LUKS_UUID" && $(probe "$header" LABEL) == "$LABEL_NEW" ]] ||'),
      at('dd if="$header" of="$dev" bs="$((2 * METADATA_BYTES))" count=1 conv=notrunc,fsync status=none'),
      at('dd if="$header" of="$dev" bs="$((2 * METADATA_BYTES))" skip=1 seek=1 \\\n\t\tcount="$((HEADER_BYTES / (2 * METADATA_BYTES) - 1))" conv=notrunc,fsync status=none'),
      at("systemd-cryptenroll"),
    ];
    expect(order).toEqual(order.toSorted((a, b) => a - b));
    // The two dd lines are the only writes before the TPM's enrollment, and nothing before the
    // first one names the partition except as dd's output.
    const beforeEnroll = body.slice(0, at("systemd-cryptenroll"));
    expect([...beforeEnroll.matchAll(/"\$dev"/g)]).toHaveLength(2);
    expect(beforeEnroll.match(/^\s*dd /gm)).toHaveLength(2);
    // Every byte of the header file is written and nothing past it: the copies (2 * METADATA_BYTES
    // in one block), then the other blocks up to HEADER_BYTES, each dd bounded by its count.
    expect((LUKS.headerBytes / (2 * LUKS.metadataBytes)) % 1).toBe(0);
    expect(LUKS.headerBytes / (2 * LUKS.metadataBytes) - 1).toBe(511);
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
    // Only a partition that is empty or holds an unfinished setup (LABEL_NEW, which carries on the
    // mark of a wipe that did not finish) is formatted, and a volume whose TPM owner hierarchy is
    // gone for good: only on check_tpm's permanent verdict (Laysi/trustiot-v2#867).
    const calls = [...script.matchAll(/^\s*format "\$dev".*$/gm)].map((m) => m[0].trim());
    expect(calls).toEqual(['format "$dev" "$(make_mark tpm-owner-changed "$recorded" "$found")"', 'format "$dev" ""', 'format "$dev" "$(read_mark "$dev")"']);
    const permanent = /^\tpermanent\\ \*\)\n([\s\S]*?)\n\t\t;;$/m.exec(script)?.[1] ?? "";
    expect(permanent).toContain('format "$dev" "$(make_mark tpm-owner-changed');
    expect(script).toContain("if ((same >= CHECK_SAME_ANSWERS)); then\n\t\t\tverdict=\"permanent ${answer#other-owner }\"");
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
    // And stays off after a wipe, while the setup leaves the flag (Laysi/trustiot-v2#867).
    expect(unit).toContain(`ConditionPathExists=!${DATA_CHECK.factoryFlag}`);
    expect(script).toContain('>"$FACTORY_FLAG.new"');
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
      // The setup's owner fingerprint (Laysi/trustiot-v2#867), as `cryptsetup token import` keeps it.
      "1": { type: "trustiot-tpm-owner", keyslots: [] as string[], owner: OWNER_A },
    } as Record<string, Record<string, unknown>>,
    segments: { "0": { type: "crypt", offset: "16777216", size: "dynamic", iv_tweak: "0", encryption: LUKS.cipher, sector_size: LUKS.sectorBytes } },
    digests: {},
    config: { json_size: "12288", keyslots_size: "16744448", label: LUKS.label },
  };

  test("a LUKS2 header the setup leaves passes, and every other shape is found", () => {
    const json = (change: (h: typeof good) => unknown) => {
      const copy = structuredClone(good);
      change(copy);
      return JSON.stringify(copy);
    };
    expect(luksHeaderProblems(JSON.stringify(good))).toEqual([]);
    // A second keyslot, such as the setup's random key left behind or a password added later.
    expect(luksHeaderProblems(json((h) => Object.assign(h.keyslots, { "0": { type: "luks2" } }))).join()).toContain("2 keyslots");
    expect(luksHeaderProblems(json((h) => Object.assign(h.tokens, { "3": { type: "systemd-recovery", keyslots: ["2"] } }))).join()).toContain("2 tokens besides the setup's own");
    expect(luksHeaderProblems(json((h) => { (h.tokens["0"] as { type: string }).type = "systemd-fido2"; })).join()).toContain("expected systemd-tpm2");
    expect(luksHeaderProblems(json((h) => { (h.tokens["0"] as { "tpm2-pcrs": unknown })["tpm2-pcrs"] = [7]; })).join()).toContain("PCRs [7]");
    expect(luksHeaderProblems(json((h) => Object.assign(h.tokens["0"] as object, { "tpm2-pin": true }))).join()).toContain("PIN");
    expect(luksHeaderProblems(json((h) => Object.assign(h.tokens["0"] as object, { "tpm2-pcrlock": true }))).join()).toContain("pcrlock");
    expect(luksHeaderProblems(json((h) => { (h.tokens["0"] as { keyslots: string[] }).keyslots = ["2"]; })).join()).toContain("uses keyslots");
    expect(luksHeaderProblems(json((h) => { (h.segments["0"] as { sector_size: number }).sector_size = 512; })).join()).toContain("4096-byte sectors");
    expect(luksHeaderProblems(json((h) => { h.keyslots = {} as never; })).join()).toContain("0 keyslots");
    // The layout the setup writes: data at 16 MiB, 16 KiB metadata copies, the keyslot area between.
    expect(luksHeaderProblems(json((h) => { h.segments["0"].offset = "33554432"; })).join()).toContain("the data starts at byte 33554432, expected 16777216");
    expect(luksHeaderProblems(json((h) => { h.config.json_size = "61440"; })).join()).toContain("header areas");
    expect(luksHeaderProblems(json((h) => { h.config.keyslots_size = "33521664"; })).join()).toContain('"keyslots_size":"16744448"');
  });

  test("the setup's own tokens: the owner fingerprint always, the mark of a wipe only when expected, neither with a keyslot", () => {
    const withMark = { ...good, tokens: { ...good.tokens, "2": { type: "trustiot-back-to-factory", keyslots: [] as string[], reason: "tpm-owner-changed", sealed_owner: OWNER_A, found_owner: OWNER_B } } };
    const json = (h: object) => JSON.stringify(h);
    expect(luksHeaderProblems(json(withMark), { mark: true })).toEqual([]);
    // Not expected, the mark is also one token too many.
    expect(luksHeaderProblems(json(withMark))).toEqual(["token 2 is the mark of a wipe, expected none", "2 tokens besides the setup's own (0: systemd-tpm2, 2: trustiot-back-to-factory), expected exactly one"]);
    expect(luksHeaderProblems(json(good), { mark: true })).toEqual(["token 2 is missing, expected trustiot-back-to-factory"]);
    // A volume made before the fingerprint was recorded, or one whose token is broken.
    const { "1": _owner, ...withoutOwner } = good.tokens;
    expect(luksHeaderProblems(json({ ...good, tokens: withoutOwner }))).toEqual(["token 1 is missing, expected trustiot-tpm-owner"]);
    for (const owner of ["000b", `000c${"ab".repeat(32)}`, `000B${"AB".repeat(32)}`, 7]) {
      const broken = { ...good, tokens: { ...good.tokens, "1": { ...good.tokens["1"], owner } } };
      expect(luksHeaderProblems(json(broken)).join()).toContain("token 1 has no owner fingerprint");
      expect(ownerToken(json(broken))).toBeNull();
    }
    // Neither of the setup's tokens may open anything.
    const slotted = { ...good, tokens: { ...good.tokens, "1": { ...good.tokens["1"], keyslots: ["1"] } } };
    expect(luksHeaderProblems(json(slotted)).join()).toContain('token 1 (trustiot-tpm-owner) uses keyslots ["1"], expected none');
    const markSlotted = { ...withMark, tokens: { ...withMark.tokens, "2": { ...withMark.tokens["2"], keyslots: ["1"] } } };
    expect(luksHeaderProblems(json(markSlotted), { mark: true }).join()).toContain("token 2 (trustiot-back-to-factory) uses keyslots");
    // The systemd-tpm2 token in the setup's place does not count as one of the setup's own.
    const swapped = { ...good, tokens: { "1": good.tokens["0"], "0": good.tokens["1"] } };
    expect(luksHeaderProblems(json(swapped)).join()).toContain("token 1 is systemd-tpm2, expected trustiot-tpm-owner");
    expect(ownerToken(json(good))).toBe(OWNER_A);
    expect(markToken(json(good))).toBeNull();
    expect(markToken(json(withMark))).toEqual({ reason: "tpm-owner-changed", sealed_owner: OWNER_A, found_owner: OWNER_B });
    expect(markToken(json({ ...good, tokens: { ...good.tokens, "2": { type: "something-else", keyslots: [] } } }))).toBeNull();
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
