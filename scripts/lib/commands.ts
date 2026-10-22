// The dev environment's commands: up, status, seed, env, down and purge. Each one takes the
// process environment and returns when it is done; `scripts/dev.ts` is the entry point.
import { createServer } from "node:net";
import { copyFileSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { containerBuildArgs } from "./build-context.ts";
import { registerCleanup } from "./cleanup.ts";
import { clickhouse } from "./clickhouse.ts";
import {
  createSessionSql,
  DEV_TENANT_ID,
  DEV_USER_ID,
  ensureSigningKey,
  IDENTITY_DIR,
  identityEnv,
  liveSessionSql,
  newCookie,
  cookieHeader,
  readCookie,
  signingKeyCheck,
  writeCookie,
  type IdentityEnv,
} from "./identity.ts";
import {
  clickhouseChecks,
  mosquittoChecks,
  postgresChecks,
  runChecks,
  serviceChecks,
  waitForChecks,
  type CheckResult,
} from "./health.ts";
import { MIGRATION_ROLE, runMigrationJob } from "./migrate.ts";
import { SYSTEM_DATABASES_SQL, systemDatabaseRevokes } from "./system-databases.ts";
import { productLogins } from "./product-logins.ts";
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
  migrationRoleSql,
  mosquittoPasswordEntry,
  randomSecret,
  serviceRoleSql,
  sqlLiteral,
  sqlIdentifier,
} from "./secrets.ts";
import { discoverSeeds } from "./seed.ts";
import {
  containerfile,
  platformEnv,
  resolveServicePorts,
  serviceBuildOptions,
  SERVICES,
  type ServiceDef,
} from "./services.ts";
import { generateDevCertificates } from "./tls.ts";
import {
  acquireLock,
  caCertificatePath,
  ensurePrivateDir,
  loadState,
  removeState,
  removeStateExcept,
  resolveInstance,
  saveState,
  stateDir,
  stateRoot,
  withPrivateFile,
  writePrivateFile,
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
  nativePlatform,
  platformMismatchMessage,
  POSTGRES_HBA,
  postgresCommand,
  SERVICE_CA_DIR,
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

/**
 * Removes the instance's containers together with their anonymous volumes: the images declare
 * `VOLUME`s the instance does not mount (Mosquitto's log directory, for one), and every one of
 * them would stay behind and use a podman lock (Laysi/trustiot-v2#334). `--volumes` never
 * touches the named volumes the instance created (`down` keeps its data volumes).
 */
async function removeContainers(ctx: Context): Promise<string[]> {
  const names = await containersWithLabel(ctx.names.label);
  if (names.length > 0) await podmanOk(["rm", "--force", "--volumes", "--time", "10", ...names]);
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

/**
 * Removes a volume if there is one. An environment started before the Postgres TLS change has
 * no `postgres-ca` volume, and `down` must still work on it.
 */
async function removeVolumeIfPresent(volume: string): Promise<void> {
  const exists = await podman(["volume", "exists", volume]);
  if (exists.code === 0) await podmanOk(["volume", "rm", "--force", volume]);
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
      "--platform",
      nativePlatform(),
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
    const credentials = generateCredentials(
      Object.fromEntries(
        SERVICES.flatMap((service) => (service.database ? [[service.name, service.database.role]] : [])),
      ),
    );
    const state: State = {
      version: 2,
      instance: ctx.instance,
      startedAt: new Date().toISOString(),
      ports: { ...infraPorts, services: servicePorts },
      credentials,
    };
    saveState(ctx.dir, state);
    // The migration role's password is new for every run and known only to this run and to the
    // Job it starts; it is not stored.
    const migrationPassword = randomSecret();
    // The identity signing key is kept across runs (down keeps it); a bad one stops here, before
    // anything is started.
    const signingKey = ensureSigningKey(ctx.dir);
    log(`identity signing key: ${signingKey.created ? "generated" : "kept"}; signing keys and public keys files written (mode 0600, state directory)`);

    // A new CA and server certificate for this run. Only the CA certificate (public) is kept in
    // the state directory; the server key goes straight into the Postgres config volume below,
    // and the CA key was never written anywhere.
    const certificates = generateDevCertificates(ctx.instance);
    ensurePrivateDir(join(ctx.dir, "tls"));
    writePrivateFile(caCertificatePath(ctx.dir), certificates.caCertificate);

    await ensureNetwork(ctx);
    const pgData = await ensureVolume(ctx, "postgres-data");
    const chData = await ensureVolume(ctx, "clickhouse-data");
    const mqData = await ensureVolume(ctx, "mosquitto-data");
    const mqConfig = await freshVolume(ctx, "mosquitto-config");

    const pgConfig = await freshVolume(ctx, "postgres-config");
    // The services' copy of the CA: the volume holds nothing else, so a service never sees the
    // server key.
    const pgCa = await freshVolume(ctx, "postgres-ca");

    log("starting postgres, clickhouse and mosquitto");
    await writeVolumeFile(pgConfig, "pg_hba.conf", POSTGRES_HBA, "0644", "999:999");
    await writeVolumeFile(pgConfig, "server.crt", certificates.serverCertificate, "0644", "999:999");
    // Postgres refuses a key file that others can read, and wants the postgres user as owner.
    await writeVolumeFile(pgConfig, "server.key", certificates.serverKey, "0600", "999:999");
    await writeVolumeFile(pgConfig, "ca.crt", certificates.caCertificate, "0644", "999:999");
    await writeVolumeFile(pgCa, "ca.crt", certificates.caCertificate, "0644", "0:0");
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
            command: postgresCommand(),
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

    await preparePostgres(ctx, state, migrationPassword);

    const infraResults = await waitForChecks(
      () => [
        ...postgresChecks(state, certificates.caCertificate),
        ...clickhouseChecks(state),
        ...mosquittoChecks(state),
      ],
      120_000,
    );
    assertAllOk(ctx, infraResults);

    await migrateDatabase(ctx, state, migrationPassword);

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
async function preparePostgres(ctx: Context, state: State, migrationPassword: string): Promise<void> {
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
        "CREATE EXTENSION IF NOT EXISTS age;\n" +
        // The role the migration Job connects as, and the owner of the database (migrateDatabase).
        migrationRoleSql(MIGRATION_ROLE, migrationPassword, database) +
        // The per-database setup that service-kit's session check requires (no TEMPORARY for
        // PUBLIC), then one plain login role per service that has a database.
        readFileSync(join(ctx.repoRoot, DATABASE_SETUP), "utf8") +
        // PUBLIC keeps no access to `postgres` and `template1`, as in the bundled Cluster. The
        // statements are idempotent, so every `up` runs them, an older environment included.
        systemDatabaseRevokes(readFileSync(join(ctx.repoRoot, SYSTEM_DATABASES_SQL), "utf8")) +
        SERVICES.flatMap((service) => {
          const login = state.credentials.services[service.name];
          return login ? [serviceRoleSql(login.user, login.password)] : [];
        }).join(""),
    },
  );
}

/** service-kit's per-database template, which its own tests run unchanged. */
const DATABASE_SETUP = "crates/service-kit/sql/database.sql";

/**
 * Applies the product's migrations with the migration Job (tools/migrate), as in a cluster: it
 * creates each service's roles and schema, applies the service's migrations, and generates the
 * password of each service login role into a Secret, which here is the stand-in API of
 * lib/secret-api.ts. The password of every login role the chart binds (lib/product-logins.ts) is
 * read back and kept in the state file with the other service logins, so `env` can print it.
 */
async function migrateDatabase(ctx: Context, state: State, migrationPassword: string): Promise<void> {
  const binary = await cargoBuild(ctx, { package: "migrate", bin: "migrate" });
  const logins = productLogins();
  log("running the migration Job (tools/migrate)");
  const result = await runMigrationJob({
    binary,
    postgres: {
      port: state.ports.postgres,
      database: state.credentials.postgres.database,
      caFile: readCaPath(ctx),
    },
    login: { user: MIGRATION_ROLE, password: migrationPassword },
    stateDir: ctx.dir,
    logins,
  });
  for (const login of logins) {
    state.credentials.services[login.stateKey] = { user: login.role, password: result.passwords[login.role]! };
  }
  saveState(ctx.dir, state);
  log(
    `migration Job finished${result.changes === undefined ? "" : `: ${result.changes} change(s)`}` +
      `${result.warnings === undefined ? "" : `, ${result.warnings} warning(s)`}; ` +
      `${logins.map((login) => login.role).join(", ")} can log in`,
  );
  // The Job logs a warning for what it finds but does not stop for; this output hides its log, so
  // a developer would never see them otherwise. They name databases and roles, no secret.
  if ((result.warnings ?? 0) > 0 || result.warningLines.length > 0) {
    log(`WARNING: the migration Job reported ${result.warnings ?? result.warningLines.length} warning(s):`);
    for (const line of result.warningLines) log(`  ${line}`);
  }
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
  log("  identity         not started: run it on the host with the settings of the env command");
  log("");
  log("Credentials are generated for this run and stored only in the state directory.");
  log("Print them for your shell with: bun scripts/dev.ts env");
}

// --- services ------------------------------------------------------------------------------

/** Builds the service's binary with cargo and returns its path. */
async function cargoBuild(ctx: Context, target: { package: string; bin: string }): Promise<string> {
  const service = { cargo: target };
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
  const binary = await cargoBuild(ctx, service.cargo);
  const buildDir = mkdtempSync(join(ctx.dir, `build-${service.name}-`));
  const image = ctx.names.image(service.name);
  const platform = nativePlatform();
  const forget = registerCleanup(() => rmSync(buildDir, { recursive: true, force: true }));
  try {
    copyFileSync(binary, join(buildDir, service.cargo.bin));
    writeFileSync(join(buildDir, "Containerfile"), containerfile(IMAGES.serviceBase, service));
    await podmanOk(
      containerBuildArgs(buildDir, serviceBuildOptions(ctx.names, image, join(buildDir, "Containerfile"), platform)),
    );
    // --platform already makes podman take the right base; this stays as a guard so that a
    // podman that does not honour it fails here with the cause, not later as a container
    // that cannot start.
    const built = (await podmanOk(["image", "inspect", "--format", "{{.Os}}/{{.Architecture}}", image])).trim();
    if (built !== platform) throw new Error(platformMismatchMessage(image, built, platform));
  } finally {
    rmSync(buildDir, { recursive: true, force: true });
    forget();
  }
  log(`starting ${service.name}`);
  const login = state.credentials.services[service.name];
  const spec = {
    component: service.name,
    image,
    memory: "256m",
    publish: [[state.ports.services[service.name]!, SERVICE_PORT]] as [number, number][],
    env: { ...platformEnv(state.credentials.postgres.database), ...service.env },
    // The CA that signed the Postgres certificate, where the chart mounts it.
    volumes: [`${ctx.names.volume("postgres-ca")}:${SERVICE_CA_DIR}:ro`],
  };
  if (!login) {
    await podmanOk(runArgs(ctx.instance, spec));
    return;
  }
  // The role's password goes in an env file, never on the command line.
  await withPrivateFile(
    ctx.dir,
    `${service.name}.env`,
    dotenv({ TRUSTIOT__DATABASE__USERNAME: login.user, TRUSTIOT__DATABASE__PASSWORD: login.password }),
    (envFile) => podmanOk(runArgs(ctx.instance, { ...spec, envFile })),
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
  const key = signingKeyCheck(ctx.dir);
  const results = [
    ...containerResults,
    { name: "identity: signing key", ok: key.ok, detail: key.detail },
    ...(await runChecks([
      ...postgresChecks(state, readCaCertificate(ctx)),
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

/** The variable that carries a service's database login, for example `..._EXAMPLE_SERVICE_DATABASE_USERNAME`. */
export function serviceLoginVariable(service: string, field: "USERNAME" | "PASSWORD"): string {
  return `TRUSTIOT_DEV_${service.toUpperCase().replaceAll("-", "_")}_DATABASE_${field}`;
}

/**
 * KEY=value lines (including the passwords) for a shell or a dotenv file. `caFile` is the path
 * of the dev CA certificate.
 *
 * Postgres needs TLS: the `PG*` variables make `psql` and every other libpq client verify the
 * server (`verify-full`, the dev CA, the loopback address that the certificate names). A service
 * that runs on the host reads the `TRUSTIOT_POSTGRES_*` platform settings (chart names) and its
 * own role from `TRUSTIOT_DEV_<SERVICE>_DATABASE_{USERNAME,PASSWORD}`, which it takes as
 * `TRUSTIOT__DATABASE__{USERNAME,PASSWORD}`. Every login the state file holds is printed: the
 * product's service roles and bound platform roles (lib/product-logins.ts; the platform role of
 * `assets` is `TRUSTIOT_DEV_ASSETS_PLATFORM_DATABASE_*`) and `example-service`'s.
 *
 * With `identity` (`up` wrote the key files), the lines for the identity service follow:
 * `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` is a platform setting every service verifies tokens with
 * (the public keys file, which a cluster mounts from a ConfigMap);
 * `TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE` (a path, never a key) and `TRUSTIOT_INGRESS_ROUTES` are
 * for the identity service only; its database logins are `TRUSTIOT_DEV_IDENTITY_DATABASE_*` like
 * every other service's, and `TRUSTIOT_DEV_IDENTITY_PLATFORM_DATABASE_*` for its platform role
 * (the event log, Laysi/trustiot-v2#130). The JSON values are single-quoted.
 */
export function envLines(state: State, caFile: string, identity?: IdentityEnv): string {
  const { postgres, clickhouse: ch, mqtt } = state.credentials;
  const { ports } = state;
  const serviceLogins: Record<string, string> = {};
  for (const [service, login] of Object.entries(state.credentials.services)) {
    serviceLogins[serviceLoginVariable(service, "USERNAME")] = login.user;
    serviceLogins[serviceLoginVariable(service, "PASSWORD")] = login.password;
  }
  const identityLines: Record<string, string> = identity
    ? {
        TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE: identity.publicKeysFile,
        TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE: identity.signingKeysFile,
        TRUSTIOT_INGRESS_ROUTES: identity.routes,
      }
    : {};
  const quoted = ["TRUSTIOT_INGRESS_ROUTES"];
  return dotenv({
    PGHOST: BIND_ADDRESS,
    PGPORT: ports.postgres,
    PGUSER: postgres.user,
    PGPASSWORD: postgres.password,
    PGDATABASE: postgres.database,
    PGSSLMODE: "verify-full",
    PGSSLROOTCERT: caFile,
    TRUSTIOT_POSTGRES_HOST: BIND_ADDRESS,
    TRUSTIOT_POSTGRES_PORT: ports.postgres,
    TRUSTIOT_POSTGRES_DATABASE: postgres.database,
    TRUSTIOT_POSTGRES_SSLMODE: "verify-full",
    TRUSTIOT_POSTGRES_CA_FILE: caFile,
    ...serviceLogins,
    TRUSTIOT_DEV_POSTGRES_URL: `postgres://${postgres.user}:${postgres.password}@${BIND_ADDRESS}:${ports.postgres}/${postgres.database}`,
    CLICKHOUSE_URL: `http://${BIND_ADDRESS}:${ports.clickhouse}`,
    CLICKHOUSE_USER: ch.user,
    CLICKHOUSE_PASSWORD: ch.password,
    CLICKHOUSE_DATABASE: ch.database,
    MQTT_HOST: BIND_ADDRESS,
    MQTT_PORT: ports.mqtt,
    MQTT_USER: mqtt.user,
    MQTT_PASSWORD: mqtt.password,
    ...identityLines,
  }, { quoted });
}

export function printEnv(ctx: Context = context()): void {
  const state = requireState(ctx);
  process.stdout.write(envLines(state, readCaPath(ctx), identityEnv(ctx.dir)));
}

/** The CA certificate's path; fails when the file is gone, so `env` never prints a dead path. */
function readCaPath(ctx: Context): string {
  const path = caCertificatePath(ctx.dir);
  if (!existsSync(path)) {
    throw new Error(`${path} is missing; run the down command and start again with up`);
  }
  return path;
}

function readCaCertificate(ctx: Context): string {
  return readFileSync(readCaPath(ctx), "utf8");
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

  const cookie = await seedIdentitySession(ctx, state);

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
  log("");
  log(`identity dev session (user ${DEV_USER_ID}, tenant ${DEV_TENANT_ID}, role admin), valid for ext_authz:`);
  log(`  Cookie: ${cookieHeader(cookie)}`);
}

/** Runs SQL in the Postgres container over its unix socket (no password anywhere) and returns the output. */
async function psqlOutput(ctx: Context, state: State, sql: string): Promise<string> {
  const { user, database } = state.credentials.postgres;
  const output = await podmanOk(
    [
      "exec",
      "--interactive",
      ctx.names.container("postgres"),
      "psql",
      `--username=${user}`,
      `--dbname=${database}`,
      "--no-psqlrc",
      "--quiet",
      "--tuples-only",
      "--no-align",
      "--set=ON_ERROR_STOP=1",
      "--file=-",
    ],
    { stdin: sql },
  );
  return output.trim();
}

/**
 * The development user and membership come from `services/identity/seed/postgres`; this adds the
 * session, which needs a random cookie and so cannot be a static file. The cookie is kept in the
 * state directory (it is the only copy: the database stores its hash) and reused while the
 * session behind it is valid for at least another hour; otherwise a new one is made. The cookie
 * is a development credential and is printed by `seed`, as the issue asks; nothing else here
 * prints a secret.
 */
async function seedIdentitySession(ctx: Context, state: State): Promise<string> {
  const live = async (cookie: string) => (await psqlOutput(ctx, state, liveSessionSql(cookie))) === "1";
  let cookie = readCookie(ctx.dir);
  if (cookie === undefined || !(await live(cookie))) {
    cookie = newCookie();
    // The file first: a cookie without a session is replaced by the next seed, a session whose
    // cookie nobody has is just unused.
    writeCookie(ctx.dir, cookie);
    await psqlOutput(ctx, state, createSessionSql(cookie));
    log("created the identity dev session");
  } else {
    log("kept the identity dev session");
  }
  if (!(await live(cookie))) throw new Error("seed verification failed: the identity dev session is not valid");
  return cookie;
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
    // The credentials and the certificates were generated for the containers that are gone. The
    // server's private key must not outlive them, so the volumes it was written to go too; `up`
    // makes new ones.
    for (const name of ["postgres-config", "postgres-ca"]) {
      await removeVolumeIfPresent(ctx.names.volume(name));
    }
    // The identity signing key and the dev session cookie stay: the sessions are in the data
    // volume, which stays too (purge removes all of it).
    removeStateExcept(ctx.dir, [IDENTITY_DIR]);
    log(
      `removed ${removed.length} container(s); data volumes and the identity signing key kept ` +
        "(use purge to delete them)",
    );
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
