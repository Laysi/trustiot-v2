// The smoke's check of the internal identity keys (Laysi/trustiot-v2#404; docs/architecture/
// secrets.md, 「內部身分簽章金鑰」): on a fresh install where nothing created a key, the identity
// pod's init container generated one into the Secret the chart declares and published its public
// key in the ConfigMap; only the identity service's ServiceAccount may get or patch either, which
// `kubectl auth can-i` confirms for every ServiceAccount of the namespace. It also measures how
// long the services built from the commit took to become ready on this fresh install: the kubelet
// mounts the Secret and the ConfigMap when the pod starts, before the init container fills them,
// so a service sees the keys, and becomes ready, only once the kubelet has updated the files (on
// the lab's k3s, before the container started or within 6 s after it; docs/test-reports/
// 2026-10-15-404-identity-keys-in-cluster.md).
// The times come from the pods' status (container start, Ready condition), not from their logs,
// which the nightly never reads (diagnostics.test.ts).
//
// Nothing here reads a private key: the Secret is summarized by a go-template that prints its uid,
// the uid it is bound to and its key names, so its data never reaches this process.
import { exec, run, type RunLog } from "./exec.ts";
import type { K8sObject } from "./manifests.ts";

/** The init container that generates the keys (deploy/chart/templates/_identity.tpl). */
export const SIGNING_KEYS_CONTAINER = "signing-keys";
/** The key of the Secret, and of the ConfigMap (crates/identity-keys). */
export const SIGNING_KEYS_KEY = "signing-keys.json";
export const PUBLIC_KEYS_KEY = "jwks.json";
/** The annotation that binds the Secret to the object tools/migrate filled (tools/migrate/src/passwords.rs). */
export const BOUND_UID_ANNOTATION = "trustiot.io/password-secret-uid";
/** The services built from the commit, whose readiness waits for the keys (crates/service-kit). */
export const KEY_READERS = ["identity", "assets"] as const;

/** The verbs that `kubectl auth can-i` is asked, on the named Secret and ConfigMap, and on their kinds. */
export const NAMED_VERBS = ["get", "patch", "update", "delete"] as const;
export const KIND_VERBS = ["create", "list", "watch"] as const;
/** The only answers that may be yes: the identity service's account, on the two objects. */
export const ALLOWED_VERBS = ["get", "patch"] as const;

type Map_ = Record<string, unknown>;
const isMap = (value: unknown): value is Map_ => value !== null && typeof value === "object" && !Array.isArray(value);
const asMap = (value: unknown): Map_ => (isMap(value) ? value : {});
const asArray = (value: unknown): unknown[] => (Array.isArray(value) ? value : []);
const asString = (value: unknown): string | undefined => (typeof value === "string" ? value : undefined);

/** The identity Deployment's keys objects, from the init container's settings. */
export interface IdentityKeyObjects {
  deployment: string;
  serviceAccount: string;
  secret: string;
  configMap: string;
}

export function identityKeyObjectsOf(objects: K8sObject[]): IdentityKeyObjects {
  const deployments = objects.filter((o) => o.kind === "Deployment" && o.metadata?.labels?.["app.kubernetes.io/component"] === "identity");
  if (deployments.length !== 1) throw new Error(`the release should have one identity Deployment, found ${deployments.length}`);
  const deployment = deployments[0]!;
  const spec = asMap(asMap(asMap(deployment.spec).template).spec);
  const init = asArray(spec.initContainers).filter(isMap).find((c) => c.name === SIGNING_KEYS_CONTAINER);
  if (init === undefined) throw new Error(`the identity Deployment has no ${SIGNING_KEYS_CONTAINER} init container`);
  const env = asArray(init.env).filter(isMap);
  const value = (name: string): string => {
    const found = asString(env.find((e) => e.name === name)?.value);
    if (found === undefined || found === "") throw new Error(`the ${SIGNING_KEYS_CONTAINER} init container has no ${name}`);
    return found;
  };
  return {
    deployment: deployment.metadata?.name ?? "",
    serviceAccount: asString(spec.serviceAccountName) ?? "",
    secret: value("TRUSTIOT__KEYS__SECRET"),
    configMap: value("TRUSTIOT__KEYS__CONFIG_MAP"),
  };
}

/** The go-template that summarizes the Secret as `<uid>|<bound uid>|<key>,<key>,` without its data. */
export const SECRET_SUMMARY_TEMPLATE = `{{.metadata.uid}}|{{with .metadata.annotations}}{{index . "${BOUND_UID_ANNOTATION}"}}{{end}}|{{range $key, $value := .data}}{{$key}},{{end}}`;

/**
 * Why the Secret's summary is not that of a Secret the init container filled; empty when it is.
 * `expectedKey`: its only key, the identity service's signing keys unless given (certs.ts passes
 * the certs service's issuing CA keys).
 */
export function secretSummaryProblems(name: string, summary: string, expectedKey: string = SIGNING_KEYS_KEY): string[] {
  const [uid, bound, keys, ...rest] = summary.trim().split("|");
  if (uid === undefined || bound === undefined || keys === undefined || rest.length > 0) return [`${name}: unexpected summary`];
  const names = keys.split(",").filter((key) => key !== "");
  const problems: string[] = [];
  if (names.join(",") !== expectedKey) problems.push(`${name}: holds the keys [${names.join(", ")}], expected only ${expectedKey}`);
  if (uid === "" || bound !== uid) problems.push(`${name}: not bound to itself (${BOUND_UID_ANNOTATION} is not its uid)`);
  return problems;
}

/** The key ids of the ConfigMap's JWK Set (`kubectl get configmap -o json`), and why it is not one; never a key. */
export function publishedKeysOf(name: string, configMap: unknown): { kids: string[]; problems: string[] } {
  const data = asMap(asMap(configMap).data);
  const problems: string[] = [];
  const keys = Object.keys(data);
  if (keys.join(",") !== PUBLIC_KEYS_KEY) problems.push(`${name}: holds the keys [${keys.join(", ")}], expected only ${PUBLIC_KEYS_KEY}`);
  let set: unknown;
  try {
    set = JSON.parse(asString(data[PUBLIC_KEYS_KEY]) ?? "");
  } catch {
    return { kids: [], problems: [...problems, `${name}: ${PUBLIC_KEYS_KEY} is not JSON`] };
  }
  const entries = asArray(asMap(set).keys).filter(isMap);
  if (entries.length === 0) problems.push(`${name}: ${PUBLIC_KEYS_KEY} lists no key`);
  const kids: string[] = [];
  for (const entry of entries) {
    const kid = asString(entry.kid) ?? "";
    kids.push(kid);
    const members = Object.keys(entry).sort();
    const extra = members.filter((member) => !["alg", "crv", "kid", "kty", "use", "x"].includes(member));
    if (extra.length > 0) problems.push(`${name}: key ${kid} has members beyond the public ones: ${extra.join(", ")}`);
    if (entry.kty !== "OKP" || entry.crv !== "Ed25519") problems.push(`${name}: key ${kid} is not an Ed25519 public key`);
    // Generated keys are named by their RFC 7638 thumbprint: SHA-256, base64url, 43 characters.
    if (!/^[A-Za-z0-9_-]{43}$/.test(kid)) problems.push(`${name}: key id ${kid} is not a thumbprint`);
  }
  return { kids, problems };
}

/** One `kubectl auth can-i` answer. */
export interface AccessAnswer {
  serviceAccount: string;
  verb: string;
  /** `secrets/<name>`, `configmaps/<name>`, or the kind alone. */
  resource: string;
  allowed: boolean;
}

/** The questions to ask for every ServiceAccount. */
export function accessQuestions(objects: IdentityKeyObjects): { verb: string; resource: string }[] {
  const named = [`secrets/${objects.secret}`, `configmaps/${objects.configMap}`];
  return [
    ...named.flatMap((resource) => NAMED_VERBS.map((verb) => ({ verb, resource }))),
    ...["secrets", "configmaps"].flatMap((resource) => KIND_VERBS.map((verb) => ({ verb, resource }))),
  ];
}

/** Why the answers differ from "only the identity service's account may get and patch the two objects". */
export function accessProblems(objects: IdentityKeyObjects, answers: AccessAnswer[]): string[] {
  const named = [`secrets/${objects.secret}`, `configmaps/${objects.configMap}`];
  const problems: string[] = [];
  for (const answer of answers) {
    const expected =
      answer.serviceAccount === objects.serviceAccount && named.includes(answer.resource) && (ALLOWED_VERBS as readonly string[]).includes(answer.verb);
    if (answer.allowed !== expected) {
      problems.push(`${answer.serviceAccount} ${answer.allowed ? "may" : "may not"} ${answer.verb} ${answer.resource}`);
    }
  }
  if (!answers.some((a) => a.serviceAccount === objects.serviceAccount)) problems.push(`no answer for ${objects.serviceAccount}`);
  return problems;
}

/** When the identity pods' init containers finished: the moment the keys were published on this install. */
export function publishedAt(podList: unknown, deployment: string): number | undefined {
  const times = asArray(asMap(podList).items)
    .filter(isMap)
    .filter((pod) => (asString(asMap(pod.metadata).name) ?? "").startsWith(`${deployment}-`))
    .flatMap((pod) => asArray(asMap(pod.status).initContainerStatuses).filter(isMap))
    .filter((status) => status.name === SIGNING_KEYS_CONTAINER)
    .map((status) => Date.parse(asString(asMap(asMap(status.state).terminated).finishedAt) ?? ""))
    .filter((time) => !Number.isNaN(time));
  return times.length === 0 ? undefined : Math.min(...times);
}

/** A service pod, when its main container started and when the pod became Ready, from the pod list. */
export interface ReaderPod {
  pod: string;
  service: string;
  startedAt: number;
  readyAt?: number;
}

export function readerPods(podList: unknown, release: string): ReaderPod[] {
  const pods: ReaderPod[] = [];
  for (const raw of asArray(asMap(podList).items).filter(isMap)) {
    const component = asString(asMap(asMap(raw.metadata).labels)["app.kubernetes.io/component"]);
    const name = asString(asMap(raw.metadata).name) ?? "";
    if (component === undefined || !(KEY_READERS as readonly string[]).includes(component) || !name.startsWith(`${release}-${component}-`)) continue;
    const status = asArray(asMap(raw.status).containerStatuses)
      .filter(isMap)
      .find((c) => c.name === component);
    const startedAt = Date.parse(asString(asMap(asMap(status?.state).running).startedAt) ?? "");
    const ready = asArray(asMap(raw.status).conditions)
      .filter(isMap)
      .find((condition) => condition.type === "Ready" && condition.status === "True");
    const readyAt = Date.parse(asString(ready?.lastTransitionTime) ?? "");
    if (!Number.isNaN(startedAt)) pods.push({ pod: name, service: component, startedAt, ...(Number.isNaN(readyAt) ? {} : { readyAt }) });
  }
  return pods.sort((a, b) => a.pod.localeCompare(b.pod));
}

const seconds = (ms: number): string => `${(ms / 1000).toFixed(1)} s`;

/** Where the check runs. */
export interface IdentityKeysContext {
  kubectl: string;
  kubeconfig: string;
  log: RunLog;
}

/** The check; returns the report line, throws naming every problem. */
export async function checkIdentityKeys(ctx: IdentityKeysContext, namespace: string, release: string, objects: K8sObject[]): Promise<string> {
  const env = { KUBECONFIG: ctx.kubeconfig };
  const kubectl = (args: string[], quiet = true) => run(ctx.log, [ctx.kubectl, "--namespace", namespace, ...args], { env, timeoutMs: 60_000, quiet });
  const keys = identityKeyObjectsOf(objects);
  const problems: string[] = [];

  // The Secret, without its data.
  const summary = await kubectl(["get", "secret", keys.secret, "--output", `go-template=${SECRET_SUMMARY_TEMPLATE}`]);
  problems.push(...secretSummaryProblems(keys.secret, summary.stdout));
  // The ConfigMap is public.
  const configMap = await kubectl(["get", "configmap", keys.configMap, "--output", "json"]);
  const published = publishedKeysOf(keys.configMap, JSON.parse(configMap.stdout));
  problems.push(...published.problems);

  // Who may change them: every ServiceAccount of the namespace.
  const accounts = await kubectl(["get", "serviceaccounts", "--output", "json"]);
  const names = asArray(asMap(JSON.parse(accounts.stdout)).items)
    .filter(isMap)
    .map((item) => asString(asMap(item.metadata).name) ?? "")
    .filter((name) => name !== "");
  const answers: AccessAnswer[] = [];
  for (const serviceAccount of names) {
    for (const { verb, resource } of accessQuestions(keys)) {
      const asked = await exec(
        [ctx.kubectl, "auth", "can-i", verb, resource, "--namespace", namespace, "--as", `system:serviceaccount:${namespace}:${serviceAccount}`],
        { env, timeoutMs: 60_000 },
      );
      const said = asked.stdout.trim();
      if (said !== "yes" && said !== "no") throw new Error(`kubectl auth can-i ${verb} ${resource} as ${serviceAccount} answered ${JSON.stringify(said)}: ${asked.stderr.trim()}`);
      answers.push({ serviceAccount, verb, resource, allowed: said === "yes" });
    }
  }
  problems.push(...accessProblems(keys, answers));

  // How long the services took to become ready, from the pods' status.
  const pods = JSON.parse((await kubectl(["get", "pods", "--output", "json"])).stdout) as unknown;
  const written = publishedAt(pods, keys.deployment);
  if (written === undefined) problems.push(`no ${SIGNING_KEYS_CONTAINER} init container of ${keys.deployment} has finished`);
  const delays: string[] = [];
  const readers = readerPods(pods, release);
  for (const service of KEY_READERS) {
    if (!readers.some((reader) => reader.service === service)) problems.push(`no running pod of ${service}`);
  }
  for (const reader of readers) {
    if (reader.readyAt === undefined) {
      problems.push(`${reader.pod} is not ready`);
      continue;
    }
    const sinceStart = seconds(reader.readyAt - reader.startedAt);
    const sincePublished = written === undefined ? "" : `, ${seconds(reader.readyAt - written)} after the keys were published`;
    delays.push(`${reader.service} ready ${sinceStart} after its container started${sincePublished}`);
  }
  if (problems.length > 0) throw new Error(problems.join("; "));
  return (
    `Secret ${keys.secret}: only ${SIGNING_KEYS_KEY}, bound; ConfigMap ${keys.configMap}: ${published.kids.length} public key(s); ` +
    `of ${names.length} ServiceAccounts only ${keys.serviceAccount} may get and patch them (${answers.length} can-i answers); ` +
    `fresh install: ${delays.join("; ")}`
  );
}

// ---------------------------------------------------------------------------------------------
// The CronJob that rotates the signing key by age (Laysi/trustiot-v2#429; docs/architecture/
// secrets.md, 「依金鑰年齡自動輪替」). The smoke runs it once on the fresh install: on a key minutes
// old it is not due, so a run that succeeds and leaves the published keys as they were proves its
// pod starts, reaches the API through the mesh and the NetworkPolicies, and may get and patch the
// two objects as the identity service's ServiceAccount. The rotation itself is tested on the lab
// (deploy/chart/tests/e2e/key-rotation.e2e.ts), not here: it takes several minutes.

/** The component label of the rotation CronJob (deploy/chart/templates/internal-identity.yaml). */
export const ROTATION_COMPONENT = "identity-key-rotation";
/** The name of the Job the smoke creates from it: the CronJob's name (at most 52) plus this. */
export const ROTATION_JOB_SUFFIX = "-smoke";

export interface RotationCronJob {
  name: string;
  serviceAccount: string;
}

/**
 * The release's rotation CronJob, if it renders one (internalIdentity.maxKeyAgeSeconds is not 0),
 * and why its pod is not the one the design describes: the identity service's ServiceAccount with
 * a projected token only, one container, no Secret mounted or read.
 */
export function rotationCronJobOf(objects: K8sObject[], keys: IdentityKeyObjects): { cronJob?: RotationCronJob; problems: string[] } {
  const found = objects.filter((o) => o.kind === "CronJob" && o.metadata?.labels?.["app.kubernetes.io/component"] === ROTATION_COMPONENT);
  if (found.length === 0) return { problems: [] };
  if (found.length > 1) return { problems: [`the release has ${found.length} rotation CronJobs`] };
  const cronJob = found[0]!;
  const name = cronJob.metadata?.name ?? "";
  const pod = asMap(asMap(asMap(asMap(asMap(cronJob.spec).jobTemplate).spec).template).spec);
  const problems: string[] = [];
  const serviceAccount = asString(pod.serviceAccountName) ?? "";
  if (serviceAccount !== keys.serviceAccount) problems.push(`${name}: runs as ${serviceAccount || "the default ServiceAccount"}, not ${keys.serviceAccount}`);
  if (pod.automountServiceAccountToken !== false) problems.push(`${name}: mounts the default token`);
  if (asArray(pod.containers).length !== 1 || asArray(pod.initContainers).length > 0) problems.push(`${name}: has more than its one container`);
  const volumes = JSON.stringify(pod.volumes ?? []);
  if (volumes.includes('"secret"') || volumes.includes(keys.secret)) problems.push(`${name}: mounts a Secret`);
  if (JSON.stringify(pod).includes("secretKeyRef")) problems.push(`${name}: reads a Secret into its environment`);
  return { cronJob: { name, serviceAccount }, problems };
}

/** Whether a Job (`kubectl get job -o json`) has succeeded or failed, with the reason; undefined while it runs. */
export function jobOutcome(job: unknown): { done: "succeeded" | "failed"; reason: string } | undefined {
  const status = asMap(asMap(job).status);
  if (typeof status.succeeded === "number" && status.succeeded > 0) return { done: "succeeded", reason: "" };
  const failed = asArray(status.conditions)
    .filter(isMap)
    .find((condition) => condition.type === "Failed" && condition.status === "True");
  if (failed !== undefined) return { done: "failed", reason: `${asString(failed.reason) ?? ""}: ${asString(failed.message) ?? ""}` };
  return undefined;
}

/** How long the smoke waits for the run: an image pull and a start on a loaded runner. */
export const ROTATION_RUN_TIMEOUT_MS = 300_000;

/** Runs the rotation CronJob once and checks it succeeded without changing the keys; returns the report line. */
export async function checkKeyRotationRun(ctx: IdentityKeysContext, namespace: string, objects: K8sObject[]): Promise<string> {
  const env = { KUBECONFIG: ctx.kubeconfig };
  const kubectl = (args: string[], quiet = true) => run(ctx.log, [ctx.kubectl, "--namespace", namespace, ...args], { env, timeoutMs: 60_000, quiet });
  const keys = identityKeyObjectsOf(objects);
  const { cronJob, problems } = rotationCronJobOf(objects, keys);
  if (problems.length > 0) throw new Error(problems.join("; "));
  if (cronJob === undefined) return "the release renders no rotation CronJob (internalIdentity.maxKeyAgeSeconds is 0)";
  const publishedNow = async () => publishedKeysOf(keys.configMap, JSON.parse((await kubectl(["get", "configmap", keys.configMap, "--output", "json"])).stdout));
  const before = await publishedNow();
  const job = `${cronJob.name}${ROTATION_JOB_SUFFIX}`;
  await kubectl(["create", "job", job, `--from=cronjob/${cronJob.name}`], false);
  const started = Date.now();
  try {
    let outcome: ReturnType<typeof jobOutcome>;
    while (outcome === undefined) {
      if (Date.now() - started > ROTATION_RUN_TIMEOUT_MS) {
        throw new Error(`Job ${job} from ${cronJob.name} did not finish within ${ROTATION_RUN_TIMEOUT_MS / 1000} s`);
      }
      await Bun.sleep(2_000);
      outcome = jobOutcome(JSON.parse((await kubectl(["get", "job", job, "--output", "json"])).stdout));
    }
    if (outcome.done === "failed") throw new Error(`Job ${job} from ${cronJob.name} failed (${outcome.reason}); kubectl logs job/${job} says why`);
    const took = seconds(Date.now() - started);
    const after = await publishedNow();
    const summary = await kubectl(["get", "secret", keys.secret, "--output", `go-template=${SECRET_SUMMARY_TEMPLATE}`]);
    const changed = [...after.problems, ...secretSummaryProblems(keys.secret, summary.stdout)];
    if (JSON.stringify(after.kids) !== JSON.stringify(before.kids)) {
      changed.push(`the published keys changed from [${before.kids.join(", ")}] to [${after.kids.join(", ")}], though a key minutes old is not due`);
    }
    if (changed.length > 0) throw new Error(changed.join("; "));
    return `${cronJob.name} ran once as ${cronJob.serviceAccount} (Job ${job}) and succeeded within ${took}; the key was not due, so the ${after.kids.length} published key(s) stayed`;
  } finally {
    await kubectl(["delete", "job", job, "--ignore-not-found", "--wait=true", "--timeout=120s"]);
  }
}
