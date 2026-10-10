// The declarative description of the dev environment: images, names, ports and how a container
// is started. Everything here is pure, so the rules that matter for safety (images pinned by
// digest, ports published on 127.0.0.1 only, no credentials in the argument list) are covered by
// unit tests.

/**
 * Container images, each pinned by digest of the multi-arch manifest list (amd64 and arm64), so
 * a moved tag cannot change what runs. The comment names the tag the digest was resolved from
 * on 2026-10-10. Update a digest together with the tag.
 */
export const IMAGES = {
  // apache/age:release_PG18_1.8.0, PostgreSQL 18 with Apache AGE (decisions 4 and 21).
  postgres:
    "docker.io/apache/age@sha256:47a0b054c3663a3e64a25fc0c5a982f1b58348019274b326a691cdf77f78eb58",
  // clickhouse/clickhouse-server:26.8 (decision 14).
  clickhouse:
    "docker.io/clickhouse/clickhouse-server@sha256:110d3d1f8e1ed7e451a6b5d85a16761e948ad43bc2d9ddf10b155964a8b2b40c",
  // eclipse-mosquitto:2.1.2-alpine (decision 6; Docker Hub has no plain 2.1.2 tag).
  mosquitto:
    "docker.io/library/eclipse-mosquitto@sha256:38c0da4f2ef84284d47b3b3eeea1cb3bdeabe81ee10caf0cd5c5ff61ee3ea408",
  // gcr.io/distroless/cc-debian13:nonroot, the base of the service images. Same digest as the
  // service-kit container test, which runs the service as PID 1 on it.
  serviceBase:
    "gcr.io/distroless/cc-debian13@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2",
} as const;

export const DIGEST_PATTERN = /@sha256:[0-9a-f]{64}$/;

/** Loopback only: a published port is never reachable from another machine. */
export const BIND_ADDRESS = "127.0.0.1";

export const LABEL_KEY = "io.laysi.trustiot-dev.instance";
export const LABEL_COMPONENT = "io.laysi.trustiot-dev.component";

export type InfraComponent = "postgres" | "clickhouse" | "mosquitto";

export const DEFAULT_PORTS = { postgres: 15432, clickhouse: 18123, mqtt: 11883 } as const;
/** Ports inside the containers; the services reach the dependencies at `<name>:<port>`. */
export const CONTAINER_PORTS = { postgres: 5432, clickhouse: 8123, mqtt: 1883 } as const;
/** The HTTP port every service listens on (deploy/chart/README.md). */
export const SERVICE_PORT = 8080;

export interface Names {
  prefix: string;
  label: string;
  network: string;
  container(component: string): string;
  volume(name: string): string;
  image(service: string): string;
}

export function namesFor(instance: string): Names {
  const prefix = `trustiot-dev-${instance}`;
  return {
    prefix,
    label: `${LABEL_KEY}=${instance}`,
    network: prefix,
    container: (component) => `${prefix}-${component}`,
    volume: (name) => `${prefix}-${name}`,
    image: (service) => `localhost/${prefix}-${service}:dev`,
  };
}

export interface ContainerSpec {
  component: string;
  image: string;
  memory: string;
  /** `[hostPort, containerPort]`; always published on {@link BIND_ADDRESS}. */
  publish: [number, number][];
  /** A file of `KEY=value` lines (secrets); passed with `--env-file`. */
  envFile?: string;
  /** Non-secret variables, passed with `-e`. */
  env?: Record<string, string>;
  /** `volume-name:/path[:options]` */
  volumes?: string[];
  command?: string[];
}

/** The `podman run` arguments for one container, detached, on the instance's network. */
export function runArgs(instance: string, spec: ContainerSpec): string[] {
  const names = namesFor(instance);
  if (!DIGEST_PATTERN.test(spec.image) && !spec.image.startsWith("localhost/")) {
    throw new Error(`image of ${spec.component} is not pinned by digest: ${spec.image}`);
  }
  const args = [
    "run",
    "--detach",
    "--name",
    names.container(spec.component),
    "--network",
    names.network,
    "--network-alias",
    spec.component,
    "--label",
    names.label,
    "--label",
    `${LABEL_COMPONENT}=${spec.component}`,
    "--memory",
    spec.memory,
    "--security-opt",
    "no-new-privileges",
    "--restart",
    "no",
  ];
  for (const [hostPort, containerPort] of spec.publish) {
    args.push("--publish", `${BIND_ADDRESS}:${hostPort}:${containerPort}/tcp`);
  }
  if (spec.envFile) args.push("--env-file", spec.envFile);
  for (const [key, value] of Object.entries(spec.env ?? {})) args.push("--env", `${key}=${value}`);
  for (const volume of spec.volumes ?? []) args.push("--volume", volume);
  args.push(spec.image, ...(spec.command ?? []));
  return args;
}

export interface Ports {
  postgres: number;
  clickhouse: number;
  mqtt: number;
}

/** Host ports, from the defaults or `TRUSTIOT_DEV_{POSTGRES,CLICKHOUSE,MQTT}_PORT`. */
export function resolvePorts(env: Record<string, string | undefined>): Ports {
  const read = (name: string, fallback: number): number => {
    const raw = env[name];
    if (raw === undefined || raw === "") return fallback;
    const port = Number(raw);
    if (!/^[0-9]+$/.test(raw) || port < 1024 || port > 65535) {
      throw new Error(`${name} must be a port number between 1024 and 65535`);
    }
    return port;
  };
  const ports = {
    postgres: read("TRUSTIOT_DEV_POSTGRES_PORT", DEFAULT_PORTS.postgres),
    clickhouse: read("TRUSTIOT_DEV_CLICKHOUSE_PORT", DEFAULT_PORTS.clickhouse),
    mqtt: read("TRUSTIOT_DEV_MQTT_PORT", DEFAULT_PORTS.mqtt),
  };
  if (new Set(Object.values(ports)).size !== 3) {
    throw new Error("the Postgres, ClickHouse and MQTT host ports must all differ");
  }
  return ports;
}

/** Throws when two components would publish the same host port. */
export function assertDistinctPorts(ports: Record<string, number>): void {
  const owners = new Map<number, string>();
  for (const [name, port] of Object.entries(ports)) {
    const other = owners.get(port);
    if (other !== undefined) {
      throw new Error(`${other} and ${name} would both publish host port ${port}; set different ports`);
    }
    owners.set(port, name);
  }
}

/**
 * Postgres client authentication. The image's default `pg_hba.conf` trusts connections from
 * 127.0.0.1 and ::1 inside the container. Rootless port forwarding does not present host
 * connections as loopback (checked on 2026-10-10), but that is a property of the forwarder, not
 * of the configuration. This file trusts only the unix socket inside the container (used by
 * `podman exec`) and demands SCRAM from every address, so no forwarder can change the outcome.
 */
export const POSTGRES_HBA = `# Generated by scripts/dev.ts for local development.
local all all trust
host all all 0.0.0.0/0 scram-sha-256
host all all ::/0 scram-sha-256
`;

/** Where the generated Postgres configuration is mounted in the container. */
export const POSTGRES_HBA_PATH = "/etc/trustiot/pg_hba.conf";

/** The Mosquitto configuration: authentication required, one ACL file, persistence on. */
export const MOSQUITTO_CONFIG = `# Generated by scripts/dev.ts for local development.
listener ${CONTAINER_PORTS.mqtt}
allow_anonymous false
password_file /mosquitto/config/passwd
acl_file /mosquitto/config/acl
persistence true
persistence_location /mosquitto/data/
log_dest stdout
`;

export function mosquittoAcl(user: string): string {
  return `user ${user}\ntopic readwrite #\ntopic read $SYS/#\n`;
}
