// The dev environment's commands: up, status, seed, env, down and purge. Each one takes the
// process environment and returns when it is done; `scripts/dev.ts` is the entry point.
import { createServer } from "node:net";
import { copyFileSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { registerCleanup } from "./cleanup.ts";
import { clickhouse } from "./clickhouse.ts";
import {
  clickhouseChecks,
  mosquittoChecks,
  postgresChecks,
  runChecks,
  serviceChecks,
  waitForChecks,
  type CheckResult,
} from "./health.ts";
import { publishRetained, readRetained } from "./mqtt.ts";
import {
  containersWithLabel,
  imagesWithLabel,
  networksWithLabel,
  podman,
  podmanOk,
  requirePodman,
  volumesWithLabel,
} from "./podman.ts";
import {
  dotenv,
  generateCredentials,
  mosquittoPasswordEntry,
  sqlLiteral,
  sqlIdentifier,
} from "./secrets.ts";
import { discoverSeeds } from "./seed.ts";
import {
  containerfile,
  imageLabelArgs,
  platformEnv,
  resolveServicePorts,
  SERVICES,
  type ServiceDef,
} from "./services.ts";
import {
  acquireLock,
  loadState,
  removeState,
  resolveInstance,
  saveState,
  stateDir,
  stateRoot,
  withPrivateFile,
  type State,
} from "./state.ts";
import {
  assertDistinctPorts,
  BIND_ADDRESS,
  CONTAINER_PORTS,
  IMAGES,
  MOSQUITTO_CONFIG,
  mosquittoAcl,
  namesFor,
  POSTGRES_HBA,
  POSTGRES_HBA_PATH,
  resolvePorts,
  runArgs,
  SERVICE_PORT,
  type Names,
} from "./stack.ts";

export interface Context {
  instance: string;
  names: Names;
  root: string;
  dir: string;
  repoRoot: string;
  env: Record<string, string | undefined>;
}

export function context(env: Record<string, string | undefined> = process.env): Context {
  const instance = resolveInstance(env);
  return {
    instance,
    names: namesFor(instance),
    root: stateRoot(env),
    dir: stateDir(env, instance),
    repoRoot: join(import.meta.dir, "..", ".."),
    env,
  };
}

const log = (message: string) => console.log(message);

// --- containers, network and volumes of one instance ---------------------------------------

async function removeContainers(ctx: Context): Promise<string[]> {
  const names = await containersWithLabel(ctx.names.label);
  if (names.length > 0) await podmanOk(["rm", "--force", "--time", "10", ...names]);
  return names;
}

async function ensureNetwork(ctx: Context): Promise<void> {
  const exists = await podman(["network", "exists", ctx.names.network]);
  if (exists.code !== 0) {
    await podmanOk(["network", "create", "--label", ctx.names.label, ctx.names.network]);
  }
}

async function ensureVolume(ctx: Context, name: string): Promise<string> {
  const volume = ctx.names.volume(name);
  const exists = await podman(["volume", "exists", volume]);
  if (exists.code !== 0) {
    await podmanOk(["volume", "create", "--label", ctx.names.label, volume]);
  }
  return volume;
}

async function freshVolume(ctx: Context, name: string): Promise<string> {
  const volume = ctx.names.volume(name);
  await podman(["volume", "rm", "--force", volume]);
  await podmanOk(["volume", "create", "--label", ctx.names.label, volume]);
  return volume;
}

/** Writes a file into a volume through a throwaway container; the content travels on stdin. */
async function writeVolumeFile(
  volume: string,
  file: string,
  content: string,
  mode: string,
  owner: string,
): Promise<void> {
  await podmanOk(
    [
      "run",
      "--rm",
      "--interactive",
      "--user",
      "root",
      "--entrypoint",
      "sh",
      "--volume",
      `${volume}:/target`,
      IMAGES.mosquitto,
      "-c",
      'cat > "$0" && chmod "$1" "$0" && chown "$2" "$0"',
      `/target/${file}`,
      mode,
      owner,
    ],
    { stdin: content },
  );
}

function portFree(port: number): Promise<boolean> {
  return new Promise((resolve) => {
    const server = createServer();
    server.once("error", () => resolve(false));
    server.listen({ host: BIND_ADDRESS, port }, () => server.close(() => resolve(true)));
  });
}

async function assertPortsFree(ports: Record<string, number>): Promise<void> {
  for (const [name, port] of Object.entries(ports)) {
    if (!(await portFree(port))) {
      throw new Error(
        `port ${port} (${name}) on ${BIND_ADDRESS} is already in use; stop what uses it or set ` +
          "another port with the TRUSTIOT_DEV_*_PORT variables (see scripts/README.md)",
      );
    }
  }
}

async function waitFor(what: string, attempt: () => Promise<boolean>, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!(await attempt())) {
    if (Date.now() >= deadline) throw new Error(`timed out waiting for ${what}`);
    await Bun.sleep(1000);
  }
}

// --- up ------------------------------------------------------------------------------------

export async function up(ctx: Context = context()): Promise<void> {
  await requirePodman();
  const release = acquireLock(ctx.root, ctx.instance);
  try {
    // Validate everything that can be validated before the environment is touched.
    const infraPorts = resolvePorts(ctx.env);
    const servicePorts = resolveServicePorts(SERVICES, ctx.env);
    const requested: Record<string, number> = {
      postgres: infraPorts.postgres,
      clickhouse: infraPorts.clickhouse,
      mqtt: infraPorts.mqtt,
      ...Object.fromEntries(Object.entries(servicePorts).map(([name, port]) => [`service ${name}`, port])),
    };
    assertDistinctPorts(requested);

    const removed = await removeContainers(ctx);
    if (removed.length > 0) log(`replaced ${removed.length} container(s) of instance "${ctx.instance}"`);
    await assertPortsFree(requested);

    // New credentials on every start. They live only in the state directory (mode 0600) and in
    // the containers; down deletes the file, and Postgres is told the new password below.
    const credentials = generateCredentials();
    const state: State = {
      version: 1,
      instance: ctx.instance,
      startedAt: new Date().toISOString(),
      ports: { ...infraPorts, services: servicePorts },
      credentials,
    };
    saveState(ctx.dir, state);

    await ensureNetwork(ctx);
    const pgData = await ensureVolume(ctx, "postgres-data");
    const chData = await ensureVolume(ctx, "clickhouse-data");
    const mqData = await ensureVolume(ctx, "mosquitto-data");
    const mqConfig = await freshVolume(ctx, "mosquitto-config");

    const pgConfig = await freshVolume(ctx, "postgres-config");

    log("starting postgres, clickhouse and mosquitto");
    await writeVolumeFile(pgConfig, "pg_hba.conf", POSTGRES_HBA, "0644", "999:999");
    await writeVolumeFile(mqConfig, "mosquitto.conf", MOSQUITTO_CONFIG, "0644", "1883:1883");
    await writeVolumeFile(mqConfig, "acl", mosquittoAcl(credentials.mqtt.user), "0644", "1883:1883");
    await writeVolumeFile(
      mqConfig,
      "passwd",
      mosquittoPasswordEntry(credentials.mqtt.user, credentials.mqtt.password),
      "0600",
      "1883:1883",
    );

    await withPrivateFile(
      ctx.dir,
      "postgres.env",
      dotenv({
        POSTGRES_USER: credentials.postgres.user,
        POSTGRES_PASSWORD: credentials.postgres.password,
        POSTGRES_DB: credentials.postgres.database,
      }),
      (envFile) =>
        podmanOk(
          runArgs(ctx.instance, {
            component: "postgres",
            image: IMAGES.postgres,
            memory: "768m",
            publish: [[infraPorts.postgres, CONTAINER_PORTS.postgres]],
            envFile,
            volumes: [`${pgData}:/var/lib/postgresql`, `${pgConfig}:/etc/trustiot:ro`],
            command: ["postgres", "-c", `hba_file=${POSTGRES_HBA_PATH}`],
          }),
        ),
    );
    await withPrivateFile(
      ctx.dir,
      "clickhouse.env",
      dotenv({
        CLICKHOUSE_USER: credentials.clickhouse.user,
        CLICKHOUSE_PASSWORD: credentials.clickhouse.password,
        CLICKHOUSE_DB: credentials.clickhouse.database,
        CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT: "1",
      }),
      (envFile) =>
        podmanOk(
          runArgs(ctx.instance, {
            component: "clickhouse",
            image: IMAGES.clickhouse,
            memory: "1g",
            publish: [[infraPorts.clickhouse, CONTAINER_PORTS.clickhouse]],
            envFile,
            volumes: [`${chData}:/var/lib/clickhouse`],
          }),
        ),
    );
    await podmanOk(
      runArgs(ctx.instance, {
        component: "mosquitto",
        image: IMAGES.mosquitto,
        memory: "128m",
        publish: [[infraPorts.mqtt, CONTAINER_PORTS.mqtt]],
        volumes: [`${mqConfig}:/mosquitto/config`, `${mqData}:/mosquitto/data`],
        command: ["mosquitto", "-c", "/mosquitto/config/mosquitto.conf"],
      }),
    );

    await preparePostgres(ctx, state);

    const infraResults = await waitForChecks(
      () => [
        ...postgresChecks(state),
        ...clickhouseChecks(state),
        ...mosquittoChecks(state),
      ],
      120_000,
    );
    assertAllOk(ctx, infraResults);

    for (const service of SERVICES) await startService(ctx, state, service);
    if (SERVICES.length > 0) {
      const serviceResults = await waitForChecks(() => serviceChecks(state, SERVICES), 120_000);
      assertAllOk(ctx, serviceResults);
    }

    log("");
    log(`environment "${ctx.instance}" is up; every check passed`);
    printEndpoints(state);
  } finally {
    release();
  }
}

/**
 * The data volume outlives `down`, so the role's password from an earlier run is still stored
 * in it. Local connections inside the container use the trust method, so the new password is
 * set over the socket. On a fresh volume this sets the value the container was started with.
 * Apache AGE is enabled in the database explicitly as well, so the health check never depends
 * on an init script of the image (which only runs on an empty data volume).
 */
async function preparePostgres(ctx: Context, state: State): Promise<void> {
  const container = ctx.names.container("postgres");
  const { user, database, password } = state.credentials.postgres;
  await waitFor(
    "postgres to accept connections",
    async () =>
      (
        await podman([
          "exec",
          container,
          "pg_isready",
          "--host=127.0.0.1",
          "--port=5432",
          `--username=${user}`,
          `--dbname=${database}`,
        ])
      ).code === 0,
    120_000,
  );
  await podmanOk(
    [
      "exec",
      "--interactive",
      container,
      "psql",
      `--username=${user}`,
      `--dbname=${database}`,
      "--no-psqlrc",
      "--quiet",
      "--set=ON_ERROR_STOP=1",
      "--file=-",
    ],
    {
      stdin:
        `ALTER ROLE ${sqlIdentifier(user)} WITH PASSWORD ${sqlLiteral(password)};\n` +
        "CREATE EXTENSION IF NOT EXISTS age;\n",
    },
  );
}

function assertAllOk(ctx: Context, results: CheckResult[]): void {
  printResults(results);
  const failed = results.filter((item) => !item.ok);
  if (failed.length > 0) {
    throw new Error(
      `${failed.length} check(s) failed. The containers are left running for inspection: ` +
        `podman logs ${ctx.names.container("<component>")}; run the down command to remove them`,
    );
  }
}

function printResults(results: CheckResult[]): void {
  for (const item of results) log(`${item.ok ? "ok  " : "FAIL"} ${item.name}: ${item.detail}`);
}

function printEndpoints(state: State): void {
  const { ports } = state;
  log("");
  log(`  postgres         ${BIND_ADDRESS}:${ports.postgres}  (database ${state.credentials.postgres.database}, Apache AGE)`);
  log(`  clickhouse       http://${BIND_ADDRESS}:${ports.clickhouse}`);
  log(`  mqtt             ${BIND_ADDRESS}:${ports.mqtt}`);
  for (const [name, port] of Object.entries(ports.services)) {
    log(`  ${name.padEnd(16)} http://${BIND_ADDRESS}:${port}`);
  }
  log("");
  log("Credentials are generated for this run and stored only in the state directory.");
  log("Print them for your shell with: bun scripts/dev.ts env");
}

// --- services ------------------------------------------------------------------------------

/** Builds the service's binary with cargo and returns its path. */
async function cargoBuild(ctx: Context, service: ServiceDef): Promise<string> {
  log(`building ${service.cargo.package} (${service.cargo.bin}) with cargo`);
  const child = Bun.spawn(
    [
      "cargo",
      "build",
      "--package",
      service.cargo.package,
      "--bin",
      service.cargo.bin,
      "--message-format",
      "json-render-diagnostics",
    ],
    { cwd: ctx.repoRoot, stdout: "pipe", stderr: "inherit" },
  );
  const [output, code] = await Promise.all([new Response(child.stdout).text(), child.exited]);
  if (code !== 0) throw new Error(`cargo build of ${service.cargo.package} failed (exit ${code})`);
  for (const line of output.split("\n").reverse()) {
    if (!line.startsWith("{")) continue;
    const message = JSON.parse(line) as {
      reason?: string;
      executable?: string | null;
      target?: { name?: string };
    };
    if (message.reason === "compiler-artifact" && message.target?.name === service.cargo.bin && message.executable) {
      return message.executable;
    }
  }
  throw new Error(`cargo did not report an executable for ${service.cargo.bin}`);
}

async function startService(ctx: Context, state: State, service: ServiceDef): Promise<void> {
  const binary = await cargoBuild(ctx, service);
  const buildDir = mkdtempSync(join(ctx.dir, `build-${service.name}-`));
  const image = ctx.names.image(service.name);
  const forget = registerCleanup(() => rmSync(buildDir, { recursive: true, force: true }));
  try {
    copyFileSync(binary, join(buildDir, service.cargo.bin));
    writeFileSync(join(buildDir, "Containerfile"), containerfile(IMAGES.serviceBase, service));
    await podmanOk([
      "build",
      "--quiet",
      ...imageLabelArgs(ctx.names),
      "--tag",
      image,
      "--file",
      join(buildDir, "Containerfile"),
      buildDir,
    ]);
  } finally {
    rmSync(buildDir, { recursive: true, force: true });
    forget();
  }
  log(`starting ${service.name}`);
  await podmanOk(
    runArgs(ctx.instance, {
      component: service.name,
      image,
      memory: "256m",
      publish: [[state.ports.services[service.name]!, SERVICE_PORT]],
      env: { ...platformEnv(state.credentials.postgres.database), ...service.env },
    }),
  );
}

// --- status, env ---------------------------------------------------------------------------

function requireState(ctx: Context): State {
  const state = loadState(ctx.dir);
  if (!state) {
    throw new Error(`instance "${ctx.instance}" is not running; start it with: bun scripts/dev.ts up`);
  }
  return state;
}

export async function status(ctx: Context = context()): Promise<boolean> {
  await requirePodman();
  const state = requireState(ctx);
  const expected = ["postgres", "clickhouse", "mosquitto", ...SERVICES.map((service) => service.name)];
  const running = (
    await podmanOk([
      "ps",
      "--filter",
      `label=${ctx.names.label}`,
      "--filter",
      "status=running",
      "--format",
      "{{.Names}}",
    ])
  )
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);
  const containerResults: CheckResult[] = expected.map((component) => {
    const ok = running.includes(ctx.names.container(component));
    return { name: `container ${component}`, ok, detail: ok ? "running" : "not running" };
  });
  const results = [
    ...containerResults,
    ...(await runChecks([
      ...postgresChecks(state),
      ...clickhouseChecks(state),
      ...mosquittoChecks(state),
      ...serviceChecks(state, SERVICES),
    ])),
  ];
  printResults(results);
  const ok = results.every((item) => item.ok);
  log(ok ? "all checks passed" : "some checks failed");
  return ok;
}

/** KEY=value lines (including the passwords) for a shell or a dotenv file. */
export function envLines(state: State): string {
  const { postgres, clickhouse: ch, mqtt } = state.credentials;
  const { ports } = state;
  return dotenv({
    PGHOST: BIND_ADDRESS,
    PGPORT: ports.postgres,
    PGUSER: postgres.user,
    PGPASSWORD: postgres.password,
    PGDATABASE: postgres.database,
    TRUSTIOT_DEV_POSTGRES_URL: `postgres://${postgres.user}:${postgres.password}@${BIND_ADDRESS}:${ports.postgres}/${postgres.database}`,
    CLICKHOUSE_URL: `http://${BIND_ADDRESS}:${ports.clickhouse}`,
    CLICKHOUSE_USER: ch.user,
    CLICKHOUSE_PASSWORD: ch.password,
    CLICKHOUSE_DATABASE: ch.database,
    MQTT_HOST: BIND_ADDRESS,
    MQTT_PORT: ports.mqtt,
    MQTT_USER: mqtt.user,
    MQTT_PASSWORD: mqtt.password,
  });
}

export function printEnv(ctx: Context = context()): void {
  process.stdout.write(envLines(requireState(ctx)));
}

// --- seed ----------------------------------------------------------------------------------

export const SEED_TOPIC = "trustiot-dev/seed/hello";
export const SEED_MESSAGE = JSON.stringify({ seeded: true, source: "scripts/dev.ts seed" });

export async function seed(ctx: Context = context()): Promise<void> {
  await requirePodman();
  const state = requireState(ctx);
  const container = ctx.names.container("postgres");
  const { user, database } = state.credentials.postgres;

  const files = discoverSeeds(ctx.repoRoot);
  for (const file of files) {
    const label = `${file.origin}/${file.store}/${file.path.split("/").at(-1)}`;
    const sql = readFileSync(file.path, "utf8");
    if (file.store === "postgres") {
      // Over the container's unix socket: no password on any command line.
      await podmanOk(
        [
          "exec",
          "--interactive",
          container,
          "psql",
          `--username=${user}`,
          `--dbname=${database}`,
          "--no-psqlrc",
          "--quiet",
          "--set=ON_ERROR_STOP=1",
          "--file=-",
        ],
        { stdin: sql },
      );
    } else {
      await clickhouse(
        {
          port: state.ports.clickhouse,
          user: state.credentials.clickhouse.user,
          password: state.credentials.clickhouse.password,
        },
        sql,
      );
    }
    log(`applied ${label}`);
  }

  const endpoint = { host: BIND_ADDRESS, port: state.ports.mqtt };
  const login = { user: state.credentials.mqtt.user, password: state.credentials.mqtt.password };
  await publishRetained(endpoint, login, SEED_TOPIC, SEED_MESSAGE);
  log(`published retained message on ${SEED_TOPIC}`);

  // Read everything back, so a seed that silently did nothing is an error.
  const problems: string[] = [];
  const machines = await podmanOk(
    [
      "exec",
      container,
      "psql",
      `--username=${user}`,
      `--dbname=${database}`,
      "--no-psqlrc",
      "--tuples-only",
      "--no-align",
      "--command",
      "SELECT count(*) FROM dev_seed.machine",
    ],
  );
  if (machines.trim() !== "2") problems.push(`dev_seed.machine has ${machines.trim()} rows, expected 2`);
  const graph = await podmanOk(
    [
      "exec",
      "--interactive",
      container,
      "psql",
      `--username=${user}`,
      `--dbname=${database}`,
      "--no-psqlrc",
      "--quiet",
      "--tuples-only",
      "--no-align",
      "--file=-",
    ],
    {
      stdin:
        `LOAD 'age';\nSET search_path = ag_catalog, "$user", public;\n` +
        `SELECT * FROM cypher('dev_seed_graph', $$ MATCH (m:Machine) RETURN count(m) $$) AS (machines agtype);\n`,
    },
  );
  if (graph.trim() !== "2") problems.push(`dev_seed_graph has ${graph.trim()} machines, expected 2`);
  const rows = (
    await clickhouse(
      {
        port: state.ports.clickhouse,
        user: state.credentials.clickhouse.user,
        password: state.credentials.clickhouse.password,
      },
      "SELECT count() FROM dev_seed.telemetry",
    )
  ).trim();
  if (rows !== "240") problems.push(`dev_seed.telemetry has ${rows} rows, expected 240`);
  const retained = await readRetained(endpoint, login, SEED_TOPIC);
  if (retained !== SEED_MESSAGE) problems.push("the retained MQTT message was not stored");
  if (problems.length > 0) throw new Error(`seed verification failed: ${problems.join("; ")}`);
  log(`seed verified: 2 machines (table and graph), ${rows} telemetry rows, 1 retained message`);
}

// --- down, purge ---------------------------------------------------------------------------

/** Stops and removes the containers and the network; data volumes and images stay. */
export async function down(ctx: Context = context()): Promise<void> {
  await requirePodman();
  const release = acquireLock(ctx.root, ctx.instance);
  try {
    const removed = await removeContainers(ctx);
    const networks = await networksWithLabel(ctx.names.label);
    if (networks.length > 0) await podmanOk(["network", "rm", "--force", ...networks]);
    // The credentials were generated for the containers that are gone.
    removeState(ctx.dir);
    log(`removed ${removed.length} container(s); data volumes kept (use purge to delete them)`);
  } finally {
    release();
  }
}

/**
 * Removes everything the instance created: containers, network, data volumes, built service
 * images and the state directory. With `images`, also the pinned images pulled from registries.
 */
export async function purge(ctx: Context = context(), options: { images?: boolean } = {}): Promise<void> {
  await requirePodman();
  const release = acquireLock(ctx.root, ctx.instance);
  try {
    const containers = await removeContainers(ctx);
    const networks = await networksWithLabel(ctx.names.label);
    if (networks.length > 0) await podmanOk(["network", "rm", "--force", ...networks]);
    const volumes = await volumesWithLabel(ctx.names.label);
    if (volumes.length > 0) await podmanOk(["volume", "rm", "--force", ...volumes]);
    const images = await imagesWithLabel(ctx.names.label);
    if (images.length > 0) await podmanOk(["rmi", "--force", ...images]);
    removeState(ctx.dir);
    log(
      `purged instance "${ctx.instance}": ${containers.length} container(s), ${networks.length} network(s), ` +
        `${volumes.length} volume(s), ${images.length} image(s), credentials and state`,
    );
    if (options.images) {
      for (const image of Object.values(IMAGES)) {
        const result = await podman(["rmi", image]);
        log(result.code === 0 ? `removed ${image}` : `kept ${image} (not present or in use)`);
      }
    }
  } finally {
    release();
  }
}
