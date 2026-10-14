// What `bun scripts/dev.ts env` prints for the TLS database, and where the generated files live.
import { describe, expect, test } from "bun:test";
import { join, relative } from "node:path";

import { spawnSync } from "node:child_process";

import { envLines, serviceLoginVariable } from "../lib/commands.ts";
import { ingressRoutes } from "../lib/identity.ts";
import { generateCredentials } from "../lib/secrets.ts";
import { caCertificatePath, stateDir, type State } from "../lib/state.ts";

const state = (): State => ({
  version: 2,
  instance: "t",
  startedAt: "2026-10-11T00:00:00.000Z",
  ports: { postgres: 15432, clickhouse: 18123, mqtt: 11883, services: { "example-service": 18080 } },
  credentials: generateCredentials({ "example-service": "example_service" }),
});

function parse(text: string): Record<string, string> {
  return Object.fromEntries(
    text
      .trim()
      .split("\n")
      .map((line) => {
        const at = line.indexOf("=");
        return [line.slice(0, at), line.slice(at + 1)];
      }),
  );
}

describe("env", () => {
  const caFile = "/state/trustiot-v2/dev/t/tls/ca.crt";

  test("prints the TLS settings a service reads, with the chart's variable names", () => {
    const env = parse(envLines(state(), caFile));
    expect(env.TRUSTIOT_POSTGRES_HOST).toBe("127.0.0.1");
    expect(env.TRUSTIOT_POSTGRES_PORT).toBe("15432");
    expect(env.TRUSTIOT_POSTGRES_DATABASE).toBe("trustiot");
    expect(env.TRUSTIOT_POSTGRES_SSLMODE).toBe("verify-full");
    expect(env.TRUSTIOT_POSTGRES_CA_FILE).toBe(caFile);
  });

  test("makes psql and other libpq clients verify the server too", () => {
    const env = parse(envLines(state(), caFile));
    expect(env.PGHOST).toBe("127.0.0.1");
    expect(env.PGSSLMODE).toBe("verify-full");
    expect(env.PGSSLROOTCERT).toBe(caFile);
  });

  test("never offers a weaker sslmode", () => {
    expect(envLines(state(), caFile)).not.toMatch(/sslmode=(disable|allow|prefer|require|verify-ca)/i);
  });

  test("gives the service's own role, not the dev superuser", () => {
    const current = state();
    const env = parse(envLines(current, caFile));
    const login = current.credentials.services["example-service"]!;
    expect(env.TRUSTIOT_DEV_EXAMPLE_SERVICE_DATABASE_USERNAME).toBe("example_service");
    expect(env.TRUSTIOT_DEV_EXAMPLE_SERVICE_DATABASE_PASSWORD).toBe(login.password);
    expect(login.password).not.toBe(current.credentials.postgres.password);
    expect(serviceLoginVariable("example-service", "USERNAME")).toBe(
      "TRUSTIOT_DEV_EXAMPLE_SERVICE_DATABASE_USERNAME",
    );
  });

  test("keeps the variables that existed before", () => {
    const env = parse(envLines(state(), caFile));
    for (const key of ["PGPORT", "PGUSER", "PGPASSWORD", "PGDATABASE", "TRUSTIOT_DEV_POSTGRES_URL", "CLICKHOUSE_URL", "MQTT_PORT"]) {
      expect(env[key], key).toBeDefined();
    }
  });
});

describe("env for the identity service", () => {
  const caFile = "/state/trustiot-v2/dev/t/tls/ca.crt";
  const keys = '{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"dev-0123456789ab","x":"AAAA"}]}';
  const identity = { signingKeyFile: "/state/trustiot-v2/dev/t/identity/signing-key.pem", keys, routes: ingressRoutes() };
  const withIdentity = (): State => {
    const current = state();
    current.credentials.services.identity = { user: "identity_app", password: "generated-by-the-job" };
    return current;
  };

  test("prints the public keys, the key file's path and the routes, and the identity_app login", () => {
    const text = envLines(withIdentity(), caFile, identity);
    const env = parse(text);
    expect(env.TRUSTIOT_INTERNAL_IDENTITY_KEYS).toBe(`'${keys}'`);
    expect(env.TRUSTIOT_IDENTITY_SIGNING_KEY_FILE).toBe(identity.signingKeyFile);
    expect(env.TRUSTIOT_INGRESS_ROUTES).toBe(`'${identity.routes}'`);
    expect(env.TRUSTIOT_DEV_IDENTITY_DATABASE_USERNAME).toBe("identity_app");
    expect(env.TRUSTIOT_DEV_IDENTITY_DATABASE_PASSWORD).toBe("generated-by-the-job");
    expect(text).not.toContain("PRIVATE KEY");
  });

  test("prints a platform login under its own variables, after the service's own role", () => {
    const current = withIdentity();
    current.credentials.services["assets-platform"] = { user: "assets_platform", password: "platform-generated" };
    const env = parse(envLines(current, caFile, identity));
    expect(env.TRUSTIOT_DEV_ASSETS_PLATFORM_DATABASE_USERNAME).toBe("assets_platform");
    expect(env.TRUSTIOT_DEV_ASSETS_PLATFORM_DATABASE_PASSWORD).toBe("platform-generated");
    expect(serviceLoginVariable("assets-platform", "PASSWORD")).toBe("TRUSTIOT_DEV_ASSETS_PLATFORM_DATABASE_PASSWORD");
  });

  test("a shell that evals the output gets the JSON values back unchanged", () => {
    const script =
      'set -a; eval "$1"; set +a; printf "%s\\n%s\\n%s" "$TRUSTIOT_INTERNAL_IDENTITY_KEYS" "$TRUSTIOT_INGRESS_ROUTES" "$TRUSTIOT_DEV_IDENTITY_DATABASE_PASSWORD"';
    const result = spawnSync("sh", ["-c", script, "sh", envLines(withIdentity(), caFile, identity)], { encoding: "utf8" });
    expect(result.status).toBe(0);
    const lines = result.stdout.split("\n");
    expect(lines).toEqual([keys, identity.routes, "generated-by-the-job"]);
    expect(JSON.parse(lines[1]!)[0].host).toBe("localhost");
  });

  test("prints none of it before the instance has a key, and the rest is unchanged", () => {
    const text = envLines(state(), caFile);
    for (const name of ["TRUSTIOT_INTERNAL_IDENTITY_KEYS", "TRUSTIOT_IDENTITY_SIGNING_KEY_FILE", "TRUSTIOT_INGRESS_ROUTES", "IDENTITY"]) {
      expect(text, name).not.toContain(name);
    }
    expect(parse(text).TRUSTIOT_POSTGRES_HOST).toBe("127.0.0.1");
  });
});

describe("generated certificate files", () => {
  test("are inside the instance's state directory, never in the repository", () => {
    const repo = join(import.meta.dir, "..", "..");
    const dir = stateDir({ XDG_STATE_HOME: "/home/dev/.local/state" }, "t");
    expect(caCertificatePath(dir)).toBe("/home/dev/.local/state/trustiot-v2/dev/t/tls/ca.crt");
    expect(relative(repo, caCertificatePath(dir)).startsWith("..")).toBe(true);
  });
});
