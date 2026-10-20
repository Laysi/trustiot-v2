import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { renderOpaqueSecret, renderPullSecret } from "./lib.ts";
import { SOPS_SCOPE_LINE, fakeSops } from "./sops-fixture.ts";
import { type Encrypt, encryptedProblem, readPiped, writeEncrypted } from "./sops.ts";

/** Where the demo cluster's SMTP Secret goes, a Secret SOPS_SECRETS lists for it. */
const DIR = "deploy/flux/clusters/demo/trustiot";
const SMTP = `${DIR}/trustiot-smtp.sops.yaml`;

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
  mkdirSync(join(root, DIR), { recursive: true });
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
      // sops encrypts what the file's own sops block says: a wider scope or a scope option hides more.
      [encrypted.replace(SOPS_SCOPE_LINE, "  encrypted_regex: ^(data|stringData|annotations)$\n"), /sops\.encrypted_regex must be exactly/],
      [encrypted.replace(SOPS_SCOPE_LINE, `${SOPS_SCOPE_LINE}  unencrypted_suffix: _unencrypted\n`), /sops\.unencrypted_suffix is set/],
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
    expect(await writeEncrypted(plainSecret, SMTP, { root: dir, encrypt })).toBe(SMTP);
    expect(calls).toEqual([SMTP]);
    expect(readFileSync(join(dir, SMTP), "utf8")).toBe(fakeSops(plainSecret));
    expect(readdirSync(join(dir, DIR))).toEqual(["trustiot-smtp.sops.yaml"]);
  });

  test("refuses output that is not encrypted, writes nothing, and keeps a file that was there", async () => {
    const dir = repository();
    const plainOut: Encrypt = async (plain) => plain;
    const error = writeEncrypted(plainSecret, SMTP, { root: dir, encrypt: plainOut });
    await expect(error).rejects.toThrow(/is not an encrypted Secret .*; nothing was written/);
    await error.catch((e: unknown) => expect(String(e)).not.toContain(secretValue));
    expect(readdirSync(join(dir, DIR))).toEqual([]);

    await writeEncrypted(plainSecret, SMTP, { root: dir, encrypt: async (plain) => fakeSops(plain) });
    const before = readFileSync(join(dir, SMTP), "utf8");
    const halfEncrypted: Encrypt = async (plain) => fakeSops(plain).replace(/^( {2}"password"): .*$/m, `$1: ${secretValue}`);
    await expect(writeEncrypted(plainSecret, SMTP, { root: dir, encrypt: halfEncrypted })).rejects.toThrow(/not SOPS-encrypted/);
    expect(readFileSync(join(dir, SMTP), "utf8")).toBe(before);
    expect(readdirSync(join(dir, DIR))).toEqual(["trustiot-smtp.sops.yaml"]);
  });

  test("refuses a Secret the cluster may not hold, of another type or name, or in another place, before running sops", async () => {
    const dir = repository();
    mkdirSync(join(dir, "deploy/flux/clusters/saas/trustiot"), { recursive: true });
    let ran = false;
    const encrypt: Encrypt = async (plain) => {
      ran = true;
      return fakeSops(plain);
    };
    const opaque = (name: string, namespace = "trustiot") => renderOpaqueSecret({ namespace, name, key: "password", value: secretValue, format: "token" });
    const cases: [string, string, RegExp][] = [
      // The chart generates this one in the cluster; from git, Helm would adopt it and keep its data.
      [opaque("trustiot-identity-signing-keys"), `${DIR}/trustiot-identity-signing-keys.sops.yaml`, /cluster demo holds no SOPS Secret trustiot-identity-signing-keys/],
      [opaque("trustiot-smtp"), "deploy/flux/clusters/saas/trustiot/trustiot-smtp.sops.yaml", /cluster saas holds no SOPS Secret trustiot-smtp/],
      [opaque("ghcr-pull"), `${DIR}/ghcr-pull.sops.yaml`, /ghcr-pull of cluster demo has type kubernetes\.io\/dockerconfigjson, not Opaque/],
      [opaque("trustiot-smtp", "default"), SMTP, /must be in namespace trustiot/],
      [opaque("trustiot-smtp"), `${DIR}/smtp.sops.yaml`, /written only to deploy\/flux\/clusters\/<cluster>\/trustiot\/<its name>\.sops\.yaml/],
    ];
    for (const [plain, path, message] of cases) {
      await expect(writeEncrypted(plain, path, { root: dir, encrypt })).rejects.toThrow(message);
    }
    expect(ran).toBe(false);
    expect(readdirSync(join(dir, DIR))).toEqual([]);
    // The pull Secret, of its own type, is written.
    expect(await writeEncrypted(renderPullSecret("trustiot", "ghcr.io", "trustiot-bot", secretValue), `${DIR}/ghcr-pull.sops.yaml`, { root: dir, encrypt })).toBe(`${DIR}/ghcr-pull.sops.yaml`);
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
