/**
 * Runs age (https://age-encryption.org), the encryption tool behind the SOPS files in
 * deploy/flux/ (deploy/flux/README.md):
 *
 *   bun scripts/flux/age.ts age-keygen -o <file>     # a new key pair; prints the public key
 *   bun scripts/flux/age.ts age-keygen -y <file>     # the public key of an existing identity
 *   bun scripts/flux/age.ts age [args...]            # encrypt or decrypt, e.g. offline backups
 *
 * age publishes no checksum file with its release binaries (only Sigsum proofs, which proto
 * cannot verify), so a proto schema plugin could not verify the download the way every other
 * tool in .prototools is verified. Instead this script builds the official source with the Go
 * toolchain pinned in .prototools: `go install filippo.io/age/cmd/...@<AGE_VERSION>` fetches the
 * module through the Go module proxy and checks it against the Go checksum database, a public
 * transparency log, and fails on any mismatch. The binaries are cached per version under
 * $XDG_CACHE_HOME/trustiot-v2/age/.
 */

import { existsSync, mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { protoTool, run } from "../release/exec.ts";
import { ReleaseError, errorMessage } from "../release/lib.ts";

/** The age release that the scripts and the runbooks use. Update by hand (no Renovate rule). */
export const AGE_VERSION = "v1.3.2";
export const AGE_COMMANDS = ["age", "age-keygen"] as const;
type AgeCommand = (typeof AGE_COMMANDS)[number];

/**
 * The Go environment of the build: the public proxy and checksum database only, and nothing
 * from the caller's environment that could turn the verification off or redirect the download.
 */
export function goEnvironment(bin: string): Record<string, string | undefined> {
  return {
    GOBIN: bin,
    GOFLAGS: "-mod=readonly -trimpath",
    GOPROXY: "https://proxy.golang.org",
    GOSUMDB: "sum.golang.org",
    GONOSUMDB: undefined,
    GONOSUMCHECK: undefined,
    GONOPROXY: undefined,
    GOPRIVATE: undefined,
    GOINSECURE: undefined,
    GOTOOLCHAIN: "local",
    CGO_ENABLED: "0",
  };
}

function cacheDir(): string {
  const base = process.env.XDG_CACHE_HOME || join(homedir(), ".cache");
  return join(base, "trustiot-v2", "age", AGE_VERSION);
}

/** The path of an age command, built and verified on first use. */
export async function ageTool(command: AgeCommand): Promise<string> {
  const bin = cacheDir();
  const path = join(bin, command);
  if (!AGE_COMMANDS.every((name) => existsSync(join(bin, name)))) {
    mkdirSync(bin, { recursive: true, mode: 0o700 });
    const go = await protoTool("go");
    console.error(`building age ${AGE_VERSION} from source (verified against the Go checksum database)`);
    await run([go, "install", `filippo.io/age/cmd/...@${AGE_VERSION}`], { env: goEnvironment(bin) });
  }
  if (!existsSync(path)) {
    throw new ReleaseError(`go install did not produce ${path}`);
  }
  return path;
}

if (import.meta.main) {
  const [command, ...args] = Bun.argv.slice(2);
  if (!AGE_COMMANDS.includes(command as AgeCommand)) {
    console.error(`usage: bun scripts/flux/age.ts <${AGE_COMMANDS.join("|")}> [args...]`);
    process.exit(2);
  }
  try {
    const tool = await ageTool(command as AgeCommand);
    const proc = Bun.spawn([tool, ...args], { stdin: "inherit", stdout: "inherit", stderr: "inherit" });
    process.exit(await proc.exited);
  } catch (error) {
    console.error(errorMessage(error));
    process.exit(1);
  }
}
