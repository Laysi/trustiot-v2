// Bundled Postgres e2e test (Laysi/trustiot-v2#58, Laysi/trustiot-v2#60; decisions 21 and 29,
// checklist M3 and M8): installs the chart with `postgres.mode: bundled`, the bundled ClickHouse,
// the collector and the migration Job in one `helm install`, as a real install does, into an
// existing namespace of a cluster where the CloudNativePG operator runs
// (deploy/cloudnative-pg/README.md), then checks that
//   1. the Cluster becomes healthy with one instance outside the mesh, under the restricted Pod
//      Security Standard, and the data survives `helm uninstall`;
//   2. the server runs with TLS, Apache AGE preloaded and installed at the image's version, the
//      built-in C.UTF-8 locale, no superuser password and an ordinary database owner;
//   3. the server refuses a TCP connection without TLS, whatever the client asks for;
//   4. the migration Job generates the passwords into the empty Secrets the chart declares, and a
//      service that lists postgres under mesh.calls (identity, played by the example service)
//      connects with verify-full as the role the Job created, using the CA certificate the chart
//      mounts and the read-write Service's host name; the collector starts with its password too,
//      and the Job logs in to ClickHouse with the admin password that ClickHouse's init container
//      generated (Laysi/trustiot-v2#403);
//   5. the same pod is refused with another CA, with a host name the certificate does not list, and
//      without the labels the Postgres NetworkPolicy admits;
//   6. `helm upgrade` with the same values and `helm rollback` keep the generated passwords; a new
//      migrate.passwordGeneration replaces them and the service and the collector become ready
//      again with the new ones; a lost key is regenerated when the Job runs again
//      (migrate.revision). Passwords are compared by digest inside this script; it prints and
//      records only "unchanged" or "changed", never a value or a digest.
// README.md in this directory lists the prerequisites; run from the repository root:
//
//   E2E_NAMESPACE=e2e-60 E2E_POSTGRES_IMAGE=<image> E2E_SERVICE_IMAGE=<image> E2E_MIGRATE_IMAGE=<image> \
//     bun test --timeout 900000 ./deploy/chart/tests/e2e/postgres.e2e.ts
//
// Not part of `bun test deploy/chart` (the file name has no .test. suffix): it needs a cluster.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { isAbsolute, join } from "node:path";

const CHART_DIR = join(import.meta.dir, "..", "..");
const REPO_ROOT = join(CHART_DIR, "..", "..");

function required(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required (see deploy/chart/tests/e2e/README.md)`);
  return value;
}

const NAMESPACE = required("E2E_NAMESPACE");
const POSTGRES_IMAGE = required("E2E_POSTGRES_IMAGE");
const SERVICE_IMAGE = required("E2E_SERVICE_IMAGE");
const MIGRATE_IMAGE = required("E2E_MIGRATE_IMAGE");
const SHAPE = process.env.E2E_SHAPE ?? "onprem";
const CONTEXT = process.env.E2E_KUBECONTEXT;
const RELEASE = process.env.E2E_RELEASE ?? "trustiot";
const PULL_POLICY = process.env.E2E_IMAGE_PULL_POLICY ?? "IfNotPresent";
const REPORT_DIR = process.env.E2E_REPORT_DIR;
const KEEP = process.env.E2E_KEEP === "1";
if (SHAPE !== "saas" && SHAPE !== "onprem") throw new Error(`E2E_SHAPE must be saas or onprem, not ${SHAPE}`);

const PREFIX = RELEASE.includes("trustiot") ? RELEASE : `${RELEASE}-trustiot`;
// The chart's "trustiot.postgres.clusterName" for names that fit in 50 characters; a longer one
// gets a hash the test does not compute.
const CLUSTER = `${PREFIX}-postgres`;
if (CLUSTER.length > 50) throw new Error(`E2E_RELEASE ${RELEASE} is too long: the Cluster name ${CLUSTER} must fit in 50 characters`);
const DATABASE = "trustiot";
const OWNER = "trustiot_owner";
const RUN = crypto.randomUUID().replaceAll("-", "").slice(0, 8);
// The service that uses Postgres: identity, whose role the migration Job's registry knows, played
// by the example service (its readiness opens a verify-full session as that role).
const SERVICE = "identity";
const ROLE = "identity_app";
// The password Secrets the chart declares and the Job fills (templates/passwords.yaml).
const SERVICE_SECRET = `${PREFIX}-${SERVICE}-password`;
const COLLECTOR_SECRET = `${PREFIX}-otel-collector-password`;
const PASSWORD_SECRETS = [SERVICE_SECRET, COLLECTOR_SECRET];
// The bundled ClickHouse's admin password, which its pod's init container generates into the
// empty Secret the chart declares and the Job reads (Laysi/trustiot-v2#403).
const ADMIN_SECRET = `${PREFIX}-clickhouse-admin-password`;
const ADMIN_ACCOUNT = "trustiot_admin";
// Names of their own, so the test never replaces a Secret or object it did not create.
const PROBE = `trustiot-e2e-probe-${RUN}`;
// deploy/images/postgres/Containerfile: `ARG AGE_VERSION=<version>`.
const AGE_VERSION = readFileSync(join(REPO_ROOT, "deploy/images/postgres/Containerfile"), "utf8").match(/^ARG AGE_VERSION=(\S+)$/m)?.[1];

function pinnedTool(tool: string): string {
  const result = Bun.spawnSync({ cmd: ["proto", "bin", tool, "--reporter", "text"], cwd: REPO_ROOT });
  const path = result.stdout.toString().trim().split("\n").at(-1)?.trim() ?? "";
  if (result.exitCode !== 0 || !isAbsolute(path) || !existsSync(path)) {
    throw new Error(`cannot find the pinned ${tool}; run \`proto install\``);
  }
  return path;
}
const HELM = pinnedTool("helm");
const KUBECTL = pinnedTool("kubectl");

interface Result {
  code: number;
  stdout: string;
  stderr: string;
}
function exec(cmd: string[], stdin?: string): Result {
  const proc = Bun.spawnSync({
    cmd,
    cwd: CHART_DIR,
    stdin: stdin === undefined ? "ignore" : new TextEncoder().encode(stdin),
    stdout: "pipe",
    stderr: "pipe",
  });
  return { code: proc.exitCode ?? -1, stdout: proc.stdout.toString(), stderr: proc.stderr.toString() };
}
function run(cmd: string[], stdin?: string): string {
  const result = exec(cmd, stdin);
  if (result.code !== 0) throw new Error(`${cmd.slice(0, 5).join(" ")} failed (${result.code}):\n${result.stderr}`);
  return result.stdout;
}
const kubectlArgs = () => [KUBECTL, ...(CONTEXT ? ["--context", CONTEXT] : []), "--namespace", NAMESPACE];
const kubectl = (...args: string[]) => run([...kubectlArgs(), ...args]);
const kubectlIn = (stdin: string, ...args: string[]) => run([...kubectlArgs(), ...args], stdin);
const helm = (...args: string[]) =>
  run([HELM, ...(CONTEXT ? ["--kube-context", CONTEXT] : []), "--namespace", NAMESPACE, ...args]);
const json = <T>(...args: string[]) => JSON.parse(kubectl(...args, "--output", "json")) as T;

async function waitFor<T>(what: string, check: () => T | undefined | Promise<T | undefined>, timeoutMs = 300_000) {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  while (Date.now() < deadline) {
    try {
      const value = await check();
      if (value !== undefined) return value;
    } catch (error) {
      last = error;
    }
    await Bun.sleep(3_000);
  }
  throw new Error(`timed out after ${timeoutMs / 1000} s waiting for ${what}${last ? `; last error: ${String(last)}` : ""}`);
}

interface ClusterStatus {
  phase?: string;
  instances?: number;
  readyInstances?: number;
  currentPrimary?: string;
  conditions?: { type: string; status: string }[];
}
const clusterStatus = () => json<{ status?: ClusterStatus }>("get", `cluster.postgresql.cnpg.io/${CLUSTER}`).status ?? {};
const primary = () => {
  const name = clusterStatus().currentPrimary;
  if (!name) throw new Error(`Cluster ${CLUSTER} has no current primary`);
  return name;
};

// SQL in the primary as the postgres role, over the pod's local socket (peer authentication, the
// way the operator works): no password exists for it. The SQL comes on stdin.
function psql(sql: string, database = DATABASE): string {
  return kubectlIn(
    sql,
    "exec",
    "-i",
    primary(),
    "-c",
    "postgres",
    "--",
    "psql",
    "--no-psqlrc",
    "--quiet",
    "--tuples-only",
    "--no-align",
    "--set",
    "ON_ERROR_STOP=1",
    "--dbname",
    database,
  ).trim();
}

function imageParts(image: string, what: string) {
  const match = /^(?<repository>[^@]+?)(?::(?<tag>[\w][\w.-]*))?(?:@(?<digest>sha256:[a-f0-9]{64}))?$/.exec(image);
  if (!match?.groups?.repository) throw new Error(`${what} is not an image reference: ${image}`);
  const { repository, tag, digest } = match.groups;
  return { repository, tag: tag ?? "latest", digest: digest ?? "" };
}
function imageValues(path: string, image: string, what: string): string[] {
  const { repository, tag, digest } = imageParts(image, what);
  return [
    "--set-string",
    `${path}.repository=${repository}`,
    "--set-string",
    `${path}.tag=${tag}`,
    "--set-string",
    `${path}.digest=${digest}`,
    "--set-string",
    `${path}.pullPolicy=${PULL_POLICY}`,
  ];
}

const work = mkdtempSync(join(tmpdir(), "trustiot-postgres-e2e-"));
// What this run created and still has to remove; the uninstall test hands the kept Cluster over.
const created = { passwordSecrets: false, release: false, cluster: false, clickhouse: false, probePolicy: false, probes: [] as string[] };
const report: Record<string, unknown> = { run: RUN, shape: SHAPE, namespace: NAMESPACE, release: RELEASE, cluster: CLUSTER };

const base = [
  "--values",
  `values-${SHAPE}.yaml`,
  "--values",
  `tests/values/${SHAPE}-instance.yaml`,
  "--values",
  "tests/e2e/values-postgres-e2e.yaml",
  ...imageValues("postgres.bundled.image", POSTGRES_IMAGE, "E2E_POSTGRES_IMAGE"),
  ...imageValues(`services.${SERVICE}.image`, SERVICE_IMAGE, "E2E_SERVICE_IMAGE"),
  ...imageValues("migrate.image", MIGRATE_IMAGE, "E2E_MIGRATE_IMAGE"),
];

interface StoredSecret {
  metadata: {
    uid?: string;
    annotations?: Record<string, string>;
    labels?: Record<string, string>;
    managedFields?: { manager: string; fieldsV1?: { "f:data"?: Record<string, unknown> } }[];
  };
  data?: Record<string, string>;
}
const secret = (name: string) => json<StoredSecret>("get", "secret", name);
// A digest of every password Secret's data, kept in this process only: compared, never printed.
function passwordDigest(): string {
  const hash = new Bun.CryptoHasher("sha256");
  for (const name of PASSWORD_SECRETS) {
    const data = secret(name).data ?? {};
    for (const key of Object.keys(data).sort()) hash.update(`${name}/${key}=${data[key]}\n`);
  }
  return hash.digest("hex");
}
const compared = (before: string, after: string) => (before === after ? "unchanged" : "changed");

// What the password Secrets hold, by name only: their keys, the generation that made them, whether
// the Job bound the data to this object (its uid) and who wrote the data. kubectl leaves
// managedFields out of its output unless asked (found by the lab run of 2026-10-13, where
// dataManagers came back empty).
function passwordSecretsShape() {
  return PASSWORD_SECRETS.map((name) => {
    const found = json<StoredSecret>("get", "secret", name, "--show-managed-fields");
    return {
      name,
      keys: Object.keys(found.data ?? {}).sort(),
      generation: found.metadata.annotations?.["trustiot.io/password-generation"],
      bound: found.metadata.uid !== undefined && found.metadata.annotations?.["trustiot.io/password-secret-uid"] === found.metadata.uid,
      keep: found.metadata.annotations?.["helm.sh/resource-policy"],
      filledBy: found.metadata.labels?.["trustiot.io/filled-by"],
      dataManagers: (found.metadata.managedFields ?? []).filter((f) => f.fieldsV1?.["f:data"] !== undefined).map((f) => f.manager),
    };
  });
}

const jobName = () => {
  const jobs = json<{ items: { metadata: { name: string }; status?: { succeeded?: number } }[] }>(
    "get",
    "jobs",
    "--selector",
    `app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=migrate`,
  ).items;
  if (jobs.length !== 1) throw new Error(`expected one migration Job, found ${jobs.length}`);
  return jobs[0]?.metadata.name ?? "";
};

// helm upgrade with the test's values plus `extra`, waiting for every workload and the Job.
function upgrade(...extra: string[]) {
  helm("upgrade", RELEASE, ".", ...base, ...extra, "--wait", "--timeout", "10m");
}
// Waits until the service and the collector run only pods of their current template, all Ready.
function rolledOut() {
  for (const deployment of [`${PREFIX}-${SERVICE}`, `${PREFIX}-otel-collector`]) {
    kubectl("rollout", "status", `deployment/${deployment}`, "--timeout=600s");
  }
}

beforeAll(async () => {
  if (!AGE_VERSION) throw new Error("deploy/images/postgres/Containerfile has no ARG AGE_VERSION");
  // The chart refuses a service that uses Postgres with another tag than the migration Job's.
  if (imageParts(SERVICE_IMAGE, "E2E_SERVICE_IMAGE").tag !== imageParts(MIGRATE_IMAGE, "E2E_MIGRATE_IMAGE").tag) {
    throw new Error("E2E_SERVICE_IMAGE and E2E_MIGRATE_IMAGE must have the same tag: the chart holds a service that uses Postgres to the migration Job's release");
  }
  // Never touch an existing installation (the same check as cluster.ts).
  const status = exec([HELM, ...(CONTEXT ? ["--kube-context", CONTEXT] : []), "--namespace", NAMESPACE, "status", RELEASE]);
  if (status.code === 0) throw new Error(`release ${RELEASE} already exists in ${NAMESPACE}; use another E2E_RELEASE or namespace`);
  if (!status.stderr.includes("not found")) throw new Error(`helm status ${RELEASE} failed:\n${status.stderr}`);
  // The operator is a platform component (deploy/cloudnative-pg); discovery needs no cluster-wide rights.
  const resources = run([KUBECTL, ...(CONTEXT ? ["--context", CONTEXT] : []), "api-resources", "--api-group=postgresql.cnpg.io", "--output", "name"]);
  for (const kind of ["clusters", "imagecatalogs", "databases"]) {
    if (!resources.split("\n").includes(`${kind}.postgresql.cnpg.io`)) {
      throw new Error(`the cluster has no ${kind}.postgresql.cnpg.io; install CloudNativePG first (deploy/cloudnative-pg/README.md)`);
    }
  }
  // The Cluster's name follows the release, and an uninstalled release keeps its Cluster, image
  // catalog and volume (helm.sh/resource-policy: keep). Cleanup deletes them by that name, so
  // refuse to start when any of them is already there: they hold someone's data.
  // The password Secrets are kept the same way, and would hand this run an earlier run's passwords.
  const leftovers = [
    kubectl("get", `cluster.postgresql.cnpg.io/${CLUSTER}`, `imagecatalog.postgresql.cnpg.io/${CLUSTER}`, "--ignore-not-found", "--output", "name"),
    kubectl("get", "pvc", "--selector", `cnpg.io/cluster=${CLUSTER}`, "--output", "name"),
    kubectl("get", "pvc", `data-${PREFIX}-clickhouse-0`, "--ignore-not-found", "--output", "name"),
    kubectl("get", "secret", ...PASSWORD_SECRETS, ADMIN_SECRET, "--ignore-not-found", "--output", "name"),
  ]
    .join("\n")
    .split("\n")
    .filter((line) => line.trim() !== "");
  if (leftovers.length > 0) {
    throw new Error(
      `${NAMESPACE} already has ${leftovers.join(", ")}, which this run would delete when it cleans up; use another E2E_RELEASE or namespace, or remove them yourself first`,
    );
  }

  // One install, as a real one: the databases, the migration Job, which waits for them, generates
  // the passwords and creates the roles, and the service and the collector, which wait for the
  // Job. From here on the Cluster, its image catalog, the volumes and the password Secrets are
  // this run's (checked above), so cleanup removes them even when the install fails halfway.
  const started = Date.now();
  created.release = true;
  created.cluster = true;
  created.clickhouse = true;
  created.passwordSecrets = true;
  helm("install", RELEASE, ".", ...base, "--rollback-on-failure", "--wait", "--timeout", "15m");
  const helmSeconds = (Date.now() - started) / 1000;
  const healthyAtHelmReturn = clusterStatus().phase === "Cluster in healthy state";
  kubectl("wait", `cluster.postgresql.cnpg.io/${CLUSTER}`, "--for=condition=Ready", "--timeout=600s");
  await waitFor(`Database ${CLUSTER}-${DATABASE} applied`, () => {
    const status = json<{ status?: { applied?: boolean; message?: string } }>("get", `database.postgresql.cnpg.io/${CLUSTER}-${DATABASE}`).status;
    return status?.applied === true ? true : undefined;
  });
  report.install = { helmSeconds, healthyAtHelmReturn, readySeconds: (Date.now() - started) / 1000, job: jobName() };
}, 1_200_000);

afterAll(() => {
  if (REPORT_DIR) {
    mkdirSync(REPORT_DIR, { recursive: true });
    writeFileSync(join(REPORT_DIR, `postgres-e2e-${SHAPE}.json`), `${JSON.stringify(report, null, 2)}\n`);
  }
  rmSync(work, { recursive: true, force: true });
  if (KEEP) return;
  const errors: string[] = [];
  const steps: [boolean, () => unknown][] = [
    [created.probes.length > 0, () => kubectl("delete", "pod", ...created.probes, "--ignore-not-found", "--wait=true")],
    [created.probePolicy, () => kubectl("delete", "networkpolicy", PROBE, "--ignore-not-found")],
    [created.release, () => helm("uninstall", RELEASE, "--wait", "--timeout", "3m")],
    // Kept on uninstall (helm.sh/resource-policy: keep), like the Cluster below.
    [created.passwordSecrets, () => kubectl("delete", "secret", ...PASSWORD_SECRETS, ADMIN_SECRET, "--ignore-not-found")],
    // The ClickHouse StatefulSet keeps its volume on uninstall (persistentVolumeClaimRetentionPolicy).
    [created.clickhouse, () => kubectl("delete", "pvc", `data-${PREFIX}-clickhouse-0`, "--ignore-not-found", "--wait=true", "--timeout=180s")],
    // Kept on uninstall (helm.sh/resource-policy: keep): deleting the Cluster deletes its volume.
    [created.cluster, () => kubectl("delete", `cluster.postgresql.cnpg.io/${CLUSTER}`, "--ignore-not-found", "--wait=true", "--timeout=180s")],
    [created.cluster, () => kubectl("delete", `imagecatalog.postgresql.cnpg.io/${CLUSTER}`, "--ignore-not-found", "--wait=true")],
    [created.cluster, () => kubectl("delete", "pvc", "--selector", `cnpg.io/cluster=${CLUSTER}`, "--ignore-not-found", "--wait=true", "--timeout=180s")],
  ];
  for (const [done, cleanup] of steps) {
    if (!done) continue;
    try {
      cleanup();
    } catch (error) {
      errors.push(String(error));
    }
  }
  if (errors.length) throw new Error(`cleanup failed:\n${errors.join("\n")}`);
}, 600_000);

interface Pod {
  metadata: { name: string; labels?: Record<string, string> };
  spec: {
    containers: { name: string; env?: { name: string; value?: string }[] }[];
    volumes?: { name: string; secret?: { secretName: string; items?: { key: string; path: string }[] } }[];
    securityContext?: Record<string, unknown>;
  };
  status: { phase?: string; podIP?: string; conditions?: { type: string; status: string }[] };
}
const ready = (pod: Pod) => pod.status.conditions?.some((c) => c.type === "Ready" && c.status === "True") === true;

// A standalone pod from the example service's own pod template, changed by `edit`. It never gets
// the Deployment's pod-template-hash, so the ReplicaSet does not adopt it.
function startProbe(suffix: string, edit: (spec: Pod["spec"] & Record<string, unknown>, labels: Record<string, string>) => void): string {
  if (!created.probePolicy) {
    // A probe whose labels no chart policy selects still needs the kubelet's readiness probe,
    // or its Postgres check never runs: admit the node source on the health port, as the chart
    // does for its services ("trustiot.mesh.hostSource").
    created.probePolicy = true;
    kubectlIn(
      JSON.stringify({
        apiVersion: "networking.k8s.io/v1",
        kind: "NetworkPolicy",
        metadata: { name: PROBE },
        spec: {
          podSelector: { matchLabels: { "trustiot-e2e/probe": RUN } },
          policyTypes: ["Ingress"],
          ingress: [{ from: [{ ipBlock: { cidr: "169.254.7.127/32" } }], ports: [{ port: "health", protocol: "TCP" }] }],
        },
      }),
      "apply",
      "-f",
      "-",
    );
  }
  const deployment = json<{ spec: { template: { metadata: { labels: Record<string, string> }; spec: Pod["spec"] & Record<string, unknown> } } }>(
    "get",
    `deployment/${PREFIX}-${SERVICE}`,
  );
  const labels = { ...deployment.spec.template.metadata.labels, "trustiot-e2e/probe": RUN };
  const spec = structuredClone(deployment.spec.template.spec);
  edit(spec, labels);
  const name = `${PROBE}-${suffix}`;
  created.probes.push(name);
  kubectlIn(JSON.stringify({ apiVersion: "v1", kind: "Pod", metadata: { name, labels }, spec }), "apply", "-f", "-");
  return name;
}

// The service's readiness log for Postgres (service-kit logs a failed check once, as JSON).
async function readinessFailure(pod: string): Promise<string> {
  return waitFor(`a failed Postgres readiness check in ${pod}`, () => {
    const logs = exec([...kubectlArgs(), "logs", pod]).stdout;
    const line = logs.split("\n").find((l) => l.includes("readiness check failed") && l.includes("postgres"));
    return line;
  }, 180_000);
}

describe(`bundled Postgres (${SHAPE} shape)`, () => {
  test("the Cluster is healthy with one instance outside the mesh, admitted under the restricted Pod Security Standard", () => {
    const status = clusterStatus();
    expect(status.phase).toBe("Cluster in healthy state");
    expect(status.instances).toBe(1);
    expect(status.readyInstances).toBe(1);
    const pods = json<{ items: Pod[] }>("get", "pods", "--selector", `cnpg.io/cluster=${CLUSTER},cnpg.io/podRole=instance`).items;
    expect(pods.map((p) => p.metadata.name)).toEqual([primary()]);
    const pod = pods[0] as Pod;
    expect(ready(pod)).toBe(true);
    // The NetworkPolicy selects these labels; the pod stays out of the ambient mesh.
    expect(pod.metadata.labels?.["istio.io/dataplane-mode"]).toBe("none");
    report.instance = { name: pod.metadata.name, labels: pod.metadata.labels };
    // What the restricted Pod Security Standard requires of every container, init containers
    // included (README: run in a namespace that enforces it, where such a pod is not even admitted).
    const spec = pod.spec as Pod["spec"] & {
      initContainers?: Pod["spec"]["containers"];
      securityContext?: { runAsNonRoot?: boolean; seccompProfile?: { type: string } };
    };
    const containers = [...(spec.initContainers ?? []), ...spec.containers] as (Pod["spec"]["containers"][number] & {
      securityContext?: { allowPrivilegeEscalation?: boolean; runAsNonRoot?: boolean; capabilities?: { drop?: string[]; add?: string[] }; seccompProfile?: { type: string } };
    })[];
    report.podSecurity = { pod: spec.securityContext, containers: containers.map((c) => ({ name: c.name, securityContext: c.securityContext })) };
    for (const container of containers) {
      const context = container.securityContext ?? {};
      expect(context.allowPrivilegeEscalation, container.name).toBe(false);
      expect(context.capabilities?.drop, container.name).toContain("ALL");
      expect(context.capabilities?.add ?? [], container.name).toEqual([]);
      expect(context.runAsNonRoot ?? spec.securityContext?.runAsNonRoot, container.name).toBe(true);
      expect((context.seccompProfile ?? spec.securityContext?.seccompProfile)?.type, container.name).toBe("RuntimeDefault");
    }
  });

  test("the server runs with TLS, AGE preloaded at the image's version, the built-in locale, no superuser password and an owner without superuser rights", () => {
    const settings = psql(
      "SELECT current_setting('ssl'), current_setting('shared_preload_libraries'), current_setting('server_version_num')::int / 10000, current_setting('max_connections'), current_setting('shared_buffers');",
    ).split("|");
    expect(settings[0]).toBe("on");
    expect(settings[1]?.split(",").map((s) => s.trim())).toContain("age");
    expect(settings[2]).toBe("18");
    expect(settings[3]).toBe("100");
    expect(settings[4]).toBe("128MB");
    expect(psql("SELECT extversion FROM pg_extension WHERE extname = 'age';")).toBe(AGE_VERSION ?? "");
    const locale = psql(`SELECT datlocprovider, datlocale, pg_encoding_to_char(encoding), pg_get_userbyid(datdba) FROM pg_database WHERE datname = '${DATABASE}';`);
    expect(locale).toBe(`b|C.UTF-8|UTF8|${OWNER}`);
    const roles = psql(
      `SELECT rolname, rolsuper, rolcreaterole, rolcreatedb, rolbypassrls, rolcanlogin, rolpassword IS NULL FROM pg_authid WHERE rolname IN ('postgres', '${OWNER}', '${ROLE}') ORDER BY rolname;`,
    )
      .split("\n")
      .map((line) => line.split("|"));
    report.roles = roles.map(([name, superuser, createrole, createdb, bypassrls, login, noPassword]) => ({
      name,
      superuser,
      createrole,
      createdb,
      bypassrls,
      login,
      noPassword,
    }));
    expect(roles).toEqual([
      // The service's role, which the migration Job created: a login without any privilege beyond
      // its own.
      [ROLE, "f", "f", "f", "f", "t", "f"],
      // The superuser has no password: only the local socket in the pod reaches it.
      ["postgres", "t", "t", "t", "t", "t", "t"],
      // The database owner: a login role, no superuser, CREATEDB or BYPASSRLS. It has CREATEROLE
      // (postInitApplicationSQL) because the migration Job logs in as it and creates the services'
      // roles (Laysi/trustiot-v2#60).
      [OWNER, "f", "t", "f", "f", "t", "f"],
    ]);
    // No Secret with superuser credentials.
    expect(exec([...kubectlArgs(), "get", "secret", `${CLUSTER}-superuser`]).stderr).toContain("NotFound");
    // No temporary tables for anyone (crates/service-kit/sql/database.sql, run by the Job).
    expect(psql(`SELECT has_database_privilege('${ROLE}', '${DATABASE}', 'TEMPORARY');`)).toBe("f");
  });

  test("AGE runs cypher() in the bundled database", () => {
    const graph = `e2e_${RUN}`;
    // As the superuser over the local socket: which grants the services' and tenants' roles need
    // on ag_catalog is Laysi/trustiot-v2#60's; the owner's privileges are recorded for it.
    report.ownerAgePrivileges = psql(
      `SELECT has_schema_privilege('${OWNER}', 'ag_catalog', 'USAGE'), has_table_privilege('${OWNER}', 'ag_catalog.ag_graph', 'INSERT'), has_table_privilege('${OWNER}', 'ag_catalog.ag_label', 'SELECT');`,
    );
    const out = psql(
      [
        "SET search_path = ag_catalog, \"$user\", public;",
        `SELECT 'created' FROM create_graph('${graph}');`,
        `SELECT * FROM cypher('${graph}', $$ CREATE (:Machine {name: 'press-1'})-[:FEEDS]->(:Machine {name: 'oven-2'}) $$) AS (a agtype);`,
        `SELECT * FROM cypher('${graph}', $$ MATCH (a:Machine)-[:FEEDS]->(b:Machine) RETURN a.name, b.name $$) AS (a agtype, b agtype);`,
        `SELECT 'dropped' FROM drop_graph('${graph}', true);`,
      ].join("\n"),
    );
    report.age = out.split("\n").filter((line) => line && !line.startsWith("NOTICE"));
    expect(out).toContain('"press-1"|"oven-2"');
    expect(out).toContain("dropped");
  });

  test("the server refuses a TCP connection without TLS", () => {
    // From inside the pod over TCP, so only the chart's pg_hba rule stands between: sslmode=disable
    // sends no TLS request at all.
    const result = exec(
      [...kubectlArgs(), "exec", primary(), "-c", "postgres", "--", "psql", "--no-password", `host=127.0.0.1 port=5432 dbname=${DATABASE} user=${OWNER} sslmode=disable`, "--command", "SELECT 1"],
    );
    report.plaintext = result.stderr.trim();
    expect(result.code).not.toBe(0);
    expect(result.stderr).toContain("no encryption");
  });

  test("the migration Job generated the passwords into the empty Secrets the chart declares, and nothing else holds them", () => {
    const shape = passwordSecretsShape();
    report.passwordSecrets = shape;
    expect(shape).toEqual([
      { name: SERVICE_SECRET, keys: [ROLE], generation: "1", bound: true, keep: "keep", filledBy: "migrate", dataManagers: ["trustiot-migrate"] },
      { name: COLLECTOR_SECRET, keys: ["trustiot_collector"], generation: "1", bound: true, keep: "keep", filledBy: "migrate", dataManagers: ["trustiot-migrate"] },
    ]);
    for (const name of PASSWORD_SECRETS) {
      for (const value of Object.values(secret(name).data ?? {})) {
        // 64 hex digits, base64-encoded: 88 characters.
        expect(Buffer.from(value, "base64").toString()).toMatch(/^[0-9a-f]{64}$/);
      }
    }
    // The Job's log names what it generated, never a value.
    const logs = kubectl("logs", `job/${jobName()}`);
    expect(logs).toContain(`generated the password of ${ROLE} in Secret ${SERVICE_SECRET} (it had none)`);
    expect(logs).toContain(`generated the password of trustiot_collector in Secret ${COLLECTOR_SECRET} (it had none)`);
    for (const name of PASSWORD_SECRETS) {
      for (const value of Object.values(secret(name).data ?? {})) {
        expect(logs.includes(value) || logs.includes(Buffer.from(value, "base64").toString())).toBe(false);
      }
    }
    // ClickHouse's init container generated the admin password the Job logged in with
    // (Laysi/trustiot-v2#403), into the Secret the chart declares, bound to it like the others.
    const admin = json<StoredSecret>("get", "secret", ADMIN_SECRET, "--show-managed-fields");
    report.adminSecret = {
      keys: Object.keys(admin.data ?? {}),
      generation: admin.metadata.annotations?.["trustiot.io/password-generation"],
      bound: admin.metadata.annotations?.["trustiot.io/password-secret-uid"] === admin.metadata.uid,
    };
    expect(report.adminSecret).toEqual({ keys: [ADMIN_ACCOUNT], generation: "1", bound: true });
    expect(Buffer.from(admin.data?.[ADMIN_ACCOUNT] ?? "", "base64").toString()).toMatch(/^[0-9a-f]{64}$/);
    expect(logs.includes(admin.data?.[ADMIN_ACCOUNT] ?? "-")).toBe(false);
    // The collector started with its password: Ready, not waiting on a missing key.
    const collector = json<{ items: Pod[] }>("get", "pods", "--selector", `app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=otel-collector`).items;
    expect(collector.length).toBe(1);
    expect(ready(collector[0] as Pod)).toBe(true);
  });

  test("a service that lists postgres connects with verify-full, with the mounted CA and its own role", () => {
    const pods = json<{ items: Pod[] }>("get", "pods", "--selector", `app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=${SERVICE}`).items.filter(
      (pod) => pod.metadata.labels?.["pod-template-hash"] !== undefined,
    );
    expect(pods.length).toBe(1);
    const pod = pods[0] as Pod;
    // Ready means its Postgres readiness check passed: a verify-full TLS session as its role.
    expect(ready(pod)).toBe(true);
    const env = Object.fromEntries((pod.spec.containers[0]?.env ?? []).map((e) => [e.name, e.value]));
    expect(env.TRUSTIOT_POSTGRES_CA_FILE).toBe("/etc/trustiot/postgres-ca/ca.crt");
    const volume = pod.spec.volumes?.find((v) => v.name === "postgres-ca");
    // Only the certificate of CloudNativePG's CA Secret, never its key.
    expect(volume?.secret).toMatchObject({ secretName: `${CLUSTER}-ca`, items: [{ key: "ca.crt", path: "ca.crt" }] });
    const config = json<{ data: Record<string, string> }>("get", "configmap", `${PREFIX}-platform`).data;
    expect(config.TRUSTIOT_POSTGRES_HOST).toBe(`${CLUSTER}-rw.${NAMESPACE}.svc`);
    expect(config.TRUSTIOT_POSTGRES_SSLMODE).toBe("verify-full");
    // The server saw the role over TLS.
    const sessions = psql(`SELECT count(*) FROM pg_stat_ssl JOIN pg_stat_activity USING (pid) WHERE usename = '${ROLE}' AND ssl;`);
    report.serviceSessionsOverTls = Number(sessions);
    expect(Number(sessions)).toBeGreaterThan(0);
    const logs = kubectl("logs", pod.metadata.name);
    expect(logs).not.toContain("readiness check failed");
  });

  test("the same pod is refused with another CA, a host name the certificate does not list, or without the admitted labels", async () => {
    // Control: an unchanged copy becomes ready, so the differences below are what is refused.
    const control = startProbe("control", () => {});
    // Another CA: the cluster's own root CA (a public certificate every namespace has).
    const wrongCa = startProbe("wrong-ca", (spec) => {
      spec.volumes = (spec.volumes ?? []).map((v) =>
        v.name === "postgres-ca" ? ({ name: "postgres-ca", configMap: { name: "kube-root-ca.crt", items: [{ key: "ca.crt", path: "ca.crt" }] } } as never) : v,
      );
    });
    // A name the certificate does not list: the primary's pod IP.
    const podIp = json<Pod>("get", "pod", primary()).status.podIP ?? "";
    expect(podIp).not.toBe("");
    const wrongHost = startProbe("wrong-host", (spec) => {
      const container = spec.containers[0];
      if (container) container.env = [...(container.env ?? []), { name: "TRUSTIOT_POSTGRES_HOST", value: podIp }];
    });
    // Not a service that lists postgres: the Postgres NetworkPolicy does not admit it.
    const unlisted = startProbe("unlisted", (_spec, labels) => {
      labels["app.kubernetes.io/component"] = `e2e-unlisted-${RUN}`;
    });

    await waitFor(`${control} ready`, () => (ready(json<Pod>("get", "pod", control)) ? true : undefined), 240_000);
    const failures = {
      wrongCa: await readinessFailure(wrongCa),
      wrongHost: await readinessFailure(wrongHost),
      unlisted: await readinessFailure(unlisted),
    };
    report.refusals = failures;
    expect(failures.wrongCa).toMatch(/UnknownIssuer/);
    expect(failures.wrongHost).toMatch(/not valid for name/i);
    expect(failures.wrongHost).toContain(podIp);
    // The NetworkPolicy drops it, so the connection never reaches Postgres (in ambient the pod's
    // ztunnel resets it): no certificate and no password was ever exchanged.
    expect(failures.unlisted).not.toMatch(/certificate|UnknownIssuer|password/i);
    for (const pod of [wrongCa, wrongHost, unlisted]) expect(ready(json<Pod>("get", "pod", pod))).toBe(false);
  }, 600_000);

  test("helm upgrade with the same values and helm rollback keep the generated passwords", () => {
    for (const probe of created.probes) kubectl("delete", "pod", probe, "--ignore-not-found", "--wait=true");
    created.probes = [];
    const job = jobName();
    const before = passwordDigest();
    upgrade();
    // Same values, same Job: it does not run again, and Helm leaves the data it never rendered.
    expect(jobName()).toBe(job);
    const afterUpgrade = compared(before, passwordDigest());
    helm("rollback", RELEASE, "1", "--wait", "--timeout", "10m");
    const afterRollback = compared(before, passwordDigest());
    report.sameValues = { upgrade: afterUpgrade, rollback: afterRollback };
    console.log(`password Secrets after upgrade with the same values: ${afterUpgrade}; after rollback: ${afterRollback}`);
    expect(afterUpgrade).toBe("unchanged");
    expect(afterRollback).toBe("unchanged");
    rolledOut();
  }, 1_200_000);

  test("a new password generation replaces every password, and the service and the collector become ready with the new ones", () => {
    const job = jobName();
    const before = passwordDigest();
    upgrade("--set", "migrate.passwordGeneration=2");
    rolledOut();
    const after = compared(before, passwordDigest());
    report.rotation = { secrets: after, newJob: jobName() !== job, generations: passwordSecretsShape().map((s) => s.generation) };
    console.log(`password Secrets after migrate.passwordGeneration=2: ${after}`);
    expect(after).toBe("changed");
    expect(jobName()).not.toBe(job);
    expect(passwordSecretsShape().map((s) => s.generation)).toEqual(["2", "2"]);
    const logs = kubectl("logs", `job/${jobName()}`);
    expect(logs).toContain(`generated the password of ${ROLE} in Secret ${SERVICE_SECRET} (password generation 2)`);
    expect(logs).toContain(`set the password of ${ROLE}`);
    expect(logs).toContain("set the password of ClickHouse account trustiot_collector");
    // Ready again: the service's readiness opened a verify-full session with the new password.
    for (const component of [SERVICE, "otel-collector"]) {
      const pods = json<{ items: Pod[] }>("get", "pods", "--selector", `app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=${component}`).items;
      expect(pods.length, component).toBeGreaterThan(0);
      for (const pod of pods) expect(ready(pod), pod.metadata.name).toBe(true);
    }
  }, 1_200_000);

  test("a rollback to the older password generation keeps the new passwords, and so does the upgrade after it", () => {
    // Flux's remediation (strategy: rollback) does this after a failed upgrade: the rollback
    // renders migrate.passwordGeneration=1 again, and generations only go up.
    const before = passwordDigest();
    const revisions = JSON.parse(helm("history", RELEASE, "--output", "json")) as { revision: number }[];
    const beforeRotation = Math.max(...revisions.map((r) => r.revision)) - 1;
    helm("rollback", RELEASE, String(beforeRotation), "--wait", "--timeout", "10m");
    rolledOut();
    const afterRollback = compared(before, passwordDigest());
    const rollbackLog = kubectl("logs", `job/${jobName()}`);
    upgrade("--set", "migrate.passwordGeneration=2");
    rolledOut();
    const afterUpgrade = compared(before, passwordDigest());
    const generations = passwordSecretsShape().map((s) => s.generation);
    report.rollbackAfterRotation = { rollback: afterRollback, upgrade: afterUpgrade, generations };
    console.log(`password Secrets after a rollback to generation 1: ${afterRollback}; after the upgrade back to 2: ${afterUpgrade}`);
    expect(afterRollback).toBe("unchanged");
    expect(afterUpgrade).toBe("unchanged");
    expect(generations).toEqual(["2", "2"]);
    expect(rollbackLog).not.toContain("generated the password");
    expect(kubectl("logs", `job/${jobName()}`)).not.toContain("generated the password");
  }, 1_800_000);

  test("a lost key is regenerated when the Job runs again", () => {
    const before = passwordDigest();
    kubectl("patch", "secret", SERVICE_SECRET, "--type", "json", "--patch", JSON.stringify([{ op: "remove", path: `/data/${ROLE}` }]));
    upgrade("--set", "migrate.passwordGeneration=2", "--set", "migrate.revision=2");
    rolledOut();
    const after = compared(before, passwordDigest());
    report.lostKey = { secrets: after, keys: passwordSecretsShape().map((s) => s.keys) };
    console.log(`password Secrets after a lost key and migrate.revision=2: ${after}`);
    expect(after).toBe("changed");
    expect(Object.keys(secret(SERVICE_SECRET).data ?? {})).toEqual([ROLE]);
    expect(kubectl("logs", `job/${jobName()}`)).toContain(`generated the password of ${ROLE} in Secret ${SERVICE_SECRET} (it had none)`);
  }, 1_200_000);

  test("helm uninstall keeps the database and the passwords: the Cluster, its image catalog, its volume and the password Secrets stay", () => {
    for (const probe of created.probes) kubectl("delete", "pod", probe, "--ignore-not-found", "--wait=true");
    created.probes = [];
    psql(`CREATE TABLE e2e_kept_${RUN} (id int); INSERT INTO e2e_kept_${RUN} VALUES (1);`);
    helm("uninstall", RELEASE, "--wait", "--timeout", "3m");
    created.release = false;
    const kept = kubectl("get", `cluster.postgresql.cnpg.io/${CLUSTER}`, `imagecatalog.postgresql.cnpg.io/${CLUSTER}`, "--output", "name");
    expect(kept.trim().split("\n").sort()).toEqual([`cluster.postgresql.cnpg.io/${CLUSTER}`, `imagecatalog.postgresql.cnpg.io/${CLUSTER}`]);
    // The Database object goes with the release; its reclaim policy keeps the database.
    expect(exec([...kubectlArgs(), "get", `database.postgresql.cnpg.io/${CLUSTER}-${DATABASE}`]).stderr).toContain("NotFound");
    expect(psql(`SELECT count(*) FROM e2e_kept_${RUN};`)).toBe("1");
    const pvcs = kubectl("get", "pvc", "--selector", `cnpg.io/cluster=${CLUSTER}`, "--output", "name").trim();
    // The password Secrets stay too, so a reinstall finds the passwords the roles still have.
    const secrets = kubectl("get", "secret", ...PASSWORD_SECRETS, "--output", "name").trim().split("\n").sort();
    report.keptAfterUninstall = { objects: kept.trim().split("\n"), pvcs: pvcs.split("\n"), secrets };
    expect(pvcs).not.toBe("");
    expect(secrets).toEqual(PASSWORD_SECRETS.map((name) => `secret/${name}`).sort());
  }, 300_000);
});
