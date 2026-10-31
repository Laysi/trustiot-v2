// Files the build writes or edits in the image, and the parsers the check reads them back with.
// Pure functions only, so the unit tests cover them without an image.
import { partuuid } from "./layout.ts";
import { ADMIN_USER, DATA, NTP_SERVERS, TPM_OVERLAY } from "./pins.ts";

const MARK = "TrustIoT gateway image (edge/image, Laysi/trustiot-v2#809)";

// ---- /etc/fstab ------------------------------------------------------------------------------

export interface FstabEntry {
  spec: string;
  file: string;
  vfstype: string;
  options: string[];
  freq: string;
  passno: string;
}

export function parseFstab(text: string): FstabEntry[] {
  const entries: FstabEntry[] = [];
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) continue;
    const [spec = "", file = "", vfstype = "", options = "", freq = "0", passno = "0"] = line.split(/\s+/);
    entries.push({ spec, file, vfstype, options: options.split(","), freq, passno });
  }
  return entries;
}

/** The data partition's fstab line, by PARTUUID (first boot rewrites the disk identifier in it). */
export function dataFstabLine(labelId: string): string {
  return `PARTUUID=${partuuid(labelId, DATA.number)}  ${DATA.mountPoint}  ext4  ${DATA.mountOptions}  0  2`;
}

export function fstabWithData(fstab: string, labelId: string): string {
  if (parseFstab(fstab).some((e) => e.file === DATA.mountPoint)) {
    throw new Error(`/etc/fstab already mounts ${DATA.mountPoint}`);
  }
  const body = fstab.endsWith("\n") || fstab === "" ? fstab : `${fstab}\n`;
  return (
    `${body}# ${MARK}: agent state. nofail keeps the gateway bootable and reachable when the\n` +
    `# partition does not mount; the agent then does not start (its StateDirectory= requires it).\n` +
    `${dataFstabLine(labelId)}\n`
  );
}

// ---- ext4 inodes (debugfs) -----------------------------------------------------------------------

export interface DebugfsInode {
  type: string;
  /** Permission bits only (mode & 0o7777), whether debugfs printed them alone or with the type. */
  permissions: number;
  uid: number;
  gid: number;
}

/** The type, permissions and owner of `debugfs -R "stat <inode>"` output, or null if one is missing. */
export function parseDebugfsStat(text: string): DebugfsInode | null {
  const type = /\bType:\s+(\S+)/.exec(text)?.[1];
  const mode = /\bMode:\s+([0-7]+)\b/.exec(text)?.[1];
  const owner = /\bUser:\s+(\d+)\s+Group:\s+(\d+)/.exec(text);
  if (!type || !mode || !owner) return null;
  return { type, permissions: Number.parseInt(mode, 8) & 0o7777, uid: Number(owner[1]), gid: Number(owner[2]) };
}

// ---- config.txt and cmdline.txt on the boot partition --------------------------------------------

export const TPM_CONFIG_LINES = ["dtparam=spi=on", `dtoverlay=${TPM_OVERLAY}`] as const;

export function configTxtWithTpm(config: string): string {
  if (allModelLines(config).some((line) => line.startsWith(`dtoverlay=${TPM_OVERLAY}`))) {
    throw new Error(`config.txt already loads ${TPM_OVERLAY}`);
  }
  const body = config.endsWith("\n") || config === "" ? config : `${config}\n`;
  return `${body}\n# ${MARK}: Infineon SLB 9670 TPM on SPI0 CE0.\n[all]\n${TPM_CONFIG_LINES.join("\n")}\n`;
}

/**
 * Lines of config.txt that apply to every model: those before the first section header and those
 * under an [all] header. Comments and blank lines are left out.
 */
export function allModelLines(config: string): string[] {
  const lines: string[] = [];
  let all = true;
  for (const raw of config.split("\n")) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) continue;
    if (line.startsWith("[")) {
      all = line.toLowerCase() === "[all]";
      continue;
    }
    if (all) lines.push(line);
  }
  return lines;
}

/** The body of the first node with this name (properties only: dtc prints them before subnodes). */
function nodeProperties(dts: string, name: string): string | undefined {
  const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return dts.match(new RegExp(`(?:^|\\s)${escaped} \\{([^{}]*)`))?.[1];
}

function hasProperty(body: string, property: string): boolean {
  return body.split("\n").some((line) => line.trim() === property);
}

/**
 * What is wrong with the TPM in a device tree decompiled by `dtc -O dts` after the overlay was
 * merged onto a board's tree: SPI0 enabled, the SLB 9670 on chip select 0, and spidev0 off.
 * Property order inside a node is not fixed after a merge, so each one is looked up on its own.
 */
export function tpmDeviceTreeProblems(dts: string): string[] {
  const problems: string[] = [];
  const spi0 = nodeProperties(dts, "spi@7e204000");
  if (spi0 === undefined || !hasProperty(spi0, 'status = "okay";')) problems.push("spi0 (spi@7e204000) is not enabled");
  const tpm = nodeProperties(dts, "slb9670@0");
  if (tpm === undefined) problems.push("there is no slb9670@0 node");
  else {
    if (!hasProperty(tpm, 'compatible = "infineon,slb9670";')) problems.push('slb9670@0 is not compatible "infineon,slb9670"');
    if (!tpm.split("\n").some((line) => /^reg = <0x0+>;$/.test(line.trim()))) problems.push("slb9670@0 is not at reg 0");
    if (!hasProperty(tpm, 'status = "okay";')) problems.push("slb9670@0 is not enabled");
  }
  const spidev = nodeProperties(dts, "spidev@0");
  if (spidev === undefined || !hasProperty(spidev, 'status = "disabled";')) problems.push("spidev0 (spidev@0) is not disabled");
  return problems;
}

export function parseCmdline(text: string): string[] {
  return text.trim().split(/\s+/).filter((t) => t !== "");
}

// ---- systemd-timesyncd ---------------------------------------------------------------------------

export const TIMESYNCD_DROP_IN = "/etc/systemd/timesyncd.conf.d/50-trustiot.conf";

export function timesyncdDropIn(): string {
  return (
    `# ${MARK}: the firewall allows NTP only to these\n` +
    `# servers, so the compiled-in fallback pool is turned off.\n` +
    `[Time]\nNTP=${NTP_SERVERS.join(" ")}\nFallbackNTP=\n`
  );
}

export interface TimesyncdServers {
  ntp: string[];
  /** undefined: no file sets FallbackNTP=, so timesyncd uses its compiled-in pool. */
  fallback: string[] | undefined;
}

/**
 * The servers timesyncd ends up with, from `systemd-analyze cat-config systemd/timesyncd.conf`
 * (the main file and its drop-ins in the order systemd reads them). An empty assignment resets
 * the list, any other one appends to it (timesyncd.conf(5)).
 */
export function effectiveTimesyncd(catConfig: string): TimesyncdServers {
  const result: TimesyncdServers = { ntp: [], fallback: undefined };
  let section = "";
  for (const raw of catConfig.split("\n")) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#") || line.startsWith(";")) continue;
    if (line.startsWith("[")) {
      section = line;
      continue;
    }
    if (section !== "[Time]") continue;
    const eq = line.indexOf("=");
    if (eq < 0) continue;
    const key = line.slice(0, eq).trim();
    const values = line.slice(eq + 1).trim().split(/\s+/).filter((v) => v !== "");
    if (key === "NTP") result.ntp = values.length === 0 ? [] : [...result.ntp, ...values];
    if (key === "FallbackNTP") result.fallback = values.length === 0 ? [] : [...(result.fallback ?? []), ...values];
  }
  return result;
}

// ---- OpenSSH server ------------------------------------------------------------------------------

/** First in sshd_config.d, so its values win: sshd keeps the first value it reads for a keyword. */
export const SSHD_DROP_IN = "/etc/ssh/sshd_config.d/00-trustiot.conf";

/** What `sshd -G` must print for the image (keys in lower case, as sshd prints them). */
export const SSHD_EXPECTED: Readonly<Record<string, string>> = {
  permitrootlogin: "no",
  passwordauthentication: "no",
  kbdinteractiveauthentication: "no",
  permitemptypasswords: "no",
  pubkeyauthentication: "yes",
  authenticationmethods: "publickey",
  allowusers: ADMIN_USER,
  x11forwarding: "no",
};

export function sshdDropIn(): string {
  return (
    `# ${MARK}: SSH public keys only, and only\n` +
    `# for the administrator account. sshd keeps the first value it reads, and this file is read first.\n` +
    `PermitRootLogin no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPermitEmptyPasswords no\n` +
    `PubkeyAuthentication yes\nAuthenticationMethods publickey\nAllowUsers ${ADMIN_USER}\nX11Forwarding no\n`
  );
}

/** `sshd -G` output: keyword (lower case) to every value printed for it. */
export function parseSshdDump(text: string): Map<string, string[]> {
  const result = new Map<string, string[]>();
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line === "") continue;
    const space = line.indexOf(" ");
    const key = (space < 0 ? line : line.slice(0, space)).toLowerCase();
    const value = space < 0 ? "" : line.slice(space + 1).trim();
    result.set(key, [...(result.get(key) ?? []), value]);
  }
  return result;
}

export function sshdProblems(dump: Map<string, string[]>): string[] {
  const problems: string[] = [];
  for (const [key, expected] of Object.entries(SSHD_EXPECTED)) {
    const values = dump.get(key) ?? [];
    if (values.length !== 1 || values[0] !== expected) {
      problems.push(`sshd ${key} is ${values.length === 0 ? "not set" : values.join(", ")}, expected ${expected}`);
    }
  }
  return problems;
}

/**
 * Connections `sshd -G -C` evaluates the configuration for, besides plain `sshd -G`: a Match block
 * only applies to a connection, so plain `sshd -G` alone would miss one that loosens the rules for
 * some user or address. Documentation names and addresses only (RFC 6761, RFC 5737).
 */
export const SSHD_TEST_CONNECTIONS = [
  `user=${ADMIN_USER},host=gateway.example,addr=192.0.2.1,laddr=192.0.2.2,lport=22`,
  "user=root,host=gateway.example,addr=192.0.2.1,laddr=192.0.2.2,lport=22",
  "user=pi,host=gateway.example,addr=198.51.100.1,laddr=192.0.2.2,lport=22",
] as const;

/** Line numbers (1-based) of Match blocks in one sshd configuration file. The image has none. */
export function sshdMatchLines(text: string): number[] {
  return text
    .split("\n")
    .map((line, index) => (/^\s*match\s/i.test(line) ? index + 1 : 0))
    .filter((line) => line > 0);
}

// ---- sudo ----------------------------------------------------------------------------------------

export const SUDOERS_FILE = `/etc/sudoers.d/${ADMIN_USER}`;

export function sudoersFile(): string {
  return (
    `# ${MARK}: the administrator has no\n` +
    `# password, so sudo cannot ask for one. Narrowed for production in Laysi/trustiot-v2#219.\n` +
    `${ADMIN_USER} ALL=(ALL:ALL) NOPASSWD: ALL\n`
  );
}

/**
 * What is wrong with the sudo rules of the image against the source image: /etc/sudoers.d may
 * hold only the source's files, unchanged, plus SUDOERS_FILE; /etc/sudoers itself is unchanged.
 * `base` and `image` list every path of each root file system, `changed` the paths whose
 * content or metadata differ.
 */
export function sudoersProblems(base: Iterable<string>, image: Iterable<string>, changed: Iterable<string>): string[] {
  const rules = (paths: Iterable<string>) => [...paths].filter((p) => p === "/etc/sudoers" || p.startsWith("/etc/sudoers.d/"));
  const allowed = new Set([...rules(base), SUDOERS_FILE]);
  const problems = rules(image)
    .filter((p) => !allowed.has(p))
    .map((p) => `${p} is not in the source image`);
  for (const p of rules(changed)) if (p !== SUDOERS_FILE) problems.push(`${p} differs from the source image's`);
  return problems.sort();
}

// ---- first-boot setup ----------------------------------------------------------------------------

/**
 * cloud-init's units among unit file names: every one is masked in the image. A mask, unlike
 * /etc/cloud/cloud-init.disabled, also holds when the kernel command line says cloud-init=enabled.
 */
export function cloudInitUnits(names: Iterable<string>): string[] {
  return [...names].filter((name) => /^cloud-[\w-]+\.(service|socket|target|timer|path)$/.test(name)).sort();
}

/** Units that let someone at the console or with the SD card set up a login; masked in the image. */
export const MASKED_SETUP_UNITS = ["userconfig.service"] as const;

// ---- gitleaks ------------------------------------------------------------------------------------

export interface LeakFinding {
  rule: string;
  /** Path in the root file system ("/etc/...") or on the boot partition ("boot:/..."). */
  path: string;
  line: number;
}

export interface KnownFinding {
  readonly path: string;
  readonly rule: string;
  readonly package: string;
  readonly sha256: string;
}

/**
 * Why a gitleaks finding is not accepted, or undefined when one of `known` names its path, rule,
 * owning package and the file's whole content. `owners` are the packages `dpkg-query -S` names for
 * the path (architecture suffixes allowed); a file the build writes itself is never accepted.
 */
export function findingProblem(
  finding: LeakFinding,
  fileSha256: string,
  owners: readonly string[],
  buildWritten: ReadonlySet<string>,
  known: readonly KnownFinding[],
): string | undefined {
  const where = `${finding.rule} in ${finding.path}:${finding.line}`;
  if (buildWritten.has(finding.path)) return `${where} (a file the build writes)`;
  const entry = known.find((k) => k.path === finding.path && k.rule === finding.rule);
  if (!entry) return `${where} (SHA-256 ${fileSha256}, package ${owners.join(", ") || "none"})`;
  const packages = owners.map((owner) => owner.split(":")[0]);
  if (!packages.includes(entry.package)) return `${where}: owned by ${owners.join(", ") || "no package"}, expected ${entry.package}`;
  if (fileSha256 !== entry.sha256) return `${where}: SHA-256 ${fileSha256}, the reviewed file has ${entry.sha256}`;
  return undefined;
}

/** Packages `dpkg-query -S <path>` names ("pkg1, pkg2:arm64: /path"). */
export function parseDpkgOwners(output: string, path: string): string[] {
  const line = output.split("\n").find((l) => l.endsWith(`: ${path}`));
  if (!line) return [];
  return line
    .slice(0, -`: ${path}`.length)
    .split(",")
    .map((owner) => owner.trim())
    .filter((owner) => owner !== "");
}

// ---- accounts ------------------------------------------------------------------------------------

/** Members listed for a group in /etc/group (the fourth field). */
export function groupMembers(group: string, name: string): string[] {
  const fields = parseColonFile(group).find((f) => f[0] === name);
  return (fields?.[3] ?? "").split(",").filter((m) => m !== "");
}

/** Lines of a colon-separated account file (passwd, shadow, group, gshadow). */
export function parseColonFile(text: string): string[][] {
  return text
    .split("\n")
    .filter((line) => line.trim() !== "" && !line.startsWith("#"))
    .map((line) => line.split(":"));
}

/** A password field nobody can log in with: locked (!...) or never set (*...). */
function unusable(field: string): boolean {
  return field.startsWith("!") || field.startsWith("*");
}

/**
 * Accounts someone could log in to with a password: a password hash, or an empty password, in
 * shadow or (old style) in passwd. "x" in passwd means "see shadow".
 */
export function accountsWithPassword(passwd: string, shadow: string): string[] {
  const shadowByName = new Map(parseColonFile(shadow).map((f) => [f[0] ?? "", f[1] ?? ""]));
  const found: string[] = [];
  for (const fields of parseColonFile(passwd)) {
    const name = fields[0] ?? "";
    const inPasswd = fields[1] ?? "";
    if (inPasswd !== "x") {
      if (!unusable(inPasswd)) found.push(`${name} (password in /etc/passwd)`);
      continue;
    }
    const inShadow = shadowByName.get(name);
    if (inShadow !== undefined && !unusable(inShadow)) found.push(name);
  }
  // A shadow entry without a passwd line cannot log in, but a hash there is still a leftover.
  const names = new Set(parseColonFile(passwd).map((f) => f[0]));
  for (const [name, field] of shadowByName) {
    if (!names.has(name) && !unusable(field) && field !== "") found.push(`${name} (only in /etc/shadow)`);
  }
  return found;
}

/** Groups with a group password (newgrp would accept it from non-members). */
export function groupsWithPassword(gshadow: string): string[] {
  return parseColonFile(gshadow)
    .filter((f) => {
      const field = f[1] ?? "";
      return field !== "" && !unusable(field);
    })
    .map((f) => f[0] ?? "");
}
