import { describe, expect, test } from "bun:test";

import {
  assertDistinctPorts,
  BIND_ADDRESS,
  DEFAULT_PORTS,
  DIGEST_PATTERN,
  IMAGES,
  MOSQUITTO_CONFIG,
  mosquittoAcl,
  namesFor,
  POSTGRES_CONFIG_DIR,
  POSTGRES_HBA,
  POSTGRES_HBA_PATH,
  postgresCommand,
  resolvePorts,
  runArgs,
  SERVICE_CA_PATH,
  SERVICE_PORT,
  type ContainerSpec,
} from "../lib/stack.ts";
import {
  containerfile,
  platformEnv,
  portVariable,
  resolveServicePorts,
  SERVICES,
  type ServiceDef,
} from "../lib/services.ts";

const spec = (overrides: Partial<ContainerSpec> = {}): ContainerSpec => ({
  component: "postgres",
  image: IMAGES.postgres,
  memory: "256m",
  publish: [[15432, 5432]],
  envFile: "/state/postgres.env",
  ...overrides,
});

describe("images", () => {
  test("every image is pinned by digest", () => {
    for (const [name, image] of Object.entries(IMAGES)) {
      expect(DIGEST_PATTERN.test(image), name).toBe(true);
      expect(image, name).not.toMatch(/:latest/);
    }
  });

  test("runArgs refuses an image that is not pinned", () => {
    expect(() => runArgs("t", spec({ image: "docker.io/library/postgres:18" }))).toThrow(/digest/);
  });

  test("a locally built service image is allowed", () => {
    expect(() => runArgs("t", spec({ image: "localhost/trustiot-dev-t-x:dev" }))).not.toThrow();
  });
});

describe("runArgs", () => {
  test("publishes only on the loopback address", () => {
    const args = runArgs("t", spec({ publish: [[15432, 5432], [18123, 8123]] }));
    const published = args.flatMap((arg, index) => (args[index - 1] === "--publish" ? [arg] : []));
    expect(published).toEqual([
      `${BIND_ADDRESS}:15432:5432/tcp`,
      `${BIND_ADDRESS}:18123:8123/tcp`,
    ]);
    expect(args.join(" ")).not.toContain("0.0.0.0");
    expect(args).not.toContain("--publish-all");
    expect(args).not.toContain("-P");
    expect(args).not.toContain("--network=host");
  });

  test("labels the container so down and purge can find it", () => {
    const args = runArgs("t", spec());
    expect(args).toContain(namesFor("t").label);
    expect(args).toContain("trustiot-dev-t-postgres");
  });

  test("passes secrets only through the env file, which holds no values here", () => {
    const args = runArgs("t", spec({ env: { TRUSTIOT_DEPLOYMENT_MODE: "onprem" } }));
    expect(args).toContain("--env-file");
    expect(args).toContain("/state/postgres.env");
    expect(args.filter((arg) => /PASSWORD|SECRET|TOKEN/i.test(arg))).toEqual([]);
  });

  test("keeps the image and command last", () => {
    const args = runArgs("t", spec({ command: ["postgres", "-c", "x=y"] }));
    expect(args.slice(-4)).toEqual([IMAGES.postgres, "postgres", "-c", "x=y"]);
  });

  test("drops privilege escalation and never restarts by itself", () => {
    const args = runArgs("t", spec());
    expect(args.join(" ")).toContain("--security-opt no-new-privileges");
    expect(args.join(" ")).toContain("--restart no");
  });
});

describe("resolvePorts", () => {
  test("uses the defaults", () => {
    expect(resolvePorts({})).toEqual(DEFAULT_PORTS);
  });

  test("takes overrides", () => {
    expect(resolvePorts({ TRUSTIOT_DEV_POSTGRES_PORT: "25432" }).postgres).toBe(25432);
  });

  test("rejects invalid and duplicate ports", () => {
    for (const bad of ["abc", "80", "70000", "1e4", "-1", "15432.5"]) {
      expect(() => resolvePorts({ TRUSTIOT_DEV_MQTT_PORT: bad })).toThrow(/port number/);
    }
    expect(() =>
      resolvePorts({ TRUSTIOT_DEV_MQTT_PORT: "20000", TRUSTIOT_DEV_CLICKHOUSE_PORT: "20000" }),
    ).toThrow(/differ/);
  });
});

describe("assertDistinctPorts", () => {
  test("accepts distinct ports and names both parties of a collision", () => {
    expect(() => assertDistinctPorts({ postgres: 1, "service a": 2, "service b": 3 })).not.toThrow();
    expect(() => assertDistinctPorts({ postgres: 18080, "service a": 18080 })).toThrow(
      /postgres and service a would both publish host port 18080/,
    );
  });

  test("the default ports of the infrastructure and the services do not collide", () => {
    const defaults: Record<string, number> = { ...DEFAULT_PORTS };
    for (const service of SERVICES) defaults[`service ${service.name}`] = service.hostPort;
    expect(() => assertDistinctPorts(defaults)).not.toThrow();
  });
});

describe("generated configuration", () => {
  test("Postgres trusts only the local socket and demands TLS and SCRAM everywhere else", () => {
    const rules = POSTGRES_HBA.split("\n").filter((line) => line && !line.startsWith("#"));
    expect(rules).toEqual([
      "local all all trust",
      "hostssl all all 0.0.0.0/0 scram-sha-256",
      "hostssl all all ::/0 scram-sha-256",
      "host all all 0.0.0.0/0 reject",
      "host all all ::/0 reject",
    ]);
  });

  test("the Postgres command turns TLS on with the generated files and the generated pg_hba.conf", () => {
    const command = postgresCommand();
    expect(command[0]).toBe("postgres");
    const settings = command.flatMap((arg, index) => (command[index - 1] === "-c" ? [arg] : []));
    expect(settings).toEqual([
      `hba_file=${POSTGRES_HBA_PATH}`,
      "ssl=on",
      `ssl_cert_file=${POSTGRES_CONFIG_DIR}/server.crt`,
      `ssl_key_file=${POSTGRES_CONFIG_DIR}/server.key`,
    ]);
  });

  test("services find the CA where the Helm chart mounts it", () => {
    expect(SERVICE_CA_PATH).toBe("/etc/trustiot/postgres-ca/ca.crt");
    expect(platformEnv("trustiot").TRUSTIOT_POSTGRES_CA_FILE).toBe(SERVICE_CA_PATH);
  });

  test("Mosquitto refuses anonymous clients and reads its logins from files", () => {
    expect(MOSQUITTO_CONFIG).toContain("allow_anonymous false");
    expect(MOSQUITTO_CONFIG).toContain("password_file");
    expect(MOSQUITTO_CONFIG).toContain("acl_file");
    expect(MOSQUITTO_CONFIG).not.toMatch(/listener\s+\d+\s+0\.0\.0\.0\s*\n.*allow_anonymous true/s);
  });

  test("the ACL names one user", () => {
    expect(mosquittoAcl("dev")).toBe("user dev\ntopic readwrite #\ntopic read $SYS/#\n");
  });
});

const example: ServiceDef = {
  name: "example-service",
  cargo: { package: "service-kit", bin: "example-service" },
  hostPort: 18080,
  env: { TRUSTIOT__GREETING: "hello" },
};

describe("services", () => {
  test("every declared service has a unique name and port, and follows the chart's conventions", () => {
    const names = SERVICES.map((service) => service.name);
    const ports = SERVICES.map((service) => service.hostPort);
    expect(new Set(names).size).toBe(names.length);
    expect(new Set(ports).size).toBe(ports.length);
    for (const service of SERVICES) {
      expect(service.hostPort).toBeGreaterThanOrEqual(1024);
      for (const key of Object.keys(service.env)) {
        // Service settings are TRUSTIOT__<SECTION>__<KEY>; TRUSTIOT_<NAME> is the chart's.
        expect(key, service.name).toMatch(/^TRUSTIOT__[A-Z0-9]+(__[A-Z0-9_]+)*$/);
      }
    }
  });

  test("port overrides are named after the service", () => {
    expect(portVariable("example-service")).toBe("TRUSTIOT_DEV_PORT_EXAMPLE_SERVICE");
    expect(resolveServicePorts([example], {})).toEqual({ "example-service": 18080 });
    expect(
      resolveServicePorts([example], { TRUSTIOT_DEV_PORT_EXAMPLE_SERVICE: "28080" }),
    ).toEqual({ "example-service": 28080 });
    expect(() => resolveServicePorts([example], { TRUSTIOT_DEV_PORT_EXAMPLE_SERVICE: "x" })).toThrow();
  });

  test("platform settings use the chart's names and carry no credentials", () => {
    const env = platformEnv("trustiot");
    for (const key of Object.keys(env)) expect(key).toMatch(/^TRUSTIOT_[A-Z0-9]+(_[A-Z0-9]+)*$/);
    expect(env.TRUSTIOT_POSTGRES_HOST).toBe("postgres");
    expect(JSON.stringify(env)).not.toMatch(/passw|secret|token|key/i);
  });

  test("services always verify the database server, as service-kit demands", () => {
    expect(platformEnv("trustiot").TRUSTIOT_POSTGRES_SSLMODE).toBe("verify-full");
    expect(JSON.stringify(platformEnv("trustiot"))).not.toContain("disable");
  });

  test("a database role is a plain identifier and each is unique", () => {
    const roles = SERVICES.flatMap((service) => (service.database ? [service.database.role] : []));
    expect(new Set(roles).size).toBe(roles.length);
    for (const role of roles) expect(role).toMatch(/^[a-z][a-z0-9_]*$/);
  });

  test("the container serves HTTP on the chart's port", () => {
    expect(SERVICE_PORT).toBe(8080);
    const args = runArgs("t", {
      component: example.name,
      image: "localhost/trustiot-dev-t-example-service:dev",
      memory: "256m",
      publish: [[18080, SERVICE_PORT]],
    });
    expect(args).toContain("127.0.0.1:18080:8080/tcp");
  });

  test("the Containerfile starts the binary on the pinned base", () => {
    const text = containerfile(IMAGES.serviceBase, example);
    expect(text).toContain(`FROM ${IMAGES.serviceBase}`);
    expect(text).toContain('ENTRYPOINT ["/usr/local/bin/example-service"]');
  });
});
