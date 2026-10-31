// The gateway's local login surface (Laysi/trustiot-v2#815): no login prompt on the screen or the
// serial console, no shell when the initramfs fails, and USB keyboards and storage without a
// driver. Files the build writes and the parsers the check reads the image back with; pure
// functions only, so the unit tests cover them without an image. README.md, section 「登入面」.

const MARK = "TrustIoT gateway image (edge/image, Laysi/trustiot-v2#815)";

// ---- consoles ------------------------------------------------------------------------------------

/**
 * Units that put a login prompt or a shell on a console, masked in the image: the screen's virtual
 * consoles (getty@ and its alias autovt@, which logind starts when someone switches to a console,
 * and Debian's getty-static for systems without logind), the serial console (serial-getty@, which
 * systemd's getty generator starts for a serial console= on the kernel command line), /dev/console
 * and container consoles, and the debug shell (a root shell without a password on tty9, started by
 * systemd.debug_shell on the kernel command line). Two more units attach to a console:
 * systemd-firstboot asks on the console for settings, a root password among them, on a first boot,
 * and systemd-storagetm (started only from the kernel command line) shows its status there while it
 * exports every local disk over the network. A masked template masks every instance of it.
 */
export const MASKED_CONSOLE_UNITS = [
  "autovt@.service",
  "console-getty.service",
  "container-getty@.service",
  "debug-shell.service",
  "getty-static.service",
  "getty@.service",
  "serial-getty@.service",
  "systemd-firstboot.service",
  "systemd-storagetm.service",
] as const;

/**
 * Instances the check asks systemctl about: every virtual console logind could hand out, and the
 * serial devices of the Raspberry Pi platform (ttyS0 is the mini UART, ttyAMA0 the PL011, ttyAMA10
 * the debug UART of later models). Each must come out masked.
 */
export const CONSOLE_INSTANCES = [
  ...[1, 2, 3, 4, 5, 6].flatMap((n) => [`getty@tty${n}.service`, `autovt@tty${n}.service`]),
  ...["ttyS0", "ttyS1", "ttyAMA0", "ttyAMA1", "ttyAMA10", "hvc0"].map((tty) => `serial-getty@${tty}.service`),
  "container-getty@0.service",
] as const;

/**
 * Units allowed to read a terminal without being masked: systemd's emergency and rescue modes run
 * sulogin, which refuses to start a shell while root's password field is locked ("*" or "!"), so
 * they print a message and give no shell. SYSTEMD_SULOGIN_FORCE would make sulogin start one
 * anyway; the check makes sure nothing sets it.
 */
export const SULOGIN_UNITS = ["emergency.service", "rescue.service"] as const;

/**
 * True when a unit file connects the service to a terminal (StandardInput=tty, tty-force or
 * tty-fail, or a TTYPath=), or runs a program that opens one itself (an Exec*= line naming a getty,
 * sulogin, an interactive shell or a terminal device): the ways a getty, a shell or a prompt on a
 * console is written. A heuristic: a program that opens a terminal under another name is missed.
 */
export function readsTerminal(unitText: string): boolean {
  return unitText.split("\n").some((raw) => {
    const line = raw.trim();
    if (/^StandardInput\s*=\s*tty(-force|-fail)?\s*$/.test(line) || /^TTYPath\s*=\s*\S/.test(line)) return true;
    if (!/^Exec[A-Za-z]*\s*=/.test(line)) return false;
    return /\b(a|m|min)?getty\b|\bsulogin\b|\bsh -i\b|\/dev\/(tty|console|hvc)/.test(line);
  });
}

/**
 * Other ways a script reaches the boot: sysv-generator runs /etc/init.d scripts and
 * rc-local-generator /etc/rc.local. The /etc/rc?.d links only enable an init.d script (systemctl
 * enable and mask write them for Debian's scripts), so a new script needs a new init.d file.
 */
export function legacyStartupScript(path: string): boolean {
  return /^\/etc\/(rc\.local|init\.d\/[^/]+)$/.test(path);
}

/** Directories the system manager loads service units and their drop-ins from (systemd.unit(5)). */
export const UNIT_DIRS = ["/etc/systemd/system", "/usr/local/lib/systemd/system", "/usr/lib/systemd/system"] as const;

/**
 * The service a file configures: a unit file directly in one of UNIT_DIRS (`getty@.service`) or a
 * drop-in in its `.d` directory (`getty@.service.d/x.conf`, also for `service.d`, which applies to
 * every service: then "*"). Undefined for any other path.
 */
export function unitOfFile(path: string): string | undefined {
  for (const dir of UNIT_DIRS) {
    if (!path.startsWith(`${dir}/`)) continue;
    const parts = path.slice(dir.length + 1).split("/");
    if (parts.length === 1 && parts[0]?.endsWith(".service")) return parts[0];
    if (parts.length === 2 && parts[1]?.endsWith(".conf")) {
      if (parts[0] === "service.d") return "*";
      if (parts[0]?.endsWith(".service.d")) return parts[0].slice(0, -2);
    }
  }
  return undefined;
}

/**
 * systemd-ssh-generator, masked by a link to /dev/null (systemd.generator(7)). At every boot it
 * would add sshd-unix-local.socket, sshd listening on /run/ssh-unix-local/socket for any local
 * process (no SocketMode=, so the socket is 0666), including the agent's account, whose egress
 * rules only cover the network; on a VM also sshd-vsock.socket, and sshd-extra.socket for
 * systemd.ssh_listen= on the kernel command line. Maintenance uses SSH over the network only.
 */
export const MASKED_GENERATORS = ["/etc/systemd/system-generators/systemd-ssh-generator"] as const;

/** The sockets that generator writes, masked too: a mask in /etc wins over generator output. */
export const MASKED_SSH_SOCKETS = ["sshd-extra.socket", "sshd-unix-local.socket", "sshd-vsock.socket"] as const;

/** A password field sulogin treats as locked (util-linux: it starts with "!" or "*"). */
export function lockedPassword(field: string | undefined): boolean {
  return field !== undefined && (field.startsWith("!") || field.startsWith("*"));
}

export const LOGIND_DROP_IN = "/etc/systemd/logind.conf.d/50-trustiot.conf";

/**
 * logind starts autovt@ on a virtual console when someone switches to it (NAutoVTs=) and always on
 * the reserved one (ReserveVT=). Both are masked, so this only keeps logind from trying.
 */
export const LOGIND_EXPECTED: Readonly<Record<string, string>> = { NAutoVTs: "0", ReserveVT: "0" };

export function logindDropIn(): string {
  return (
    `# ${MARK}: no login prompt on any virtual\n` +
    `# console. The getty units are masked as well; this keeps logind from starting them.\n` +
    `[Login]\n${Object.entries(LOGIND_EXPECTED).map(([key, value]) => `${key}=${value}`).join("\n")}\n`
  );
}

/**
 * The value each key of one section ends up with, from `systemd-analyze cat-config` output (the
 * main file and its drop-ins in the order systemd reads them): the last assignment wins.
 */
export function effectiveSection(catConfig: string, section: string): Map<string, string> {
  const result = new Map<string, string>();
  let current = "";
  for (const raw of catConfig.split("\n")) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#") || line.startsWith(";")) continue;
    if (line.startsWith("[")) {
      current = line;
      continue;
    }
    if (current !== `[${section}]`) continue;
    const eq = line.indexOf("=");
    if (eq < 0) continue;
    result.set(line.slice(0, eq).trim(), line.slice(eq + 1).trim());
  }
  return result;
}

// ---- kernel command line -------------------------------------------------------------------------

/**
 * On cmdline.txt: the kernel reboots 10 seconds after a panic, and the initramfs (initramfs-tools'
 * panic()) reboots instead of starting its rescue shell, a root shell without a password on the
 * console. The USB keyboard driver is built into the kernel and the USB rules below are not in the
 * initramfs, so that shell would take input from a keyboard; the gateway is unattended, so
 * rebooting is the useful outcome anyway.
 */
export const PANIC_ARGUMENT = "panic=10";

/**
 * cmdline.txt with PANIC_ARGUMENT appended after the last argument; still one line, as the firmware
 * requires, and ending in a newline only if the source did.
 */
export function cmdlineWithPanic(text: string): string {
  const line = text.replace(/\r?\n$/, "");
  if (/[\r\n]/.test(line)) throw new Error("cmdline.txt has more than one line");
  const tokens = line.split(/\s+/).filter((t) => t !== "");
  if (tokens.length === 0) throw new Error("cmdline.txt is empty");
  if (tokens.some((t) => t.startsWith("panic="))) throw new Error("cmdline.txt already sets panic=");
  return `${line.trimEnd()} ${PANIC_ARGUMENT}${text.endsWith("\n") ? "\n" : ""}`;
}

/**
 * Problems with the initramfs-tools scripts unpacked from one initramfs: `init` must read panic=
 * from the kernel command line and `scripts/functions` must reboot or halt instead of starting a
 * shell when it is set. The texts are the files' content, empty when missing.
 */
export function initramfsPanicProblems(init: string, functions: string): string[] {
  const problems: string[] = [];
  if (!/^\s*panic=\*\)\s*$/m.test(init) || !/panic="\$\{x#panic=\}"/.test(init)) problems.push("init does not read panic= from the kernel command line");
  const body = /^panic\(\)\s*\{([\s\S]*?)^\}/m.exec(functions)?.[1];
  if (body === undefined) problems.push("scripts/functions has no panic()");
  else {
    const guard = body.indexOf('if [ -n "${panic?}" ]; then');
    const reboot = body.indexOf("reboot -f");
    const shell = body.search(/\bsh -i\b|\$\{SHELL\}|\/bin\/sh\b/);
    if (guard < 0 || reboot < guard) problems.push("panic() does not reboot when panic= is set");
    else if (shell >= 0 && shell < reboot) problems.push("panic() starts a shell before it checks panic=");
  }
  return problems;
}

// ---- USB -----------------------------------------------------------------------------------------

/**
 * USB interface classes that get no driver: HID (keyboards, mice, barcode scanners and other input
 * devices) and mass storage. Every other class keeps working, for example hubs, USB network adapters
 * and modems (CDC) and USB serial adapters (vendor-specific class).
 */
export const DENIED_USB_CLASSES = [
  { code: "03", name: "HID" },
  { code: "08", name: "mass storage" },
] as const;

/**
 * The kernel drivers of those devices. The Raspberry Pi kernels build usbhid, usb-storage and uas
 * in (not as modules), so blocking modules would not stop them; the ums-* modules drive storage
 * devices that the kernel knows by vendor and product ID, whatever class they announce.
 */
export const DENIED_USB_DRIVERS = ["usbhid", "usb-storage", "uas", "ums-*"] as const;

/**
 * In /usr/lib, so that a file of the same name in /run/udev/rules.d (the maintenance exception,
 * README.md 「登入面」) overrides it until the next boot.
 */
export const USB_DENY_RULES = "/usr/lib/udev/rules.d/70-trustiot-usb-deny.rules";

export function usbDenyRules(): string {
  const match = 'SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_interface"';
  const classes = DENIED_USB_CLASSES.map((c) => c.code).join("|");
  return (
    `# ${MARK}: USB keyboards and\n` +
    `# storage do not work. Their drivers are built into the kernel and bind as the device appears;\n` +
    `# deauthorizing the interface unbinds the driver and keeps any other from binding. "add" also\n` +
    `# covers devices present at boot (systemd-udev-trigger replays them), "bind" a driver that binds\n` +
    `# after the device appeared.\n` +
    `# Interface classes ${DENIED_USB_CLASSES.map((c) => `${c.code} (${c.name})`).join(" and ")}:\n` +
    `ACTION=="add|bind", ${match}, ATTR{bInterfaceClass}=="${classes}", ATTR{authorized}="0"\n` +
    `# Their drivers, whatever class the device announces:\n` +
    `ACTION=="add|bind", ${match}, DRIVER=="${DENIED_USB_DRIVERS.join("|")}", ATTR{authorized}="0"\n`
  );
}

/**
 * Rules files that write an interface's or device's `authorized` attribute, by file name: with
 * ATTR{} or through a program (RUN+=, PROGRAM=) that names it.
 */
export function rulesWritingAuthorized(files: ReadonlyMap<string, string>): string[] {
  return rulesMatching(files, (line) => /ATTRS?\{(interface_)?authorized(_default)?\}\s*:?=(?!=)/.test(line) || (/\b(RUN|PROGRAM|IMPORT)\b/.test(line) && /authorized/.test(line)));
}

/**
 * Rules files that stop udev from running later rules for a device (OPTIONS+="last_rule"): one that
 * sorts before USB_DENY_RULES would skip it.
 */
export function rulesWithLastRule(files: ReadonlyMap<string, string>): string[] {
  return rulesMatching(files, (line) => /last_rule/.test(line));
}

/**
 * Directories a rules file of the same name in would replace USB_DENY_RULES: udev(7) reads /etc,
 * /run, /usr/local/lib and /usr/lib, and the first file of a name wins. /run is empty in the image
 * (the maintenance exception uses it at run time only).
 */
export const OVERRIDING_RULES_DIRS = ["/etc/udev/rules.d", "/usr/local/lib/udev/rules.d"] as const;

function rulesMatching(files: ReadonlyMap<string, string>, match: (line: string) => boolean): string[] {
  return [...files]
    .filter(([, text]) => text.split("\n").some((line) => !line.trim().startsWith("#") && match(line)))
    .map(([name]) => name)
    .sort();
}

