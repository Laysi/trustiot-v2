/**
 * Encrypts a plain Secret manifest with sops for the recipients that .sops.yaml names for its
 * target path, and writes only the encrypted file (deploy/flux/README.md, 「祕密」). Shared by
 * ghcr-pull-secret.ts, sops-secret.ts and demo-values.ts.
 */

import { existsSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { basename, dirname, join, relative, resolve } from "node:path";
import { sopsMetadataValid, sopsValueValid } from "../ci/check-secret-handling.ts";
import { protoTool, run } from "../release/exec.ts";
import { ReleaseError, isMap } from "../release/lib.ts";

const ROOT = resolve(import.meta.dir, "..", "..");

/** Encrypts `plain` for the creation rule of `path` (relative to the repository) and returns sops's output. */
export type Encrypt = (plain: string, path: string) => Promise<string>;

/** The pinned sops; --filename-override picks the creation rule (recipients) in .sops.yaml for the target path. */
export const sopsEncrypt: Encrypt = async (plain, path) => {
  const sops = await protoTool("sops");
  const encrypted = await run([sops, "encrypt", "--input-type", "yaml", "--output-type", "yaml", "--filename-override", path], { cwd: ROOT, stdin: plain });
  return encrypted.stdout;
};

/**
 * Why sops's output is not the encrypted form of `plain` (undefined when it is): one document with
 * the same apiVersion, kind, type and metadata, the same keys under the same data block, every value
 * a SOPS-encrypted value and a `sops` block as SOPS writes it for age (the same checks as
 * scripts/ci/check-secret-handling.ts), and nothing else. Never quotes a value.
 */
export function encryptedProblem(plain: string, encrypted: string): string | undefined {
  let parsed: unknown;
  try {
    parsed = Bun.YAML.parse(encrypted);
  } catch {
    return "it is not YAML";
  }
  const source = Bun.YAML.parse(plain) as Record<string, unknown>;
  if (!isMap(parsed)) return "it is not one YAML mapping";
  const block = source.stringData !== undefined ? "stringData" : "data";
  const expectedKeys = [...Object.keys(source).filter((key) => key !== block), block, "sops"].sort();
  if (JSON.stringify(Object.keys(parsed).sort()) !== JSON.stringify(expectedKeys)) {
    return `its top-level keys are ${Object.keys(parsed).sort().join(", ")}, not ${expectedKeys.join(", ")}`;
  }
  for (const key of ["apiVersion", "kind", "type", "metadata"]) {
    if (JSON.stringify(parsed[key]) !== JSON.stringify(source[key])) return `its ${key} differs from the plain Secret's`;
  }
  const values = parsed[block];
  const plainValues = source[block];
  if (!isMap(values) || !isMap(plainValues) || JSON.stringify(Object.keys(values).sort()) !== JSON.stringify(Object.keys(plainValues).sort())) {
    return `its ${block} keys differ from the plain Secret's`;
  }
  const clear = Object.keys(values).filter((key) => !sopsValueValid(values[key]));
  if (clear.length > 0) return `the ${block} values of ${clear.join(", ")} are not SOPS-encrypted`;
  if (!sopsMetadataValid(parsed.sops)) return "its sops block is not what sops writes for age recipients";
  return undefined;
}

export interface WriteOptions {
  /** The repository root `out` is relative to (tests). */
  root?: string;
  encrypt?: Encrypt;
}

/**
 * Encrypts `plain` (YAML, only ever on sops's stdin) and writes it to `out`, a path under the
 * repository. The plain text never touches the disk: sops reads it from stdin, the output is
 * checked to be encrypted (encryptedProblem) before anything is written, and the file is written
 * through a temporary name in the same directory, so a failure leaves no partial file.
 */
export async function writeEncrypted(plain: string, out: string, options: WriteOptions = {}): Promise<string> {
  const root = options.root ?? ROOT;
  const target = resolve(root, out);
  const path = relative(root, target);
  if (path.startsWith("..") || !existsSync(dirname(target))) {
    throw new ReleaseError(`${out} is not in an existing directory of this repository`);
  }
  const encrypted = await (options.encrypt ?? sopsEncrypt)(plain, path);
  const problem = encryptedProblem(plain, encrypted);
  if (problem !== undefined) {
    throw new ReleaseError(`sops's output for ${path} is not an encrypted Secret (${problem}); nothing was written`);
  }
  const temporary = join(dirname(target), `.${basename(target)}.tmp-${process.pid}`);
  try {
    writeFileSync(temporary, encrypted, { mode: 0o644 });
    renameSync(temporary, target);
  } finally {
    rmSync(temporary, { force: true });
  }
  return path;
}

/** Where a value comes from: stdin, which must be a pipe. */
export interface Stdin {
  isTTY?: boolean;
  text(): Promise<string>;
}

/**
 * The value piped into the script. A terminal is refused: what a person types there could echo or
 * land in a scrollback, and values never come from arguments (docs/architecture/secrets.md, rule 3).
 */
export async function readPiped(what: string, stdin: Stdin = { isTTY: process.stdin.isTTY, text: () => Bun.stdin.text() }): Promise<string> {
  if (stdin.isTTY) {
    throw new ReleaseError(`pipe ${what} into stdin; it is never taken from an argument or a prompt that echoes`);
  }
  return stdin.text();
}

/** Whether a kustomization.yaml lists `file` under resources; says what to add when it does not. */
export function kustomizationReminder(dir: string, file: string): string | undefined {
  const kustomization = join(ROOT, dir, "kustomization.yaml");
  const parsed = Bun.YAML.parse(readFileSync(kustomization, "utf8")) as { resources?: unknown };
  const resources = Array.isArray(parsed.resources) ? parsed.resources : [];
  return resources.includes(file) ? undefined : `add "${file}" to resources in ${relative(ROOT, kustomization)} (bun test deploy/flux checks it)`;
}
