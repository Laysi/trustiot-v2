// The gateway's local login surface (Laysi/trustiot-v2#815): the files the build writes and the
// parsers the image check reads them back with.
import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import { REPO } from "../src/host.ts";
import {
  CONSOLE_INSTANCES,
  DENIED_USB_CLASSES,
  DENIED_USB_DRIVERS,
  LOGIND_EXPECTED,
  MASKED_CONSOLE_UNITS,
  MASKED_GENERATORS,
  MASKED_SSH_SOCKETS,
  OVERRIDING_RULES_DIRS,
  PANIC_ARGUMENT,
  SULOGIN_UNITS,
  USB_DENY_RULES,
  cmdlineWithPanic,
  effectiveSection,
  initramfsPanicProblems,
  legacyStartupScript,
  lockedPassword,
  logindDropIn,
  readsTerminal,
  rulesWithLastRule,
  rulesWritingAuthorized,
  unitOfFile,
  usbDenyRules,
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

  test("adds panic= and keeps everything else", () => {
    const built = cmdlineWithPanic(source);
    expect(built).toBe(`${source.trimEnd()} ${PANIC_ARGUMENT}\n`);
    expect(cmdlineWithPanic(source.trimEnd())).toBe(`${source.trimEnd()} ${PANIC_ARGUMENT}`);
    expect(PANIC_ARGUMENT).toMatch(/^panic=[1-9][0-9]*$/);
  });

  test("refuses a second panic=, a second line or an empty file", () => {
    expect(() => cmdlineWithPanic(cmdlineWithPanic(source))).toThrow("already sets panic=");
    expect(() => cmdlineWithPanic(`${source}quiet\n`)).toThrow("more than one line");
    expect(() => cmdlineWithPanic("\n")).toThrow("empty");
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

describe("USB", () => {
  test("denies HID and mass storage by class and by driver, on add and bind", () => {
    expect(DENIED_USB_CLASSES.map((c) => c.code)).toEqual(["03", "08"]);
    expect(DENIED_USB_DRIVERS).toEqual(["usbhid", "usb-storage", "uas", "ums-*"]);
    const rules = usbDenyRules()
      .split("\n")
      .filter((l) => l !== "" && !l.startsWith("#"));
    expect(rules).toEqual([
      'ACTION=="add|bind", SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_interface", ATTR{bInterfaceClass}=="03|08", ATTR{authorized}="0"',
      'ACTION=="add|bind", SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_interface", DRIVER=="usbhid|usb-storage|uas|ums-*", ATTR{authorized}="0"',
    ]);
  });

  test("lives where a same-named file in /run overrides it", () => {
    expect(USB_DENY_RULES).toStartWith("/usr/lib/udev/rules.d/");
    // Sorted before 80-drivers.rules, which loads driver modules.
    expect(USB_DENY_RULES.split("/").at(-1)! < "80-drivers.rules").toBe(true);
  });

  test("finds every rules file that writes authorized", () => {
    const files = new Map([
      [USB_DENY_RULES, usbDenyRules()],
      ["/usr/lib/udev/rules.d/60-x.rules", 'ACTION=="add", ATTR{authorized}=="0", GOTO="end"\n'],
      ["/usr/lib/udev/rules.d/90-y.rules", "# ATTR{authorized}=\"1\"\n"],
      ["/etc/udev/rules.d/99-z.rules", 'SUBSYSTEM=="usb", ATTR{authorized}:="1"\n'],
      ["/etc/udev/rules.d/99-w.rules", 'SUBSYSTEM=="usb", ATTR{interface_authorized_default}="1"\n'],
      ["/etc/udev/rules.d/99-v.rules", 'SUBSYSTEM=="usb", RUN+="/bin/sh -c \'echo 1 > /sys$devpath/authorized\'"\n'],
      ["/usr/lib/udev/rules.d/50-u.rules", 'SUBSYSTEM=="usb", OPTIONS+="last_rule"\n'],
    ]);
    expect(rulesWritingAuthorized(files)).toEqual(["/etc/udev/rules.d/99-v.rules", "/etc/udev/rules.d/99-w.rules", "/etc/udev/rules.d/99-z.rules", USB_DENY_RULES]);
    expect(rulesWithLastRule(files)).toEqual(["/usr/lib/udev/rules.d/50-u.rules"]);
  });

  test("every directory that replaces the rules by name is checked", () => {
    expect(OVERRIDING_RULES_DIRS).toEqual(["/etc/udev/rules.d", "/usr/local/lib/udev/rules.d"]);
  });
});

test("README describes the login surface", async () => {
  const readme = await Bun.file(join(REPO, "edge/image/README.md")).text();
  for (const value of [...MASKED_CONSOLE_UNITS, ...MASKED_GENERATORS, ...MASKED_SSH_SOCKETS, USB_DENY_RULES, PANIC_ARGUMENT, ...DENIED_USB_DRIVERS]) expect(readme).toContain(value);
});
