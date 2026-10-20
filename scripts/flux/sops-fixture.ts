/**
 * A stand-in for sops in tests (scripts/flux/sops.test.ts, deploy/flux/tests/flux.test.ts): what
 * sops 3.13.3 writes for a plain Secret under the rules of .sops.yaml, without a key or the tool.
 * Bytes that do not read as text stand in for ciphertext (deterministic), in the shape SOPS writes:
 * a 32-byte IV, a 16-byte tag, an age recipient and its wrapped data key. The same shape as the
 * fixture in scripts/ci/check-secret-handling.test.ts. Nothing here is a secret.
 */

const opaque = (length: number, seed: number) => Buffer.from(Array.from({ length }, (_, i) => ((seed + i * 151) % 256) | 0x80)).toString("base64");

/** An encrypted value in SOPS's shape; `seed` makes it differ from the others. */
export const enc = (seed: number) => `ENC[AES256_GCM,data:${opaque(12, seed)},iv:${opaque(32, 1)},tag:${opaque(16, 2)},type:str]`;

const unpadded = (length: number, seed: number) => opaque(length, seed).replace(/=+$/, "");
const ageFile = () => {
  const header = Buffer.from(`age-encryption.org/v1\n-> X25519 ${unpadded(32, 3)}\n${unpadded(32, 4)}\n--- ${unpadded(32, 5)}\n`);
  const lines = Buffer.concat([header, Buffer.from(opaque(48, 6), "base64")]).toString("base64").match(/.{1,64}/g) ?? [];
  return ["-----BEGIN AGE ENCRYPTED FILE-----", ...lines, "-----END AGE ENCRYPTED FILE-----"].map((line) => `        ${line}`).join("\n");
};

/** An age recipient that is not anyone's key. */
export const FIXTURE_RECIPIENT = `age1${"qpzry9x8gf2tvdw0s3jn54khce6mua7l".repeat(2).slice(0, 58)}`;

/** The scope line of the sops block, as .sops.yaml sets it. */
export const SOPS_SCOPE_LINE = "  encrypted_regex: ^(data|stringData)$\n";

const SOPS_BLOCK = [
  "sops:",
  "  age:",
  `    - recipient: ${FIXTURE_RECIPIENT}`,
  "      enc: |",
  ageFile(),
  '  lastmodified: "2026-10-20T00:00:00Z"',
  `  mac: ${enc(9)}`,
  SOPS_SCOPE_LINE.replace(/\n$/, ""),
  '  version: "3.13.3"',
  "",
].join("\n");

/**
 * What sops writes for `plain` (a Secret as scripts/flux/lib.ts renders it): the same document,
 * every data or stringData value encrypted, and the sops block.
 */
export function fakeSops(plain: string): string {
  let seed = 10;
  let block = "";
  const lines = plain.split("\n").map((line) => {
    if (/^\S/.test(line)) block = line.replace(/:.*$/, "");
    const value = /^( {2}"?[^"\s:]+"?): .+$/.exec(line);
    return value !== null && (block === "data" || block === "stringData") ? `${value[1]}: ${enc(seed++)}` : line;
  });
  return `${lines.join("\n").replace(/\n$/, "")}\n${SOPS_BLOCK}`;
}
