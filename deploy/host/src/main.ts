// What the trustiot-host-install executable does, apart from choosing the real host: the entry
// point (trustiot-host-install.ts) calls main() with HostSystem, the tests with a simulated host.
import { stat } from "node:fs/promises";
import { USAGE, UsageError, parseArgs } from "./cli.ts";
import { InstallError, install, type Logger } from "./install.ts";
import type { System } from "./system.ts";
import { TokenError } from "./token.ts";

/**
 * The umask every system tool runs under. sudo passes a stricter umask of the caller on to the
 * installer (it combines the caller's umask with its own default of 022), and dnf, rpm, systemctl,
 * firewall-cmd and the k3s installer create system files with it: under 077, dnf leaves its
 * system state and repository caches readable by root only, and a plain user's dnf query then
 * finds nothing (Laysi/trustiot-v2#476). The installer's own files and directories do not depend
 * on it: System.writeFile, makeDirectory and makeTempDirectory set their modes explicitly.
 */
export const SYSTEM_UMASK = 0o022;

export const consoleLog: Logger = {
  info: (message) => console.log(`  ${message}`),
  warn: (message) => console.log(`! ${message}`),
  change: (message) => console.log(`+ ${message}`),
};

async function readToken(source: ReturnType<typeof parseArgs>["tokenSource"], log: Logger): Promise<Uint8Array | null> {
  switch (source.kind) {
    case "none":
      return null;
    case "stdin":
      if (process.stdin.isTTY) throw new UsageError("--token-stdin needs the token piped in, not typed on a terminal");
      return new Uint8Array(await Bun.stdin.arrayBuffer());
    case "file": {
      const info = await stat(source.path);
      if ((info.mode & 0o077) !== 0) {
        log.warn(`${source.path} is readable by other users; delete it once the installer has stored the token`);
      }
      return new Uint8Array(await Bun.file(source.path).arrayBuffer());
    }
  }
}

export async function main(
  argv: string[],
  sys: System,
  log: Logger,
  // Tests replace the pinned k3s installer digest; nothing else does.
  pins: { k3sInstallerSha256?: string } = {},
): Promise<number> {
  // First, before the installer creates a file or starts a process.
  const callerUmask = process.umask(SYSTEM_UMASK);
  let args: ReturnType<typeof parseArgs>;
  try {
    args = parseArgs(argv);
  } catch (error) {
    console.error(`error: ${(error as Error).message}\n\n${USAGE}`);
    return 2;
  }
  if (args.help) {
    console.log(USAGE);
    return 0;
  }
  if (callerUmask !== SYSTEM_UMASK) {
    log.info(`umask 0${callerUmask.toString(8).padStart(3, "0")} replaced by 0${SYSTEM_UMASK.toString(8).padStart(3, "0")} for the system tools; the installer sets its own files' modes explicitly`);
  }
  try {
    const token = await readToken(args.tokenSource, log);
    const result = await install(sys, { ...args.options, token }, log, pins);
    const verb = args.options.dryRun ? "planned" : "made";
    console.log(result.changes.length === 0 ? "Done: the host already matched; nothing changed." : `Done: ${result.changes.length} change(s) ${verb}.`);
    return 0;
  } catch (error) {
    // Known failures carry an explanation for the operator; anything else gets its stack.
    const known = error instanceof InstallError || error instanceof TokenError || error instanceof UsageError;
    console.error(`error: ${known ? (error as Error).message : String((error as Error).stack ?? error)}`);
    return 1;
  }
}
