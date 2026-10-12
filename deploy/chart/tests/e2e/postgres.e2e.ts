// Bundled Postgres e2e test (Laysi/trustiot-v2#58; decisions 21 and 29, checklist M3 and M8):
// installs the chart with `postgres.mode: bundled` into an existing namespace of a cluster where
// the CloudNativePG operator runs (deploy/cloudnative-pg/README.md), then checks that
//   1. the Cluster becomes healthy with one instance outside the mesh, under the restricted Pod
//      Security Standard, and the data survives `helm uninstall`;
//   2. the server runs with TLS, Apache AGE preloaded and installed at the image's version, the
//      built-in C.UTF-8 locale, no superuser password and an ordinary database owner;
//   3. the server refuses a TCP connection without TLS, whatever the client asks for;
//   4. a service that lists postgres under mesh.calls connects with verify-full, using the CA
//      certificate the chart mounts and the read-write Service's host name, with a role of its own
//      (a CloudNativePG DatabaseRole, the way Laysi/trustiot-v2#60 will add the services' roles);
//   5. the same pod is refused with another CA, with a host name the certificate does not list, and
//      without the labels the Postgres NetworkPolicy admits.
// README.md in this directory lists the prerequisites; run from the repository root:
//
//   E2E_NAMESPACE=e2e-58 E2E_POSTGRES_IMAGE=<image> E2E_SERVICE_IMAGE=<image> \
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
// Names of their own, so the test never replaces a Secret or object it did not create.
const ROLE = `e2e_app_${RUN}`;
const ROLE_OBJECT = `trustiot-e2e-role-${RUN}`;
const PROBE = `trustiot-e2e-probe-${RUN}`;
// deploy/images/postgres/Containerfile: `ARG AGE_VERSION=<version>`.
const AGE_VERSION = readFileSync(join(REPO_ROOT, "deploy/images/postgres/Containerfile"), "utf8").match(/^ARG AGE_VERSION=(\S+)$/m)?.[1];
// Run once per database by its owner before any service connects (decision 29).
const DATABASE_SQL = readFileSync(join(REPO_ROOT, "crates/service-kit/sql/database.sql"), "utf8");

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
const created = { secret: false, role: false, release: false, cluster: false, probePolicy: false, probes: [] as string[] };
const report: Record<string, unknown> = { run: RUN, shape: SHAPE, namespace: NAMESPACE, release: RELEASE, cluster: CLUSTER };

function hex(bytes: number): string {
  return Array.from(crypto.getRandomValues(new Uint8Array(bytes)), (b) => b.toString(16).padStart(2, "0")).join("");
}

const base = [
  "--values",
  `values-${SHAPE}.yaml`,
  "--values",
  `tests/values/${SHAPE}-instance.yaml`,
  "--values",
  "tests/e2e/values-postgres-e2e.yaml",
  ...imageValues("postgres.bundled.image", POSTGRES_IMAGE, "E2E_POSTGRES_IMAGE"),
];

beforeAll(async () => {
  if (!AGE_VERSION) throw new Error("deploy/images/postgres/Containerfile has no ARG AGE_VERSION");
  // Never touch an existing installation (the same check as cluster.ts).
  const status = exec([HELM, ...(CONTEXT ? ["--kube-context", CONTEXT] : []), "--namespace", NAMESPACE, "status", RELEASE]);
  if (status.code === 0) throw new Error(`release ${RELEASE} already exists in ${NAMESPACE}; use another E2E_RELEASE or namespace`);
  if (!status.stderr.includes("not found")) throw new Error(`helm status ${RELEASE} failed:\n${status.stderr}`);
  // The operator is a platform component (deploy/cloudnative-pg); discovery needs no cluster-wide rights.
  const resources = run([KUBECTL, ...(CONTEXT ? ["--context", CONTEXT] : []), "api-resources", "--api-group=postgresql.cnpg.io", "--output", "name"]);
  for (const kind of ["clusters", "imagecatalogs", "databases", "databaseroles"]) {
    if (!resources.split("\n").includes(`${kind}.postgresql.cnpg.io`)) {
      throw new Error(`the cluster has no ${kind}.postgresql.cnpg.io; install CloudNativePG first (deploy/cloudnative-pg/README.md)`);
    }
  }
  // The Cluster's name follows the release, and an uninstalled release keeps its Cluster, image
  // catalog and volume (helm.sh/resource-policy: keep). Cleanup deletes them by that name, so
  // refuse to start when any of them is already there: they hold someone's data.
  const leftovers = [
    kubectl("get", `cluster.postgresql.cnpg.io/${CLUSTER}`, `imagecatalog.postgresql.cnpg.io/${CLUSTER}`, "--ignore-not-found", "--output", "name"),
    kubectl("get", "pvc", "--selector", `cnpg.io/cluster=${CLUSTER}`, "--output", "name"),
  ]
    .join("\n")
    .split("\n")
    .filter((line) => line.trim() !== "");
  if (leftovers.length > 0) {
    throw new Error(
      `${NAMESPACE} already has ${leftovers.join(", ")}, which this run would delete when it cleans up; use another E2E_RELEASE or namespace, or remove them yourself first`,
    );
  }

  // Step 1: the database alone. From here on the Cluster, its image catalog and its volume are
  // this run's (checked above), so cleanup removes them even when the install fails halfway.
  const started = Date.now();
  created.release = true;
  created.cluster = true;
  helm("install", RELEASE, ".", ...base, "--rollback-on-failure", "--wait", "--timeout", "10m");
  const helmSeconds = (Date.now() - started) / 1000;
  const healthyAtHelmReturn = clusterStatus().phase === "Cluster in healthy state";
  kubectl("wait", `cluster.postgresql.cnpg.io/${CLUSTER}`, "--for=condition=Ready", "--timeout=600s");
  await waitFor(`Database ${CLUSTER}-${DATABASE} applied`, () => {
    const status = json<{ status?: { applied?: boolean; message?: string } }>("get", `database.postgresql.cnpg.io/${CLUSTER}-${DATABASE}`).status;
    return status?.applied === true ? true : undefined;
  });
  report.install = { helmSeconds, healthyAtHelmReturn, readySeconds: (Date.now() - started) / 1000 };

  // The service's role, as Laysi/trustiot-v2#60 will declare each service's: a DatabaseRole whose
  // password is in a basic-auth Secret. The password exists only in a private temp file and the Secret.
  const passwordFile = join(work, "password");
  writeFileSync(passwordFile, hex(24), { mode: 0o600 });
  const usernameFile = join(work, "username");
  writeFileSync(usernameFile, ROLE, { mode: 0o600 });
  created.secret = true;
  kubectl("create", "secret", "generic", ROLE_OBJECT, "--type=kubernetes.io/basic-auth", `--from-file=username=${usernameFile}`, `--from-file=password=${passwordFile}`);
  created.role = true;
  kubectlIn(
    JSON.stringify({
      apiVersion: "postgresql.cnpg.io/v1",
      kind: "DatabaseRole",
      metadata: { name: ROLE_OBJECT },
      spec: { cluster: { name: CLUSTER }, name: ROLE, ensure: "present", login: true, passwordSecret: { name: ROLE_OBJECT } },
    }),
    "apply",
    "-f",
    "-",
  );
  await waitFor(`DatabaseRole ${ROLE_OBJECT} applied`, () => {
    const status = json<{ status?: { applied?: boolean } }>("get", `databaserole.postgresql.cnpg.io/${ROLE_OBJECT}`).status;
    return status?.applied === true ? true : undefined;
  });
  // What the migration Job will do as the owner (Laysi/trustiot-v2#60): no temporary tables for
  // anyone, which service-kit requires of every session.
  psql(`SET ROLE ${OWNER};\n${DATABASE_SQL}`);

  // Step 2: the example service, connecting with its role.
  helm(
    "upgrade",
    RELEASE,
    ".",
    ...base,
    "--set",
    "services.assets.enabled=true",
    ...imageValues("services.assets.image", SERVICE_IMAGE, "E2E_SERVICE_IMAGE"),
    "--set-json",
    `services.assets.env=${JSON.stringify([
      { name: "TRUSTIOT__GREETING", value: "hello from the postgres e2e" },
      { name: "TRUSTIOT__DATABASE__USERNAME", value: ROLE },
      { name: "TRUSTIOT__DATABASE__PASSWORD", valueFrom: { secretKeyRef: { name: ROLE_OBJECT, key: "password" } } },
    ])}`,
    "--rollback-on-failure",
    "--wait",
    "--timeout",
    "8m",
  );
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
    // The role goes while its Cluster still runs, so the operator can finish with it.
    [created.role, () => kubectl("delete", `databaserole.postgresql.cnpg.io/${ROLE_OBJECT}`, "--ignore-not-found", "--wait=true", "--timeout=120s")],
    [created.release, () => helm("uninstall", RELEASE, "--wait", "--timeout", "3m")],
    // Kept on uninstall (helm.sh/resource-policy: keep): deleting the Cluster deletes its volume.
    [created.cluster, () => kubectl("delete", `cluster.postgresql.cnpg.io/${CLUSTER}`, "--ignore-not-found", "--wait=true", "--timeout=180s")],
    [created.cluster, () => kubectl("delete", `imagecatalog.postgresql.cnpg.io/${CLUSTER}`, "--ignore-not-found", "--wait=true")],
    [created.cluster, () => kubectl("delete", "pvc", "--selector", `cnpg.io/cluster=${CLUSTER}`, "--ignore-not-found", "--wait=true", "--timeout=180s")],
    [created.secret, () => kubectl("delete", "secret", ROLE_OBJECT, "--ignore-not-found")],
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
    `deployment/${PREFIX}-assets`,
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

  test("the server runs with TLS, AGE preloaded at the image's version, the built-in locale, no superuser password and an ordinary owner", () => {
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
      name: name === ROLE ? "e2e_app_<run>" : name,
      superuser,
      createrole,
      createdb,
      bypassrls,
      login,
      noPassword,
    }));
    expect(roles).toEqual([
      // The service's role: a login without any privilege beyond its own.
      [ROLE, "f", "f", "f", "f", "t", "f"],
      // The superuser has no password: only the local socket in the pod reaches it.
      ["postgres", "t", "t", "t", "t", "t", "t"],
      // The database owner: a login role, no superuser, CREATEROLE or BYPASSRLS.
      [OWNER, "f", "f", "f", "f", "t", "f"],
    ]);
    // No Secret with superuser credentials.
    expect(exec([...kubectlArgs(), "get", "secret", `${CLUSTER}-superuser`]).stderr).toContain("NotFound");
    // No temporary tables for anyone (crates/service-kit/sql/database.sql, run as the owner).
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

  test("a service that lists postgres connects with verify-full, with the mounted CA and its own role", () => {
    const pods = json<{ items: Pod[] }>("get", "pods", "--selector", `app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=assets`).items.filter(
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

  test("helm uninstall keeps the database: the Cluster, its image catalog and its volume stay", () => {
    for (const probe of created.probes) kubectl("delete", "pod", probe, "--ignore-not-found", "--wait=true");
    created.probes = [];
    kubectl("delete", `databaserole.postgresql.cnpg.io/${ROLE_OBJECT}`, "--wait=true", "--timeout=120s");
    created.role = false;
    psql(`CREATE TABLE e2e_kept_${RUN} (id int); INSERT INTO e2e_kept_${RUN} VALUES (1);`);
    helm("uninstall", RELEASE, "--wait", "--timeout", "3m");
    created.release = false;
    const kept = kubectl("get", `cluster.postgresql.cnpg.io/${CLUSTER}`, `imagecatalog.postgresql.cnpg.io/${CLUSTER}`, "--output", "name");
    expect(kept.trim().split("\n").sort()).toEqual([`cluster.postgresql.cnpg.io/${CLUSTER}`, `imagecatalog.postgresql.cnpg.io/${CLUSTER}`]);
    // The Database object goes with the release; its reclaim policy keeps the database.
    expect(exec([...kubectlArgs(), "get", `database.postgresql.cnpg.io/${CLUSTER}-${DATABASE}`]).stderr).toContain("NotFound");
    expect(psql(`SELECT count(*) FROM e2e_kept_${RUN};`)).toBe("1");
    const pvcs = kubectl("get", "pvc", "--selector", `cnpg.io/cluster=${CLUSTER}`, "--output", "name").trim();
    report.keptAfterUninstall = { objects: kept.trim().split("\n"), pvcs: pvcs.split("\n") };
    expect(pvcs).not.toBe("");
  }, 300_000);
});
