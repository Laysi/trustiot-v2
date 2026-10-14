// Credential generation and encoding. Pure functions plus one source of randomness
// (node:crypto); nothing here touches the disk or the network.
import { pbkdf2Sync, randomBytes } from "node:crypto";

/** Bytes of entropy per generated secret: 256 bits. */
export const SECRET_BYTES = 32;

/** A fresh URL-safe secret (base64url, 43 characters for 32 bytes). */
export function randomSecret(bytes: number = SECRET_BYTES): string {
  if (!Number.isInteger(bytes) || bytes < 16) {
    throw new Error("a secret needs at least 16 random bytes");
  }
  return randomBytes(bytes).toString("base64url");
}

export interface Login {
  user: string;
  password: string;
}

export interface Credentials {
  postgres: Login & { database: string };
  clickhouse: Login & { database: string };
  mqtt: Login;
  /** Database logins of the services that have a database role, by service name. */
  services: Record<string, Login>;
}

/** User names are not secrets; the passwords are generated for every `up`. */
export const DEV_USER = "trustiot_dev";
export const POSTGRES_DATABASE = "trustiot";
export const CLICKHOUSE_DATABASE = "trustiot";

/**
 * New passwords for every component and, for `serviceRoles` (service name to database role
 * name), a new login per service.
 */
export function generateCredentials(serviceRoles: Record<string, string> = {}): Credentials {
  return {
    postgres: { user: DEV_USER, password: randomSecret(), database: POSTGRES_DATABASE },
    clickhouse: { user: DEV_USER, password: randomSecret(), database: CLICKHOUSE_DATABASE },
    mqtt: { user: DEV_USER, password: randomSecret() },
    services: Object.fromEntries(
      Object.entries(serviceRoles).map(([service, role]) => [
        service,
        { user: role, password: randomSecret() },
      ]),
    ),
  };
}

/**
 * SQL that creates the service's database role if it is missing and sets its password. The
 * role is a plain login role: it cannot be a superuser, bypass row-level security, create
 * databases or roles, or replicate, and it owns nothing, which is what service-kit's session
 * check demands of a service role. Idempotent, because the data volume outlives `down`.
 */
export function serviceRoleSql(role: string, password: string): string {
  if (!/^[a-z][a-z0-9_]{0,62}$/.test(role)) {
    throw new Error(`invalid database role name: ${role}`);
  }
  const attributes = "LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION";
  return (
    `DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = ${sqlLiteral(role)}) ` +
    `THEN CREATE ROLE ${sqlIdentifier(role)} ${attributes}; END IF; END $$;\n` +
    `ALTER ROLE ${sqlIdentifier(role)} WITH ${attributes} PASSWORD ${sqlLiteral(password)};\n`
  );
}

/**
 * SQL that creates the migration role of tools/migrate if it is missing, sets its password and
 * makes it the owner of the database, which is the shape the Job demands (README of tools/migrate,
 * "一次執行做什麼" step 2): owns the database, has CREATEROLE, and is not a superuser, does not
 * bypass row-level security and owns no other database. The chart's CloudNativePG bootstrap gives
 * the bundled Postgres the same `trustiot_owner`. Idempotent, because the data volume outlives
 * `down`; the password is new for every `up` and only that run (and the Job it starts) knows it.
 */
export function migrationRoleSql(role: string, password: string, database: string): string {
  if (!/^[a-z][a-z0-9_]{0,62}$/.test(role)) {
    throw new Error(`invalid database role name: ${role}`);
  }
  const attributes = "LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB CREATEROLE NOREPLICATION";
  return (
    `DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = ${sqlLiteral(role)}) ` +
    `THEN CREATE ROLE ${sqlIdentifier(role)} ${attributes}; END IF; END $$;\n` +
    `ALTER ROLE ${sqlIdentifier(role)} WITH ${attributes} PASSWORD ${sqlLiteral(password)};\n` +
    `ALTER DATABASE ${sqlIdentifier(database)} OWNER TO ${sqlIdentifier(role)};\n`
  );
}

/**
 * The entry for a Mosquitto password file in the `sha512-pbkdf2` format
 * (`user:$7$<iterations>$<salt>$<hash>`, standard base64). Computed here so the password never
 * appears on a command line; `mosquitto_passwd` only accepts it as an argument.
 */
export function mosquittoPasswordEntry(
  user: string,
  password: string,
  salt: Buffer = randomBytes(12),
): string {
  if (!/^[A-Za-z0-9_.-]+$/.test(user)) {
    throw new Error("an MQTT user name may only contain letters, digits, '_', '.' and '-'");
  }
  const iterations = 101;
  const hash = pbkdf2Sync(password, salt, iterations, 64, "sha512");
  return `${user}:$7$${iterations}$${salt.toString("base64")}$${hash.toString("base64")}\n`;
}

/** Single-quoted SQL string literal. */
export function sqlLiteral(value: string): string {
  return `'${value.replaceAll("'", "''")}'`;
}

/** Double-quoted SQL identifier. */
export function sqlIdentifier(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}

/**
 * `KEY=value` lines for `podman --env-file` and shells. Values are restricted to a character set
 * that needs no quoting, so the output means the same to podman, to `export` and to dotenv
 * readers.
 *
 * `options.quoted` names the keys whose value is JSON (a JWK Set, a route table): that text has
 * quotes and braces, so it is written in single quotes, which a shell `eval` and dotenv readers
 * understand but `podman --env-file` would keep literally. Only a value without a single quote,
 * newline or NUL byte can be quoted this way.
 */
export function dotenv(
  entries: Record<string, string | number>,
  options: { quoted?: readonly string[] } = {},
): string {
  const lines = Object.entries(entries).map(([key, value]) => {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) {
      throw new Error(`invalid environment variable name: ${key}`);
    }
    const text = String(value);
    if (options.quoted?.includes(key)) {
      if (/['\n\r\0]/.test(text)) {
        throw new Error(`the value of ${key} contains characters that cannot be single-quoted`);
      }
      return `${key}='${text}'`;
    }
    if (!/^[A-Za-z0-9_@:/.,+=-]*$/.test(text)) {
      throw new Error(`the value of ${key} contains characters that need quoting`);
    }
    return `${key}=${text}`;
  });
  return `${lines.join("\n")}\n`;
}
