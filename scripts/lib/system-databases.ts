// The dev Postgres revokes PUBLIC's CONNECT and TEMPORARY on `postgres` and `template1`, as the
// bundled Cluster does at bootstrap (Laysi/trustiot-v2#400, Laysi/trustiot-v2#427). The chart's
// file stays the only source of the statements: this reads it and keeps its `REVOKE`s. Its other
// statements create CloudNativePG's `streaming_replica` and `cnpg_metrics_exporter` roles and grant
// them CONNECT; the dev Postgres has no operator and its pg_hba requires a password, which those
// roles do not have, so they could never log in here and are not created.

/** The chart's statements for the cluster's other databases, relative to the repository root. */
export const SYSTEM_DATABASES_SQL = "deploy/chart/files/postgres/system-databases.sql";

/**
 * The statements of the file, in order. The file's format (its header): statements are separated
 * by a blank line, a line that starts with `--` is a comment, and each statement ends with `;`.
 */
export function systemDatabaseStatements(file: string): string[] {
  return file
    .split(/\n[ \t]*\n/)
    .map((block) =>
      block
        .split("\n")
        .filter((line) => !line.trimStart().startsWith("--"))
        .join("\n")
        .trim(),
    )
    .filter((statement) => statement !== "");
}

/** The file's `REVOKE` statements, one per line, ready to send to psql; throws when it has none. */
export function systemDatabaseRevokes(file: string): string {
  const revokes = systemDatabaseStatements(file).filter((statement) => /^REVOKE\s/i.test(statement));
  if (revokes.length === 0) {
    throw new Error(`${SYSTEM_DATABASES_SQL} has no REVOKE statement; the dev Postgres would keep PUBLIC's access`);
  }
  for (const statement of revokes) {
    if (!statement.endsWith(";")) throw new Error(`${SYSTEM_DATABASES_SQL}: a REVOKE does not end with ";"`);
  }
  return revokes.map((statement) => `${statement.replace(/\s+/g, " ")}\n`).join("");
}
