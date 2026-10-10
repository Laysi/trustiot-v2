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
}

/** User names are not secrets; the passwords are generated for every `up`. */
export const DEV_USER = "trustiot_dev";
export const POSTGRES_DATABASE = "trustiot";
export const CLICKHOUSE_DATABASE = "trustiot";

export function generateCredentials(): Credentials {
  return {
    postgres: { user: DEV_USER, password: randomSecret(), database: POSTGRES_DATABASE },
    clickhouse: { user: DEV_USER, password: randomSecret(), database: CLICKHOUSE_DATABASE },
    mqtt: { user: DEV_USER, password: randomSecret() },
  };
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
 */
export function dotenv(entries: Record<string, string | number>): string {
  const lines = Object.entries(entries).map(([key, value]) => {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) {
      throw new Error(`invalid environment variable name: ${key}`);
    }
    const text = String(value);
    if (!/^[A-Za-z0-9_@:/.,+=-]*$/.test(text)) {
      throw new Error(`the value of ${key} contains characters that need quoting`);
    }
    return `${key}=${text}`;
  });
  return `${lines.join("\n")}\n`;
}
