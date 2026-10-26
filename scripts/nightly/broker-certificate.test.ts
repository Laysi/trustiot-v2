// Tests of the broker certificate check (broker-certificate.ts, Laysi/trustiot-v2#535) without a
// cluster: the release's objects as the chart renders them with the smoke's values, the Secret's
// summary, the served chain, the listening sockets, the refusal verdict and the access answers.
// The certificates are real, made with openssl in a temporary directory for this file.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { COMMAND_TIMEOUT_MS, SLOW_TEST_MS, throwIfTimedOut } from "../../deploy/chart/tests/timing.ts";
import { apiValues, routesOfValues } from "./api.ts";
import {
  BROKER_SERVER_NAME,
  brokerCertificateObjectsOf,
  certificateAccessProblems,
  certificateAccessQuestions,
  GATEWAY_PORT,
  listensOn,
  refusalVerdict,
  restartCountOf,
  servedChainProblems,
  tlsSecretSummaryProblems,
} from "./broker-certificate.ts";
import type { BrokerCertificateObjects } from "./broker-certificate.ts";
import { rootValues } from "./certs.ts";
import type { MeshAnswer } from "./certs.ts";
import { exec, pinnedTool } from "./exec.ts";
import type { AccessAnswer } from "./identity-keys.ts";
import { parseManifests } from "./manifests.ts";
import type { K8sObject } from "./manifests.ts";
import { discoverShapes, valuesArgs } from "./smoke.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");
const CHART_DIR = join(REPO_ROOT, "deploy", "chart");
const SERVICE_NAME = "trustiot-mqtt-broker.nightly-saas.svc";
const NAMES = [BROKER_SERVER_NAME, SERVICE_NAME];
/** The Secret the CronJob fills, for release `trustiot`. */
const TLS_OBJECT = "trustiot-mqtt-broker-tls";

// ---------------------------------------------------------------------------------------------
// Real certificates: a root, an issuing CA it signed, and server certificates of every kind the
// check must tell apart. Every key stays in the temporary directory, removed after.

interface Fixtures {
  root: string;
  issuing: string;
  /** The broker's certificate as the certs service signs it. */
  server: string;
  clientOnly: string;
  bothUsages: string;
  fixedNameOnly: string;
  otherSubject: string;
  byRoot: string;
}
let dir: string;
let fx: Fixtures;

beforeAll(async () => {
  dir = mkdtempSync(join(tmpdir(), "nightly-broker-certificate-test-"));
  const leaf = (usage: string, names: string) => [
    "basicConstraints = critical,CA:FALSE",
    "keyUsage = critical,digitalSignature",
    `extendedKeyUsage = ${usage}`,
    `subjectAltName = ${names}`,
    "authorityKeyIdentifier = keyid",
  ];
  const both = `DNS:${BROKER_SERVER_NAME},DNS:${SERVICE_NAME}`;
  const config = [
    "[req]", "distinguished_name = dn", "prompt = no", "[dn]", "CN = x",
    "[root_ext]", "basicConstraints = critical,CA:TRUE", "keyUsage = critical,keyCertSign,cRLSign", "subjectKeyIdentifier = hash",
    "[issuing_ext]", "basicConstraints = critical,CA:TRUE,pathlen:0", "keyUsage = critical,keyCertSign,cRLSign", "subjectKeyIdentifier = hash", "authorityKeyIdentifier = keyid",
    "[server_ext]", ...leaf("serverAuth", both),
    "[client_ext]", ...leaf("clientAuth", both),
    "[both_ext]", ...leaf("serverAuth,clientAuth", both),
    "[fixed_ext]", ...leaf("serverAuth", `DNS:${BROKER_SERVER_NAME}`),
  ];
  writeFileSync(join(dir, "openssl.cnf"), `${config.join("\n")}\n`);
  const openssl = async (args: string[]) => {
    const result = await exec(["openssl", ...args], { cwd: dir, timeoutMs: COMMAND_TIMEOUT_MS });
    if (result.code !== 0) throw new Error(`openssl ${args.join(" ")} exited with ${result.code}: ${result.stderr}`);
  };
  const key = (curve: string) => ["-newkey", "ec", "-pkeyopt", `ec_paramgen_curve:${curve}`, "-nodes", "-config", "openssl.cnf"];
  await openssl(["req", "-x509", ...key("P-384"), "-extensions", "root_ext", "-subj", "/CN=Test root", "-days", "2", "-keyout", "root.key", "-out", "root.pem"]);
  await openssl(["req", "-new", ...key("P-256"), "-subj", "/CN=Test issuing", "-keyout", "issuing.key", "-out", "issuing.csr"]);
  const sign = (request: string, ca: string, extensions: string, out: string) =>
    openssl(["x509", "-req", "-in", request, "-CA", `${ca}.pem`, "-CAkey", `${ca}.key`, "-set_serial", "0x5a17", "-days", "1", "-extfile", "openssl.cnf", "-extensions", extensions, "-out", out]);
  await sign("issuing.csr", "root", "issuing_ext", "issuing.pem");
  await openssl(["req", "-new", ...key("P-256"), "-subj", `/CN=${BROKER_SERVER_NAME}`, "-keyout", "server.key", "-out", "server.csr"]);
  await openssl(["req", "-new", "-key", "server.key", "-config", "openssl.cnf", "-subj", "/CN=someone.else", "-out", "other.csr"]);
  await sign("server.csr", "issuing", "server_ext", "server.pem");
  await sign("server.csr", "issuing", "client_ext", "client.pem");
  await sign("server.csr", "issuing", "both_ext", "both.pem");
  await sign("server.csr", "issuing", "fixed_ext", "fixed.pem");
  await sign("other.csr", "issuing", "server_ext", "other.pem");
  await sign("server.csr", "root", "server_ext", "by-root.pem");
  const read = (file: string) => readFileSync(join(dir, file), "utf8");
  fx = {
    root: read("root.pem"),
    issuing: read("issuing.pem"),
    server: read("server.pem"),
    clientOnly: read("client.pem"),
    bothUsages: read("both.pem"),
    fixedNameOnly: read("fixed.pem"),
    otherSubject: read("other.pem"),
    byRoot: read("by-root.pem"),
  };
}, SLOW_TEST_MS);

afterAll(() => {
  if (dir !== undefined) rmSync(dir, { recursive: true, force: true });
});

// ---------------------------------------------------------------------------------------------

describe("the served chain (tls.crt)", () => {
  test("the server certificate for both names, serverAuth only, signed by the issuing CA, then the issuing CA, passes", () => {
    const { served, problems } = servedChainProblems(`${fx.server}${fx.issuing}`, fx.issuing, NAMES);
    expect(problems).toEqual([]);
    expect(served).toEqual({ serial: "5a17", notAfter: expect.any(String), names: [...NAMES].sort() });
  });

  test.each([
    ["the server certificate alone", () => fx.server, "tls.crt holds 1 certificate(s)"],
    ["the root after it, not the issuing CA", () => `${fx.server}${fx.root}`, "second certificate is not the issuing CA"],
    ["a certificate the root signed", () => `${fx.byRoot}${fx.issuing}`, "not signed by the issuing CA"],
    ["clientAuth instead of serverAuth", () => `${fx.clientOnly}${fx.issuing}`, "extended key usage is [1.3.6.1.5.5.7.3.2], not serverAuth only"],
    ["clientAuth besides serverAuth", () => `${fx.bothUsages}${fx.issuing}`, "not serverAuth only"],
    ["the fixed name alone", () => `${fx.fixedNameOnly}${fx.issuing}`, `names [DNS:${BROKER_SERVER_NAME}], expected DNS`],
    ["another subject", () => `${fx.otherSubject}${fx.issuing}`, 'subject is "CN=someone.else"'],
    ["text outside the certificates", () => `${fx.server}${fx.issuing}junk`, "text outside the CERTIFICATE blocks"],
  ])("fails on %s", (_label, chain, message) => {
    const { served, problems } = servedChainProblems(chain(), fx.issuing, NAMES);
    expect(served).toBeUndefined();
    expect(problems.join("; ")).toContain(message);
  });

  test("an expired certificate fails", () => {
    const later = new Date(Date.now() + 3 * 24 * 3600 * 1000);
    expect(servedChainProblems(`${fx.server}${fx.issuing}`, fx.issuing, NAMES, later).problems.join("; ")).toContain("expired");
  });
});

describe("the Secret's summary", () => {
  const good = "Opaque|u-1|u-1|2|tls.crt,tls.key,";
  test("an Opaque Secret bound to itself with the CronJob's generation and the two keys passes", () => {
    expect(tlsSecretSummaryProblems("s", good, "2")).toEqual([]);
  });

  test.each([
    ["the kubernetes.io/tls type", "kubernetes.io/tls|u-1|u-1|2|tls.crt,tls.key,", "is of type kubernetes.io/tls, not Opaque"],
    ["no bound uid", "Opaque|u-1|<no value>|2|tls.crt,tls.key,", "not bound to itself"],
    ["another uid", "Opaque|u-1|u-0|2|tls.crt,tls.key,", "not bound to itself"],
    ["an older generation", "Opaque|u-1|u-1|1|tls.crt,tls.key,", 'records generation "1", not the CronJob\'s 2'],
    ["no data", "Opaque|u-1|<no value>|<no value>|", "holds the keys [], expected only tls.crt,tls.key"],
    ["another key", "Opaque|u-1|u-1|2|ca.crt,tls.crt,tls.key,", "holds the keys [ca.crt, tls.crt, tls.key]"],
    ["a summary that is not one", "Opaque|u-1", "unexpected summary"],
  ])("fails on %s", (_label, summary, message) => {
    expect(tlsSecretSummaryProblems("s", summary, "2").join("; ")).toContain(message);
  });
});

describe("the gateway port in /proc/net/tcp", () => {
  const header = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode";
  const line = (local: string, remote: string, state: string) => `   0: ${local} ${remote} ${state} 00000000:00000000 00:00000000 00000000  1883        0 1234 1 0000000000000000 100 0 0 10 0`;
  test("a socket in LISTEN on 8883 (0x22B3), IPv4 or IPv6, counts; others do not", () => {
    expect(GATEWAY_PORT).toBe(0x22b3);
    expect(listensOn([header, line("00000000:075B", "00000000:0000", "0A"), line("00000000:22B3", "00000000:0000", "0A")].join("\n"), GATEWAY_PORT)).toBe(true);
    expect(listensOn([header, line("00000000000000000000000000000000:22B3", "00000000000000000000000000000000:0000", "0A")].join("\n"), GATEWAY_PORT)).toBe(true);
    // The services' port only, a connection on 8883 that is not a listener, and a remote 8883.
    expect(listensOn([header, line("00000000:075B", "00000000:0000", "0A")].join("\n"), GATEWAY_PORT)).toBe(false);
    expect(listensOn([header, line("0100007F:22B3", "0100007F:9C40", "01")].join("\n"), GATEWAY_PORT)).toBe(false);
    expect(listensOn([header, line("0100007F:9C40", "0100007F:22B3", "0A")].join("\n"), GATEWAY_PORT)).toBe(false);
    // The header alone, as before any socket exists.
    expect(listensOn(`${header}\n`, GATEWAY_PORT)).toBe(false);
  });

  test("restartCountOf reads the named container's restarts", () => {
    const pod = { status: { containerStatuses: [{ name: "trust", restartCount: 0 }, { name: "mosquitto", restartCount: 1 }] } };
    expect(restartCountOf(pod, "mosquitto")).toBe(1);
    expect(restartCountOf(pod, "missing")).toBeUndefined();
  });
});

describe("the broker port from the broker's pod", () => {
  const ok: MeshAnswer = { ok: true, body: "-----BEGIN CERTIFICATE-----", detail: "" };
  const reset: MeshAnswer = { ok: false, body: "", detail: "wget: error getting response: Connection reset by peer" };
  const status = (code: number): MeshAnswer => ({ ok: false, status: code, body: "", detail: `server returned error: HTTP/1.1 ${code}` });

  test("no HTTP answer between two 200s from the API port is the refusal", () => {
    expect(refusalVerdict(ok, reset, ok)).toBe("no answer: wget: error getting response: Connection reset by peer");
  });

  test.each([
    ["an HTTP answer (the port admitted the call)", ok, status(400), ok, "reached the broker certificate port (HTTP 400)"],
    ["a 2xx answer", ok, ok, ok, "got a 2xx answer"],
    ["a control that fails before", status(503), reset, ok, "the control must be 200"],
    ["a control that fails after (the pod lost the mesh)", ok, reset, reset, "the control must be 200"],
  ])("fails on %s", (_label, before, probe, after, message) => {
    expect(() => refusalVerdict(before, probe, after)).toThrow(message);
  });
});

describe("who may change the Secret", () => {
  const objects = {
    cronJob: "trustiot-mqtt-broker-certificate",
    serviceAccount: "trustiot-mqtt-broker-certificate",
    secret: TLS_OBJECT,
    brokerAccount: "trustiot-mqtt-broker",
  } as BrokerCertificateObjects;
  const answersFor = (allowed: (account: string, verb: string, resource: string) => boolean): AccessAnswer[] =>
    ["trustiot-mqtt-broker-certificate", "trustiot-mqtt-broker", "trustiot-certs"].flatMap((serviceAccount) =>
      certificateAccessQuestions(objects, serviceAccount).map(({ verb, resource }) => ({ serviceAccount, verb, resource, allowed: allowed(serviceAccount, verb, resource) })),
    );
  const designed = (account: string, verb: string, resource: string) =>
    account === objects.serviceAccount && resource === `secrets/${objects.secret}` && (verb === "get" || verb === "patch");

  test("the CronJob's ServiceAccount is asked about Secrets of the namespace too; the others about the Secret", () => {
    expect(certificateAccessQuestions(objects, objects.serviceAccount).map((q) => `${q.verb} ${q.resource}`)).toEqual([
      "get secrets/trustiot-mqtt-broker-tls",
      "patch secrets/trustiot-mqtt-broker-tls",
      "update secrets/trustiot-mqtt-broker-tls",
      "delete secrets/trustiot-mqtt-broker-tls",
      "create secrets",
      "list secrets",
      "watch secrets",
    ]);
    expect(certificateAccessQuestions(objects, "trustiot-mqtt-broker")).toHaveLength(4);
  });

  test("only the CronJob's get and patch pass", () => {
    expect(certificateAccessProblems(objects, answersFor(designed))).toEqual([]);
  });

  test.each([
    ["the broker may patch it", (a: string, v: string, r: string) => designed(a, v, r) || (a === "trustiot-mqtt-broker" && v === "patch"), "trustiot-mqtt-broker may patch secrets/trustiot-mqtt-broker-tls"],
    ["the CronJob may list Secrets", (a: string, v: string, r: string) => designed(a, v, r) || (a === objects.serviceAccount && v === "list"), "may list secrets"],
    ["the CronJob may not patch it", (a: string, v: string, r: string) => designed(a, v, r) && v !== "patch", "may not patch secrets/trustiot-mqtt-broker-tls"],
  ])("fails when %s", (_label, allowed, message) => {
    expect(certificateAccessProblems(objects, answersFor(allowed)).join("; ")).toContain(message);
  });

  test("fails when the CronJob's or the broker's ServiceAccount was not asked", () => {
    const answers = answersFor(designed).filter((a) => a.serviceAccount !== "trustiot-mqtt-broker");
    expect(certificateAccessProblems(objects, answers)).toEqual(["no answer for trustiot-mqtt-broker"]);
  });
});

// ---------------------------------------------------------------------------------------------

describe("the chart with the smoke's certs values", () => {
  for (const shape of discoverShapes(CHART_DIR)) {
    test(`values-${shape.name}.yaml: the CronJob, its Secret and the broker are the objects the design describes`, async () => {
      const helm = await pinnedTool("helm", REPO_ROOT);
      const work = mkdtempSync(join(tmpdir(), "nightly-broker-certificate-values-"));
      let objects: K8sObject[];
      try {
        const write = (name: string, values: unknown) => {
          const file = join(work, name);
          writeFileSync(file, Bun.YAML.stringify(values, null, 2));
          return file;
        };
        const files = [write("api.yaml", apiValues(shape.name, routesOfValues(CHART_DIR, shape.valuesFiles))), write("root.yaml", rootValues(fx.root))];
        const cmd = [helm, "template", "trustiot", CHART_DIR, "--namespace", `nightly-${shape.name}`, ...valuesArgs(CHART_DIR, shape), ...files.flatMap((file) => ["-f", file])];
        const rendered = Bun.spawnSync({ cmd, stdout: "pipe", stderr: "pipe", timeout: COMMAND_TIMEOUT_MS });
        throwIfTimedOut(cmd, rendered);
        expect(rendered.stderr.toString()).toBe("");
        expect(rendered.exitCode, "helm template exit code").toBe(0);
        objects = parseManifests(rendered.stdout.toString());
      } finally {
        rmSync(work, { recursive: true, force: true });
      }
      const namespace = `nightly-${shape.name}`;
      expect(brokerCertificateObjectsOf(objects)).toEqual({
        found: {
          cronJob: "trustiot-mqtt-broker-certificate",
          serviceAccount: "trustiot-mqtt-broker-certificate",
          secret: TLS_OBJECT,
          certsUrl: `http://trustiot-certs-broker.${namespace}.svc:8082`,
          serviceName: `trustiot-mqtt-broker.${namespace}.svc`,
          generation: "1",
          brokerPod: "trustiot-mqtt-broker-0",
          brokerAccount: "trustiot-mqtt-broker",
          trustCertsUrl: `http://trustiot-certs.${namespace}.svc:8080`,
        },
        problems: [],
      });
      // The chart declares the Secret empty, for the CronJob to fill.
      const secret = objects.find((o) => o.kind === "Secret" && o.metadata?.name === "trustiot-mqtt-broker-tls");
      expect(secret).toMatchObject({ type: "Opaque" });
      expect(secret?.data).toBeUndefined();

      // What the check refuses: the CronJob as the broker, with a Secret mounted, or another
      // workload mounting the broker's Secret.
      const changed = (change: (copy: K8sObject[]) => void): string[] => {
        const copy = structuredClone(objects);
        change(copy);
        return brokerCertificateObjectsOf(copy).problems;
      };
      const podOf = (copy: K8sObject[], kind: string, component: string) => {
        const object = copy.find((o) => o.kind === kind && o.metadata?.labels?.["app.kubernetes.io/component"] === component)!;
        const spec = object.spec as Record<string, any>;
        return (kind === "CronJob" ? spec.jobTemplate.spec.template.spec : spec.template.spec) as Record<string, any>;
      };
      const mountTls = (pod: Record<string, any>) => {
        pod.volumes = [...(pod.volumes ?? []), { name: "tls", secret: { secretName: TLS_OBJECT } }];
      };
      expect(
        changed((copy) => {
          podOf(copy, "CronJob", "mqtt-broker-certificate").serviceAccountName = "trustiot-mqtt-broker";
        }),
      ).toContain("trustiot-mqtt-broker-certificate: runs as trustiot-mqtt-broker, not a ServiceAccount of its own");
      expect(
        changed((copy) => {
          mountTls(podOf(copy, "CronJob", "mqtt-broker-certificate"));
        }),
      ).toContain("trustiot-mqtt-broker-certificate: mounts a Secret");
      expect(
        changed((copy) => {
          mountTls(podOf(copy, "Deployment", "certs"));
        }),
      ).toContain("Deployment/trustiot-certs mounts or reads trustiot-mqtt-broker-tls");
      expect(
        changed((copy) => {
          delete (copy.find((o) => o.kind === "CronJob" && o.metadata?.name === "trustiot-mqtt-broker-certificate")!.spec as Record<string, any>).jobTemplate.spec.template.metadata.labels["istio.io/dataplane-mode"];
        }),
      ).toContain("trustiot-mqtt-broker-certificate: its pod is not in the ambient mesh");
    }, SLOW_TEST_MS);
  }
});
