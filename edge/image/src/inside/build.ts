// The build, run inside the tool container by build.ts. It reads the verified source image, the
// agent executable and the administrator's public keys from /in, and leaves the image and its
// build record in /work/out. Every step is described in README.md, section 「映像內容」.
import { chmod, chown, copyFile, mkdir, open, readFile, readdir, rm, truncate, utimes, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import {
  MASKED_SETUP_UNITS,
  SSHD_DROP_IN,
  SSHD_TEST_CONNECTIONS,
  SUDOERS_FILE,
  TIMESYNCD_DROP_IN,
  cloudInitUnits,
  configTxtWithTpm,
  effectiveTimesyncd,
  fstabWithData,
  parseCmdline,
  parseColonFile,
  parseSshdDump,
  sshdDropIn,
  sshdMatchLines,
  sshdProblems,
  sudoersFile,
  timesyncdDropIn,
} from "../files.ts";
import { aarch64ElfProblem, authorizedKeys, keyFingerprint, parsePublicKeys } from "../inputs.ts";
import {
  BOOT_CODE_BYTES,
  compareTables,
  imageSectors,
  keepsDiskIdentifier,
  parseSfdiskJson,
  partuuid,
  renderSfdiskScript,
  sourceTable,
  targetTable,
} from "../layout.ts";
import {
  ADMIN_USER,
  AGENT_SYSTEMD_FILES,
  DATA,
  DATA_FILES,
  DEBIAN_SNAPSHOT,
  IMAGE_PACKAGES,
  NTP_SERVERS,
  REMOVED_BOOT_FILES,
  SECTOR,
  SOURCE,
  SOURCE_DATE_EPOCH,
  SOURCE_USER,
  TOOL_IMAGE,
  TPM_OVERLAY,
} from "../pins.ts";
import {
  Chroot,
  Ext4Mount,
  PATHS,
  chrootApt,
  extractPartition,
  installTools,
  placePartition,
  run,
  sha256Bytes,
  sha256File,
  step,
} from "./common.ts";

const W = PATHS.work;
const ROOT_MOUNT = "/mnt/root";
const MTOOLS_ENV = { MTOOLS_SKIP_CHECK: "1" };
const OUT_NAME = "trustiot-gateway.img";

/** Log files the source image ships empty; the build's package installation is not kept in them. */
const EMPTY_LOGS = [
  "var/log/alternatives.log",
  "var/log/dpkg.log",
  "var/log/apt/history.log",
  "var/log/apt/term.log",
  "var/log/apt/eipp.log.xz",
];

/** Backups useradd and userdel leave behind; the source image has none. */
const ACCOUNT_BACKUPS = ["passwd-", "group-", "shadow-", "gshadow-", "subuid-", "subgid-"];

interface BuildRecord {
  agent: { sha256: string; version: string };
  sshKeys: string[];
  packages: string;
  upgradable: string[];
  agentUser: { uid: number; gid: number };
  adminUser: { uid: number; gid: number };
}

function ids(passwd: string, name: string): { uid: number; gid: number } {
  const fields = parseColonFile(passwd).find((f) => f[0] === name);
  if (!fields) throw new Error(`/etc/passwd has no ${name}`);
  return { uid: Number(fields[2]), gid: Number(fields[3]) };
}

async function writeRootFile(root: string, path: string, content: string, mode: number): Promise<void> {
  const target = join(root, path);
  await mkdir(join(target, ".."), { recursive: true });
  await writeFile(target, content);
  await chown(target, 0, 0);
  await chmod(target, mode);
}

async function installAgent(chroot: Chroot, record: Partial<BuildRecord>): Promise<void> {
  step("agent");
  const r = chroot.root;
  const head = new Uint8Array(await Bun.file(PATHS.agent).slice(0, 64).arrayBuffer());
  const problem = aarch64ElfProblem(head);
  if (problem) throw new Error(`the agent executable is not usable: ${problem}`);
  await copyFile(PATHS.agent, join(r, "usr/bin/trustiot-agent"));
  await chown(join(r, "usr/bin/trustiot-agent"), 0, 0);
  await chmod(join(r, "usr/bin/trustiot-agent"), 0o755);
  for (const file of AGENT_SYSTEMD_FILES) {
    const target = join(r, file.image.slice(1));
    await mkdir(dirname(target), { recursive: true });
    await copyFile(join(PATHS.agentSystemd, file.repo), target);
    await chown(target, 0, 0);
    await chmod(target, 0o644);
  }
  await chroot.run(["systemd-sysusers", "/usr/lib/sysusers.d/trustiot-agent.conf"], { inherit: true });
  await chroot.run(["systemctl", "enable", "trustiot-agent.service"], { inherit: true });
  // Runs the executable on the image's own C library: a build against a newer glibc fails here.
  const version = (await chroot.run(["/usr/bin/trustiot-agent", "--version"])).stdout.trim();
  console.log(`trustiot-agent --version: ${version}`);
  record.agent = { sha256: await sha256File(PATHS.agent), version };
}

async function installPackages(chroot: Chroot, record: Partial<BuildRecord>): Promise<void> {
  step(`packages from snapshot.debian.org ${DEBIAN_SNAPSHOT}`);
  await chrootApt(chroot, ["update"], { inherit: true });
  const pinned = Object.entries(IMAGE_PACKAGES).map(([name, version]) => `${name}=${version}`);
  await chrootApt(chroot, ["install", "-y", "--no-install-recommends", ...pinned], { inherit: true });
  const audit = await chroot.run(["dpkg", "--audit"]);
  if (audit.stdout.trim() !== "") throw new Error(`dpkg --audit reports unfinished packages:\n${audit.stdout}`);
  const versions = await chroot.run(["dpkg-query", "-W", ...Object.keys(IMAGE_PACKAGES), "libtss2-*"]);
  record.packages = versions.stdout.trim();
  console.log(record.packages);
  // For the record only: the build installs no updates (README.md, section 「套件更新」).
  const simulated = await chrootApt(chroot, ["-s", "upgrade"]);
  record.upgradable = simulated.stdout.split("\n").filter((l) => l.startsWith("Inst ")).map((l) => l.slice(5));
  console.log(`packages with a newer version in the snapshot (not installed): ${record.upgradable.length}`);
  for (const line of record.upgradable) console.log(`  ${line}`);
}

async function configureAccounts(chroot: Chroot, keys: string[], record: Partial<BuildRecord>): Promise<void> {
  step("accounts and SSH");
  const r = chroot.root;
  await chroot.run(["userdel", "--remove", SOURCE_USER], { inherit: true });
  await chroot.run(["useradd", "--create-home", "--user-group", "--shell", "/bin/bash", "--comment", "TrustIoT gateway administrator", ADMIN_USER]);
  // "*": no password exists, so none can be guessed; sshd and sudo never ask for one.
  await chroot.run(["usermod", "--password", "*", ADMIN_USER]);
  const admin = ids(await readFile(join(r, "etc/passwd"), "utf8"), ADMIN_USER);
  record.adminUser = admin;
  const home = join(r, "home", ADMIN_USER);
  await chmod(home, 0o700);
  await mkdir(join(home, ".ssh"), { recursive: true });
  await writeFile(join(home, ".ssh/authorized_keys"), authorizedKeys(keys));
  for (const [path, mode] of [[join(home, ".ssh"), 0o700], [join(home, ".ssh/authorized_keys"), 0o600]] as const) {
    await chown(path, admin.uid, admin.gid);
    await chmod(path, mode);
  }
  const unreadable = await keysOpenSshRejects(chroot, keys);
  if (unreadable.length > 0) throw new Error(`the image's ssh-keygen does not read these keys: ${unreadable.join("; ")}`);
  await writeRootFile(r, SUDOERS_FILE, sudoersFile(), 0o440);
  await chroot.run(["visudo", "--check", "--strict"], { inherit: true });

  await writeRootFile(r, SSHD_DROP_IN, sshdDropIn(), 0o644);
  // Points sshd at the rename-the-default-user banner of the source image; that user is gone.
  await rm(join(r, "etc/ssh/sshd_config.d/rename_user.conf"), { force: true });
  await chroot.run(["systemctl", "enable", "ssh.service"], { inherit: true });
  const problems = await sshdConfigProblems(chroot);
  if (problems.length > 0) throw new Error(`sshd configuration: ${problems.join("; ")}`);

  // cloud-init would read users, keys and passwords from the boot partition (anyone holding the
  // SD card can write it), and userconfig asks for a new user on the console. The marker file
  // alone gives way to cloud-init=enabled on the kernel command line; a masked unit does not.
  await writeRootFile(r, "etc/cloud/cloud-init.disabled", "# TrustIoT gateway image: accounts come from the image build (edge/image).\n", 0o644);
  const units = [...cloudInitUnits(await readdir(join(r, "usr/lib/systemd/system"))), ...MASKED_SETUP_UNITS];
  if (units.length === MASKED_SETUP_UNITS.length) throw new Error("the source image has no cloud-init units: README.md, section 「映像內容」 needs a look");
  await chroot.run(["systemctl", "disable", ...units], { inherit: true });
  await chroot.run(["systemctl", "mask", ...units], { inherit: true });
}

/**
 * Keys the image's own OpenSSH cannot read, or reads with another fingerprint: parsePublicKeys
 * checks every field already, and this is the same check by the program that will use the keys.
 * The key files go on the chroot's /tmp (a tmpfs), never into the image.
 */
async function keysOpenSshRejects(chroot: Chroot, keys: string[]): Promise<string[]> {
  const rejected: string[] = [];
  for (const [index, key] of keys.entries()) {
    const file = `/tmp/admin-key-${index}.pub`;
    await writeFile(join(chroot.root, file.slice(1)), `${key}\n`);
    const listed = await chroot.run(["ssh-keygen", "-l", "-E", "sha256", "-f", file], { okCodes: [0, 1, 255] });
    if (listed.code !== 0 || !listed.stdout.includes(` ${keyFingerprint(key)} `)) rejected.push(`${keyFingerprint(key)}: ${(listed.stdout + listed.stderr).trim().slice(0, 200)}`);
    await rm(join(chroot.root, file.slice(1)), { force: true });
  }
  return rejected;
}

/**
 * The effective sshd settings that differ from SSHD_EXPECTED: plain `sshd -G`, then `sshd -T` for
 * each of SSHD_TEST_CONNECTIONS so that a Match block cannot hide anything, and any Match line.
 */
async function sshdConfigProblems(chroot: Chroot): Promise<string[]> {
  const r = chroot.root;
  const problems = sshdProblems(parseSshdDump((await chroot.run(["sshd", "-G"])).stdout));
  // sshd -T wants a host key and its privilege separation directory: both go on the chroot's
  // tmpfs mounts (/tmp, /run), never into the image.
  await mkdir(join(r, "run/sshd"), { recursive: true, mode: 0o755 });
  await rm(join(r, "tmp/sshd-test-key"), { force: true });
  await rm(join(r, "tmp/sshd-test-key.pub"), { force: true });
  await chroot.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "", "-f", "/tmp/sshd-test-key"]);
  for (const connection of SSHD_TEST_CONNECTIONS) {
    const dump = parseSshdDump((await chroot.run(["sshd", "-T", "-h", "/tmp/sshd-test-key", "-C", connection])).stdout);
    problems.push(...sshdProblems(dump).map((p) => `${connection}: ${p}`));
  }
  const dropIns = (await readdir(join(r, "etc/ssh/sshd_config.d"))).map((name) => `/etc/ssh/sshd_config.d/${name}`);
  for (const path of ["/etc/ssh/sshd_config", ...dropIns]) {
    const lines = sshdMatchLines(await readFile(join(r, path.slice(1)), "utf8"));
    if (lines.length > 0) problems.push(`${path} has a Match block (line ${lines.join(", ")})`);
  }
  return problems;
}

async function configureTime(chroot: Chroot): Promise<void> {
  step("time");
  await writeRootFile(chroot.root, TIMESYNCD_DROP_IN, timesyncdDropIn(), 0o644);
  const effective = effectiveTimesyncd((await chroot.run(["systemd-analyze", "cat-config", "systemd/timesyncd.conf"])).stdout);
  if (effective.ntp.join(" ") !== NTP_SERVERS.join(" ") || effective.fallback?.length !== 0) {
    throw new Error(`timesyncd would use NTP=${effective.ntp.join(" ")} FallbackNTP=${effective.fallback?.join(" ") ?? "(built-in)"}`);
  }
  console.log(`NTP=${effective.ntp.join(" ")}, no fallback`);
}

async function configureData(chroot: Chroot): Promise<void> {
  step("data partition: encryption, unlock and mount");
  const r = chroot.root;
  const fstab = await readFile(join(r, "etc/fstab"), "utf8");
  await writeFile(join(r, "etc/fstab"), fstabWithData(fstab));
  // The mount point itself; the agent never sees it unmounted (its drop-in requires the mount).
  await mkdir(join(r, DATA.mountPoint.slice(1)), { recursive: true });
  await chown(join(r, DATA.mountPoint.slice(1)), 0, 0);
  await chmod(join(r, DATA.mountPoint.slice(1)), 0o700);
  // The first-boot setup, the unlock and the agent's dependency (README.md, 「資料分割區加密」),
  // and the journal kept in memory (「日誌」). The partition itself stays all zeros in the image.
  for (const file of DATA_FILES) {
    const target = join(r, file.image.slice(1));
    await mkdir(dirname(target), { recursive: true });
    await copyFile(join(PATHS.src, "files/data", file.repo), target);
    await chown(target, 0, 0);
    await chmod(target, file.mode);
  }
}

async function compileOverlay(chroot: Chroot): Promise<string> {
  step("TPM overlay");
  await copyFile(join(PATHS.src, `files/${TPM_OVERLAY}-overlay.dts`), join(chroot.root, "tmp/overlay.dts"));
  await chroot.run(["dtc", "-@", "-I", "dts", "-O", "dtb", "-o", "/tmp/overlay.dtbo", "/tmp/overlay.dts"], { inherit: true });
  const out = join(W, `${TPM_OVERLAY}.dtbo`);
  await copyFile(join(chroot.root, "tmp/overlay.dtbo"), out);
  return out;
}

async function tidyRoot(root: string): Promise<void> {
  step("tidy");
  for (const log of EMPTY_LOGS) {
    if (await Bun.file(join(root, log)).exists()) await truncate(join(root, log), 0);
  }
  for (const backup of ACCOUNT_BACKUPS) await rm(join(root, "etc", backup), { force: true });
  // An uninitialized machine ID is what makes the first boot a first boot (new SSH host keys).
  const machineId = (await readFile(join(root, "etc/machine-id"), "utf8")).trim();
  if (machineId !== "uninitialized") throw new Error(`/etc/machine-id is ${JSON.stringify(machineId)}, expected "uninitialized"`);
}

async function buildRoot(rootFile: string, keys: string[], record: Partial<BuildRecord>): Promise<string> {
  const mount = await Ext4Mount.open(rootFile, ROOT_MOUNT, false);
  const chroot = new Chroot(ROOT_MOUNT);
  let overlay = "";
  try {
    await chroot.enter({ network: true });
    await installPackages(chroot, record);
    await installAgent(chroot, record);
    await configureAccounts(chroot, keys, record);
    await configureTime(chroot);
    await configureData(chroot);
    overlay = await compileOverlay(chroot);
    record.agentUser = ids(await readFile(join(ROOT_MOUNT, "etc/passwd"), "utf8"), "trustiot-agent");
    await tidyRoot(ROOT_MOUNT);
  } finally {
    await chroot.leave();
    await mount.unmount();
  }
  await run(["e2fsck", "-f", "-n", rootFile], { inherit: true });
  return overlay;
}

async function setEpoch(path: string): Promise<void> {
  await utimes(path, SOURCE_DATE_EPOCH, SOURCE_DATE_EPOCH);
}

async function buildBoot(bootFile: string, overlay: string): Promise<void> {
  step("boot partition");
  const cmdline = parseCmdline((await run(["mtype", "-i", bootFile, "::cmdline.txt"], { env: MTOOLS_ENV })).stdout);
  const root = `root=PARTUUID=${partuuid(sourceTable().labelId, 2)}`;
  if (!cmdline.includes(root) || !cmdline.includes("resize")) {
    throw new Error(`cmdline.txt is not the source's (${cmdline.join(" ")}): it must have ${root} and resize`);
  }
  const config = (await run(["mtype", "-i", bootFile, "::config.txt"], { env: MTOOLS_ENV })).stdout;
  const newConfig = join(W, "config.txt");
  await writeFile(newConfig, configTxtWithTpm(config));
  await setEpoch(newConfig);
  await run(["mcopy", "-o", "-m", "-i", bootFile, newConfig, "::config.txt"], { env: MTOOLS_ENV });
  await setEpoch(overlay);
  await run(["mcopy", "-m", "-D", "o", "-i", bootFile, overlay, `::overlays/${TPM_OVERLAY}.dtbo`], { env: MTOOLS_ENV });
  // cloud-init's inputs: the image masks cloud-init, so they would do nothing (README.md, 「映像內容」).
  await run(["mdel", "-i", bootFile, ...REMOVED_BOOT_FILES.map((name) => `::${name}`)], { env: MTOOLS_ENV });
  await run(["fsck.vfat", "-n", bootFile], { inherit: true });
}

/**
 * Boot and root at their places; the data partition is left as the zeros of the new image file,
 * which the gateway's first boot recognises as never used and encrypts (README.md,
 * 「資料分割區加密」).
 */
async function assemble(mbr: Uint8Array, boot: string, root: string): Promise<string> {
  step("assemble");
  const table = targetTable();
  await mkdir(join(W, "out"), { recursive: true });
  const image = join(W, "out", OUT_NAME);
  await rm(image, { force: true });
  await writeFile(image, "");
  await truncate(image, imageSectors(table) * SECTOR);
  await run(["sfdisk", "--no-reread", "--no-tell-kernel", "--wipe", "never", "--quiet", image], { input: renderSfdiskScript(table) });
  // The source's boot code area, so that first boot's parted keeps the disk identifier
  // (keepsDiskIdentifier in src/layout.ts).
  if (!keepsDiskIdentifier(mbr)) throw new Error("the source image's MBR has no boot code; first boot would change the disk identifier");
  const handle = await open(image, "r+");
  try {
    await handle.write(mbr.subarray(0, BOOT_CODE_BYTES), 0, BOOT_CODE_BYTES, 0);
  } finally {
    await handle.close();
  }
  const written = parseSfdiskJson((await run(["sfdisk", "--json", image])).stdout);
  const problems = compareTables(written, table);
  if (problems.length > 0) throw new Error(`partition table: ${problems.join("; ")}`);
  for (const [file, number] of [[boot, 1], [root, 2]] as const) {
    const p = table.partitions.find((x) => x.number === number);
    if (!p) throw new Error(`no partition ${number}`);
    await placePartition(image, file, p.start);
  }
  console.log((await run(["sfdisk", "--list", image])).stdout.trim());
  return image;
}

async function main(): Promise<void> {
  const record: Partial<BuildRecord> = {};
  await installTools();

  step("inputs");
  const keys = parsePublicKeys(await readFile(PATHS.sshKey, "utf8"));
  record.sshKeys = keys.map(keyFingerprint);
  console.log(`administrator keys: ${record.sshKeys.join(", ")}`);
  const sourceSha = await sha256File(PATHS.source);
  if (sourceSha !== SOURCE.sha256) throw new Error(`source image SHA-256 ${sourceSha}, expected ${SOURCE.sha256}`);
  console.log(`source ${SOURCE.file} SHA-256 ${sourceSha}`);

  step("source partitions");
  const source = join(W, "source.img");
  await run(["sh", "-c", 'xz --decompress --stdout -- "$1" > "$2"', "xz", PATHS.source, source]);
  if (Bun.file(source).size !== SOURCE.imageBytes) throw new Error(`decompressed source has ${Bun.file(source).size} bytes, expected ${SOURCE.imageBytes}`);
  const sourceProblems = compareTables(parseSfdiskJson((await run(["sfdisk", "--json", source])).stdout), sourceTable());
  if (sourceProblems.length > 0) throw new Error(`source partition table: ${sourceProblems.join("; ")}`);
  const [sBoot, sRoot] = sourceTable().partitions;
  if (!sBoot || !sRoot) throw new Error("source table needs two partitions");
  const boot = join(W, "boot.vfat");
  const root = join(W, "root.ext4");
  const mbr = new Uint8Array(await Bun.file(source).slice(0, SECTOR).arrayBuffer());
  await extractPartition(source, sBoot.start, sBoot.size, boot);
  await extractPartition(source, sRoot.start, sRoot.size, root);
  await rm(source);

  const overlay = await buildRoot(root, keys, record);
  await buildBoot(boot, overlay);
  if (!record.agentUser) throw new Error("the agent user was not created");
  const image = await assemble(mbr, boot, root);
  for (const file of [boot, root, overlay, join(W, "config.txt")]) await rm(file, { force: true });

  step("record");
  const sha256 = await sha256File(image);
  await writeFile(`${image}.sha256`, `${sha256}  ${OUT_NAME}\n`);
  const info = {
    image: { file: OUT_NAME, bytes: Bun.file(image).size, sha256 },
    source: { url: SOURCE.url, sha256: SOURCE.sha256 },
    toolImage: TOOL_IMAGE,
    debianSnapshot: DEBIAN_SNAPSHOT,
    partitions: targetTable(),
    ...record,
    files: { sshd: sha256Bytes(sshdDropIn()), timesyncd: sha256Bytes(timesyncdDropIn()), sudoers: sha256Bytes(sudoersFile()) },
  };
  await writeFile(join(W, "out", "build-info.json"), `${JSON.stringify(info, null, 2)}\n`);
  console.log(`image ${OUT_NAME}: ${info.image.bytes} bytes, SHA-256 ${sha256}`);
}

await main().catch((error: unknown) => {
  console.error(error instanceof Error ? `error: ${error.message}` : error);
  process.exit(1);
});
