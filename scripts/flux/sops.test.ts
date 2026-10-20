import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { renderOpaqueSecret, renderPullSecret } from "./lib.ts";
import { type Encrypt, encryptedProblem, readPiped, writeEncrypted } from "./sops.ts";

// Bytes that do not read as text, standing in for ciphertext (deterministic), in the shape SOPS
// writes: a 32-byte IV, a 16-byte tag, an age recipient and its wrapped data key. The same shape
// as the fixture in scripts/ci/check-secret-handling.test.ts.
const opaque = (length: number, seed: number) => Buffer.from(Array.from({ length }, (_, i) => ((seed + i * 151) % 256) | 0x80)).toString("base64");
const enc = (seed: number) => `ENC[AES256_GCM,data:${opaque(12, seed)},iv:${opaque(32, 1)},tag:${opaque(16, 2)},type:str]`;
const unpadded = (length: number, seed: number) => opaque(length, seed).replace(/=+$/, "");
const ageFile = () => {
  const header = Buffer.from(`age-encryption.org/v1\n-> X25519 ${unpadded(32, 3)}\n${unpadded(32, 4)}\n--- ${unpadded(32, 5)}\n`);
  const lines = Buffer.concat([header, Buffer.from(opaque(48, 6), "base64")]).toString("base64").match(/.{1,64}/g) ?? [];
  return ["-----BEGIN AGE ENCRYPTED FILE-----", ...lines, "-----END AGE ENCRYPTED FILE-----"].map((line) => `        ${line}`).join("\n");
};
const SOPS_BLOCK = [
  "sops:",
  "  age:",
  `    - recipient: age1${"qpzry9x8gf2tvdw0s3jn54khce6mua7l".repeat(2).slice(0, 58)}`,
  "      enc: |",
  ageFile(),
  '  lastmodified: "2026-10-20T00:00:00Z"',
  `  mac: ${enc(9)}`,
  "  encrypted_regex: ^(data|stringData)$",
  '  version: "3.13.3"',
  "",
].join("\n");

/** What sops writes for `plain`: the same document, every data or stringData value encrypted, and the sops block. */
function fakeSops(plain: string): string {
  let seed = 10;
  const lines = plain.split("\n").map((line) => {
    const value = /^( {2}"?[^"\s:]+"?): .+$/.exec(line);
    return value !== null && !line.startsWith("  name:") && !line.startsWith("  namespace:") && !line.startsWith("  labels:") ? `${value[1]}: ${enc(seed++)}` : line;
  });
  return `${lines.join("\n").replace(/\n$/, "")}\n${SOPS_BLOCK}`;
}

// Made up at run time, so a test can tell whether an error or a file holds it.
const secretValue = `s${crypto.randomUUID().replaceAll("-", "")}`;
const plainSecret = renderOpaqueSecret({ namespace: "trustiot", name: "trustiot-smtp", key: "password", value: secretValue, format: "token", plain: { username: "resend" } });

let root = "";
afterEach(() => {
  if (root !== "") rmSync(root, { recursive: true, force: true });
  root = "";
});

function repository(): string {
  root = mkdtempSync(join(tmpdir(), "trustiot-sops-"));
  mkdirSync(join(root, "deploy", "flux"), { recursive: true });
  return root;
}

describe("encryptedProblem", () => {
  test("accepts what sops writes for an Opaque Secret and for the pull Secret", () => {
    expect(encryptedProblem(plainSecret, fakeSops(plainSecret))).toBeUndefined();
    const pull = renderPullSecret("trustiot", "ghcr.io", "trustiot-bot", secretValue);
    expect(encryptedProblem(pull, fakeSops(pull))).toBeUndefined();
  });

  test("refuses plain text, a value left in clear, a missing or forged sops block, and another Secret, without quoting values", () => {
    const encrypted = fakeSops(plainSecret);
    const cases: [string, RegExp][] = [
      [plainSecret, /top-level keys/],
      [encrypted.replace(/^( {2}"password"): .*$/m, `$1: ${secretValue}`), /values of password are not SOPS-encrypted/],
      [encrypted.replace(/^ {2}mac: .*$/m, `  mac: ENC[AES256_GCM,data:${secretValue}]`), /sops block/],
      [encrypted.replace(/\nsops:[\s\S]*$/, "\n"), /top-level keys/],
      [encrypted.replace("name: trustiot-smtp", "name: other"), /metadata differs/],
      [encrypted.replace(/^( {2}"username"): .*$/m, ""), /stringData keys differ/],
      [`${encrypted}---\n${encrypted}`, /not one YAML mapping/],
      ["{", /not YAML/],
    ];
    for (const [output, message] of cases) {
      const problem = encryptedProblem(plainSecret, output);
      expect(problem).toMatch(message);
      expect(problem).not.toContain(secretValue);
    }
  });
});

describe("writeEncrypted", () => {
  test("writes sops's output when it is encrypted, and nothing else in the directory", async () => {
    const dir = repository();
    const calls: string[] = [];
    const encrypt: Encrypt = async (plain, path) => {
      calls.push(path);
      return fakeSops(plain);
    };
    expect(await writeEncrypted(plainSecret, "deploy/flux/trustiot-smtp.sops.yaml", { root: dir, encrypt })).toBe("deploy/flux/trustiot-smtp.sops.yaml");
    expect(calls).toEqual(["deploy/flux/trustiot-smtp.sops.yaml"]);
    expect(readFileSync(join(dir, "deploy/flux/trustiot-smtp.sops.yaml"), "utf8")).toBe(fakeSops(plainSecret));
    expect(readdirSync(join(dir, "deploy/flux"))).toEqual(["trustiot-smtp.sops.yaml"]);
  });

  test("refuses output that is not encrypted, writes nothing, and keeps a file that was there", async () => {
    const dir = repository();
    const plainOut: Encrypt = async (plain) => plain;
    const error = writeEncrypted(plainSecret, "deploy/flux/trustiot-smtp.sops.yaml", { root: dir, encrypt: plainOut });
    await expect(error).rejects.toThrow(/is not an encrypted Secret .*; nothing was written/);
    await error.catch((e: unknown) => expect(String(e)).not.toContain(secretValue));
    expect(readdirSync(join(dir, "deploy/flux"))).toEqual([]);

    await writeEncrypted(plainSecret, "deploy/flux/trustiot-smtp.sops.yaml", { root: dir, encrypt: async (plain) => fakeSops(plain) });
    const before = readFileSync(join(dir, "deploy/flux/trustiot-smtp.sops.yaml"), "utf8");
    const halfEncrypted: Encrypt = async (plain) => fakeSops(plain).replace(/^( {2}"password"): .*$/m, `$1: ${secretValue}`);
    await expect(writeEncrypted(plainSecret, "deploy/flux/trustiot-smtp.sops.yaml", { root: dir, encrypt: halfEncrypted })).rejects.toThrow(/not SOPS-encrypted/);
    expect(readFileSync(join(dir, "deploy/flux/trustiot-smtp.sops.yaml"), "utf8")).toBe(before);
    expect(readdirSync(join(dir, "deploy/flux"))).toEqual(["trustiot-smtp.sops.yaml"]);
  });

  test("refuses a path outside the repository or in a directory that does not exist, before running sops", async () => {
    const dir = repository();
    let ran = false;
    const encrypt: Encrypt = async (plain) => {
      ran = true;
      return fakeSops(plain);
    };
    await expect(writeEncrypted(plainSecret, "../outside.sops.yaml", { root: dir, encrypt })).rejects.toThrow(/not in an existing directory/);
    await expect(writeEncrypted(plainSecret, "deploy/missing/x.sops.yaml", { root: dir, encrypt })).rejects.toThrow(/not in an existing directory/);
    expect(ran).toBe(false);
    expect(existsSync(join(dir, "..", "outside.sops.yaml"))).toBe(false);
  });
});

describe("readPiped", () => {
  test("reads a pipe and refuses a terminal", async () => {
    expect(await readPiped("the value", { isTTY: false, text: async () => "x\n" })).toBe("x\n");
    let read = false;
    const terminal = { isTTY: true, text: async () => ((read = true), "typed") };
    await expect(readPiped("the token", terminal)).rejects.toThrow(/pipe the token into stdin/);
    expect(read).toBe(false);
  });
});
