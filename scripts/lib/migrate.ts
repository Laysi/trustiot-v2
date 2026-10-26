// Runs the migration Job (tools/migrate, Laysi/trustiot-v2#60) against the local Postgres: the
// same binary and the same settings as the chart's Job, so the local roles, schemas, grants and
// passwords come from the code that makes them in a cluster and not from a second copy of it.
//
// The Job connects as the migration role over `verify-full` to the published loopback port, and
// reaches the password Secrets through the stand-in Kubernetes API (lib/secret-api.ts), which
// `up` starts for the length of the run. It also applies the product's ClickHouse DDL
// (services/ingest/migrations/clickhouse) to the local ClickHouse as the bootstrap account that
// has access management, the way the chart's Job does it (Laysi/trustiot-v2#679). Passwords
// reach the Job only through its environment and a token file (mode 0600, removed afterwards),
// never through the argument list.
import type { ProductLogin } from "./product-logins.ts";
import { startSecretApi } from "./secret-api.ts";
import { BIND_ADDRESS } from "./stack.ts";
import { withPrivateFile } from "./state.ts";

/** The migration role `up` creates; the chart's CloudNativePG bootstrap calls it the same. */
export const MIGRATION_ROLE = "trustiot_owner";

/** The namespace and the Secret the stand-in declares; names only, nothing is secret about them. */
export const STORE_NAMESPACE = "trustiot-dev";

/** The part of a {@link ProductLogin} the Job's password settings need. */
export type PasswordLogin = Pick<ProductLogin, "role" | "secret">;

/**
 * The Job's `TRUSTIOT__PASSWORDS__POSTGRES` value: the login roles it manages and the Secret
 * that holds each one's password. The services with a database role of their own that `up`
 * creates itself (`SERVICES`) are not in the Job's service list and are not listed.
 */
export function passwordBindings(logins: readonly PasswordLogin[]): string {
  return logins.map(({ role, secret }) => `${role}=${secret}`).join(",");
}

/** The Secrets the stand-in API declares for these logins, each once, in order. */
export function secretNames(logins: readonly PasswordLogin[]): string[] {
  return [...new Set(logins.map(({ secret }) => secret))];
}

export interface MigrationRun {
  binary: string;
  /** The Postgres published on loopback. */
  postgres: { port: number; database: string; caFile: string };
  /** The migration role's login. */
  login: { user: string; password: string };
  /**
   * The ClickHouse published on loopback and its bootstrap account, which has access management
   * (the container is started with `CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1`): the Job creates
   * the product's databases, tables, roles and row policies with it.
   */
  clickhouse: { port: number; user: string; password: string };
  /** Directory for the token file (the instance's state directory). */
  stateDir: string;
  /** The login roles the Job gives a password to, and the Secret of each (lib/product-logins.ts). */
  logins: readonly PasswordLogin[];
}

export interface MigrationResult {
  /** The password the Job generated for each role, by role name. */
  passwords: Record<string, string>;
  /** Number of changes the Job reports, when its log says. */
  changes: number | undefined;
  /** Number of warnings the Job reports; undefined when its log has no count (an older Job). */
  warnings: number | undefined;
  /** The Job's warning lines, secrets removed; what the count counts. */
  warningLines: string[];
}

/** The environment of the Job. Not inherited from the caller: a stray `TRUSTIOT_*` would change it. */
export function jobEnvironment(
  run: MigrationRun,
  api: { url: string },
  tokenFile: string,
  inherited: Record<string, string | undefined> = process.env,
): Record<string, string> {
  const env: Record<string, string> = {
    TRUSTIOT_POSTGRES_HOST: BIND_ADDRESS,
    TRUSTIOT_POSTGRES_PORT: String(run.postgres.port),
    TRUSTIOT_POSTGRES_DATABASE: run.postgres.database,
    TRUSTIOT_POSTGRES_SSLMODE: "verify-full",
    TRUSTIOT_POSTGRES_CA_FILE: run.postgres.caFile,
    // Plain HTTP on the loopback port, as the migrate tests reach their throwaway ClickHouse. The
    // collector and telemetry-query accounts (TRUSTIOT__PASSWORDS__CLICKHOUSE_*) stay unset: the
    // Job then leaves the collector alone and keeps `trustiot_telemetry_query` at HOST NONE.
    TRUSTIOT_CLICKHOUSE_URL: `http://${BIND_ADDRESS}:${run.clickhouse.port}`,
    TRUSTIOT__POSTGRES__USERNAME: run.login.user,
    TRUSTIOT__POSTGRES__PASSWORD: run.login.password,
    TRUSTIOT__CLICKHOUSE__USERNAME: run.clickhouse.user,
    TRUSTIOT__CLICKHOUSE__PASSWORD: run.clickhouse.password,
    TRUSTIOT__PASSWORDS__API_URL: api.url,
    TRUSTIOT__PASSWORDS__NAMESPACE: STORE_NAMESPACE,
    TRUSTIOT__PASSWORDS__TOKEN_FILE: tokenFile,
    // Unused for `http://`, but the setting is required; the dev CA is a file that exists.
    TRUSTIOT__PASSWORDS__CA_FILE: run.postgres.caFile,
    TRUSTIOT__PASSWORDS__GENERATION: "1",
    TRUSTIOT__PASSWORDS__POSTGRES: passwordBindings(run.logins),
    TRUSTIOT__WAIT__DATABASES: "2m",
    TRUSTIOT__WAIT__LOCK: "2m",
  };
  for (const name of ["PATH", "HOME", "TMPDIR", "LANG"]) {
    const value = inherited[name];
    if (value !== undefined) env[name] = value;
  }
  return env;
}

/** The text with every secret removed: the Job logs none, and this keeps it so if that changes. */
export function scrub(text: string, secrets: readonly string[]): string {
  let out = text;
  for (const secret of secrets) if (secret.length >= 8) out = out.replaceAll(secret, "<redacted>");
  return out;
}

/** A number the Job's final log line reports under `name`, or undefined when it has none. */
function reportedCount(log: string, name: "changes" | "warnings"): number | undefined {
  for (const line of log.split("\n").reverse()) {
    if (!line.startsWith("{") || !line.includes("migration run finished")) continue;
    try {
      const parsed = JSON.parse(line) as Record<string, unknown> & { fields?: Record<string, unknown> };
      const count = parsed.fields?.[name] ?? parsed[name];
      if (typeof count === "number") return count;
    } catch {
      // not JSON: keep looking
    }
  }
  return undefined;
}

/** How many changes the Job's final log line reports. */
export function reportedChanges(log: string): number | undefined {
  return reportedCount(log, "changes");
}

/** How many warnings the Job's final log line reports (Laysi/trustiot-v2#400); undefined in the old format. */
export function reportedWarnings(log: string): number | undefined {
  return reportedCount(log, "warnings");
}

/** The text of each warning the Job logged (the `warning` field of its log lines), in order. */
export function warningLines(log: string): string[] {
  const out: string[] = [];
  for (const line of log.split("\n")) {
    if (!line.startsWith("{") || !line.includes('"warning"')) continue;
    try {
      const parsed = JSON.parse(line) as { fields?: { warning?: unknown }; warning?: unknown };
      const warning = parsed.fields?.warning ?? parsed.warning;
      if (typeof warning === "string") out.push(warning);
    } catch {
      // not JSON: not a warning line
    }
  }
  return out;
}

export async function runMigrationJob(run: MigrationRun): Promise<MigrationResult> {
  const api = startSecretApi(STORE_NAMESPACE, secretNames(run.logins));
  try {
    return await withPrivateFile(run.stateDir, "kube-token", `${api.token}\n`, async (tokenFile) => {
      const child = Bun.spawn([run.binary], {
        cwd: run.stateDir,
        env: jobEnvironment(run, api, tokenFile),
        stdin: "ignore",
        stdout: "pipe",
        stderr: "pipe",
      });
      const [stdout, stderr, code] = await Promise.all([
        new Response(child.stdout).text(),
        new Response(child.stderr).text(),
        child.exited,
      ]);
      const passwords: Record<string, string> = {};
      for (const { role, secret } of run.logins) {
        const value = api.value(secret, role);
        if (value) passwords[role] = value;
      }
      const hidden = [run.login.password, run.clickhouse.password, api.token, ...Object.values(passwords)];
      if (code !== 0) {
        throw new Error(
          `the migration Job (tools/migrate) failed with exit code ${code}:\n` +
            scrub(`${stdout}${stderr}`.trim(), hidden),
        );
      }
      for (const { role } of run.logins) {
        if (!passwords[role]) {
          throw new Error(`the migration Job finished but left no password for ${role}`);
        }
      }
      const output = `${stdout}\n${stderr}`;
      return {
        passwords,
        changes: reportedChanges(output),
        warnings: reportedWarnings(output),
        warningLines: warningLines(output).map((line) => scrub(line, hidden)),
      };
    });
  } finally {
    api.stop();
  }
}
