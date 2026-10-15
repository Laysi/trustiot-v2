// Bundled Postgres: replication, backup and restore keep working without PUBLIC on the cluster's
// `postgres` and `template1` databases (Laysi/trustiot-v2#400). The chart's Cluster revokes
// CONNECT and TEMPORARY there at bootstrap and gives CONNECT on `postgres` back by name to the two
// roles of CloudNativePG that need it (templates/postgres.yaml, files/postgres/system-databases.sql);
// this test shows on a real cluster that CloudNativePG's own connections still work:
//   1. a newly bootstrapped Cluster from the chart's rendering has no CONNECT or TEMPORARY on
//      either database for PUBLIC, the owner or a role created as the migration Job creates the
//      services' roles, and CONNECT on `postgres` for streaming_replica and cnpg_metrics_exporter;
//   2. replication: the replica streams from the primary and has the primary's data;
//   3. a switchover, after which the former primary rejoins as a replica; its log shows whether
//      pg_rewind ran (as streaming_replica on the new primary's `postgres` database) and how it
//      ended;
//   4. backup through the Barman Cloud plugin: WAL archiving is healthy (the Cluster's
//      ContinuousArchiving condition) and a Backup with `method: plugin` completes;
//   5. restore: a new Cluster bootstraps from that backup through the plugin, has the data and
//      keeps the revocations (they live in the catalog, which the backup carries);
//   6. the instance manager stays healthy throughout: every instance Ready, the metrics exporter
//      collects without errors, and no instance logs a refused database privilege.
//
// The chart keeps `instances: 1` and has no backup settings (backups are Laysi/trustiot-v2#134),
// so this test changes its own copy of the rendered Cluster before applying it, and says so in
// the report: `instances: 2`, the plugin as WAL archiver (`spec.plugins`), smaller resources and
// volume. Everything else of the Cluster (bootstrap, postInitSQL, pg_hba, image catalog, no
// superuser access) is the chart's. It applies only the chart's Postgres objects (ImageCatalog,
// Cluster, NetworkPolicy), plus a NetworkPolicy of its own that lets the Cluster's pods reach its
// instances on 5432: the instances each other, and the Job that clones a new replica from the
// primary (its pods carry cnpg.io/cluster but not cnpg.io/podRole=instance). The chart's admits
// only the services and the migration Job, since it has one instance.
//
// The S3-compatible store is RustFS in this namespace, the image and version that the Barman
// Cloud plugin v0.15.1 runs its own end-to-end tests against (test/e2e/internal/objectstore/s3.go
// in its repository), pinned by digest. Its access keys are random values this process generates
// into a Secret; they are never printed or written to the report.
//
// README.md in this directory lists the prerequisites (CloudNativePG, the Barman Cloud plugin and
// cert-manager installed); run from the repository root:
//
//   E2E_NAMESPACE=e2e-400 E2E_POSTGRES_IMAGE=<image> \
//     bun test --timeout 2400000 ./deploy/chart/tests/e2e/postgres-replication.e2e.ts
//
// Not part of `bun test deploy/chart` (the file name has no .test. suffix): it needs a cluster.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
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
// RustFS 1.0.0 (glibc build), the digest the Barman Cloud plugin v0.15.1 pins in its e2e tests.
const STORE_IMAGE =
  process.env.E2E_STORE_IMAGE ?? "docker.io/rustfs/rustfs:1.0.0-glibc@sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858";
const CONTEXT = process.env.E2E_KUBECONTEXT;
const RELEASE = process.env.E2E_RELEASE ?? "trustiot";
// A Cluster's imagePullPolicy also applies to the containers CloudNativePG adds to each instance:
// the operator's bootstrap-controller and the Barman Cloud plugin's sidecar, which a node pulls from
// ghcr.io. So it is always IfNotPresent; an image imported into the node under a tag other than
// `latest` is then used without a pull.
const PULL_POLICY = "IfNotPresent";
const REPORT_DIR = process.env.E2E_REPORT_DIR;
const KEEP = process.env.E2E_KEEP === "1";

const PREFIX = RELEASE.includes("trustiot") ? RELEASE : `${RELEASE}-trustiot`;
// The chart's "trustiot.postgres.clusterName" for names that fit in 50 characters.
const CLUSTER = `${PREFIX}-postgres`;
const RESTORED = `${PREFIX}-postgres-restored`;
if (RESTORED.length > 50) throw new Error(`E2E_RELEASE ${RELEASE} is too long: ${RESTORED} must fit in 50 characters`);
const DATABASE = "trustiot";
const OWNER = "trustiot_owner";
const RUN = crypto.randomUUID().replaceAll("-", "").slice(0, 8);
// The test's own objects carry the run in their names, so it never replaces one it did not create.
const STORE = `trustiot-e2e-store-${RUN}`;
const BUCKET = "trustiot-e2e";
const PEERS = `trustiot-e2e-postgres-peers-${RUN}`;
const BACKUP = `trustiot-e2e-backup-${RUN}`;
const PLUGIN = "barman-cloud.cloudnative-pg.io";
// A role made the way the migration Job makes a service's login role: by the owner, with
// CREATEROLE (tools/migrate's CREATE_LOGIN_ROLE). The Job itself runs in postgres.e2e.ts.
const SERVICE_LIKE_ROLE = "e2e_app";
const SYSTEM_DATABASES = ["postgres", "template1"];
// CloudNativePG's roles that connect to `postgres` without being superusers.
const OPERATOR_ROLES = ["cnpg_metrics_exporter", "streaming_replica"];

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
const apply = (object: unknown) => run([...kubectlArgs(), "apply", "-f", "-"], JSON.stringify(object));
const json = <T>(...args: string[]) => JSON.parse(kubectl(...args, "--output", "json")) as T;

async function waitFor<T>(what: string, check: () => T | undefined | Promise<T | undefined>, timeoutMs = 600_000) {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  while (Date.now() < deadline) {
    try {
      const value = await check();
      if (value !== undefined) return value;
    } catch (error) {
      last = error;
    }
    await Bun.sleep(5_000);
  }
  throw new Error(`timed out after ${timeoutMs / 1000} s waiting for ${what}${last ? `; last error: ${String(last)}` : ""}`);
}

type Manifest = { apiVersion: string; kind: string; metadata: { name: string; [key: string]: unknown }; spec: Record<string, unknown> };
interface ClusterStatus {
  phase?: string;
  instances?: number;
  readyInstances?: number;
  currentPrimary?: string;
  targetPrimary?: string;
  instanceNames?: string[];
  conditions?: { type: string; status: string; reason?: string; message?: string }[];
}
const clusterStatus = (name: string) => json<{ status?: ClusterStatus }>("get", `cluster.postgresql.cnpg.io/${name}`).status ?? {};
const condition = (name: string, type: string) => clusterStatus(name).conditions?.find((c) => c.type === type);
const primaryOf = (name: string) => {
  const primary = clusterStatus(name).currentPrimary;
  if (!primary) throw new Error(`Cluster ${name} has no current primary`);
  return primary;
};
const instancePods = (name: string) =>
  json<{ items: { metadata: { name: string }; status: { conditions?: { type: string; status: string }[] } }[] }>(
    "get",
    "pods",
    "--selector",
    `cnpg.io/cluster=${name},cnpg.io/podRole=instance`,
  ).items;
const healthy = (name: string, instances: number) => {
  const status = clusterStatus(name);
  const ready = instancePods(name).filter((p) => p.status.conditions?.some((c) => c.type === "Ready" && c.status === "True"));
  return status.phase === "Cluster in healthy state" && status.readyInstances === instances && ready.length === instances ? status : undefined;
};

// SQL in an instance as the postgres role, over the pod's local socket (peer authentication, the
// way the operator works): no password exists for it. The SQL comes on stdin.
function psqlIn(pod: string, sql: string, database = DATABASE): string {
  return run(
    [...kubectlArgs(), "exec", "-i", pod, "-c", "postgres", "--", "psql", "--no-psqlrc", "--quiet", "--tuples-only", "--no-align", "--set", "ON_ERROR_STOP=1", "--dbname", database],
    sql,
  ).trim();
}

function imageParts(image: string, what: string) {
  const match = /^(?<repository>[^@]+?)(?::(?<tag>[\w][\w.-]*))?(?:@(?<digest>sha256:[a-f0-9]{64}))?$/.exec(image);
  if (!match?.groups?.repository) throw new Error(`${what} is not an image reference: ${image}`);
  const { repository, tag, digest } = match.groups;
  return { repository, tag: tag ?? "latest", digest: digest ?? "" };
}

// The privileges on the two system databases of every role that is neither a superuser nor a
// predefined pg_ role, as "role database connect temporary".
function systemDatabasePrivileges(pod: string): string[] {
  return psqlIn(
    pod,
    `SELECT format('%s %s %s %s', r.rolname, d.datname, has_database_privilege(r.oid, d.oid, 'CONNECT'), has_database_privilege(r.oid, d.oid, 'TEMPORARY'))
     FROM pg_roles r CROSS JOIN pg_database d
     WHERE NOT r.rolsuper AND r.rolname !~ '^pg_' AND d.datname IN ('postgres', 'template1') ORDER BY r.rolname, d.datname;`,
    "postgres",
  ).split("\n");
}
// What the revocation must leave: CONNECT on `postgres` for the operator's two roles only.
function expectedPrivileges(roles: string[]): string[] {
  return roles.flatMap((role) =>
    SYSTEM_DATABASES.map((database) => `${role} ${database} ${database === "postgres" && OPERATOR_ROLES.includes(role) ? "t" : "f"} f`),
  );
}

// Lines of an instance's log (JSON, one record per line) whose message or record mentions `what`.
function logLines(pod: string, what: RegExp): string[] {
  const logs = exec([...kubectlArgs(), "logs", pod, "-c", "postgres"]).stdout;
  return logs.split("\n").filter((line) => what.test(line));
}
function messages(lines: string[]): string[] {
  return lines.map((line) => {
    try {
      const record = JSON.parse(line) as { msg?: string; logger?: string; record?: { message?: string } };
      return [record.logger, record.msg, record.record?.message].filter(Boolean).join(": ");
    } catch {
      return line;
    }
  });
}
// The metrics exporter of an instance (plain HTTP on 9187), read from inside its postgres
// container with bash's /dev/tcp: the image has no HTTP client, and the chart's NetworkPolicy
// admits nothing on 9187, so the API server's proxy to the pod cannot reach it.
function metrics(pod: string): string {
  const response = run([
    ...kubectlArgs(),
    "exec",
    pod,
    "-c",
    "postgres",
    "--",
    "bash",
    "-c",
    'exec 3<>/dev/tcp/127.0.0.1/9187 && printf "GET /metrics HTTP/1.0\\r\\nHost: localhost\\r\\n\\r\\n" >&3 && cat <&3',
  ]);
  const [head = "", ...body] = response.split(/\r?\n\r?\n/);
  if (!/^HTTP\/1\.[01] 200 /.test(head)) throw new Error(`metrics of ${pod}: ${head.split("\n")[0]}`);
  return body.join("\n\n");
}
function metric(text: string, name: string): string | undefined {
  return text
    .split("\n")
    .find((line) => line.startsWith(`${name} `) || line.startsWith(`${name}{`))
    ?.split(" ")
    .at(-1);
}

const report: Record<string, unknown> = { run: RUN, namespace: NAMESPACE, cluster: CLUSTER, restored: RESTORED, storeImage: STORE_IMAGE };
const created = { store: false, objectStore: false, cluster: false, peers: false, backup: false, restored: false };
const timeline: { step: string; at: string }[] = [];
const mark = (step: string) => timeline.push({ step, at: new Date().toISOString() });

// The chart's Postgres objects, rendered as a bundled on-prem install renders them, with the
// test's image; the Cluster is then changed as the header says.
function renderPostgres(): Manifest[] {
  const { repository, tag, digest } = imageParts(POSTGRES_IMAGE, "E2E_POSTGRES_IMAGE");
  const rendered = run([
    HELM,
    "template",
    RELEASE,
    ".",
    "--namespace",
    NAMESPACE,
    "--values",
    "values-onprem.yaml",
    "--values",
    "tests/values/onprem-instance.yaml",
    "--set-string",
    `postgres.bundled.image.repository=${repository}`,
    "--set-string",
    `postgres.bundled.image.tag=${tag}`,
    "--set-string",
    `postgres.bundled.image.digest=${digest}`,
    "--set-string",
    `postgres.bundled.image.pullPolicy=${PULL_POLICY}`,
    "--show-only",
    "templates/postgres.yaml",
  ]);
  const parsed = Bun.YAML.parse(rendered) as unknown;
  return (Array.isArray(parsed) ? parsed : [parsed]).filter((m): m is Manifest => m !== null && typeof m === "object");
}
const SMALL = { requests: { cpu: "50m", memory: "192Mi" }, limits: { memory: "512Mi" } };

let renderedPostgresql: Record<string, unknown> = {};
let catalogRef: Record<string, unknown> = {};

beforeAll(async () => {
  // The platform: CloudNativePG and the Barman Cloud plugin (deploy/cloudnative-pg/README.md and
  // the README next to this file); discovery needs no cluster-wide rights.
  const resources = run([KUBECTL, ...(CONTEXT ? ["--context", CONTEXT] : []), "api-resources", "--output", "name"]).split("\n");
  for (const kind of ["clusters.postgresql.cnpg.io", "imagecatalogs.postgresql.cnpg.io", "backups.postgresql.cnpg.io", "objectstores.barmancloud.cnpg.io"]) {
    if (!resources.includes(kind)) throw new Error(`the cluster has no ${kind}; install the platform first (deploy/chart/tests/e2e/README.md)`);
  }
  // Clusters, volumes and the chart's NetworkPolicy are named after the release; refuse to start
  // when any of them exists (someone's data or access rules): apply would replace it and cleanup
  // would delete it.
  const leftovers = [
    kubectl(
      "get",
      `cluster.postgresql.cnpg.io/${CLUSTER}`,
      `cluster.postgresql.cnpg.io/${RESTORED}`,
      `imagecatalog.postgresql.cnpg.io/${CLUSTER}`,
      `networkpolicy/${PREFIX}-postgres`,
      "--ignore-not-found",
      "--output",
      "name",
    ),
    kubectl("get", "pvc", "--selector", `cnpg.io/cluster in (${CLUSTER},${RESTORED})`, "--output", "name"),
  ]
    .join("\n")
    .split("\n")
    .filter((line) => line.trim() !== "");
  if (leftovers.length > 0) throw new Error(`${NAMESPACE} already has ${leftovers.join(", ")}, which this run would delete; remove them first or use another E2E_RELEASE`);

  // The object store: random keys, made here and kept in the Secret only.
  const hex = (bytes: number) => Buffer.from(crypto.getRandomValues(new Uint8Array(bytes))).toString("hex");
  created.store = true;
  apply({ apiVersion: "v1", kind: "Secret", metadata: { name: STORE }, type: "Opaque", stringData: { ACCESS_KEY_ID: hex(10), ACCESS_SECRET_KEY: hex(20) } });
  const restricted = { allowPrivilegeEscalation: false, capabilities: { drop: ["ALL"] }, runAsNonRoot: true, seccompProfile: { type: "RuntimeDefault" } };
  const fromSecret = (name: string, key: string) => ({ name, valueFrom: { secretKeyRef: { name: STORE, key } } });
  apply({
    apiVersion: "apps/v1",
    kind: "Deployment",
    metadata: { name: STORE, labels: { "trustiot-e2e/store": RUN } },
    spec: {
      replicas: 1,
      selector: { matchLabels: { "trustiot-e2e/store": RUN } },
      template: {
        metadata: { labels: { "trustiot-e2e/store": RUN } },
        spec: {
          automountServiceAccountToken: false,
          securityContext: { runAsUser: 65532, runAsGroup: 65532, fsGroup: 65532, runAsNonRoot: true, seccompProfile: { type: "RuntimeDefault" } },
          containers: [
            {
              name: "rustfs",
              image: STORE_IMAGE,
              imagePullPolicy: "IfNotPresent",
              command: ["/usr/bin/rustfs"],
              ports: [{ containerPort: 9000, name: "s3" }],
              env: [
                { name: "RUSTFS_ADDRESS", value: ":9000" },
                { name: "RUSTFS_VOLUMES", value: "/data" },
                { name: "RUSTFS_REGION", value: "us-east-1" },
                { name: "RUSTFS_CONSOLE_ENABLE", value: "false" },
                { name: "RUSTFS_OBS_LOG_DIRECTORY", value: "/logs" },
                fromSecret("RUSTFS_ACCESS_KEY", "ACCESS_KEY_ID"),
                fromSecret("RUSTFS_SECRET_KEY", "ACCESS_SECRET_KEY"),
              ],
              readinessProbe: { httpGet: { path: "/health", port: 9000 }, periodSeconds: 5 },
              resources: { requests: { cpu: "20m", memory: "64Mi" }, limits: { memory: "384Mi" } },
              securityContext: restricted,
              volumeMounts: [
                { name: "data", mountPath: "/data" },
                { name: "logs", mountPath: "/logs" },
              ],
            },
          ],
          volumes: [
            { name: "data", emptyDir: { sizeLimit: "2Gi" } },
            { name: "logs", emptyDir: { sizeLimit: "256Mi" } },
          ],
        },
      },
    },
  });
  apply({ apiVersion: "v1", kind: "Service", metadata: { name: STORE }, spec: { selector: { "trustiot-e2e/store": RUN }, ports: [{ name: "s3", port: 9000, targetPort: 9000 }] } });
  kubectl("rollout", "status", `deployment/${STORE}`, "--timeout=300s");
  created.objectStore = true;
  apply({
    apiVersion: "barmancloud.cnpg.io/v1",
    kind: "ObjectStore",
    metadata: { name: STORE },
    spec: {
      configuration: {
        destinationPath: `s3://${BUCKET}/`,
        endpointURL: `http://${STORE}.${NAMESPACE}.svc:9000`,
        s3Credentials: { accessKeyId: { name: STORE, key: "ACCESS_KEY_ID" }, secretAccessKey: { name: STORE, key: "ACCESS_SECRET_KEY" } },
      },
      instanceSidecarConfiguration: { resources: { requests: { cpu: "10m", memory: "64Mi" }, limits: { memory: "256Mi" } } },
    },
  });

  // The chart's objects, the Cluster changed for this test (see the header).
  const manifests = renderPostgres();
  const pick = (kind: string) => {
    const found = manifests.find((m) => m.kind === kind);
    if (!found) throw new Error(`the chart rendered no ${kind}`);
    return found;
  };
  const catalog = pick("ImageCatalog");
  const policy = pick("NetworkPolicy");
  const cluster = pick("Cluster");
  const chartSpec = structuredClone(cluster.spec);
  renderedPostgresql = chartSpec.postgresql as Record<string, unknown>;
  catalogRef = chartSpec.imageCatalogRef as Record<string, unknown>;
  const changes = {
    instances: 2,
    resources: SMALL,
    storage: { size: "1Gi" },
    plugins: [{ name: PLUGIN, isWALArchiver: true, parameters: { barmanObjectName: STORE } }],
  };
  cluster.spec = { ...cluster.spec, ...changes };
  report.clusterChanges = { ...changes, unchanged: Object.keys(chartSpec).filter((key) => !(key in changes)).sort() };
  report.bootstrap = (chartSpec.bootstrap as { initdb: Record<string, unknown> }).initdb;
  created.peers = true;
  apply({
    apiVersion: "networking.k8s.io/v1",
    kind: "NetworkPolicy",
    metadata: { name: PEERS },
    spec: {
      podSelector: { matchLabels: { "cnpg.io/cluster": CLUSTER, "cnpg.io/podRole": "instance" } },
      policyTypes: ["Ingress"],
      // Every pod of the Cluster: the instances and the join Job of a new replica.
      ingress: [{ from: [{ podSelector: { matchLabels: { "cnpg.io/cluster": CLUSTER } } }], ports: [{ port: 5432, protocol: "TCP" }] }],
    },
  });
  created.cluster = true;
  for (const object of [catalog, policy, cluster]) apply(object);
  mark("applied");
  const status = await waitFor(`Cluster ${CLUSTER} healthy with 2 ready instances`, () => healthy(CLUSTER, 2), 900_000);
  mark("healthy");
  report.instances = status.instanceNames;
  report.firstPrimary = status.currentPrimary;
}, 1_500_000);

afterAll(() => {
  report.timeline = timeline;
  if (REPORT_DIR) {
    mkdirSync(REPORT_DIR, { recursive: true });
    writeFileSync(join(REPORT_DIR, "postgres-replication-e2e.json"), `${JSON.stringify(report, null, 2)}\n`);
  }
  if (KEEP) return;
  const errors: string[] = [];
  const steps: [boolean, () => unknown][] = [
    [created.restored, () => kubectl("delete", `cluster.postgresql.cnpg.io/${RESTORED}`, "--ignore-not-found", "--wait=true", "--timeout=300s")],
    [created.backup, () => kubectl("delete", `backup.postgresql.cnpg.io/${BACKUP}`, "--ignore-not-found", "--wait=true")],
    [created.cluster, () => kubectl("delete", `cluster.postgresql.cnpg.io/${CLUSTER}`, "--ignore-not-found", "--wait=true", "--timeout=300s")],
    [created.cluster, () => kubectl("delete", `imagecatalog.postgresql.cnpg.io/${CLUSTER}`, `networkpolicy/${PREFIX}-postgres`, "--ignore-not-found", "--wait=true")],
    [created.cluster || created.restored, () => kubectl("delete", "pvc", "--selector", `cnpg.io/cluster in (${CLUSTER},${RESTORED})`, "--ignore-not-found", "--wait=true", "--timeout=300s")],
    [created.peers, () => kubectl("delete", "networkpolicy", PEERS, "--ignore-not-found")],
    [created.objectStore, () => kubectl("delete", `objectstore.barmancloud.cnpg.io/${STORE}`, "--ignore-not-found", "--wait=true")],
    [created.store, () => kubectl("delete", `deployment/${STORE}`, `service/${STORE}`, `secret/${STORE}`, "--ignore-not-found", "--wait=true", "--timeout=300s")],
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
}, 1_200_000);

describe("bundled Postgres without PUBLIC on postgres and template1 (Laysi/trustiot-v2#400)", () => {
  test("the newly bootstrapped Cluster gives no role but the operator's CONNECT on postgres, and nothing on template1", () => {
    const primary = primaryOf(CLUSTER);
    // A role as the migration Job creates a service's login role: as the owner, with CREATEROLE.
    psqlIn(primary, `SET ROLE ${OWNER}; CREATE ROLE ${SERVICE_LIKE_ROLE} NOLOGIN NOINHERIT; RESET ROLE;`);
    // Data for the replica and the restore to show.
    psqlIn(primary, "CREATE TABLE e2e_marker (step text PRIMARY KEY, at timestamptz NOT NULL DEFAULT now()); INSERT INTO e2e_marker (step) VALUES ('bootstrapped');");
    const acls = psqlIn(primary, "SELECT datname, datacl FROM pg_database WHERE datname IN ('postgres', 'template1') ORDER BY 1;", "postgres").split("\n");
    report.acls = acls;
    expect(acls).toEqual([
      "postgres|{postgres=CTc/postgres,streaming_replica=c/postgres,cnpg_metrics_exporter=c/postgres}",
      "template1|{postgres=CTc/postgres}",
    ]);
    const privileges = systemDatabasePrivileges(primary);
    report.privileges = privileges;
    const roles = [...new Set(privileges.map((line) => line.split(" ")[0] ?? ""))];
    for (const role of [OWNER, SERVICE_LIKE_ROLE, ...OPERATOR_ROLES]) expect(roles).toContain(role);
    expect(privileges).toEqual(expectedPrivileges(roles));
    // The TrustIoT database keeps PUBLIC's CONNECT: it is created after postInitSQL, from
    // template1, whose privileges a new database does not copy.
    expect(psqlIn(primary, `SELECT has_database_privilege('${SERVICE_LIKE_ROLE}', '${DATABASE}', 'CONNECT');`)).toBe("t");
    // The operator created its roles over the ones postInitSQL made, with its own attributes.
    report.operatorRoles = psqlIn(
      primary,
      "SELECT format('%s login=%s replication=%s inherit=%s pg_monitor=%s', rolname, rolcanlogin, rolreplication, rolinherit, pg_has_role(rolname, 'pg_monitor', 'member')) FROM pg_roles WHERE rolname IN ('streaming_replica', 'cnpg_metrics_exporter') ORDER BY 1;",
      "postgres",
    ).split("\n");
    expect(report.operatorRoles).toEqual([
      "cnpg_metrics_exporter login=t replication=f inherit=t pg_monitor=t",
      "streaming_replica login=t replication=t inherit=t pg_monitor=f",
    ]);
  });

  test("replication: the replica streams from the primary and has its data", async () => {
    const primary = primaryOf(CLUSTER);
    const replica = instancePods(CLUSTER)
      .map((p) => p.metadata.name)
      .find((name) => name !== primary);
    if (!replica) throw new Error("no replica");
    report.replication = {
      primary,
      replica,
      senders: psqlIn(primary, "SELECT format('%s %s %s', usename, application_name, state) FROM pg_stat_replication;", "postgres").split("\n"),
      receiver: psqlIn(replica, "SELECT status FROM pg_stat_wal_receiver;", "postgres"),
    };
    expect(report.replication).toMatchObject({ senders: [`streaming_replica ${replica} streaming`], receiver: "streaming" });
    psqlIn(primary, "INSERT INTO e2e_marker (step) VALUES ('replicated');");
    await waitFor(`the row on ${replica}`, () => (psqlIn(replica, "SELECT count(*) FROM e2e_marker WHERE step = 'replicated';") === "1" ? true : undefined), 120_000);
  });

  test("a switchover: the former primary rejoins as a replica, and its log shows pg_rewind", async () => {
    const before = primaryOf(CLUSTER);
    const target = instancePods(CLUSTER)
      .map((p) => p.metadata.name)
      .find((name) => name !== before);
    if (!target) throw new Error("no replica to switch over to");
    mark("switchover requested");
    // What `kubectl cnpg promote` and CloudNativePG's own e2e tests do: set the target primary.
    kubectl("patch", `cluster.postgresql.cnpg.io/${CLUSTER}`, "--subresource=status", "--type=merge", "--patch", JSON.stringify({ status: { targetPrimary: target } }));
    await waitFor(`${target} as the current primary`, () => (clusterStatus(CLUSTER).currentPrimary === target ? true : undefined), 600_000);
    await waitFor(`Cluster ${CLUSTER} healthy again with 2 ready instances`, () => healthy(CLUSTER, 2), 600_000);
    mark("switchover done");
    // The former primary follows the new one.
    const receiver = await waitFor(`${before} streaming from ${target}`, () => {
      const status = psqlIn(before, "SELECT status FROM pg_stat_wal_receiver;", "postgres");
      return status === "streaming" ? status : undefined;
    }, 300_000);
    psqlIn(target, "INSERT INTO e2e_marker (step) VALUES ('switched over');");
    await waitFor(`the row on ${before}`, () => (psqlIn(before, "SELECT count(*) FROM e2e_marker WHERE step = 'switched over';") === "1" ? true : undefined), 120_000);
    // pg_rewind runs in the former primary's pod, connecting to the new primary's `postgres`
    // database as streaming_replica (CloudNativePG's instance_startup.go). Its outcome is in the
    // log either way: "Done!" when it rewound, "no rewind required" when the timelines allowed
    // the former primary to follow without it.
    const rewind = messages(logLines(before, /pg_rewind/));
    report.switchover = { from: before, to: target, receiver, rewind };
    expect(rewind.some((line) => line.includes("Starting up pg_rewind"))).toBe(true);
    expect(rewind.some((line) => /Done!|no rewind required/.test(line))).toBe(true);
    expect(rewind.some((line) => /permission denied|CONNECT privilege|pg_rewind: error|while executing pg_rewind/.test(line))).toBe(false);
  });

  test("backup: WAL archiving is healthy and a Backup through the plugin completes", async () => {
    await waitFor(`Cluster ${CLUSTER} ContinuousArchiving`, () => (condition(CLUSTER, "ContinuousArchiving")?.status === "True" ? true : undefined), 600_000);
    const primary = primaryOf(CLUSTER);
    psqlIn(primary, "INSERT INTO e2e_marker (step) VALUES ('before backup');");
    created.backup = true;
    mark("backup requested");
    apply({
      apiVersion: "postgresql.cnpg.io/v1",
      kind: "Backup",
      metadata: { name: BACKUP },
      spec: { cluster: { name: CLUSTER }, method: "plugin", pluginConfiguration: { name: PLUGIN } },
    });
    const backup = await waitFor(`Backup ${BACKUP} completed`, () => {
      const status = json<{ status?: { phase?: string; error?: string; backupId?: string; beginWal?: string; endWal?: string; method?: string } }>("get", `backup.postgresql.cnpg.io/${BACKUP}`).status;
      if (status?.phase === "failed") throw new Error(`Backup ${BACKUP} failed: ${status.error}`);
      return status?.phase === "completed" ? status : undefined;
    }, 600_000);
    mark("backup completed");
    // A WAL switch, so the row after the backup reaches the archive too.
    psqlIn(primary, "INSERT INTO e2e_marker (step) VALUES ('after backup'); SELECT pg_switch_wal();");
    const archiving = condition(CLUSTER, "ContinuousArchiving");
    report.backup = { phase: backup.phase, method: backup.method, backupId: backup.backupId, beginWal: backup.beginWal, endWal: backup.endWal, continuousArchiving: archiving };
    expect(backup.phase).toBe("completed");
    expect(archiving?.status).toBe("True");
  });

  test("restore: a new Cluster bootstraps from the backup through the plugin, with the data and the revocations", async () => {
    created.restored = true;
    mark("restore requested");
    apply({
      apiVersion: "postgresql.cnpg.io/v1",
      kind: "Cluster",
      metadata: { name: RESTORED },
      spec: {
        instances: 1,
        enablePDB: false,
        enableSuperuserAccess: false,
        imageCatalogRef: catalogRef,
        imagePullPolicy: PULL_POLICY,
        postgresql: renderedPostgresql,
        resources: SMALL,
        storage: { size: "1Gi" },
        affinity: { nodeSelector: { "kubernetes.io/arch": "arm64" } },
        inheritedMetadata: { labels: { "istio.io/dataplane-mode": "none" } },
        bootstrap: { recovery: { source: "origin", database: DATABASE, owner: OWNER } },
        // Recovery through the plugin needs only externalClusters[].plugin: the plugin v0.15.1
        // documents exactly this Cluster ("Restoring a Cluster" in its web/docs/usage.md) and
        // adds `spec.plugins` only to archive the restored Cluster's own WAL, which would write
        // into the store this test reads from. The run itself shows the recovery works.
        externalClusters: [{ name: "origin", plugin: { name: PLUGIN, parameters: { barmanObjectName: STORE, serverName: CLUSTER } } }],
      },
    });
    await waitFor(`Cluster ${RESTORED} healthy`, () => healthy(RESTORED, 1), 900_000);
    mark("restored");
    const restoredPrimary = primaryOf(RESTORED);
    const rows = psqlIn(restoredPrimary, "SELECT step FROM e2e_marker ORDER BY at;").split("\n");
    const acls = psqlIn(restoredPrimary, "SELECT datname, datacl FROM pg_database WHERE datname IN ('postgres', 'template1') ORDER BY 1;", "postgres").split("\n");
    const privileges = systemDatabasePrivileges(restoredPrimary);
    report.restore = { rows, acls, privileges };
    for (const step of ["bootstrapped", "replicated", "switched over", "before backup"]) expect(rows).toContain(step);
    expect(acls).toEqual([
      "postgres|{postgres=CTc/postgres,streaming_replica=c/postgres,cnpg_metrics_exporter=c/postgres}",
      "template1|{postgres=CTc/postgres}",
    ]);
    const roles = [...new Set(privileges.map((line) => line.split(" ")[0] ?? ""))];
    expect(roles).toContain(SERVICE_LIKE_ROLE);
    expect(privileges).toEqual(expectedPrivileges(roles));
    // The restored instance's manager collects metrics without errors as well.
    const text = metrics(restoredPrimary);
    report.restoreMetrics = { up: metric(text, "cnpg_collector_up"), lastCollectionError: metric(text, "cnpg_collector_last_collection_error") };
    expect(report.restoreMetrics).toEqual({ up: "1", lastCollectionError: "0" });
    // Checked; the restored Cluster goes now to leave the node its memory (cleanup repeats it).
    kubectl("delete", `cluster.postgresql.cnpg.io/${RESTORED}`, "--wait=true", "--timeout=300s");
    mark("restored Cluster deleted");
  });

  test("the instance manager stayed healthy throughout: Ready instances, metrics without errors, no refused database privilege", () => {
    const status = healthy(CLUSTER, 2);
    expect(status?.phase).toBe("Cluster in healthy state");
    const health: Record<string, unknown> = {};
    for (const pod of instancePods(CLUSTER).map((p) => p.metadata.name)) {
      const text = metrics(pod);
      const refused = messages(logLines(pod, /permission denied for database|CONNECT privilege|permission denied to create temporary/));
      // The exporter's default queries run in `postgres` and, per database, in every database
      // that takes connections except templates: both work.
      const databases = [...text.matchAll(/^cnpg_pg_database_size_bytes\{datname="([^"]+)"/gm)].map((m) => m[1]).sort();
      health[pod] = {
        up: metric(text, "cnpg_collector_up"),
        lastCollectionError: metric(text, "cnpg_collector_last_collection_error"),
        collectionErrors: text.split("\n").filter((line) => line.startsWith("cnpg_collector_collection_errors_total{")),
        databases,
        refused,
        restarts: json<{ status: { containerStatuses?: { name: string; restartCount: number }[] } }>("get", "pod", pod).status.containerStatuses?.map((c) => `${c.name}=${c.restartCount}`),
      };
      expect(metric(text, "cnpg_collector_up")).toBe("1");
      expect(metric(text, "cnpg_collector_last_collection_error")).toBe("0");
      expect(refused).toEqual([]);
    }
    report.instanceManager = health;
    report.finalStatus = { phase: status?.phase, currentPrimary: status?.currentPrimary, conditions: clusterStatus(CLUSTER).conditions };
    // Which version of the chart's statements this run deployed.
    report.systemDatabasesSqlSha256 = new Bun.CryptoHasher("sha256").update(readFileSync(join(CHART_DIR, "files/postgres/system-databases.sql"), "utf8")).digest("hex");
  });
});
