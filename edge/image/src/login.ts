// The gateway's local login surface (Laysi/trustiot-v2#815): no login prompt on the screen or the
// serial console, no shell when the initramfs fails, and no USB device authorized by the kernel.
// Files the build writes and the parsers the check reads the image back with; pure functions only,
// so the unit tests cover them without an image. README.md, section 「登入面」.

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
 * console. USB keyboards stay unauthorized (USBCORE_ARGUMENT), but the serial console still reaches
 * that shell, and the gateway is unattended, so rebooting is the useful outcome anyway.
 */
export const PANIC_ARGUMENT = "panic=10";

/**
 * On cmdline.txt: usbcore, built into the kernel, leaves every USB device except the root hubs
 * unauthorized from the moment it appears, initramfs included: no driver binds to it and no
 * interface, input device or disk appears. That covers keyboards and storage and also hubs, network
 * adapters, modems and USB serial adapters; the built-in Ethernet of the boards the image targets is
 * not on USB.
 */
export const USBCORE_ARGUMENT = "usbcore.authorized_default=0";

/** The arguments the build appends to cmdline.txt, in this order. */
export const BOOT_ARGUMENTS = [PANIC_ARGUMENT, USBCORE_ARGUMENT] as const;

/**
 * cmdline.txt with BOOT_ARGUMENTS appended after the last argument; still one line, as the firmware
 * requires, and ending in a newline only if the source did. A source that already sets one of them
 * is refused: the kernel would then see two values.
 */
export function cmdlineWithArguments(text: string): string {
  const line = text.replace(/\r?\n$/, "");
  if (/[\r\n]/.test(line)) throw new Error("cmdline.txt has more than one line");
  const tokens = line.split(/\s+/).filter((t) => t !== "");
  if (tokens.length === 0) throw new Error("cmdline.txt is empty");
  // The kernel stops reading its own parameters at "--" and hands the rest to init.
  if (tokens.includes("--")) throw new Error("cmdline.txt has --: arguments after it would not reach the kernel");
  for (const argument of BOOT_ARGUMENTS) {
    const key = parameterName(argument);
    if (tokens.some((t) => parameterName(t) === key)) throw new Error(`cmdline.txt already sets ${key}=`);
  }
  return `${line.trimEnd()} ${BOOT_ARGUMENTS.join(" ")}${text.endsWith("\n") ? "\n" : ""}`;
}

/** A kernel parameter's name: the part before "=", with "-" read as "_" as the kernel does. */
export function parameterName(token: string): string {
  return (token.includes("=") ? token.slice(0, token.indexOf("=")) : token).replaceAll("-", "_");
}

/**
 * Problems with cmdline.txt as the image check reads it back: one line (the firmware reads only the
 * first), no "--" (the kernel would not read what follows it), every BOOT_ARGUMENTS parameter
 * exactly once with the expected value, and no USB gadget module loaded from it (a gadget would
 * make the gateway itself a USB device, which usbcore.authorized_default does not cover).
 */
export function cmdlineProblems(text: string): string[] {
  const problems: string[] = [];
  const line = text.replace(/\r?\n$/, "");
  if (/[\r\n]/.test(line)) problems.push("cmdline.txt has more than one line; the firmware reads only the first");
  const tokens = line.split(/\s+/).filter((t) => t !== "");
  if (tokens.includes("--")) problems.push("cmdline.txt has --: the kernel does not read the parameters after it");
  for (const argument of BOOT_ARGUMENTS) {
    const key = parameterName(argument);
    const found = tokens.filter((t) => parameterName(t) === key);
    if (found.length !== 1 || found[0] !== argument) problems.push(`cmdline.txt has ${found.join(" ") || `no ${key}=`}, expected ${argument} once`);
  }
  for (const token of tokens) {
    if (parameterName(token) === "modules_load" && /(^|[=,])(g_|dwc2|libcomposite|usb_f_)/.test(token)) problems.push(`cmdline.txt loads a USB gadget driver: ${token}`);
  }
  return problems;
}

/**
 * Lines of config.txt that would make the firmware read the kernel command line from somewhere else
 * than the boot partition's cmdline.txt (cmdline=, os_prefix=) or that pull in another file the
 * check would have to follow (include). Any section counts.
 */
export function configCmdlineRedirects(config: string): string[] {
  return config
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => /^(cmdline|os_prefix)\s*=|^include\s/.test(line));
}

/**
 * Lines of config.txt that put the dwc2 controller in device (peripheral) or OTG mode, in any
 * section: the gateway could then act as a USB gadget for whatever it is plugged into, outside what
 * usbcore.authorized_default covers. Host mode (dr_mode=host, as the source image sets for CM5) is
 * fine.
 */
export function configUsbGadget(config: string): string[] {
  return config
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => /^dtoverlay\s*=\s*dwc2(,|\s*$)/.test(line) && !/(^|,)dr_mode=host(,|$)/.test(line.replace(/^dtoverlay\s*=\s*/, "")));
}

/**
 * Problems with a kernel build configuration (/boot/config-<version>): a built-in command line that
 * replaces the firmware's (CONFIG_CMDLINE_FORCE) or one that names usbcore.
 */
export function kernelConfigProblems(config: string): string[] {
  const problems: string[] = [];
  if (/^CONFIG_CMDLINE_FORCE=y$/m.test(config)) problems.push("CONFIG_CMDLINE_FORCE=y: the kernel ignores cmdline.txt");
  const builtIn = /^CONFIG_CMDLINE="(.*)"$/m.exec(config)?.[1] ?? "";
  if (/usbcore/.test(builtIn)) problems.push(`the built-in command line names usbcore: ${builtIn}`);
  return problems;
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

// ---- USB: nothing re-authorizes a device ---------------------------------------------------------

/** Where udev(7) reads rules files from; /run is empty in the image. */
export const UDEV_RULES_DIRS = ["/etc/udev/rules.d", "/run/udev/rules.d", "/usr/local/lib/udev/rules.d", "/usr/lib/udev/rules.d"] as const;

/** Where systemd-tmpfiles reads its configuration from (tmpfiles.d(5)). */
export const TMPFILES_DIRS = ["/etc/tmpfiles.d", "/run/tmpfiles.d", "/usr/local/lib/tmpfiles.d", "/usr/lib/tmpfiles.d"] as const;

/** sysfsutils' configuration, which writes sysfs attributes at boot when the package is installed. */
export const SYSFS_CONFIG = ["/etc/sysfs.conf", "/etc/sysfs.d"] as const;

/** Packages and units that authorize USB devices by policy; not installed, or masked if they are. */
export const USB_AUTHORIZING_PACKAGES = ["usbguard"] as const;
export const USB_AUTHORIZING_UNITS = ["usbguard.service", "usbguard-dbus.service"] as const;

/**
 * A file naming a USB device's or interface's authorization attribute as sysfs or udev write it
 * (/authorized, {authorized}, interface_authorized, authorized_default); "authorized_keys" and the
 * plain word do not count.
 */
export const AUTHORIZED_ATTRIBUTE = /[/{]authorized(_default)?\b|\binterface_authorized(_default)?\b|\bauthorized_default\b/;

/**
 * The image check's broad scan: text files up to BROAD_SCAN_MAX_BYTES under these trees must not
 * name the attribute (a unit's ExecStart=, a helper script, a hook). Firmware and kernel modules
 * are binary and are skipped for speed.
 */
export const BROAD_SCAN = /^\/(etc|usr\/local|usr\/bin|usr\/sbin|usr\/libexec|usr\/lib)\//;
export const BROAD_SCAN_SKIP = /^\/usr\/lib\/(firmware|modules)\//;
export const BROAD_SCAN_MAX_BYTES = 1024 * 1024;

function contentLines(text: string): string[] {
  return text.split("\n").filter((line) => line.trim() !== "" && !line.trim().startsWith("#"));
}

/**
 * Rules files that write a device's or interface's `authorized` or `authorized_default` attribute,
 * by file name: with ATTR{} (also through a relative path such as ATTR{../authorized}), or through a
 * program (RUN, PROGRAM, IMPORT) whose arguments name it.
 */
export function rulesWritingAuthorized(files: ReadonlyMap<string, string>): string[] {
  return [...files]
    .filter(([, text]) =>
      contentLines(text).some(
        (line) => /ATTRS?\{[^}]*authorized[^}]*\}\s*[+:]?=(?!=)/.test(line) || (/\b(RUN|PROGRAM|IMPORT)\b/.test(line) && /authorized/.test(line)),
      ),
    )
    .map(([name]) => name)
    .sort();
}

/** Interpreters whose first absolute-path argument is the script that actually runs. */
const INTERPRETERS = new Set(["sh", "bash", "dash", "env", "perl", "python3"]);

/** /bin, /sbin and /lib are links into /usr on the image (merged /usr). */
function usrPath(path: string): string {
  return path.replace(/^\/(bin|sbin|lib)\//, "/usr/$1/");
}

/**
 * The programs rules run (RUN, PROGRAM, IMPORT{program}), as absolute paths under /usr where the
 * image has merged /usr: udev looks a relative name up in /usr/lib/udev. For an interpreter, its
 * first absolute-path argument is listed too. Builtins (RUN{builtin}, IMPORT{builtin}) and IMPORT
 * of files, the database or the command line run no program and are left out.
 */
export function rulePrograms(text: string): string[] {
  const programs = new Set<string>();
  for (const line of contentLines(text)) {
    for (const m of line.matchAll(/\b(RUN|PROGRAM|IMPORT)(\{([^}]*)\})?\s*\+?:?=\s*"([^"]*)"/g)) {
      const [, key, , option, value] = m;
      if (key === "IMPORT" && option !== "program") continue;
      if (key === "RUN" && option !== undefined && option !== "program") continue;
      const words = value?.trim().split(/\s+/) ?? [];
      const word = words[0];
      if (!word || word.includes("$") || word.includes("%")) continue;
      programs.add(usrPath(word.startsWith("/") ? word : `/usr/lib/udev/${word}`));
      if (INTERPRETERS.has(word.slice(word.lastIndexOf("/") + 1))) {
        // The script is the first argument that is not an option or a variable (sh -c runs an inline command instead).
        const script = words.slice(1).find((w) => !w.startsWith("-") && !/^[A-Za-z_][A-Za-z0-9_]*=/.test(w));
        if (!words.includes("-c") && script?.startsWith("/") && !script.includes("$") && !script.includes("%")) programs.add(usrPath(script));
      }
    }
  }
  return [...programs].sort();
}

/** tmpfiles.d lines whose path names an `authorized` attribute (AUTHORIZED_ATTRIBUTE: not authorized_keys), whatever the line type. */
export function tmpfilesWritingAuthorized(text: string): string[] {
  return contentLines(text).filter((line) => AUTHORIZED_ATTRIBUTE.test(line.trim().split(/\s+/)[1] ?? ""));
}

/** sysfsutils lines (attribute = value) that name an `authorized` attribute. */
export function sysfsWritingAuthorized(text: string): string[] {
  return contentLines(text).filter((line) => AUTHORIZED_ATTRIBUTE.test(`/${line.trim()}`));
}
