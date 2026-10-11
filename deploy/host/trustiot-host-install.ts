// Entry point of the SaaS host installer. Build it into one executable per architecture with
// `bun run --cwd deploy/host build`, copy it to the host and run it as root; the host needs no
// Bun or other runtime. See deploy/host/README.md.
import { stat } from "node:fs/promises";
import { USAGE, UsageError, parseArgs } from "./src/cli.ts";
import { InstallError, install, type Logger } from "./src/install.ts";
import { HostSystem } from "./src/system.ts";
import { TokenError } from "./src/token.ts";

const log: Logger = {
  info: (message) => console.log(`  ${message}`),
  warn: (message) => console.log(`! ${message}`),
  change: (message) => console.log(`+ ${message}`),
};

async function readToken(source: ReturnType<typeof parseArgs>["tokenSource"]): Promise<Uint8Array | null> {
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

async function main(): Promise<number> {
  let args: ReturnType<typeof parseArgs>;
  try {
    args = parseArgs(process.argv.slice(2));
  } catch (error) {
    console.error(`error: ${(error as Error).message}\n\n${USAGE}`);
    return 2;
  }
  if (args.help) {
    console.log(USAGE);
    return 0;
  }
  try {
    const token = await readToken(args.tokenSource);
    const result = await install(new HostSystem(), { ...args.options, token }, log);
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

process.exit(await main());
