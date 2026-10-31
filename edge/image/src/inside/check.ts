// The image check, run inside the tool container by check.ts. It reads only the finished image,
// the source image, the agent executable and the administrator's public keys; it shares the
// expected values (pins, file contents) with the build but none of the build's steps.
// README.md, section 「映像檢查」 lists what each check proves.
import { copyFile, lstat, mkdir, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { dirname, join, relative } from "node:path";
import {
  MASKED_SETUP_UNITS,
  SSHD_DROP_IN,
  SSHD_TEST_CONNECTIONS,
  SUDOERS_FILE,
  TIMESYNCD_DROP_IN,
  TPM_CONFIG_LINES,
  accountsWithPassword,
  allModelLines,
  cloudInitUnits,
  effectiveTimesyncd,
  findingProblem,
  groupMembers,
  groupsWithPassword,
  parseCmdline,
  parseColonFile,
  parseDpkgOwners,
  parseFstab,
  parseSshdDump,
  sshdAdmitsProblems,
  sshdMatchLines,
  sshdProblems,
  sudoersFile,
  sudoersProblems,
  tpmDeviceTreeProblems,
} from "../files.ts";
import { CRYPTTAB_OPTIONS, crypttabProblems, deviceTimeoutProblems, deviceUnit, effectiveSetting, generatedUnitProblems, parseShellAssignments, syslogDaemons } from "../data.ts";
import { aarch64ElfProblem, authorizedKeys, keyFingerprint, parsePublicKeys } from "../inputs.ts";
import {
  AUTHORIZED_ATTRIBUTE,
  BOOT_ARGUMENTS,
  BROAD_SCAN,
  BROAD_SCAN_MAX_BYTES,
  BROAD_SCAN_SKIP,
  CONSOLE_INSTANCES,
  LOGIND_DROP_IN,
  LOGIND_EXPECTED,
  MASKED_CONSOLE_UNITS,
  MASKED_GENERATORS,
  MASKED_SSH_SOCKETS,
  PANIC_ARGUMENT,
  SULOGIN_UNITS,
  SYSFS_CONFIG,
  TMPFILES_DIRS,
  UDEV_RULES_DIRS,
  UNIT_DIRS,
  USB_AUTHORIZING_PACKAGES,
  USB_AUTHORIZING_UNITS,
  cmdlineProblems,
  configCmdlineRedirects,
  configUsbGadget,
  effectiveSection,
  initramfsPanicProblems,
  kernelConfigProblems,
  legacyStartupScript,
  lockedPassword,
  logindDropIn,
  readsTerminal,
  rulePrograms,
  rulesWritingAuthorized,
  sysfsWritingAuthorized,
  tmpfilesWritingAuthorized,
  unitOfFile,
} from "../login.ts";
import {
  BOOT_CODE_BYTES,
  compareTables,
  imageSectors,
  keepsDiskIdentifier,
  layoutProblems,
  parseSfdiskJson,
  partuuid,
  sourceTable,
  targetTable,
} from "../layout.ts";
import {
  ADMIN_USER,
  AGENT_SYSTEMD_FILES,
  AGENT_USER,
  DATA,
  DATA_FILES,
  IMAGE_PACKAGES,
  KNOWN_FINDINGS,
  LUKS,
  NTP_SERVERS,
  REMOVED_BOOT_FILES,
  SECTOR,
  SOURCE,
  SOURCE_USER,
  TPM_OVERLAY,
} from "../pins.ts";
import { compareTrees, isText, manifest, resolveInImage, textProblems, walk } from "../tree.ts";
import { Chroot, Ext4Mount, PATHS, extractPartition, installTools, run, sha256File, step } from "./common.ts";

const W = PATHS.work;
const ROOT = "/mnt/root";
const BASE = "/mnt/base";
const MTOOLS_ENV = { MTOOLS_SKIP_CHECK: "1" };
const REPORT = join(W, "report");

interface Outcome {
  name: string;
  problems: string[];
  detail: string;
}

const outcomes: Outcome[] = [];

/** Runs one named check; an exception counts as a failure of that check, not of the run. */
async function check(name: string, body: () => Promise<{ problems: string[]; detail?: string }>): Promise<void> {
  let outcome: Outcome;
  try {
    const result = await body();
    outcome = { name, problems: result.problems, detail: result.detail ?? "" };
  } catch (error) {
    outcome = { name, problems: [error instanceof Error ? error.message : String(error)], detail: "" };
  }
  outcomes.push(outcome);
  const text = outcome.problems.length === 0 ? outcome.detail : outcome.problems.join("\n      ");
  console.log(`${outcome.problems.length === 0 ? "PASS" : "FAIL"}  ${name}${text ? `: ${text}` : ""}`);
}

async function mtype(boot: string, path: string): Promise<string> {
  return (await run(["mtype", "-i", boot, `::${path}`], { env: MTOOLS_ENV })).stdout;
}

async function mfetch(boot: string, path: string, out: string): Promise<void> {
  await run(["mcopy", "-n", "-i", boot, `::${path}`, out], { env: MTOOLS_ENV });
}

async function fileInfo(path: string): Promise<{ mode: number; uid: number; gid: number; isDir: boolean } | null> {
  const info = await stat(path).catch(() => null);
  return info ? { mode: info.mode & 0o7777, uid: info.uid, gid: info.gid, isDir: info.isDirectory() } : null;
}

function expectInfo(path: string, info: Awaited<ReturnType<typeof fileInfo>>, mode: number, uid: number, gid: number): string[] {
  if (!info) return [`${path} is missing`];
  const problems: string[] = [];
  if (info.mode !== mode) problems.push(`${path} has mode ${info.mode.toString(8)}, expected ${mode.toString(8)}`);
  if (info.uid !== uid || info.gid !== gid) problems.push(`${path} is owned by ${info.uid}:${info.gid}, expected ${uid}:${gid}`);
  return problems;
}

async function isEnabled(chroot: Chroot, unit: string): Promise<string> {
  return (await chroot.run(["systemctl", "is-enabled", unit], { okCodes: [0, 1] })).stdout.trim();
}

function account(passwd: string, name: string): string[] | undefined {
  return parseColonFile(passwd).find((f) => f[0] === name);
}

async function main(): Promise<void> {
  await installTools();
  await mkdir(REPORT, { recursive: true });
  const target = targetTable();
  const labelId = target.labelId;

  step("partition table");
  await check("partition table", async () => {
    const actual = parseSfdiskJson((await run(["sfdisk", "--json", PATHS.image])).stdout);
    const problems = [...compareTables(actual, target), ...layoutProblems(actual)];
    const bytes = Bun.file(PATHS.image).size;
    if (bytes !== imageSectors(target) * SECTOR) problems.push(`image has ${bytes} bytes, expected ${imageSectors(target) * SECTOR}`);
    const parts = actual.partitions.map((p) => `p${p.number} ${p.start}+${p.size} type ${p.type}`).join(", ");
    return { problems, detail: `${actual.labelId}: ${parts}` };
  });

  step("extract");
  const boot = join(W, "boot.vfat");
  const root = join(W, "root.ext4");
  const data = join(W, "data.ext4");
  for (const [file, number] of [[boot, 1], [root, 2], [data, DATA.number]] as const) {
    const p = target.partitions.find((x) => x.number === number);
    if (!p) throw new Error(`no partition ${number}`);
    await extractPartition(PATHS.image, p.start, p.size, file);
  }
  const sourceSha = await sha256File(PATHS.source);
  if (sourceSha !== SOURCE.sha256) throw new Error(`source image SHA-256 ${sourceSha}, expected ${SOURCE.sha256}`);
  const source = join(W, "source.img");
  await run(["sh", "-c", 'xz --decompress --stdout -- "$1" > "$2"', "xz", PATHS.source, source]);
  const baseBoot = join(W, "base-boot.vfat");
  const baseRoot = join(W, "base-root.ext4");
  const [sBoot, sRoot] = sourceTable().partitions;
  if (!sBoot || !sRoot) throw new Error("source table needs two partitions");
  await extractPartition(source, sBoot.start, sBoot.size, baseBoot);
  await extractPartition(source, sRoot.start, sRoot.size, baseRoot);
  await check("MBR boot code is the source image's (first boot keeps the disk identifier)", async () => {
    const [mine, theirs] = [
      new Uint8Array(await Bun.file(PATHS.image).slice(0, BOOT_CODE_BYTES).arrayBuffer()),
      new Uint8Array(await Bun.file(source).slice(0, BOOT_CODE_BYTES).arrayBuffer()),
    ];
    const problems: string[] = [];
    if (!Buffer.from(mine).equals(Buffer.from(theirs))) problems.push("bytes 0-439 differ from the source image's");
    if (!keepsDiskIdentifier(mine)) problems.push("the boot code area starts with a zero byte: parted would pick a new disk identifier");
    return { problems, detail: `first byte 0x${(mine[0] ?? 0).toString(16).padStart(2, "0")}` };
  });
  await rm(source);

  await check("file systems are clean", async () => {
    const problems: string[] = [];
    const r = await run(["e2fsck", "-f", "-n", root], { okCodes: [0, 1, 2, 4, 8] });
    if (r.code !== 0) problems.push(`e2fsck ${root}: exit ${r.code}: ${r.stdout.trim().slice(-500)}`);
    const f = await run(["fsck.vfat", "-n", boot], { okCodes: [0, 1, 2] });
    if (f.code !== 0) problems.push(`fsck.vfat: exit ${f.code}: ${f.stdout.trim().slice(-500)}`);
    return { problems, detail: "e2fsck root, fsck.vfat boot" };
  });

  await check("data partition is all zeros (the gateway encrypts it on its first boot)", async () => {
    // trustiot-data-setup formats only a partition in which blkid finds nothing and whose LUKS2
    // header area is zero; the whole partition is zero, so no data of the build reaches the card.
    const problems: string[] = [];
    const bytes = DATA.sizeSectors * SECTOR;
    if (Bun.file(data).size !== bytes) problems.push(`the extracted partition has ${Bun.file(data).size} bytes, expected ${bytes}`);
    const zero = await run(["cmp", "--silent", `--bytes=${bytes}`, data, "/dev/zero"], { okCodes: [0, 1] });
    if (zero.code !== 0) problems.push("the data partition is not all zeros");
    const probe = await run(["blkid", "--probe", "--output", "export", data], { okCodes: [0, 2] });
    if (probe.code !== 2) problems.push(`blkid finds a format on the data partition: ${probe.stdout.trim().replace(/\n/g, " ")}`);
    return { problems, detail: `${bytes} bytes of zeros, no format blkid knows` };
  });

  const rootMount = await Ext4Mount.open(root, ROOT, true);
  const baseMount = await Ext4Mount.open(baseRoot, BASE, true);
  const chroot = new Chroot(ROOT);
  try {
    step("changes against the source image");
    const [baseTree, newTree] = [await walk(BASE), await walk(ROOT)];
    const delta = await compareTrees(BASE, baseTree, ROOT, newTree);
    await writeFile(
      join(REPORT, "delta.tsv"),
      [...delta.added.map((p) => `added\t${p}`), ...delta.changed.map((p) => `changed\t${p}`), ...delta.removed.map((p) => `removed\t${p}`)].join("\n") + "\n",
    );
    console.log(`added ${delta.added.length}, changed ${delta.changed.length}, removed ${delta.removed.length} (report/delta.tsv)`);
    if (process.env.TRUSTIOT_IMAGE_MANIFEST === "1") {
      await writeFile(join(REPORT, "manifest-root.tsv"), await manifest(ROOT, newTree));
      console.log("manifest of the root file system: report/manifest-root.tsv");
    }

    await chroot.enter({ network: false });
    const passwd = await readFile(join(ROOT, "etc/passwd"), "utf8");
    const agentUser = account(passwd, "trustiot-agent");
    const adminUser = account(passwd, ADMIN_USER);

    step("data partition");
    await check("data partition encryption, unlock and mount are installed", async () => {
      const problems: string[] = [];
      for (const { repo, image, mode } of DATA_FILES) {
        const [a, b] = [await readFile(join(PATHS.src, "files/data", repo)), await readFile(join(ROOT, image.slice(1))).catch(() => null)];
        if (!b || !a.equals(b)) problems.push(`${image} differs from edge/image/files/data/${repo}`);
        problems.push(...expectInfo(image, await fileInfo(join(ROOT, image.slice(1))), mode, 0, 0));
      }
      problems.push(...crypttabProblems(await readFile(join(ROOT, "etc/crypttab"), "utf8").catch(() => "")));
      // The units systemd makes from /etc/crypttab and /etc/fstab at boot, made here by the image's
      // own generators (the fstab generator's into a directory of its own, which systemd-analyze
      // verify below does not read). The fstab generator skips every device entry when /sys is
      // read-only, as in the tool container, unless SYSTEMD_SYSFS_CHECK=0 (parse_fstab_one() and
      // sysfs_check() in src/fstab-generator/fstab-generator.c of systemd v257, read 2026-10-31 at
      // https://github.com/systemd/systemd/blob/v257/src/fstab-generator/fstab-generator.c).
      const generatorDirs = { "systemd-cryptsetup-generator": "run/systemd/generator", "systemd-fstab-generator": "run/trustiot-check/fstab-generator" };
      for (const [generator, dir] of Object.entries(generatorDirs)) {
        await rm(join(ROOT, dir), { recursive: true, force: true });
        await mkdir(join(ROOT, dir), { recursive: true });
        await chroot.run([`/usr/lib/systemd/system-generators/${generator}`, `/${dir}`, `/${dir}.early`, `/${dir}.late`], {
          env: { SYSTEMD_PROC_CMDLINE: "", SYSTEMD_SYSFS_CHECK: "0" },
        });
      }
      // The volume and the unlocked volume each get the device timeout (DATA.deviceTimeout), so the
      // first boot's setup has the time it needs.
      for (const [device, dirOf] of [
        [`/dev/disk/by-uuid/${LUKS.uuid}`, generatorDirs["systemd-cryptsetup-generator"]],
        [`/dev/mapper/${LUKS.mapping}`, generatorDirs["systemd-fstab-generator"]],
      ] as const) {
        const dir = join(ROOT, dirOf, `${deviceUnit(device)}.d`);
        const names = (await readdir(dir).catch(() => [] as string[])).filter((n) => n.endsWith(".conf")).sort();
        const dropIns = (await Promise.all(names.map((n) => readFile(join(dir, n), "utf8")))).join("\n");
        problems.push(...deviceTimeoutProblems(deviceUnit(device), dropIns));
      }
      const unitName = `systemd-cryptsetup@${LUKS.mapping}.service`;
      const generated = await readFile(join(ROOT, "run/systemd/generator", unitName), "utf8").catch(() => "");
      if (generated === "") problems.push(`systemd-cryptsetup-generator made no ${unitName}`);
      else problems.push(...generatedUnitProblems(generated));
      // nofail: the volume is wanted by cryptsetup.target, not required, so boot does not wait for it.
      if (!(await Bun.file(join(ROOT, "run/systemd/generator/cryptsetup.target.wants", unitName)).exists())) problems.push(`cryptsetup.target does not want ${unitName}`);
      if (await Bun.file(join(ROOT, "run/systemd/generator/cryptsetup.target.requires", unitName)).exists()) problems.push(`cryptsetup.target requires ${unitName}`);
      // Every unit and drop-in of the data partition loads without errors (the generated unit by
      // its path, with the image's drop-in for it).
      const verify = await chroot.run(["systemd-analyze", "verify", "--man=no", "trustiot-data-setup.service", `/run/systemd/generator/${unitName}`, "trustiot-agent.service"], {
        okCodes: [0, 1],
        env: { SYSTEMD_LOG_COLOR: "0" },
      });
      if (verify.code !== 0) problems.push(`systemd-analyze verify: ${(verify.stdout + verify.stderr).trim().slice(-1000)}`);
      if ((await isEnabled(chroot, "trustiot-agent.service")) !== "enabled") problems.push("trustiot-agent.service is not enabled");
      return {
        problems,
        detail: `${DATA_FILES.length} files as in files/data; /etc/crypttab ${LUKS.mapping} UUID=${LUKS.uuid} none ${CRYPTTAB_OPTIONS.join(",")}; generated ${unitName} matches; device timeout ${DATA.deviceTimeout} for the volume and the unlocked volume`,
      };
    });

    await check("data partition is mounted at the agent's state directory, and only from the unlocked volume", async () => {
      const problems: string[] = [];
      const fstab = parseFstab(await readFile(join(ROOT, "etc/fstab"), "utf8"));
      const entries = fstab.filter((e) => e.file === DATA.mountPoint);
      const entry = entries[0];
      const spec = `/dev/mapper/${LUKS.mapping}`;
      if (entries.length !== 1 || !entry) problems.push(`/etc/fstab has ${entries.length} entries for ${DATA.mountPoint}`);
      else {
        if (entry.spec !== spec) problems.push(`source ${entry.spec}, expected ${spec}`);
        if (entry.vfstype !== "ext4") problems.push(`type ${entry.vfstype}`);
        for (const option of ["nodev", "nosuid", "noexec", "nofail", `x-systemd.device-timeout=${DATA.deviceTimeout}`]) if (!entry.options.includes(option)) problems.push(`option ${option} is missing`);
        if (entry.passno !== "2") problems.push(`fsck pass ${entry.passno}, expected 2`);
      }
      const rootEntry = fstab.find((e) => e.file === "/");
      const bootEntry = fstab.find((e) => e.file === "/boot/firmware");
      if (rootEntry?.spec !== `PARTUUID=${partuuid(labelId, 2)}`) problems.push(`root is mounted from ${rootEntry?.spec}`);
      if (bootEntry?.spec !== `PARTUUID=${partuuid(labelId, 1)}`) problems.push(`boot is mounted from ${bootEntry?.spec}`);
      // Nothing may ever be on the root partition underneath the mount.
      problems.push(...expectInfo(DATA.mountPoint, await fileInfo(join(ROOT, DATA.mountPoint)), 0o700, 0, 0));
      const underneath = [...newTree.keys()].filter((p) => p.startsWith(`${DATA.mountPoint}/`));
      if (underneath.length > 0) problems.push(`${DATA.mountPoint} on the root partition is not empty: ${underneath.join(", ")}`);
      const unit = await readFile(join(ROOT, "usr/lib/systemd/system/trustiot-agent.service"), "utf8");
      if (!/^StateDirectory=trustiot-agent$/m.test(unit)) problems.push("the agent unit has no StateDirectory=trustiot-agent");
      const dropIn = await readFile(join(ROOT, "usr/lib/systemd/system/trustiot-agent.service.d/trustiot-data.conf"), "utf8").catch(() => "");
      for (const line of [`RequiresMountsFor=${DATA.mountPoint}`, `AssertPathIsMountPoint=${DATA.mountPoint}`]) {
        if (!dropIn.split("\n").includes(line)) problems.push(`the agent's drop-in has no ${line}`);
      }
      return { problems, detail: `${spec} ${DATA.mountPoint} ext4 ${DATA.mountOptions}; the agent requires the mount and asserts it` };
    });

    await check("logs, swap and core dumps stay off the root partition", async () => {
      const problems: string[] = [];
      const journald = (await chroot.run(["systemd-analyze", "cat-config", "systemd/journald.conf"])).stdout;
      const storage = effectiveSetting(journald, "Journal", "Storage");
      if (storage !== "volatile") problems.push(`journald Storage=${storage ?? "(default: auto)"}, expected volatile`);
      const forward = effectiveSetting(journald, "Journal", "ForwardToSyslog");
      if (forward !== "no") problems.push(`journald ForwardToSyslog=${forward ?? "(default)"}, expected no`);
      const packages = (await chroot.run(["dpkg-query", "-W", "-f", "${Package} ${db:Status-Abbrev} ${Provides}\\n"])).stdout;
      const daemons = syslogDaemons(packages);
      if (daemons.length > 0) problems.push(`syslog daemons installed: ${daemons.join(", ")}`);
      // rpi-swap reads its settings with systemd's drop-in rules; its generator does the same.
      const swap = parseShellAssignments((await chroot.run(["rpi-systemd-config", "rpi/swap.conf", "CONF_MECHANISM", "Main::Mechanism"])).stdout);
      if (swap.get("CONF_MECHANISM") !== "zram") problems.push(`rpi-swap Mechanism=${swap.get("CONF_MECHANISM") ?? "(default: auto, zram+file)"}, expected zram`);
      if (await stat(join(ROOT, "var/swap")).catch(() => null)) problems.push("/var/swap exists on the root partition");
      for (const unit of ["trustiot-data-setup.service", `systemd-cryptsetup@${LUKS.mapping}.service.d/trustiot-data.conf`, "trustiot-agent.service.d/trustiot-data.conf"]) {
        const text = await readFile(join(ROOT, "usr/lib/systemd/system", unit), "utf8").catch(() => "");
        if (!text.split("\n").includes("LimitCORE=0")) problems.push(`${unit} has no LimitCORE=0`);
      }
      return {
        problems,
        detail: "journald Storage=volatile, ForwardToSyslog=no, no syslog daemon; rpi-swap Mechanism=zram, no /var/swap; LimitCORE=0 for the setup, the unlock and the agent",
      };
    });

    step("first boot");
    await check("first boot grows root and leaves boot and data in place", async () => {
      // What the source image's initramfs (resize_early) runs, on a copy of the partition table
      // in a 16 GiB file standing in for a larger card.
      const problems: string[] = [];
      const card = join(ROOT, "tmp/card.img");
      const cardSectors = (16 * 1024 * 1024 * 1024) / SECTOR;
      await run(["dd", `if=${PATHS.image}`, `of=${card}`, "bs=512", "count=1", "status=none"]);
      await run(["truncate", "-s", String(cardSectors * SECTOR), card]);
      await chroot.run(["parted", "-m", "/tmp/card.img", "u", "s", "resizepart", "2", String(cardSectors - 1)]);
      const grown = parseSfdiskJson((await run(["sfdisk", "--json", card])).stdout);
      const rootPart = target.partitions.find((p) => p.number === 2);
      const expected = {
        labelId,
        partitions: target.partitions.map((p) => (p.number === 2 ? { ...p, size: cardSectors - p.start } : p)),
      };
      problems.push(...compareTables(grown, expected));
      await rm(card);
      return { problems, detail: `parted resizepart 2: root ${rootPart?.size} -> ${cardSectors - (rootPart?.start ?? 0)} sectors, p1 and p3 unchanged` };
    });

    await check("first boot state is kept (machine ID, no SSH host keys)", async () => {
      const problems: string[] = [];
      const machineId = (await readFile(join(ROOT, "etc/machine-id"), "utf8")).trim();
      if (machineId !== "uninitialized") problems.push(`/etc/machine-id is ${JSON.stringify(machineId)}`);
      const hostKeys = [...newTree.keys()].filter((p) => /^\/etc\/ssh\/ssh_host_.*key/.test(p));
      if (hostKeys.length > 0) problems.push(`SSH host keys in the image: ${hostKeys.join(", ")}`);
      if ((await isEnabled(chroot, "regenerate_ssh_host_keys.service")) !== "enabled") problems.push("regenerate_ssh_host_keys.service is not enabled");
      const cmdline = parseCmdline(await mtype(boot, "cmdline.txt"));
      if (!cmdline.includes(`root=PARTUUID=${partuuid(labelId, 2)}`)) problems.push(`cmdline.txt root is not partition 2: ${cmdline.join(" ")}`);
      if (!cmdline.includes("resize")) problems.push("cmdline.txt has no resize (first boot would not grow root or set a new disk identifier)");
      for (const token of cmdline) if (/^(init|systemd\.run)=/.test(token)) problems.push(`cmdline.txt runs ${token}`);
      return { problems, detail: "machine-id uninitialized, no host keys, cmdline.txt resize" };
    });

    step("agent");
    await check("agent executable is the given build", async () => {
      const problems: string[] = [];
      const installed = join(ROOT, "usr/bin/trustiot-agent");
      const [want, have] = [await sha256File(PATHS.agent), await sha256File(installed)];
      if (want !== have) problems.push(`SHA-256 ${have}, expected ${want}`);
      problems.push(...expectInfo("/usr/bin/trustiot-agent", await fileInfo(installed), 0o755, 0, 0));
      const elf = aarch64ElfProblem(new Uint8Array(await Bun.file(installed).slice(0, 64).arrayBuffer()));
      if (elf) problems.push(elf);
      const version = await chroot.run(["/usr/bin/trustiot-agent", "--version"], { okCodes: [0, 1, 2, 126, 127] });
      if (version.code !== 0) problems.push(`trustiot-agent --version exited ${version.code}: ${version.stderr.trim()}`);
      return { problems, detail: `SHA-256 ${have}, aarch64 ELF, runs on the image's C library (${version.stdout.trim()})` };
    });

    await check("agent unit and system user are installed and enabled", async () => {
      const problems: string[] = [];
      for (const { repo, image } of AGENT_SYSTEMD_FILES) {
        const [a, b] = [await readFile(join(PATHS.agentSystemd, repo)), await readFile(join(ROOT, image.slice(1))).catch(() => null)];
        if (!b || !a.equals(b)) problems.push(`${image} differs from edge/agent/systemd/${repo}`);
        problems.push(...expectInfo(image, await fileInfo(join(ROOT, image.slice(1))), 0o644, 0, 0));
      }
      const enabled = await isEnabled(chroot, "trustiot-agent.service");
      if (enabled !== "enabled") problems.push(`trustiot-agent.service is ${enabled}`);
      if (!agentUser) problems.push("no trustiot-agent user");
      else if (agentUser[5] !== DATA.mountPoint || agentUser[6] !== "/usr/sbin/nologin") problems.push(`trustiot-agent user: ${agentUser.join(":")}`);
      const group = parseColonFile(await readFile(join(ROOT, "etc/group"), "utf8")).find((f) => f[0] === "tss");
      if (!group) problems.push("no tss group (the unit's SupplementaryGroups=tss)");
      const rules = await readFile(join(ROOT, "usr/lib/udev/rules.d/60-tpm-udev.rules"), "utf8").catch(() => "");
      if (!/KERNEL=="tpmrm\[0-9\]\*".*GROUP="tss"/.test(rules)) problems.push("no udev rule giving /dev/tpmrm0 to the tss group");
      if (await Bun.file(join(ROOT, "etc/trustiot/agent.toml")).exists()) problems.push("/etc/trustiot/agent.toml is in the image (it comes from the factory)");
      return { problems, detail: `enabled, user ${agentUser?.slice(0, 4).join(":")}, tss group ${group?.[2]}, tpmrm0 udev rule` };
    });

    await check("added packages at the pinned versions", async () => {
      const problems: string[] = [];
      const listed = (await chroot.run(["dpkg-query", "-W", "-f", "${Package} ${Version} ${db:Status-Abbrev}\\n", ...Object.keys(IMAGE_PACKAGES)])).stdout;
      for (const [name, version] of Object.entries(IMAGE_PACKAGES)) {
        if (!new RegExp(`^${name} ${version.replace(/[+.]/g, "\\$&")} ii `, "m").test(listed)) problems.push(`${name} is not installed at ${version}: ${listed.trim()}`);
      }
      const audit = (await chroot.run(["dpkg", "--audit"])).stdout.trim();
      if (audit !== "") problems.push(`dpkg --audit: ${audit}`);
      return { problems, detail: listed.trim().replace(/\s+ii\s*/g, "; ").replace(/;\s*$/, "") };
    });

    step("TPM overlay");
    await check("TPM overlay is installed and loaded for every model", async () => {
      const problems: string[] = [];
      await copyFile(join(PATHS.src, `files/${TPM_OVERLAY}-overlay.dts`), join(ROOT, "tmp/overlay.dts"));
      await chroot.run(["dtc", "-q", "-@", "-I", "dts", "-O", "dtb", "-o", "/tmp/expected.dtbo", "/tmp/overlay.dts"]);
      await mfetch(boot, `overlays/${TPM_OVERLAY}.dtbo`, join(ROOT, "tmp/installed.dtbo"));
      const [expected, installed] = [await readFile(join(ROOT, "tmp/expected.dtbo")), await readFile(join(ROOT, "tmp/installed.dtbo"))];
      if (!expected.equals(installed)) problems.push(`overlays/${TPM_OVERLAY}.dtbo is not files/${TPM_OVERLAY}-overlay.dts compiled`);
      const lines = allModelLines(await mtype(boot, "config.txt"));
      for (const line of TPM_CONFIG_LINES) if (!lines.includes(line)) problems.push(`config.txt does not have ${line} for every model`);
      await mfetch(boot, "bcm2711-rpi-4-b.dtb", join(ROOT, "tmp/base.dtb"));
      await chroot.run(["dtmerge", "/tmp/base.dtb", "/tmp/merged.dtb", "/tmp/installed.dtbo"]);
      const merged = (await chroot.run(["dtc", "-q", "-I", "dtb", "-O", "dts", "/tmp/merged.dtb"])).stdout;
      const mergedProblems = tpmDeviceTreeProblems(merged);
      if (mergedProblems.length > 0) {
        // Kept for a look at what the merge produced.
        await writeFile(join(REPORT, "merged-bcm2711-rpi-4-b.dts"), merged);
        problems.push(...mergedProblems.map((p) => `after dtmerge: ${p}`));
      }
      return { problems, detail: `${TPM_CONFIG_LINES.join(", ")}; dtmerge onto bcm2711-rpi-4-b.dtb gives slb9670@0 on spi0, spidev0 disabled` };
    });

    step("time");
    await check("NTP servers", async () => {
      const problems: string[] = [];
      const effective = effectiveTimesyncd((await chroot.run(["systemd-analyze", "cat-config", "systemd/timesyncd.conf"])).stdout);
      if (effective.ntp.join(" ") !== NTP_SERVERS.join(" ")) problems.push(`NTP=${effective.ntp.join(" ")}, expected ${NTP_SERVERS.join(" ")}`);
      if (effective.fallback === undefined || effective.fallback.length > 0) problems.push(`FallbackNTP is ${effective.fallback?.join(" ") ?? "the built-in pool"}`);
      if (!(await Bun.file(join(ROOT, TIMESYNCD_DROP_IN)).exists())) problems.push(`${TIMESYNCD_DROP_IN} is missing`);
      const enabled = await isEnabled(chroot, "systemd-timesyncd.service");
      if (enabled !== "enabled") problems.push(`systemd-timesyncd.service is ${enabled}`);
      return { problems, detail: `NTP=${effective.ntp.join(" ")}, FallbackNTP empty, systemd-timesyncd enabled` };
    });

    step("login");
    await check("sshd accepts public keys only, for the administrator only", async () => {
      const problems = sshdProblems(parseSshdDump((await chroot.run(["sshd", "-G"])).stdout));
      // A Match block applies per connection, which plain sshd -G does not evaluate: sshd -T does,
      // with a throwaway host key and privilege separation directory on the chroot's tmpfs mounts.
      await mkdir(join(ROOT, "run/sshd"), { recursive: true, mode: 0o755 });
      await chroot.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "", "-f", "/tmp/sshd-test-key"]);
      for (const connection of SSHD_TEST_CONNECTIONS) {
        const dump = parseSshdDump((await chroot.run(["sshd", "-T", "-h", "/tmp/sshd-test-key", "-C", connection])).stdout);
        problems.push(...sshdProblems(dump).map((p) => `${connection}: ${p}`));
        problems.push(...sshdAdmitsProblems(dump, [AGENT_USER, "root", SOURCE_USER]).map((p) => `${connection}: ${p}`));
        // For the report: who sshd -T lets log in on this connection, and how.
        const decided = ["allowusers", "denyusers", "authenticationmethods"].map((key) => `${key} ${(dump.get(key) ?? ["(none)"]).join(" ")}`);
        console.log(`sshd -T -C ${connection}: ${decided.join("; ")}`);
      }
      const configs = ["/etc/ssh/sshd_config", ...[...newTree.keys()].filter((p) => p.startsWith("/etc/ssh/sshd_config.d/"))];
      for (const path of configs) {
        if (newTree.get(path)?.type !== "file") continue;
        const lines = sshdMatchLines(await readFile(join(ROOT, path), "utf8"));
        if (lines.length > 0) problems.push(`${path} has a Match block (line ${lines.join(", ")})`);
      }
      const enabled = await isEnabled(chroot, "ssh.service");
      if (enabled !== "enabled") problems.push(`ssh.service is ${enabled}`);
      if (!(await Bun.file(join(ROOT, SSHD_DROP_IN)).exists())) problems.push(`${SSHD_DROP_IN} is missing`);
      if (await Bun.file(join(ROOT, "etc/ssh/sshd_config.d/rename_user.conf")).exists()) problems.push("rename_user.conf is still there");
      return {
        problems,
        detail: `sshd -G and sshd -T for ${SSHD_TEST_CONNECTIONS.length} connections (${ADMIN_USER}, root, pi, ${AGENT_USER}): permitrootlogin no, passwordauthentication no, kbdinteractiveauthentication no, authenticationmethods publickey, allowusers ${ADMIN_USER} (admits neither ${AGENT_USER}, root nor pi); no Match block`,
      };
    });

    await check("administrator has the given public keys and no password", async () => {
      const problems: string[] = [];
      if (!adminUser) return { problems: [`no ${ADMIN_USER} account`] };
      const [uid, gid] = [Number(adminUser[2]), Number(adminUser[3])];
      const home = `/home/${ADMIN_USER}`;
      const keys = parsePublicKeys(await readFile(PATHS.sshKey, "utf8"));
      const installed = await readFile(join(ROOT, home, ".ssh/authorized_keys"), "utf8").catch(() => "");
      if (installed !== authorizedKeys(keys)) problems.push("authorized_keys is not exactly the given public keys");
      // The image's own ssh-keygen reads each installed key with the fingerprint of the given one
      // (key copies go on the chroot's /tmp, a tmpfs).
      for (const [index, key] of keys.entries()) {
        await writeFile(join(ROOT, `tmp/admin-key-${index}.pub`), `${key}\n`);
        const listed = await chroot.run(["ssh-keygen", "-l", "-E", "sha256", "-f", `/tmp/admin-key-${index}.pub`], { okCodes: [0, 1, 255] });
        if (listed.code !== 0 || !listed.stdout.includes(` ${keyFingerprint(key)} `)) problems.push(`ssh-keygen in the image does not read ${keyFingerprint(key)}`);
      }
      problems.push(...expectInfo(home, await fileInfo(join(ROOT, home)), 0o700, uid, gid));
      problems.push(...expectInfo(`${home}/.ssh`, await fileInfo(join(ROOT, home, ".ssh")), 0o700, uid, gid));
      problems.push(...expectInfo(`${home}/.ssh/authorized_keys`, await fileInfo(join(ROOT, home, ".ssh/authorized_keys")), 0o600, uid, gid));
      const shadow = parseColonFile(await readFile(join(ROOT, "etc/shadow"), "utf8")).find((f) => f[0] === ADMIN_USER);
      if (shadow?.[1] !== "*") problems.push(`${ADMIN_USER} password field is ${JSON.stringify(shadow?.[1])}, expected "*"`);
      const sudoers = await readFile(join(ROOT, SUDOERS_FILE), "utf8").catch(() => "");
      if (sudoers !== sudoersFile()) problems.push(`${SUDOERS_FILE} differs from the expected rule`);
      problems.push(...expectInfo(SUDOERS_FILE, await fileInfo(join(ROOT, SUDOERS_FILE)), 0o440, 0, 0));
      // The only sudo rule the build adds is SUDOERS_FILE; the source's rules stay as they were.
      problems.push(...sudoersProblems(baseTree.keys(), newTree.keys(), delta.changed));
      const sudoGroup = groupMembers(await readFile(join(ROOT, "etc/group"), "utf8"), "sudo");
      if (sudoGroup.length > 0) problems.push(`the sudo group has members: ${sudoGroup.join(", ")}`);
      await chroot.run(["visudo", "--check", "--strict"]);
      return {
        problems,
        detail: `${keys.length} key(s), each read by the image's ssh-keygen, password field "*", sudo without password (${SUDOERS_FILE}, the only sudo rule added; the sudo group is empty)`,
      };
    });

    await check("no account can log in with a password", async () => {
      const problems: string[] = [];
      for (const name of ["passwd", "shadow", "group", "gshadow"]) {
        if (await Bun.file(join(ROOT, `etc/${name}-`)).exists()) problems.push(`/etc/${name}- is left over`);
      }
      const shadow = await readFile(join(ROOT, "etc/shadow"), "utf8");
      const withPassword = accountsWithPassword(passwd, shadow);
      if (withPassword.length > 0) problems.push(`accounts with a usable password: ${withPassword.join(", ")}`);
      const groups = groupsWithPassword(await readFile(join(ROOT, "etc/gshadow"), "utf8"));
      if (groups.length > 0) problems.push(`groups with a password: ${groups.join(", ")}`);
      if (account(passwd, SOURCE_USER)) problems.push(`the source image's ${SOURCE_USER} account is still there`);
      const shells = parseColonFile(passwd)
        .filter((f) => !/(nologin|false|sync)$/.test(f[6] ?? ""))
        .map((f) => `${f[0]} (${f[6]})`);
      return { problems, detail: `${parseColonFile(passwd).length} accounts, none with a password; login shells: ${shells.join(", ")}` };
    });

    await check("no account setup from the boot partition or the console (cloud-init, userconfig)", async () => {
      const problems: string[] = [];
      if (!(await Bun.file(join(ROOT, "etc/cloud/cloud-init.disabled")).exists())) problems.push("/etc/cloud/cloud-init.disabled is missing");
      const cloudUnits = cloudInitUnits([...newTree.keys()].filter((p) => p.startsWith("/usr/lib/systemd/system/")).map((p) => p.slice("/usr/lib/systemd/system/".length)));
      if (cloudUnits.length === 0) problems.push("no cloud-init unit found: the check no longer knows what to look at");
      for (const unit of [...cloudUnits, ...MASKED_SETUP_UNITS]) {
        const state = await isEnabled(chroot, unit);
        if (state !== "masked") problems.push(`${unit} is ${state || "unknown"}, expected masked`);
      }
      const listed = (await run(["mdir", "-b", "-i", boot, "::"], { env: MTOOLS_ENV })).stdout.split("\n").map((l) => l.trim().toLowerCase());
      // config.txt is always there: if it is not listed, the listing is not in the expected form.
      if (!listed.includes("::/config.txt")) problems.push("mdir -b does not list ::/config.txt: the boot listing is not in the expected form");
      for (const name of REMOVED_BOOT_FILES) if (listed.includes(`::/${name}`)) problems.push(`boot:${name} is still there`);
      return { problems, detail: `masked: ${[...cloudUnits, ...MASKED_SETUP_UNITS].join(", ")}; /etc/cloud/cloud-init.disabled; no ${REMOVED_BOOT_FILES.join(", ")} on the boot partition` };
    });

    // Laysi/trustiot-v2#815: README.md, section 「登入面」.
    step("login surface: consoles and USB");
    await check("no login prompt or shell on the screen or the serial console", async () => {
      const problems: string[] = [];
      for (const unit of [...MASKED_CONSOLE_UNITS, ...CONSOLE_INSTANCES]) {
        const state = await isEnabled(chroot, unit);
        if (state !== "masked") problems.push(`${unit} is ${state || "unknown"}, expected masked`);
      }
      // Every service a unit file or drop-in connects to a terminal: masked, or sulogin.
      const terminal = new Set<string>();
      const forced: string[] = [];
      const sulogin = new Set<string>(SULOGIN_UNITS);
      for (const [path, entry] of newTree) {
        const unit = unitOfFile(path);
        if (unit === undefined) continue;
        // The sulogin units are trusted for what the vendor file runs; any other file (an override
        // in /etc, a drop-in, a link) could change ExecStart= to a shell.
        if ((sulogin.has(unit) || unit === "*") && path !== `/usr/lib/systemd/system/${unit}` && !(unit === "*" && entry.type !== "file")) {
          if (sulogin.has(unit)) problems.push(`${path} configures ${unit}; only the vendor unit file is expected`);
        }
        if (entry.type !== "file") continue;
        const text = await readFile(join(ROOT, path), "utf8");
        if (readsTerminal(text)) terminal.add(unit);
        if (unit === "*" && /^\s*ExecStart\s*=/m.test(text)) problems.push(`${path} sets ExecStart= for every service`);
      }
      // SYSTEMD_SULOGIN_FORCE can come from a unit, a drop-in, an EnvironmentFile= or the manager's
      // DefaultEnvironment=/ManagerEnvironment=: no text file of the configuration may name it
      // (systemd-sulogin-shell itself is a binary and reads it).
      for (const [path, entry] of newTree) {
        if (entry.type !== "file" || !/^\/(etc|usr\/lib\/systemd|usr\/local\/lib\/systemd)\//.test(path)) continue;
        const bytes = await readFile(join(ROOT, path));
        if (isText(bytes) && bytes.includes("SYSTEMD_SULOGIN_FORCE")) forced.push(path);
      }
      // A unit linked in from outside the unit directories would not be read above.
      for (const [path, entry] of newTree) {
        if (entry.type !== "link" || unitOfFile(path) === undefined || entry.link === "/dev/null") continue;
        const target = entry.link?.startsWith("/") ? entry.link : join(dirname(path), entry.link ?? "");
        if (!UNIT_DIRS.some((dir) => target.startsWith(`${dir}/`))) problems.push(`${path} links to ${target}, outside the unit directories`);
      }
      // Scripts sysv-generator and rc-local-generator would start at boot.
      for (const path of delta.added) if (legacyStartupScript(path)) problems.push(`${path} was added: a script started at boot outside the checked units`);
      if (terminal.has("*")) problems.push("a service.d drop-in connects every service to a terminal");
      terminal.delete("*");
      for (const unit of [...terminal].sort()) {
        if (sulogin.has(unit)) continue;
        const state = await isEnabled(chroot, unit);
        if (state !== "masked") problems.push(`${unit} reads a terminal and is ${state || "unknown"}, expected masked`);
      }
      for (const unit of SULOGIN_UNITS) {
        const text = await readFile(join(ROOT, "usr/lib/systemd/system", unit), "utf8").catch(() => "");
        if (!/^ExecStart=-?\/usr\/lib\/systemd\/systemd-sulogin-shell /m.test(text)) problems.push(`${unit} does not run systemd-sulogin-shell`);
      }
      if (forced.length > 0) problems.push(`SYSTEMD_SULOGIN_FORCE (sulogin starts a shell for a locked root) is set in ${forced.join(", ")}`);
      const root = parseColonFile(await readFile(join(ROOT, "etc/shadow"), "utf8")).find((f) => f[0] === "root");
      if (!lockedPassword(root?.[1])) problems.push(`root's password field is ${JSON.stringify(root?.[1])}: sulogin would ask for it instead of refusing`);
      const logind = effectiveSection((await chroot.run(["systemd-analyze", "cat-config", "systemd/logind.conf"])).stdout, "Login");
      for (const [key, value] of Object.entries(LOGIND_EXPECTED)) {
        if (logind.get(key) !== value) problems.push(`logind ${key} is ${logind.get(key) ?? "not set"}, expected ${value}`);
      }
      if ((await readFile(join(ROOT, LOGIND_DROP_IN), "utf8").catch(() => "")) !== logindDropIn()) problems.push(`${LOGIND_DROP_IN} differs from the expected file`);
      return {
        problems,
        detail:
          `masked: ${MASKED_CONSOLE_UNITS.join(", ")} (and ${CONSOLE_INSTANCES.length} instances); every service whose unit reads a terminal (StandardInput=tty*, TTYPath=, or an Exec*= naming a getty, sulogin, sh -i or a terminal device) is masked except ` +
          `${SULOGIN_UNITS.join(", ")} (sulogin, root locked, no SYSTEMD_SULOGIN_FORCE in /etc or the systemd directories); no unit linked in from elsewhere, no added init.d or rc.local script; logind ${Object.entries(LOGIND_EXPECTED).map(([k, v]) => `${k}=${v}`).join(" ")}`,
      };
    });

    await check("a failed boot reboots instead of opening the initramfs shell", async () => {
      const problems: string[] = [];
      const cmdline = parseCmdline(await mtype(boot, "cmdline.txt"));
      const panics = cmdline.filter((t) => t.startsWith("panic="));
      if (panics.length !== 1 || panics[0] !== PANIC_ARGUMENT) problems.push(`cmdline.txt has ${panics.join(" ") || "no panic="}, expected ${PANIC_ARGUMENT}`);
      const listed = (await run(["mdir", "-b", "-i", boot, "::"], { env: MTOOLS_ENV })).stdout.split("\n").map((l) => l.trim());
      const initramfs = listed.filter((l) => /^::\/initramfs[^/]*$/i.test(l)).map((l) => l.slice(3));
      if (initramfs.length === 0) problems.push("the boot partition has no initramfs: the check no longer knows what to look at");
      for (const name of initramfs) {
        await rm(join(ROOT, "tmp/initramfs"), { recursive: true, force: true });
        await mkdir(join(ROOT, "tmp/initramfs"));
        await mfetch(boot, name, join(ROOT, "tmp/initramfs.img"));
        await chroot.run(["unmkinitramfs", "/tmp/initramfs.img", "/tmp/initramfs"]);
        const [init, functions] = await Promise.all(
          ["tmp/initramfs/init", "tmp/initramfs/scripts/functions"].map((p) => readFile(join(ROOT, p), "utf8").catch(() => "")),
        );
        problems.push(...initramfsPanicProblems(init ?? "", functions ?? "").map((p) => `${name}: ${p}`));
        await rm(join(ROOT, "tmp/initramfs"), { recursive: true, force: true });
        await rm(join(ROOT, "tmp/initramfs.img"), { force: true });
      }
      return { problems, detail: `cmdline.txt ${PANIC_ARGUMENT}; ${initramfs.join(", ")}: init reads panic=, panic() reboots before any shell` };
    });

    await check("no USB device is authorized (usbcore.authorized_default=0)", async () => {
      const problems: string[] = [];
      // (a) The kernel command line: what cmdline.txt says, and nothing that replaces it.
      problems.push(...cmdlineProblems(await mtype(boot, "cmdline.txt")));
      const config = await mtype(boot, "config.txt");
      const redirects = configCmdlineRedirects(config);
      if (redirects.length > 0) problems.push(`config.txt has ${redirects.join("; ")}: the firmware could read another command line`);
      const gadget = configUsbGadget(config);
      if (gadget.length > 0) problems.push(`config.txt puts dwc2 in device or OTG mode: ${gadget.join("; ")}`);
      const bootFiles = (await run(["mdir", "-b", "-i", boot, "::"], { env: MTOOLS_ENV })).stdout.split("\n").map((l) => l.trim().toLowerCase());
      if (bootFiles.includes("::/autoboot.txt")) problems.push("the boot partition has autoboot.txt: the bootloader could boot another partition's cmdline.txt");
      // usbcore must be built into every kernel, so the parameter holds from the kernel's start, and
      // no kernel may carry a command line of its own that replaces or overrides cmdline.txt.
      const kernels: string[] = [];
      for (const [path, entry] of newTree) {
        const kernel = /^\/usr\/lib\/modules\/([^/]+)\/modules\.builtin$/.exec(path)?.[1];
        if (!kernel || entry.type !== "file") continue;
        kernels.push(kernel);
        if (!/^kernel\/drivers\/usb\/core\/usbcore\.ko$/m.test(await readFile(join(ROOT, path), "utf8"))) problems.push(`${kernel}: usbcore is not built in`);
        const kernelConfig = await readFile(join(ROOT, "boot", `config-${kernel}`), "utf8").catch(() => undefined);
        if (kernelConfig === undefined) problems.push(`${kernel}: no /boot/config-${kernel} to check for a built-in command line`);
        else problems.push(...kernelConfigProblems(kernelConfig).map((p) => `${kernel}: ${p}`));
      }
      if (kernels.length === 0) problems.push("no modules.builtin: the check no longer knows which kernels the image has");

      // (b) Nothing in the image authorizes a device again. Reads a file the way the program that
      // uses it would, following links inside the image (never on the host); null for /dev/null.
      const readConfig = async (path: string): Promise<string | null> => {
        const entry = newTree.get(path);
        if (entry?.type === "file") return readFile(join(ROOT, path), "utf8");
        if (entry?.type !== "link") throw new Error(`${path} is neither a file nor a link`);
        const target = resolveInImage(newTree, path);
        if (target === "/dev/null") return null;
        if (target === undefined || newTree.get(target)?.type !== "file") throw new Error(`${path} links to something the check cannot read`);
        return readFile(join(ROOT, target), "utf8");
      };
      const filesIn = (dirs: readonly string[], suffix: string) =>
        [...newTree.keys()].filter((path) => dirs.some((dir) => path.startsWith(`${dir}/`) && !path.slice(dir.length + 1).includes("/")) && path.endsWith(suffix)).sort();
      const read = async (paths: string[]) => {
        const texts = new Map<string, string>();
        for (const path of paths) {
          try {
            const text = await readConfig(path);
            if (text !== null) texts.set(path, text);
          } catch (error) {
            problems.push(error instanceof Error ? error.message : String(error));
          }
        }
        return texts;
      };
      const rules = await read(filesIn(UDEV_RULES_DIRS, ".rules"));
      const writers = rulesWritingAuthorized(rules);
      if (writers.length > 0) problems.push(`udev rules writing "authorized": ${writers.join(", ")}`);
      // A program a rule runs could write the attribute without the rule naming it: every program
      // must be found in the image, and a script among them must not name the attribute.
      const programs = new Set<string>();
      for (const text of rules.values()) for (const program of rulePrograms(text)) programs.add(program);
      const scripts: string[] = [];
      for (const program of [...programs].sort()) {
        const target = newTree.get(program)?.type === "link" ? resolveInImage(newTree, program) : program;
        if (target === undefined || newTree.get(target)?.type !== "file") {
          problems.push(`${program}, run by a udev rule, is not a file in the image`);
          continue;
        }
        const bytes = await readFile(join(ROOT, target));
        if (!isText(bytes)) continue;
        scripts.push(program);
        if (AUTHORIZED_ATTRIBUTE.test(new TextDecoder().decode(bytes))) problems.push(`${program}, run by a udev rule, names the "authorized" attribute`);
      }
      const tmpfiles = await read(filesIn(TMPFILES_DIRS, ".conf"));
      for (const [path, text] of tmpfiles) for (const line of tmpfilesWritingAuthorized(text)) problems.push(`${path}: ${line.trim()}`);
      const sysfs = await read([...(newTree.has(SYSFS_CONFIG[0]) ? [SYSFS_CONFIG[0]] : []), ...filesIn([SYSFS_CONFIG[1]], ".conf")]);
      for (const [path, text] of sysfs) for (const line of sysfsWritingAuthorized(text)) problems.push(`${path}: ${line.trim()}`);
      const packages = (await chroot.run(["dpkg-query", "-W", "-f", "${Package} ${db:Status-Abbrev}\\n", ...USB_AUTHORIZING_PACKAGES], { okCodes: [0, 1] })).stdout;
      const installed = packages.split("\n").filter((line) => /^\S+ [a-z]i/.test(line)).map((line) => line.split(" ")[0]);
      for (const unit of USB_AUTHORIZING_UNITS) {
        const present = UNIT_DIRS.some((dir) => newTree.has(`${dir}/${unit}`));
        if (!present && installed.length === 0) continue;
        const state = await isEnabled(chroot, unit);
        if (state !== "masked") problems.push(`${unit} is ${state || "unknown"}, expected masked or not installed`);
      }
      // Any other text file that runs at boot or on demand (a unit's ExecStart=, a script, a
      // helper) naming the attribute; files up to 1 MiB, outside data-only trees.
      const named: string[] = [];
      let scanned = 0;
      for (const [path, entry] of newTree) {
        if (entry.type !== "file" || entry.size > BROAD_SCAN_MAX_BYTES || !BROAD_SCAN.test(path) || BROAD_SCAN_SKIP.test(path)) continue;
        const bytes = await readFile(join(ROOT, path));
        if (!isText(bytes)) continue;
        scanned++;
        if (AUTHORIZED_ATTRIBUTE.test(new TextDecoder().decode(bytes))) named.push(path);
      }
      if (named.length > 0) problems.push(`files naming the "authorized" attribute: ${named.join(", ")}`);
      // The same for every initramfs on the boot partition, which runs before the root file system.
      const initramfs = bootFiles.filter((l) => /^::\/initramfs[^/]*$/.test(l)).map((l) => l.slice(3));
      let initramfsScanned = 0;
      for (const name of initramfs) {
        const dir = join(ROOT, "tmp/initramfs-usb");
        await rm(dir, { recursive: true, force: true });
        await mkdir(dir);
        await mfetch(boot, name, join(ROOT, "tmp/initramfs-usb.img"));
        await chroot.run(["unmkinitramfs", "/tmp/initramfs-usb.img", "/tmp/initramfs-usb"]);
        for (const file of (await readdir(dir, { recursive: true })) as string[]) {
          const info = await lstat(join(dir, file));
          if (!info.isFile() || info.size > BROAD_SCAN_MAX_BYTES) continue;
          const bytes = await readFile(join(dir, file));
          if (!isText(bytes)) continue;
          initramfsScanned++;
          if (AUTHORIZED_ATTRIBUTE.test(new TextDecoder().decode(bytes))) problems.push(`${name}: ${file} names the "authorized" attribute`);
        }
        await rm(dir, { recursive: true, force: true });
        await rm(join(ROOT, "tmp/initramfs-usb.img"), { force: true });
      }
      if (initramfs.length === 0) problems.push("the boot partition has no initramfs: the check no longer knows what to look at");
      return {
        problems,
        detail:
          `cmdline.txt: one line, no --, ${BOOT_ARGUMENTS.join(" and ")} once each, no gadget module; config.txt has no cmdline=, os_prefix=, include or dwc2 outside host mode; no autoboot.txt; ` +
          `usbcore built into ${kernels.join(", ")}, none with CONFIG_CMDLINE_FORCE or usbcore in a built-in command line; ` +
          `${rules.size} udev rules files in ${UDEV_RULES_DIRS.join(", ")}: none writes "authorized", ${programs.size} programs they run are in the image and the ${scripts.length} scripts among them do not name it; ` +
          `${tmpfiles.size} tmpfiles.d files and ${sysfs.size} sysfsutils files: none writes it; ${USB_AUTHORIZING_PACKAGES.join(", ")} ${installed.length > 0 ? `installed: ${installed.join(", ")}, masked` : "not installed"}; ` +
          `none of ${scanned} text files under /etc, /usr/local, /usr/bin, /usr/sbin, /usr/libexec and /usr/lib, nor ${initramfsScanned} in ${initramfs.join(", ")}, names the attribute`,
      };
    });

    await check("sshd has no local socket (only SSH over the network)", async () => {
      const problems: string[] = [];
      for (const generator of MASKED_GENERATORS) {
        const entry = newTree.get(generator);
        if (entry?.type !== "link" || entry.link !== "/dev/null") problems.push(`${generator} is not a link to /dev/null`);
      }
      for (const unit of MASKED_SSH_SOCKETS) {
        const state = await isEnabled(chroot, unit);
        if (state !== "masked") problems.push(`${unit} is ${state || "unknown"}, expected masked`);
      }
      // Any other unit that puts sshd on a local socket: a Listen*= path, abstract name or vsock
      // address in a socket unit whose service (Service=, or the socket's own name) runs sshd.
      for (const [path, entry] of newTree) {
        if (entry.type !== "file" || !/^\/(etc|usr\/local\/lib|usr\/lib)\/systemd\/system\/[^/]+\.socket$/.test(path)) continue;
        const text = await readFile(join(ROOT, path), "utf8");
        if (!/^\s*Listen(Stream|SequentialPacket|Datagram)\s*=\s*([/@]|vsock:)/m.test(text)) continue;
        const unit = path.slice(path.lastIndexOf("/") + 1);
        const named = /^\s*Service\s*=\s*(\S+)\s*$/m.exec(text)?.[1];
        const candidates = named ? [named] : [unit.replace(/\.socket$/, "@.service"), unit.replace(/\.socket$/, ".service")];
        let service = "";
        for (const dir of UNIT_DIRS) for (const candidate of candidates) service += await readFile(join(ROOT, dir.slice(1), candidate), "utf8").catch(() => "");
        if (/sshd/.test(service) && (await isEnabled(chroot, unit)) !== "masked") problems.push(`${unit} puts sshd on a local socket`);
      }
      return { problems, detail: `${MASKED_GENERATORS.join(", ")} -> /dev/null; masked: ${MASKED_SSH_SOCKETS.join(", ")}` };
    });

    step("secrets and LAN names");
    await check("no private keys, secrets or LAN names in what the build changed", async () => {
      const problems: string[] = [];
      const deltaDir = join(W, "delta");
      const ownFiles = new Set([
        "/etc/fstab",
        SSHD_DROP_IN,
        TIMESYNCD_DROP_IN,
        SUDOERS_FILE,
        "/etc/cloud/cloud-init.disabled",
        `/home/${ADMIN_USER}/.ssh/authorized_keys`,
        ...AGENT_SYSTEMD_FILES.map((file) => file.image),
        LOGIND_DROP_IN,
        ...DATA_FILES.map((file) => file.image),
      ]);
      const fromRepo = new Set<string>(AGENT_SYSTEMD_FILES.map((file) => file.image));
      let scanned = 0;
      for (const path of [...delta.added, ...delta.changed]) {
        if (newTree.get(path)?.type !== "file") continue;
        const bytes = await readFile(join(ROOT, path));
        await mkdir(dirname(join(deltaDir, "root", path)), { recursive: true });
        await writeFile(join(deltaDir, "root", path), bytes);
        scanned++;
        if (!isText(bytes)) continue;
        // The agent's files are copies of edge/agent/systemd (checked byte for byte above), public
        // and reviewed there; the egress rules name the addresses they refuse on purpose.
        const lan = !fromRepo.has(path) && (ownFiles.has(path) || /^\/(etc|home|root|var\/lib\/trustiot-agent)\//.test(path));
        problems.push(...textProblems(path, bytes.toString("utf8"), lan));
      }
      // Boot partition: every file that differs from the source's.
      const bootDir = join(W, "bootfs");
      const baseBootDir = join(W, "base-bootfs");
      for (const [image, dir] of [[boot, bootDir], [baseBoot, baseBootDir]] as const) {
        await mkdir(dir, { recursive: true });
        await run(["mcopy", "-s", "-n", "-i", image, "::*", dir], { env: MTOOLS_ENV });
      }
      const [bootTree, baseBootTree] = [await walk(bootDir), await walk(baseBootDir)];
      const bootDelta = await compareTrees(baseBootDir, baseBootTree, bootDir, bootTree);
      for (const path of [...bootDelta.added, ...bootDelta.changed]) {
        if (bootTree.get(path)?.type !== "file") continue;
        const bytes = await readFile(join(bootDir, path));
        await mkdir(dirname(join(deltaDir, "boot", path)), { recursive: true });
        await writeFile(join(deltaDir, "boot", path), bytes);
        scanned++;
        if (isText(bytes)) problems.push(...textProblems(`boot:${path}`, bytes.toString("utf8"), true));
      }
      await writeFile(join(REPORT, "delta-boot.tsv"), [...bootDelta.added.map((p) => `added\t${p}`), ...bootDelta.changed.map((p) => `changed\t${p}`), ...bootDelta.removed.map((p) => `removed\t${p}`)].join("\n") + "\n");
      // The JSON report holds the rule, file and line of each finding, with the secret redacted.
      const leaksReport = join(REPORT, "gitleaks.json");
      const leaks = await run(
        [PATHS.gitleaks, "dir", "--redact", "--no-banner", "--no-color", "--exit-code", "1", "--log-level", "warn", "--report-format", "json", "--report-path", leaksReport, deltaDir],
        { okCodes: [0, 1] },
      );
      const accepted: string[] = [];
      if (leaks.code !== 0) {
        const findings = JSON.parse(await readFile(leaksReport, "utf8")) as { RuleID?: string; File?: string; StartLine?: number }[];
        if (findings.length === 0) problems.push(`gitleaks exited ${leaks.code} without findings: ${leaks.stdout}${leaks.stderr}`);
        for (const f of findings) {
          // Copies sit at <deltaDir>/root/<path> and <deltaDir>/boot/<path>.
          const rel = relative(deltaDir, f.File ?? "");
          const inRoot = rel.startsWith("root/");
          const finding = { rule: f.RuleID ?? "", path: inRoot ? rel.slice("root".length) : `boot:${rel.slice("boot".length)}`, line: f.StartLine ?? 0 };
          const sha256 = await sha256File(join(deltaDir, rel));
          const owners = inRoot ? parseDpkgOwners((await chroot.run(["dpkg-query", "-S", finding.path], { okCodes: [0, 1] })).stdout, finding.path) : [];
          const problem = findingProblem(finding, sha256, owners, ownFiles, KNOWN_FINDINGS);
          if (problem) problems.push(`gitleaks: ${problem}`);
          else accepted.push(`${finding.rule} in ${finding.path}:${finding.line}`);
        }
      }
      const keysElsewhere = [...newTree.keys()].filter((p) => /^\/(root|home\/[^/]+)\/\.ssh\/id_/.test(p));
      if (keysElsewhere.length > 0) problems.push(`SSH identity files: ${keysElsewhere.join(", ")}`);
      await rm(deltaDir, { recursive: true, force: true });
      const known = accepted.length === 0 ? "gitleaks clean" : `gitleaks: only the reviewed package files of KNOWN_FINDINGS (${accepted.join(", ")})`;
      return { problems, detail: `${scanned} changed or added files (root and boot) scanned: no private key blocks, no LAN names in configuration, ${known}` };
    });
  } finally {
    await chroot.leave();
    await rootMount.unmount();
    await baseMount.unmount();
  }

  for (const file of [boot, root, data, baseBoot, baseRoot]) await rm(file, { force: true });
  await rm(join(W, "bootfs"), { recursive: true, force: true });
  await rm(join(W, "base-bootfs"), { recursive: true, force: true });

  const failed = outcomes.filter((o) => o.problems.length > 0);
  console.log(`\n${outcomes.length - failed.length} of ${outcomes.length} checks passed`);
  await writeFile(join(REPORT, "checks.json"), `${JSON.stringify(outcomes, null, 2)}\n`);
  if (failed.length > 0) process.exit(1);
}

// Exit 1 is reserved for "a check failed"; anything else that stops the run exits 3.
await main().catch((error: unknown) => {
  console.error(error instanceof Error ? `error: ${error.message}` : error);
  process.exit(3);
});
