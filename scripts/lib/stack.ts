// The declarative description of the dev environment: images, names, ports and how a container
// is started. Everything here is pure, so the rules that matter for safety (images pinned by
// digest, ports published on 127.0.0.1 only, no credentials in the argument list) are covered by
// unit tests.

/**
 * Container images, each pinned by digest of the multi-arch manifest list (amd64 and arm64), so
 * a moved tag cannot change what runs. Each reference names the tag the digest was resolved from
 * (first on 2026-10-10) as name:tag@digest: Renovate reads that form and updates every copy of
 * the same pin in one pull request (renovate.json5).
 */
export const IMAGES = {
  // PostgreSQL 18 with Apache AGE (decisions 4 and 21).
  postgres:
    "docker.io/apache/age:release_PG18_1.8.0@sha256:47a0b054c3663a3e64a25fc0c5a982f1b58348019274b326a691cdf77f78eb58",
  // ClickHouse 26.8 LTS (decision 14). deploy/chart/values.yaml and services/ingest/tests pin the
  // same image; the chart tests fail when the three differ.
  clickhouse:
    "docker.io/clickhouse/clickhouse-server:26.8@sha256:110d3d1f8e1ed7e451a6b5d85a16761e948ad43bc2d9ddf10b155964a8b2b40c",
  // Mosquitto 2.1.2 (decision 6; Docker Hub has no plain 2.1.2 tag). crates/protocol/tests pins
  // the same image.
  mosquitto:
    "docker.io/library/eclipse-mosquitto:2.1.2-alpine@sha256:860aeb5d63ce509508f6dcfb2bc8a1c1dd2bc60555b562b45ce90e52d8dabb0f",
  // The base of the service images. Same image as the service-kit container tests, which run the
  // service as PID 1 on it, and the chart's e2e example service.
  serviceBase:
    "gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2",
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

/**
 * The platform of the machine the dev environment runs on, as podman spells it (`linux/amd64`,
 * `linux/arm64`): the containers run natively, never under emulation. podman matches a name
 * to whatever copy the local store holds under it, whatever its architecture, so every image
 * the environment builds from or starts from a registry names this platform
 * (Laysi/trustiot-v2#406). `arch` is Node's `process.arch`.
 */
export function nativePlatform(arch: string = process.arch): string {
  switch (arch) {
    case "x64":
      return "linux/amd64";
    case "arm64":
      return "linux/arm64";
    default:
      throw new Error(`the dev environment runs on x86_64 and aarch64 machines; this one reports "${arch}"`);
  }
}

/**
 * The message for an image whose architecture is not the machine's: what happened and what to
 * do about it, because the symptom (`exec container process ... No such file or directory`)
 * names neither.
 */
export function platformMismatchMessage(image: string, actual: string, expected: string): string {
  return (
    `the image ${image} is ${actual}, but this machine runs ${expected}, so its container cannot start ` +
    `(podman reports "exec container process ... No such file or directory"). The podman store probably ` +
    `holds a copy of the base image for another architecture, for example from a local arm64 release build. ` +
    `Remove that copy (podman rmi <base image>) and run the command again.`
  );
}

/** The `podman run` arguments for one container, detached, on the instance's network. */
export function runArgs(instance: string, spec: ContainerSpec, platform: string = nativePlatform()): string[] {
  const names = namesFor(instance);
  const pinned = DIGEST_PATTERN.test(spec.image);
  if (!pinned && !spec.image.startsWith("localhost/")) {
    throw new Error(`image of ${spec.component} is not pinned by digest: ${spec.image}`);
  }
  const args = [
    "run",
    "--detach",
    // A registry image is selected by platform, so a cached copy for another architecture is
    // replaced by a pull instead of being started. Local builds already have the right one.
    ...(pinned ? ["--platform", platform] : []),
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
 * It also demands TLS: a connection from an address that does not use TLS is rejected, so no
 * client can talk to the database in the clear, as services never do (verify-full).
 */
export const POSTGRES_HBA = `# Generated by scripts/dev.ts for local development.
local all all trust
hostssl all all 0.0.0.0/0 scram-sha-256
hostssl all all ::/0 scram-sha-256
host all all 0.0.0.0/0 reject
host all all ::/0 reject
`;

/** Where the generated Postgres configuration and TLS files are mounted in the container. */
export const POSTGRES_CONFIG_DIR = "/etc/trustiot";
export const POSTGRES_HBA_PATH = `${POSTGRES_CONFIG_DIR}/pg_hba.conf`;
export const POSTGRES_CERT_PATH = `${POSTGRES_CONFIG_DIR}/server.crt`;
export const POSTGRES_KEY_PATH = `${POSTGRES_CONFIG_DIR}/server.key`;
/** The dev CA certificate next to them, for the `psql` that the health checks run in the container. */
export const POSTGRES_CA_PATH = `${POSTGRES_CONFIG_DIR}/ca.crt`;

/**
 * Where the services find the CA that signed the Postgres certificate: the path the Helm chart
 * mounts it at (`postgres.external.caSecret`, deploy/chart/README.md), so a service reads the
 * same `TRUSTIOT_POSTGRES_CA_FILE` here and in a cluster.
 */
export const SERVICE_CA_DIR = "/etc/trustiot/postgres-ca";
export const SERVICE_CA_PATH = `${SERVICE_CA_DIR}/ca.crt`;

/** The Postgres server command: the generated `pg_hba.conf` and TLS on. */
export function postgresCommand(): string[] {
  return [
    "postgres",
    "-c",
    `hba_file=${POSTGRES_HBA_PATH}`,
    "-c",
    "ssl=on",
    "-c",
    `ssl_cert_file=${POSTGRES_CERT_PATH}`,
    "-c",
    `ssl_key_file=${POSTGRES_KEY_PATH}`,
  ];
}

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
