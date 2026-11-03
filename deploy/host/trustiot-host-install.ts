// Entry point of the SaaS host installer. Build it into one executable per architecture with
// `bun run --cwd deploy/host build`, copy it to the host and run it as root; the host needs no
// Bun or other runtime. See deploy/host/README.md.
import { consoleLog, main } from "./src/main.ts";
import { HostSystem } from "./src/system.ts";

process.exit(await main(process.argv.slice(2), new HostSystem(), consoleLog));
