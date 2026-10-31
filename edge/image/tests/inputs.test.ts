// The administrator's public keys and the agent executable the build is given. Keys are made at
// run time; no key, public or private, is kept in the repository.
import { describe, expect, test } from "bun:test";
import { generateKeyPairSync } from "node:crypto";
import { MAX_KEYS, aarch64ElfProblem, authorizedKeys, keyBlobProblem, parsePublicKeys } from "../src/inputs.ts";

function sshString(value: Uint8Array | string): Buffer {
  const bytes = typeof value === "string" ? Buffer.from(value, "latin1") : Buffer.from(value);
  const length = Buffer.alloc(4);
  length.writeUInt32BE(bytes.length);
  return Buffer.concat([length, bytes]);
}

/** A fresh Ed25519 key in OpenSSH public key format, and its private key as PEM. */
function ed25519(): { line: string; privatePem: string } {
  const { publicKey, privateKey } = generateKeyPairSync("ed25519");
  const raw = publicKey.export({ type: "spki", format: "der" }).subarray(-32);
  const blob = Buffer.concat([sshString("ssh-ed25519"), sshString(raw)]).toString("base64");
  return { line: `ssh-ed25519 ${blob}`, privatePem: privateKey.export({ type: "pkcs8", format: "pem" }).toString() };
}

function ed25519Raw(): Buffer {
  return generateKeyPairSync("ed25519").publicKey.export({ type: "spki", format: "der" }).subarray(-32);
}

/** The uncompressed public point (0x04 || X || Y) of a fresh key on a NIST curve. */
function ecdsaPoint(namedCurve: "P-256" | "P-384" | "P-521"): Buffer {
  const jwk = generateKeyPairSync("ec", { namedCurve }).publicKey.export({ format: "jwk" });
  return Buffer.concat([Buffer.from([4]), Buffer.from(jwk.x ?? "", "base64url"), Buffer.from(jwk.y ?? "", "base64url")]);
}

function ecdsa(curve: string, namedCurve: "P-256" | "P-384" | "P-521"): string {
  const type = `ecdsa-sha2-${curve}`;
  return `${type} ${Buffer.concat([sshString(type), sshString(curve), sshString(ecdsaPoint(namedCurve))]).toString("base64")}`;
}

describe("public keys", () => {
  test("keeps type and key, drops the comment", () => {
    const { line } = ed25519();
    expect(parsePublicKeys(`${line} someone@workstation\n`)).toEqual([line]);
    expect(authorizedKeys([line])).toBe(`${line}\n`);
  });

  test("skips blank and comment lines and duplicates", () => {
    const a = ed25519().line;
    const b = ed25519().line;
    expect(parsePublicKeys(`# admins\n\n${a} one\n${b}\n${a} again\n`)).toEqual([a, b]);
  });

  test("refuses a private key", () => {
    const { privatePem } = ed25519();
    expect(() => parsePublicKeys(privatePem)).toThrow("private key");
    expect(() => parsePublicKeys("-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n")).toThrow("private key");
  });

  test("refuses options, RSA and unknown types", () => {
    const { line } = ed25519();
    expect(() => parsePublicKeys(`command="/bin/sh" ${line}`)).toThrow("not accepted");
    const rsa = `ssh-rsa ${Buffer.concat([sshString("ssh-rsa"), sshString("x")]).toString("base64")}`;
    expect(() => parsePublicKeys(rsa)).toThrow("not accepted");
  });

  test("refuses a key whose data does not match its type", () => {
    const blob = Buffer.concat([sshString("ecdsa-sha2-nistp256"), sshString("x")]).toString("base64");
    expect(() => parsePublicKeys(`ssh-ed25519 ${blob}`)).toThrow("not a ssh-ed25519 key");
    expect(() => parsePublicKeys("ssh-ed25519 not*base64")).toThrow("not base64");
  });

  test("accepts complete ECDSA and security keys", () => {
    const keys = [ecdsa("nistp256", "P-256"), ecdsa("nistp384", "P-384"), ecdsa("nistp521", "P-521")];
    const ed = ed25519Raw();
    const p256 = ecdsaPoint("P-256");
    const skEd = `sk-ssh-ed25519@openssh.com ${Buffer.concat([sshString("sk-ssh-ed25519@openssh.com"), sshString(ed), sshString("ssh:")]).toString("base64")}`;
    const skEc = `sk-ecdsa-sha2-nistp256@openssh.com ${Buffer.concat([sshString("sk-ecdsa-sha2-nistp256@openssh.com"), sshString("nistp256"), sshString(p256), sshString("ssh:")]).toString("base64")}`;
    expect(parsePublicKeys([...keys, skEd, skEc].join("\n"))).toEqual([...keys, skEd, skEc]);
  });

  test("refuses a truncated key, extra bytes and wrong fields", () => {
    const blob = Buffer.from(ed25519().line.split(" ")[1] ?? "", "base64");
    const encode = (bytes: Uint8Array) => Buffer.from(bytes).toString("base64");
    // Every cut of a valid key is refused (a cut at a 4-byte boundary still decodes as base64).
    for (let length = 1; length < blob.length; length++) {
      expect(() => parsePublicKeys(`ssh-ed25519 ${encode(blob.subarray(0, length))}`)).toThrow();
    }
    expect(keyBlobProblem("ssh-ed25519", Buffer.concat([blob, Buffer.from([0, 0, 0, 0])]))).toContain("fields");
    expect(keyBlobProblem("ssh-ed25519", Buffer.concat([blob, Buffer.from([1])]))).toContain("truncated");
    const shortEd = Buffer.concat([sshString("ssh-ed25519"), sshString(new Uint8Array(31))]);
    expect(keyBlobProblem("ssh-ed25519", shortEd)).toContain("expected 32");
    const p256 = ecdsaPoint("P-256");
    const wrongCurve = Buffer.concat([sshString("ecdsa-sha2-nistp256"), sshString("nistp384"), sshString(p256)]);
    expect(keyBlobProblem("ecdsa-sha2-nistp256", wrongCurve)).toContain("expected nistp256");
    const compressed = Buffer.concat([sshString("ecdsa-sha2-nistp256"), sshString("nistp256"), sshString(Buffer.concat([Buffer.from([2]), p256.subarray(1, 33)]))]);
    expect(keyBlobProblem("ecdsa-sha2-nistp256", compressed)).toContain("uncompressed point");
    const noApplication = Buffer.concat([sshString("sk-ssh-ed25519@openssh.com"), sshString(ed25519Raw())]);
    expect(keyBlobProblem("sk-ssh-ed25519@openssh.com", noApplication)).toContain("fields");
    const emptyApplication = Buffer.concat([sshString("sk-ssh-ed25519@openssh.com"), sshString(ed25519Raw()), sshString("")]);
    expect(keyBlobProblem("sk-ssh-ed25519@openssh.com", emptyApplication)).toContain("empty");
  });

  test("refuses an empty file and too many keys", () => {
    expect(() => parsePublicKeys("\n# nothing\n")).toThrow("no public key");
    const many = Array.from({ length: MAX_KEYS + 1 }, () => ed25519().line).join("\n");
    expect(() => parsePublicKeys(many)).toThrow(`at most ${MAX_KEYS}`);
  });
});

describe("agent executable", () => {
  function elf(machine: number, type = 3, klass = 2, data = 1): Uint8Array {
    const head = new Uint8Array(64);
    head.set([0x7f, 0x45, 0x4c, 0x46, klass, data, 1]);
    head[16] = type;
    head[18] = machine & 0xff;
    head[19] = machine >> 8;
    return head;
  }

  test("accepts a 64-bit little-endian AArch64 executable", () => {
    expect(aarch64ElfProblem(elf(183))).toBeNull();
    expect(aarch64ElfProblem(elf(183, 2))).toBeNull();
  });

  test("refuses other machines and formats", () => {
    expect(aarch64ElfProblem(elf(62))).toContain("not AArch64");
    expect(aarch64ElfProblem(elf(183, 1))).toContain("not an executable");
    expect(aarch64ElfProblem(elf(183, 3, 1))).toContain("not a 64-bit");
    expect(aarch64ElfProblem(new TextEncoder().encode("#!/bin/sh\necho a shell script\n"))).toBe("not an ELF file");
    expect(aarch64ElfProblem(new Uint8Array(4))).toContain("too short");
  });
});
