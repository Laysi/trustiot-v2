// The Rust services that run in the dev environment. Adding a service is one entry here: the
// start, status and purge commands build it, run it and check it from this list alone.
//
// A service runs the way it runs in the Helm chart (deploy/chart/README.md): one container,
// HTTP on port 8080, `GET /healthz` for liveness and `GET /readyz` for readiness, platform
// settings in `TRUSTIOT_<NAME>` variables and service settings in `TRUSTIOT__<SECTION>__<KEY>`.
// Moving the environment from podman to k3d later changes how the container is started, not
// what it is given.
import { CONTAINER_PORTS, SERVICE_CA_PATH, type Names } from "./stack.ts";

export interface ServiceDef {
  /** The chart's service key (the `services` map in deploy/chart/values.yaml). */
  name: string;
  /** The Cargo package and binary that is built into the image. */
  cargo: { package: string; bin: string };
  /** Default host port, published on 127.0.0.1 only; override with `TRUSTIOT_DEV_PORT_<NAME>`. */
  hostPort: number;
  /** Service-specific settings, added to the platform settings below. */
  env: Record<string, string>;
  /**
   * The service's database role. `up` creates it (a plain login role: no superuser, no
   * BYPASSRLS, owns nothing) with a fresh password and hands the pair to the service as
   * `TRUSTIOT__DATABASE__USERNAME` and `TRUSTIOT__DATABASE__PASSWORD`. Omit for a service
   * without a database.
   */
  database?: { role: string };
}

export const SERVICES: readonly ServiceDef[] = [
  {
    // The service-kit demonstration service; the first real service replaces or joins it.
    name: "example-service",
    cargo: { package: "service-kit", bin: "example-service" },
    hostPort: 18080,
    env: { TRUSTIOT__GREETING: "hello from the dev environment" },
    // With a database the example registers a `postgres` readiness check, so `/readyz` is only
    // 200 while the service has a verify-full connection to Postgres.
    database: { role: "example_service" },
  },
];

/** Variable name of a service's host-port override, for example `TRUSTIOT_DEV_PORT_EXAMPLE_SERVICE`. */
export function portVariable(service: string): string {
  return `TRUSTIOT_DEV_PORT_${service.toUpperCase().replaceAll("-", "_")}`;
}

export function resolveServicePorts(
  services: readonly ServiceDef[],
  env: Record<string, string | undefined>,
): Record<string, number> {
  const ports: Record<string, number> = {};
  for (const service of services) {
    const name = portVariable(service.name);
    const raw = env[name];
    if (raw === undefined || raw === "") {
      ports[service.name] = service.hostPort;
      continue;
    }
    const port = Number(raw);
    if (!/^[0-9]+$/.test(raw) || port < 1024 || port > 65535) {
      throw new Error(`${name} must be a port number between 1024 and 65535`);
    }
    ports[service.name] = port;
  }
  return ports;
}

/**
 * The platform settings every service receives, named as in the chart's `platform` ConfigMap.
 * Dependencies are reached by their network alias. The local Postgres serves TLS with a
 * certificate for that alias, signed by the CA that `up` generates and mounts at
 * `TRUSTIOT_POSTGRES_CA_FILE`, so services connect with `verify-full` exactly as in a cluster.
 * A service's database role and password are given separately (`database` in {@link ServiceDef});
 * the production ones arrive with the Postgres and migration issues (Laysi/trustiot-v2#58,
 * Laysi/trustiot-v2#60).
 */
export function platformEnv(database: string): Record<string, string> {
  return {
    TRUSTIOT_DEPLOYMENT_MODE: "onprem",
    TRUSTIOT_POSTGRES_HOST: "postgres",
    TRUSTIOT_POSTGRES_PORT: String(CONTAINER_PORTS.postgres),
    TRUSTIOT_POSTGRES_DATABASE: database,
    TRUSTIOT_POSTGRES_SSLMODE: "verify-full",
    TRUSTIOT_POSTGRES_CA_FILE: SERVICE_CA_PATH,
    TRUSTIOT_CLICKHOUSE_URL: `http://clickhouse:${CONTAINER_PORTS.clickhouse}`,
  };
}

/** The Containerfile of a service image: the built binary on the pinned distroless base. */
export function containerfile(base: string, service: ServiceDef): string {
  return [
    `FROM ${base}`,
    `COPY ${service.cargo.bin} /usr/local/bin/${service.name}`,
    `ENTRYPOINT ["/usr/local/bin/${service.name}"]`,
    "",
  ].join("\n");
}

export function imageLabelArgs(names: Names): string[] {
  return ["--label", names.label];
}
