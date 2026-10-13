// Deploys the Helm chart with one values file into a k3d cluster and checks that it works:
// every workload of the release is ready (the kubelet's HTTP probes answer), every CloudNativePG
// Cluster is healthy, each ingress gateway's NetworkPolicy admits exactly the ports its Istio pods
// use, every Service that exposes a probe routes to a ready pod, the migration Job generated the
// passwords into the Secrets the chart declares, and an uninstall leaves nothing behind but the
// objects the chart keeps on purpose (the database and the password Secrets), which are then
// deleted. Components the chart gains later are covered without changes here, because the checks
// read the release's own manifests (manifests.ts).
import { existsSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { runIngressE2e } from "./e2e.ts";
import { exec, retry, run, RunLog, waitUntilGone } from "./exec.ts";
import {
  gatewayPortProblems,
  gatewayPorts,
  gatewaysOf,
  healthEndpointsOf,
  keptObjectsOf,
  migrationJobsOf,
  migrationOrderProblems,
  missingSecrets,
  missingTlsSecrets,
  parseManifests,
  passwordSecretProblems,
  passwordSecretsOf,
  podProblems,
  postgresClusterProblems,
  postgresClustersOf,
  readyEndpointsFor,
  restartsOf,
  workloadsOf,
} from "./manifests.ts";
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
  return `nightly-${shape.name}`;
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

  let manifest = "";
  let passed = false;
  let diagnostics: string | undefined;
  try {
    passed =
      (await check("namespace with the restricted Pod Security level", async () => {
        await kubectl(["create", "namespace", namespace]);
        const labels = ["enforce", "warn", "audit"].map((mode) => `pod-security.kubernetes.io/${mode}=restricted`);
        await kubectl(["label", "namespace", namespace, ...labels, "pod-security.kubernetes.io/enforce-version=latest"]);
        return "enforce, warn and audit are restricted";
      })) &&
      (await check("dummy Secrets for the references the chart makes", async () => {
        // Secrets reach a cluster from outside the chart (Laysi/trustiot-v2#72); the smoke test
        // checks the wiring, so every Secret the rendered pods read gets a dummy value.
        const rendered = await helm(
          ["template", RELEASE, ctx.chartDir, "--namespace", namespace, ...valuesArgs(ctx.chartDir, shape)],
          120_000,
          true,
        );
        const missing = missingSecrets(parseManifests(rendered.stdout));
        for (const [name, keys] of missing) {
          const literals = [...keys].map((key) => `--from-literal=${key}=nightly-dummy-value`);
          await kubectl(["create", "secret", "generic", name, "--namespace", namespace, ...literals]);
        }
        const tls = missingTlsSecrets(parseManifests(rendered.stdout));
        for (const name of tls) await createTlsSecret(ctx, namespace, name);
        const created = [...missing.keys(), ...tls];
        return created.length === 0 ? "none needed" : `${created.length} created: ${created.join(", ")}`;
      })) &&
      (await check("helm install --wait", async () => {
        await helm(
          [
            "upgrade", "--install", RELEASE, ctx.chartDir,
            "--namespace", namespace,
            ...valuesArgs(ctx.chartDir, shape),
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
        const job = await kubectl(["get", "--namespace", namespace, `job/${jobs[0]}`, "--output", "json"]);
        const pods = await kubectl(["get", "pods", "--namespace", namespace, "--output", "json"]);
        const parsed = JSON.parse(job.stdout) as { status?: { completionTime?: string } };
        const problems = migrationOrderProblems(parsed, JSON.parse(pods.stdout) as { items?: unknown[] });
        if (problems.length > 0) throw new Error(problems.join("; "));
        return `job/${jobs[0]} completed at ${parsed.status?.completionTime}; every service container started after it`;
      })) &&
      (await check("the migration Job generated every password into its Secret", async () => {
        // Laysi/trustiot-v2#60: the chart declares the password Secrets empty and the Job fills
        // them on the first install. The Secret's JSON holds the passwords: it is read quietly
        // and only key names reach the log and the report.
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
          const status = await kubectl(["get", "--namespace", namespace, target, "--output", "json"]);
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
      (await check("no pod is broken, not ready or crash-looping", async () => {
        const pods = await kubectl(["get", "pods", "--namespace", namespace, "--output", "json"]);
        const list = JSON.parse(pods.stdout) as { items?: unknown[] };
        const problems = podProblems(list);
        if (problems.length > 0) throw new Error(problems.map((p) => `${p.pod}: ${p.problem}`).join("; "));
        const restarts = restartsOf(list);
        const count = list.items?.length ?? 0;
        return restarts.size === 0
          ? `${count} pods running or completed`
          : `${count} pods running or completed; restarts: ${[...restarts].map(([pod, n]) => `${pod}=${n}`).join(", ")}`;
      })) &&
      (await check("each Gateway's NetworkPolicy admits exactly the ports its Istio pods use", async () => {
        // The chart derives the ingress gateway's NetworkPolicy ports from its Gateway and Istio's
        // status port; this compares them with the Deployment and Service the installed Istio made.
        const objects = parseManifests(manifest);
        const found: string[] = [];
        for (const gateway of gatewaysOf(objects)) {
          const generated = await kubectl([
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
            const slices = await kubectl([
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
      (await check("ingress e2e test (deploy/chart/tests/e2e)", () => runIngressE2e(ctx, shape.name)));
  } catch (error) {
    // The checks catch their own errors; this is a bug in the smoke test itself.
    const detail = error instanceof Error ? (error.stack ?? error.message) : String(error);
    checks.push({ name: "smoke test internal error", ok: false, detail, durationMs: 0 });
    passed = false;
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
 * A self-signed certificate under the given Secret name, for a Gateway listener. The key lives in
 * a private temporary directory only as long as the command needs it and is never logged.
 */
async function createTlsSecret(ctx: SmokeContext, namespace: string, name: string): Promise<void> {
  const dir = mkdtempSync(join(tmpdir(), "nightly-tls-"));
  try {
    const cert = join(dir, "tls.crt");
    const key = join(dir, "tls.key");
    await run(ctx.log, ["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-subj", "/CN=nightly.test", "-days", "1", "-keyout", key, "-out", cert], { timeoutMs: 30_000 });
    await run(
      ctx.log,
      [ctx.kubectl, "create", "secret", "tls", name, "--namespace", namespace, `--cert=${cert}`, `--key=${key}`],
      { env: { KUBECONFIG: ctx.kubeconfig }, timeoutMs: 60_000 },
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

/** Cluster state and pod logs of a failed shape. Nothing here prints a Secret's data. */
async function collectDiagnostics(ctx: SmokeContext, namespace: string, shape: Shape): Promise<string> {
  const kube = { env: { KUBECONFIG: ctx.kubeconfig }, timeoutMs: 60_000 };
  const sections: string[] = [];
  const section = async (title: string, cmd: string[]): Promise<void> => {
    const result = await exec(cmd, kube);
    sections.push(`### ${title}\n${(result.stdout + result.stderr).trim()}\n`);
  };
  const k = ctx.kubectl;
  // Helm 4 has no `--all`; with a state flag set it lists only those states, so name them all.
  await section("helm list", [ctx.helm, "list", "--namespace", namespace, "--deployed", "--failed", "--pending", "--superseded", "--uninstalled", "--uninstalling"]);
  await section("objects", [k, "get", "all,configmap,serviceaccount,pvc", "--namespace", namespace, "--output", "wide"]);
  // The bundled Postgres: the operator's view of it (no Secret data).
  await section("CloudNativePG", [k, "get", "clusters.postgresql.cnpg.io,databases.postgresql.cnpg.io,imagecatalogs.postgresql.cnpg.io", "--namespace", namespace, "--output", "wide"]);
  await section("describe CloudNativePG Clusters", [k, "describe", "clusters.postgresql.cnpg.io", "--namespace", namespace]);
  await section("CloudNativePG operator log", [k, "logs", "deployment/cnpg-controller-manager", "--namespace", "cnpg-system", "--tail=80"]);
  await section("events", [k, "get", "events", "--namespace", namespace, "--sort-by=.lastTimestamp"]);
  await section("pods in all namespaces", [k, "get", "pods", "--all-namespaces", "--output", "wide"]);
  await section("describe pods", [k, "describe", "pods", "--namespace", namespace]);
  const names = await exec([k, "get", "pods", "--namespace", namespace, "--output", "name"], kube);
  for (const pod of names.stdout.split("\n").filter((line) => line !== "")) {
    await section(`logs ${pod}`, [k, "logs", pod, "--namespace", namespace, "--all-containers", "--tail=80"]);
  }
  const text = sections.join("\n");
  writeFileSync(join(ctx.outDir, `diagnostics-${shape.name}.txt`), text);
  return text;
}
