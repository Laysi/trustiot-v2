// The image check, run inside the tool container by check.ts. It reads only the finished image,
// the source image, the agent executable and the administrator's public keys; it shares the
// expected values (pins, file contents) with the build but none of the build's steps.
// README.md, section 「映像檢查」 lists what each check proves.
import { copyFile, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
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
  parseDebugfsStat,
  parseDpkgOwners,
  parseFstab,
  parseSshdDump,
  sshdMatchLines,
  sshdProblems,
  sudoersFile,
  sudoersProblems,
  tpmDeviceTreeProblems,
} from "../files.ts";
import { aarch64ElfProblem, authorizedKeys, keyFingerprint, parsePublicKeys } from "../inputs.ts";
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
  DATA,
  IMAGE_PACKAGES,
  KNOWN_FINDINGS,
  NTP_SERVERS,
  REMOVED_BOOT_FILES,
  SECTOR,
  SOURCE,
  SOURCE_USER,
  TPM_OVERLAY,
} from "../pins.ts";
import { compareTrees, isText, manifest, textProblems, walk } from "../tree.ts";
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
    for (const file of [root, data]) {
      const r = await run(["e2fsck", "-f", "-n", file], { okCodes: [0, 1, 2, 4, 8] });
      if (r.code !== 0) problems.push(`e2fsck ${file}: exit ${r.code}: ${r.stdout.trim().slice(-500)}`);
    }
    const f = await run(["fsck.vfat", "-n", boot], { okCodes: [0, 1, 2] });
    if (f.code !== 0) problems.push(`fsck.vfat: exit ${f.code}: ${f.stdout.trim().slice(-500)}`);
    return { problems, detail: "e2fsck root and data, fsck.vfat boot" };
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
    await check("data partition is formatted for the agent", async () => {
      const problems: string[] = [];
      const header = (await run(["dumpe2fs", "-h", data])).stdout;
      if (!new RegExp(`^Filesystem volume name:\\s+${DATA.label}$`, "m").test(header)) problems.push(`label is not ${DATA.label}`);
      if (!new RegExp(`^Filesystem UUID:\\s+${DATA.uuid}$`, "m").test(header)) problems.push(`UUID is not ${DATA.uuid}`);
      const inode = parseDebugfsStat((await run(["debugfs", "-R", "stat <2>", data])).stdout);
      if (!inode) problems.push("debugfs stat of the root directory has no type, mode or owner");
      else {
        if (!agentUser) problems.push("trustiot-agent is not in /etc/passwd");
        else if (String(inode.uid) !== agentUser[2] || String(inode.gid) !== agentUser[3]) {
          problems.push(`root directory owner ${inode.uid}:${inode.gid}, expected trustiot-agent ${agentUser[2]}:${agentUser[3]}`);
        }
        if (inode.type !== "directory") problems.push(`root inode type is ${inode.type}, expected directory`);
        if (inode.permissions !== 0o700) problems.push(`root directory mode ${inode.permissions.toString(8).padStart(4, "0")}, expected 0700`);
      }
      // `ls -p` prints /inode/mode/uid/gid/name/size/ per entry.
      const listing = (await run(["debugfs", "-R", "ls -p /", data])).stdout.split("\n").filter((l) => l.startsWith("/"));
      const names = listing.map((l) => l.split("/")[5] ?? "").filter((n) => n !== "." && n !== ".." && n !== "lost+found");
      if (names.length > 0) problems.push(`the data file system is not empty: ${names.join(", ")}`);
      return { problems, detail: `${DATA.label}, ${DATA.uuid}, owner trustiot-agent, mode 0700, empty` };
    });

    await check("data partition is mounted at the agent's state directory", async () => {
      const problems: string[] = [];
      const fstab = parseFstab(await readFile(join(ROOT, "etc/fstab"), "utf8"));
      const entries = fstab.filter((e) => e.file === DATA.mountPoint);
      const entry = entries[0];
      const spec = `PARTUUID=${partuuid(labelId, DATA.number)}`;
      if (entries.length !== 1 || !entry) problems.push(`/etc/fstab has ${entries.length} entries for ${DATA.mountPoint}`);
      else {
        if (entry.spec !== spec) problems.push(`source ${entry.spec}, expected ${spec}`);
        if (entry.vfstype !== "ext4") problems.push(`type ${entry.vfstype}`);
        for (const option of ["nodev", "nosuid", "noexec", "nofail"]) if (!entry.options.includes(option)) problems.push(`option ${option} is missing`);
        if (entry.passno !== "2") problems.push(`fsck pass ${entry.passno}, expected 2`);
      }
      const rootEntry = fstab.find((e) => e.file === "/");
      const bootEntry = fstab.find((e) => e.file === "/boot/firmware");
      if (rootEntry?.spec !== `PARTUUID=${partuuid(labelId, 2)}`) problems.push(`root is mounted from ${rootEntry?.spec}`);
      if (bootEntry?.spec !== `PARTUUID=${partuuid(labelId, 1)}`) problems.push(`boot is mounted from ${bootEntry?.spec}`);
      problems.push(...expectInfo(DATA.mountPoint, await fileInfo(join(ROOT, DATA.mountPoint)), 0o700, 0, 0));
      const unit = await readFile(join(ROOT, "usr/lib/systemd/system/trustiot-agent.service"), "utf8");
      if (!/^StateDirectory=trustiot-agent$/m.test(unit)) problems.push("the agent unit has no StateDirectory=trustiot-agent (which makes it require the mount)");
      return { problems, detail: `${spec} ${DATA.mountPoint} ext4 ${DATA.mountOptions}` };
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

    await check("TPM packages at the pinned versions", async () => {
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
        detail: `sshd -G and sshd -T for ${SSHD_TEST_CONNECTIONS.length} connections (${ADMIN_USER}, root, pi): permitrootlogin no, passwordauthentication no, kbdinteractiveauthentication no, authenticationmethods publickey, allowusers ${ADMIN_USER}; no Match block`,
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
      ]);
      let scanned = 0;
      for (const path of [...delta.added, ...delta.changed]) {
        if (newTree.get(path)?.type !== "file") continue;
        const bytes = await readFile(join(ROOT, path));
        await mkdir(dirname(join(deltaDir, "root", path)), { recursive: true });
        await writeFile(join(deltaDir, "root", path), bytes);
        scanned++;
        if (!isText(bytes)) continue;
        const lan = ownFiles.has(path) || /^\/(etc|home|root|var\/lib\/trustiot-agent)\//.test(path);
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
