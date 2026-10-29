// How `up` runs the migration Job (lib/migrate.ts): its settings, what it may inherit, and what
// is printed when it fails.
import { describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  jobEnvironment,
  MIGRATION_ROLE,
  passwordBindings,
  reportedChanges,
  reportedWarnings,
  scrub,
  warningLines,
  secretNames,
  runMigrationJob,
  type MigrationRun,
} from "../lib/migrate.ts";
import { productLogins } from "../lib/product-logins.ts";
import { migrationRoleSql } from "../lib/secrets.ts";

const run: MigrationRun = {
  binary: "/repo/target/debug/migrate",
  postgres: { port: 15432, database: "trustiot", caFile: "/state/tls/ca.crt" },
  login: { user: MIGRATION_ROLE, password: "migration-login-value" },
  clickhouse: { port: 18123, user: "trustiot_dev", password: "clickhouse-login-value" },
  stateDir: "/state",
  logins: [{ role: "identity_app", secret: "dev-identity-password" }],
};

describe("the Job's environment", () => {
  const env = jobEnvironment(run, { url: "http://127.0.0.1:4321" }, "/state/kube-token.abc", {
    PATH: "/bin",
    HOME: "/home/dev",
    TRUSTIOT__POSTGRES__USERNAME: "someone-else",
    TRUSTIOT_CONFIG_FILE: "/elsewhere.toml",
    CARGO_BUILD_JOBS: "2",
  });

  test("names the local Postgres as the chart's platform settings do, with verify-full", () => {
    expect(env.TRUSTIOT_POSTGRES_HOST).toBe("127.0.0.1");
    expect(env.TRUSTIOT_POSTGRES_PORT).toBe("15432");
    expect(env.TRUSTIOT_POSTGRES_DATABASE).toBe("trustiot");
    expect(env.TRUSTIOT_POSTGRES_SSLMODE).toBe("verify-full");
    expect(env.TRUSTIOT_POSTGRES_CA_FILE).toBe("/state/tls/ca.crt");
  });

  test("logs in as the migration role and reaches the passwords through the stand-in API", () => {
    expect(env.TRUSTIOT__POSTGRES__USERNAME).toBe("trustiot_owner");
    expect(env.TRUSTIOT__POSTGRES__PASSWORD).toBe("migration-login-value");
    expect(env.TRUSTIOT__PASSWORDS__API_URL).toBe("http://127.0.0.1:4321");
    expect(env.TRUSTIOT__PASSWORDS__TOKEN_FILE).toBe("/state/kube-token.abc");
    expect(env.TRUSTIOT__PASSWORDS__POSTGRES).toBe("identity_app=dev-identity-password");
    expect(env.TRUSTIOT__PASSWORDS__GENERATION).toBe("1");
  });

  test("applies the product's ClickHouse DDL to the local ClickHouse as its bootstrap account", () => {
    expect(env.TRUSTIOT_CLICKHOUSE_URL).toBe("http://127.0.0.1:18123");
    expect(env.TRUSTIOT__CLICKHOUSE__USERNAME).toBe("trustiot_dev");
    expect(env.TRUSTIOT__CLICKHOUSE__PASSWORD).toBe("clickhouse-login-value");
    // The password comes with the account, not through a Secret of the stand-in API.
    expect(env.TRUSTIOT__CLICKHOUSE__PASSWORD_SECRET).toBeUndefined();
  });

  test("leaves the collector and telemetry-query accounts unset, as a chart without them does", () => {
    expect(env.TRUSTIOT__PASSWORDS__CLICKHOUSE_COLLECTOR).toBeUndefined();
    expect(env.TRUSTIOT__PASSWORDS__CLICKHOUSE_TELEMETRY_QUERY).toBeUndefined();
    expect(env.TRUSTIOT_CLICKHOUSE_CA_FILE).toBeUndefined();
    expect(env.TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS).toBeUndefined();
  });

  test("follows the ClickHouse port the environment published", () => {
    const other = jobEnvironment(
      { ...run, clickhouse: { ...run.clickhouse, port: 28123 } },
      { url: "http://127.0.0.1:4321" },
      "/state/kube-token.abc",
      {},
    );
    expect(other.TRUSTIOT_CLICKHOUSE_URL).toBe("http://127.0.0.1:28123");
  });

  test("keeps the Postgres settings the same with and without the ClickHouse ones", () => {
    const postgres = Object.fromEntries(
      Object.entries(env).filter(([name]) => !name.includes("CLICKHOUSE") && !["PATH", "HOME"].includes(name)),
    );
    expect(postgres).toEqual({
      TRUSTIOT_POSTGRES_HOST: "127.0.0.1",
      TRUSTIOT_POSTGRES_PORT: "15432",
      TRUSTIOT_POSTGRES_DATABASE: "trustiot",
      TRUSTIOT_POSTGRES_SSLMODE: "verify-full",
      TRUSTIOT_POSTGRES_CA_FILE: "/state/tls/ca.crt",
      TRUSTIOT__POSTGRES__USERNAME: "trustiot_owner",
      TRUSTIOT__POSTGRES__PASSWORD: "migration-login-value",
      TRUSTIOT__PASSWORDS__API_URL: "http://127.0.0.1:4321",
      TRUSTIOT__PASSWORDS__NAMESPACE: "trustiot-dev",
      TRUSTIOT__PASSWORDS__TOKEN_FILE: "/state/kube-token.abc",
      TRUSTIOT__PASSWORDS__CA_FILE: "/state/tls/ca.crt",
      TRUSTIOT__PASSWORDS__GENERATION: "1",
      TRUSTIOT__PASSWORDS__POSTGRES: "identity_app=dev-identity-password",
      TRUSTIOT__WAIT__DATABASES: "2m",
      TRUSTIOT__WAIT__LOCK: "2m",
    });
  });

  test("inherits only what a process needs, not the caller's TRUSTIOT settings", () => {
    expect(env.PATH).toBe("/bin");
    expect(env.HOME).toBe("/home/dev");
    expect(Object.values(env)).not.toContain("someone-else");
    expect(env.TRUSTIOT_CONFIG_FILE).toBeUndefined();
    expect(env.CARGO_BUILD_JOBS).toBeUndefined();
  });
});

describe("how the Job is started", () => {
  // A stand-in binary that only reports what it was given and fails: nothing here starts a
  // database or a container. runMigrationJob prints the Job's output when it fails, so the
  // report shows the argument count and the environment the process received, and what the
  // error message does and does not contain.
  const clickhousePassword = "clickhouse-pass-value-0123";
  const migrationPassword = "migration-pass-value-4567";

  async function failingRun(): Promise<{ error: Error }> {
    const dir = mkdtempSync(join(tmpdir(), "trustiot-dev-migrate-"));
    try {
      const binary = join(dir, "migrate");
      writeFileSync(
        binary,
        [
          "#!/bin/sh",
          'echo "argc=$#"',
          'echo "clickhouse-url=$TRUSTIOT_CLICKHOUSE_URL"',
          'echo "clickhouse-user=$TRUSTIOT__CLICKHOUSE__USERNAME"',
          'echo "clickhouse-password=$TRUSTIOT__CLICKHOUSE__PASSWORD"',
          'echo "postgres-password=$TRUSTIOT__POSTGRES__PASSWORD"',
          "exit 3",
          "",
        ].join("\n"),
      );
      chmodSync(binary, 0o700);
      const error = await runMigrationJob({
        ...run,
        binary,
        stateDir: dir,
        login: { user: MIGRATION_ROLE, password: migrationPassword },
        clickhouse: { port: 18123, user: "trustiot_dev", password: clickhousePassword },
      }).then(
        () => new Error("the Job was expected to fail"),
        (reason: unknown) => (reason instanceof Error ? reason : new Error(String(reason))),
      );
      return { error };
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  }

  test("takes no argument, so no password can be on its command line, and gets the ClickHouse settings in its environment", async () => {
    const { error } = await failingRun();
    expect(error.message).toContain("failed with exit code 3");
    expect(error.message).toContain("argc=0");
    expect(error.message).toContain("clickhouse-url=http://127.0.0.1:18123");
    expect(error.message).toContain("clickhouse-user=trustiot_dev");
  });

  test("removes the ClickHouse and migration passwords from the output it prints when the Job fails", async () => {
    const { error } = await failingRun();
    expect(error.message).not.toContain(clickhousePassword);
    expect(error.message).not.toContain(migrationPassword);
    expect(error.message).toContain("clickhouse-password=<redacted>");
    expect(error.message).toContain("postgres-password=<redacted>");
  });
});

describe("password bindings", () => {
  test("pair each login role with the Secret that holds its password", () => {
    expect(passwordBindings([{ role: "identity_app", secret: "dev-identity-password" }])).toBe(
      "identity_app=dev-identity-password",
    );
    expect(
      passwordBindings([
        { role: "a_app", secret: "dev-a-password" },
        { role: "b_app", secret: "dev-b-password" },
      ]),
    ).toBe("a_app=dev-a-password,b_app=dev-b-password");
  });

  test("bind every login role of the product, with one Secret per service as the chart declares", () => {
    const logins = productLogins();
    const text = passwordBindings(logins);
    expect(text).toBe(
      "identity_app=dev-identity-password,identity_platform=dev-identity-password," +
        "assets_app=dev-assets-password,assets_platform=dev-assets-password," +
        "telemetry_query_app=dev-telemetry-query-password,telemetry_query_platform=dev-telemetry-query-password," +
        "certs_app=dev-certs-password,certs_platform=dev-certs-password,assistant_app=dev-assistant-password," +
        "ai_gateway_app=dev-ai-gateway-password," +
        "ingest_app=dev-ingest-password,ingest_platform=dev-ingest-password," +
        "gateway_ops_app=dev-gateway-ops-password,gateway_ops_platform=dev-gateway-ops-password",
    );
    // The Job refuses a binding whose Secret the API does not declare, and the same Secret holds
    // a service's two passwords: declared once.
    expect(secretNames(logins)).toEqual([
      "dev-identity-password",
      "dev-assets-password",
      "dev-telemetry-query-password",
      "dev-certs-password",
      "dev-assistant-password",
      "dev-ai-gateway-password",
      "dev-ingest-password",
      "dev-gateway-ops-password",
    ]);
    const env = jobEnvironment({ ...run, logins }, { url: "http://127.0.0.1:4321" }, "/state/kube-token.abc", {});
    expect(env.TRUSTIOT__PASSWORDS__POSTGRES).toBe(text);
  });
});

describe("what is printed when the Job fails", () => {
  test("every secret is removed from the text", () => {
    const text = "login failed for password long-secret-value and token another-secret-value; short x";
    expect(scrub(text, ["long-secret-value", "another-secret-value", "x"])).toBe(
      "login failed for password <redacted> and token <redacted>; short x",
    );
  });

  test("reads the number of changes from the final log line", () => {
    const log = [
      '{"message":"created role identity_owner"}',
      '{"fields":{"message":"migration run finished","changes":14}}',
      "not json",
    ].join("\n");
    expect(reportedChanges(log)).toBe(14);
    expect(reportedChanges('{"message":"migration run finished","changes":3}')).toBe(3);
    expect(reportedChanges("nothing here")).toBeUndefined();
  });
});

describe("the warnings the Job reports", () => {
  test("reads the count from the final log line", () => {
    const log = [
      '{"fields":{"message":"another database admits the service roles","warning":"database postgres: may connect"}}',
      '{"fields":{"message":"migration run finished","changes":14,"warnings":2}}',
    ].join("\n");
    expect(reportedWarnings(log)).toBe(2);
    expect(reportedWarnings('{"message":"migration run finished","changes":3,"warnings":0}')).toBe(0);
  });

  test("is undefined without a count and for a line in the old format", () => {
    expect(reportedWarnings('{"fields":{"message":"migration run finished","changes":3}}')).toBeUndefined();
    expect(reportedWarnings('{"fields":{"message":"migration run finished","warnings":"2"}}')).toBeUndefined();
    expect(reportedWarnings("migration run finished warnings=2")).toBeUndefined();
    expect(reportedWarnings("")).toBeUndefined();
  });

  test("collects the text of each warning line and ignores the rest", () => {
    const log = [
      '{"fields":{"message":"another database admits the service roles","warning":"database postgres: roles a may connect"}}',
      'not json "warning"',
      '{"fields":{"message":"created role identity_owner"}}',
      '{"warning":"database template1: roles a may connect"}',
    ].join("\n");
    expect(warningLines(log)).toEqual([
      "database postgres: roles a may connect",
      "database template1: roles a may connect",
    ]);
    expect(warningLines("")).toEqual([]);
  });
});

describe("the migration role", () => {
  const sql = migrationRoleSql("trustiot_owner", "p'w", "trustiot");

  test("is a login role that is neither a superuser nor bypasses row-level security, and can create roles", () => {
    expect(sql).toContain('CREATE ROLE "trustiot_owner" LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB CREATEROLE NOREPLICATION');
    expect(sql).toContain('ALTER ROLE "trustiot_owner" WITH LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB CREATEROLE NOREPLICATION');
    expect(sql).toContain("PASSWORD 'p''w'");
  });

  test("owns the database, which is what the Job checks", () => {
    expect(sql).toContain('ALTER DATABASE "trustiot" OWNER TO "trustiot_owner"');
  });

  test("can run again, and refuses a name that is not a plain identifier", () => {
    expect(sql).toContain("IF NOT EXISTS (SELECT FROM pg_roles");
    // A dollar-quoted block, as serviceRoleSql writes it.
    expect(sql).toStartWith("DO $$ BEGIN IF NOT EXISTS");
    expect(sql).toContain("END IF; END $$;\n");
    expect(() => migrationRoleSql('x"; DROP', "p", "trustiot")).toThrow(/invalid database role name/);
  });
});
