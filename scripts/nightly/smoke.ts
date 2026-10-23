// Deploys the Helm chart with one values file into a k3d cluster and checks that it works:
// every workload of the release is ready (the kubelet's HTTP probes answer), every CloudNativePG
// Cluster is healthy, each ingress gateway's NetworkPolicy admits exactly the ports its Istio pods
// use, every Service that exposes a probe routes to a ready pod, the migration Job generated the
// passwords into the Secrets the chart declares, the identity and assets services built from the
// commit answer through the ingress with a session cookie (api.ts, Laysi/trustiot-v2#65), so does
// the telemetry-query service once it made the new tenant's ClickHouse account, which the smoke
// then finds in ClickHouse (api.ts, Laysi/trustiot-v2#539), and an
// uninstall leaves nothing behind but the objects the chart keeps on purpose (the database and the
// password Secrets), which are then deleted. The same request through the ingress also proves the
// MS0 edges with the real services (api.ts, Laysi/trustiot-v2#77): a stolen identity header never
// gets through, and a Host with a port or a path the mesh normalizes reaches the right service.
// Nothing creates a key for the identity service: its init container generates the internal
// identity keys in the cluster, and the smoke checks that they are there, that only the identity
// service's account may change them, and how long the services took to pick them up on this fresh
// install (identity-keys.ts, Laysi/trustiot-v2#404), then runs the CronJob that rotates the key by
// age once and checks that it succeeds and leaves the key, which is not due, as it is
// (Laysi/trustiot-v2#429). A shape that exposes the ingress through the tunnel pins the ingress
// gateway Service's ClusterIP to an address in the static band of the cluster's Service range,
// and the smoke checks that the Service Istio generated has it (ingress-address.ts,
// Laysi/trustiot-v2#478).
// After the ingress e2e test, the SaaS shape also runs the service-to-service authorization e2e
// test (mesh.e2e.ts, e2e.ts). Components the chart gains later are covered without
// changes here, because the checks read the release's own manifests (manifests.ts).
// The MQTT broker (Laysi/trustiot-v2#84) mounts a server certificate Secret that the installing
// environment provides: the smoke issues one from a CA it makes for the run, for the names of the
// broker's Services, and checks that no Service of the release is reachable from outside the
// cluster and that no route sends traffic to the broker.
// The certs service (Laysi/trustiot-v2#517) runs with a root CA the smoke makes for the run with
// certs-ca: it checks that the service waits for its issuing CA certificate, signs the request the
// init container wrote, upgrades the release with the certificate, and checks that the service then
// signs, inside the mesh and through the ingress, for platform administrators only (certs.ts).
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";

import { apiValues, routesOfValues, runApiCheck } from "./api.ts";
import {
  CERTS_CA,
  checkCertsSigning,
  checkCertsWaiting,
  type CertsWaiting,
  issuingValues,
  rootCommonName,
  rootValues,
  signIssuingCa,
  TestRoot,
} from "./certs.ts";
import { gatherDiagnostics } from "./diagnostics.ts";
import { E2E_SUITES, installEvidenceSection, runClusterE2e, suitesOf } from "./e2e.ts";
import { exec, retry, run, RunLog, waitUntilGone } from "./exec.ts";
import { checkIdentityKeys, checkKeyRotationRun } from "./identity-keys.ts";
import {
  CLUSTER_IP_FIELD,
  clusterIpValues,
  exposureOfValues,
  generatedClusterIpProblems,
  holdersOf,
  serviceCidrsOf,
  smokeClusterIpOf,
  staticBandProblems,
} from "./ingress-address.ts";
import {
  exposureProblems,
  gatewayPortProblems,
  gatewayPorts,
  gatewaysOf,
  healthEndpointsOf,
  ingressHostsOf,
  keptObjectsOf,
  migrationJobsOf,
  migrationOrderProblems,
  missingSecrets,
  missingTlsSecrets,
  mqttBrokerServicesOf,
  parseManifests,
  passwordSecretProblems,
  passwordSecretsOf,
  podProblems,
  postgresClusterProblems,
  postgresClustersOf,
  readyEndpointsFor,
  restartsOf,
  type ServerTlsSecret,
  serverTlsSecretsOf,
  workloadsOf,
} from "./manifests.ts";
import { smokeNamespaceOf, smokeProviderNameOf } from "./platform.ts";
import type { Check, ShapeResult } from "./report.ts";
import { tail } from "./report.ts";

/** One deployment shape: the chart's values files for it, in `helm -f` order. */
export interface Shape {
  name: string;
  /** Paths relative to the chart directory. */
  valuesFiles: string[];
}

export interface SmokeContext {
  repoRoot: string;
  chartDir: string;
  /** Where per-shape diagnostics files go. */
  outDir: string;
  kubeconfig: string;
  helm: string;
  kubectl: string;
  log: RunLog;
  /** Fails a run on purpose after the install, to exercise the failure path and notification. */
  simulateFailure: boolean;
  /** The `certs-ca` program built with the certs image (run.ts), which makes the run's test root CA (certs.ts). */
  certsCa: string;
}

export const RELEASE = "trustiot";
const INSTANCE_VALUES = (shape: string): string => `tests/values/${shape}-instance.yaml`;
const SMOKE_VALUES = "tests/values/smoke.yaml";
/**
 * How long the volumes of a deleted CloudNativePG Cluster may take to go. They are the Cluster's
 * dependents, so Kubernetes' garbage collector deletes them in the background after the Cluster
 * is gone, and the pvc-protection finalizer holds each one until the Postgres pod that mounts it
 * has stopped. The pod gets CloudNativePG's smart shutdown window (smartShutdownTimeout, 180 s by
 * default) before a fast shutdown, so the bound is above it with room for a loaded runner.
 */
const CLUSTER_VOLUMES_GONE_MS = 300_000;

/**
 * One shape per `values-<name>.yaml` in the chart, each with its instance overlay and the
 * smoke overlay that adds a stand-in service (tests/values/smoke.yaml) until real images exist.
 * A values file without an instance overlay is an error, so a third shape cannot be skipped.
 */
export function discoverShapes(chartDir: string): Shape[] {
  const shapes = readdirSync(chartDir)
    .map((file) => /^values-([a-z0-9-]+)\.yaml$/.exec(file)?.[1])
    .filter((name): name is string => name !== undefined)
    .sort();
  if (shapes.length === 0) throw new Error(`no values-<shape>.yaml in ${chartDir}`);
  return shapes.map((name) => {
    const valuesFiles = [`values-${name}.yaml`, INSTANCE_VALUES(name), SMOKE_VALUES];
    for (const file of valuesFiles) {
      if (!existsSync(join(chartDir, file))) {
        throw new Error(`values-${name}.yaml needs ${file}, which does not exist (deploy/chart/README.md)`);
      }
    }
    return { name, valuesFiles };
  });
}

export function namespaceOf(shape: Shape): string {
  return smokeNamespaceOf(shape.name);
}

/** The `-f` arguments for helm, as absolute paths. */
export function valuesArgs(chartDir: string, shape: Shape): string[] {
  return shape.valuesFiles.flatMap((file) => ["-f", join(chartDir, file)]);
}

/** Runs the checks of one shape in order; a failed check stops the shape and collects diagnostics. */
export async function smokeShape(ctx: SmokeContext, shape: Shape): Promise<ShapeResult> {
  const namespace = namespaceOf(shape);
  const kube = { env: { KUBECONFIG: ctx.kubeconfig } };
  // Rendered and deployed manifests can contain Secrets once the chart creates any, so the
  // output of the commands that print them never reaches the log.
  const helm = (args: string[], timeoutMs: number, quiet = false) =>
    run(ctx.log, [ctx.helm, ...args], { ...kube, timeoutMs, quiet });
  const kubectl = (args: string[], timeoutMs = 120_000) =>
    run(ctx.log, [ctx.kubectl, ...args], { ...kube, timeoutMs });
  // For `--output json` reads that code parses: a Pod's JSON holds its spec, with the environment
  // values, so it stays out of the run log, which is uploaded as an artifact (Laysi/trustiot-v2#410).
  const kubectlJson = (args: string[], timeoutMs = 120_000) =>
    run(ctx.log, [ctx.kubectl, ...args], { ...kube, timeoutMs, quiet: true });
  const checks: Check[] = [];
  const started = Date.now();
  ctx.log.line(`== values-${shape.name}.yaml in namespace ${namespace}`);

  async function check(name: string, fn: () => Promise<string>): Promise<boolean> {
    const begin = Date.now();
    ctx.log.line(`-- ${name}`);
    try {
      const detail = await fn();
      checks.push({ name, ok: true, detail, durationMs: Date.now() - begin });
      ctx.log.line(`   ok: ${detail}`);
      return true;
    } catch (error) {
      const detail = error instanceof Error ? error.message : String(error);
      checks.push({ name, ok: false, detail, durationMs: Date.now() - begin });
      ctx.log.line(`   FAILED: ${detail}`);
      return false;
    }
  }

  /** The cluster e2e tests of this shape (e2e.ts), in order; the first failure stops the rest. */
  async function clusterE2eTests(): Promise<boolean> {
    for (const suite of suitesOf(shape.name)) {
      if (!(await check(E2E_SUITES[suite].title, () => runClusterE2e(ctx, suite, shape.name)))) return false;
    }
    return true;
  }

  let manifest = "";
  let passed = false;
  let diagnostics: string | undefined;
  // The shape's values files, then the API check's values (api.ts) once they are written.
  const values = valuesArgs(ctx.chartDir, shape);
  // The certificate the smoke made for an HTTPS listener, which the API check trusts.
  let ingressCa: string | undefined;
  // The ClusterIP the release pins for the tunnel ingress (ingress-address.ts); undefined for a
  // shape whose ingress faces the network directly.
  let fixedClusterIp: string | undefined;
  // The certs service's CA (certs.ts, Laysi/trustiot-v2#517): the run's test root, whose private
  // key lives only in this object until it signs or the shape ends; what the waiting check found;
  // and the issuing CA certificate the root signed.
  let testRoot: TestRoot | undefined;
  let certsWaiting: CertsWaiting | undefined;
  let issuingCa: { certificate: string; keyId: string } | undefined;
  try {
    passed =
      (await check("namespace with the restricted Pod Security level", async () => {
        await kubectl(["create", "namespace", namespace]);
        const labels = ["enforce", "warn", "audit"].map((mode) => `pod-security.kubernetes.io/${mode}=restricted`);
        await kubectl(["label", "namespace", namespace, ...labels, "pod-security.kubernetes.io/enforce-version=latest"]);
        return "enforce, warn and audit are restricted";
      })) &&
      (await check("the values of the API check", async () => {
        // Laysi/trustiot-v2#65: the release runs the identity and assets services built from this
        // commit, behind the smoke namespace's ext_authz provider (platform.ts). No key is made
        // here: the identity pod's init container generates it (Laysi/trustiot-v2#404). Also the
        // AI gateway (Laysi/trustiot-v2#536) and telemetry-query (Laysi/trustiot-v2#539).
        const file = join(ctx.outDir, `values-api-${shape.name}.yaml`);
        const routes = routesOfValues(ctx.chartDir, shape.valuesFiles);
        writeFileSync(file, Bun.YAML.stringify(apiValues(shape.name, routes), null, 2));
        values.push("-f", file);
        return `${basename(file)}: identity, assets, ai-gateway, telemetry-query and certs, provider ${smokeProviderNameOf(shape.name)}`;
      })) &&
      (await check("a test root CA for the certs service, made for this run, sets certificateAuthority.rootCertificate (Laysi/trustiot-v2#517)", async () => {
        // The README's ceremony, step 1 and 2 (services/certs/README.md, 「發簽發 CA 憑證」): the
        // root's private key stays in testRoot, never on a disk; only its certificate is written.
        testRoot = await TestRoot.create((cmd, options) => run(ctx.log, cmd, options), ctx.certsCa, rootCommonName(shape.name));
        const file = join(ctx.outDir, `values-certs-${shape.name}.yaml`);
        writeFileSync(file, Bun.YAML.stringify(rootValues(testRoot.certificate), null, 2));
        values.push("-f", file);
        return `${CERTS_CA} root create made "${testRoot.commonName}"; ${basename(file)}: its certificate`;
      })) &&
      (await check("the tunnel ingress gets a fixed ClusterIP from the static band of the cluster's Service range (Laysi/trustiot-v2#478)", async () => {
        // cloudflared forwards to the ingress Service's ClusterIP, so the chart pins it
        // (ingress.tunnel.clusterIP). The address must be free: a Service that holds it would
        // keep Istio from creating the gateway's Service, and the Gateway would never be programmed.
        const exposure = exposureOfValues(ctx.chartDir, shape.valuesFiles);
        fixedClusterIp = smokeClusterIpOf(shape.name, exposure);
        if (fixedClusterIp === undefined) return `exposure ${exposure}: the chart takes no fixed ClusterIP`;
        const range = await kubectlJson(["get", "servicecidr", "kubernetes", "--output", "json"]);
        const cidrs = serviceCidrsOf(JSON.parse(range.stdout));
        const problems = staticBandProblems(fixedClusterIp, cidrs);
        const services = await kubectlJson(["get", "services", "--all-namespaces", "--output", "json"]);
        const holders = holdersOf(fixedClusterIp, JSON.parse(services.stdout));
        if (holders.length > 0) problems.push(`${fixedClusterIp} is already the ClusterIP of ${holders.join(", ")}`);
        if (problems.length > 0) throw new Error(problems.join("; "));
        const file = join(ctx.outDir, `values-ingress-address-${shape.name}.yaml`);
        writeFileSync(file, Bun.YAML.stringify(clusterIpValues(fixedClusterIp), null, 2));
        values.push("-f", file);
        return `${basename(file)}: ${CLUSTER_IP_FIELD} ${fixedClusterIp}, unused and in the static band of ${cidrs.join(", ")}`;
      })) &&
      (await check("dummy Secrets for the references the chart makes", async () => {
        // Secrets reach a cluster from outside the chart (Laysi/trustiot-v2#72); the smoke test
        // checks the wiring, so every Secret the rendered pods read gets a dummy value. The
        // Secrets the chart declares for the cluster to fill are not among them.
        const rendered = await helm(["template", RELEASE, ctx.chartDir, "--namespace", namespace, ...values], 120_000, true);
        const objects = parseManifests(rendered.stdout);
        const missing = missingSecrets(objects);
        for (const [name, keys] of missing) {
          const literals = [...keys].map((key) => `--from-literal=${key}=nightly-dummy-value`);
          await kubectl(["create", "secret", "generic", name, "--namespace", namespace, ...literals]);
        }
        const tls = missingTlsSecrets(objects);
        for (const name of tls) ingressCa = await createTlsSecret(ctx, namespace, name, ingressHostsOf(objects));
        const servers = serverTlsSecretsOf(objects, namespace);
        for (const secret of servers) await createServerTlsSecret(ctx, namespace, secret);
        const created = [...missing.keys(), ...tls, ...servers.map((secret) => secret.name)];
        return created.length === 0 ? "none needed" : `${created.length} created: ${created.join(", ")}`;
      })) &&
      (await check("helm install --wait", async () => {
        await helm(
          [
            "upgrade", "--install", RELEASE, ctx.chartDir,
            "--namespace", namespace,
            ...values,
            "--wait", "--wait-for-jobs", "--timeout", "5m",
          ],
          6 * 60_000,
        );
        manifest = (await helm(["get", "manifest", RELEASE, "--namespace", namespace], 60_000, true)).stdout;
        return `release ${RELEASE} deployed, ${parseManifests(manifest).length} objects`;
      })) &&
      (!ctx.simulateFailure ||
        (await check("simulated failure (requested for this run)", async () => {
          throw new Error("this run was started with simulate_failure to prove the failure notification");
        }))) &&
      (await check("every workload of the release is ready", async () => {
        const workloads = workloadsOf(parseManifests(manifest));
        if (workloads.length === 0) {
          // tests/values/smoke.yaml always adds a stand-in service, so none means that overlay broke.
          throw new Error("the release has no Deployment, StatefulSet, DaemonSet or Job; tests/values/smoke.yaml should add one");
        }
        for (const workload of workloads) {
          const target = `${workload.kind.toLowerCase()}/${workload.name}`;
          if (workload.kind === "Job") {
            await kubectl(["wait", "--namespace", namespace, "--for=condition=complete", target, "--timeout=180s"], 200_000);
          } else {
            await kubectl(["rollout", "status", "--namespace", namespace, target, "--timeout=180s"], 200_000);
          }
        }
        return workloads.map((w) => `${w.kind}/${w.name}`).join(", ");
      })) &&
      (await check("the migration Job completed before any service started", async () => {
        // Laysi/trustiot-v2#60: every service pod waits in an init container until the Job is
        // complete, so no service runs on a schema older than its own.
        const jobs = migrationJobsOf(parseManifests(manifest));
        if (jobs.length !== 1) throw new Error(`the release should have one migration Job, found ${jobs.length}: ${jobs.join(", ")}`);
        const job = await kubectlJson(["get", "--namespace", namespace, `job/${jobs[0]}`, "--output", "json"]);
        const pods = await kubectlJson(["get", "pods", "--namespace", namespace, "--output", "json"]);
        const parsed = JSON.parse(job.stdout) as { status?: { completionTime?: string } };
        const problems = migrationOrderProblems(parsed, JSON.parse(pods.stdout) as { items?: unknown[] });
        if (problems.length > 0) throw new Error(problems.join("; "));
        return `job/${jobs[0]} completed at ${parsed.status?.completionTime}; every service container started after it`;
      })) &&
      (await check("every password generated in the cluster is in its Secret", async () => {
        // Laysi/trustiot-v2#60: the chart declares the password Secrets empty and the Job fills
        // them on the first install; the bundled ClickHouse's init container fills its admin
        // password Secret, which the Job then logs in with (Laysi/trustiot-v2#403). The Secret's
        // JSON holds the passwords: it is read quietly and only key names reach the log and the
        // report.
        const secrets = passwordSecretsOf(parseManifests(manifest));
        const problems: string[] = [];
        for (const [name, keys] of secrets) {
          const got = await run(ctx.log, [ctx.kubectl, "get", "secret", name, "--namespace", namespace, "--output", "json"], {
            ...kube,
            timeoutMs: 60_000,
            quiet: true,
          });
          problems.push(...passwordSecretProblems(name, keys, JSON.parse(got.stdout)));
        }
        if (problems.length > 0) throw new Error(problems.join("; "));
        return secrets.size === 0
          ? "the release has no password Secret"
          : [...secrets].map(([name, keys]) => `${name}: ${keys.join(", ")}`).join("; ");
      })) &&
      (await check("every CloudNativePG Cluster is healthy", async () => {
        // The bundled Postgres (Laysi/trustiot-v2#58): the operator creates its pods after helm
        // has returned, and bootstrapping the database takes a while on a loaded runner.
        const clusters = postgresClustersOf(parseManifests(manifest));
        const states: string[] = [];
        for (const cluster of clusters) {
          const target = `cluster.postgresql.cnpg.io/${cluster}`;
          await kubectl(["wait", "--namespace", namespace, "--for=condition=Ready", target, "--timeout=600s"], 630_000);
          const status = await kubectlJson(["get", "--namespace", namespace, target, "--output", "json"]);
          const problems = postgresClusterProblems(JSON.parse(status.stdout));
          if (problems.length > 0) throw new Error(problems.join("; "));
          states.push(`${cluster}: healthy`);
        }
        return clusters.length === 0 ? "the release has no CloudNativePG Cluster" : states.join(", ");
      })) &&
      (await check("every Gateway is programmed", async () => {
        // Istio creates a gateway pod for each Gateway object, after helm has returned.
        const gateways = gatewaysOf(parseManifests(manifest));
        for (const gateway of gateways) {
          await kubectl(["wait", "--namespace", namespace, "--for=condition=Programmed", `gateway.gateway.networking.k8s.io/${gateway}`, "--timeout=180s"], 200_000);
        }
        return gateways.length === 0 ? "the release has no Gateway" : gateways.join(", ");
      })) &&
      (await check("the tunnel ingress Service that Istio generated has the fixed ClusterIP (Laysi/trustiot-v2#478)", async () => {
        if (fixedClusterIp === undefined) return "this shape pins no ClusterIP";
        const gateways = gatewaysOf(parseManifests(manifest));
        if (gateways.length !== 1) throw new Error(`the release should have one Gateway, found ${gateways.length}`);
        const gateway = gateways[0]!;
        const services = await kubectlJson([
          "get", "services", "--namespace", namespace,
          "--selector", `gateway.networking.k8s.io/gateway-name=${gateway}`, "--output", "json",
        ]);
        const problems = generatedClusterIpProblems(gateway, JSON.parse(services.stdout), fixedClusterIp);
        if (problems.length > 0) throw new Error(problems.join("; "));
        return `the Service of Gateway ${gateway} has ClusterIP ${fixedClusterIp}`;
      })) &&
      (await check("no pod is broken, not ready or crash-looping", async () => {
        const pods = await kubectlJson(["get", "pods", "--namespace", namespace, "--output", "json"]);
        const list = JSON.parse(pods.stdout) as { items?: unknown[] };
        const problems = podProblems(list);
        if (problems.length > 0) throw new Error(problems.map((p) => `${p.pod}: ${p.problem}`).join("; "));
        const restarts = restartsOf(list);
        const count = list.items?.length ?? 0;
        return restarts.size === 0
          ? `${count} pods running or completed`
          : `${count} pods running or completed; restarts: ${[...restarts].map(([pod, n]) => `${pod}=${n}`).join(", ")}`;
      })) &&
      (await check("the internal identity keys were generated in the cluster, only the identity service's account may change them, and every service picked them up (Laysi/trustiot-v2#404)", () =>
        checkIdentityKeys({ kubectl: ctx.kubectl, kubeconfig: ctx.kubeconfig, log: ctx.log }, namespace, RELEASE, parseManifests(manifest)),
      )) &&
      (await check("the CronJob that rotates the internal identity key by age runs as the identity service's account and leaves a key that is not due as it is (Laysi/trustiot-v2#429)", () =>
        checkKeyRotationRun({ kubectl: ctx.kubectl, kubeconfig: ctx.kubeconfig, log: ctx.log }, namespace, parseManifests(manifest)),
      )) &&
      (await check("each Gateway's NetworkPolicy admits exactly the ports its Istio pods use", async () => {
        // The chart derives the ingress gateway's NetworkPolicy ports from its Gateway and Istio's
        // status port; this compares them with the Deployment and Service the installed Istio made.
        const objects = parseManifests(manifest);
        const found: string[] = [];
        for (const gateway of gatewaysOf(objects)) {
          const generated = await kubectlJson([
            "get", "deployments,services", "--namespace", namespace,
            "--selector", `gateway.networking.k8s.io/gateway-name=${gateway}`, "--output", "json",
          ]);
          const ports = gatewayPorts(gateway, parseManifests(generated.stdout), objects);
          const problems = gatewayPortProblems(ports);
          if (problems.length > 0) throw new Error(problems.join("; "));
          found.push(`${gateway}: ${ports.needed.join(", ")}`);
        }
        return found.length === 0 ? "the release has no Gateway" : found.join("; ");
      })) &&
      (await check("Services that expose a health endpoint have ready endpoints", async () => {
        const endpoints = healthEndpointsOf(parseManifests(manifest));
        if (endpoints.length === 0) {
          throw new Error("no Service exposes an HTTP probe of a workload; tests/values/smoke.yaml should add a service with probes");
        }
        // Not called over the network: default deny refuses the API server's Service proxy
        // (manifests.ts, readyEndpointsFor). The kubelet's probes already answered for every
        // ready pod; this checks that each Service routes to one.
        const routed: string[] = [];
        for (const endpoint of endpoints) {
          await retry(ctx.log, 6, 5_000, async () => {
            const slices = await kubectlJson([
              "get", "endpointslices", "--namespace", namespace,
              "--selector", `kubernetes.io/service-name=${endpoint.service}`, "--output", "json",
            ], 30_000);
            const ready = readyEndpointsFor(JSON.parse(slices.stdout) as { items?: unknown[] }, endpoint.port);
            if (ready === 0) throw new Error(`Service ${endpoint.service} has no ready endpoint on port ${endpoint.port}`);
            return slices;
          });
          routed.push(`${endpoint.service}:${endpoint.port}${endpoint.path}`);
        }
        return routed.join(", ");
      })) &&
      (await check("no Service of the release is reachable from outside the cluster, and no route sends traffic to the MQTT broker (Laysi/trustiot-v2#84)", async () => {
        const objects = parseManifests(manifest);
        const services = await kubectlJson(["get", "services", "--namespace", namespace, "--output", "json"]);
        const problems = exposureProblems(objects, JSON.parse(services.stdout) as { items?: unknown[] });
        if (problems.length > 0) throw new Error(problems.join("; "));
        const count = objects.filter((o) => o.kind === "Service").length;
        const broker = mqttBrokerServicesOf(objects);
        return `${count} Services, each ClusterIP without node ports or external addresses; MQTT broker: ${broker.length === 0 ? "not in this release" : `${broker.join(", ")}, named by no route`}`;
      })) &&
      (await check(
        "through the ingress, a new tenant reaches the assets service and, with its ClickHouse account, the telemetry API; a stolen identity header never does, and Host ports and normalized paths reach it (Laysi/trustiot-v2#65, #77, #539)",
        () => runApiCheck(ctx, namespace, parseManifests(manifest), ingressCa, `nightly smoke ${shape.name}`),
      )) &&
      (await check(
        "certs waits for its issuing CA certificate: the init container wrote the key Secret and the request ConfigMap, the mesh serves the root alone, and the workstation certificate API answers 401, 403 and 503 ca_not_ready (Laysi/trustiot-v2#517)",
        async () => {
          if (testRoot === undefined) throw new Error("no test root was made");
          const found = await checkCertsWaiting(ctx, namespace, parseManifests(manifest), ingressCa, testRoot, `nightly smoke certs ${shape.name}`);
          certsWaiting = found.waiting;
          return found.detail;
        },
      )) &&
      (await check("the test root signs the issuing CA's request from the ConfigMap, and helm upgrade sets the certificate (Laysi/trustiot-v2#517)", async () => {
        // The README's ceremony, step 4 and 5: the key id certs-ca prints must be the request's.
        if (testRoot === undefined || certsWaiting === undefined) throw new Error("the waiting check left nothing to sign");
        issuingCa = await signIssuingCa(ctx, ctx.certsCa, testRoot, certsWaiting, shape.name);
        const file = join(ctx.outDir, `values-certs-issuing-${shape.name}.yaml`);
        writeFileSync(file, Bun.YAML.stringify(issuingValues(issuingCa.certificate), null, 2));
        values.push("-f", file);
        await helm(["upgrade", RELEASE, ctx.chartDir, "--namespace", namespace, ...values, "--wait", "--wait-for-jobs", "--timeout", "5m"], 6 * 60_000);
        // The uninstall check compares with the release's objects as they are now.
        manifest = (await helm(["get", "manifest", RELEASE, "--namespace", namespace], 60_000, true)).stdout;
        return `${CERTS_CA} root sign certified key ${issuingCa.keyId}, the key of ${certsWaiting.objects.configMap}'s request; ${basename(file)}: issuingCertificates; release upgraded`;
      })) &&
      (await check(
        "certs signs with the issuing CA without a restart: the mesh serves the root and the issuing CA and a CRL the issuing CA signed, and only a platform administrator issues and revokes a workstation certificate (Laysi/trustiot-v2#517)",
        async () => {
          if (testRoot === undefined || certsWaiting === undefined || issuingCa === undefined) throw new Error("no issuing CA certificate was set");
          return checkCertsSigning(ctx, namespace, parseManifests(manifest), ingressCa, testRoot, issuingCa, certsWaiting);
        },
      )) &&
      (await check("helm uninstall leaves no object of the release but the kept database and password Secrets", async () => {
        await helm(["uninstall", RELEASE, "--namespace", namespace, "--wait", "--timeout", "3m"], 4 * 60_000);
        const left = await exec(
          [ctx.kubectl, "get", "-f", "-", "--namespace", namespace, "--ignore-not-found", "--output", "name"],
          { ...kube, stdin: manifest, timeoutMs: 60_000 },
        );
        if (left.code !== 0) throw new Error(`kubectl get failed: ${tail(left.stderr.trim(), 400)}`);
        // The chart keeps the database and the password Secrets on uninstall
        // (helm.sh/resource-policy: keep); everything else must be gone, and the kept objects
        // must all still be there.
        const kept = keptObjectsOf(parseManifests(manifest));
        const remaining = left.stdout.trim().split("\n").filter((line) => line !== "");
        const unexpected = remaining.filter((name) => !kept.includes(name));
        const lost = kept.filter((name) => !remaining.includes(name));
        if (unexpected.length > 0) throw new Error(`still present after uninstall: ${unexpected.join(", ")}`);
        if (lost.length > 0) throw new Error(`removed by uninstall although the chart keeps them: ${lost.join(", ")}`);
        if (kept.length === 0) return "all objects removed";
        // Deleting a Cluster deletes its volumes; then nothing of the release is left.
        await kubectl(["delete", "--namespace", namespace, ...kept, "--wait=true", "--timeout=180s"], 200_000);
        const waited = await waitUntilGone(
          "the deleted Cluster's volumes (PVCs labelled cnpg.io/cluster) to be deleted",
          async () => {
            const volumes = await kubectl(["get", "pvc", "--namespace", namespace, "--selector", "cnpg.io/cluster", "--output", "name"]);
            return volumes.stdout.split("\n").filter((line) => line.trim() !== "");
          },
          { timeoutMs: CLUSTER_VOLUMES_GONE_MS },
        );
        return `all objects removed; kept on uninstall as intended, then deleted: ${kept.join(", ")}; their volumes were gone after ${Math.round(waited / 1000)} s`;
      })) &&
      (await clusterE2eTests());
  } catch (error) {
    // The checks catch their own errors; this is a bug in the smoke test itself.
    const detail = error instanceof Error ? (error.stack ?? error.message) : String(error);
    checks.push({ name: "smoke test internal error", ok: false, detail, durationMs: 0 });
    passed = false;
  } finally {
    // Signing dropped the key already; a shape that failed before that drops it here.
    testRoot?.discard();
  }

  if (!passed) diagnostics = await collectDiagnostics(ctx, namespace, shape);

  // The namespace goes in every case, so the next shape starts from a clean cluster.
  const cleaned = await check("delete the namespace", async () => {
    await exec([ctx.helm, "uninstall", RELEASE, "--namespace", namespace, "--ignore-not-found", "--wait", "--timeout", "2m"], {
      ...kube,
      timeoutMs: 3 * 60_000,
    });
    await kubectl(["delete", "namespace", namespace, "--ignore-not-found", "--wait=true", "--timeout=180s"], 200_000);
    return "deleted";
  });

  return {
    shape: shape.name,
    ok: passed && cleaned,
    checks,
    ...(diagnostics === undefined ? {} : { diagnostics }),
    durationMs: Date.now() - started,
  };
}

/**
 * A self-signed certificate for the ingress hosts under the given Secret name, for a Gateway
 * listener; returns the certificate, which the API check trusts. The key lives in a private
 * temporary directory only as long as the command needs it and is never logged.
 */
async function createTlsSecret(ctx: SmokeContext, namespace: string, name: string, hosts: string[]): Promise<string> {
  const dir = mkdtempSync(join(tmpdir(), "nightly-tls-"));
  try {
    const cert = join(dir, "tls.crt");
    const key = join(dir, "tls.key");
    const names = hosts.map((host) => `DNS:${host}`).join(",");
    await run(
      ctx.log,
      ["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-subj", "/CN=nightly.test", "-addext", `subjectAltName=${names}`, "-days", "1", "-keyout", key, "-out", cert],
      { timeoutMs: 30_000 },
    );
    await run(
      ctx.log,
      [ctx.kubectl, "create", "secret", "tls", name, "--namespace", namespace, `--cert=${cert}`, `--key=${key}`],
      { env: { KUBECONFIG: ctx.kubeconfig }, timeoutMs: 60_000 },
    );
    return readFileSync(cert, "utf8");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

/**
 * A server certificate for the Services of a pod that mounts it (manifests.ts, serverTlsSecretsOf),
 * issued by a CA made for this run alone, as a kubernetes.io/tls Secret whose tls.crt holds the
 * certificate and then the CA's, like the chain the cluster will issue (Laysi/trustiot-v2#535).
 * Both keys live in a private temporary directory only as long as the commands need them, and
 * are never logged; the CA's key is thrown away, so nothing else can be signed with it.
 */
async function createServerTlsSecret(ctx: SmokeContext, namespace: string, secret: ServerTlsSecret): Promise<void> {
  if (secret.hosts.length === 0) throw new Error(`no Service of the release selects the pods that mount ${secret.name}`);
  const dir = mkdtempSync(join(tmpdir(), "nightly-server-tls-"));
  const file = (name: string) => join(dir, name);
  try {
    // Every extension spelled out, so nothing depends on the runner's openssl.cnf.
    const config = [
      "[req]",
      "distinguished_name = dn",
      "prompt = no",
      "[dn]",
      "CN = nightly.test",
      "[ca]",
      "basicConstraints = critical,CA:TRUE,pathlen:0",
      "keyUsage = critical,keyCertSign,cRLSign",
      "subjectKeyIdentifier = hash",
      "[server]",
      "basicConstraints = critical,CA:FALSE",
      "keyUsage = critical,digitalSignature",
      "extendedKeyUsage = serverAuth",
      `subjectAltName = ${secret.hosts.map((host) => `DNS:${host}`).join(",")}`,
      "subjectKeyIdentifier = hash",
      "authorityKeyIdentifier = keyid",
    ];
    writeFileSync(file("openssl.cnf"), `${config.join("\n")}\n`);
    const key = ["-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-config", file("openssl.cnf")];
    const openssl = (args: string[]) => run(ctx.log, ["openssl", ...args], { timeoutMs: 30_000 });
    await openssl(["req", "-x509", ...key, "-extensions", "ca", "-subj", "/CN=nightly server CA", "-days", "1", "-keyout", file("ca.key"), "-out", file("ca.crt")]);
    await openssl(["req", "-new", ...key, "-subj", `/CN=${secret.hosts[0]}`, "-keyout", file("tls.key"), "-out", file("tls.csr")]);
    await openssl([
      "x509", "-req", "-in", file("tls.csr"), "-CA", file("ca.crt"), "-CAkey", file("ca.key"),
      "-CAcreateserial", "-CAserial", file("ca.srl"), "-days", "1",
      "-extfile", file("openssl.cnf"), "-extensions", "server", "-out", file("server.crt"),
    ]);
    writeFileSync(file("tls.crt"), `${readFileSync(file("server.crt"), "utf8")}${readFileSync(file("ca.crt"), "utf8")}`);
    await run(
      ctx.log,
      [ctx.kubectl, "create", "secret", "tls", secret.name, "--namespace", namespace, `--cert=${file("tls.crt")}`, `--key=${file("tls.key")}`],
      { env: { KUBECONFIG: ctx.kubeconfig }, timeoutMs: 60_000 },
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

/**
 * What is not ready in a failed shape, and why (diagnostics.ts, Laysi/trustiot-v2#410): status
 * fields, conditions, restart counts, termination reasons and events. The report is public, so
 * there is no Pod spec, no container log and no `describe` in it.
 */
async function collectDiagnostics(ctx: SmokeContext, namespace: string, shape: Shape): Promise<string> {
  const kube = { env: { KUBECONFIG: ctx.kubeconfig }, timeoutMs: 60_000 };
  const command = async (cmd: string[]): Promise<string> => {
    const result = await exec(cmd, kube);
    if (result.code !== 0) {
      throw new Error(`${cmd.slice(0, 2).join(" ")} ${result.timedOut ? "timed out" : `exited with ${result.code}`}: ${tail(result.stderr.trim(), 300)}`);
    }
    return result.stdout;
  };
  const sections = [
    await gatherDiagnostics(namespace, {
      kubectl: (args) => command([ctx.kubectl, ...args]),
      // Helm 4 has no `--all`; with a state flag set it lists only those states, so name them all.
      helmList: () => command([ctx.helm, "list", "--namespace", namespace, "--deployed", "--failed", "--pending", "--superseded", "--uninstalled", "--uninstalling"]),
    }),
  ];
  // Last, because the report keeps the end of a long text. The ingress e2e installs the chart into
  // a namespace of its own, which is deleted before this runs; the test saved what it saw of that
  // namespace when its install failed (Laysi/trustiot-v2#399).
  for (const suite of suitesOf(shape.name)) {
    const installEvidence = installEvidenceSection(ctx.outDir, suite, shape.name);
    if (installEvidence !== undefined) sections.push(installEvidence);
  }
  const text = sections.join("\n");
  writeFileSync(join(ctx.outDir, `diagnostics-${shape.name}.txt`), text);
  return text;
}
