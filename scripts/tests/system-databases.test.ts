// The dev Postgres's revocation on `postgres` and `template1` is read from the chart's file
// (lib/system-databases.ts), so there is one copy of the statements.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { SYSTEM_DATABASES_SQL, systemDatabaseRevokes, systemDatabaseStatements } from "../lib/system-databases.ts";

const repoRoot = join(import.meta.dir, "..", "..");
const file = readFileSync(join(repoRoot, SYSTEM_DATABASES_SQL), "utf8");

describe("the chart's system-databases file", () => {
  test("yields exactly the two REVOKEs for postgres and template1", () => {
    expect(systemDatabaseRevokes(file)).toBe(
      "REVOKE CONNECT, TEMPORARY ON DATABASE postgres FROM PUBLIC;\n" +
        "REVOKE CONNECT, TEMPORARY ON DATABASE template1 FROM PUBLIC;\n",
    );
  });

  test("keeps the role statements out of what the dev environment runs", () => {
    expect(systemDatabaseRevokes(file)).not.toMatch(/CREATE ROLE|GRANT|streaming_replica|cnpg_metrics_exporter/);
    // They are in the file, so the parser is not just failing to see them.
    expect(systemDatabaseStatements(file).some((statement) => statement.includes("CREATE ROLE"))).toBe(true);
  });
});

describe("the statement parser", () => {
  test("splits on blank lines and drops comments", () => {
    expect(systemDatabaseStatements("-- c\nSELECT 1;\n\n-- d\nREVOKE a\n  ON b;\n")).toEqual([
      "SELECT 1;",
      "REVOKE a\n  ON b;",
    ]);
  });

  test("refuses a file without a REVOKE", () => {
    expect(() => systemDatabaseRevokes("-- only a comment\nSELECT 1;\n")).toThrow(/no REVOKE/);
  });

  test("refuses a REVOKE without a terminating semicolon", () => {
    expect(() => systemDatabaseRevokes("REVOKE CONNECT ON DATABASE postgres FROM PUBLIC\n")).toThrow(/does not end/);
  });
});
