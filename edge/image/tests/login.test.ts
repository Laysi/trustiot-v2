// The gateway's local login surface (Laysi/trustiot-v2#815): the files the build writes and the
// parsers the image check reads them back with.
import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import { REPO } from "../src/host.ts";
import {
  AUTHORIZED_ATTRIBUTE,
  BOOT_ARGUMENTS,
  CONSOLE_INSTANCES,
  LOGIND_EXPECTED,
  MASKED_CONSOLE_UNITS,
  MASKED_GENERATORS,
  MASKED_SSH_SOCKETS,
  PANIC_ARGUMENT,
  SULOGIN_UNITS,
  SYSFS_CONFIG,
  TMPFILES_DIRS,
  UDEV_RULES_DIRS,
  USBCORE_ARGUMENT,
  USB_AUTHORIZING_PACKAGES,
  cmdlineProblems,
  cmdlineWithArguments,
  configCmdlineRedirects,
  configUsbGadget,
  kernelConfigProblems,
  parameterName,
  effectiveSection,
  initramfsPanicProblems,
  legacyStartupScript,
  lockedPassword,
  logindDropIn,
  readsTerminal,
  rulePrograms,
  rulesWritingAuthorized,
  sysfsWritingAuthorized,
  tmpfilesWritingAuthorized,
  unitOfFile,
} from "../src/login.ts";

describe("consoles", () => {
  test("masks every getty template and the console shells", () => {
    for (const unit of ["getty@.service", "autovt@.service", "serial-getty@.service", "getty-static.service", "debug-shell.service", "systemd-firstboot.service"]) {
      expect(MASKED_CONSOLE_UNITS).toContain(unit as (typeof MASKED_CONSOLE_UNITS)[number]);
    }
    // The emergency and rescue shells stay: sulogin refuses a locked root.
    for (const unit of SULOGIN_UNITS) expect(MASKED_CONSOLE_UNITS).not.toContain(unit as never);
  });

  test("asks about the screen's consoles and the serial ports of the platform", () => {
    for (const unit of ["getty@tty1.service", "autovt@tty6.service", "serial-getty@ttyS0.service", "serial-getty@ttyAMA0.service"]) {
      expect(CONSOLE_INSTANCES).toContain(unit as (typeof CONSOLE_INSTANCES)[number]);
    }
    // Every instance belongs to a masked template.
    for (const instance of CONSOLE_INSTANCES) {
      const template = instance.replace(/@[^.]+\./, "@.");
      expect(MASKED_CONSOLE_UNITS).toContain(template as (typeof MASKED_CONSOLE_UNITS)[number]);
    }
  });

  test("finds units that read a terminal", () => {
    expect(readsTerminal("[Service]\nExecStart=-/sbin/agetty - $TERM\nStandardInput=tty\nTTYPath=/dev/%I\n")).toBe(true);
    expect(readsTerminal("[Service]\nStandardInput=tty-force\n")).toBe(true);
    expect(readsTerminal("[Service]\nStandardInput = tty-fail\n")).toBe(true);
    expect(readsTerminal("[Service]\nTTYPath=/dev/tty9\n")).toBe(true);
    expect(readsTerminal("[Service]\nStandardInput=null\nStandardOutput=tty\n")).toBe(false);
    expect(readsTerminal("[Service]\n# StandardInput=tty\nExecStart=/bin/true\n")).toBe(false);
    // Programs that open a terminal themselves.
    expect(readsTerminal("[Service]\nExecStart=/sbin/agetty ttyAMA0 115200\n")).toBe(true);
    expect(readsTerminal("[Service]\nExecStart=/bin/sh -c 'exec sh -i </dev/tty1 >/dev/tty1'\n")).toBe(true);
    expect(readsTerminal("[Service]\nExecStartPre=-/usr/lib/systemd/systemd-sulogin-shell rescue\n")).toBe(true);
    expect(readsTerminal("[Service]\nExecStart=/usr/bin/tee /dev/console\n")).toBe(true);
    expect(readsTerminal("[Service]\nExecStart=/usr/sbin/sshd -D\nDescription=no getty here\n")).toBe(false);
  });

  test("finds scripts started at boot outside the units", () => {
    for (const path of ["/etc/rc.local", "/etc/init.d/backdoor"]) expect(legacyStartupScript(path)).toBe(true);
    for (const path of ["/etc/init.d", "/etc/rc2.d/S01ssh", "/etc/rcS.d/S01x", "/etc/init.d/x/y", "/etc/rc.local.d/x", "/usr/lib/systemd/system/x.service"]) expect(legacyStartupScript(path)).toBe(false);
  });

  test("maps unit files and drop-ins to their service", () => {
    expect(unitOfFile("/usr/lib/systemd/system/getty@.service")).toBe("getty@.service");
    expect(unitOfFile("/etc/systemd/system/getty@tty1.service.d/autologin.conf")).toBe("getty@tty1.service");
    expect(unitOfFile("/etc/systemd/system/service.d/x.conf")).toBe("*");
    expect(unitOfFile("/usr/lib/systemd/system/getty.target.wants/getty@tty1.service")).toBeUndefined();
    expect(unitOfFile("/usr/lib/systemd/system/getty.target")).toBeUndefined();
    expect(unitOfFile("/usr/lib/systemd/user/rpi-connect.service")).toBeUndefined();
  });

  test("a locked root is what makes sulogin refuse", () => {
    expect(lockedPassword("*")).toBe(true);
    expect(lockedPassword("!")).toBe(true);
    expect(lockedPassword("!$y$j9T$abc")).toBe(true);
    expect(lockedPassword("")).toBe(false);
    expect(lockedPassword("$y$j9T$abc")).toBe(false);
    expect(lockedPassword(undefined)).toBe(false);
  });

  test("sshd gets no local socket from systemd-ssh-generator", () => {
    expect(MASKED_GENERATORS).toEqual(["/etc/systemd/system-generators/systemd-ssh-generator"]);
    expect(MASKED_SSH_SOCKETS).toContain("sshd-unix-local.socket");
    expect(MASKED_SSH_SOCKETS).toContain("sshd-vsock.socket");
  });

  test("logind starts no getty on any virtual console", () => {
    const merged = `# /usr/lib/systemd/logind.conf\n[Login]\n#NAutoVTs=6\nNAutoVTs=6\n\n# /etc/systemd/logind.conf.d/50-trustiot.conf\n${logindDropIn()}`;
    const effective = effectiveSection(merged, "Login");
    for (const [key, value] of Object.entries(LOGIND_EXPECTED)) expect(effective.get(key)).toBe(value);
    expect(LOGIND_EXPECTED).toEqual({ NAutoVTs: "0", ReserveVT: "0" });
    // A later drop-in wins; other sections do not count.
    expect(effectiveSection(`${merged}\n[Login]\nNAutoVTs=2\n`, "Login").get("NAutoVTs")).toBe("2");
    expect(effectiveSection(`${merged}\n[Other]\nNAutoVTs=2\n`, "Login").get("NAutoVTs")).toBe("0");
  });
});

describe("kernel command line", () => {
  const source = "console=serial0,115200 console=tty1 root=PARTUUID=27b94958-02 rootfstype=ext4 fsck.repair=yes rootwait resize\n";

  test("adds panic= and usbcore.authorized_default=0 and keeps everything else", () => {
    expect(BOOT_ARGUMENTS).toEqual([PANIC_ARGUMENT, USBCORE_ARGUMENT]);
    expect(cmdlineWithArguments(source)).toBe(`${source.trimEnd()} panic=10 usbcore.authorized_default=0\n`);
    expect(cmdlineWithArguments(source.trimEnd())).toBe(`${source.trimEnd()} panic=10 usbcore.authorized_default=0`);
    expect(PANIC_ARGUMENT).toMatch(/^panic=[1-9][0-9]*$/);
    expect(USBCORE_ARGUMENT).toBe("usbcore.authorized_default=0");
  });

  test("refuses a second panic= or usbcore.authorized_default=, a second line or an empty file", () => {
    expect(() => cmdlineWithArguments(cmdlineWithArguments(source))).toThrow("already sets panic=");
    expect(() => cmdlineWithArguments(source.replace("resize", "resize usbcore.authorized_default=1"))).toThrow("already sets usbcore.authorized_default=");
    expect(() => cmdlineWithArguments(source.replace("resize", "resize usbcore.authorized-default=1"))).toThrow("already sets usbcore.authorized_default=");
    expect(() => cmdlineWithArguments(source.replace("resize", "resize -- single"))).toThrow("has --");
    expect(() => cmdlineWithArguments(`${source}quiet\n`)).toThrow("more than one line");
    expect(() => cmdlineWithArguments("\n")).toThrow("empty");
  });

  test("reads the built command line back", () => {
    const built = cmdlineWithArguments(source);
    expect(cmdlineProblems(built)).toEqual([]);
    expect(parameterName("usbcore.authorized-default=1")).toBe("usbcore.authorized_default");
    expect(parameterName("rootwait")).toBe("rootwait");
    expect(cmdlineProblems(built.replace(" usbcore.authorized_default=0", ""))).toEqual(["cmdline.txt has no usbcore.authorized_default=, expected usbcore.authorized_default=0 once"]);
    expect(cmdlineProblems(built.replace("rootwait", "rootwait usbcore.authorized-default=1"))).toEqual([
      "cmdline.txt has usbcore.authorized-default=1 usbcore.authorized_default=0, expected usbcore.authorized_default=0 once",
    ]);
    expect(cmdlineProblems(built.replace("usbcore.authorized_default=0", "usbcore.authorized_default=1"))).toHaveLength(1);
    expect(cmdlineProblems(built.replace("rootwait", "rootwait --"))).toEqual(["cmdline.txt has --: the kernel does not read the parameters after it"]);
    expect(cmdlineProblems(built.replace(" usbcore.authorized_default=0", "\nusbcore.authorized_default=0"))).toEqual([
      "cmdline.txt has more than one line; the firmware reads only the first",
    ]);
    expect(cmdlineProblems(built.replace("rootwait", "rootwait modules-load=dwc2,g_ether"))).toEqual(["cmdline.txt loads a USB gadget driver: modules-load=dwc2,g_ether"]);
  });

  test("finds dwc2 outside host mode and a kernel command line that replaces cmdline.txt", () => {
    const config = "[cm5]\ndtoverlay=dwc2,dr_mode=host\n[all]\n# dtoverlay=dwc2\ndtoverlay=dwc2\ndtoverlay=dwc2,dr_mode=peripheral\ndtoverlay=dwc2-other\n";
    expect(configUsbGadget(config)).toEqual(["dtoverlay=dwc2", "dtoverlay=dwc2,dr_mode=peripheral"]);
    expect(kernelConfigProblems('CONFIG_CMDLINE="console=ttyAMA0,115200"\n# CONFIG_CMDLINE_FORCE is not set\n')).toEqual([]);
    expect(kernelConfigProblems("CONFIG_CMDLINE_FORCE=y\n")).toEqual(["CONFIG_CMDLINE_FORCE=y: the kernel ignores cmdline.txt"]);
    expect(kernelConfigProblems('CONFIG_CMDLINE="usbcore.authorized_default=1"\nCONFIG_CMDLINE_EXTEND=y\n')).toHaveLength(1);
  });

  test("finds config.txt lines that move the command line elsewhere", () => {
    const config = "# cmdline=other.txt\ndtparam=audio=on\n[pi4]\ncmdline=other.txt\n[all]\n os_prefix = alt/\ninclude extra.txt\nkernel=kernel8.img\n";
    expect(configCmdlineRedirects(config)).toEqual(["cmdline=other.txt", "os_prefix = alt/", "include extra.txt"]);
    expect(configCmdlineRedirects("arm_64bit=1\n[all]\ndtoverlay=tpm-slb9670\n")).toEqual([]);
  });

  // Excerpts of initramfs-tools 0.148 (Debian 13), as unmkinitramfs unpacks them.
  const init = 'for x in $(cat /proc/cmdline); do\n\tcase $x in\n\tpanic=*)\n\t\tpanic="${x#panic=}"\n\t\t;;\n\tesac\ndone\n';
  const functions =
    'panic()\n{\n\techo "$@"\n\t# The panic= parameter implies we should disallow console access\n\tif [ -n "${panic?}" ]; then\n' +
    '\t\tdelay="${panic}"\n\t\tif [ -n "${delay}" ]; then\n\t\t\tsleep "${delay}"\n\t\t\treboot -f\n\t\tfi\n\t\texit\n\tfi\n' +
    "\tREASON=\"$*\" PS1='(initramfs) ' sh -i </dev/console >/dev/console 2>&1\n}\n";

  test("accepts an initramfs that reboots on panic= before its shell", () => {
    expect(initramfsPanicProblems(init, functions)).toEqual([]);
  });

  test("names what is missing", () => {
    expect(initramfsPanicProblems("", functions)).toEqual(["init does not read panic= from the kernel command line"]);
    expect(initramfsPanicProblems(init, "")).toEqual(["scripts/functions has no panic()"]);
    expect(initramfsPanicProblems(init, functions.replace("reboot -f", "true"))).toEqual(["panic() does not reboot when panic= is set"]);
    const shellFirst = functions.replace('\t# The panic=', "\tsh -i </dev/console\n\t# The panic=");
    expect(initramfsPanicProblems(init, shellFirst)).toEqual(["panic() starts a shell before it checks panic="]);
  });
});

describe("USB: nothing authorizes a device again", () => {
  test("finds every rules file that writes authorized", () => {
    const files = new Map([
      ["/usr/lib/udev/rules.d/60-x.rules", 'ACTION=="add", ATTR{authorized}=="0", GOTO="end"\n'],
      ["/usr/lib/udev/rules.d/90-y.rules", "# ATTR{authorized}=\"1\"\n"],
      ["/etc/udev/rules.d/99-z.rules", 'SUBSYSTEM=="usb", ATTR{authorized}:="1"\n'],
      ["/etc/udev/rules.d/99-w.rules", 'SUBSYSTEM=="usb", ATTR{interface_authorized_default}="1"\n'],
      ["/etc/udev/rules.d/99-v.rules", 'SUBSYSTEM=="usb", RUN+="/bin/sh -c \'echo 1 > /sys$devpath/authorized\'"\n'],
      ["/run/udev/rules.d/99-u.rules", 'SUBSYSTEM=="usb", ATTR{../authorized_default}="1"\n'],
      ["/usr/lib/udev/rules.d/99-t.rules", 'SUBSYSTEM=="usb", PROGRAM="/usr/bin/set-authorized", RESULT=="1"\n'],
      ["/usr/lib/udev/rules.d/50-s.rules", 'SUBSYSTEM=="usb", OPTIONS+="last_rule"\n'],
      ["/usr/lib/udev/rules.d/98-r.rules", 'SUBSYSTEM=="usb", ATTR{authorized}+="1"\n'],
    ]);
    expect(rulesWritingAuthorized(files)).toEqual([
      "/etc/udev/rules.d/99-v.rules",
      "/etc/udev/rules.d/99-w.rules",
      "/etc/udev/rules.d/99-z.rules",
      "/run/udev/rules.d/99-u.rules",
      "/usr/lib/udev/rules.d/98-r.rules",
      "/usr/lib/udev/rules.d/99-t.rules",
    ]);
  });

  test("lists the programs rules run, as absolute paths under /usr", () => {
    const text =
      'RUN+="usb-helper $devpath"\nPROGRAM="/usr/bin/x --flag"\nIMPORT{program}="probe"\nIMPORT{builtin}="usb_id"\nIMPORT{file}="/etc/f"\n' +
      'RUN{builtin}+="kmod load $env{MODALIAS}"\nRUN{program}+="/bin/sh -c \'true\'"\n# RUN+="/usr/bin/commented"\nRUN+="$env{X}"\n' +
      'RUN+="/bin/sh /sbin/usb-auth $kernel"\nRUN+="/usr/bin/env -S /usr/libexec/y"\n' +
      // Seen in the source image: the words of an inline sh -c command are not script paths.
      "RUN+=\"/bin/sh -c 'echo $env{X} | sed -n s//p' /sys/class/gpio\"\n";
    expect(rulePrograms(text)).toEqual(["/usr/bin/env", "/usr/bin/sh", "/usr/bin/x", "/usr/lib/udev/probe", "/usr/lib/udev/usb-helper", "/usr/libexec/y", "/usr/sbin/usb-auth"]);
  });

  test("names the attribute only where sysfs or udev write it", () => {
    for (const text of ["echo 1 > /sys/bus/usb/devices/usb1/authorized_default", 'ATTR{authorized}="1"', "echo 1 > $d/authorized", "interface_authorized_default", "authorized_default=1"]) {
      expect(text).toMatch(AUTHORIZED_ATTRIBUTE);
    }
    for (const text of ["AuthorizedKeysFile .ssh/authorized_keys", "~/.ssh/authorized_keys", "only authorized staff", "unauthorized"]) {
      expect(text).not.toMatch(AUTHORIZED_ATTRIBUTE);
    }
  });

  test("finds tmpfiles.d and sysfsutils lines that write authorized", () => {
    const tmpfiles = "# w /sys/bus/usb/devices/usb1/authorized_default - - - - 1\nd /run/x 0755 root root -\nw+ /sys/bus/usb/devices/usb2/authorized_default - - - - 1\nf /sys/devices/platform/x/usb1/authorized - - - - 1\n";
    expect(tmpfilesWritingAuthorized(tmpfiles)).toEqual(["w+ /sys/bus/usb/devices/usb2/authorized_default - - - - 1", "f /sys/devices/platform/x/usb1/authorized - - - - 1"]);
    expect(tmpfilesWritingAuthorized("d /run/authorized-cache 0755 root root -\n").length).toBe(1);
    // Seen in the source image: provisioning root's authorized_keys is not the attribute.
    expect(tmpfilesWritingAuthorized("f^ /root/.ssh/authorized_keys :0600 root :root - ssh.authorized_keys.root\n")).toEqual([]);
    expect(sysfsWritingAuthorized("# bus/usb/devices/usb1/authorized_default = 1\nbus/usb/devices/usb1/authorized_default = 1\nmodule/x/parameters/y = 2\n")).toEqual([
      "bus/usb/devices/usb1/authorized_default = 1",
    ]);
  });

  test("checks every rules and tmpfiles directory, and usbguard", () => {
    expect(UDEV_RULES_DIRS).toEqual(["/etc/udev/rules.d", "/run/udev/rules.d", "/usr/local/lib/udev/rules.d", "/usr/lib/udev/rules.d"]);
    expect(TMPFILES_DIRS).toEqual(["/etc/tmpfiles.d", "/run/tmpfiles.d", "/usr/local/lib/tmpfiles.d", "/usr/lib/tmpfiles.d"]);
    expect(SYSFS_CONFIG).toEqual(["/etc/sysfs.conf", "/etc/sysfs.d"]);
    expect(USB_AUTHORIZING_PACKAGES).toContain("usbguard");
  });
});

test("README describes the login surface", async () => {
  const readme = await Bun.file(join(REPO, "edge/image/README.md")).text();
  for (const value of [...MASKED_CONSOLE_UNITS, ...MASKED_GENERATORS, ...MASKED_SSH_SOCKETS, PANIC_ARGUMENT, USBCORE_ARGUMENT, ...UDEV_RULES_DIRS, ...TMPFILES_DIRS, ...SYSFS_CONFIG, ...USB_AUTHORIZING_PACKAGES]) {
    expect(readme).toContain(value);
  }
  expect(readme).not.toContain("70-trustiot-usb-deny.rules");
});
