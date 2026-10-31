// The files the build writes into the image and the parsers the check reads them back with.
import { describe, expect, test } from "bun:test";
import {
  MASKED_SETUP_UNITS,
  SSHD_DROP_IN,
  SSHD_EXPECTED,
  SSHD_TEST_CONNECTIONS,
  SUDOERS_FILE,
  TIMESYNCD_DROP_IN,
  TPM_CONFIG_LINES,
  accountsWithPassword,
  allModelLines,
  cloudInitUnits,
  configTxtWithTpm,
  dataFstabLine,
  effectiveTimesyncd,
  findingProblem,
  fstabWithData,
  groupMembers,
  groupsWithPassword,
  parseCmdline,
  parseDebugfsStat,
  parseDpkgOwners,
  parseFstab,
  parseSshdDump,
  sshdDropIn,
  sshdMatchLines,
  sshdProblems,
  sudoersFile,
  sudoersProblems,
  timesyncdDropIn,
  tpmDeviceTreeProblems,
} from "../src/files.ts";
import { ADMIN_USER, DATA, KNOWN_FINDINGS, NTP_SERVERS, SOURCE_USER } from "../src/pins.ts";
import { LAN_PATTERNS } from "../src/tree.ts";

describe("fstab", () => {
  const source = "proc /proc proc defaults 0 0\nPARTUUID=27b94958-02  /  ext4  defaults,noatime  0  1\n";

  test("mounts the data partition by PARTUUID, hardened and nofail", () => {
    const entry = parseFstab(fstabWithData(source, "0x27b94958")).find((e) => e.file === DATA.mountPoint);
    expect(entry).toEqual({
      spec: "PARTUUID=27b94958-03",
      file: "/var/lib/trustiot-agent",
      vfstype: "ext4",
      options: ["defaults", "noatime", "nodev", "nosuid", "noexec", "nofail"],
      freq: "0",
      passno: "2",
    });
  });

  test("keeps the source lines and adds exactly one", () => {
    const built = fstabWithData(source, "0x27b94958");
    expect(built.startsWith(source)).toBe(true);
    expect(parseFstab(built)).toHaveLength(3);
    expect(fstabWithData("proc /proc proc defaults 0 0", "0x27b94958")).toContain(`\n${dataFstabLine("0x27b94958")}\n`);
  });

  test("refuses a second data mount", () => {
    expect(() => fstabWithData(fstabWithData(source, "0x27b94958"), "0x27b94958")).toThrow("already mounts");
  });
});

describe("debugfs stat", () => {
  const stat = (mode: string, type = "directory") =>
    `Inode: 2   Type: ${type}    Mode:  ${mode}   Flags: 0x80000\nGeneration: 0    Version: 0x00000000:00000000\nUser:   985   Group:   985   Project:     0   Size: 4096\n`;

  test("reads the permission bits whether or not the type bits are printed", () => {
    expect(parseDebugfsStat(stat("0700"))).toEqual({ type: "directory", permissions: 0o700, uid: 985, gid: 985 });
    expect(parseDebugfsStat(stat("040700"))?.permissions).toBe(0o700);
    expect(parseDebugfsStat(stat("0755"))?.permissions).toBe(0o755);
    expect(parseDebugfsStat(stat("1777"))?.permissions).toBe(0o1777);
    expect(parseDebugfsStat(stat("0700", "regular"))?.type).toBe("regular");
  });

  test("gives null when a field is missing", () => {
    expect(parseDebugfsStat("")).toBeNull();
    expect(parseDebugfsStat("Inode: 2   Type: directory\nUser:   0   Group:   0\n")).toBeNull();
    expect(parseDebugfsStat("Inode: 2   Type: directory    Mode:  0700\n")).toBeNull();
  });
});

describe("config.txt", () => {
  const source = "dtparam=audio=on\n[cm4]\notg_mode=1\n[pi5]\ndtoverlay=nospi10\n[all]\n";

  test("loads the TPM overlay for every model, after the model sections", () => {
    const built = configTxtWithTpm(source);
    expect(built.startsWith(source)).toBe(true);
    for (const line of TPM_CONFIG_LINES) expect(allModelLines(built)).toContain(line);
    expect(TPM_CONFIG_LINES).toEqual(["dtparam=spi=on", "dtoverlay=tpm-slb9670-ce0"]);
  });

  test("lines under a model filter do not count for every model", () => {
    const filtered = `${source}[pi4]\ndtparam=spi=on\ndtoverlay=tpm-slb9670-ce0\n`;
    expect(allModelLines(filtered)).not.toContain("dtparam=spi=on");
    expect(allModelLines("dtparam=spi=on\n")).toEqual(["dtparam=spi=on"]);
  });

  test("refuses to add the overlay twice", () => {
    expect(() => configTxtWithTpm(configTxtWithTpm(source))).toThrow("already loads");
  });

  test("cmdline tokens", () => {
    expect(parseCmdline("console=tty1 root=PARTUUID=27b94958-02  resize\n")).toEqual(["console=tty1", "root=PARTUUID=27b94958-02", "resize"]);
  });
});

describe("merged device tree", () => {
  // The shape `dtc -I dtb -O dts` prints: properties first, then subnodes; the merge may reorder
  // properties, so compatible comes after reg here.
  const tree = (tpm: string, spidevStatus: string, spiStatus = "okay") =>
    "/dts-v1/;\n\n/ {\n\tsoc {\n" +
    `\t\tspi@7e204000 {\n\t\t\tcompatible = "brcm,bcm2835-spi";\n\t\t\tstatus = "${spiStatus}";\n\t\t\t#address-cells = <0x01>;\n\n` +
    `\t\t\tspidev@0 {\n\t\t\t\tcompatible = "spidev";\n\t\t\t\treg = <0x00>;\n\t\t\t\tstatus = "${spidevStatus}";\n\t\t\t};\n\n` +
    `\t\t\tspidev@1 {\n\t\t\t\tcompatible = "spidev";\n\t\t\t\treg = <0x01>;\n\t\t\t\tstatus = "okay";\n\t\t\t};\n${tpm}` +
    "\t\t};\n\t};\n};\n";
  const tpm = (reg: string, status = "okay") =>
    `\n\t\t\tslb9670@0 {\n\t\t\t\treg = <${reg}>;\n\t\t\t\tstatus = "${status}";\n\t\t\t\tcompatible = "infineon,slb9670";\n\t\t\t\tspi-max-frequency = <0x1e84800>;\n\t\t\t\tphandle = <0xd4>;\n\t\t\t};\n`;

  test("TPM on CE0, SPI0 on and spidev0 off pass in any property order", () => {
    expect(tpmDeviceTreeProblems(tree(tpm("0x00"), "disabled"))).toEqual([]);
  });

  test("names each problem", () => {
    expect(tpmDeviceTreeProblems(tree(tpm("0x01"), "disabled"))).toEqual(["slb9670@0 is not at reg 0"]);
    expect(tpmDeviceTreeProblems(tree(tpm("0x00", "disabled"), "okay"))).toEqual(["slb9670@0 is not enabled", "spidev0 (spidev@0) is not disabled"]);
    expect(tpmDeviceTreeProblems(tree("", "disabled", "disabled"))).toEqual(["spi0 (spi@7e204000) is not enabled", "there is no slb9670@0 node"]);
    expect(tpmDeviceTreeProblems(tree(tpm("0x00").replace("infineon,slb9670", "tcg,tpm_tis-spi"), "disabled"))).toEqual([
      'slb9670@0 is not compatible "infineon,slb9670"',
    ]);
  });
});

describe("timesyncd", () => {
  const main = "[Time]\n#NTP=\n#FallbackNTP=0.debian.pool.ntp.org 1.debian.pool.ntp.org\n";

  test("the drop-in sets the pinned servers and turns the fallback pool off", () => {
    const effective = effectiveTimesyncd(`# /etc/systemd/timesyncd.conf\n${main}\n# /etc/systemd/timesyncd.conf.d/50-trustiot.conf\n${timesyncdDropIn()}`);
    expect(effective).toEqual({ ntp: [...NTP_SERVERS], fallback: [] });
  });

  test("without the drop-in timesyncd would use its built-in pool", () => {
    expect(effectiveTimesyncd(main)).toEqual({ ntp: [], fallback: undefined });
  });

  test("assignments append, an empty one resets, other sections do not count", () => {
    const text = "[Time]\nNTP=a b\nNTP=c\nFallbackNTP=x\n[Other]\nNTP=z\n[Time]\nNTP=\nNTP=d\n";
    expect(effectiveTimesyncd(text)).toEqual({ ntp: ["d"], fallback: ["x"] });
  });
});

describe("sshd", () => {
  test("the drop-in sets every expected value", () => {
    const lines = sshdDropIn()
      .split("\n")
      .filter((l) => l !== "" && !l.startsWith("#"));
    const dump = parseSshdDump(lines.map((l) => l.replace(/^(\S+)/, (k) => k.toLowerCase())).join("\n"));
    expect(sshdProblems(dump)).toEqual([]);
    expect(Object.keys(SSHD_EXPECTED)).toHaveLength(lines.length);
  });

  test("sshd -G of the source image fails the check", () => {
    const source =
      "port 22\nusepam yes\npermitrootlogin without-password\npubkeyauthentication yes\npasswordauthentication yes\n" +
      "kbdinteractiveauthentication no\npermitemptypasswords no\nauthenticationmethods any\nx11forwarding yes\n";
    const problems = sshdProblems(parseSshdDump(source));
    expect(problems).toContain("sshd permitrootlogin is without-password, expected no");
    expect(problems).toContain("sshd passwordauthentication is yes, expected no");
    expect(problems).toContain("sshd authenticationmethods is any, expected publickey");
    expect(problems).toContain(`sshd allowusers is not set, expected ${ADMIN_USER}`);
  });

  test("a value printed twice is not accepted", () => {
    const dump = parseSshdDump(`${Object.entries(SSHD_EXPECTED).map(([k, v]) => `${k} ${v}`).join("\n")}\nallowusers someone\n`);
    expect(sshdProblems(dump)).toEqual([`sshd allowusers is ${ADMIN_USER}, someone, expected ${ADMIN_USER}`]);
  });

  test("Match blocks are found, in any case and indentation", () => {
    expect(sshdMatchLines("PermitRootLogin no\n# Match User x\n  match User root\nMatch Address 192.0.2.0/24\n")).toEqual([3, 4]);
    expect(sshdMatchLines(sshdDropIn())).toEqual([]);
    expect(sshdMatchLines("MatchUser x\n")).toEqual([]);
  });

  test("the test connections cover the administrator, root and the source's user, with documentation addresses", () => {
    const users = SSHD_TEST_CONNECTIONS.map((c) => /(?:^|,)user=([^,]+)/.exec(c)?.[1]);
    expect(users).toEqual([ADMIN_USER, "root", SOURCE_USER]);
    for (const connection of SSHD_TEST_CONNECTIONS) {
      expect(connection).toMatch(/(?:^|,)host=[\w.-]+\.example(,|$)/);
      for (const [, address] of connection.matchAll(/(?:^|,)l?addr=([^,]+)/g)) expect(address).toMatch(/^(192\.0\.2|198\.51\.100|203\.0\.113)\.\d+$/);
      expect(LAN_PATTERNS.some((p) => p.test(connection))).toBe(false);
    }
  });
});

describe("first-boot setup", () => {
  test("every cloud-init unit, and nothing else", () => {
    const names = [
      "cloud-config.service",
      "cloud-config.target",
      "cloud-final.service",
      "cloud-init-hotplugd.service",
      "cloud-init-hotplugd.socket",
      "cloud-init-local.service",
      "cloud-init-main.service",
      "cloud-init-main.service.d",
      "cloud-init-network.service",
      "cloud-init.target",
      "ssh.service",
      "userconfig.service",
      "rpi-cloud-init.conf",
    ];
    expect(cloudInitUnits(names)).toEqual([
      "cloud-config.service",
      "cloud-config.target",
      "cloud-final.service",
      "cloud-init-hotplugd.service",
      "cloud-init-hotplugd.socket",
      "cloud-init-local.service",
      "cloud-init-main.service",
      "cloud-init-network.service",
      "cloud-init.target",
    ]);
    expect(MASKED_SETUP_UNITS).toContain("userconfig.service");
  });
});

describe("gitleaks findings", () => {
  const known = KNOWN_FINDINGS[0];
  const finding = { rule: known.rule, path: known.path, line: 87 };
  const owners = [`${known.package}:arm64`];

  test("a reviewed package file with its exact content is accepted", () => {
    expect(findingProblem(finding, known.sha256, owners, new Set(), KNOWN_FINDINGS)).toBeUndefined();
  });

  test("other content, another owner, another rule, another file or a file the build writes is not", () => {
    const other = "0".repeat(64);
    expect(findingProblem(finding, other, owners, new Set(), KNOWN_FINDINGS)).toContain(`the reviewed file has ${known.sha256}`);
    expect(findingProblem(finding, known.sha256, ["some-package"], new Set(), KNOWN_FINDINGS)).toContain(`expected ${known.package}`);
    expect(findingProblem(finding, known.sha256, [], new Set(), KNOWN_FINDINGS)).toContain("owned by no package");
    expect(findingProblem({ ...finding, rule: "private-key" }, known.sha256, owners, new Set(), KNOWN_FINDINGS)).toContain("private-key in");
    expect(findingProblem({ ...finding, path: "/etc/other.json" }, known.sha256, owners, new Set(), KNOWN_FINDINGS)).toContain("/etc/other.json:87");
    expect(findingProblem(finding, known.sha256, owners, new Set([known.path]), KNOWN_FINDINGS)).toContain("a file the build writes");
  });

  test("every reviewed finding is a package file, never one the build writes", () => {
    const written = [SSHD_DROP_IN, SUDOERS_FILE, TIMESYNCD_DROP_IN, "/etc/fstab", "/etc/cloud/cloud-init.disabled", `/home/${ADMIN_USER}/.ssh/authorized_keys`];
    for (const entry of KNOWN_FINDINGS) {
      expect(entry.path).toMatch(/^\/[\w./-]+$/);
      expect(entry.sha256).toMatch(/^[0-9a-f]{64}$/);
      expect(entry.package).toMatch(/^[a-z0-9][a-z0-9.+-]+$/);
      expect(written).not.toContain(entry.path);
      expect(entry.path.startsWith(`/home/`) || entry.path.startsWith("/root/")).toBe(false);
    }
  });

  test("dpkg-query -S owners", () => {
    expect(parseDpkgOwners("libtss2-fapi1t64:arm64: /etc/a.json\n", "/etc/a.json")).toEqual(["libtss2-fapi1t64:arm64"]);
    expect(parseDpkgOwners("raspi-config, sudo, raspberrypi-sys-mods: /etc/sudoers.d\n", "/etc/sudoers.d")).toEqual(["raspi-config", "sudo", "raspberrypi-sys-mods"]);
    expect(parseDpkgOwners("dpkg-query: no path found matching pattern /etc/x\n", "/etc/x")).toEqual([]);
  });
});

describe("sudo rules", () => {
  const base = ["/etc/sudoers", "/etc/sudoers.d", "/etc/sudoers.d/README", "/etc/sudoers.d/010_proxy", "/etc/ssh"];

  test("only the administrator's file is added", () => {
    expect(sudoersProblems(base, [...base, SUDOERS_FILE], [SUDOERS_FILE])).toEqual([]);
  });

  test("another new file, or a changed source rule, is a problem", () => {
    expect(sudoersProblems(base, [...base, SUDOERS_FILE, "/etc/sudoers.d/010_pi-nopasswd"], [])).toEqual([
      "/etc/sudoers.d/010_pi-nopasswd is not in the source image",
    ]);
    expect(sudoersProblems(base, base, ["/etc/sudoers", "/etc/sudoers.d/010_proxy", "/etc/ssh/sshd_config"])).toEqual([
      "/etc/sudoers differs from the source image's",
      "/etc/sudoers.d/010_proxy differs from the source image's",
    ]);
  });

  test("group members", () => {
    expect(groupMembers("sudo:x:27:\nadm:x:4:a,b\n", "sudo")).toEqual([]);
    expect(groupMembers("sudo:x:27:pi\n", "sudo")).toEqual(["pi"]);
    expect(groupMembers("", "sudo")).toEqual([]);
  });
});

describe("accounts", () => {
  const passwd = "root:x:0:0:root:/root:/bin/bash\nadmin:x:1000:1000::/home/admin:/bin/bash\ntss:x:102:105::/var/lib/tpm:/bin/false\n";

  test("locked and never-set passwords are not usable", () => {
    expect(accountsWithPassword(passwd, "root:*:1::::::\nadmin:*:1::::::\ntss:!:1::::::\n")).toEqual([]);
    expect(accountsWithPassword(passwd, "root:!*:1::::::\nadmin:!$y$j9T$abc:1::::::\n")).toEqual([]);
  });

  test("a hash or an empty password is usable", () => {
    expect(accountsWithPassword(passwd, "root:*:1::::::\nadmin:$y$j9T$salt$hash:1::::::\ntss::1::::::\n")).toEqual(["admin", "tss"]);
  });

  test("a password in /etc/passwd or a hash without a passwd line is found", () => {
    expect(accountsWithPassword("old::0:0::/:/bin/sh\nlocked:*:1:1::/:/bin/sh\n", "")).toEqual(["old (password in /etc/passwd)"]);
    expect(accountsWithPassword(passwd, "ghost:$6$x$y:1::::::\n")).toEqual(["ghost (only in /etc/shadow)"]);
  });

  test("group passwords", () => {
    expect(groupsWithPassword("root:*::\nsudo:!::\nusers:::\nstaff:$6$x$y::\n")).toEqual(["staff"]);
  });

  test("sudo rule is for the administrator only", () => {
    const rules = sudoersFile()
      .split("\n")
      .filter((l) => l !== "" && !l.startsWith("#"));
    expect(rules).toEqual([`${ADMIN_USER} ALL=(ALL:ALL) NOPASSWD: ALL`]);
  });
});
