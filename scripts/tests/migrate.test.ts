// How `up` runs the migration Job (lib/migrate.ts): its settings, what it may inherit, and what
// is printed when it fails.
import { describe, expect, test } from "bun:test";

import {
  jobEnvironment,
  MIGRATION_ROLE,
  passwordBindings,
  reportedChanges,
  reportedWarnings,
  scrub,
  warningLines,
  secretNames,
  type MigrationRun,
} from "../lib/migrate.ts";
import { productLogins } from "../lib/product-logins.ts";
import { migrationRoleSql } from "../lib/secrets.ts";

const run: MigrationRun = {
  binary: "/repo/target/debug/migrate",
  postgres: { port: 15432, database: "trustiot", caFile: "/state/tls/ca.crt" },
  login: { user: MIGRATION_ROLE, password: "migration-login-value" },
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

  test("inherits only what a process needs, not the caller's TRUSTIOT settings", () => {
    expect(env.PATH).toBe("/bin");
    expect(env.HOME).toBe("/home/dev");
    expect(Object.values(env)).not.toContain("someone-else");
    expect(env.TRUSTIOT_CONFIG_FILE).toBeUndefined();
    expect(env.CARGO_BUILD_JOBS).toBeUndefined();
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
        "ai_gateway_app=dev-ai-gateway-password",
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
