// The boot test, run inside the tool container by boot-test.ts (README.md, section 「開機測試」).
// It boots a copy of the finished image in QEMU, with or without a software TPM (swtpm), and
// reads what boot-test/probe.sh found in each boot: the data partition is encrypted on the first
// boot with a TPM, also when that takes longer than systemd's default device timeout, unlocked
// and mounted by that TPM afterwards, also when the TPM shows up late; without a TPM, without the
// owner fingerprint in the header, with a TPM whose owner hierarchy cannot be read, or with
// another gateway's TPM (one that holds a key), it stays locked and unmounted, without a password
// prompt and without the agent (also when someone starts the agent by hand), and is never wiped;
// a TPM whose 0x81000001 holds a key other than its standard storage key is never sealed under;
// with another TPM without keys or a cleared one it is wiped and made anew for that TPM, with the
// mark that the gateway goes back to the factory, and the agent stays off until the mark is
// removed (Laysi/trustiot-v2#867); anything on the partition that is not
// the image's volume is left as it is; and a power cut after any step of the first boot's setup,
// of a setup that started again after such a cut, or of a wipe, leaves a partition the next boot
// encrypts again or opens, with the TPM's keyslot as its only one (and the mark, after a wipe).
// The same three setups are also recorded write by write (QEMU's write log): every state a power
// cut inside a step, a card cache that loses or reorders unflushed writes, or a torn write can
// leave is rebuilt from the log and given to the image's own setup, which must set the partition
// up again, open it, or refuse it (fail closed), and one state of each outcome is booted
// (Laysi/trustiot-v2#918).
// The image itself is only read; every boot runs on a copy.
import { closeSync, openSync, readSync, writeSync } from "node:fs";
import { copyFile, cp, lstat, mkdir, readFile, rm, stat, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import {
  type BootExpectation,
  type Owners,
  POWER_CUTS,
  type PowerCut,
  RESTART_CUTS,
  type SeenState,
  type SetupPath,
  type SetupRunOn,
  UNITS,
  WIPED,
  WIPE_CUTS,
  bootProblems,
  bootRuns,
  cutStateProblems,
  describeCutState,
  describeHeaderWrites,
  headerWriterProblems,
  outcomeBoots,
  outcomeKey,
  parseProbe,
  parseTpmLooks,
  powerCutPrograms,
  redactTpmFields,
  unitSeconds,
  wipeMarkProblems,
  writerWords,
} from "../boot-results.ts";
import { BOOT_TEST_FILES, powerCutLink, wantsLink } from "../boot-test-files.ts";
import { luksHeaderProblems, ownerToken } from "../data.ts";
import { parseSfdiskJson, partuuid, targetTable } from "../layout.ts";
import { BOOT_TEST, DATA, LUKS, SECTOR } from "../pins.ts";
import {
  type CrashState,
  HEADER_SECTORS,
  METADATA_SECTORS,
  PartitionView,
  STEP_MARK_SECTOR,
  type SetupRecord,
  applyState,
  crashStates,
  parseWriteLog,
  setupRecord,
} from "../write-log.ts";
import { CLASSIFY_WORKERS, Classifier, nonZeroDataSectors, readHeader } from "./classify.ts";
import { Ext4Mount, PATHS, extractPartition, installTools, placePartition, run, sha256Bytes, sha256File, step } from "./common.ts";

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
/**
 * Seconds into the boot at which the late TPM shows up (boot-test/tpm-late.sh): after systemd has
 * given up waiting for /dev/tpmrm0 (90 s) and the setup has started looking, and well inside the
 * setup's window of DATA_CHECK.windowS seconds.
 */
const TPM_LATE_S = 150;
/** What the setup logs when the TPM opens a finished volume. */
const OPENS = "is encrypted and opens only with the TPM";

interface Outcome {
  name: string;
  problems: string[];
  detail: string;
}
const outcomes: Outcome[] = [];

function record(name: string, problems: string[], detail: string): void {
  outcomes.push({ name, problems, detail });
  console.log(`${problems.length > 0 ? "FAIL" : "PASS"}  ${name}${detail ? `: ${detail}` : ""}`);
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
 * Installs the boot test's files (src/boot-test-files.ts) in the root file system mounted at
 * `root`: those of every copy, or those of the power-cut copy only. The image check refuses an
 * image that carries any of them.
 */
async function installBootTestFiles(root: string, powerCutOnly: boolean): Promise<void> {
  for (const file of BOOT_TEST_FILES.filter((f) => (f.powerCutOnly ?? false) === powerCutOnly)) {
    const target = join(root, file.image.slice(1));
    if (await present(target)) throw new Error(`the image already has ${file.image}`);
    await copyFile(join(PATHS.src, "boot-test", file.repo), target);
    await run(["chmod", file.mode.toString(8).padStart(4, "0"), target]);
    const link = wantsLink(file);
    if (link) {
      await mkdir(join(root, link.slice(1, link.lastIndexOf("/"))), { recursive: true });
      await symlink(`../${file.image.slice(file.image.lastIndexOf("/") + 1)}`, join(root, link.slice(1)));
    }
  }
}

const present = (path: string) => lstat(path).then(() => true, () => false);

/**
 * The test's copy of the image: the Debian kernel's modules (with tpm_tis), the probe and the late
 * TPM's loader (boot-test/tpm-late.sh, which does nothing unless the kernel command line asks for
 * it) added to the root file system; the boot and data partitions are the image's, byte for byte.
 */
async function prepareDisk(tpmTis: string): Promise<{ disk: string; rootPartuuid: string }> {
  step("test copy of the image");
  const disk = join(W, "disk0.img");
  await run(["cp", "--sparse=always", PATHS.image, disk]);
  const table = parseSfdiskJson((await run(["sfdisk", "--json", disk])).stdout);
  const root = table.partitions.find((p) => p.number === 2);
  if (!root) throw new Error("the image has no partition 2");
  // The step marks (boot-test/power-cut.sh) go to a sector no partition holds.
  if (STEP_MARK_SECTOR < 1 || !table.partitions.every((p) => p.start > STEP_MARK_SECTOR)) {
    throw new Error(`sector ${STEP_MARK_SECTOR} (the step marks) is not between the partition table and the first partition`);
  }
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
    await installBootTestFiles(MNT, false);
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
 * /usr/local/sbin as each program it stands in for. Only the power-cut boots and the recorded
 * setups use it; without trustiot_test.power-cut= or trustiot_test.step-marks= on the kernel
 * command line it runs the real programs. Returns the disk and its root file system's file, which
 * the classification of the recorded setups' states runs in (classify.ts).
 */
async function preparePowerCutDisk(testDisk: string): Promise<{ disk: string; rootFile: string }> {
  step("power-cut copy of the image");
  const disk = join(W, "disk-power-cut.img");
  await run(["cp", "--sparse=always", testDisk, disk]);
  const root = parseSfdiskJson((await run(["sfdisk", "--json", disk])).stdout).partitions.find((p) => p.number === 2);
  if (!root) throw new Error("the image has no partition 2");
  const rootFile = join(W, "root-power-cut.ext4");
  await extractPartition(disk, root.start, root.size, rootFile);
  const programs = powerCutPrograms(await readFile(join(PATHS.src, "boot-test/power-cut.sh"), "utf8"));
  if (programs.size === 0) throw new Error("boot-test/power-cut.sh names no program");
  const shim = BOOT_TEST_FILES.find((f) => f.repo === "power-cut.sh");
  if (!shim) throw new Error("src/boot-test-files.ts does not list power-cut.sh");
  const mount = await Ext4Mount.open(rootFile, MNT, false);
  try {
    await installBootTestFiles(MNT, true);
    for (const [program, real] of programs) {
      const link = join(MNT, powerCutLink(program).slice(1));
      if (!(await present(join(MNT, real)))) throw new Error(`the image has no ${real}, which power-cut.sh runs as ${program}`);
      if (await present(link)) throw new Error(`the image already has ${powerCutLink(program)}`);
      if (link.slice(0, link.lastIndexOf("/")) !== join(MNT, shim.image.slice(1, shim.image.lastIndexOf("/")))) throw new Error(`${powerCutLink(program)} is not next to ${shim.image}`);
      await symlink(shim.image.slice(shim.image.lastIndexOf("/") + 1), link);
    }
  } finally {
    await mount.unmount();
  }
  await run(["e2fsck", "-f", "-n", rootFile]);
  await placePartition(disk, rootFile, root.start);
  return { disk, rootFile };
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
async function clearTpm(state: string): Promise<string> {
  const tpm = await Swtpm.start(state, "clear", true);
  const tcti = ["--tcti", `swtpm:path=${tpm.server}`];
  try {
    await run(["tpm2_clear", ...tcti, "--auth-hierarchy", "l"]);
    return (await run(["tpm2_getcap", ...tcti, "handles-persistent"])).stdout.trim();
  } finally {
    await tpm.stop();
  }
}

/**
 * A new TPM with a password on its owner hierarchy (TPM2_HierarchyChangeAuth; made here and not
 * kept): nothing can make a primary key there without it, so the setup cannot read the owner
 * hierarchy's fingerprint.
 */
async function tpmWithOwnerPassword(state: string): Promise<void> {
  const tpm = await Swtpm.start(state, "owner-password", true);
  try {
    const password = Buffer.from(crypto.getRandomValues(new Uint8Array(16))).toString("hex");
    await run(["tpm2_changeauth", "--tcti", `swtpm:path=${tpm.server}`, "--object-context=o", password]);
  } finally {
    await tpm.stop();
  }
}

/**
 * Gives a TPM what a registered gateway's TPM holds (in its state directory): the storage key its
 * setup had systemd persist at 0x81000001 (made here when it is not there yet), and a key the
 * factory persisted at 0x81000100 (the agent's default handle; factory/workstation, --handle).
 * Returns what `tpm2_getcap handles-persistent` prints afterwards.
 */
async function addGatewayKey(state: string): Promise<string> {
  const tpm = await Swtpm.start(state, "with-key", true);
  const keyWork = join(W, "tpm-with-key");
  await mkdir(keyWork, { recursive: true, mode: 0o700 });
  const tcti = ["--tcti", `swtpm:path=${tpm.server}`];
  try {
    const before = (await run(["tpm2_getcap", ...tcti, "handles-persistent"])).stdout;
    if (!before.split("\n").includes("- 0x81000001")) {
      await run(["tpm2_createprimary", ...tcti, "--hierarchy=o", "--hash-algorithm=sha256", "--key-algorithm=ecc256:aes128cfb", "--attributes=fixedtpm|fixedparent|sensitivedataorigin|userwithauth|noda|restricted|decrypt", `--key-context=${keyWork}/srk.ctx`]);
      await run(["tpm2_evictcontrol", ...tcti, "--hierarchy=o", `--object-context=${keyWork}/srk.ctx`, "0x81000001"]);
      // Without a resource manager, transient objects stay loaded until flushed (swtpm has few slots).
      await run(["tpm2_flushcontext", ...tcti, "--transient-object"]);
    }
    await run(["tpm2_create", ...tcti, "--parent-context=0x81000001", "--key-algorithm=ecc256", `--public=${keyWork}/key.pub`, `--private=${keyWork}/key.priv`]);
    await run(["tpm2_load", ...tcti, "--parent-context=0x81000001", `--public=${keyWork}/key.pub`, `--private=${keyWork}/key.priv`, `--key-context=${keyWork}/key.ctx`]);
    await run(["tpm2_evictcontrol", ...tcti, "--hierarchy=o", `--object-context=${keyWork}/key.ctx`, "0x81000100"]);
    await run(["tpm2_flushcontext", ...tcti, "--transient-object"]);
    return (await run(["tpm2_getcap", ...tcti, "handles-persistent"])).stdout.trim();
  } finally {
    await tpm.stop();
    await rm(keyWork, { recursive: true, force: true });
  }
}

/**
 * Gives a new TPM (in its state directory) a storage key at 0x81000001 that is not the one the
 * setup and systemd-cryptenroll make (the TCG's ECC template): an RSA one, as other software may
 * persist there. systemd-cryptenroll would seal the volume key under whatever key is there.
 * Returns what `tpm2_getcap handles-persistent` prints afterwards.
 */
async function addForeignStorageKey(state: string): Promise<string> {
  const tpm = await Swtpm.start(state, "foreign-srk", true);
  const keyWork = join(W, "tpm-foreign-srk");
  await mkdir(keyWork, { recursive: true, mode: 0o700 });
  const tcti = ["--tcti", `swtpm:path=${tpm.server}`];
  try {
    await run(["tpm2_createprimary", ...tcti, "--hierarchy=o", "--hash-algorithm=sha256", "--key-algorithm=rsa2048:aes128cfb", "--attributes=fixedtpm|fixedparent|sensitivedataorigin|userwithauth|noda|restricted|decrypt", `--key-context=${keyWork}/srk.ctx`]);
    await run(["tpm2_evictcontrol", ...tcti, "--hierarchy=o", `--object-context=${keyWork}/srk.ctx`, "0x81000001"]);
    await run(["tpm2_flushcontext", ...tcti, "--transient-object"]);
    return (await run(["tpm2_getcap", ...tcti, "handles-persistent"])).stdout.trim();
  } finally {
    await tpm.stop();
    await rm(keyWork, { recursive: true, force: true });
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
  /** The TPM's driver loads only this many seconds into the boot (tpm-late.sh): no /dev/tpmrm0 until then. */
  tpmLateS?: number;
  /**
   * QEMU logs every write to the gateway's disk and every flush to this file (blklogwrites, the
   * format of Linux's dm-log-writes), and power-cut.sh marks the setup's steps in it (the
   * power-cut copy only; src/write-log.ts reads it).
   */
  writeLog?: string;
  /**
   * The kernel traces every block write it queues (the block_bio_queue event), and probe.sh lists
   * the ones to the data partition's header area with the program that wrote them (header-writes).
   */
  traceHeaderWrites?: boolean;
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
  if (b.writeLog) append.push(`trustiot_test.step-marks=${STEP_MARK_SECTOR}`);
  // From the kernel's start on; a boot queues some 25,000 block requests, reads included (one
  // boot of Laysi/trustiot-v2#918's experiments: 25,529), far fewer than the buffer of 16 MiB per
  // processor holds (probe.sh reports how many it holds and any it dropped).
  if (b.traceHeaderWrites) append.push("trace_event=block:block_bio_queue", "trace_buf_size=16M");
  // udev does not load the TPM's driver for the device; tpm-late.sh loads it by name later.
  if (b.tpmLateS) append.push("modprobe.blacklist=tpm_tis", `trustiot_test.tpm-late=${b.tpmLateS}`);
  // The gateway's disk: without a log, as before; with one, through QEMU's blklogwrites driver.
  // Its node passes the guest's flushes on (they are what the log is for: the virtio disk
  // announces a write cache, so the guest flushes as on a card), and the file nodes below it
  // drop them, as cache=unsafe does.
  if (b.writeLog && b.readBytesPerS) throw new Error(`boot ${b.name}: a write log and a slow disk together are not supported`);
  const gatewayDisk = b.writeLog
    ? [
        "-blockdev", `driver=file,node-name=gateway-file,filename=${b.disk},cache.no-flush=on`,
        "-blockdev", `driver=file,node-name=gateway-log-file,filename=${b.writeLog},cache.no-flush=on`,
        "-blockdev", "driver=blklogwrites,node-name=gateway,file=gateway-file,log=gateway-log-file,log-append=off,log-super-update-interval=1",
        "-device", "virtio-blk-pci,drive=gateway,serial=trustiot-gateway,write-cache=on",
      ]
    : [
        "-drive", `if=none,id=gateway,file=${b.disk},format=raw,cache=unsafe${b.readBytesPerS ? `,throttling.bps-read=${b.readBytesPerS}` : ""}`,
        "-device", "virtio-blk-pci,drive=gateway,serial=trustiot-gateway",
      ];
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
    ...gatewayDisk,
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

async function checkBoot(b: Boot, rootPartuuid: string, what: string, expect: BootExpectation): Promise<string> {
  const text = await boot(b, rootPartuuid);
  record(`${b.name}: ${what}`, bootProblems(text, expect), expect.tpmLooks ? `looks at the TPM: ${looksSeen(text)}` : "");
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

/** The data partition of `disk` as the build host reads it. */
async function dataState(disk: string): Promise<SeenState> {
  const p = dataPart();
  const file = join(W, "data-part.img");
  await extractPartition(disk, p.start, p.size, file);
  try {
    const blkid = (await run(["blkid", "--probe", "--output", "export", file], { okCodes: [0, 2] })).stdout;
    const json = blkid.split("\n").includes("TYPE=crypto_LUKS") ? (await run(["cryptsetup", "luksDump", "--dump-json-metadata", file])).stdout : null;
    const allZero = (await run(["cmp", "--silent", `--bytes=${p.size * SECTOR}`, file, "/dev/zero"], { okCodes: [0, 1] })).code === 0;
    return { blkid, json, allZero };
  } finally {
    await rm(file, { force: true });
  }
}

/** True when the data partition of `disk` is all zeros behind the LUKS2 header (LUKS.headerBytes). */
async function dataAreaIsZero(disk: string): Promise<boolean> {
  const p = dataPart();
  const r = await run(
    ["cmp", "--silent", `--bytes=${p.size * SECTOR - LUKS.headerBytes}`, `--ignore-initial=${p.start * SECTOR + LUKS.headerBytes}:0`, disk, "/dev/zero"],
    { okCodes: [0, 1] },
  );
  return r.code === 0;
}

/** Boots a power-cut boot and checks that the virtual machine lost its power at that step. */
async function checkPowerCut(b: Boot & { powerCut: string }, rootPartuuid: string): Promise<void> {
  const text = await boot(b, rootPartuuid);
  const problems = text.trim() === `power-cut ${b.powerCut}` ? [] : [`the results disk holds ${JSON.stringify(redactTpmFields(text).slice(0, 200))}, expected "power-cut ${b.powerCut}" (the setup ran on, or the probe ran)`];
  record(`${b.name}: the virtual machine loses its power at ${b.powerCut} in the setup`, problems, "");
}

/**
 * One power cut and the boot after it: the cut happens at that step, leaves the state that step
 * leaves, and the next boot with the same TPM sets the partition up (or opens it) and unlocks and
 * mounts it, with the TPM's keyslot as the only one. In a wipe (`mark`), the TPM is another one
 * than the volume was sealed under, and the next boot also ends with the mark in the header and
 * the agent off.
 */
async function checkCutAndNextBoot(
  c: { n: number; tag: string; disk: string; tpm: string; cut: PowerCut; keepCut?: { disk: string; tpm: string }; mark?: boolean },
  rootPartuuid: string,
): Promise<void> {
  const { n, tag, disk, tpm, cut, mark } = c;
  await checkPowerCut({ name: `${n}a-power-cut-${tag}`, disk, tpm, powerCut: cut.step }, rootPartuuid);
  record(`${n}a-power-cut-${tag}: the data partition holds what that step leaves (read on the build host)`, cutStateProblems(cut.state, await dataState(disk)), describeCutState(cut.state));
  if (c.keepCut) {
    await run(["cp", "--sparse=always", disk, c.keepCut.disk]);
    await run(["cp", "-a", tpm, c.keepCut.tpm]);
  }
  const what = mark
    ? `the next boot after a power cut at ${cut.step} in a wipe: the same TPM unlocks and mounts it, the mark is in the header and the agent stays off`
    : `the next boot after a power cut at ${cut.step}: the same TPM unlocks and mounts it`;
  await checkBoot({ name: `${n}b-after-${tag}`, disk, tpm }, rootPartuuid, what, {
    volume: "unlocked",
    setup: "success",
    setupLogs: cut.nextLogs,
    mark,
  });
  record(
    `${n}b-after-${tag}: LUKS2 header has the TPM token as its only keyslot${mark ? ", and the mark" : ""} (read on the build host)`,
    luksHeaderProblems(await luksJson(disk), { mark }),
    "",
  );
}

/**
 * Removes one LUKS2 token from the volume on `disk` (cryptsetup token remove on a copy of the
 * partition, written back): the owner fingerprint, as on a volume made before it was recorded, or
 * the mark, as the factory does.
 */
async function removeToken(disk: string, id: string): Promise<void> {
  const p = dataPart();
  const file = join(W, "data-part.img");
  await extractPartition(disk, p.start, p.size, file);
  try {
    await run(["cryptsetup", "token", "remove", "--token-id", id, file]);
    await placePartition(disk, file, p.start);
  } finally {
    await rm(file, { force: true });
  }
}

/** The setup's looks at the TPM in a boot's probe output, for the test's output. */
function looksSeen(probeText: string): string {
  const looks = parseTpmLooks(parseProbe(probeText).sections.get("journal-setup")?.output ?? "");
  return looks.length === 0 ? "no looks" : looks.map((l) => `${l.at} s ${l.answer.replace(/ 000b[0-9a-f]{64}$/, "")}`).join(", ");
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

/** Where the classification mounts the power-cut copy's root file system (read-only) and runs in it. */
const CLASSIFY_ROOT = "/mnt/classify";
/** The steps of the setup's format(), as power-cut.sh marks them. */
const SETUP_STEPS = ["luks-format", "write-header", "write-keyslots", "tpm-enroll", "mkfs", "kill-slot", "relabel"];
const KEYSLOTS_AS_LEFT = "as the setup left it";

/** A setup the boot test records write by write, and what it starts from. */
interface Recording {
  path: SetupPath;
  /** The disk it starts from (a copy of the power-cut copy, run on a copy of it). */
  start: string;
  /** The TPM it runs with (a copy of this state directory), or null for a new one. */
  startTpm: string | null;
  /** The data partition before the setup as a file, or null to take it from `start`. */
  base: string | null;
  /** The owner fingerprint the partition was sealed under before: a wipe's. */
  before: string | null;
  what: string;
  expect: BootExpectation;
}

/** States the setup decides the same about (outcomeKey), with the first one of them. */
interface StateClass {
  key: string;
  runs: SetupRunOn[];
  first: CrashState;
  firstView: string;
  count: number;
  models: Record<string, number>;
}

/** The header area's keyslot area, behind the two metadata copies. */
function keyslotArea(view: PartitionView): Buffer {
  const h = view.header;
  return Buffer.from(h.buffer, h.byteOffset + METADATA_SECTORS * SECTOR, (HEADER_SECTORS - METADATA_SECTORS) * SECTOR);
}

/** Writes a state onto a file that holds the partition before the setup. */
function writeState(file: string, writes: SetupRecord["writes"], state: CrashState): void {
  const fd = openSync(file, "r+");
  try {
    applyState(writes, state, {
      put: (sector, bytes) => {
        let done = 0;
        while (done < bytes.length) done += writeSync(fd, bytes, done, bytes.length - done, sector * SECTOR + done);
      },
      get: (sector) => {
        const out = new Uint8Array(SECTOR);
        if (readSync(fd, out, 0, SECTOR, sector * SECTOR) !== SECTOR) throw new Error(`${file} has no sector ${sector}`);
        return out;
      },
    });
  } finally {
    closeSync(fd);
  }
}

const tally = (counts: Record<string, number>, key: string, by = 1) => {
  counts[key] = (counts[key] ?? 0) + by;
};
const describeCounts = (counts: Record<string, number>) =>
  Object.entries(counts)
    .map(([k, v]) => `${k} ${v}`)
    .join(", ");

/**
 * Records one setup (README.md, section 「開機測試」): boots it on the power-cut copy with QEMU's
 * write log and the step marks, rebuilds from the log every state a power cut can leave
 * (src/write-log.ts), has the image's own setup decide about each (classify.ts), and boots one
 * state of each decision, and on the first boot a cut in the middle of each step that writes,
 * with the TPM of the recording: the boots must show what the setup decided. `next` hands out
 * the boot numbers.
 */
async function checkRecordedSetup(r: Recording, next: () => number, powerCutDisk: string, classifier: Classifier, rootPartuuid: string): Promise<void> {
  const name = `${next()}-record-${r.path}`;
  const disk = join(W, `record-${r.path}.img`);
  const tpm = join(W, `tpm/R-${r.path}`);
  const tpmAfter = join(W, `tpm/R-${r.path}-after`);
  const log = join(W, `write-log-${r.path}.bin`);
  const base = r.base ?? join(W, `record-${r.path}-base.img`);
  const part = dataPart();
  await run(["cp", "--sparse=always", r.start, disk]);
  if (r.startTpm) await run(["cp", "-a", r.startTpm, tpm]);
  if (!r.base) await extractPartition(disk, part.start, part.size, base);
  await writeFile(log, "");
  try {
    await checkBoot({ name, disk, tpm, writeLog: log }, rootPartuuid, r.what, r.expect);
    await run(["cp", "-a", tpm, tpmAfter]);
    let rec: SetupRecord;
    try {
      rec = setupRecord(parseWriteLog(new Uint8Array(await readFile(log))), { start: part.start, sectors: part.size }, STEP_MARK_SECTOR, SETUP_STEPS.at(-1) as string);
    } catch (error) {
      record(`${name}: the write log holds the setup's steps`, [error instanceof Error ? error.message : String(error)], "");
      return;
    }
    await rm(log);
    const seen = rec.steps.map((s) => s.step);
    record(
      `${name}: the write log holds the setup's steps in order, each ended with exit code 0`,
      [
        ...(JSON.stringify(seen) === JSON.stringify(SETUP_STEPS) ? [] : [`the steps were ${seen.join(", ")}, expected ${SETUP_STEPS.join(", ")}`]),
        ...rec.steps.filter((s) => s.exit !== 0).map((s) => `${s.step} ended with ${s.exit}`),
      ],
      `${rec.writes.length} writes to the data partition and ${rec.flushes.length} flushes until the end of ${SETUP_STEPS.at(-1)}; writes per step: ${rec.steps.map((s) => `${s.step} ${s.end - s.first}`).join(", ")}`,
    );

    // The partition before the setup, and what the log says the setup left: the disk must hold it.
    const before = PartitionView.of(readHeader(base), nonZeroDataSectors(base));
    const after = before.clone();
    for (const w of rec.writes) after.put(w.sector, w.data);
    const leftFile = join(W, `record-${r.path}-left.img`);
    await extractPartition(disk, part.start, part.size, leftFile);
    const left = readHeader(leftFile);
    await rm(leftFile);
    record(
      `${name}: the header area rebuilt from the log is the one on the disk after the boot (read on the build host)`,
      Buffer.from(left).equals(Buffer.from(after.header)) ? [] : ["they differ: the log misses writes of the setup, or something wrote the header after it"],
      "",
    );
    const recorded = ownerToken(await luksJson(disk));
    if (!recorded) {
      record(`${name}: the volume records the TPM's owner fingerprint`, ["it does not"], "");
      return;
    }
    const owners: Owners = { recorded, before: r.before };

    // Every state, each distinct partition given to the setup once.
    step(`${name}: the setup's decision on every state a power cut can leave`);
    const finalKeyslots = keyslotArea(after);
    const keyslotNames = new Map<string, string>();
    const keyslotsName = (view: PartitionView): string => {
      const area = keyslotArea(view);
      if (area.equals(finalKeyslots)) return KEYSLOTS_AS_LEFT;
      const hash = sha256Bytes(area);
      const known = keyslotNames.get(hash) ?? `not as the setup left it (version ${keyslotNames.size + 1})`;
      keyslotNames.set(hash, known);
      return known;
    };
    const classKey = (runs: readonly SetupRunOn[], keyslots: string): string => {
      const last = bootRuns(runs, owners).at(-1);
      const opens = last?.decision === "check" && last.owner === owners.recorded;
      return `${outcomeKey(runs, owners)}${opens && keyslots !== KEYSLOTS_AS_LEFT ? `; keyslot area ${keyslots}` : ""}`;
    };
    type Seen = { first: CrashState; count: number; models: Record<string, number>; keyslots: string; runs: SetupRunOn[] | null; error: unknown };
    const byView = new Map<string, Seen>();
    const pending = new Set<Promise<void>>();
    const started = Date.now();
    let states = 0;
    const cur = before.clone();
    let durable = 0;
    for (const state of crashStates(rec, before.header.subarray(0, METADATA_SECTORS * SECTOR))) {
      for (; durable < state.durable; durable++) {
        const w = rec.writes[durable] as SetupRecord["writes"][number];
        cur.put(w.sector, w.data);
      }
      const view = cur.clone();
      applyState(rec.writes, state, view, state.durable);
      const key = view.key();
      states++;
      let seenState = byView.get(key);
      if (!seenState) {
        const fresh: Seen = { first: state, count: 0, models: {}, keyslots: keyslotsName(view), runs: null, error: null };
        byView.set(key, fresh);
        seenState = fresh;
        const p: Promise<void> = classifier
          .classify(view)
          .then(
            (runs) => {
              fresh.runs = runs;
            },
            (error: unknown) => {
              fresh.error = error;
            },
          )
          .finally(() => pending.delete(p));
        pending.add(p);
        if (pending.size >= CLASSIFY_WORKERS) await Promise.race(pending);
      }
      seenState.count++;
      tally(seenState.models, state.model);
    }
    await Promise.all(pending);
    const errors = [...byView.values()].filter((s) => s.error !== null);
    record(
      `${name}: the image's setup decided about every state`,
      errors.slice(0, 5).map((s) => `${s.first.model} ${s.first.what}: ${s.error instanceof Error ? s.error.message : String(s.error)}`),
      `${states} states, ${byView.size} distinct partitions, ${classifier.runs} runs of the setup so far, ${Math.round((Date.now() - started) / 1000)} s${errors.length > 5 ? `; ${errors.length} failed` : ""}`,
    );

    // The setup decides about the partition before and after the recorded setup as the boots did.
    const decided = (view: PartitionView) => {
      const s = byView.get(view.key());
      return s?.runs ? outcomeKey(s.runs, owners) : "(not decided)";
    };
    const expected = {
      "first-boot": ["set up as on a first boot", "a finished volume of the recording's TPM"],
      "start-again": ["started again", "a finished volume of the recording's TPM"],
      wipe: ["a finished volume of the TPM before the wipe", "a finished volume of the recording's TPM (flag: the wipe's mark)"],
    }[r.path];
    const [beforeSeen, afterSeen] = [decided(before), decided(after)];
    record(
      `${name}: the image's setup, given the partition before and after the recorded setup, decides as the boots did`,
      [
        ...(beforeSeen === expected[0] ? [] : [`before: ${beforeSeen}, expected ${expected[0]}`]),
        ...(afterSeen === expected[1] ? [] : [`after: ${afterSeen}, expected ${expected[1]}`]),
      ],
      "",
    );

    const classes = new Map<string, StateClass>();
    for (const [view, s] of byView) {
      if (!s.runs) continue;
      const key = classKey(s.runs, s.keyslots);
      const c = classes.get(key) ?? { key, runs: s.runs, first: s.first, firstView: view, count: 0, models: {} };
      classes.set(key, c);
      c.count += s.count;
      for (const [model, k] of Object.entries(s.models)) tally(c.models, model, k);
    }
    const stuck: string[] = [];
    for (const c of classes.values()) {
      const o = outcomeBoots(c.runs, r.path, owners);
      if (o.stuck) stuck.push(c.key);
      record(
        `${name}: ${c.count} states: ${c.key}`,
        o.problems,
        `${o.stuck ? "fail closed for good: every later boot refuses it until the factory sets the partition up; " : ""}${describeCounts(c.models)}; the first: ${c.first.model} in ${c.first.step}, ${c.first.what}`,
      );
    }
    await writeFile(
      join(REPORT, `${name}.states.json`),
      `${JSON.stringify(
        {
          path: r.path,
          steps: rec.steps,
          flushes: rec.flushes,
          writes: rec.writes.map((w) => ({ entry: w.entry, sector: w.sector, sectors: w.data.length / SECTOR, step: w.step })),
          otherEntries: { elsewhere: rec.elsewhere, afterTheSetup: rec.after },
          states,
          distinctPartitions: byView.size,
          classes: [...classes.values()].map((c) => ({ outcome: c.key, states: c.count, models: c.models, stuck: stuck.includes(c.key), first: { model: c.first.model, step: c.first.step, what: c.first.what } })),
        },
        null,
        2,
      )}\n`,
    );

    // The states booted: on the first boot a cut in the middle of each step that writes, then
    // the first state of each class those do not cover.
    const chosen: { slug: string; state: CrashState; runs: SetupRunOn[]; view: string }[] = [];
    const covered = new Set<string>();
    if (r.path === "first-boot") {
      for (const s of rec.steps) {
        const writes = s.end - s.first;
        let state: CrashState | null = null;
        if (writes >= 2) {
          const k = s.first + Math.floor(writes / 2);
          state = { model: "cut", step: s.step, durable: k, pieces: [], what: `cut before write ${k + 1} of ${rec.writes.length}, the middle of ${s.step}` };
        } else if (writes === 1 && (rec.writes[s.first]?.data.length ?? 0) > SECTOR) {
          const sectors = (rec.writes[s.first]?.data.length ?? 0) / SECTOR;
          const j = Math.floor(sectors / 2);
          state = { model: "torn-cut", step: s.step, durable: s.first, pieces: [{ write: s.first, from: 0, to: j }], what: `write ${s.first + 1} of ${rec.writes.length}, all of ${s.step}, cut after ${j} of its ${sectors} sectors` };
        }
        if (!state) continue;
        const view = before.clone();
        applyState(rec.writes, state, view);
        const known = byView.get(view.key());
        const runs = known?.runs ?? (await classifier.classify(view));
        chosen.push({ slug: `mid-${s.step}`, state, runs, view: view.key() });
        covered.add(classKey(runs, known?.keyslots ?? keyslotsName(view)));
      }
    }
    for (const c of classes.values()) {
      if (covered.has(c.key)) continue;
      covered.add(c.key);
      chosen.push({ slug: `class-${chosen.length + 1}`, state: c.first, runs: c.runs, view: c.firstView });
    }
    console.log(`${name}: ${classes.size} outcomes; booting ${chosen.length} states`);

    for (const c of chosen) {
      const k = next();
      const tag = `${k}-${r.path}-${c.slug}`;
      const file = join(W, `state-${tag}.img`);
      await run(["cp", "--sparse=always", base, file]);
      writeState(file, rec.writes, c.state);
      const header = sha256Bytes(readHeader(file));
      record(`${tag}: the partition rebuilt for the boot is the one the setup decided about (${c.state.model} in ${c.state.step}: ${c.state.what})`, header === c.view.slice(0, 64) ? [] : [`its header area has sha256 ${header}, the setup was given ${c.view.slice(0, 64)}`], "");
      const stateDisk = join(W, `state-${tag}-disk.img`);
      await run(["cp", "--sparse=always", powerCutDisk, stateDisk]);
      await placePartition(stateDisk, file, part.start);
      await rm(file);
      const stateTpm = join(W, `tpm/S-${tag}`);
      await run(["cp", "-a", tpmAfter, stateTpm]);
      // What the agent would write on the volume in a boot that opens it, for the boots after it.
      const marker = new Bun.CryptoHasher("sha256").update(crypto.getRandomValues(new Uint8Array(32))).digest("hex").slice(0, 32);
      const o = outcomeBoots(c.runs, r.path, owners, marker);
      for (const [i, b] of o.boots.entries()) {
        const bootName = `${k}${String.fromCharCode(97 + i)}-${r.path}-${c.slug}`;
        const decided = c.runs[i] as SetupRunOn;
        const hashBefore = b.expect.volume === "locked" ? await dataHash(stateDisk) : null;
        const what = i === 0 ? `after a power cut (${c.state.model} in ${c.state.step}: ${c.state.what}), the setup ${b.what}` : `the boot after that: the setup ${b.what}`;
        const probeText = await checkBoot({ name: bootName, disk: stateDisk, tpm: stateTpm, traceHeaderWrites: true, ...(b.expect.writeMarker ? { marker: b.expect.writeMarker } : {}) }, rootPartuuid, what, b.expect);
        // Where the setup does not make a new header (it leaves the volume to the TPM, or refuses),
        // the programs that wrote the header area in the boot are the ones its decision on the
        // state names: the setup's own cryptsetup or systemd-cryptenroll, udisks2, or none.
        if (decided.decision === "refused" || (decided.decision === "check" && decided.owner === owners.recorded)) {
          record(`${bootName}: the programs that wrote the header area in the boot are the ones the setup's decision on the state names (${writerWords(decided.changedBy)})`, headerWriterProblems(probeText, decided.changedBy), describeHeaderWrites(probeText));
        }
        if (b.expect.volume === "unlocked") {
          record(`${bootName}: LUKS2 header has the TPM token as its only keyslot${b.expect.mark ? ", and the mark" : ""} (read on the build host)`, luksHeaderProblems(await luksJson(stateDisk), { mark: b.expect.mark }), "");
        } else if (decided.changed) {
          // The classification saw one metadata copy written over the other: so must the boot.
          record(`${bootName}: ${writerWords(decided.changedBy)} wrote one metadata copy over the other, as when the setup decided about the state`, (await dataHash(stateDisk)) !== hashBefore ? [] : ["the data partition is unchanged"], "");
        } else {
          record(`${bootName}: the data partition is unchanged`, (await dataHash(stateDisk)) === hashBefore ? [] : ["the data partition changed"], "");
        }
      }
      await rm(stateDisk);
      await rm(stateTpm, { recursive: true, force: true });
    }
  } finally {
    await rm(disk, { force: true });
    await rm(log, { force: true });
    await rm(tpm, { recursive: true, force: true });
    await rm(tpmAfter, { recursive: true, force: true });
    if (!r.base) await rm(base, { force: true });
  }
}

async function main(): Promise<void> {
  await mkdir(REPORT, { recursive: true });
  await installTestPackages();
  const tpmTis = await buildTpmTis();
  const { disk: disk0, rootPartuuid } = await prepareDisk(tpmTis);
  if (!(await dataIsZero(disk0))) throw new Error("the image's data partition is not all zeros (run check.ts first)");
  const { disk: powerCutDisk, rootFile: powerCutRoot } = await preparePowerCutDisk(disk0);
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
    setupLogs: OPENS,
    readMarker: marker,
  });
  const unchanged = async (name: string) =>
    record(`${name}: the LUKS2 header is the one the first boot wrote (read on the build host)`, (await luksJson(disk)) === header ? [] : ["the header changed after the first boot"], "");

  // Without the TPM the setup looks for it for the whole window (README.md, section
  // 「TPM 永久解不開時」), then leaves the volume as it is. After the boot the probe starts the agent
  // by hand, as root would: as is, and skipping its dependencies, so that only
  // AssertPathIsMountPoint= is left to stop it.
  await checkBoot(
    { name: "8-tpm-removed", disk, tpm: null, startAgent: true },
    rootPartuuid,
    "without the TPM the setup looks for it for the whole window and does not wipe; it stays locked: no prompt, not mounted, the agent stays off, also when started by hand",
    { volume: "locked", setup: "success", setupLogs: "not wiping it: it stays locked", startAgent: true, tpmLooks: { outcome: "gave-up", answers: ["absent"] } },
  );
  await unchanged("8-tpm-removed");

  // A TPM that is not there yet when the setup starts (as one that does not answer for a while):
  // the setup keeps looking, and the TPM opens the same volume once it is there.
  const late = await checkBoot(
    { name: "9-tpm-a-late", disk, tpm: tpmA, tpmLateS: TPM_LATE_S },
    rootPartuuid,
    `TPM A shows up ${TPM_LATE_S} s into the boot: the setup waits for it, and it unlocks and mounts the same volume with the same data`,
    { volume: "unlocked", setup: "success", setupLogs: OPENS, readMarker: marker, tpmLooks: { outcome: "opened-late", answers: ["absent", "silent", "same-owner"] } },
  );
  const lateDone = unitSeconds(late, UNITS.setup, "StateChangeTimestampMonotonic");
  record(
    "9-tpm-a-late: the setup finished only after the TPM showed up",
    lateDone >= TPM_LATE_S ? [] : [`the setup finished ${lateDone.toFixed(1)} s after boot, before the TPM's driver was loaded at ${TPM_LATE_S} s`],
    `setup finished ${lateDone.toFixed(1)} s after boot`,
  );
  await unchanged("9-tpm-a-late");

  // Another TPM, on a copy of the volume without the owner fingerprint (as one made before it was
  // recorded): nothing proves that no TPM can open it, so it is never wiped.
  const unrecorded = join(W, "unrecorded.img");
  await run(["cp", "--sparse=always", disk, unrecorded]);
  await removeToken(unrecorded, LUKS.ownerToken.id);
  const unrecordedBefore = await dataHash(unrecorded);
  await checkBoot(
    { name: "10-unrecorded-tpm-b", disk: unrecorded, tpm: tpmB },
    rootPartuuid,
    "TPM B and a volume without the owner fingerprint: the setup looks for the whole window and does not wipe; it stays locked, the agent stays off",
    { volume: "locked", setup: "success", setupLogs: "not wiping it: it stays locked", tpmLooks: { outcome: "gave-up", answers: ["unrecorded"] } },
  );
  record("10-unrecorded-tpm-b: the data partition is unchanged", (await dataHash(unrecorded)) === unrecordedBefore ? [] : ["the data partition changed"], "");
  await rm(unrecorded);

  // Another TPM whose owner hierarchy has a password: the setup cannot read its fingerprint, so it
  // cannot tell whether the TPM is another one, and does not wipe.
  const tpmO = join(W, "tpm/O");
  await tpmWithOwnerPassword(tpmO);
  await checkBoot(
    { name: "11-owner-password", disk, tpm: tpmO },
    rootPartuuid,
    "a TPM with a password on its owner hierarchy: the setup cannot read the fingerprint, looks for the whole window and does not wipe; it stays locked, the agent stays off",
    { volume: "locked", setup: "success", setupLogs: "not wiping it: it stays locked", tpmLooks: { outcome: "gave-up", answers: ["unknown-owner"] } },
  );
  await unchanged("11-owner-password");

  await checkBoot({ name: "12-tpm-a-last", disk, tpm: tpmA }, rootPartuuid, "the failed unlocks destroyed nothing: TPM A opens the same volume with the same data", {
    volume: "unlocked",
    setup: "success",
    setupLogs: OPENS,
    readMarker: marker,
  });
  await unchanged("12-tpm-a-last");
  const countAfter = await rawCount(disk, marker);
  record("12-tpm-a-last: the marker still never appears in the disk's raw bytes", countAfter === 0 ? [] : [`the marker appears ${countAfter} times`], "");

  // Another disk that carries the volume's UUID: crypttab finds the volume by it, so the setup
  // refuses and the volume stays locked; neither disk is changed. With another TPM: the setup
  // refuses before it looks at the TPM, so it never wipes either.
  const otherDisk = join(W, "other.img");
  await run(["mv", await luksFile("other", LUKS.uuid, LUKS.label), otherDisk]);
  const [dataBefore, otherBefore] = [await dataHash(disk), await sha256File(otherDisk)];
  await checkBoot(
    { name: "13-duplicate-uuid", disk, tpm: tpmB, otherDisk },
    rootPartuuid,
    "another disk with the volume's UUID, and TPM B: the setup refuses before it looks at the TPM, the volume stays locked and is not wiped",
    { volume: "locked", setup: "failed", setupLogs: `the volume UUID ${LUKS.uuid} is on` },
  );
  const changed = [
    ...((await dataHash(disk)) === dataBefore ? [] : ["the data partition changed"]),
    ...((await sha256File(otherDisk)) === otherBefore ? [] : ["the other disk changed"]),
  ];
  record("13-duplicate-uuid: neither disk changed", changed, "");
  await rm(otherDisk);

  // Another registered gateway's TPM (a card moved from gateway to gateway): another owner
  // hierarchy, but the TPM holds a key besides the storage key, so the setup never wipes the card;
  // it stays locked for its own gateway.
  const tpmK = join(W, "tpm/K");
  const kHandles = await addGatewayKey(tpmK);
  record("14-tpm-with-a-key: the TPM holds the storage key and a key at 0x81000100 (made on the build host)", kHandles === "- 0x81000001\n- 0x81000100" ? [] : [`tpm2_getcap handles-persistent printed ${JSON.stringify(kHandles)}`], "");
  await checkBoot(
    { name: "14-tpm-with-a-key", disk, tpm: tpmK },
    rootPartuuid,
    "another gateway's TPM, with its own storage key and a key at 0x81000100: the setup looks for the whole window and does not wipe; it stays locked, the agent stays off",
    { volume: "locked", setup: "success", setupLogs: "most likely another gateway's TPM", tpmLooks: { outcome: "gave-up", answers: ["keyed 0x81000100"] } },
  );
  await unchanged("14-tpm-with-a-key");

  // TPM A cleared: its owner hierarchy has a new seed, so no TPM can ever open the volume again.
  // The setup sees the same other owner hierarchy in DATA_CHECK.sameAnswers looks in a row, wipes
  // the volume and makes it anew for this TPM, with the mark; the agent stays off.
  // Before the clear, the copy of TPM A gets the key a registered gateway holds: TPM2_Clear must
  // remove it too, or the setup would take the cleared TPM for another gateway's and not wipe.
  const tpmCleared = join(W, "tpm/A-cleared");
  await run(["cp", "-a", tpmA, tpmCleared]);
  const aHandles = await addGatewayKey(tpmCleared);
  const clearedHandles = await clearTpm(tpmCleared);
  record(
    "15-tpm-a-cleared: TPM A's copy held its storage key and a key at 0x81000100, and TPM2_Clear removed both (made on the build host)",
    [
      ...(aHandles === "- 0x81000001\n- 0x81000100" ? [] : [`before the clear, tpm2_getcap handles-persistent printed ${JSON.stringify(aHandles)}`]),
      ...(clearedHandles === "" ? [] : [`after the clear, tpm2_getcap handles-persistent printed ${JSON.stringify(clearedHandles)}`]),
    ],
    "",
  );
  const sealedA = ownerToken(header);
  await checkBoot(
    { name: "15-tpm-a-cleared", disk, tpm: tpmCleared },
    rootPartuuid,
    "TPM A after TPM2_Clear can never open it again: the setup wipes it and makes it anew for this TPM, with the mark; no prompt, the agent stays off",
    { volume: "unlocked", setup: "success", setupLogs: WIPED, mark: true, markerGone: true, tpmLooks: { outcome: "permanent", answers: [] } },
  );
  const wipedHeader = await luksJson(disk);
  record("15-tpm-a-cleared: the mark names the owner hierarchy the volume was sealed under and the new one (read on the build host)", wipeMarkProblems(wipedHeader, sealedA), "");
  record("15-tpm-a-cleared: LUKS2 header has the TPM token as its only keyslot, and the mark (read on the build host)", luksHeaderProblems(wipedHeader, { mark: true }), "");

  // The volume made for the cleared TPM: it opens at once, the mark stays, the agent stays off.
  const marker2 = new Bun.CryptoHasher("sha256").update(crypto.getRandomValues(new Uint8Array(32))).digest("hex").slice(0, 32);
  await checkBoot(
    { name: "16-tpm-a-cleared-again", disk, tpm: tpmCleared, marker: marker2 },
    rootPartuuid,
    "the cleared TPM opens the new volume at the next boot, without a wipe; the mark stays and the agent stays off",
    { volume: "unlocked", setup: "success", setupLogs: OPENS, mark: true, writeMarker: marker2, readMarker: marker2 },
  );
  record("16-tpm-a-cleared-again: the LUKS2 header is the one the wipe wrote (read on the build host)", (await luksJson(disk)) === wipedHeader ? [] : ["the header changed after the wipe"], "");

  // Another TPM again: wiped again, and the mark now names the cleared TPM's owner hierarchy.
  await checkBoot(
    { name: "17-tpm-b", disk, tpm: tpmB },
    rootPartuuid,
    "TPM B cannot open the volume made for the cleared TPM: wiped again for TPM B, with the mark; the agent stays off",
    { volume: "unlocked", setup: "success", setupLogs: WIPED, mark: true, markerGone: true, tpmLooks: { outcome: "permanent", answers: [] } },
  );
  const wipedAgain = await luksJson(disk);
  record("17-tpm-b: the mark names the cleared TPM's owner hierarchy and TPM B's (read on the build host)", wipeMarkProblems(wipedAgain, ownerToken(wipedHeader)), "");

  // The factory removes the mark (README.md, section 「TPM 永久解不開時」): TPM B opens the volume
  // and the agent starts.
  await removeToken(disk, LUKS.markToken.id);
  const marker3 = new Bun.CryptoHasher("sha256").update(crypto.getRandomValues(new Uint8Array(32))).digest("hex").slice(0, 32);
  await checkBoot(
    { name: "18-mark-removed", disk, tpm: tpmB, marker: marker3 },
    rootPartuuid,
    "after the mark is removed, TPM B opens the volume and the agent starts",
    { volume: "unlocked", setup: "success", setupLogs: OPENS, writeMarker: marker3, readMarker: marker3 },
  );
  record("18-mark-removed: LUKS2 header has the TPM token as its only keyslot, without the mark (read on the build host)", luksHeaderProblems(await luksJson(disk)), "");

  // The first boot loses its power after each step of the setup (the power-cut copy, each time on
  // a fresh copy with an all-zero data partition and a TPM of its own). The next boot with that
  // TPM must encrypt the partition again or open it, never refuse it, and leave only the TPM's
  // keyslot.
  let n = 19;
  // What a cut at mkfs leaves (before the next boot), kept for the setups that start again below.
  const afterMkfs = { disk: join(W, "power-cut-mkfs-kept.img"), tpm: join(W, "tpm/P-mkfs-kept") };
  for (const cut of POWER_CUTS) {
    const disk = join(W, `power-cut-${cut.step}.img`);
    const tpm = join(W, `tpm/P-${cut.step}`);
    await run(["cp", "--sparse=always", powerCutDisk, disk]);
    await checkCutAndNextBoot({ n, tag: cut.step, disk, tpm, cut, ...(cut.step === "mkfs" ? { keepCut: afterMkfs } : {}) }, rootPartuuid);
    await rm(disk);
    n += 1;
  }

  // A setup that starts again on what a power cut at mkfs left (a "new" header in front of the
  // earlier attempt's file system, so the partition is no longer zeros behind the header) loses
  // its power after each of its steps until the new header and its keyslot area are on the
  // partition; after that it runs the first boot's steps again. No cut may leave a partition the
  // next boot refuses (Laysi/trustiot-v2#917). Each starts from a fresh copy of that disk and TPM.
  if (!(await Bun.file(afterMkfs.disk).exists())) throw new Error("no power cut at mkfs to start again from");
  for (const cut of RESTART_CUTS) {
    const tag = `again-${cut.step}`;
    const disk = join(W, `power-cut-${tag}.img`);
    const tpm = join(W, `tpm/P-${tag}`);
    await run(["cp", "--sparse=always", afterMkfs.disk, disk]);
    await run(["cp", "-a", afterMkfs.tpm, tpm]);
    record(
      `${n}a-power-cut-${tag}: before the setup starts again, the data partition holds the earlier attempt's file system behind the header (read on the build host)`,
      (await dataAreaIsZero(disk)) ? ["the data partition is zeros behind the header, not what a power cut at mkfs leaves"] : [],
      "",
    );
    await checkCutAndNextBoot({ n, tag, disk, tpm, cut }, rootPartuuid);
    await rm(disk);
    n += 1;
  }

  // A wipe loses its power after the steps where its mark could get lost (WIPE_CUTS; the steps in
  // between are the first boot's, which the cuts above cover). Each starts from a copy of a volume
  // TPM W made on the power-cut copy, with a new TPM of its own, so the setup wipes it; the next
  // boot with that TPM must end with the volume sealed under it and the mark in its header.
  const sealedW = join(W, "power-cut-wipe-base.img");
  const tpmW = join(W, "tpm/W");
  await run(["cp", "--sparse=always", powerCutDisk, sealedW]);
  await checkBoot({ name: `${n}-wipe-base-tpm-w`, disk: sealedW, tpm: tpmW }, rootPartuuid, "TPM W encrypts the power-cut copy's partition (the volume the wipes below start from)", {
    volume: "unlocked",
    setup: "success",
    setupLogs: "is empty: first boot",
  });
  n += 1;
  for (const cut of WIPE_CUTS) {
    const tag = `wipe-${cut.step}`;
    const disk = join(W, `power-cut-${tag}.img`);
    const tpm = join(W, `tpm/P-${tag}`);
    await run(["cp", "--sparse=always", sealedW, disk]);
    await checkCutAndNextBoot({ n, tag, disk, tpm, cut, mark: true }, rootPartuuid);
    await rm(disk);
    n += 1;
  }

  // A TPM whose 0x81000001 holds a key that is not its owner hierarchy's standard storage key: the
  // setup does not seal the volume under it, and refuses before it writes anything to the
  // partition (a wipe formats through the same check). On a copy of the power-cut copy, whose
  // data partition is all zeros (its stand-ins run the real programs on this boot).
  const tpmX = join(W, "tpm/X");
  const xHandles = await addForeignStorageKey(tpmX);
  record(`${n}-foreign-storage-key: the TPM holds an RSA storage key at 0x81000001 and nothing else (made on the build host)`, xHandles === "- 0x81000001" ? [] : [`tpm2_getcap handles-persistent printed ${JSON.stringify(xHandles)}`], "");
  const foreignKeyDisk = join(W, "foreign-storage-key.img");
  await run(["cp", "--sparse=always", powerCutDisk, foreignKeyDisk]);
  await checkBoot(
    { name: `${n}-foreign-storage-key`, disk: foreignKeyDisk, tpm: tpmX },
    rootPartuuid,
    "a TPM with another key at 0x81000001: the setup refuses to seal the volume under it; not encrypted, not mounted, the agent stays off",
    { volume: "locked", setup: "failed", setupLogs: "0x81000001 holds a key that is not this TPM's standard storage key" },
  );
  record(`${n}-foreign-storage-key: the data partition is still all zeros`, (await dataIsZero(foreignKeyDisk)) ? [] : ["the data partition was written"], "");
  await rm(foreignKeyDisk);
  n += 1;

  // The same three setups once more, each recorded write by write (QEMU's write log, with the
  // steps marked): on an all-zero partition, over what a power cut at mkfs left, and a wipe of
  // TPM W's volume by a new TPM. Every state a cut inside a step, a lost or reordered card cache
  // or a torn write can leave (src/write-log.ts) goes to the image's own setup, which must set
  // the partition up again, open it, or refuse it with the agent off (fail closed); a wipe's
  // mark must never get lost. One state of each outcome, and on the first boot a cut in the
  // middle of each step, is then booted.
  const tpmWOwner = ownerToken(await luksJson(sealedW));
  if (!tpmWOwner) throw new Error("TPM W's volume has no owner fingerprint");
  const classifier = await Classifier.open(powerCutRoot, CLASSIFY_ROOT);
  try {
    const zeros = join(W, "zero-partition.img");
    await run(["truncate", "-s", String(dataPart().size * SECTOR), zeros]);
    const recordings: Recording[] = [
      {
        path: "first-boot",
        start: powerCutDisk,
        startTpm: null,
        base: zeros,
        before: null,
        what: "TPM R1 encrypts the power-cut copy's all-zero partition, with every write logged",
        expect: { volume: "unlocked", setup: "success", setupLogs: "is empty: first boot" },
      },
      {
        path: "start-again",
        start: afterMkfs.disk,
        startTpm: afterMkfs.tpm,
        base: null,
        before: null,
        what: "the TPM of the power cut at mkfs starts that setup again, with every write logged",
        expect: { volume: "unlocked", setup: "success", setupLogs: "holds a setup that did not finish (no data was ever written to it): starting again" },
      },
      {
        path: "wipe",
        start: sealedW,
        startTpm: null,
        base: null,
        before: tpmWOwner,
        what: "TPM R3 cannot open TPM W's volume: the setup wipes it and makes it anew for TPM R3, with the mark, with every write logged",
        expect: { volume: "unlocked", setup: "success", setupLogs: WIPED, mark: true, tpmLooks: { outcome: "permanent", answers: [] } },
      },
    ];
    const next = () => n++;
    for (const r of recordings) {
      // A recording that cannot be checked to the end is a failure of its own; the others still run.
      try {
        await checkRecordedSetup(r, next, powerCutDisk, classifier, rootPartuuid);
      } catch (error) {
        record(`record-${r.path}: the recorded setup was checked to the end`, [error instanceof Error ? (error.stack ?? error.message) : String(error)], "");
      }
    }
    await rm(zeros);
  } finally {
    await classifier.close();
  }
  await rm(afterMkfs.disk);
  await rm(sealedW);
  await rm(powerCutDisk);
  await rm(powerCutRoot);

  await writeFile(join(REPORT, "boot-test.json"), `${JSON.stringify(outcomes, null, 2)}\n`);
  await writeFile(join(REPORT, "luks-header.json"), redactTpmFields(header));
  await writeFile(join(REPORT, "luks-header-wiped.json"), redactTpmFields(wipedHeader));
  const failed = outcomes.filter((o) => o.problems.length > 0);
  console.log(`\n${outcomes.length - failed.length} of ${outcomes.length} checks passed`);
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
