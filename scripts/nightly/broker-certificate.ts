// The smoke's check of the MQTT broker's server certificate (Laysi/trustiot-v2#535;
// deploy/chart/README.md, "MQTT broker", 「伺服器憑證」), once the certs check (certs.ts) has set the
// issuing CA certificate the run's test root signed, so the certs service can sign:
//
// 1. The release's objects are the ones the design describes: one CronJob of the component
//    mqtt-broker-certificate, in the ambient mesh, running as a ServiceAccount of its own (not the
//    broker's) with a projected token only, one container, no Secret mounted or read, writing the
//    Secret the broker's StatefulSet mounts as server-tls; no other workload mounts that Secret.
// 2. One Job made from the CronJob succeeds: from its pod, inside the mesh, it reached the certs
//    service's broker port (<release>-certs-broker:8082) and got a certificate, or found the one a
//    scheduled run got in the meantime.
// 3. The Secret is an Opaque one bound to itself (the uid annotation), records the CronJob's
//    generation and holds tls.crt and tls.key only. tls.crt is read alone (a go-template prints
//    that key, so tls.key never leaves the cluster): the server certificate for
//    mqtt-broker.trustiot.internal and the broker Service's name, with serverAuth only, signed by
//    the issuing CA the run's root certified, then that CA's certificate.
// 4. The broker opens its gateway port: 8883 is in LISTEN in the pod's /proc/net/tcp or tcp6
//    within LISTEN_LIMIT_MS (the kubelet updates the mounted Secret, the trust sidecar copies the
//    gateway listener and restarts the broker once), and the mosquitto container restarted at
//    most once during the check. Mosquitto exits when a listener's certificate does not load, so
//    a listening port means it loaded the certificate and key; that a gateway verifies it with the
//    root and the name is the Rust tests' (services/certs/tests/broker_certificate.rs).
// 5. The security gate on the broker port (it is open to the CronJob's ServiceAccount alone): the
//    broker's own pod, a workload of the mesh that reaches the certs service's API port (ca.pem
//    answers 200 from it, before and after), gets no HTTP answer from the broker port. The body
//    is not a certificate request, so even a call the mesh wrongly admitted could not get one.
// 6. Access: of every ServiceAccount of the namespace only the CronJob's may get and patch the
//    Secret, nobody may update or delete it, and the CronJob's may not create, list or watch
//    Secrets (`kubectl auth can-i`).
//
// Renewal at half of the validity and the broker's reload by SIGHUP are not observed here: the
// certificate lives 30 days. The Rust tests cover them (tools/migrate/tests/mqtt_certificate.rs,
// services/certs/tests/broker_certificate.rs). Nothing here reads the private key or logs the
// Secret's data; the certificates are public.
import { X509Certificate } from "node:crypto";

import { meshAnswerOf, meshFetcherOf, pemBlocks, pollUntil } from "./certs.ts";
import type { MeshAnswer } from "./certs.ts";
import { exec, run, type RunLog } from "./exec.ts";
import { type AccessAnswer, BOUND_UID_ANNOTATION, jobOutcome } from "./identity-keys.ts";
import type { K8sObject } from "./manifests.ts";

/** The component label of the CronJob and of the broker (deploy/chart/templates/mqtt-broker*.yaml). */
export const CERTIFICATE_COMPONENT = "mqtt-broker-certificate";
export const BROKER_COMPONENT = "mqtt-broker";
/** The broker pod's volume of the Secret, and its main container. */
export const SERVER_TLS_VOLUME = "server-tls";
export const BROKER_CONTAINER = "mosquitto";
/** The gateways' port (deploy/chart/files/mosquitto/gateway-listener.conf). */
export const GATEWAY_PORT = 8883;
/** The name gateways verify (crates/contract, certs::BROKER_SERVER_NAME). */
export const BROKER_SERVER_NAME = "mqtt-broker.trustiot.internal";
/** id-kp-serverAuth. */
export const SERVER_AUTH = "1.3.6.1.5.5.7.3.1";
/** The annotation that records the certificate's generation (tools/migrate/src/mqtt_certificate.rs). */
export const GENERATION_ANNOTATION = "trustiot.io/certificate-generation";
/** The Secret's keys, as the summary lists them. */
export const TLS_KEYS = "tls.crt,tls.key";
/** The certs service's broker certificate issuance (services/certs/src/broker.rs). */
export const ISSUE_PATH = "/internal/certs/v1/broker-certificate";
/** The certs service's public CA file, the control of the refusal check. */
const CHAIN_PATH = "/internal/certs/v1/ca.pem";
/** The name of the Job the smoke creates: the CronJob's name (at most 52 characters) plus this. */
export const JOB_SUFFIX = "-smoke";
/** How long the run may take: its pod's start on a loaded runner and its 2-minute wait for certs. */
export const RUN_TIMEOUT_MS = 300_000;
/**
 * How long the broker may take to open its gateway port after the run: the kubelet updates a
 * mounted Secret within its sync period plus the cache's TTL (about a minute, up to two on a loaded
 * node), the sidecar checks every mqttBroker.trustInterval (30 s), and the broker restarts once.
 */
export const LISTEN_LIMIT_MS = 300_000;
const POLL_MS = 3_000;

/** Verbs asked about the Secret by name, and about Secrets of the namespace. */
export const NAMED_VERBS = ["get", "patch", "update", "delete"] as const;
export const KIND_VERBS = ["create", "list", "watch"] as const;
export const ALLOWED_VERBS = ["get", "patch"] as const;

type Map_ = Record<string, unknown>;
const isMap = (value: unknown): value is Map_ => value !== null && typeof value === "object" && !Array.isArray(value);
const asMap = (value: unknown): Map_ => (isMap(value) ? value : {});
const asArray = (value: unknown): unknown[] => (Array.isArray(value) ? value : []);
const asString = (value: unknown): string | undefined => (typeof value === "string" ? value : undefined);

/** The pod template of a workload or a CronJob, or undefined. */
function podTemplateOf(object: K8sObject): Map_ | undefined {
  const spec = asMap(object.spec);
  if (["Deployment", "StatefulSet", "DaemonSet", "Job"].includes(object.kind)) return asMap(spec.template);
  if (object.kind === "CronJob") return asMap(asMap(asMap(spec.jobTemplate).spec).template);
  return undefined;
}

/** What the check works with, from the release's manifests. */
export interface BrokerCertificateObjects {
  cronJob: string;
  serviceAccount: string;
  secret: string;
  /** The certs service's broker port, as the CronJob calls it. */
  certsUrl: string;
  /** The broker Service's name the certificate carries after the fixed one. */
  serviceName: string;
  generation: string;
  /** The broker's pod (StatefulSet, one replica) and its ServiceAccount. */
  brokerPod: string;
  brokerAccount: string;
  /** The certs service's API port, as the broker's trust containers call it. */
  trustCertsUrl: string;
}

/** The release's objects for the check, and why they are not the ones the design describes. */
export function brokerCertificateObjectsOf(objects: K8sObject[]): { found?: BrokerCertificateObjects; problems: string[] } {
  const cronJobs = objects.filter((o) => o.kind === "CronJob" && o.metadata?.labels?.["app.kubernetes.io/component"] === CERTIFICATE_COMPONENT);
  const sets = objects.filter((o) => o.kind === "StatefulSet" && o.metadata?.labels?.["app.kubernetes.io/component"] === BROKER_COMPONENT);
  if (cronJobs.length !== 1) return { problems: [`the release should have one ${CERTIFICATE_COMPONENT} CronJob, found ${cronJobs.length}`] };
  if (sets.length !== 1) return { problems: [`the release should have one MQTT broker StatefulSet, found ${sets.length}`] };
  const cronJob = cronJobs[0]!;
  const name = cronJob.metadata?.name ?? "";
  const template = podTemplateOf(cronJob) ?? {};
  const pod = asMap(template.spec);
  const problems: string[] = [];
  const containers = asArray(pod.containers).filter(isMap);
  if (containers.length !== 1 || asArray(pod.initContainers).length > 0) problems.push(`${name}: has more than its one container`);
  const env = containers.flatMap((c) => asArray(c.env).filter(isMap));
  const value = (key: string): string => {
    const found = asString(env.find((e) => e.name === key)?.value) ?? "";
    if (found === "" && key !== "TRUSTIOT__CERTIFICATE__SERVICE_NAME") problems.push(`${name}: sets no ${key}`);
    return found;
  };
  const secret = value("TRUSTIOT__CERTIFICATE__SECRET");
  const certsUrl = value("TRUSTIOT__CERTIFICATE__CERTS_URL");
  const serviceName = value("TRUSTIOT__CERTIFICATE__SERVICE_NAME");
  const generation = value("TRUSTIOT__CERTIFICATE__GENERATION");
  if (certsUrl !== "" && !/^http:\/\/[a-z0-9.-]+:8082$/.test(certsUrl)) problems.push(`${name}: TRUSTIOT__CERTIFICATE__CERTS_URL ${JSON.stringify(certsUrl)} is not http://<host>:8082`);
  const serviceAccount = asString(pod.serviceAccountName) ?? "";
  if (pod.automountServiceAccountToken !== false) problems.push(`${name}: mounts the default token`);
  if (asMap(asMap(template.metadata).labels)["istio.io/dataplane-mode"] !== "ambient") problems.push(`${name}: its pod is not in the ambient mesh`);
  const volumes = JSON.stringify(pod.volumes ?? []);
  if (volumes.includes('"secret"')) problems.push(`${name}: mounts a Secret`);
  if (JSON.stringify(pod).includes("secretKeyRef") || JSON.stringify(pod).includes("secretRef")) problems.push(`${name}: reads a Secret into its environment`);

  const set = sets[0]!;
  const brokerSpec = asMap(asMap(asMap(set.spec).template).spec);
  const brokerAccount = asString(brokerSpec.serviceAccountName) ?? "";
  if (serviceAccount === "" || serviceAccount === brokerAccount) {
    problems.push(`${name}: runs as ${serviceAccount || "the default ServiceAccount"}, not a ServiceAccount of its own`);
  }
  const mounted = asArray(brokerSpec.volumes)
    .filter(isMap)
    .filter((v) => v.name === SERVER_TLS_VOLUME)
    .map((v) => asString(asMap(v.secret).secretName) ?? "");
  if (mounted.length !== 1 || mounted[0] !== secret) problems.push(`the broker mounts [${mounted.join(", ")}] as ${SERVER_TLS_VOLUME}, not the CronJob's Secret ${secret}`);
  // Nothing else mounts or reads it.
  for (const object of objects) {
    if (object === set) continue;
    const other = podTemplateOf(object);
    if (other === undefined || secret === "") continue;
    const otherSpec = asMap(other.spec);
    const named = asArray(otherSpec.volumes)
      .filter(isMap)
      .some((v) => asString(asMap(v.secret).secretName) === secret || JSON.stringify(v.projected ?? {}).includes(`"${secret}"`));
    const read = JSON.stringify([...asArray(otherSpec.containers), ...asArray(otherSpec.initContainers)]).includes(`"name":"${secret}"`);
    if (named || read) problems.push(`${object.kind}/${object.metadata?.name ?? ""} mounts or reads ${secret}`);
  }
  const fetcher = meshFetcherOf(objects);
  return {
    found: {
      cronJob: name,
      serviceAccount,
      secret,
      certsUrl,
      serviceName,
      generation,
      brokerPod: fetcher.pod,
      brokerAccount,
      trustCertsUrl: fetcher.certsUrl,
    },
    problems,
  };
}

/** The go-template that summarizes the Secret as `<type>|<uid>|<bound uid>|<generation>|<key>,<key>,` without its data. */
export const TLS_SECRET_SUMMARY_TEMPLATE =
  `{{.type}}|{{.metadata.uid}}|` +
  `{{with .metadata.annotations}}{{index . "${BOUND_UID_ANNOTATION}"}}{{end}}|` +
  `{{with .metadata.annotations}}{{index . "${GENERATION_ANNOTATION}"}}{{end}}|` +
  `{{range $key, $value := .data}}{{$key}},{{end}}`;

/** The go-template that prints the Secret's tls.crt alone (base64), never tls.key. */
export const TLS_CERTIFICATE_TEMPLATE = `{{index .data "tls.crt"}}`;

/** Why the Secret's summary is not that of the Secret the CronJob filled for `generation`; empty when it is. */
export function tlsSecretSummaryProblems(name: string, summary: string, generation: string): string[] {
  const [type, uid, bound, recorded, keys, ...rest] = summary.trim().split("|");
  if (type === undefined || uid === undefined || bound === undefined || recorded === undefined || keys === undefined || rest.length > 0) {
    return [`${name}: unexpected summary`];
  }
  const problems: string[] = [];
  if (type !== "Opaque") problems.push(`${name}: is of type ${type}, not Opaque`);
  if (uid === "" || bound !== uid) problems.push(`${name}: not bound to itself (${BOUND_UID_ANNOTATION} is not its uid)`);
  if (recorded !== generation) problems.push(`${name}: records generation ${JSON.stringify(recorded)}, not the CronJob's ${generation}`);
  const names = keys.split(",").filter((key) => key !== "");
  if (names.join(",") !== TLS_KEYS) problems.push(`${name}: holds the keys [${names.join(", ")}], expected only ${TLS_KEYS}`);
  return problems;
}

/** The served certificate, as the report names it. */
export interface ServedCertificate {
  serial: string;
  notAfter: string;
  names: string[];
}

/**
 * Why `chain` (tls.crt) is not the server certificate for `names` signed by `issuing`, followed by
 * `issuing`, valid at `now`; empty problems when it is.
 */
export function servedChainProblems(chain: string, issuing: string, names: string[], now: Date = new Date()): { served?: ServedCertificate; problems: string[] } {
  let blocks: Buffer[];
  try {
    blocks = pemBlocks(chain, "CERTIFICATE");
  } catch (error) {
    return { problems: [`tls.crt: ${error instanceof Error ? error.message : String(error)}`] };
  }
  if (blocks.length !== 2) return { problems: [`tls.crt holds ${blocks.length} certificate(s), expected the server certificate and the issuing CA`] };
  const problems: string[] = [];
  const ca = pemBlocks(issuing, "CERTIFICATE")[0]!;
  if (!blocks[1]!.equals(ca)) problems.push("tls.crt's second certificate is not the issuing CA the root certified");
  const leaf = new X509Certificate(blocks[0]!);
  const issuer = new X509Certificate(ca);
  if (!leaf.checkIssued(issuer) || !leaf.verify(issuer.publicKey)) problems.push("the server certificate is not signed by the issuing CA");
  if (leaf.ca) problems.push("the server certificate is a CA");
  if (leaf.subject !== `CN=${BROKER_SERVER_NAME}`) problems.push(`the server certificate's subject is ${JSON.stringify(leaf.subject)}`);
  const entries = (leaf.subjectAltName ?? "").split(", ").filter((entry) => entry !== "");
  const dns = entries.filter((entry) => entry.startsWith("DNS:")).map((entry) => entry.slice(4));
  const expected = [...names].sort();
  if (dns.length !== entries.length || JSON.stringify([...dns].sort()) !== JSON.stringify(expected)) {
    problems.push(`the server certificate names [${entries.join(", ")}], expected DNS ${expected.join(", ")}`);
  }
  const usage = (leaf as X509Certificate & { keyUsage?: string[] }).keyUsage ?? [];
  if (JSON.stringify(usage) !== JSON.stringify([SERVER_AUTH])) problems.push(`the server certificate's extended key usage is [${usage.join(", ")}], not serverAuth only`);
  if (new Date(leaf.validTo).getTime() <= now.getTime()) problems.push(`the server certificate expired at ${leaf.validTo}`);
  return problems.length > 0 ? { problems } : { served: { serial: leaf.serialNumber.toLowerCase(), notAfter: leaf.validTo, names: expected }, problems };
}

/** Whether a /proc/net/tcp or tcp6 listing has a socket in LISTEN (state 0A) on `port`. */
export function listensOn(listing: string, port: number): boolean {
  return listing
    .split("\n")
    .slice(1)
    .map((line) => line.trim().split(/\s+/))
    .some((fields) => {
      const local = fields[1] ?? "";
      const hex = local.slice(local.lastIndexOf(":") + 1);
      return fields[3] === "0A" && /^[0-9A-Fa-f]{4}$/.test(hex) && Number.parseInt(hex, 16) === port;
    });
}

/** The restart count of a container of a pod (`kubectl get pod -o json`), or undefined. */
export function restartCountOf(pod: unknown, container: string): number | undefined {
  const status = asArray(asMap(asMap(pod).status).containerStatuses)
    .filter(isMap)
    .find((c) => c.name === container);
  return typeof status?.restartCount === "number" ? status.restartCount : undefined;
}

/**
 * The verdict on the broker port from the broker's pod: `before` and `after` fetch ca.pem from the
 * API port (each must be 200, or the pod could not reach certs at all), `probe` posts to the
 * broker port and must get no HTTP answer. Returns what was refused, or throws naming the problem.
 */
export function refusalVerdict(before: MeshAnswer, probe: MeshAnswer, after: MeshAnswer): string {
  const summary = (answer: MeshAnswer): string => (answer.ok ? "200" : `${answer.status ?? "no answer"} ${answer.detail}`.trim());
  if (!before.ok || !after.ok) {
    throw new Error(`from the broker's pod ca.pem on the API port answered ${summary(before)} before and ${summary(after)} after; the control must be 200`);
  }
  if (probe.ok) throw new Error("the broker's pod got a 2xx answer from the broker certificate port; only the certificate CronJob may reach it");
  if (probe.status !== undefined) throw new Error(`the broker's pod reached the broker certificate port (HTTP ${probe.status}); only the certificate CronJob may`);
  return probe.detail === "" ? "no answer" : `no answer: ${probe.detail}`;
}

/** The questions for every ServiceAccount, and for the CronJob's alone. */
export function certificateAccessQuestions(objects: BrokerCertificateObjects, serviceAccount: string): { verb: string; resource: string }[] {
  const named = NAMED_VERBS.map((verb) => ({ verb, resource: `secrets/${objects.secret}` }));
  return serviceAccount === objects.serviceAccount ? [...named, ...KIND_VERBS.map((verb) => ({ verb, resource: "secrets" }))] : named;
}

/** Why the answers differ from "only the CronJob's ServiceAccount may get and patch the Secret". */
export function certificateAccessProblems(objects: BrokerCertificateObjects, answers: AccessAnswer[]): string[] {
  const problems: string[] = [];
  for (const answer of answers) {
    const expected =
      answer.serviceAccount === objects.serviceAccount && answer.resource === `secrets/${objects.secret}` && (ALLOWED_VERBS as readonly string[]).includes(answer.verb);
    if (answer.allowed !== expected) problems.push(`${answer.serviceAccount} ${answer.allowed ? "may" : "may not"} ${answer.verb} ${answer.resource}`);
  }
  for (const account of [objects.serviceAccount, objects.brokerAccount]) {
    if (!answers.some((a) => a.serviceAccount === account)) problems.push(`no answer for ${account}`);
  }
  return problems;
}

/** Where the check runs. */
export interface BrokerCertificateContext {
  kubectl: string;
  kubeconfig: string;
  log: RunLog;
}

const seconds = (ms: number): string => `${Math.round(ms / 1000)} s`;

/** The check; `issuing` is the issuing CA certificate the root certified (PEM). Returns the report line, throws naming every problem. */
export async function checkBrokerCertificate(ctx: BrokerCertificateContext, namespace: string, objects: K8sObject[], issuing: string): Promise<string> {
  const env = { KUBECONFIG: ctx.kubeconfig };
  const kubectl = (args: string[], quiet = true) => run(ctx.log, [ctx.kubectl, "--namespace", namespace, ...args], { env, timeoutMs: 60_000, quiet });
  const { found, problems: design } = brokerCertificateObjectsOf(objects);
  if (found === undefined || design.length > 0) throw new Error(design.join("; "));
  const names = [BROKER_SERVER_NAME, ...(found.serviceName === "" ? [] : [found.serviceName])];
  const brokerPodJson = async () => JSON.parse((await kubectl(["get", "pod", found.brokerPod, "--output", "json"])).stdout) as unknown;
  const restartsBefore = restartCountOf(await brokerPodJson(), BROKER_CONTAINER);
  const summarize = async () => (await kubectl(["get", "secret", found.secret, "--output", `go-template=${TLS_SECRET_SUMMARY_TEMPLATE}`])).stdout;
  const before = await summarize();
  const filledBefore = before.trim().endsWith(`${TLS_KEYS},`);

  // 2. One run of the CronJob.
  const job = `${found.cronJob}${JOB_SUFFIX}`;
  await kubectl(["create", "job", job, `--from=cronjob/${found.cronJob}`], false);
  const started = Date.now();
  let took: string;
  try {
    let outcome: ReturnType<typeof jobOutcome>;
    while (outcome === undefined) {
      if (Date.now() - started > RUN_TIMEOUT_MS) throw new Error(`Job ${job} from ${found.cronJob} did not finish within ${RUN_TIMEOUT_MS / 1000} s`);
      await Bun.sleep(2_000);
      outcome = jobOutcome(JSON.parse((await kubectl(["get", "job", job, "--output", "json"])).stdout));
    }
    if (outcome.done === "failed") throw new Error(`Job ${job} from ${found.cronJob} failed (${outcome.reason}); kubectl logs job/${job} says why`);
    took = seconds(Date.now() - started);
  } finally {
    await kubectl(["delete", "job", job, "--ignore-not-found", "--wait=true", "--timeout=120s"]);
  }

  // 3. The Secret, without tls.key.
  const problems = tlsSecretSummaryProblems(found.secret, await summarize(), found.generation);
  const encoded = (await kubectl(["get", "secret", found.secret, "--output", `go-template=${TLS_CERTIFICATE_TEMPLATE}`])).stdout.trim();
  const chain = Buffer.from(encoded, "base64").toString("utf8");
  const { served, problems: chainProblems } = servedChainProblems(chain, issuing, names);
  problems.push(...chainProblems);
  if (problems.length > 0 || served === undefined) throw new Error(problems.join("; "));

  // 4. The gateway port.
  const listening = await pollUntil<true>(
    `the broker to listen on ${GATEWAY_PORT} (the kubelet updates the Secret, the trust sidecar enables the gateway listener and restarts the broker once)`,
    LISTEN_LIMIT_MS,
    POLL_MS,
    {},
    async () => {
      const read = async (file: string) => exec([ctx.kubectl, "exec", "--namespace", namespace, found.brokerPod, "-c", BROKER_CONTAINER, "--", "cat", file], { env, timeoutMs: 60_000 });
      const v4 = await read("/proc/net/tcp");
      if (v4.code !== 0) return { pending: `cat /proc/net/tcp: ${v4.timedOut ? "timed out" : (v4.stderr.trim().split("\n").at(-1) ?? "").slice(0, 200)}` };
      const v6 = await read("/proc/net/tcp6");
      const listing = `${v4.stdout}\n${v6.code === 0 ? v6.stdout.split("\n").slice(1).join("\n") : ""}`;
      return listensOn(listing, GATEWAY_PORT) ? { done: true } : { pending: `nothing listens on ${GATEWAY_PORT} yet` };
    },
  );
  const restartsAfter = restartCountOf(await brokerPodJson(), BROKER_CONTAINER);
  if (restartsBefore === undefined || restartsAfter === undefined) throw new Error(`the broker pod ${found.brokerPod} has no ${BROKER_CONTAINER} container status`);
  if (restartsAfter - restartsBefore > 1) {
    throw new Error(`${BROKER_CONTAINER} restarted ${restartsAfter - restartsBefore} times while the gateway port opened; the sidecar restarts it once at most`);
  }

  // 5. The broker port refuses the broker's own pod.
  const wget = async (args: string[]): Promise<MeshAnswer> => {
    const cmd = [ctx.kubectl, "exec", "--namespace", namespace, found.brokerPod, "-c", BROKER_CONTAINER, "--", "wget", "-q", "-T", "10", "-O", "-", ...args];
    const answer = meshAnswerOf(await exec(cmd, { env, timeoutMs: 60_000 }));
    ctx.log.line(`$ ${cmd.join(" ")}: ${answer.ok ? `200, ${answer.body.length} bytes` : `${answer.status ?? "no answer"} ${answer.detail}`}`);
    return answer;
  };
  const control = () => wget([`${found.trustCertsUrl}${CHAIN_PATH}`]);
  const controlBefore = await control();
  const probe = await wget(["--header", "Content-Type: application/pkcs10", "--post-data", "not a certificate request", `${found.certsUrl}${ISSUE_PATH}`]);
  const controlAfter = await control();
  const refused = refusalVerdict(controlBefore, probe, controlAfter);

  // 6. Who may change the Secret.
  const accounts = await kubectl(["get", "serviceaccounts", "--output", "json"]);
  const accountNames = asArray(asMap(JSON.parse(accounts.stdout)).items)
    .filter(isMap)
    .map((item) => asString(asMap(item.metadata).name) ?? "")
    .filter((name) => name !== "");
  const answers: AccessAnswer[] = [];
  for (const serviceAccount of accountNames) {
    for (const { verb, resource } of certificateAccessQuestions(found, serviceAccount)) {
      const asked = await exec(
        [ctx.kubectl, "auth", "can-i", verb, resource, "--namespace", namespace, "--as", `system:serviceaccount:${namespace}:${serviceAccount}`],
        { env, timeoutMs: 60_000 },
      );
      const said = asked.stdout.trim();
      if (said !== "yes" && said !== "no") throw new Error(`kubectl auth can-i ${verb} ${resource} as ${serviceAccount} answered ${JSON.stringify(said)}: ${asked.stderr.trim()}`);
      answers.push({ serviceAccount, verb, resource, allowed: said === "yes" });
    }
  }
  const access = certificateAccessProblems(found, answers);
  if (access.length > 0) throw new Error(access.join("; "));

  return (
    `${found.cronJob} ran once as ${found.serviceAccount} (Job ${job}) and succeeded within ${took}; ` +
    `Secret ${found.secret} (${filledBefore ? "already filled by a scheduled run" : "empty before the run"}): Opaque, bound to itself, generation ${found.generation}, only ${TLS_KEYS}; ` +
    `tls.crt: serial ${served.serial} for ${served.names.join(", ")}, serverAuth only, signed by the issuing CA, then the issuing CA, until ${served.notAfter}; ` +
    `the broker listened on ${GATEWAY_PORT} after ${seconds(listening.waitedMs)} more (${BROKER_CONTAINER} restarts ${restartsBefore} -> ${restartsAfter}); ` +
    `from the broker's pod ca.pem on the API port answered 200 before and after, ${ISSUE_PATH} on the broker port got ${refused}; ` +
    `of ${accountNames.length} ServiceAccounts only ${found.serviceAccount} may get and patch ${found.secret} (${answers.length} can-i answers)`
  );
}
