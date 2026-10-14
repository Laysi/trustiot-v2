// Runs the migration Job (tools/migrate, Laysi/trustiot-v2#60) against the local Postgres: the
// same binary and the same settings as the chart's Job, so the local roles, schemas, grants and
// passwords come from the code that makes them in a cluster and not from a second copy of it.
//
// The Job connects as the migration role over `verify-full` to the published loopback port, and
// reaches the password Secrets through the stand-in Kubernetes API (lib/secret-api.ts), which
// `up` starts for the length of the run. Passwords reach the Job only through its environment
// and a token file (mode 0600, removed afterwards), never through the argument list.
import { startSecretApi } from "./secret-api.ts";
import { BIND_ADDRESS } from "./stack.ts";
import { withPrivateFile } from "./state.ts";

/** The migration role `up` creates; the chart's CloudNativePG bootstrap calls it the same. */
export const MIGRATION_ROLE = "trustiot_owner";

/** The namespace and the Secret the stand-in declares; names only, nothing is secret about them. */
export const STORE_NAMESPACE = "trustiot-dev";
export const IDENTITY_LOGIN_STORE = "dev-identity-password";

/**
 * The Job's `TRUSTIOT__PASSWORDS__POSTGRES` value: the login roles it manages and the Secret
 * that holds each one's password. The services with a database role of their own that `up`
 * creates itself (`SERVICES`) are not in the Job's service list and are not listed.
 */
export function passwordBindings(roles: readonly string[]): string {
  return roles.map((role) => `${role}=${IDENTITY_LOGIN_STORE}`).join(",");
}

export interface MigrationRun {
  binary: string;
  /** The Postgres published on loopback. */
  postgres: { port: number; database: string; caFile: string };
  /** The migration role's login. */
  login: { user: string; password: string };
  /** Directory for the token file (the instance's state directory). */
  stateDir: string;
  /** The login roles the Job gives a password to: today `identity_app`. */
  roles: readonly string[];
}

export interface MigrationResult {
  /** The password the Job generated for each role, by role name. */
  passwords: Record<string, string>;
  /** Number of changes the Job reports, when its log says. */
  changes: number | undefined;
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
    TRUSTIOT__POSTGRES__USERNAME: run.login.user,
    TRUSTIOT__POSTGRES__PASSWORD: run.login.password,
    TRUSTIOT__PASSWORDS__API_URL: api.url,
    TRUSTIOT__PASSWORDS__NAMESPACE: STORE_NAMESPACE,
    TRUSTIOT__PASSWORDS__TOKEN_FILE: tokenFile,
    // Unused for `http://`, but the setting is required; the dev CA is a file that exists.
    TRUSTIOT__PASSWORDS__CA_FILE: run.postgres.caFile,
    TRUSTIOT__PASSWORDS__GENERATION: "1",
    TRUSTIOT__PASSWORDS__POSTGRES: passwordBindings(run.roles),
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

/** How many changes the Job's final log line reports. */
export function reportedChanges(log: string): number | undefined {
  for (const line of log.split("\n").reverse()) {
    if (!line.startsWith("{") || !line.includes("migration run finished")) continue;
    try {
      const parsed = JSON.parse(line) as { changes?: unknown; fields?: { changes?: unknown } };
      const changes = parsed.fields?.changes ?? parsed.changes;
      if (typeof changes === "number") return changes;
    } catch {
      // not JSON: keep looking
    }
  }
  return undefined;
}

export async function runMigrationJob(run: MigrationRun): Promise<MigrationResult> {
  const api = startSecretApi(STORE_NAMESPACE, [IDENTITY_LOGIN_STORE]);
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
      for (const role of run.roles) {
        const value = api.value(IDENTITY_LOGIN_STORE, role);
        if (value) passwords[role] = value;
      }
      const hidden = [run.login.password, api.token, ...Object.values(passwords)];
      if (code !== 0) {
        throw new Error(
          `the migration Job (tools/migrate) failed with exit code ${code}:\n` +
            scrub(`${stdout}${stderr}`.trim(), hidden),
        );
      }
      for (const role of run.roles) {
        if (!passwords[role]) {
          throw new Error(`the migration Job finished but left no password for ${role}`);
        }
      }
      return { passwords, changes: reportedChanges(stdout) };
    });
  } finally {
    api.stop();
  }
}
