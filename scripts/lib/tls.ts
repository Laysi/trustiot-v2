// The development CA and the Postgres server certificate. Everything is made at `up` time with
// node:crypto, so the environment needs no openssl binary and no certificate ever lands in the
// repository: the CA key is dropped as soon as the server certificate is signed, the server key
// goes straight into the Postgres config volume, and only the CA certificate (public) is kept
// in the state directory for the host-side tools and the services.
//
// Node cannot create X.509 certificates, so the few structures needed (a self-signed CA and one
// server certificate) are encoded here as DER. tests/tls.test.ts parses the output with node's
// X509Certificate and with openssl, and the end-to-end test connects a real Postgres and a real
// rustls client to it.
import { createHash, generateKeyPairSync, randomBytes, sign, type KeyObject } from "node:crypto";

/** The names the server certificate carries: how the services and the host tools reach it. */
export const SERVER_DNS_NAMES = ["postgres", "localhost"] as const;
export const SERVER_IP_ADDRESSES = ["127.0.0.1", "::1"] as const;

/** Validity of a generated certificate. `up` makes new ones every time. */
export const VALIDITY_DAYS = 90;
/** Back-dating of `notBefore`, so a small clock difference between host and container is fine. */
const CLOCK_SKEW_MS = 5 * 60 * 1000;

// --- DER ------------------------------------------------------------------------------------

function length(size: number): Buffer {
  if (size < 0x80) return Buffer.from([size]);
  const bytes: number[] = [];
  for (let rest = size; rest > 0; rest = Math.floor(rest / 256)) bytes.unshift(rest % 256);
  return Buffer.from([0x80 | bytes.length, ...bytes]);
}

function tlv(tag: number, ...content: Buffer[]): Buffer {
  const body = Buffer.concat(content);
  return Buffer.concat([Buffer.from([tag]), length(body.length), body]);
}

const sequence = (...items: Buffer[]) => tlv(0x30, ...items);
const set = (...items: Buffer[]) => tlv(0x31, ...items);
const octetString = (content: Buffer) => tlv(0x04, content);
const boolTrue = () => tlv(0x01, Buffer.from([0xff]));
const utf8String = (text: string) => tlv(0x0c, Buffer.from(text, "utf8"));

function oid(dotted: string): Buffer {
  const parts = dotted.split(".").map(Number);
  const bytes: number[] = [parts[0]! * 40 + parts[1]!];
  for (const part of parts.slice(2)) {
    const groups = [part & 0x7f];
    for (let rest = part >>> 7; rest > 0; rest >>>= 7) groups.unshift((rest & 0x7f) | 0x80);
    bytes.push(...groups);
  }
  return tlv(0x06, Buffer.from(bytes));
}

/** A non-negative INTEGER from big-endian bytes. */
function integer(bytes: Buffer): Buffer {
  let start = 0;
  while (start < bytes.length - 1 && bytes[start] === 0) start++;
  const trimmed = bytes.subarray(start);
  return tlv(0x02, trimmed[0]! & 0x80 ? Buffer.concat([Buffer.from([0]), trimmed]) : trimmed);
}

const smallInteger = (value: number) => integer(Buffer.from([value]));

/** A BIT STRING with no unused bits. */
const bitString = (content: Buffer) => tlv(0x03, Buffer.from([0]), content);

/** A BIT STRING holding named bits (the KeyUsage value): bit 0 is the most significant. */
function namedBits(bits: number[]): Buffer {
  const highest = Math.max(...bits);
  const bytes = Buffer.alloc(Math.floor(highest / 8) + 1);
  for (const bit of bits) bytes[Math.floor(bit / 8)]! |= 0x80 >> bit % 8;
  return tlv(0x03, Buffer.from([7 - (highest % 8)]), bytes);
}

function utcTime(date: Date): Buffer {
  const p = (value: number) => String(value).padStart(2, "0");
  const year = date.getUTCFullYear();
  if (year < 1950 || year > 2049) throw new Error("certificate dates must fall in 1950 to 2049");
  const text =
    `${p(year % 100)}${p(date.getUTCMonth() + 1)}${p(date.getUTCDate())}` +
    `${p(date.getUTCHours())}${p(date.getUTCMinutes())}${p(date.getUTCSeconds())}Z`;
  return tlv(0x17, Buffer.from(text, "ascii"));
}

const OID = {
  commonName: "2.5.4.3",
  ecdsaWithSha256: "1.2.840.10045.4.3.2",
  subjectKeyIdentifier: "2.5.29.14",
  keyUsage: "2.5.29.15",
  subjectAltName: "2.5.29.17",
  basicConstraints: "2.5.29.19",
  authorityKeyIdentifier: "2.5.29.35",
  extKeyUsage: "2.5.29.37",
  serverAuth: "1.3.6.1.5.5.7.3.1",
} as const;

function name(commonName: string): Buffer {
  return sequence(set(sequence(oid(OID.commonName), utf8String(commonName))));
}

function extension(id: string, value: Buffer, critical: boolean): Buffer {
  return sequence(oid(id), ...(critical ? [boolTrue()] : []), octetString(value));
}

/** The bytes of an IPv4 or IPv6 address, for a SubjectAltName iPAddress. */
export function ipBytes(address: string): Buffer {
  if (/^\d+\.\d+\.\d+\.\d+$/.test(address)) {
    const octets = address.split(".").map(Number);
    if (octets.some((octet) => octet > 255)) throw new Error(`invalid IPv4 address: ${address}`);
    return Buffer.from(octets);
  }
  // Only the forms this file needs: "::1" and fully written eight-group addresses.
  if (address === "::1") return Buffer.concat([Buffer.alloc(15), Buffer.from([1])]);
  const groups = address.split(":");
  if (groups.length === 8 && groups.every((group) => /^[0-9a-f]{1,4}$/i.test(group))) {
    return Buffer.concat(groups.map((group) => Buffer.from(group.padStart(4, "0"), "hex")));
  }
  throw new Error(`unsupported IP address: ${address}`);
}

// --- certificates ---------------------------------------------------------------------------

interface Subject {
  commonName: string;
  publicKey: KeyObject;
}

function subjectPublicKeyInfo(key: KeyObject): Buffer {
  return key.export({ type: "spki", format: "der" });
}

/**
 * The key identifier (RFC 5280, method 1): SHA-1 of the subjectPublicKey bits. For a P-256 key
 * these are the last 65 bytes of the SubjectPublicKeyInfo: the uncompressed point.
 */
function keyIdentifier(key: KeyObject): Buffer {
  return createHash("sha1").update(subjectPublicKeyInfo(key).subarray(-65)).digest();
}

function issue(options: {
  subject: Subject;
  issuer: { commonName: string; privateKey: KeyObject; publicKey: KeyObject };
  extensions: Buffer[];
  now: Date;
}): string {
  const { subject, issuer, now } = options;
  const notBefore = new Date(now.getTime() - CLOCK_SKEW_MS);
  const notAfter = new Date(now.getTime() + VALIDITY_DAYS * 24 * 3600 * 1000);
  const serial = randomBytes(16);
  serial[0]! &= 0x7f;
  serial[0]! |= 0x40; // never zero, never negative
  const algorithm = sequence(oid(OID.ecdsaWithSha256));
  const tbs = sequence(
    tlv(0xa0, smallInteger(2)), // version: v3
    integer(serial),
    algorithm,
    name(issuer.commonName),
    sequence(utcTime(notBefore), utcTime(notAfter)),
    name(subject.commonName),
    subjectPublicKeyInfo(subject.publicKey),
    tlv(0xa3, sequence(...options.extensions)),
  );
  const signature = sign("sha256", tbs, { key: issuer.privateKey, dsaEncoding: "der" });
  const der = sequence(tbs, algorithm, bitString(signature));
  const body = der.toString("base64").match(/.{1,64}/g)!.join("\n");
  return `-----BEGIN CERTIFICATE-----\n${body}\n-----END CERTIFICATE-----\n`;
}

export interface DevCertificates {
  /** The CA certificate: public, trusted by the services and the host tools. */
  caCertificate: string;
  /** The server certificate chain (the leaf only; the CA is the client's trust anchor). */
  serverCertificate: string;
  /** The server's private key, PKCS#8 PEM. Secret: never leaves the Postgres volume. */
  serverKey: string;
}

/**
 * A fresh CA and a server certificate for Postgres, signed by it. The CA's private key exists
 * only inside this function. `commonName` identifies the instance in the CA's name.
 */
export function generateDevCertificates(commonName: string, now: Date = new Date()): DevCertificates {
  const ca = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const server = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const caName = `TrustIoT dev CA (${commonName})`;
  const caCertificate = issue({
    subject: { commonName: caName, publicKey: ca.publicKey },
    issuer: { commonName: caName, privateKey: ca.privateKey, publicKey: ca.publicKey },
    now,
    extensions: [
      // CA:TRUE with no intermediate below it.
      extension(OID.basicConstraints, sequence(boolTrue(), smallInteger(0)), true),
      // keyCertSign (5) and cRLSign (6)
      extension(OID.keyUsage, namedBits([5, 6]), true),
      extension(OID.subjectKeyIdentifier, octetString(keyIdentifier(ca.publicKey)), false),
    ],
  });
  const names = [
    ...SERVER_DNS_NAMES.map((dns) => tlv(0x82, Buffer.from(dns, "ascii"))),
    ...SERVER_IP_ADDRESSES.map((address) => tlv(0x87, ipBytes(address))),
  ];
  const serverCertificate = issue({
    subject: { commonName: SERVER_DNS_NAMES[0], publicKey: server.publicKey },
    issuer: { commonName: caName, privateKey: ca.privateKey, publicKey: ca.publicKey },
    now,
    extensions: [
      extension(OID.basicConstraints, sequence(), true), // CA:FALSE
      // digitalSignature (0): enough for ECDHE with an ECDSA key
      extension(OID.keyUsage, namedBits([0]), true),
      extension(OID.extKeyUsage, sequence(oid(OID.serverAuth)), false),
      extension(OID.subjectAltName, sequence(...names), false),
      extension(OID.subjectKeyIdentifier, octetString(keyIdentifier(server.publicKey)), false),
      extension(
        OID.authorityKeyIdentifier,
        sequence(tlv(0x80, keyIdentifier(ca.publicKey))),
        false,
      ),
    ],
  });
  return {
    caCertificate,
    serverCertificate,
    serverKey: server.privateKey.export({ type: "pkcs8", format: "pem" }).toString(),
  };
}
