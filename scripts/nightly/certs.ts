// The smoke's check of the certs service (Laysi/trustiot-v2#517; services/certs/README.md,
// 「發簽發 CA 憑證」): the release runs the certs service built from the commit, and the smoke plays
// the operators' root ceremony against it with a root CA made for this run alone.
//
// 1. Before the install, `certs-ca root create` (built with the certs image, image-cache.ts) makes
//    the test root; its certificate goes into certificateAuthority.rootCertificate.
// 2. Waiting: the certs pod's init container generated the issuing CA key into the Secret the chart
//    declares (bound to itself, nothing else in it) and wrote the key's certificate signing request
//    into the requests ConfigMap; inside the mesh (from the MQTT broker's pod, an authorized
//    caller) ca.pem is the root alone and crl.pem is empty; through the ingress the workstation
//    certificate API answers 401 without a cookie, 403 to a tenant administrator and 503
//    workstation_certificate.ca_not_ready to a platform administrator: the CA cannot sign yet.
// 3. `certs-ca root sign` signs the request from the ConfigMap; the key id it prints must be the
//    one of the request's key; helm upgrade sets the certificate in issuingCertificates.
// 4. Signing: without a restart of the certs pod, ca.pem inside the mesh becomes the root and the
//    issuing CA, crl.pem one CRL that the issuing CA signed (not the root), and the platform
//    administrator gets a workstation certificate (201) that the issuing CA signed, with the key id
//    certs-ca printed, then revokes it (200, and 409 the second time); without a cookie and as a
//    tenant administrator both calls answer 401 and 403.
//
// The metric certs.ca.signing (0 while waiting, 1 once the certificate is set) is not readable in
// the smoke: the collector's metrics pipeline exports nowhere until Laysi/trustiot-v2#183, and the
// nightly reads no pod log (diagnostics.test.ts). What the gauge reports is asserted through its
// effects instead: ca.pem and the workstation API's 503 ca_not_ready, then 201.
//
// The root private key never touches a disk: `root create` prints it to a pipe, it stays in this
// process's memory (TestRoot), `root sign` reads it from a pipe, and TestRoot drops it after that
// one signature, or when the shape ends. Neither command's standard output reaches the run log
// (exec.ts, quiet). The certificates, the request and the CRL are public. Dropping is not erasing:
// JavaScript strings cannot be zeroed, so copies of the key (the command's result, this object's
// field, the stdin buffer) stay in this process's heap until it is collected or the job ends. That
// is accepted: the runner is thrown away after the job, which holds no secret, and the root lives
// 7 days and is trusted only by the throwaway cluster. certs-ca zeroes its own copies.
import { X509Certificate, createHash } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { ConnectionError } from "../../deploy/chart/tests/e2e/raw-client.ts";
import type { RawRequest, RawResponse } from "../../deploy/chart/tests/e2e/raw-client.ts";
import { adminSessionSql, errorCode, ingressAccess, memberSessionSql, newSession, SESSION_COOKIE, TENANTS_PATH } from "./api.ts";
import type { ApiCheckContext, TestSession } from "./api.ts";
import { exec, run } from "./exec.ts";
import type { ExecOptions, ExecResult } from "./exec.ts";
import { SECRET_SUMMARY_TEMPLATE, secretSummaryProblems } from "./identity-keys.ts";
import type { K8sObject } from "./manifests.ts";

/** The offline root ceremony's program (services/certs/src/bin/certs-ca.rs). */
export const CERTS_CA = "certs-ca";
/** The certs pod's init container that generates the issuing CA keys (deploy/chart/templates/_certs.tpl). */
export const CA_KEYS_CONTAINER = "ca-keys";
/** The key of the issuing CA keys Secret (_certs.tpl, "trustiot.certs.ca"). */
export const CA_KEYS_KEY = "issuing-ca-keys.json";
/** The request of a key generation in the requests ConfigMap (crates/ca-keys). */
export const requestKeyOf = (generation: string): string => `issuing-ca-generation-${generation}.csr.pem`;
/** The certs service's public endpoints (services/certs/src/http.rs). */
export const CHAIN_PATH = "/internal/certs/v1/ca.pem";
export const CRL_PATH = "/internal/certs/v1/crl.pem";
/** The workstation certificate API (services/certs/src/workstations.rs). */
export const WORKSTATION_CERTIFICATES_PATH = "/api/certs/v1/workstation-certificates";
export const CA_NOT_READY = "workstation_certificate.ca_not_ready";
export const ALREADY_REVOKED = "workstation_certificate.already_revoked";
/** The workstation the smoke issues a certificate for. */
export const WORKSTATION = "nightly-smoke";
/** The MQTT broker's container the smoke fetches from: it has BusyBox wget, and the pod may call certs. */
export const BROKER_CONTAINER = "mosquitto";
/** The broker's setting that names the certs service (deploy/chart/templates/_mqtt-broker.tpl). */
export const CERTS_URL_ENV = "TRUSTIOT__TRUST__CERTS_URL";

/** Validity of the test root and of the issuing CA it signs: the run takes an hour at most. */
export const ROOT_DAYS = 7;
export const ISSUING_DAYS = 2;
/** Common names without customer or host names (the README's rule); a different one per shape. */
export const rootCommonName = (shape: string): string => `TrustIoT nightly test root CA ${shape}`;
export const issuingCommonName = (shape: string): string => `TrustIoT nightly issuing CA G1 ${shape}`;

/**
 * How long the checks wait: for the ingress to route the certs API; for the certs service to load
 * the issuing certificate after the upgrade (the kubelet updates a mounted ConfigMap within about
 * a minute or two, the service reads the files again every 30 s); for the CRL job's first CRL of
 * the new CA (every 10 s in the smoke, api.ts CERTS_SMOKE_ENV); and for it to sign.
 */
export const ROUTE_LIMIT_MS = 180_000;
export const RELOAD_LIMIT_MS = 420_000;
export const CRL_LIMIT_MS = 180_000;
export const SIGNER_LIMIT_MS = 180_000;
const POLL_MS = 3_000;

type Map_ = Record<string, unknown>;
const isMap = (value: unknown): value is Map_ => value !== null && typeof value === "object" && !Array.isArray(value);
const asMap = (value: unknown): Map_ => (isMap(value) ? value : {});
const asArray = (value: unknown): unknown[] => (Array.isArray(value) ? value : []);
const asString = (value: unknown): string | undefined => (typeof value === "string" ? value : undefined);

// ---------------------------------------------------------------------------------------------
// PEM and certificates.

/** The DER of every `label` block in `text`; throws when anything else is in it. */
export function pemBlocks(text: string, label: string): Buffer[] {
  const pattern = new RegExp(`-----BEGIN ${label}-----\\r?\\n([A-Za-z0-9+/=\\r\\n]+?)-----END ${label}-----`, "g");
  const blocks = [...text.matchAll(pattern)].map((match) => Buffer.from(match[1]!.replace(/\s/g, ""), "base64"));
  const rest = text.replace(pattern, "").trim();
  if (rest !== "") throw new Error(`text outside the ${label} blocks: ${JSON.stringify(rest.slice(0, 80))}`);
  return blocks;
}

/** The one certificate of `pem` as the values take it: its DER in base64 on one line. */
export function certificateValue(pem: string): string {
  const blocks = pemBlocks(pem, "CERTIFICATE");
  if (blocks.length !== 1) throw new Error(`expected one certificate, found ${blocks.length}`);
  const value = blocks[0]!.toString("base64");
  // The chart's own rule (_certs.tpl, "trustiot.certs.checkCertificates").
  if (!/^MII[A-Za-z0-9+/]+={0,2}$/.test(value)) throw new Error("the certificate is not a DER SEQUENCE of 256 bytes to 64 KiB");
  return value;
}

/** A certificate of `pem`, parsed. */
export function certificateOf(pem: string): X509Certificate {
  const blocks = pemBlocks(pem, "CERTIFICATE");
  if (blocks.length !== 1) throw new Error(`expected one certificate, found ${blocks.length}`);
  return new X509Certificate(blocks[0]!);
}

/** The key id of a public key (crates/ca-keys, key_id): SHA-256 of its SubjectPublicKeyInfo, hex. */
export function keyIdOfPublicKeyPem(pem: string): string {
  const blocks = pemBlocks(pem, "PUBLIC KEY");
  if (blocks.length !== 1) throw new Error(`expected one public key, found ${blocks.length}`);
  return createHash("sha256").update(blocks[0]!).digest("hex");
}

/** The key id `certs-ca root sign` printed on stderr ("... for key <id>; ..."). */
export function signedKeyIdOf(stderr: string): string {
  const ids = [...stderr.matchAll(/\bfor key ([0-9a-f]{64})\b/g)].map((match) => match[1]!);
  if (ids.length !== 1) throw new Error(`${CERTS_CA} root sign printed ${ids.length} key ids, expected one`);
  return ids[0]!;
}

/** Why `body` (ca.pem) is not exactly the certificates `expected` (PEM), in order; empty when it is. */
export function chainProblems(body: string, expected: string[]): string[] {
  let got: Buffer[];
  try {
    got = pemBlocks(body, "CERTIFICATE");
  } catch (error) {
    return [`ca.pem: ${error instanceof Error ? error.message : String(error)}`];
  }
  const want = expected.map((pem) => pemBlocks(pem, "CERTIFICATE")[0]!);
  if (got.length !== want.length) return [`ca.pem holds ${got.length} certificate(s), expected ${want.length}`];
  return got.flatMap((der, index) => (der.equals(want[index]!) ? [] : [`ca.pem: certificate ${index + 1} is not the expected one`]));
}

// ---------------------------------------------------------------------------------------------
// The release's objects.

/** The certs Deployment and its CA key objects, from the init container's settings. */
export interface CertsObjects {
  deployment: string;
  /** The Deployment's pod selector, as `kubectl --selector` takes it. */
  selector: string;
  secret: string;
  configMap: string;
  generation: string;
}

export function certsObjectsOf(objects: K8sObject[]): CertsObjects {
  const deployments = objects.filter((o) => o.kind === "Deployment" && o.metadata?.labels?.["app.kubernetes.io/component"] === "certs");
  if (deployments.length !== 1) throw new Error(`the release should have one certs Deployment, found ${deployments.length}`);
  const deployment = deployments[0]!;
  const labels = asMap(asMap(asMap(deployment.spec).selector).matchLabels);
  const selector = Object.entries(labels)
    .filter((entry): entry is [string, string] => typeof entry[1] === "string")
    .map(([key, value]) => `${key}=${value}`)
    .join(",");
  if (selector === "") throw new Error("the certs Deployment has no selector labels");
  const spec = asMap(asMap(asMap(deployment.spec).template).spec);
  const init = asArray(spec.initContainers).filter(isMap).find((c) => c.name === CA_KEYS_CONTAINER);
  if (init === undefined) throw new Error(`the certs Deployment has no ${CA_KEYS_CONTAINER} init container`);
  const env = asArray(init.env).filter(isMap);
  const value = (name: string): string => {
    const found = asString(env.find((e) => e.name === name)?.value);
    if (found === undefined || found === "") throw new Error(`the ${CA_KEYS_CONTAINER} init container has no ${name}`);
    return found;
  };
  const generation = value("TRUSTIOT__KEYS__GENERATION");
  if (!/^[0-9]{1,9}$/.test(generation)) throw new Error(`the ${CA_KEYS_CONTAINER} init container's generation ${JSON.stringify(generation)} is not a number`);
  return {
    deployment: deployment.metadata?.name ?? "",
    selector,
    secret: value("TRUSTIOT__KEYS__SECRET"),
    configMap: value("TRUSTIOT__KEYS__CONFIG_MAP"),
    generation,
  };
}

/** Where the smoke fetches the certs service's public files inside the mesh: the broker's pod. */
export interface MeshFetcher {
  pod: string;
  container: string;
  /** The certs service's address the broker itself uses. */
  certsUrl: string;
}

export function meshFetcherOf(objects: K8sObject[]): MeshFetcher {
  const sets = objects.filter((o) => o.kind === "StatefulSet" && o.metadata?.labels?.["app.kubernetes.io/component"] === "mqtt-broker");
  if (sets.length !== 1) throw new Error(`the release should have one MQTT broker StatefulSet, found ${sets.length}; the certs check fetches from its pod`);
  const set = sets[0]!;
  const spec = asMap(asMap(asMap(set.spec).template).spec);
  const containers = [...asArray(spec.initContainers), ...asArray(spec.containers)].filter(isMap);
  if (!containers.some((c) => c.name === BROKER_CONTAINER)) throw new Error(`the MQTT broker has no ${BROKER_CONTAINER} container`);
  const urls = new Set(
    containers.flatMap((c) => asArray(c.env).filter(isMap)).filter((e) => e.name === CERTS_URL_ENV).map((e) => asString(e.value) ?? ""),
  );
  if (urls.size !== 1) throw new Error(`the MQTT broker's containers set ${urls.size} values of ${CERTS_URL_ENV}, expected one`);
  const certsUrl = [...urls][0]!;
  if (!/^http:\/\/[a-z0-9.-]+:[0-9]{1,5}$/.test(certsUrl)) throw new Error(`${CERTS_URL_ENV} ${JSON.stringify(certsUrl)} is not http://<host>:<port>`);
  return { pod: `${set.metadata?.name ?? ""}-0`, container: BROKER_CONTAINER, certsUrl };
}

/** A certs pod: its uid (a restart of the Deployment makes new pods) and how its init container ended. */
export interface CertsPod {
  name: string;
  uid: string;
  /** The exit code of the ca-keys init container; undefined while it has not ended. */
  caKeysExit?: number;
}

export function certsPodsOf(podList: unknown): CertsPod[] {
  return asArray(asMap(podList).items)
    .filter(isMap)
    .map((pod) => {
      const metadata = asMap(pod.metadata);
      const init = asArray(asMap(pod.status).initContainerStatuses)
        .filter(isMap)
        .find((status) => status.name === CA_KEYS_CONTAINER);
      const exit = asMap(asMap(init?.state).terminated).exitCode;
      return { name: asString(metadata.name) ?? "", uid: asString(metadata.uid) ?? "", ...(typeof exit === "number" ? { caKeysExit: exit } : {}) };
    })
    .sort((a, b) => a.name.localeCompare(b.name));
}

/** The request of `generation` in the requests ConfigMap (`kubectl get configmap -o json`), and why it is not one. */
export function requestOf(name: string, configMap: unknown, generation: string): { request?: string; problems: string[] } {
  const data = asMap(asMap(configMap).data);
  const key = requestKeyOf(generation);
  const keys = Object.keys(data);
  const problems: string[] = [];
  if (keys.join(",") !== key) problems.push(`${name}: holds the keys [${keys.join(", ")}], expected only ${key}`);
  const request = asString(data[key]);
  if (request === undefined) return { problems: [...problems, `${name}: no request of generation ${generation}`] };
  try {
    const blocks = pemBlocks(request, "CERTIFICATE REQUEST");
    if (blocks.length !== 1) problems.push(`${name}: ${key} holds ${blocks.length} requests, expected one`);
  } catch (error) {
    problems.push(`${name}: ${key}: ${error instanceof Error ? error.message : String(error)}`);
  }
  return problems.length > 0 ? { problems } : { request, problems };
}

// ---------------------------------------------------------------------------------------------
// The test root CA.

/** Runs a command; the smoke passes exec.ts's `run` with its log. */
export type Runner = (cmd: string[], options: ExecOptions) => Promise<ExecResult>;

const PRIVATE_KEY_PEM = /^-----BEGIN PRIVATE KEY-----\r?\n[A-Za-z0-9+/=\r\n]+-----END PRIVATE KEY-----\r?\n?$/;

/**
 * The run's root CA: its certificate (public) and its private key, which lives only in this
 * object, never on a disk or in the log, and signs one issuing CA: `sign` drops its reference, and
 * so does `discard`, which the smoke calls when the shape ends either way (dropped, not erased:
 * see the top of this file).
 */
export class TestRoot {
  #key: string | undefined;

  private constructor(
    readonly commonName: string,
    /** The root certificate, PEM. */
    readonly certificate: string,
    key: string,
  ) {
    this.#key = key;
  }

  /** Makes a root with `certs-ca root create`; its certificate file is in a private directory that is removed right after. */
  static async create(runner: Runner, certsCa: string, commonName: string): Promise<TestRoot> {
    const dir = mkdtempSync(join(tmpdir(), "nightly-root-ca-"));
    try {
      const file = join(dir, "root.pem");
      const made = await runner([certsCa, "root", "create", "--common-name", commonName, "--days", String(ROOT_DAYS), "--certificate", file], {
        timeoutMs: 60_000,
        quiet: true,
      });
      // Never in a message: only whether it has the shape of one key.
      if (!PRIVATE_KEY_PEM.test(made.stdout)) throw new Error(`${CERTS_CA} root create printed something other than one PKCS#8 PEM private key`);
      const certificate = readFileSync(file, "utf8");
      certificateValue(certificate);
      return new TestRoot(commonName, certificate, made.stdout);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  }

  /** Whether the key is still here. */
  get holdsKey(): boolean {
    return this.#key !== undefined;
  }

  /**
   * Signs the issuing CA certificate for `request` with `certs-ca root sign`, the key on its
   * standard input; returns it and the key id certs-ca printed. The key is dropped first, so a
   * failure cannot leave it for a second attempt.
   */
  async sign(runner: Runner, certsCa: string, request: string, commonName: string): Promise<{ certificate: string; keyId: string }> {
    const key = this.#key;
    this.#key = undefined;
    if (key === undefined) throw new Error("the test root's key was already used or discarded");
    const dir = mkdtempSync(join(tmpdir(), "nightly-issuing-ca-"));
    try {
      const file = (name: string) => join(dir, name);
      writeFileSync(file("root.pem"), this.certificate);
      writeFileSync(file("request.pem"), request);
      const signed = await runner(
        [
          certsCa, "root", "sign",
          "--root-certificate", file("root.pem"),
          "--request", file("request.pem"),
          "--common-name", commonName,
          "--days", String(ISSUING_DAYS),
          "--certificate", file("issuing.pem"),
        ],
        { stdin: key, timeoutMs: 60_000, quiet: true },
      );
      const certificate = readFileSync(file("issuing.pem"), "utf8");
      certificateValue(certificate);
      return { certificate, keyId: signedKeyIdOf(signed.stderr) };
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  }

  discard(): void {
    this.#key = undefined;
  }
}

/** The values that set the root certificate (services/certs/README.md, 「發簽發 CA 憑證」 step 2). */
export function rootValues(root: string): Record<string, unknown> {
  return { certificateAuthority: { rootCertificate: certificateValue(root) } };
}

/** The values that set the issuing CA certificate (step 5); helm keeps the root from the earlier file. */
export function issuingValues(issuing: string): Record<string, unknown> {
  return { certificateAuthority: { issuingCertificates: [certificateValue(issuing)] } };
}

// ---------------------------------------------------------------------------------------------
// Inside the mesh.

/** One fetch inside the mesh: 200 with its body, or what went wrong. */
export interface MeshAnswer {
  ok: boolean;
  /** The HTTP status wget reported for an error answer; undefined for 200 or when none was received. */
  status?: number;
  body: string;
  /** For a failure: what wget or kubectl said. */
  detail: string;
}

/** The answer of `kubectl exec <broker pod> -- wget -q -O - <url>` (BusyBox wget: exit 1 and the status on stderr). */
export function meshAnswerOf(result: ExecResult): MeshAnswer {
  if (result.code === 0) return { ok: true, body: result.stdout, detail: "" };
  const status = /server returned error: HTTP\/1\.[01] ([0-9]{3})\b/.exec(result.stderr)?.[1];
  const detail = result.timedOut ? "timed out" : (result.stderr.trim().split("\n").at(-1) ?? "").slice(0, 200);
  return { ok: false, ...(status === undefined ? {} : { status: Number(status) }), body: "", detail };
}

/** What the CRL check learns from openssl. */
export interface CrlVerdict {
  /** The CRL's signature verifies with the CA's key. */
  verified: boolean;
  /** The CRL's issuer, RFC 2253. */
  issuer: string;
}

/** Verifies `crl` (one PEM CRL) against the CA certificate `ca` with openssl, in a temporary directory. */
export async function opensslCrlVerdict(crl: string, ca: string): Promise<CrlVerdict> {
  const dir = mkdtempSync(join(tmpdir(), "nightly-crl-"));
  try {
    writeFileSync(join(dir, "crl.pem"), crl);
    writeFileSync(join(dir, "ca.pem"), ca);
    const verified = await exec(["openssl", "crl", "-in", join(dir, "crl.pem"), "-CAfile", join(dir, "ca.pem"), "-verify", "-noout"], { timeoutMs: 30_000 });
    const issuer = await exec(["openssl", "crl", "-in", join(dir, "crl.pem"), "-noout", "-issuer", "-nameopt", "RFC2253"], { timeoutMs: 30_000 });
    if (issuer.code !== 0) throw new Error(`openssl crl -issuer failed: ${issuer.stderr.trim().slice(0, 200)}`);
    return {
      verified: verified.code === 0 && /verify OK/.test(`${verified.stdout}${verified.stderr}`),
      issuer: issuer.stdout.trim().replace(/^issuer=\s*/, ""),
    };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

/** The workstation's request the API checks send, and the id of its key. */
export interface WorkstationRequest {
  request: string;
  keyId: string;
}

/** A workstation's certificate signing request (ECDSA P-256), made with openssl; its key is thrown away. */
export async function workstationRequest(runner: Runner): Promise<string> {
  const dir = mkdtempSync(join(tmpdir(), "nightly-workstation-"));
  try {
    await runner(
      [
        "openssl", "req", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-sha256",
        "-subj", `/CN=workstation:${WORKSTATION}`, "-keyout", join(dir, "key.pem"), "-out", join(dir, "request.pem"),
      ],
      { timeoutMs: 30_000 },
    );
    return readFileSync(join(dir, "request.pem"), "utf8");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

/** The key id of the key `request` (PEM CSR) is for, once openssl verified the request's signature. */
export async function requestKeyId(runner: Runner, request: string): Promise<string> {
  const dir = mkdtempSync(join(tmpdir(), "nightly-request-"));
  try {
    writeFileSync(join(dir, "request.pem"), request);
    const checked = await runner(["openssl", "req", "-in", join(dir, "request.pem"), "-noout", "-verify", "-pubkey"], { timeoutMs: 30_000, quiet: true });
    const output = `${checked.stdout}\n${checked.stderr}`;
    if (!/verify OK/i.test(output)) throw new Error("openssl did not verify the request's signature");
    const key = /-----BEGIN PUBLIC KEY-----[\s\S]+?-----END PUBLIC KEY-----/.exec(checked.stdout)?.[0];
    if (key === undefined) throw new Error("openssl printed no public key for the request");
    return keyIdOfPublicKeyPem(key);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

// ---------------------------------------------------------------------------------------------
// Polling.

interface Clock {
  sleep?: (ms: number) => Promise<void>;
  now?: () => number;
}

/** One poll's verdict: done with a value, pending with what was seen, or a failure. */
export type Poll<T> = { done: T } | { pending: string } | { fail: string };

/** Polls `attempt` until it is done or fails, or `limitMs` passes; the error names `what` and the last answer. */
export async function pollUntil<T>(what: string, limitMs: number, pollMs: number, clock: Clock, attempt: () => Promise<Poll<T>>): Promise<{ value: T; waitedMs: number }> {
  const sleep = clock.sleep ?? Bun.sleep;
  const now = clock.now ?? Date.now;
  const started = now();
  let last = "nothing yet";
  for (;;) {
    const verdict = await attempt();
    if ("done" in verdict) return { value: verdict.done, waitedMs: now() - started };
    if ("fail" in verdict) throw new Error(`${what}: ${verdict.fail}`);
    last = verdict.pending;
    if (now() - started >= limitMs) throw new Error(`timed out after ${Math.round(limitMs / 1000)} s waiting for ${what}; last: ${last}`);
    await sleep(pollMs);
  }
}

// ---------------------------------------------------------------------------------------------
// Inside the mesh: ca.pem and crl.pem.

export interface MeshDeps extends Clock {
  /** Fetches a path of the certs service from inside the mesh. */
  fetch: (path: string) => Promise<MeshAnswer>;
  /** Checks a CRL against a CA certificate (opensslCrlVerdict). */
  crlVerdict: (crl: string, ca: string) => Promise<CrlVerdict>;
  log: (line: string) => void;
  limits?: { routeMs?: number; reloadMs?: number; crlMs?: number; pollMs?: number };
}

const answerSummary = (answer: MeshAnswer): string => (answer.ok ? `200, ${answer.body.length} bytes` : `${answer.status ?? "no answer"} ${answer.detail}`);

/** While the CA has the root only: ca.pem is the root, crl.pem empty. Returns the report part. */
export async function checkMeshWaiting(deps: MeshDeps, root: string): Promise<string> {
  const pollMs = deps.limits?.pollMs ?? POLL_MS;
  // The broker's pod reached the service already (its init container); still, allow for the
  // mesh's policies on a loaded runner.
  const chain = await pollUntil<string>(`ca.pem inside the mesh (200)`, deps.limits?.routeMs ?? ROUTE_LIMIT_MS, pollMs, deps, async () => {
    const answer = await deps.fetch(CHAIN_PATH);
    return answer.ok ? { done: answer.body } : { pending: answerSummary(answer) };
  });
  const problems = chainProblems(chain.value, [root]);
  if (problems.length > 0) throw new Error(`before the issuing CA certificate is set, ${problems.join("; ")} (expected the test root alone)`);
  const crl = await deps.fetch(CRL_PATH);
  if (!crl.ok) throw new Error(`crl.pem inside the mesh answered ${answerSummary(crl)}; with no issuing CA it is 200 and empty`);
  if (crl.body.trim() !== "") throw new Error(`crl.pem inside the mesh holds ${crl.body.length} bytes before any issuing CA exists; expected none`);
  return "inside the mesh ca.pem is the test root alone and crl.pem is empty";
}

/**
 * Once the issuing CA certificate is set: ca.pem becomes the root and the issuing CA, and crl.pem
 * one CRL that verifies with the issuing CA's key and not with the root's, issued by the issuing
 * CA's name. Returns the report part.
 */
export async function checkMeshSigning(deps: MeshDeps, root: string, issuing: string): Promise<string> {
  const pollMs = deps.limits?.pollMs ?? POLL_MS;
  const reloaded = await pollUntil<true>(
    "the certs service to serve the issuing CA in ca.pem inside the mesh (the kubelet updates the ConfigMap, the service reads it again)",
    deps.limits?.reloadMs ?? RELOAD_LIMIT_MS,
    pollMs,
    deps,
    async () => {
      const answer = await deps.fetch(CHAIN_PATH);
      if (!answer.ok) return { pending: answerSummary(answer) };
      if (chainProblems(answer.body, [root]).length === 0) return { pending: "ca.pem is still the root alone" };
      const problems = chainProblems(answer.body, [root, issuing]);
      return problems.length === 0 ? { done: true } : { fail: problems.join("; ") };
    },
  );
  deps.log(`ca.pem inside the mesh is the root and the issuing CA after ${Math.round(reloaded.waitedMs / 1000)} s`);
  // 503 while the new CA has no CRL yet (services/certs/src/http.rs).
  const crl = await pollUntil<string>(`the first CRL of the issuing CA in crl.pem inside the mesh`, deps.limits?.crlMs ?? CRL_LIMIT_MS, pollMs, deps, async () => {
    const answer = await deps.fetch(CRL_PATH);
    if (!answer.ok) return answer.status === 503 ? { pending: answerSummary(answer) } : { fail: answerSummary(answer) };
    let blocks: Buffer[];
    try {
      blocks = pemBlocks(answer.body, "X509 CRL");
    } catch (error) {
      return { fail: error instanceof Error ? error.message : String(error) };
    }
    if (blocks.length === 0) return { pending: "crl.pem is empty" };
    return blocks.length === 1 ? { done: answer.body } : { fail: `crl.pem holds ${blocks.length} CRLs, expected one` };
  });
  const byIssuing = await deps.crlVerdict(crl.value, issuing);
  const byRoot = await deps.crlVerdict(crl.value, root);
  const subject = certificateOf(issuing).subject;
  const problems: string[] = [];
  if (!byIssuing.verified) problems.push("the CRL's signature does not verify with the issuing CA's key");
  if (byRoot.verified) problems.push("the CRL verifies with the root's key; the issuing CA must sign it");
  if (byIssuing.issuer !== subject) problems.push(`the CRL's issuer is ${JSON.stringify(byIssuing.issuer)}, not the issuing CA ${JSON.stringify(subject)}`);
  if (problems.length > 0) throw new Error(problems.join("; "));
  return (
    `inside the mesh ca.pem became the root and the issuing CA after ${Math.round(reloaded.waitedMs / 1000)} s, ` +
    `crl.pem one CRL issued by ${subject} after ${Math.round(crl.waitedMs / 1000)} s more, whose signature verifies with the issuing CA's key and not the root's`
  );
}

// ---------------------------------------------------------------------------------------------
// Through the ingress: the workstation certificate API.

export interface WorkstationDeps extends Clock {
  /** Sends one request to the ingress gateway. */
  send: (request: RawRequest) => Promise<RawResponse>;
  /** Runs SQL as the database superuser. */
  psql: (sql: string) => Promise<void>;
  log: (line: string) => void;
  limits?: { routeMs?: number; signerMs?: number; pollMs?: number };
}

/** The sessions the API check uses, made while waiting and used again once the CA signs. */
export interface WorkstationActors {
  tenantId: string;
  platformAdmin: TestSession;
  /** An administrator of the tenant, who is no platform administrator. */
  tenantAdmin: TestSession;
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

function summary(response: RawResponse): string {
  return `${response.status} ${response.body.slice(0, 200).replaceAll("\n", " ")}`;
}

/** The `message_key` of an ErrorBody, or undefined. */
export function messageKey(body: string): string | undefined {
  try {
    const parsed: unknown = JSON.parse(body);
    return asString(asMap(parsed).message_key);
  } catch {
    return undefined;
  }
}

const isError = (response: RawResponse, status: number, code: string, key?: string): boolean =>
  response.status === status && errorCode(response.body) === code && (key === undefined || messageKey(response.body) === key);

/** A JSON POST to the ingress, with `session`'s cookie unless none. */
function post(appHost: string, target: string, body: unknown, session?: TestSession): RawRequest {
  return {
    target,
    method: "POST",
    hostHeader: appHost,
    headers: [...(session === undefined ? [] : [["cookie", `${SESSION_COOKIE}=${session.cookie}`] as [string, string]]), ["content-type", "application/json"]],
    body: JSON.stringify(body),
  };
}

/** The issue request's body. */
export const issueBody = (request: string): Record<string, string> => ({ workstation: WORKSTATION, certificate_request: request });

/**
 * While the CA cannot sign: a platform administrator creates a tenant and the check makes an
 * administrator of it; the issue API answers 403 to the tenant administrator (awaited: the route),
 * 401 without a cookie, and 503 ca_not_ready to the platform administrator.
 */
export async function checkWorkstationWaiting(
  deps: WorkstationDeps,
  appHost: string,
  request: string,
  tenantName: string,
): Promise<{ detail: string; actors: WorkstationActors }> {
  const pollMs = deps.limits?.pollMs ?? POLL_MS;
  const platformAdmin = newSession();
  await deps.psql(adminSessionSql(platformAdmin));
  const created = await deps.send(post(appHost, TENANTS_PATH, { name: tenantName }, platformAdmin));
  if (created.status !== 201) throw new Error(`creating a tenant answered ${summary(created)}, expected 201`);
  const tenantId = asString(asMap(JSON.parse(created.body)).tenant_id);
  if (tenantId === undefined || !UUID.test(tenantId)) throw new Error(`creating a tenant answered 201 with ${created.body.slice(0, 200)}`);
  const tenantAdmin = newSession();
  await deps.psql(memberSessionSql(tenantAdmin, tenantId, "admin"));
  deps.log(`made a platform administrator ${platformAdmin.userId} and an administrator ${tenantAdmin.userId} of tenant ${tenantId}`);

  const body = issueBody(request);
  // The certs service's own 403 proves the route and the token; Istio's or a missing route's
  // answers carry no ErrorBody. A refused request issues nothing, so it may be sent again.
  const routed = await pollUntil<true>(
    "the ingress to pass the tenant administrator's issue request to the certs service (403 permission_denied)",
    deps.limits?.routeMs ?? ROUTE_LIMIT_MS,
    pollMs,
    deps,
    async () => {
      try {
        const response = await deps.send(post(appHost, WORKSTATION_CERTIFICATES_PATH, body, tenantAdmin));
        return isError(response, 403, "permission_denied") ? { done: true } : { pending: summary(response) };
      } catch (error) {
        if (error instanceof ConnectionError) return { pending: error.message };
        throw error;
      }
    },
  );
  const anonymous = await deps.send(post(appHost, WORKSTATION_CERTIFICATES_PATH, body));
  if (!isError(anonymous, 401, "unauthenticated")) throw new Error(`issuing without a cookie answered ${summary(anonymous)}, expected 401 unauthenticated`);
  const waiting = await deps.send(post(appHost, WORKSTATION_CERTIFICATES_PATH, body, platformAdmin));
  if (!isError(waiting, 503, "unavailable", CA_NOT_READY)) {
    throw new Error(`issuing as a platform administrator before the issuing CA certificate is set answered ${summary(waiting)}, expected 503 ${CA_NOT_READY}`);
  }
  const detail =
    `POST ${WORKSTATION_CERTIFICATES_PATH} (routed after ${Math.round(routed.waitedMs / 1000)} s): without a cookie 401 unauthenticated, ` +
    `as a tenant administrator 403 permission_denied, as a platform administrator 503 ${CA_NOT_READY}`;
  return { detail, actors: { tenantId, platformAdmin, tenantAdmin } };
}

/** What the issue response holds (contract::certs::WorkstationCertificate). */
interface IssuedView {
  serial: string;
  certificate: string;
}

/** The key id of a certificate's public key (keyIdOfPublicKeyPem). */
export function certificateKeyId(certificate: X509Certificate): string {
  return createHash("sha256").update(certificate.publicKey.export({ type: "spki", format: "der" })).digest("hex");
}

/**
 * Why an issue response's body is not the certificate the issuing CA signed for the request whose
 * key has id `requestKeyId`; empty when it is.
 */
export function issuedProblems(body: string, issuing: { certificate: string; keyId: string }, requestKeyId: string): { issued?: IssuedView; problems: string[] } {
  let view: Map_;
  try {
    view = asMap(JSON.parse(body));
  } catch {
    return { problems: ["the issue response is not JSON"] };
  }
  const problems: string[] = [];
  const serial = asString(view.serial) ?? "";
  if (!/^[0-9a-fA-F]{1,64}$/.test(serial)) problems.push(`serial ${JSON.stringify(serial)} is not hex`);
  if (view.workstation !== WORKSTATION) problems.push(`workstation is ${JSON.stringify(view.workstation)}, not ${WORKSTATION}`);
  if (view.subject !== `workstation:${WORKSTATION}`) problems.push(`subject is ${JSON.stringify(view.subject)}`);
  if (view.ca_key_id !== issuing.keyId) problems.push(`ca_key_id is ${JSON.stringify(view.ca_key_id)}, not the key ${issuing.keyId} the test root certified`);
  if (view.revoked_at !== undefined && view.revoked_at !== null) problems.push("the new certificate is revoked");
  const certificate = asString(view.certificate) ?? "";
  try {
    const issued = certificateOf(certificate);
    const ca = certificateOf(issuing.certificate);
    if (!issued.checkIssued(ca) || !issued.verify(ca.publicKey)) problems.push("the certificate is not signed by the issuing CA");
    if (issued.subject !== `CN=workstation:${WORKSTATION}`) problems.push(`the certificate's subject is ${JSON.stringify(issued.subject)}`);
    if (certificateKeyId(issued) !== requestKeyId) problems.push("the certificate is not for the request's key");
    if (BigInt(`0x${issued.serialNumber}`) !== BigInt(`0x${serial || "0"}`)) problems.push("the certificate's serial number is not the response's serial");
  } catch (error) {
    problems.push(`the certificate: ${error instanceof Error ? error.message : String(error)}`);
  }
  return problems.length > 0 ? { problems } : { issued: { serial, certificate }, problems };
}

/**
 * Once the issuing CA signs: issue answers 401 without a cookie, 403 to the tenant administrator
 * and 201 to the platform administrator (awaited while it answers 503 ca_not_ready, which issues
 * nothing) with a certificate the issuing CA signed for the workstation's key; revoking it answers
 * 401, 403, then 200, and 409 already_revoked the second time.
 */
export async function checkWorkstationSigning(
  deps: WorkstationDeps,
  appHost: string,
  workstation: WorkstationRequest,
  actors: WorkstationActors,
  issuing: { certificate: string; keyId: string },
): Promise<string> {
  const pollMs = deps.limits?.pollMs ?? POLL_MS;
  const body = issueBody(workstation.request);
  const anonymous = await deps.send(post(appHost, WORKSTATION_CERTIFICATES_PATH, body));
  if (!isError(anonymous, 401, "unauthenticated")) throw new Error(`issuing without a cookie answered ${summary(anonymous)}, expected 401 unauthenticated`);
  const refused = await deps.send(post(appHost, WORKSTATION_CERTIFICATES_PATH, body, actors.tenantAdmin));
  if (!isError(refused, 403, "permission_denied")) throw new Error(`issuing as a tenant administrator answered ${summary(refused)}, expected 403 permission_denied`);
  // A POST is never sent again after a lost connection: it may have issued a certificate.
  const created = await pollUntil<RawResponse>(
    "the issuing CA to sign a workstation certificate (201)",
    deps.limits?.signerMs ?? SIGNER_LIMIT_MS,
    pollMs,
    deps,
    async () => {
      const response = await deps.send(post(appHost, WORKSTATION_CERTIFICATES_PATH, body, actors.platformAdmin));
      if (response.status === 201) return { done: response };
      return isError(response, 503, "unavailable", CA_NOT_READY) ? { pending: summary(response) } : { fail: `unexpected answer ${summary(response)}` };
    },
  );
  const { issued, problems } = issuedProblems(created.value.body, issuing, workstation.keyId);
  if (issued === undefined) throw new Error(`the issued workstation certificate: ${problems.join("; ")}`);
  deps.log(`issued workstation certificate ${issued.serial} after ${Math.round(created.waitedMs / 1000)} s`);

  const revoke = `${WORKSTATION_CERTIFICATES_PATH}/${issued.serial}/revoke`;
  const reason = { reason: "superseded" };
  const revokeAnonymous = await deps.send(post(appHost, revoke, reason));
  if (!isError(revokeAnonymous, 401, "unauthenticated")) throw new Error(`revoking without a cookie answered ${summary(revokeAnonymous)}, expected 401 unauthenticated`);
  const revokeRefused = await deps.send(post(appHost, revoke, reason, actors.tenantAdmin));
  if (!isError(revokeRefused, 403, "permission_denied")) throw new Error(`revoking as a tenant administrator answered ${summary(revokeRefused)}, expected 403 permission_denied`);
  const revoked = await deps.send(post(appHost, revoke, reason, actors.platformAdmin));
  if (revoked.status !== 200) throw new Error(`revoking as a platform administrator answered ${summary(revoked)}, expected 200`);
  const view = asMap(JSON.parse(revoked.body));
  if (view.serial !== issued.serial || typeof view.revoked_at !== "string" || view.revocation_reason !== "superseded") {
    throw new Error(`revoking answered 200 with ${revoked.body.slice(0, 300)}, not certificate ${issued.serial} revoked as superseded`);
  }
  const again = await deps.send(post(appHost, revoke, reason, actors.platformAdmin));
  if (!isError(again, 409, "conflict", ALREADY_REVOKED)) throw new Error(`revoking again answered ${summary(again)}, expected 409 ${ALREADY_REVOKED}`);
  return (
    `POST ${WORKSTATION_CERTIFICATES_PATH}: without a cookie 401, as a tenant administrator 403, as a platform administrator 201 after ` +
    `${Math.round(created.waitedMs / 1000)} s with certificate ${issued.serial} that the issuing CA signed, ca_key_id ${issuing.keyId}; ` +
    "its revoke: without a cookie 401, as a tenant administrator 403, as a platform administrator 200 (superseded), again 409 already_revoked"
  );
}

// ---------------------------------------------------------------------------------------------
// Against the release.

/** What the waiting check found, for the signing step and check. */
export interface CertsWaiting {
  objects: CertsObjects;
  /** The issuing CA key's request from the ConfigMap, and the id of its key. */
  request: string;
  keyId: string;
  pods: CertsPod[];
  actors: WorkstationActors;
  /** The workstation's request the API checks send, and its key id. */
  workstation: WorkstationRequest;
}

function kubectlOf(ctx: ApiCheckContext, namespace: string) {
  const env = { KUBECONFIG: ctx.kubeconfig };
  return (args: string[]) => run(ctx.log, [ctx.kubectl, "--namespace", namespace, ...args], { env, timeoutMs: 60_000, quiet: true });
}

/** Fetches from inside the mesh: BusyBox wget in the broker's container, through ztunnel as the broker's ServiceAccount. */
function meshDeps(ctx: ApiCheckContext, namespace: string, fetcher: MeshFetcher): MeshDeps {
  const env = { KUBECONFIG: ctx.kubeconfig };
  return {
    fetch: async (path) => {
      const cmd = [ctx.kubectl, "exec", "--namespace", namespace, fetcher.pod, "-c", fetcher.container, "--", "wget", "-q", "-T", "10", "-O", "-", `${fetcher.certsUrl}${path}`];
      const answer = meshAnswerOf(await exec(cmd, { env, timeoutMs: 60_000 }));
      ctx.log.line(`$ ${cmd.join(" ")}: ${answerSummary(answer)}`);
      return answer;
    },
    crlVerdict: opensslCrlVerdict,
    log: (line) => ctx.log.line(line),
  };
}

async function certsPods(ctx: ApiCheckContext, namespace: string, objects: CertsObjects): Promise<CertsPod[]> {
  const listed = await kubectlOf(ctx, namespace)(["get", "pods", "--selector", objects.selector, "--output", "json"]);
  return certsPodsOf(JSON.parse(listed.stdout));
}

/** The waiting check against the release; returns the report line and what the next steps need. */
export async function checkCertsWaiting(
  ctx: ApiCheckContext,
  namespace: string,
  manifest: K8sObject[],
  ingressCa: string | undefined,
  root: TestRoot,
  tenantName: string,
): Promise<{ detail: string; waiting: CertsWaiting }> {
  const kubectl = kubectlOf(ctx, namespace);
  const runner: Runner = (cmd, options) => run(ctx.log, cmd, options);
  const objects = certsObjectsOf(manifest);
  const problems: string[] = [];

  // The Secret, without its data (identity-keys.ts, SECRET_SUMMARY_TEMPLATE).
  const summary = await kubectl(["get", "secret", objects.secret, "--output", `go-template=${SECRET_SUMMARY_TEMPLATE}`]);
  problems.push(...secretSummaryProblems(objects.secret, summary.stdout, CA_KEYS_KEY));
  // The requests ConfigMap is public.
  const configMap = await kubectl(["get", "configmap", objects.configMap, "--output", "json"]);
  const found = requestOf(objects.configMap, JSON.parse(configMap.stdout), objects.generation);
  problems.push(...found.problems);
  const pods = await certsPods(ctx, namespace, objects);
  if (pods.length === 0) problems.push(`no pod of ${objects.deployment}`);
  for (const pod of pods) {
    if (pod.caKeysExit !== 0) problems.push(`${pod.name}: the ${CA_KEYS_CONTAINER} init container ${pod.caKeysExit === undefined ? "has not ended" : `exited with ${pod.caKeysExit}`}`);
  }
  if (problems.length > 0 || found.request === undefined) throw new Error(problems.join("; "));
  const keyId = await requestKeyId(runner, found.request);
  const keys = `Secret ${objects.secret}: only ${CA_KEYS_KEY}, bound; ConfigMap ${objects.configMap}: ${requestKeyOf(objects.generation)}, a request for key ${keyId}`;

  const mesh = await checkMeshWaiting(meshDeps(ctx, namespace, meshFetcherOf(manifest)), root.certificate);

  const request = await workstationRequest(runner);
  const workstation: WorkstationRequest = { request, keyId: await requestKeyId(runner, request) };
  const access = await ingressAccess(ctx, namespace, manifest, ingressCa);
  try {
    await access.gateway.open();
    const api = await checkWorkstationWaiting(
      { send: (request) => access.gateway.send(request), psql: access.psql, log: (line) => ctx.log.line(line) },
      access.appHost,
      workstation.request,
      tenantName,
    );
    return {
      detail: `${keys}; ${mesh}; ${api.detail}`,
      waiting: { objects, request: found.request, keyId, pods, actors: api.actors, workstation },
    };
  } finally {
    access.gateway.close();
  }
}

/**
 * The ceremony's step 4: the test root signs the request from the ConfigMap. The key id certs-ca
 * printed must be the request's (the README's check against a request someone swapped).
 */
export async function signIssuingCa(ctx: ApiCheckContext, certsCa: string, root: TestRoot, waiting: CertsWaiting, shape: string): Promise<{ certificate: string; keyId: string }> {
  const runner: Runner = (cmd, options) => run(ctx.log, cmd, options);
  const signed = await root.sign(runner, certsCa, waiting.request, issuingCommonName(shape));
  if (signed.keyId !== waiting.keyId) throw new Error(`${CERTS_CA} root sign certified key ${signed.keyId}, but the request in the ConfigMap is for key ${waiting.keyId}`);
  const issued = certificateOf(signed.certificate);
  const rootCertificate = certificateOf(root.certificate);
  if (!issued.checkIssued(rootCertificate) || !issued.verify(rootCertificate.publicKey)) throw new Error("the issuing CA certificate is not signed by the test root");
  return signed;
}

/** The signing check against the upgraded release; returns the report line. */
export async function checkCertsSigning(
  ctx: ApiCheckContext,
  namespace: string,
  manifest: K8sObject[],
  ingressCa: string | undefined,
  root: TestRoot,
  issuing: { certificate: string; keyId: string },
  waiting: CertsWaiting,
): Promise<string> {
  const mesh = await checkMeshSigning(meshDeps(ctx, namespace, meshFetcherOf(manifest)), root.certificate, issuing.certificate);
  const access = await ingressAccess(ctx, namespace, manifest, ingressCa);
  let api: string;
  try {
    await access.gateway.open();
    api = await checkWorkstationSigning(
      { send: (request) => access.gateway.send(request), psql: access.psql, log: (line) => ctx.log.line(line) },
      access.appHost,
      waiting.workstation,
      waiting.actors,
      issuing,
    );
  } finally {
    access.gateway.close();
  }
  // Setting the certificate needs no restart (the README's step 5): the same pods serve it.
  const pods = await certsPods(ctx, namespace, waiting.objects);
  const before = waiting.pods.map((pod) => `${pod.name}/${pod.uid}`).join(", ");
  const after = pods.map((pod) => `${pod.name}/${pod.uid}`).join(", ");
  if (before !== after) throw new Error(`the certs pods changed from [${before}] to [${after}]; setting the issuing CA certificate must not restart them`);
  return `${mesh}; ${api}; the same ${pods.length} certs pod(s) throughout, no restart`;
}
