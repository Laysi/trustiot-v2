// The dev CA and server certificate: their structure (parsed by node and by openssl, the way a
// real client would see them), and the verification a client does against them. The handshake
// tests run against a fake Postgres that offers TLS after an SSLRequest, so the wrong-CA and
// wrong-host-name refusals are proven without a container.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createPrivateKey, X509Certificate } from "node:crypto";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type AddressInfo, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { TLSSocket } from "node:tls";

import {
  AUTH_SASL,
  askAuthentication,
  askAuthenticationTls,
  isCertificateRefusal,
  SSL_REQUEST,
} from "../lib/pgwire.ts";
import {
  generateDevCertificates,
  ipBytes,
  SERVER_DNS_NAMES,
  SERVER_IP_ADDRESSES,
  VALIDITY_DAYS,
} from "../lib/tls.ts";

const certs = generateDevCertificates("unit");
const ca = new X509Certificate(certs.caCertificate);
const server = new X509Certificate(certs.serverCertificate);

describe("generateDevCertificates", () => {
  test("the CA is a self-signed CA that may only sign certificates", () => {
    expect(ca.ca).toBe(true);
    expect(ca.verify(ca.publicKey)).toBe(true);
    expect(ca.subject).toContain("TrustIoT dev CA (unit)");
    expect(ca.keyUsage).toBeUndefined(); // an extended key usage list: the CA has none
  });

  test("the server certificate is a leaf signed by the CA", () => {
    expect(server.ca).toBe(false);
    expect(server.verify(ca.publicKey)).toBe(true);
    expect(server.checkIssued(ca)).toBe(true);
    expect(ca.checkIssued(server)).toBe(false);
    expect(server.issuer).toBe(ca.subject);
    expect(server.keyUsage).toEqual(["1.3.6.1.5.5.7.3.1"]);
  });

  test("the server certificate names the service host name, localhost and loopback", () => {
    for (const name of SERVER_DNS_NAMES) expect(server.checkHost(name)).toBe(name);
    for (const address of SERVER_IP_ADDRESSES) expect(server.checkIP(address)).toBe(address);
    expect(server.checkHost("postgres.example.com")).toBeUndefined();
    expect(server.checkHost("evil")).toBeUndefined();
    expect(server.checkIP("10.0.0.1")).toBeUndefined();
  });

  test("the certificates are valid now and for the stated period only", () => {
    const now = Date.now();
    for (const certificate of [ca, server]) {
      expect(Date.parse(certificate.validFrom)).toBeLessThanOrEqual(now);
      const days = (Date.parse(certificate.validTo) - now) / 86_400_000;
      expect(days).toBeGreaterThan(VALIDITY_DAYS - 1);
      expect(days).toBeLessThanOrEqual(VALIDITY_DAYS);
    }
  });

  test("every run makes a new CA, key and serial number", () => {
    const other = generateDevCertificates("unit");
    expect(other.caCertificate).not.toBe(certs.caCertificate);
    expect(other.serverKey).not.toBe(certs.serverKey);
    expect(new X509Certificate(other.serverCertificate).serialNumber).not.toBe(server.serialNumber);
    // A certificate of one run is not signed by the CA of another.
    expect(server.verify(new X509Certificate(other.caCertificate).publicKey)).toBe(false);
  });

  test("the output holds the server key only once and never a CA key", () => {
    expect(certs.serverKey).toMatch(/^-----BEGIN PRIVATE KEY-----\n/);
    expect(certs.caCertificate).not.toContain("PRIVATE KEY");
    expect(certs.serverCertificate).not.toContain("PRIVATE KEY");
    expect(Object.keys(certs).sort()).toEqual(["caCertificate", "serverCertificate", "serverKey"]);
  });

  test("the server key matches the certificate", () => {
    expect(server.checkPrivateKey(createPrivateKey(certs.serverKey))).toBe(true);
  });

  test("a date outside the range of UTCTime is refused", () => {
    expect(() => generateDevCertificates("x", new Date("2060-01-01T00:00:00Z"))).toThrow(/1950 to 2049/);
  });

  test("openssl accepts the chain for the right names and refuses a wrong one", async () => {
    const which = Bun.spawnSync(["openssl", "version"], { stderr: "ignore" });
    if (which.exitCode !== 0) {
      console.error("skipped: no openssl on PATH");
      return;
    }
    const dir = mkdtempSync(join(tmpdir(), "trustiot-tls-test-"));
    try {
      writeFileSync(join(dir, "ca.crt"), certs.caCertificate);
      writeFileSync(join(dir, "server.crt"), certs.serverCertificate);
      const verify = (...extra: string[]) =>
        Bun.spawnSync(
          ["openssl", "verify", "-CAfile", join(dir, "ca.crt"), "-purpose", "sslserver", ...extra, join(dir, "server.crt")],
          { stdout: "pipe", stderr: "pipe" },
        );
      expect(verify("-verify_hostname", "postgres").exitCode).toBe(0);
      expect(verify("-verify_ip", "127.0.0.1").exitCode).toBe(0);
      expect(verify("-verify_hostname", "wrong-name").exitCode).not.toBe(0);
      const text = Bun.spawnSync(["openssl", "x509", "-noout", "-text", "-in", join(dir, "ca.crt")], {
        stdout: "pipe",
      }).stdout.toString();
      expect(text).toContain("CA:TRUE, pathlen:0");
      expect(text).toContain("Certificate Sign, CRL Sign");
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

describe("ipBytes", () => {
  test("encodes IPv4 and IPv6 loopback", () => {
    expect([...ipBytes("127.0.0.1")]).toEqual([127, 0, 0, 1]);
    expect(ipBytes("::1")).toHaveLength(16);
    expect(ipBytes("::1")[15]).toBe(1);
  });

  test("refuses what it cannot encode", () => {
    expect(() => ipBytes("300.0.0.1")).toThrow();
    expect(() => ipBytes("fe80::1")).toThrow();
    expect(() => ipBytes("not-an-address")).toThrow();
  });
});

/**
 * A fake Postgres: it answers an SSLRequest with `S`, completes the TLS handshake with the given
 * certificate and then answers every startup message with AuthenticationSASL (what a real server
 * with SCRAM does). Without an SSLRequest it answers an ErrorResponse, like the reject rule for
 * connections without TLS in `POSTGRES_HBA`.
 */
function fakePostgres(certificate: string, key: string): Promise<Server> {
  const authSasl = Buffer.from([0x52, 0, 0, 0, 8, 0, 0, 0, 10]);
  const errorResponse = Buffer.from([0x45, 0, 0, 0, 5, 0]);
  return new Promise((resolve) => {
    const fake = createServer((socket) => {
      socket.on("error", () => {});
      socket.once("data", (first) => {
        if (!Buffer.from(first).subarray(0, 8).equals(Buffer.from(SSL_REQUEST))) {
          socket.end(errorResponse);
          return;
        }
        socket.write("S", () => {
          const secure = new TLSSocket(socket, { isServer: true, cert: certificate, key });
          secure.on("error", () => {});
          secure.once("data", () => secure.write(authSasl));
        });
      });
    });
    fake.listen({ host: "127.0.0.1", port: 0 }, () => resolve(fake));
  });
}

describe("the TLS check of the published port", () => {
  let fake: Server;
  let port: number;
  beforeAll(async () => {
    fake = await fakePostgres(certs.serverCertificate, certs.serverKey);
    port = (fake.address() as AddressInfo).port;
  });
  afterAll(() => {
    fake.close();
  });

  test("the right CA and each name in the certificate are accepted", async () => {
    for (const serverName of ["127.0.0.1", "postgres", "localhost"]) {
      expect(await askAuthenticationTls(port, "u", "d", { ca: certs.caCertificate, serverName })).toBe(
        AUTH_SASL,
      );
    }
  });

  test("another CA is refused, even with the right name", async () => {
    const other = generateDevCertificates("other");
    const attempt = askAuthenticationTls(port, "u", "d", {
      ca: other.caCertificate,
      serverName: "127.0.0.1",
    });
    const error = await attempt.catch((caught: unknown) => caught);
    expect(isCertificateRefusal(error)).toBe(true);
  });

  test("a name that the certificate does not carry is refused, even with the right CA", async () => {
    for (const serverName of ["wrong-name", "postgres.example.com", "10.0.0.1"]) {
      const attempt = askAuthenticationTls(port, "u", "d", { ca: certs.caCertificate, serverName });
      await expect(attempt).rejects.toMatchObject({ code: "ERR_TLS_CERT_ALTNAME_INVALID" });
      expect(isCertificateRefusal(await attempt.catch((caught: unknown) => caught))).toBe(true);
    }
  });

  test("a network failure is not a certificate refusal", async () => {
    const unused = createServer();
    await new Promise<void>((resolve) => unused.listen({ host: "127.0.0.1", port: 0 }, resolve));
    const closedPort = (unused.address() as AddressInfo).port;
    await new Promise((resolve) => unused.close(resolve));
    const error = await askAuthenticationTls(closedPort, "u", "d", {
      ca: certs.caCertificate,
      serverName: "127.0.0.1",
    }).catch((caught: unknown) => caught);
    expect(error).toBeInstanceOf(Error);
    expect(isCertificateRefusal(error)).toBe(false);
    expect(isCertificateRefusal(new Error("timed out"))).toBe(false);
  });

  test("a connection without TLS gets the server's refusal", async () => {
    expect(await askAuthentication(port, "u", "d")).toBe("error");
  });

  test("a server that does not offer TLS is an error, not a pass", async () => {
    const plain = createServer((socket) => {
      socket.on("error", () => {});
      socket.once("data", () => socket.end("N"));
    });
    await new Promise<void>((resolve) => plain.listen({ host: "127.0.0.1", port: 0 }, resolve));
    try {
      const plainPort = (plain.address() as AddressInfo).port;
      await expect(
        askAuthenticationTls(plainPort, "u", "d", { ca: certs.caCertificate, serverName: "127.0.0.1" }),
      ).rejects.toThrow(/does not offer TLS/);
    } finally {
      plain.close();
    }
  });
});
