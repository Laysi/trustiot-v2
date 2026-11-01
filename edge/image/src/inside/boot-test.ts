// The boot test, run inside the tool container by boot-test.ts (README.md, section 「開機測試」).
// It boots a copy of the finished image in QEMU, with or without a software TPM (swtpm), and
// reads what boot-test/probe.sh found in each boot: the data partition is encrypted on the first
// boot with a TPM, also when that takes longer than systemd's default device timeout, unlocked
// and mounted by that TPM afterwards, and stays locked and unmounted, without a password prompt
// and without the agent, with no TPM, another TPM or a cleared one, also when someone starts the
// agent by hand; anything on the partition that is not the image's volume is left as it is; and a
// power cut after any step of the first boot's setup leaves a volume the next boot encrypts again
// or opens, with the TPM's keyslot as its only one (one known bug, when the setup had started
// again, is pinned as KNOWN). The image itself is only read; every boot runs on a copy.
import { copyFile, cp, lstat, mkdir, readFile, rm, stat, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { type BootExpectation, POWER_CUTS, UNITS, bootProblems, cutStateProblems, powerCutPrograms, redactTpmFields, unitSeconds } from "../boot-results.ts";
import { luksHeaderProblems, scriptConstants } from "../data.ts";
import { parseSfdiskJson, partuuid, targetTable } from "../layout.ts";
import { BOOT_TEST, DATA, LUKS, SECTOR } from "../pins.ts";
import { Ext4Mount, PATHS, extractPartition, installTools, placePartition, run, sha256File, step } from "./common.ts";

const W = PATHS.work;
const REPORT = join(W, "report");
const MNT = "/mnt/root";
const SOCKETS = join(W, "sock");
const KERNEL = BOOT_TEST.kernel;
const SOURCE_VERSION = KERNEL.replace(/^(\d+\.\d+)\..*$/, "$1");
/** Bound for one boot of the virtual machine (emulated, without KVM): the probe waits up to 20 min itself. */
const BOOT_LIMIT_MS = 45 * 60 * 1000;
/**
 * Read rate of the slow first boot's disk: the setup then reads the 2 GiB partition for about
 * 130 s, so it finishes well after systemd's default device timeout (90 s from the start of boot)
 * would have expired.
 */
const SLOW_READ_BYTES_PER_S = 16 * 1024 * 1024;
const SLOW_SETUP_MIN_S = 120;

interface Outcome {
  name: string;
  problems: string[];
  detail: string;
  /**
   * A known bug: the check pins the behaviour the image has today, which is not the one it should
   * have. Shown as KNOWN, not PASS, while it holds; when the behaviour changes the check fails, and
   * the expectation is to be turned into the right one.
   */
  knownBug?: string;
}
const outcomes: Outcome[] = [];

function record(name: string, problems: string[], detail: string, knownBug?: string): void {
  outcomes.push({ name, problems, detail, ...(knownBug ? { knownBug } : {}) });
  console.log(`${problems.length > 0 ? "FAIL" : knownBug ? "KNOWN" : "PASS"}  ${name}${detail ? `: ${detail}` : ""}`);
  if (knownBug) console.log(`      known bug: ${knownBug}`);
  for (const p of problems) console.log(`      - ${p}`);
}

async function installTestPackages(): Promise<void> {
  await installTools();
  step("boot test packages");
  const pinned = Object.entries(BOOT_TEST.packages).map(([name, version]) => `${name}=${version}`);
  await run(["apt-get", "-o", "Acquire::Retries=5", "-q", "install", "-y", "--no-install-recommends", ...pinned], {
    env: { DEBIAN_FRONTEND: "noninteractive" },
  });
  console.log((await run(["dpkg-query", "-W", ...Object.keys(BOOT_TEST.packages)])).stdout.trim());
}

/** tpm_tis, the driver of QEMU's TPM device, built from the Debian kernel's own source (pins.ts). */
async function buildTpmTis(): Promise<string> {
  step("tpm_tis module");
  const dir = join(W, "tpm_tis");
  await mkdir(dir, { recursive: true });
  const files = ["tpm_tis.c", "tpm.h", "tpm_tis_core.h"].map((f) => `linux-source-${SOURCE_VERSION}/drivers/char/tpm/${f}`);
  await run(["tar", "-xJf", `/usr/src/linux-source-${SOURCE_VERSION}.tar.xz`, "-C", dir, "--strip-components=4", ...files]);
  await writeFile(join(dir, "Kbuild"), "obj-m := tpm_tis.o\n");
  await run(["make", "-C", `/usr/src/linux-headers-${KERNEL}`, `M=${dir}`, "modules"]);
  const info = (await run(["modinfo", "--field=vermagic", join(dir, "tpm_tis.ko")])).stdout.trim();
  if (!info.startsWith(`${KERNEL} `)) throw new Error(`tpm_tis.ko has vermagic ${info}, expected ${KERNEL}`);
  return join(dir, "tpm_tis.ko");
}

/**
 * The test's copy of the image: the Debian kernel's modules (with tpm_tis) and the probe added to
 * the root file system; the boot and data partitions are the image's, byte for byte.
 */
async function prepareDisk(tpmTis: string): Promise<{ disk: string; rootPartuuid: string }> {
  step("test copy of the image");
  const disk = join(W, "disk0.img");
  await run(["cp", "--sparse=always", PATHS.image, disk]);
  const table = parseSfdiskJson((await run(["sfdisk", "--json", disk])).stdout);
  const root = table.partitions.find((p) => p.number === 2);
  if (!root) throw new Error("the image has no partition 2");
  const rootFile = join(W, "root.ext4");
  await extractPartition(disk, root.start, root.size, rootFile);
  await mkdir(MNT, { recursive: true });
  const mount = await Ext4Mount.open(rootFile, MNT, false);
  try {
    const modules = join(MNT, "usr/lib/modules");
    await cp(`/usr/lib/modules/${KERNEL}`, join(modules, KERNEL), { recursive: true, verbatimSymlinks: true });
    await mkdir(join(modules, KERNEL, "updates"), { recursive: true });
    await copyFile(tpmTis, join(modules, KERNEL, "updates/tpm_tis.ko"));
    await run(["depmod", "-b", MNT, KERNEL]);
    await copyFile(join(PATHS.src, "boot-test/probe.sh"), join(MNT, "usr/local/sbin/trustiot-boot-test-probe"));
    await run(["chmod", "0755", join(MNT, "usr/local/sbin/trustiot-boot-test-probe")]);
    await copyFile(join(PATHS.src, "boot-test/trustiot-boot-test.service"), join(MNT, "etc/systemd/system/trustiot-boot-test.service"));
    await mkdir(join(MNT, "etc/systemd/system/multi-user.target.wants"), { recursive: true });
    await symlink("../trustiot-boot-test.service", join(MNT, "etc/systemd/system/multi-user.target.wants/trustiot-boot-test.service"));
  } finally {
    await mount.unmount();
  }
  await run(["e2fsck", "-f", "-n", rootFile]);
  await placePartition(disk, rootFile, root.start);
  await rm(rootFile);
  return { disk, rootPartuuid: partuuid(table.labelId, 2) };
}

/**
 * The power-cut copy: the test copy of the image plus boot-test/power-cut.sh, linked in
 * /usr/local/sbin as each program it stands in for. Only the power-cut boots use it; without
 * trustiot_test.power-cut= on the kernel command line it runs the real programs.
 */
async function preparePowerCutDisk(testDisk: string): Promise<string> {
  step("power-cut copy of the image");
  const disk = join(W, "disk-power-cut.img");
  await run(["cp", "--sparse=always", testDisk, disk]);
  const root = parseSfdiskJson((await run(["sfdisk", "--json", disk])).stdout).partitions.find((p) => p.number === 2);
  if (!root) throw new Error("the image has no partition 2");
  const rootFile = join(W, "root-power-cut.ext4");
  await extractPartition(disk, root.start, root.size, rootFile);
  const script = join(PATHS.src, "boot-test/power-cut.sh");
  const programs = powerCutPrograms(await readFile(script, "utf8"));
  if (programs.size === 0) throw new Error("boot-test/power-cut.sh names no program");
  const mount = await Ext4Mount.open(rootFile, MNT, false);
  try {
    const sbin = join(MNT, "usr/local/sbin");
    const name = "trustiot-boot-test-power-cut";
    await copyFile(script, join(sbin, name));
    await run(["chmod", "0755", join(sbin, name)]);
    const present = (path: string) => lstat(path).then(() => true, () => false);
    for (const [program, real] of programs) {
      if (!(await present(join(MNT, real)))) throw new Error(`the image has no ${real}, which power-cut.sh runs as ${program}`);
      if (await present(join(sbin, program))) throw new Error(`the image already has /usr/local/sbin/${program}`);
      await symlink(name, join(sbin, program));
    }
  } finally {
    await mount.unmount();
  }
  await run(["e2fsck", "-f", "-n", rootFile]);
  await placePartition(disk, rootFile, root.start);
  await rm(rootFile);
  return disk;
}

/** A software TPM for one boot; its state directory is the TPM's identity across boots. */
class Swtpm {
  private constructor(
    private readonly child: ReturnType<typeof Bun.spawn>,
    readonly ctrl: string,
    readonly server: string | null,
  ) {}

  static async start(state: string, name: string, withServer: boolean): Promise<Swtpm> {
    await mkdir(state, { recursive: true, mode: 0o700 });
    await mkdir(SOCKETS, { recursive: true, mode: 0o700 });
    const server = withServer ? join(SOCKETS, `${name}.sock`) : null;
    // tpm2-tools' swtpm TCTI expects the control socket next to the data socket, as <path>.ctrl.
    const ctrl = server ? `${server}.ctrl` : join(SOCKETS, `${name}.ctrl`);
    await rm(ctrl, { force: true });
    if (server) await rm(server, { force: true });
    const args = ["swtpm", "socket", "--tpm2", "--tpmstate", `dir=${state}`, "--ctrl", `type=unixio,path=${ctrl}`, "--log", `file=${join(W, `swtpm-${name}.log`)},level=1`];
    if (server) args.push("--server", `type=unixio,path=${server}`, "--flags", "startup-clear");
    const child = Bun.spawn(args, { stdout: "inherit", stderr: "inherit" });
    const deadline = Date.now() + 60_000;
    // A socket is not a file to Bun.file().exists(): stat it.
    while (!(await stat(server ?? ctrl).then(
      (s) => s.isSocket(),
      () => false,
    ))) {
      if (child.exitCode !== null) throw new Error(`swtpm for ${name} exited with ${child.exitCode}`);
      if (Date.now() > deadline) throw new Error(`swtpm for ${name} did not create its socket within 60 s`);
      await Bun.sleep(100);
    }
    return new Swtpm(child, ctrl, server);
  }

  async stop(): Promise<void> {
    if (this.child.exitCode === null) this.child.kill("SIGTERM");
    const code = await Promise.race([this.child.exited, Bun.sleep(60_000).then(() => "timeout" as const)]);
    if (code === "timeout") {
      this.child.kill("SIGKILL");
      throw new Error("swtpm did not exit within 60 s of SIGTERM");
    }
  }
}

/** Clears a TPM (TPM2_Clear with the lockout hierarchy), as a reset of the gateway's TPM would. */
async function clearTpm(state: string): Promise<void> {
  const tpm = await Swtpm.start(state, "clear", true);
  try {
    await run(["tpm2_clear", "--tcti", `swtpm:path=${tpm.server}`, "--auth-hierarchy", "l"]);
  } finally {
    await tpm.stop();
  }
}

interface Boot {
  name: string;
  disk: string;
  tpm: string | null;
  marker?: string;
  /** Bytes per second the virtual machine may read from the disk (QEMU's block throttling). */
  readBytesPerS?: number;
  /** A second disk, attached with the serial number trustiot-other. */
  otherDisk?: string;
  /** The probe starts the agent by hand after the boot (probe.sh). */
  startAgent?: boolean;
  /** The setup step after which the virtual machine loses its power (power-cut.sh; the power-cut copy only). */
  powerCut?: string;
}

/** Boots the disk once and returns what the probe wrote. */
async function boot(b: Boot, rootPartuuid: string): Promise<string> {
  step(`boot ${b.name} (${b.tpm ? `TPM ${b.tpm.split("/").at(-1)}` : "no TPM"})`);
  const results = join(W, `results-${b.name}.img`);
  await rm(results, { force: true });
  await run(["truncate", "-s", "16M", results]);
  const tpm = b.tpm ? await Swtpm.start(b.tpm, b.name, false) : null;
  const append = [`root=PARTUUID=${rootPartuuid}`, "rootwait", "console=ttyAMA0", "panic=-1", "loglevel=4"];
  if (b.marker) append.push(`trustiot_test.write-marker=${b.marker}`);
  if (b.startAgent) append.push("trustiot_test.start-agent=1");
  if (b.powerCut) append.push(`trustiot_test.power-cut=${b.powerCut}`);
  const qemu = [
    "qemu-system-aarch64",
    "-machine", "virt",
    "-cpu", "max,pauth-impdef=on",
    "-smp", "4",
    "-m", "2048",
    "-accel", "tcg,thread=multi",
    "-kernel", `/boot/vmlinuz-${KERNEL}`,
    "-initrd", `/boot/initrd.img-${KERNEL}`,
    "-append", append.join(" "),
    "-drive", `if=none,id=gateway,file=${b.disk},format=raw,cache=unsafe${b.readBytesPerS ? `,throttling.bps-read=${b.readBytesPerS}` : ""}`,
    "-device", "virtio-blk-pci,drive=gateway,serial=trustiot-gateway",
    "-drive", `if=none,id=results,file=${results},format=raw`,
    "-device", "virtio-blk-pci,drive=results,serial=trustiot-results",
    "-nic", "none",
    "-display", "none",
    "-monitor", "none",
    "-serial", `file:${join(REPORT, `${b.name}.serial.log`)}`,
    "-no-reboot",
  ];
  if (b.otherDisk) qemu.push("-drive", `if=none,id=other,file=${b.otherDisk},format=raw`, "-device", "virtio-blk-pci,drive=other,serial=trustiot-other");
  // QEMU's emulator backend connects to swtpm's control socket (--ctrl) only and hands swtpm the
  // data channel over it; swtpm needs no --server socket for QEMU. This is QEMU's own example,
  // including for -machine virt with tpm-tis-device: https://www.qemu.org/docs/master/specs/tpm.html
  // (the TPM emulator device examples, read 2026-10-31).
  if (tpm) qemu.push("-chardev", `socket,id=chrtpm,path=${tpm.ctrl}`, "-tpmdev", "emulator,id=tpm0,chardev=chrtpm", "-device", "tpm-tis-device,tpmdev=tpm0");
  const started = Date.now();
  const child = Bun.spawn(qemu, { stdout: "inherit", stderr: "inherit" });
  const code = await Promise.race([child.exited, Bun.sleep(BOOT_LIMIT_MS).then(() => "timeout" as const)]);
  if (code === "timeout") child.kill("SIGKILL");
  if (tpm) await tpm.stop();
  console.log(`qemu ${code === "timeout" ? "killed" : `exited with ${code}`} after ${Math.round((Date.now() - started) / 1000)} s`);
  if (code === "timeout") throw new Error(`boot ${b.name}: the virtual machine did not power off within ${BOOT_LIMIT_MS / 60000} min`);
  const text = (await readFile(results, "latin1")).replace(/\0+$/, "");
  // A power-cut boot never gets to the probe: the results disk holds power-cut.sh's record.
  await writeFile(join(REPORT, `${b.name}.${b.powerCut ? "cut" : "probe"}.txt`), redactTpmFields(text));
  await rm(results);
  return text;
}

async function checkBoot(b: Boot, rootPartuuid: string, what: string, expect: BootExpectation, knownBug?: string): Promise<string> {
  const text = await boot(b, rootPartuuid);
  record(`${b.name}: ${what}`, bootProblems(text, expect), "", knownBug);
  return text;
}

const dataPart = () => {
  const p = targetTable().partitions.find((x) => x.number === DATA.number);
  if (!p) throw new Error("no data partition in the target table");
  return p;
};

/** True when the data partition of `disk` is all zeros. */
async function dataIsZero(disk: string): Promise<boolean> {
  const p = dataPart();
  const r = await run(["cmp", "--silent", `--bytes=${p.size * SECTOR}`, `--ignore-initial=${p.start * SECTOR}:0`, disk, "/dev/zero"], { okCodes: [0, 1] });
  return r.code === 0;
}

async function dataHash(disk: string): Promise<string> {
  const p = dataPart();
  const file = join(W, "data-part.img");
  await extractPartition(disk, p.start, p.size, file);
  const hash = await sha256File(file);
  await rm(file);
  return hash;
}

/** The LUKS2 header as cryptsetup reads it from the disk file (no device mapper needed). */
async function luksJson(disk: string): Promise<string> {
  const p = dataPart();
  const file = join(W, "data-part.img");
  await extractPartition(disk, p.start, p.size, file);
  const json = (await run(["cryptsetup", "luksDump", "--dump-json-metadata", file])).stdout;
  await rm(file);
  return json;
}

/** How many bytes at the start of the data partition the setup zeroes before luksFormat (its HEADER_BYTES). */
async function setupZeroedBytes(): Promise<number> {
  const value = Number(scriptConstants(await readFile(join(PATHS.src, "files/data/trustiot-data-setup"), "utf8")).get("HEADER_BYTES"));
  if (!Number.isSafeInteger(value) || value <= 0) throw new Error("files/data/trustiot-data-setup has no HEADER_BYTES");
  return value;
}

interface DataState {
  /** `blkid --probe --output export` of the partition (empty when blkid finds nothing). */
  blkid: string;
  /** The LUKS2 header (`cryptsetup luksDump --dump-json-metadata`), when there is one. */
  json: string | null;
  /** The bytes the setup zeroes before luksFormat are zeros. */
  headZero: boolean;
  /** The whole partition is zeros. */
  allZero: boolean;
}

/** The data partition of `disk` as the build host reads it. */
async function dataState(disk: string): Promise<DataState> {
  const p = dataPart();
  const file = join(W, "data-part.img");
  await extractPartition(disk, p.start, p.size, file);
  try {
    const blkid = (await run(["blkid", "--probe", "--output", "export", file], { okCodes: [0, 2] })).stdout;
    const json = blkid.split("\n").includes("TYPE=crypto_LUKS") ? (await run(["cryptsetup", "luksDump", "--dump-json-metadata", file])).stdout : null;
    const zeros = async (bytes: number) => (await run(["cmp", "--silent", `--bytes=${bytes}`, file, "/dev/zero"], { okCodes: [0, 1] })).code === 0;
    return { blkid, json, headZero: await zeros(await setupZeroedBytes()), allZero: await zeros(p.size * SECTOR) };
  } finally {
    await rm(file, { force: true });
  }
}

/** Boots a power-cut boot and checks that the virtual machine lost its power at that step. */
async function checkPowerCut(b: Boot & { powerCut: string }, rootPartuuid: string): Promise<void> {
  const text = await boot(b, rootPartuuid);
  const problems = text.trim() === `power-cut ${b.powerCut}` ? [] : [`the results disk holds ${JSON.stringify(redactTpmFields(text).slice(0, 200))}, expected "power-cut ${b.powerCut}" (the setup ran on, or the probe ran)`];
  record(`${b.name}: the virtual machine loses its power at ${b.powerCut} in the setup`, problems, "");
}

/**
 * A LUKS2 header with a passphrase keyslot (a fake key made here and deleted), the given UUID and
 * label, in a 32 MiB file; the rest of a partition it is placed on stays as it was.
 */
async function luksFile(name: string, uuid: string, label: string): Promise<string> {
  const file = join(W, `${name}.luks`);
  const key = join(W, `${name}.key`);
  await writeFile(key, Buffer.from(crypto.getRandomValues(new Uint8Array(32))).toString("hex"), { mode: 0o600 });
  await run(["truncate", "-s", "32M", file]);
  const options = ["--type", "luks2", "--cipher", LUKS.cipher, "--key-size", String(LUKS.keyBits), "--sector-size", String(LUKS.sectorBytes)];
  const kdf = ["--pbkdf", "pbkdf2", "--pbkdf-force-iterations", "1000"];
  await run(["cryptsetup", "luksFormat", "--batch-mode", ...options, ...kdf, "--uuid", uuid, "--label", label, "--key-file", key, file]);
  await rm(key);
  return file;
}

/** A copy of the disk `source` with `file` written `offsetBytes` into the data partition. */
async function diskWith(name: string, source: string, file: string, offsetBytes = 0): Promise<string> {
  const disk = join(W, `${name}.img`);
  await run(["cp", "--sparse=always", source, disk]);
  await placePartition(disk, file, dataPart().start + offsetBytes / SECTOR);
  await rm(file);
  return disk;
}

/** Number of times `marker` appears in the disk's raw bytes. */
async function rawCount(disk: string, marker: string): Promise<number> {
  const r = await run(["grep", "--count", "--fixed-strings", "--text", marker, disk], { okCodes: [0, 1] });
  return Number(r.stdout.trim() || "0");
}

async function main(): Promise<void> {
  await mkdir(REPORT, { recursive: true });
  await installTestPackages();
  const tpmTis = await buildTpmTis();
  const { disk: disk0, rootPartuuid } = await prepareDisk(tpmTis);
  if (!(await dataIsZero(disk0))) throw new Error("the image's data partition is not all zeros (run check.ts first)");
  const powerCutDisk = await preparePowerCutDisk(disk0);
  const tpmA = join(W, "tpm/A");
  const tpmB = join(W, "tpm/B");
  const marker = new Bun.CryptoHasher("sha256").update(crypto.getRandomValues(new Uint8Array(32))).digest("hex").slice(0, 32);

  // Anything on the data partition that is not this image's volume is refused and left as it was,
  // with a TPM present (the refusal is about the content).
  const tpmF = join(W, "tpm/F");
  const fs = join(W, "foreign.ext4");
  await run(["truncate", "-s", "64M", fs]);
  await run(["mke2fs", "-t", "ext4", "-q", "-L", "someone-else", fs]);
  const noise = join(W, "noise.bin");
  await writeFile(noise, crypto.getRandomValues(new Uint8Array(4096)));
  const refused = [
    { name: "1-foreign-fs", what: "another file system", disk: await diskWith("foreign-fs", disk0, fs), logs: "not this image's data volume; not formatting it" },
    {
      name: "2-foreign-luks",
      what: "a LUKS2 volume with another UUID",
      disk: await diskWith("foreign-luks", disk0, await luksFile("foreign-luks", crypto.randomUUID(), "someone-else")),
      logs: "not this image's data volume; not formatting it",
    },
    // Zeros except for 4 KiB of random bytes 1 GiB in: no format blkid knows, but not empty.
    {
      name: "3-not-empty",
      what: "unknown bytes after the first 16 MiB",
      disk: await diskWith("not-empty", disk0, noise, 1024 * 1024 * 1024),
      logs: "has no known format but is not empty; not formatting it",
    },
  ];
  for (const r of refused) {
    const before = await dataHash(r.disk);
    await checkBoot({ name: r.name, disk: r.disk, tpm: tpmF }, rootPartuuid, `${r.what} on the data partition is refused`, {
      volume: "locked",
      setup: "failed",
      setupLogs: r.logs,
    });
    record(`${r.name}: the data partition is unchanged`, (await dataHash(r.disk)) === before ? [] : ["the data partition changed"], `sha256 ${before.slice(0, 16)}...`);
    await rm(r.disk);
  }

  // A setup that stopped before it finished (the volume still has its "new" label and the key's
  // keyslot): the next boot with a TPM does it again, and only that TPM's keyslot is left.
  const interrupted = await diskWith("interrupted", disk0, await luksFile("interrupted", LUKS.uuid, LUKS.labelNew));
  await checkBoot({ name: "4-interrupted-setup", disk: interrupted, tpm: join(W, "tpm/C") }, rootPartuuid, "an unfinished setup is done again: TPM C unlocks and mounts it", {
    volume: "unlocked",
    setup: "success",
    setupLogs: "holds a setup that did not finish (no data was ever written to it): starting again",
  });
  record("4-interrupted-setup: LUKS2 header has the TPM token as its only keyslot (read on the build host)", luksHeaderProblems(await luksJson(interrupted)), "");
  await rm(interrupted);

  // The image's own disk from here on: first without a TPM, then with TPM A, B, none, A cleared, A.
  const disk = join(W, "disk.img");
  await run(["mv", disk0, disk]);
  await checkBoot({ name: "5-first-boot-no-tpm", disk, tpm: null }, rootPartuuid, "without a TPM the partition is not encrypted, not mounted, the agent stays off", {
    volume: "locked",
    setup: "failed",
    setupLogs: "no TPM (/dev/tpmrm0)",
  });
  record("5-first-boot-no-tpm: the data partition is still all zeros", (await dataIsZero(disk)) ? [] : ["the data partition was written"], "");

  // The first boot with a TPM, on a disk that reads slowly: the setup takes longer than systemd's
  // default device timeout, and the volume must still be unlocked and mounted in the same boot.
  const first = await checkBoot(
    { name: "6-first-boot-tpm-a", disk, tpm: tpmA, marker, readBytesPerS: SLOW_READ_BYTES_PER_S },
    rootPartuuid,
    "TPM A encrypts, unlocks and mounts the partition on a slow disk; the agent starts",
    { volume: "unlocked", setup: "success", setupLogs: "is empty: first boot", writeMarker: marker, readMarker: marker },
  );
  const setupDone = unitSeconds(first, UNITS.setup, "StateChangeTimestampMonotonic");
  record(
    "6-first-boot-tpm-a: the setup finished after systemd's default device timeout would have expired",
    setupDone >= SLOW_SETUP_MIN_S ? [] : [`the setup finished ${setupDone.toFixed(1)} s after boot, expected at least ${SLOW_SETUP_MIN_S} s (the disk was not slow enough to show the device timeout)`],
    `setup finished ${setupDone.toFixed(1)} s after boot`,
  );
  const header = await luksJson(disk);
  record("6-first-boot-tpm-a: LUKS2 header has the TPM token as its only keyslot (read on the build host)", luksHeaderProblems(header), "");
  const count = await rawCount(disk, marker);
  record("6-first-boot-tpm-a: what the agent's directory holds never appears in the disk's raw bytes", count === 0 ? [] : [`the marker appears ${count} times`], "");

  await checkBoot({ name: "7-tpm-a-again", disk, tpm: tpmA }, rootPartuuid, "TPM A unlocks it again, without formatting; the data is still there", {
    volume: "unlocked",
    setup: "success",
    setupLogs: "is encrypted and opens only with the TPM",
    readMarker: marker,
  });

  // After the boot the probe starts the agent by hand, as root would: as is, and skipping its
  // dependencies, so that only AssertPathIsMountPoint= is left to stop it.
  await checkBoot({ name: "8-other-tpm", disk, tpm: tpmB, startAgent: true }, rootPartuuid, "another TPM cannot unlock it: no prompt, not mounted, the agent stays off, also when started by hand", {
    volume: "locked",
    setup: "success",
    startAgent: true,
  });
  await checkBoot({ name: "9-tpm-removed", disk, tpm: null }, rootPartuuid, "without the TPM it stays locked: no prompt, not mounted, the agent stays off", {
    volume: "locked",
    setup: "success",
  });

  const tpmCleared = join(W, "tpm/A-cleared");
  await run(["cp", "-a", tpmA, tpmCleared]);
  await clearTpm(tpmCleared);
  await checkBoot({ name: "10-tpm-a-cleared", disk, tpm: tpmCleared }, rootPartuuid, "TPM A after TPM2_Clear cannot unlock it: no prompt, not mounted, the agent stays off", {
    volume: "locked",
    setup: "success",
  });

  await checkBoot({ name: "11-tpm-a-last", disk, tpm: tpmA }, rootPartuuid, "the failed unlocks destroyed nothing: TPM A opens the same volume with the same data", {
    volume: "unlocked",
    setup: "success",
    setupLogs: "is encrypted and opens only with the TPM",
    readMarker: marker,
  });
  const after = await luksJson(disk);
  record("11-tpm-a-last: the LUKS2 header is the one the first boot wrote", after === header ? [] : ["the header changed after the first boot"], "");
  const countAfter = await rawCount(disk, marker);
  record("11-tpm-a-last: the marker still never appears in the disk's raw bytes", countAfter === 0 ? [] : [`the marker appears ${countAfter} times`], "");

  // Another disk that carries the volume's UUID: crypttab finds the volume by it, so the setup
  // refuses and the volume stays locked; neither disk is changed.
  const otherDisk = join(W, "other.img");
  await run(["mv", await luksFile("other", LUKS.uuid, LUKS.label), otherDisk]);
  const [dataBefore, otherBefore] = [await dataHash(disk), await sha256File(otherDisk)];
  await checkBoot({ name: "12-duplicate-uuid", disk, tpm: tpmA, otherDisk }, rootPartuuid, "another disk with the volume's UUID: the setup refuses, the volume stays locked", {
    volume: "locked",
    setup: "failed",
    setupLogs: `the volume UUID ${LUKS.uuid} is on`,
  });
  const changed = [
    ...((await dataHash(disk)) === dataBefore ? [] : ["the data partition changed"]),
    ...((await sha256File(otherDisk)) === otherBefore ? [] : ["the other disk changed"]),
  ];
  record("12-duplicate-uuid: neither disk changed", changed, "");
  await rm(otherDisk);

  // The first boot loses its power after each step of the setup (the power-cut copy, each time on
  // a fresh copy with an all-zero data partition and a TPM of its own). The next boot with that
  // TPM must encrypt the partition again or open it, never refuse it, and leave only the TPM's
  // keyslot.
  let n = 13;
  let afterMkfs: { disk: string; tpm: string } | null = null;
  for (const cut of POWER_CUTS) {
    const disk = join(W, `power-cut-${cut.step}.img`);
    const tpm = join(W, `tpm/P-${cut.step}`);
    await run(["cp", "--sparse=always", powerCutDisk, disk]);
    await checkPowerCut({ name: `${n}a-power-cut-${cut.step}`, disk, tpm, powerCut: cut.step }, rootPartuuid);
    const state = await dataState(disk);
    record(
      `${n}a-power-cut-${cut.step}: the data partition holds what that step leaves (read on the build host)`,
      state.json === null ? [`no LUKS2 volume on the data partition (blkid: ${JSON.stringify(state.blkid.trim())})`] : cutStateProblems(cut.state, state.blkid, state.json),
      `label ${cut.state.label}, keyslots ${cut.state.keyslots.join(" and ")}${cut.state.tpmToken ? ", the TPM's token" : ""}`,
    );
    if (cut.step === "mkfs") {
      afterMkfs = { disk: join(W, "power-cut-mkfs-again.img"), tpm: join(W, "tpm/P-mkfs-again") };
      await run(["cp", "--sparse=always", disk, afterMkfs.disk]);
      await run(["cp", "-a", tpm, afterMkfs.tpm]);
    }
    await checkBoot({ name: `${n}b-after-${cut.step}`, disk, tpm }, rootPartuuid, `the next boot after a power cut at ${cut.step}: the same TPM unlocks and mounts it`, {
      volume: "unlocked",
      setup: "success",
      setupLogs: cut.nextLogs,
    });
    record(`${n}b-after-${cut.step}: LUKS2 header has the TPM token as its only keyslot (read on the build host)`, luksHeaderProblems(await luksJson(disk)), "");
    await rm(disk);
    n += 1;
  }

  // The setup that starts again after a power cut zeroes the start of the partition first. Once
  // an earlier attempt had made the file system, the rest of the partition is no longer zeros, so
  // a power cut right after that zeroing leaves a partition the setup does not recognise: the next
  // boot refuses it. That is a known bug (below); the boot pins what happens today, which at least
  // fails closed: nothing is formatted, unlocked or mounted, the agent does not start.
  const knownBug =
    "a power cut right after a restarted setup zeroed the start of the partition leaves a partition the next boot refuses " +
    "(it should set it up); docs/test-reports/2026-11-01-890-boot-test-power-cut.md, section 「已知問題」";
  if (!afterMkfs) throw new Error("no power cut at mkfs to start again from");
  await checkPowerCut({ name: `${n}a-power-cut-zero-header-again`, disk: afterMkfs.disk, tpm: afterMkfs.tpm, powerCut: "zero-header" }, rootPartuuid);
  const zeroed = await dataState(afterMkfs.disk);
  record(
    `${n}a-power-cut-zero-header-again: the start of the data partition is zeros, the rest holds the earlier attempt (read on the build host)`,
    [
      ...(zeroed.blkid.trim() === "" ? [] : [`blkid finds ${JSON.stringify(zeroed.blkid.trim())}`]),
      ...(zeroed.headZero ? [] : ["the start of the partition is not zeros"]),
      ...(zeroed.allZero ? ["the whole partition is zeros"] : []),
    ],
    "",
  );
  const zeroedHash = await dataHash(afterMkfs.disk);
  await checkBoot(
    { name: `${n}b-after-zero-header-again`, disk: afterMkfs.disk, tpm: afterMkfs.tpm },
    rootPartuuid,
    "the next boot after a power cut at zero-header in a setup started again refuses the partition: not mounted, the agent stays off",
    { volume: "locked", setup: "failed", setupLogs: "has no known format but is not empty; not formatting it" },
    knownBug,
  );
  record(
    `${n}b-after-zero-header-again: the data partition is unchanged`,
    (await dataHash(afterMkfs.disk)) === zeroedHash ? [] : ["the data partition changed"],
    `sha256 ${zeroedHash.slice(0, 16)}...`,
    knownBug,
  );
  await rm(afterMkfs.disk);
  await rm(powerCutDisk);

  await writeFile(join(REPORT, "boot-test.json"), `${JSON.stringify(outcomes, null, 2)}\n`);
  await writeFile(join(REPORT, "luks-header.json"), redactTpmFields(header));
  const failed = outcomes.filter((o) => o.problems.length > 0);
  const known = outcomes.filter((o) => o.problems.length === 0 && o.knownBug).length;
  console.log(`\n${outcomes.length - failed.length} of ${outcomes.length} checks passed${known > 0 ? ` (${known} of them pin a known bug: KNOWN)` : ""}`);
  // Exit now, also when every check passed: the time limit of each boot (Bun.sleep raced against
  // QEMU) is still pending, and would otherwise keep the process, and the container, alive until
  // the last one expired, up to BOOT_LIMIT_MS after the last boot.
  process.exit(failed.length > 0 ? 1 : 0);
}


main().catch(async (error) => {
  console.error(error instanceof Error ? (error.stack ?? error.message) : error);
  await writeFile(join(REPORT, "boot-test.json"), `${JSON.stringify(outcomes, null, 2)}\n`).catch(() => {});
  process.exit(3);
});
