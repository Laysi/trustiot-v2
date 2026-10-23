// Tests of the certs check (certs.ts, Laysi/trustiot-v2#517) without a cluster: the parsing, the
// test root's key handling with a stand-in certs-ca, the checks' sequences against fake answers,
// and the chart rendered with the smoke's values. The certificates, requests and CRLs are real,
// made with openssl in a temporary directory for this file.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { ConnectionError } from "../../deploy/chart/tests/e2e/raw-client.ts";
import type { RawRequest, RawResponse } from "../../deploy/chart/tests/e2e/raw-client.ts";
import { COMMAND_TIMEOUT_MS, SLOW_TEST_MS, throwIfTimedOut } from "../../deploy/chart/tests/timing.ts";
import { apiValues, routesOfValues, SESSION_COOKIE, TENANTS_PATH } from "./api.ts";
import {
  CA_KEYS_CONTAINER,
  CA_NOT_READY,
  certificateOf,
  certificateValue,
  certsObjectsOf,
  certsPodsOf,
  chainProblems,
  checkMeshSigning,
  checkMeshWaiting,
  checkWorkstationSigning,
  checkWorkstationWaiting,
  CHAIN_PATH,
  CRL_PATH,
  issuedProblems,
  issuingValues,
  keyIdOfPublicKeyPem,
  meshAnswerOf,
  meshFetcherOf,
  messageKey,
  opensslCrlVerdict,
  pemBlocks,
  requestKeyId,
  requestKeyOf,
  requestOf,
  rootValues,
  signedKeyIdOf,
  TestRoot,
  WORKSTATION,
  WORKSTATION_CERTIFICATES_PATH,
  workstationRequest,
} from "./certs.ts";
import type { MeshAnswer, MeshDeps, Runner, WorkstationActors, WorkstationDeps } from "./certs.ts";
import { exec, pinnedTool } from "./exec.ts";
import type { ExecOptions, ExecResult } from "./exec.ts";
import { parseManifests } from "./manifests.ts";
import type { K8sObject } from "./manifests.ts";
import { discoverShapes, valuesArgs } from "./smoke.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");
const CHART_DIR = join(REPO_ROOT, "deploy", "chart");
const APP = "app.nightly.test";
/** The prefix of the certs objects' names for release `trustiot` (deploy/chart/templates/_certs.tpl). */
const CERTS = "trustiot-certs";

const ok = (stdout = "", stderr = ""): ExecResult => ({ code: 0, stdout, stderr, timedOut: false });
/** Runs a command in `cwd` with the bounded time every test command gets; throws on failure. */
const runner = (cwd?: string): Runner => async (cmd, options) => {
  const result = await exec(cmd, { ...options, ...(cwd === undefined ? {} : { cwd }), timeoutMs: COMMAND_TIMEOUT_MS });
  if (result.code !== 0) throw new Error(`${cmd.join(" ")} exited with ${result.code}: ${result.stderr}`);
  return result;
};

// ---------------------------------------------------------------------------------------------
// Real certificates for this file: a root, an issuing CA it signed, CRLs of both, a workstation
// request and certificates for it. Every key stays in the temporary directory, removed after.

interface Fixtures {
  root: string;
  issuing: string;
  issuingRequest: string;
  issuingKeyId: string;
  otherIssuing: string;
  issuingCrl: string;
  rootCrl: string;
  workstationRequest: string;
  workstationKeyId: string;
  workstation: string;
  /** A certificate the issuing CA signed for another key under the workstation's name. */
  workstationOtherKey: string;
  workstationByRoot: string;
}
let dir: string;
let fx: Fixtures;

beforeAll(async () => {
  dir = mkdtempSync(join(tmpdir(), "nightly-certs-test-"));
  const config = [
    "[req]", "distinguished_name = dn", "prompt = no", "[dn]", "CN = x",
    "[root_ext]", "basicConstraints = critical,CA:TRUE", "keyUsage = critical,keyCertSign,cRLSign", "subjectKeyIdentifier = hash",
    "[issuing_ext]", "basicConstraints = critical,CA:TRUE,pathlen:0", "keyUsage = critical,keyCertSign,cRLSign", "subjectKeyIdentifier = hash", "authorityKeyIdentifier = keyid",
    "[leaf_ext]", "basicConstraints = critical,CA:FALSE", "keyUsage = critical,digitalSignature", "authorityKeyIdentifier = keyid",
    "[crl_ca]", "database = index.txt", "crlnumber = crlnumber", "default_md = sha256", "default_crl_days = 1",
  ];
  writeFileSync(join(dir, "openssl.cnf"), `${config.join("\n")}\n`);
  writeFileSync(join(dir, "index.txt"), "");
  writeFileSync(join(dir, "crlnumber"), "01\n");
  const openssl = (args: string[]) => runner(dir)(["openssl", ...args], {});
  const key = (curve: string) => ["-newkey", "ec", "-pkeyopt", `ec_paramgen_curve:${curve}`, "-nodes", "-config", "openssl.cnf"];
  await openssl(["req", "-x509", ...key("P-384"), "-extensions", "root_ext", "-subj", "/CN=Test root", "-days", "2", "-keyout", "root.key", "-out", "root.pem"]);
  await openssl(["req", "-new", ...key("P-256"), "-subj", "/CN=Test issuing key", "-keyout", "issuing.key", "-out", "issuing.csr"]);
  const sign = (request: string, ca: string, extensions: string, out: string, serial: string) =>
    openssl(["x509", "-req", "-in", request, "-CA", `${ca}.pem`, "-CAkey", `${ca}.key`, "-set_serial", serial, "-days", "1", "-extfile", "openssl.cnf", "-extensions", extensions, "-out", out]);
  await sign("issuing.csr", "root", "issuing_ext", "issuing.pem", "0x01");
  // Another issuing CA of the same key under another name: a chain with the wrong certificate.
  await openssl(["req", "-new", "-key", "issuing.key", "-config", "openssl.cnf", "-subj", "/CN=Other issuing", "-out", "other.csr"]);
  await sign("other.csr", "root", "issuing_ext", "other.pem", "0x02");
  for (const ca of ["issuing", "root"]) {
    await openssl(["ca", "-config", "openssl.cnf", "-name", "crl_ca", "-gencrl", "-keyfile", `${ca}.key`, "-cert", `${ca}.pem`, "-out", `${ca}.crl`]);
  }
  await openssl(["req", "-new", ...key("P-256"), "-subj", `/CN=workstation:${WORKSTATION}`, "-keyout", "ws.key", "-out", "ws.csr"]);
  await sign("ws.csr", "issuing", "leaf_ext", "ws.pem", "0x1a2b3c");
  await sign("ws.csr", "root", "leaf_ext", "ws-root.pem", "0x1a2b3c");
  await openssl(["req", "-new", ...key("P-256"), "-subj", `/CN=workstation:${WORKSTATION}`, "-keyout", "ws-other.key", "-out", "ws-other.csr"]);
  await sign("ws-other.csr", "issuing", "leaf_ext", "ws-other.pem", "0x1a2b3c");
  const read = (file: string) => readFileSync(join(dir, file), "utf8");
  const publicKey = await openssl(["x509", "-in", "issuing.pem", "-noout", "-pubkey"]);
  fx = {
    root: read("root.pem"),
    issuing: read("issuing.pem"),
    issuingRequest: read("issuing.csr"),
    issuingKeyId: keyIdOfPublicKeyPem(publicKey.stdout),
    otherIssuing: read("other.pem"),
    issuingCrl: read("issuing.crl"),
    rootCrl: read("root.crl"),
    workstationRequest: read("ws.csr"),
    workstationKeyId: await requestKeyId(runner(dir), read("ws.csr")),
    workstation: read("ws.pem"),
    workstationOtherKey: read("ws-other.pem"),
    workstationByRoot: read("ws-root.pem"),
  };
}, SLOW_TEST_MS);

afterAll(() => rmSync(dir, { recursive: true, force: true }));

// ---------------------------------------------------------------------------------------------

describe("PEM, certificates and key ids", () => {
  test("pemBlocks takes every block of a label and refuses anything else around them", () => {
    expect(pemBlocks(`${fx.root}${fx.issuing}`, "CERTIFICATE")).toHaveLength(2);
    expect(pemBlocks("", "CERTIFICATE")).toEqual([]);
    expect(pemBlocks(fx.issuingCrl, "X509 CRL")).toHaveLength(1);
    expect(() => pemBlocks(`${fx.root}garbage`, "CERTIFICATE")).toThrow("text outside the CERTIFICATE blocks");
    expect(() => pemBlocks(fx.issuingCrl, "CERTIFICATE")).toThrow("text outside");
  });

  test("certificateValue is the DER in base64 on one line, as the chart takes it, and only for one certificate", () => {
    const value = certificateValue(fx.root);
    expect(value).toMatch(/^MII[A-Za-z0-9+/]+={0,2}$/);
    expect(Buffer.from(value, "base64").equals(pemBlocks(fx.root, "CERTIFICATE")[0]!)).toBe(true);
    expect(() => certificateValue(`${fx.root}${fx.issuing}`)).toThrow("expected one certificate, found 2");
    expect(rootValues(fx.root)).toEqual({ certificateAuthority: { rootCertificate: value } });
    expect(issuingValues(fx.issuing)).toEqual({ certificateAuthority: { issuingCertificates: [certificateValue(fx.issuing)] } });
  });

  test("the key id of a request is the SHA-256 of its key's SubjectPublicKeyInfo, the same as of the certificate for that key", async () => {
    const id = await requestKeyId(runner(), fx.issuingRequest);
    expect(id).toMatch(/^[0-9a-f]{64}$/);
    expect(id).toBe(fx.issuingKeyId);
    expect(await requestKeyId(runner(), fx.workstationRequest)).not.toBe(id);
  });

  test("workstationRequest makes a P-256 request for the smoke's workstation and keeps no key", async () => {
    const request = await workstationRequest(runner());
    expect(pemBlocks(request, "CERTIFICATE REQUEST")).toHaveLength(1);
    expect(request).not.toContain("PRIVATE KEY");
    expect(await requestKeyId(runner(), request)).toMatch(/^[0-9a-f]{64}$/);
    // RFC 2253 output is the same across OpenSSL versions; the default -text form is not (3.0 prints "CN = ...").
    const subject = await runner()(["openssl", "req", "-noout", "-subject", "-nameopt", "RFC2253"], { stdin: request });
    expect(subject.stdout.trim().replace(/^subject=\s*/, "")).toBe(`CN=workstation:${WORKSTATION}`);
    const text = await runner()(["openssl", "req", "-noout", "-text"], { stdin: request });
    expect(text.stdout).toContain("prime256v1");
  });

  test("a request whose signature does not verify has no key id", async () => {
    // Flip a byte inside the signature (the end of the DER), keeping the PEM well-formed.
    const der = pemBlocks(fx.issuingRequest, "CERTIFICATE REQUEST")[0]!;
    der[der.length - 3] = der[der.length - 3]! ^ 0x01;
    const broken = `-----BEGIN CERTIFICATE REQUEST-----\n${der.toString("base64").match(/.{1,64}/g)!.join("\n")}\n-----END CERTIFICATE REQUEST-----\n`;
    await expect(requestKeyId(runner(), broken)).rejects.toThrow();
  });

  test("signedKeyIdOf reads the one key id certs-ca root sign printed", () => {
    const id = "ab".repeat(32);
    expect(signedKeyIdOf(`certs-ca: wrote the issuing CA certificate to /x/issuing.pem for key ${id}; set it only if ...\n`)).toBe(id);
    expect(() => signedKeyIdOf("certs-ca: wrote nothing")).toThrow("printed 0 key ids");
    expect(() => signedKeyIdOf(`for key ${id} and for key ${id}`)).toThrow("printed 2 key ids");
  });

  test("chainProblems wants exactly the expected certificates, in order", () => {
    expect(chainProblems(fx.root, [fx.root])).toEqual([]);
    expect(chainProblems(`${fx.root}${fx.issuing}`, [fx.root, fx.issuing])).toEqual([]);
    expect(chainProblems(`${fx.root}${fx.issuing}`, [fx.root])).toEqual(["ca.pem holds 2 certificate(s), expected 1"]);
    expect(chainProblems(`${fx.root}${fx.otherIssuing}`, [fx.root, fx.issuing])).toEqual(["ca.pem: certificate 2 is not the expected one"]);
    expect(chainProblems("not pem", [fx.root])[0]).toContain("ca.pem: text outside");
  });
});

describe("the release's objects", () => {
  const deployment: K8sObject = {
    kind: "Deployment",
    metadata: { name: "trustiot-certs", labels: { "app.kubernetes.io/component": "certs" } },
    spec: {
      selector: { matchLabels: { "app.kubernetes.io/name": "trustiot", "app.kubernetes.io/component": "certs" } },
      template: {
        spec: {
          initContainers: [
            {
              name: CA_KEYS_CONTAINER,
              env: [
                { name: "TRUSTIOT__KEYS__SECRET", value: "trustiot-certs-ca-keys" },
                { name: "TRUSTIOT__KEYS__CONFIG_MAP", value: "trustiot-certs-ca-requests" },
                { name: "TRUSTIOT__KEYS__GENERATION", value: "1" },
              ],
            },
          ],
        },
      },
    },
  };

  test("certsObjectsOf reads the key objects from the init container and the selector from the Deployment", () => {
    expect(certsObjectsOf([deployment])).toEqual({
      deployment: "trustiot-certs",
      selector: "app.kubernetes.io/name=trustiot,app.kubernetes.io/component=certs",
      secret: `${CERTS}-ca-keys`,
      configMap: `${CERTS}-ca-requests`,
      generation: "1",
    });
    expect(() => certsObjectsOf([])).toThrow("one certs Deployment, found 0");
  });

  test("requestOf wants exactly the generation's request, one PEM request", () => {
    const key = requestKeyOf("1");
    expect(key).toBe("issuing-ca-generation-1.csr.pem");
    expect(requestOf("cm", { data: { [key]: fx.issuingRequest } }, "1")).toEqual({ request: fx.issuingRequest, problems: [] });
    expect(requestOf("cm", { data: {} }, "1").problems).toEqual(["cm: holds the keys [], expected only issuing-ca-generation-1.csr.pem", "cm: no request of generation 1"]);
    expect(requestOf("cm", { data: { [key]: fx.issuingRequest, other: "x" } }, "1").problems).toHaveLength(1);
    expect(requestOf("cm", { data: { [key]: `${fx.issuingRequest}${fx.issuingRequest}` } }, "1").problems).toEqual([`cm: ${key} holds 2 requests, expected one`]);
    expect(requestOf("cm", { data: { [key]: fx.root } }, "1").problems[0]).toContain("text outside the CERTIFICATE REQUEST blocks");
  });

  test("certsPodsOf reads each pod's uid and how its ca-keys init container ended", () => {
    const pod = (name: string, uid: string, terminated?: { exitCode: number }) => ({
      metadata: { name, uid },
      status: { initContainerStatuses: [{ name: CA_KEYS_CONTAINER, state: terminated === undefined ? { running: {} } : { terminated } }] },
    });
    expect(certsPodsOf({ items: [pod("b", "u2"), pod("a", "u1", { exitCode: 0 })] })).toEqual([
      { name: "a", uid: "u1", caKeysExit: 0 },
      { name: "b", uid: "u2" },
    ]);
  });

  test("meshAnswerOf reads BusyBox wget's exit status and error line", () => {
    expect(meshAnswerOf(ok("pem"))).toEqual({ ok: true, body: "pem", detail: "" });
    expect(meshAnswerOf({ code: 1, stdout: "", stderr: "wget: server returned error: HTTP/1.1 503 Service Unavailable\ncommand terminated with exit code 1\n", timedOut: false })).toEqual({
      ok: false,
      status: 503,
      body: "",
      detail: "command terminated with exit code 1",
    });
    expect(meshAnswerOf({ code: 1, stdout: "", stderr: "wget: can't connect to remote host: Connection refused", timedOut: false }).status).toBeUndefined();
    expect(meshAnswerOf({ code: null, stdout: "", stderr: "", timedOut: true }).detail).toBe("timed out");
  });
});

// ---------------------------------------------------------------------------------------------

describe("the test root", () => {
  /**
   * A stand-in for certs-ca: `root create` writes a certificate and prints a key-shaped PEM of
   * repeated characters; `root sign` records its standard input and writes a certificate.
   */
  function fakeCertsCa(createStdout?: string) {
    const calls: { cmd: string[]; options: ExecOptions }[] = [];
    const key = `-----BEGIN PRIVATE KEY-----\n${"A".repeat(64)}\n${"B".repeat(64)}\n-----END PRIVATE KEY-----\n`;
    const run: Runner = async (cmd, options) => {
      calls.push({ cmd, options });
      const file = cmd[cmd.indexOf("--certificate") + 1]!;
      if (cmd[2] === "create") {
        expect(existsSync(file)).toBe(false);
        writeFileSync(file, fx.root);
        return ok(createStdout ?? key, `certs-ca: wrote the root certificate to ${file}; the root private key went to stdout\n`);
      }
      expect(readFileSync(cmd[cmd.indexOf("--request") + 1]!, "utf8")).toBe(fx.issuingRequest);
      expect(readFileSync(cmd[cmd.indexOf("--root-certificate") + 1]!, "utf8")).toBe(fx.root);
      writeFileSync(file, fx.issuing);
      return ok("", `certs-ca: wrote the issuing CA certificate to ${file} for key ${fx.issuingKeyId}; set it only if ...\n`);
    };
    return { calls, key, run };
  }

  test("keeps the key off the command line and the log, hands it to sign on stdin once, then drops it", async () => {
    const certsCa = fakeCertsCa();
    const root = await TestRoot.create(certsCa.run, "/bin/certs-ca", "TrustIoT nightly test root CA saas");
    expect(root.certificate).toBe(fx.root);
    expect(root.holdsKey).toBe(true);
    const created = certsCa.calls[0]!;
    expect(created.cmd).toEqual(["/bin/certs-ca", "root", "create", "--common-name", "TrustIoT nightly test root CA saas", "--days", "7", "--certificate", created.cmd.at(-1)!]);
    // stdout (the key) never reaches the log, and the certificate's directory is gone.
    expect(created.options.quiet).toBe(true);
    expect(existsSync(created.cmd.at(-1)!)).toBe(false);
    expect(JSON.stringify(root)).not.toContain("AAAA");

    const signed = await root.sign(certsCa.run, "/bin/certs-ca", fx.issuingRequest, "TrustIoT nightly issuing CA G1 saas");
    expect(signed).toEqual({ certificate: fx.issuing, keyId: fx.issuingKeyId });
    const sign = certsCa.calls[1]!;
    expect(sign.options.stdin).toBe(certsCa.key);
    expect(sign.options.quiet).toBe(true);
    expect(sign.cmd.join(" ")).not.toContain("AAAA");
    expect(sign.cmd.slice(0, 3)).toEqual(["/bin/certs-ca", "root", "sign"]);
    expect(sign.cmd[sign.cmd.indexOf("--days") + 1]).toBe("2");
    expect(existsSync(sign.cmd[sign.cmd.indexOf("--certificate") + 1]!)).toBe(false);
    expect(root.holdsKey).toBe(false);
    await expect(root.sign(certsCa.run, "/bin/certs-ca", fx.issuingRequest, "again")).rejects.toThrow("already used or discarded");
  });

  test("a failed signature drops the key too, and discard drops it without a signature", async () => {
    const certsCa = fakeCertsCa();
    const root = await TestRoot.create(certsCa.run, "/bin/certs-ca", "r");
    const failing: Runner = async () => Promise.reject(new Error("certs-ca exited with 1"));
    await expect(root.sign(failing, "/bin/certs-ca", fx.issuingRequest, "i")).rejects.toThrow("exited with 1");
    expect(root.holdsKey).toBe(false);
    const other = await TestRoot.create(certsCa.run, "/bin/certs-ca", "r");
    other.discard();
    expect(other.holdsKey).toBe(false);
    await expect(other.sign(certsCa.run, "/bin/certs-ca", fx.issuingRequest, "i")).rejects.toThrow("already used or discarded");
  });

  test("refuses output that is not one private key, without repeating it", async () => {
    const printed = `not a key ${"Z".repeat(20)}`;
    const error = await TestRoot.create(fakeCertsCa(printed).run, "/bin/certs-ca", "r").catch((e: unknown) => e as Error);
    expect(error).toBeInstanceOf(Error);
    expect((error as Error).message).toContain("printed something other than one PKCS#8 PEM private key");
    expect((error as Error).message).not.toContain("ZZZZ");
  });
});

// ---------------------------------------------------------------------------------------------

/** A mesh that answers each path from a list of answers, the last one repeating. */
function fakeMesh(answers: Record<string, MeshAnswer[]>): { deps: MeshDeps; fetched: string[] } {
  const fetched: string[] = [];
  let clock = 0;
  const deps: MeshDeps = {
    fetch: async (path) => {
      fetched.push(path);
      const list = answers[path] ?? [];
      return list.length > 1 ? list.shift()! : list[0]!;
    },
    crlVerdict: opensslCrlVerdict,
    log: () => {},
    sleep: async (ms) => {
      clock += ms;
    },
    now: () => clock,
    limits: { routeMs: 30_000, reloadMs: 30_000, crlMs: 30_000, pollMs: 1_000 },
  };
  return { deps, fetched };
}
const body = (text: string): MeshAnswer => ({ ok: true, body: text, detail: "" });
const status = (code: number): MeshAnswer => ({ ok: false, status: code, body: "", detail: `server returned error: HTTP/1.1 ${code}` });

describe("inside the mesh", () => {
  test("while waiting: ca.pem is the root alone and crl.pem empty", async () => {
    const { deps } = fakeMesh({ [CHAIN_PATH]: [status(503), body(fx.root)], [CRL_PATH]: [body("")] });
    expect(await checkMeshWaiting(deps, fx.root)).toBe("inside the mesh ca.pem is the test root alone and crl.pem is empty");
  });

  test.each([
    ["ca.pem with an issuing CA already", () => ({ [CHAIN_PATH]: [body(`${fx.root}${fx.issuing}`)], [CRL_PATH]: [body("")] }), "ca.pem holds 2 certificate(s), expected 1"],
    ["a CRL before any issuing CA", () => ({ [CHAIN_PATH]: [body(fx.root)], [CRL_PATH]: [body(fx.issuingCrl)] }), "crl.pem inside the mesh holds"],
    ["crl.pem refused", () => ({ [CHAIN_PATH]: [body(fx.root)], [CRL_PATH]: [status(503)] }), "crl.pem inside the mesh answered 503"],
    ["ca.pem never served", () => ({ [CHAIN_PATH]: [status(503)], [CRL_PATH]: [body("")] }), "timed out after 30 s waiting for ca.pem inside the mesh (200)"],
  ])("while waiting, fails on %s", async (_label, answers, message) => {
    await expect(checkMeshWaiting(fakeMesh(answers()).deps, fx.root)).rejects.toThrow(message);
  });

  test("once signing: waits for the issuing CA in ca.pem and its first CRL, which only the issuing CA's key verifies", async () => {
    const { deps, fetched } = fakeMesh({
      [CHAIN_PATH]: [body(fx.root), body(fx.root), body(`${fx.root}${fx.issuing}`)],
      [CRL_PATH]: [status(503), body(fx.issuingCrl)],
    });
    const detail = await checkMeshSigning(deps, fx.root, fx.issuing);
    expect(detail).toContain("ca.pem became the root and the issuing CA after 2 s");
    expect(detail).toContain("crl.pem one CRL issued by CN=Test issuing key after 1 s more");
    expect(fetched).toEqual([CHAIN_PATH, CHAIN_PATH, CHAIN_PATH, CRL_PATH, CRL_PATH]);
  });

  test.each([
    ["a CRL the root signed", () => ({ [CHAIN_PATH]: [body(`${fx.root}${fx.issuing}`)], [CRL_PATH]: [body(fx.rootCrl)] }), "does not verify with the issuing CA's key"],
    ["two CRLs", () => ({ [CHAIN_PATH]: [body(`${fx.root}${fx.issuing}`)], [CRL_PATH]: [body(`${fx.issuingCrl}${fx.issuingCrl}`)] }), "crl.pem holds 2 CRLs, expected one"],
    ["another issuing CA in ca.pem", () => ({ [CHAIN_PATH]: [body(`${fx.root}${fx.otherIssuing}`)], [CRL_PATH]: [body(fx.issuingCrl)] }), "certificate 2 is not the expected one"],
    ["a service that never loads it", () => ({ [CHAIN_PATH]: [body(fx.root)], [CRL_PATH]: [body("")] }), "timed out after 30 s waiting for the certs service to serve the issuing CA"],
    ["no CRL ever", () => ({ [CHAIN_PATH]: [body(`${fx.root}${fx.issuing}`)], [CRL_PATH]: [status(503)] }), "timed out after 30 s waiting for the first CRL"],
  ])("once signing, fails on %s", async (_label, answers, message) => {
    await expect(checkMeshSigning(fakeMesh(answers()).deps, fx.root, fx.issuing)).rejects.toThrow(message);
  });

  test("opensslCrlVerdict verifies the signature with the given CA and reads the issuer", async () => {
    expect(await opensslCrlVerdict(fx.issuingCrl, fx.issuing)).toEqual({ verified: true, issuer: "CN=Test issuing key" });
    expect((await opensslCrlVerdict(fx.issuingCrl, fx.root)).verified).toBe(false);
    expect(await opensslCrlVerdict(fx.rootCrl, fx.root)).toEqual({ verified: true, issuer: "CN=Test root" });
  });
});

// ---------------------------------------------------------------------------------------------

/** The user ID of the platform administrator the bootstrap made (runApiCheck). */
const BOOTSTRAPPED = "0199f0a1-0000-7000-8000-000000000582";

/** An ingress with the certs service behind it; `issue` answers the platform administrator's issue requests in turn. */
/** A gate that is open: `actor` gets what a platform administrator gets on `call`. */
interface OpenGate {
  actor: "anonymous" | "tenant administrator";
  call: "issue" | "revoke";
}

function fakeIngress(options: { routeAfter?: number; issue: (RawResponse | Error)[]; revoke?: RawResponse[]; open?: OpenGate }) {
  const sent: RawRequest[] = [];
  const sql: string[] = [];
  let routeAfter = options.routeAfter ?? 0;
  let clock = 0;
  const error = (status: number, code: string, key = code): RawResponse => ({ status, headers: {}, body: JSON.stringify({ code, message_key: key }), ms: 1 });
  const cookieOf = (request: RawRequest) => request.headers?.find(([name]) => name === "cookie")?.[1];
  let admin: string | undefined;
  let tenantAdmin: string | undefined;
  const revokes = [...(options.revoke ?? [])];
  const deps: WorkstationDeps = {
    psql: async (text) => {
      sql.push(text);
      const cookie = /'\\x([0-9a-f]{64})'/.exec(text)?.[1];
      // The tenant administrator comes with a membership; the platform administrator's SQL is only
      // a session for the user the bootstrap made.
      if (text.includes("identity.memberships")) tenantAdmin = cookie;
      else admin = cookie;
    },
    send: async (request) => {
      sent.push(request);
      const cookie = cookieOf(request);
      const hash = cookie === undefined ? undefined : new Bun.CryptoHasher("sha256").update(Buffer.from(cookie.split("=")[1]!, "base64url")).digest("hex");
      if (request.target === TENANTS_PATH) return { status: 201, headers: {}, body: JSON.stringify({ tenant_id: "0b5e4f8e-8f2e-4c3e-9d4a-2f1b3c4d5e6f", name: "t" }), ms: 1 };
      const call = request.target === WORKSTATION_CERTIFICATES_PATH ? "issue" : "revoke";
      const openTo = (actor: OpenGate["actor"]) => options.open?.actor === actor && options.open.call === call;
      if (hash === undefined && !openTo("anonymous")) return error(401, "unauthenticated");
      if (routeAfter > 0) {
        routeAfter -= 1;
        return { status: 404, headers: {}, body: "", ms: 1 };
      }
      if (hash !== undefined && hash === tenantAdmin && !openTo("tenant administrator")) return error(403, "permission_denied");
      if (options.open === undefined) expect(hash).toBe(admin ?? "no platform administrator");
      if (request.target === WORKSTATION_CERTIFICATES_PATH) {
        const next = options.issue.length > 1 ? options.issue.shift()! : options.issue[0]!;
        if (next instanceof Error) throw next;
        return next;
      }
      return revokes.shift() ?? error(409, "conflict", "workstation_certificate.already_revoked");
    },
    log: () => {},
    sleep: async (ms) => {
      clock += ms;
    },
    now: () => clock,
    limits: { routeMs: 30_000, signerMs: 30_000, pollMs: 1_000 },
  };
  return { deps, sent, sql, error };
}

describe("the workstation certificate API through the ingress", () => {
  const notReady = (): RawResponse => ({ status: 503, headers: {}, body: JSON.stringify({ code: "unavailable", message_key: CA_NOT_READY }), ms: 1 });
  const issuedBody = (overrides: Record<string, unknown> = {}) =>
    JSON.stringify({
      serial: "1a2b3c",
      workstation: WORKSTATION,
      subject: `workstation:${WORKSTATION}`,
      ca_key_id: fx.issuingKeyId,
      not_before: "2026-10-23T00:00:00Z",
      not_after: "2026-10-24T00:00:00Z",
      certificate: fx.workstation,
      ...overrides,
    });
  const issued = (overrides: Record<string, unknown> = {}): RawResponse => ({ status: 201, headers: {}, body: issuedBody(overrides), ms: 1 });
  const revoked = (): RawResponse => ({ status: 200, headers: {}, body: issuedBody({ revoked_at: "2026-10-23T01:00:00Z", revocation_reason: "superseded" }), ms: 1 });
  const issuing = () => ({ certificate: fx.issuing, keyId: fx.issuingKeyId });
  const workstation = () => ({ request: fx.workstationRequest, keyId: fx.workstationKeyId });

  async function waitingActors(): Promise<WorkstationActors> {
    const ingress = fakeIngress({ issue: [notReady()] });
    return (await checkWorkstationWaiting(ingress.deps, APP, fx.workstationRequest, BOOTSTRAPPED, "t")).actors;
  }

  test("while waiting: 403 to the tenant administrator once routed, 401 without a cookie, 503 ca_not_ready to the platform administrator", async () => {
    const ingress = fakeIngress({ routeAfter: 2, issue: [notReady()] });
    const { detail, actors } = await checkWorkstationWaiting(ingress.deps, APP, fx.workstationRequest, BOOTSTRAPPED, "nightly smoke certs saas");
    expect(detail).toContain("routed after 2 s");
    expect(detail).toContain(`as a platform administrator 503 ${CA_NOT_READY}`);
    // The tenant administrator is an administrator of the new tenant, and no platform administrator.
    expect(ingress.sql[1]).toContain(`'${actors.tenantId}', '${actors.tenantAdmin.userId}', 'admin'`);
    expect(ingress.sql[1]).not.toContain("platform_admin");
    // The platform administrator is the user the bootstrap made: a session of theirs, no new user.
    expect(actors.platformAdmin.userId).toBe(BOOTSTRAPPED);
    expect(ingress.sql[0]).toContain(`'${BOOTSTRAPPED}', now()`);
    expect(ingress.sql[0]).not.toContain("identity.users");
    // The cookie of each request, and the body: the workstation's request, as JSON.
    const issueRequests = ingress.sent.filter((r) => r.target === WORKSTATION_CERTIFICATES_PATH);
    expect(issueRequests.map((r) => r.headers?.find(([n]) => n === "cookie")?.[1].startsWith(`${SESSION_COOKIE}=`) ?? false)).toEqual([true, true, true, false, true]);
    for (const request of issueRequests) {
      expect(request.method).toBe("POST");
      expect(request.headers).toContainEqual(["content-type", "application/json"]);
      expect(JSON.parse(request.body!)).toEqual({ workstation: WORKSTATION, certificate_request: fx.workstationRequest });
    }
  });

  test("while waiting, fails when the platform administrator gets a certificate: the CA must not sign yet", async () => {
    const ingress = fakeIngress({ issue: [issued()] });
    await expect(checkWorkstationWaiting(ingress.deps, APP, fx.workstationRequest, BOOTSTRAPPED, "t")).rejects.toThrow(`expected 503 ${CA_NOT_READY}`);
  });

  test("once signing: 401 and 403, then 201 once the CA signs, the certificate checked; revoke 401, 403, 200 and 409", async () => {
    const actors = await waitingActors();
    const ingress = fakeIngress({ issue: [notReady(), issued()], revoke: [revoked()] });
    // The sessions of the waiting check, which this ingress learns from its own SQL.
    await ingress.deps.psql(`identity.sessions '\\x${actors.platformAdmin.tokenHash}'`);
    await ingress.deps.psql(`identity.memberships '\\x${actors.tenantAdmin.tokenHash}'`);
    const detail = await checkWorkstationSigning(ingress.deps, APP, workstation(), actors, issuing());
    expect(detail).toContain("as a platform administrator 201 after 1 s with certificate 1a2b3c");
    expect(detail).toContain("again 409 already_revoked");
    const targets = ingress.sent.map((r) => r.target);
    const revoke = `${WORKSTATION_CERTIFICATES_PATH}/1a2b3c/revoke`;
    expect(targets).toEqual([...Array(4).fill(WORKSTATION_CERTIFICATES_PATH), revoke, revoke, revoke, revoke]);
    expect(JSON.parse(ingress.sent.at(-1)!.body!)).toEqual({ reason: "superseded" });
  });

  test("once signing, never sends an issue request again after a lost connection: it may have issued", async () => {
    const actors = await waitingActors();
    const ingress = fakeIngress({ issue: [new ConnectionError("connection reset"), issued()] });
    await ingress.deps.psql(`identity.sessions '\\x${actors.platformAdmin.tokenHash}'`);
    await ingress.deps.psql(`identity.memberships '\\x${actors.tenantAdmin.tokenHash}'`);
    await expect(checkWorkstationSigning(ingress.deps, APP, workstation(), actors, issuing())).rejects.toThrow("connection reset");
    expect(ingress.sent.filter((r) => r.target === WORKSTATION_CERTIFICATES_PATH)).toHaveLength(3);
  });

  // An open gate must fail the check: these are what the nightly proves about /api/certs.
  test("while waiting, fails when the tenant administrator is let through: it never sees the 403 it waits for", async () => {
    const ingress = fakeIngress({ issue: [notReady()], open: { actor: "tenant administrator", call: "issue" } });
    await expect(checkWorkstationWaiting(ingress.deps, APP, fx.workstationRequest, BOOTSTRAPPED, "t")).rejects.toThrow("permission_denied");
  });

  test("while waiting, fails when issuing without a cookie is let through", async () => {
    const ingress = fakeIngress({ issue: [notReady()], open: { actor: "anonymous", call: "issue" } });
    await expect(checkWorkstationWaiting(ingress.deps, APP, fx.workstationRequest, BOOTSTRAPPED, "t")).rejects.toThrow("expected 401 unauthenticated");
  });

  test.each([
    ["anonymous", "issue", "expected 401 unauthenticated"],
    ["tenant administrator", "issue", "expected 403 permission_denied"],
    ["anonymous", "revoke", "expected 401 unauthenticated"],
    ["tenant administrator", "revoke", "expected 403 permission_denied"],
  ] as const)("once signing, fails when the %s may %s", async (actor, call, message) => {
    const actors = await waitingActors();
    const ingress = fakeIngress({ issue: [issued()], revoke: [revoked(), revoked()], open: { actor, call } });
    await ingress.deps.psql(`identity.sessions '\\x${actors.platformAdmin.tokenHash}'`);
    await ingress.deps.psql(`identity.memberships '\\x${actors.tenantAdmin.tokenHash}'`);
    await expect(checkWorkstationSigning(ingress.deps, APP, workstation(), actors, issuing())).rejects.toThrow(message);
  });

  test("issuedProblems checks the certificate against the issuing CA, the key id certs-ca printed and the request's key", () => {
    const problems = (body: string) => issuedProblems(body, issuing(), fx.workstationKeyId);
    expect(problems(issuedBody())).toEqual({ issued: { serial: "1a2b3c", certificate: fx.workstation }, problems: [] });
    expect(problems(issuedBody({ ca_key_id: "00".repeat(32) })).problems[0]).toContain(`not the key ${fx.issuingKeyId}`);
    expect(problems(issuedBody({ certificate: fx.workstationByRoot })).problems).toEqual(["the certificate is not signed by the issuing CA"]);
    expect(problems(issuedBody({ certificate: fx.workstationOtherKey })).problems).toEqual(["the certificate is not for the request's key"]);
    expect(problems(issuedBody({ serial: "ff" })).problems).toEqual(["the certificate's serial number is not the response's serial"]);
    expect(problems(issuedBody({ serial: "../x" })).problems[0]).toContain("is not hex");
    expect(problems(issuedBody({ workstation: "other" })).problems[0]).toContain("workstation is");
    expect(problems("<html>").problems).toEqual(["the issue response is not JSON"]);
  });

  test("messageKey reads an ErrorBody's message_key only", () => {
    expect(messageKey(JSON.stringify({ code: "unavailable", message_key: CA_NOT_READY }))).toBe(CA_NOT_READY);
    expect(messageKey("upstream connect error")).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------------------------

describe("the chart with the smoke's certs values", () => {
  for (const shape of discoverShapes(CHART_DIR)) {
    // Two renders and resolving the pinned helm, each command with its own limit.
    test(`values-${shape.name}.yaml: the CA objects the check reads, and setting the issuing CA certificate changes no pod`, async () => {
      const helm = await pinnedTool("helm", REPO_ROOT);
      const work = mkdtempSync(join(tmpdir(), "nightly-certs-values-"));
      const render = (files: string[]): K8sObject[] => {
        const cmd = [helm, "template", "trustiot", CHART_DIR, "--namespace", `nightly-${shape.name}`, ...valuesArgs(CHART_DIR, shape), ...files.flatMap((file) => ["-f", file])];
        const rendered = Bun.spawnSync({ cmd, stdout: "pipe", stderr: "pipe", timeout: COMMAND_TIMEOUT_MS });
        throwIfTimedOut(cmd, rendered);
        expect(rendered.stderr.toString()).toBe("");
        expect(rendered.exitCode, "helm template exit code").toBe(0);
        return parseManifests(rendered.stdout.toString());
      };
      let waiting: K8sObject[];
      let signing: K8sObject[];
      try {
        const write = (name: string, values: unknown) => {
          const file = join(work, name);
          writeFileSync(file, Bun.YAML.stringify(values, null, 2));
          return file;
        };
        const api = write("api.yaml", apiValues(shape.name, routesOfValues(CHART_DIR, shape.valuesFiles)));
        const root = write("root.yaml", rootValues(fx.root));
        const issuingFile = write("issuing.yaml", issuingValues(fx.issuing));
        waiting = render([api, root]);
        signing = render([api, root, issuingFile]);
      } finally {
        rmSync(work, { recursive: true, force: true });
      }
      const certificates = (objects: K8sObject[]) => objects.find((o) => o.kind === "ConfigMap" && o.metadata?.name === "trustiot-certs-ca-certificates")?.data as Record<string, string>;
      expect(pemBlocks(certificates(waiting)["root.pem"]!, "CERTIFICATE")).toHaveLength(1);
      expect(certificateOf(certificates(waiting)["root.pem"]!).fingerprint256).toBe(certificateOf(fx.root).fingerprint256);
      expect(certificates(waiting)["issuing.pem"]!.trim()).toBe("");
      expect(certificateOf(certificates(signing)["issuing.pem"]!).fingerprint256).toBe(certificateOf(fx.issuing).fingerprint256);
      // The certs pods do not restart: nothing in their Deployment depends on the certificates.
      const deployment = (objects: K8sObject[]) => objects.find((o) => o.kind === "Deployment" && o.metadata?.name === "trustiot-certs");
      expect(deployment(signing)).toEqual(deployment(waiting)!);
      // What the check reads: the key objects, and where it fetches inside the mesh.
      const objects = certsObjectsOf(signing);
      expect(objects).toMatchObject({ secret: `${CERTS}-ca-keys`, configMap: `${CERTS}-ca-requests`, generation: "1" });
      expect(signing.find((o) => o.kind === "Secret" && o.metadata?.name === objects.secret)).toBeDefined();
      const fetcher = meshFetcherOf(signing);
      expect(fetcher).toEqual({ pod: "trustiot-mqtt-broker-0", container: "mosquitto", certsUrl: `http://trustiot-certs.nightly-${shape.name}.svc:8080` });
    }, SLOW_TEST_MS);
  }
});
