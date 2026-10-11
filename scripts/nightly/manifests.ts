// Reads rendered Helm manifests and derives what the nightly smoke test has to check. It
// never hard-codes a component: workloads, health endpoints and the Secrets a release needs
// all come from the manifests, so components added to the chart later are covered as they
// land (docs/architecture/ci-release.md, nightly.yml).

type YamlMap = Record<string, unknown>;

/** A Kubernetes object, narrowed only as far as the nightly test reads it. */
export interface K8sObject {
  apiVersion?: string;
  kind: string;
  metadata?: { name?: string; namespace?: string; labels?: Record<string, string> };
  spec?: YamlMap;
  [key: string]: unknown;
}

export interface Workload {
  kind: "Deployment" | "StatefulSet" | "DaemonSet" | "Job";
  name: string;
}

/** One HTTP probe of one workload, resolved to a Service that exposes its port. */
export interface HealthEndpoint {
  workload: string;
  probe: "startup" | "liveness" | "readiness";
  service: string;
  /** Service port name, or the number when the port is unnamed. */
  port: string;
  scheme: "http" | "https";
  path: string;
}

const WORKLOAD_KINDS = new Set(["Deployment", "StatefulSet", "DaemonSet", "Job"]);

function isMap(value: unknown): value is YamlMap {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function asMap(value: unknown): YamlMap {
  return isMap(value) ? value : {};
}

function asArray(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

/** Parses `helm template` or `helm get manifest` output into its objects. */
export function parseManifests(text: string): K8sObject[] {
  const parsed: unknown = Bun.YAML.parse(text);
  const documents = Array.isArray(parsed) ? parsed : [parsed];
  const objects: K8sObject[] = [];
  for (const document of documents) {
    if (!isMap(document)) continue; // an empty document, for example a template that rendered nothing
    if (typeof document.kind !== "string") {
      throw new Error(`a manifest document has no kind: ${JSON.stringify(document).slice(0, 200)}`);
    }
    objects.push(document as K8sObject);
  }
  return objects;
}

/** `List` objects hold items of their own; this returns the flat sequence. */
function flatten(objects: K8sObject[]): K8sObject[] {
  return objects.flatMap((object) =>
    object.kind === "List" ? flatten(asArray(object.items).filter(isMap) as K8sObject[]) : [object],
  );
}

/** The pod template of an object that runs pods, or undefined. */
function podTemplate(object: K8sObject): { labels: Record<string, string>; spec: YamlMap } | undefined {
  const spec = asMap(object.spec);
  let template: YamlMap | undefined;
  if (WORKLOAD_KINDS.has(object.kind)) template = asMap(spec.template);
  else if (object.kind === "CronJob") template = asMap(asMap(asMap(spec.jobTemplate).spec).template);
  else if (object.kind === "Pod") template = { metadata: object.metadata, spec };
  if (template === undefined) return undefined;
  const labels: Record<string, string> = {};
  for (const [key, value] of Object.entries(asMap(asMap(template.metadata).labels))) {
    if (typeof value === "string") labels[key] = value;
  }
  return { labels, spec: asMap(template.spec) };
}

function containersOf(spec: YamlMap): YamlMap[] {
  return [...asArray(spec.initContainers), ...asArray(spec.containers)].filter(isMap);
}

/** Deployments, StatefulSets, DaemonSets and Jobs that have to become ready or complete. */
export function workloadsOf(objects: K8sObject[]): Workload[] {
  const workloads: Workload[] = [];
  for (const object of flatten(objects)) {
    const name = object.metadata?.name;
    if (WORKLOAD_KINDS.has(object.kind) && name !== undefined) {
      workloads.push({ kind: object.kind as Workload["kind"], name });
    }
  }
  return workloads;
}

/**
 * Secrets that pods of the release read but the release does not create itself, with the keys
 * they read. The nightly cluster creates them with dummy values: the real Secrets come from the
 * installing environment (docs/architecture/deployment.md), and the smoke test checks the wiring,
 * not the credentials.
 */
export function missingSecrets(objects: K8sObject[]): Map<string, Set<string>> {
  const flat = flatten(objects);
  const created = new Set(flat.filter((o) => o.kind === "Secret").map((o) => o.metadata?.name));
  const needed = new Map<string, Set<string>>();
  const need = (name: unknown, key?: unknown): void => {
    if (typeof name !== "string" || created.has(name)) return;
    const keys = needed.get(name) ?? new Set<string>();
    // A reference without a key (envFrom, a whole-Secret volume) still needs the Secret to exist.
    keys.add(typeof key === "string" ? key : "placeholder");
    needed.set(name, keys);
  };
  for (const object of flat) {
    const template = podTemplate(object);
    if (template === undefined) continue;
    for (const container of containersOf(template.spec)) {
      for (const env of asArray(container.env).filter(isMap)) {
        const ref = asMap(asMap(env.valueFrom).secretKeyRef);
        if (ref.optional !== true) need(ref.name, ref.key);
      }
      for (const from of asArray(container.envFrom).filter(isMap)) {
        const ref = asMap(from.secretRef);
        if (ref.optional !== true) need(ref.name);
      }
    }
    for (const volume of asArray(template.spec.volumes).filter(isMap)) {
      const secret = asMap(volume.secret);
      if (secret.optional === true) continue;
      const items = asArray(secret.items).filter(isMap);
      if (items.length === 0) need(secret.secretName);
      for (const item of items) need(secret.secretName, item.key);
    }
  }
  return needed;
}

/** Resolves a probe or Service port (a name or a number) to a container port number. */
function portNumber(value: unknown, containerPorts: Map<string, number>): number | undefined {
  if (typeof value === "number") return value;
  if (typeof value === "string") {
    const named = containerPorts.get(value);
    if (named !== undefined) return named;
    return /^\d+$/.test(value) ? Number(value) : undefined;
  }
  return undefined;
}

/**
 * The HTTP probes of every workload, each with a Service of the release that reaches the probe's
 * port. A workload whose probes no Service exposes yields nothing: its readiness is still checked
 * through the rollout.
 */
export function healthEndpointsOf(objects: K8sObject[]): HealthEndpoint[] {
  const flat = flatten(objects);
  const services = flat.filter((o) => o.kind === "Service");
  const endpoints: HealthEndpoint[] = [];
  for (const object of flat) {
    const workloadName = object.metadata?.name;
    const template = podTemplate(object);
    if (template === undefined || workloadName === undefined || object.kind === "CronJob") continue;
    for (const container of containersOf(template.spec)) {
      const containerPorts = new Map<string, number>();
      for (const port of asArray(container.ports).filter(isMap)) {
        const name = asString(port.name);
        if (name !== undefined && typeof port.containerPort === "number") {
          containerPorts.set(name, port.containerPort);
        }
      }
      for (const probe of ["startup", "liveness", "readiness"] as const) {
        const http = asMap(asMap(container[`${probe}Probe`]).httpGet);
        const path = asString(http.path);
        const target = portNumber(http.port, containerPorts);
        if (path === undefined || target === undefined) continue;
        const scheme = http.scheme === "HTTPS" ? "https" : "http";
        for (const service of services) {
          const serviceName = service.metadata?.name;
          const selector = asMap(asMap(service.spec).selector);
          const selects =
            Object.keys(selector).length > 0 &&
            Object.entries(selector).every(([key, value]) => template.labels[key] === value);
          if (!selects || serviceName === undefined) continue;
          for (const port of asArray(asMap(service.spec).ports).filter(isMap)) {
            const reached = portNumber(port.targetPort ?? port.port, containerPorts);
            if (reached !== target) continue;
            const exposed = asString(port.name) ?? (typeof port.port === "number" ? String(port.port) : undefined);
            if (exposed === undefined) continue;
            endpoints.push({ workload: workloadName, probe, service: serviceName, port: exposed, scheme, path });
          }
        }
      }
    }
  }
  return dedupe(endpoints);
}

function dedupe(endpoints: HealthEndpoint[]): HealthEndpoint[] {
  const seen = new Set<string>();
  return endpoints.filter((e) => {
    const key = [e.service, e.port, e.scheme, e.path].join("|");
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

/**
 * Ready endpoints that a Service's EndpointSlices (from `kubectl get endpointslices -o json`)
 * list for one Service port (its name, or the number when the port is unnamed: the slice then
 * names it ""). The health endpoints are not called over the network: the chart's default-deny
 * NetworkPolicy and AuthorizationPolicies (decision 27) refuse connections from the node, which
 * is where the API server's Service proxy comes from. The kubelet's HTTP probes, which pass
 * through istio-cni's probe address, have already answered for every ready pod.
 */
export function readyEndpointsFor(sliceList: { items?: unknown[] }, port: string): number {
  let ready = 0;
  for (const slice of asArray(sliceList.items).filter(isMap)) {
    const ports = asArray(slice.ports).filter(isMap);
    // A slice port without a number means the Service port resolved to no container port.
    const servesPort = ports.some((p) => {
      const name = asString(p.name) ?? "";
      return typeof p.port === "number" && (name === port || (name === "" && /^\d+$/.test(port)));
    });
    if (!servesPort) continue;
    for (const endpoint of asArray(slice.endpoints).filter(isMap)) {
      // An unset ready condition means ready (discovery.k8s.io/v1); only false means not ready.
      if (asMap(endpoint.conditions).ready !== false) ready += 1;
    }
  }
  return ready;
}

/** Waiting reasons that mean a pod will not come up without a change. */
const BROKEN_WAITING_REASONS = new Set([
  "CrashLoopBackOff",
  "ErrImagePull",
  "ImagePullBackOff",
  "InvalidImageName",
  "CreateContainerConfigError",
  "CreateContainerError",
  "RunContainerError",
]);

export interface PodProblem {
  pod: string;
  problem: string;
}

/** Pods (from `kubectl get pods -o json`) that are neither running and ready nor completed. */
export function podProblems(podList: { items?: unknown[] }): PodProblem[] {
  const problems: PodProblem[] = [];
  for (const raw of asArray(podList.items).filter(isMap)) {
    const pod = asString(asMap(raw.metadata).name) ?? "(unnamed)";
    const status = asMap(raw.status);
    const phase = asString(status.phase);
    if (phase === "Succeeded") continue;
    const statuses = [...asArray(status.initContainerStatuses), ...asArray(status.containerStatuses)].filter(isMap);
    for (const container of statuses) {
      const reason = asString(asMap(asMap(container.state).waiting).reason);
      if (reason !== undefined && BROKEN_WAITING_REASONS.has(reason)) {
        problems.push({ pod, problem: `${asString(container.name)} is waiting: ${reason}` });
      }
    }
    if (phase !== "Running") {
      problems.push({ pod, problem: `phase ${phase ?? "unknown"}` });
    } else if (!asArray(status.containerStatuses).filter(isMap).every((c) => c.ready === true)) {
      problems.push({ pod, problem: "a container is not ready" });
    }
  }
  return problems;
}

/** Total container restarts per pod, for the report. */
export function restartsOf(podList: { items?: unknown[] }): Map<string, number> {
  const restarts = new Map<string, number>();
  for (const raw of asArray(podList.items).filter(isMap)) {
    const pod = asString(asMap(raw.metadata).name) ?? "(unnamed)";
    let count = 0;
    for (const container of asArray(asMap(raw.status).containerStatuses).filter(isMap)) {
      if (typeof container.restartCount === "number") count += container.restartCount;
    }
    if (count > 0) restarts.set(pod, count);
  }
  return restarts;
}

/** Names of the Gateway API Gateways in the release: Istio creates a gateway pod for each. */
export function gatewaysOf(objects: K8sObject[]): string[] {
  return flatten(objects)
    .filter((o) => o.kind === "Gateway" && asString(o.apiVersion)?.startsWith("gateway.networking.k8s.io/") === true)
    .flatMap((o) => (o.metadata?.name === undefined ? [] : [o.metadata.name]));
}

/**
 * TLS Secrets that the release's Gateway listeners refer to but the release does not create: the
 * installing environment provides them (the customer's certificate on-prem), so the nightly cluster
 * creates a self-signed one under each name.
 */
export function missingTlsSecrets(objects: K8sObject[]): string[] {
  const flat = flatten(objects);
  const created = new Set(flat.filter((o) => o.kind === "Secret").map((o) => o.metadata?.name));
  const names = new Set<string>();
  for (const gateway of flat.filter((o) => o.kind === "Gateway")) {
    for (const listener of asArray(asMap(gateway.spec).listeners).filter(isMap)) {
      for (const ref of asArray(asMap(listener.tls).certificateRefs).filter(isMap)) {
        const kind = asString(ref.kind) ?? "Secret";
        const name = asString(ref.name);
        if (kind === "Secret" && name !== undefined && !created.has(name)) names.add(name);
      }
    }
  }
  return [...names].sort();
}

/** The TCP ports one Istio gateway's pods use, and what the release's NetworkPolicies admit for them. */
export interface GatewayPorts {
  gateway: string;
  /** Ports of the pods' startup, liveness and readiness probes and the target ports of their Service. */
  needed: number[];
  /** Ports that NetworkPolicies of the release admit for the pods from any source; "all" when a rule names no port. */
  admitted: number[] | "all";
}

/** Container port numbers by name, across the containers of a pod spec. */
function namedPorts(spec: YamlMap): Map<string, number> {
  const ports = new Map<string, number>();
  for (const container of containersOf(spec)) {
    for (const port of asArray(container.ports).filter(isMap)) {
      const name = asString(port.name);
      if (name !== undefined && typeof port.containerPort === "number") ports.set(name, port.containerPort);
    }
  }
  return ports;
}

function selectsPod(selector: YamlMap, labels: Record<string, string>): boolean {
  if (asArray(selector.matchExpressions).length > 0) {
    throw new Error("a NetworkPolicy podSelector uses matchExpressions, which this check does not evaluate");
  }
  return Object.entries(asMap(selector.matchLabels)).every(([key, value]) => labels[key] === value);
}

/**
 * Compares the ports that Istio's generated gateway pods really use with what the release's
 * NetworkPolicies let in. `generated` holds the Deployment and Service that Istio created for the
 * Gateway (`kubectl get deployments,services -l gateway.networking.k8s.io/gateway-name=<name>`);
 * `release` is the release's manifest. The chart derives the policy's ports from its own Gateway
 * and Istio's fixed status port (deploy/chart/templates/_ingress.tpl); this checks them against
 * the Istio version that is actually installed.
 */
export function gatewayPorts(gateway: string, generated: K8sObject[], release: K8sObject[]): GatewayPorts {
  const objects = flatten(generated);
  const deployments = objects.filter((o) => o.kind === "Deployment");
  const deployment = deployments[0];
  if (deployment === undefined || deployments.length !== 1) {
    throw new Error(`expected one Deployment generated for Gateway ${gateway}, found ${deployments.length}`);
  }
  const template = podTemplate(deployment);
  if (template === undefined) throw new Error(`the Deployment of Gateway ${gateway} has no pod template`);
  const byName = namedPorts(template.spec);
  const needed = new Set<number>();
  for (const container of containersOf(template.spec)) {
    for (const probe of ["startup", "liveness", "readiness"] as const) {
      const spec = asMap(container[`${probe}Probe`]);
      for (const handler of ["httpGet", "tcpSocket", "grpc"]) {
        if (!isMap(spec[handler])) continue;
        const value = asMap(spec[handler]).port;
        const port = portNumber(value, byName);
        if (port === undefined) throw new Error(`Gateway ${gateway}: the ${probe} probe's port ${JSON.stringify(value)} does not resolve`);
        needed.add(port);
      }
    }
  }
  const services = objects.filter((o) => o.kind === "Service");
  if (services.length === 0) throw new Error(`no Service generated for Gateway ${gateway}`);
  for (const service of services) {
    for (const port of asArray(asMap(service.spec).ports).filter(isMap)) {
      const target = portNumber(port.targetPort ?? port.port, byName);
      if (target === undefined) throw new Error(`Gateway ${gateway}: Service port ${JSON.stringify(port)} does not resolve`);
      needed.add(target);
    }
  }

  let admitted: Set<number> | "all" = new Set<number>();
  for (const policy of flatten(release).filter((o) => o.kind === "NetworkPolicy")) {
    const spec = asMap(policy.spec);
    if (!selectsPod(asMap(spec.podSelector), template.labels)) continue;
    for (const rule of asArray(spec.ingress).filter(isMap)) {
      const ports = asArray(rule.ports).filter(isMap);
      // A rule without ports, or a NetworkPolicyPort without a port, admits every port.
      if (ports.length === 0) admitted = "all";
      for (const port of ports) {
        if ((asString(port.protocol) ?? "TCP") !== "TCP") continue;
        const first = portNumber(port.port, byName);
        if (first === undefined) admitted = "all";
        if (admitted === "all" || first === undefined) continue;
        const last = typeof port.endPort === "number" ? port.endPort : first;
        for (let p = first; p <= last; p += 1) admitted.add(p);
      }
    }
  }
  const sorted = (ports: Set<number>): number[] => [...ports].sort((a, b) => a - b);
  return { gateway, needed: sorted(needed), admitted: admitted === "all" ? "all" : sorted(admitted) };
}

/** What is wrong with a gateway's ports: needed ports the policies refuse, and admitted ports nothing uses. */
export function gatewayPortProblems(ports: GatewayPorts): string[] {
  const admitted = ports.admitted;
  if (admitted === "all") return [`Gateway ${ports.gateway}: a NetworkPolicy rule admits every port`];
  const problems: string[] = [];
  const refused = ports.needed.filter((p) => !admitted.includes(p));
  const unused = admitted.filter((p) => !ports.needed.includes(p));
  if (refused.length > 0) {
    problems.push(`Gateway ${ports.gateway}: the NetworkPolicy refuses ${refused.join(", ")}, which the pods' probes or Service use`);
  }
  if (unused.length > 0) {
    problems.push(`Gateway ${ports.gateway}: the NetworkPolicy admits ${unused.join(", ")}, which neither a probe nor the Service uses`);
  }
  return problems;
}
