// Checks on the files the operator hands to the build: the administrator's SSH public keys and
// the agent executable.

/** Key types accepted for the administrator. RSA and DSA are refused. */
export const ACCEPTED_KEY_TYPES: ReadonlySet<string> = new Set([
  "ssh-ed25519",
  "sk-ssh-ed25519@openssh.com",
  "ecdsa-sha2-nistp256",
  "ecdsa-sha2-nistp384",
  "ecdsa-sha2-nistp521",
  "sk-ecdsa-sha2-nistp256@openssh.com",
]);

export const MAX_KEY_FILE_BYTES = 64 * 1024;
export const MAX_KEYS = 16;

export class InputError extends Error {}

/** Reads the length-prefixed strings of an SSH public key blob (RFC 4251 "string"). */
function sshStrings(bytes: Uint8Array): Uint8Array[] | null {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const strings: Uint8Array[] = [];
  let offset = 0;
  while (offset < bytes.length) {
    if (offset + 4 > bytes.length) return null;
    const length = view.getUint32(offset);
    offset += 4;
    if (length > bytes.length - offset) return null;
    strings.push(bytes.subarray(offset, offset + length));
    offset += length;
  }
  return strings;
}

const ECDSA_POINT_BYTES: Readonly<Record<string, number>> = { nistp256: 65, nistp384: 97, nistp521: 133 };

/**
 * Why a public key blob is not a complete key of `type`, or null when it is: the exact fields of
 * the type (RFC 8709 for Ed25519, RFC 5656 for ECDSA, OpenSSH PROTOCOL.u2f for the sk- keys), each
 * of the right length, and nothing after them. A truncated key would otherwise reach
 * authorized_keys and leave the gateway without a login.
 */
export function keyBlobProblem(type: string, blob: Uint8Array): string | null {
  const fields = sshStrings(blob);
  if (!fields || fields.length === 0) return "the key data is truncated";
  const text = (bytes: Uint8Array | undefined) => Buffer.from(bytes ?? new Uint8Array()).toString("latin1");
  if (text(fields[0]) !== type) return `the key data is not a ${type} key`;
  const sk = type.startsWith("sk-");
  const expectedFields = (type.includes("ed25519") ? 2 : 3) + (sk ? 1 : 0);
  if (fields.length !== expectedFields) return `the key data has ${fields.length} fields, a ${type} key has ${expectedFields}`;
  if (type.includes("ed25519")) {
    if (fields[1]?.length !== 32) return `the Ed25519 key is ${fields[1]?.length} bytes, expected 32`;
  } else {
    const curve = text(fields[1]);
    const expectedCurve = /nistp(256|384|521)/.exec(type)?.[0] ?? "";
    if (curve !== expectedCurve) return `the curve is ${JSON.stringify(curve.slice(0, 20))}, expected ${expectedCurve}`;
    const point = fields[2];
    const pointBytes = ECDSA_POINT_BYTES[curve] ?? 0;
    if (point?.length !== pointBytes || point[0] !== 0x04) return `the ${curve} point is not an uncompressed point of ${pointBytes} bytes`;
  }
  if (sk && (fields[expectedFields - 1]?.length ?? 0) === 0) return "the security key application is empty";
  return null;
}

/**
 * Parses an authorized_keys style file of public keys and returns each key as "<type> <base64>".
 * The comment after the key is dropped: it is often user@host of a workstation, which does not
 * belong in an image. Options before the key, private keys and unknown types are refused.
 */
export function parsePublicKeys(text: string): string[] {
  if (/PRIVATE KEY/i.test(text) || text.startsWith("openssh-key-v1") || text.includes("PuTTY-User-Key-File")) {
    throw new InputError("the SSH key file holds a private key; pass the public key (.pub) instead");
  }
  const keys: string[] = [];
  text.split("\n").forEach((raw, index) => {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) return;
    const [type = "", blob = ""] = line.split(/\s+/);
    const where = `line ${index + 1}`;
    if (!ACCEPTED_KEY_TYPES.has(type)) {
      throw new InputError(
        `${where}: key type ${JSON.stringify(type.slice(0, 40))} is not accepted (options before the key are not either); accepted: ${[...ACCEPTED_KEY_TYPES].join(", ")}`,
      );
    }
    if (!/^[A-Za-z0-9+/]+={0,2}$/.test(blob) || blob.length % 4 !== 0) throw new InputError(`${where}: the key is not base64`);
    const problem = keyBlobProblem(type, Buffer.from(blob, "base64"));
    if (problem) throw new InputError(`${where}: ${problem}`);
    const normalized = `${type} ${blob}`;
    if (!keys.includes(normalized)) keys.push(normalized);
  });
  if (keys.length === 0) throw new InputError("the SSH key file holds no public key");
  if (keys.length > MAX_KEYS) throw new InputError(`the SSH key file holds ${keys.length} keys; at most ${MAX_KEYS}`);
  return keys;
}

/** The SHA-256 fingerprint of a "<type> <base64>" key, as `ssh-keygen -l -E sha256` prints it. */
export function keyFingerprint(key: string): string {
  const blob = Buffer.from(key.split(" ")[1] ?? "", "base64");
  return `SHA256:${new Bun.CryptoHasher("sha256").update(blob).digest("base64").replace(/=+$/, "")}`;
}

/** The authorized_keys content the image gets for the given keys. */
export function authorizedKeys(keys: string[]): string {
  return `${keys.join("\n")}\n`;
}

const EM_AARCH64 = 183;

/** Why the bytes are not a 64-bit little-endian AArch64 ELF executable, or null when they are. */
export function aarch64ElfProblem(head: Uint8Array): string | null {
  if (head.length < 20) return "the file is too short to be an ELF executable";
  if (head[0] !== 0x7f || head[1] !== 0x45 || head[2] !== 0x4c || head[3] !== 0x46) return "not an ELF file";
  if (head[4] !== 2) return "not a 64-bit ELF file";
  if (head[5] !== 1) return "not a little-endian ELF file";
  const type = (head[16] ?? 0) | ((head[17] ?? 0) << 8);
  if (type !== 2 && type !== 3) return `ELF type ${type} is not an executable`;
  const machine = (head[18] ?? 0) | ((head[19] ?? 0) << 8);
  if (machine !== EM_AARCH64) return `ELF machine ${machine} is not AArch64 (${EM_AARCH64})`;
  return null;
}
