// The login roles of the product that the local migration Job gives a password to
// (Laysi/trustiot-v2#407), named as tools/migrate and the chart name them.
//
// In a cluster a login role logs in only while a Secret is bound to it (the Job sets every other
// role NOLOGIN). The chart binds `<schema>_app` for every service of `PRODUCT`
// (tools/migrate/src/registry.rs) and `<schema>_platform` only for the services listed in
// "trustiot.migrate.platformLogins" (deploy/chart/templates/_migrate.tpl). The local environment
// binds the same roles, and `tests/product-logins.test.ts` reads both files and fails when this
// list and they disagree.

/**
 * The services of `PRODUCT`, in the registry's order: each has a `<schema>_app` login role.
 * Add a service here in the same change that adds it to `PRODUCT`.
 */
export const PRODUCT_SERVICES: readonly string[] = ["identity", "assets", "telemetry-query", "certs", "assistant", "ai-gateway", "ingest"];

/**
 * The services whose platform role logs in, as the chart binds them
 * ("trustiot.migrate.platformLogins"): the outbox consumers, which read their producers' events
 * as the platform role (identity among them since it keeps the event log, Laysi/trustiot-v2#130),
 * and certs, which writes the platform-wide factory registration event (Laysi/trustiot-v2#79).
 * A platform role not listed here stays NOLOGIN on purpose.
 */
export const PLATFORM_LOGIN_SERVICES: readonly string[] = ["assets", "certs", "identity", "ingest", "telemetry-query"];

export interface ProductLogin {
  /** The service's chart key, for example `telemetry-query`. */
  service: string;
  kind: "app" | "platform";
  /** The Postgres role, for example `telemetry_query_app`. */
  role: string;
  /** The Secret that holds its password: one per service, like the chart's `<release>-<service>-password`. */
  secret: string;
  /**
   * The key of the login in the state file (`credentials.services`), which also names the
   * variables `env` prints (`TRUSTIOT_DEV_<KEY>_DATABASE_{USERNAME,PASSWORD}`): the service name
   * for its own role, and `<service>-platform` for the platform role.
   */
  stateKey: string;
}

/** The schema of a service and the prefix of its role names: the key with `-` written `_`. */
export function schemaOf(service: string): string {
  return service.replaceAll("-", "_");
}

/** The password Secret of a service in the local Secret API. */
export function passwordSecretOf(service: string): string {
  return `dev-${service}-password`;
}

/**
 * Every login role the local Job binds a password to: each service's own role, then its platform
 * role when the chart binds one.
 */
export function productLogins(
  services: readonly string[] = PRODUCT_SERVICES,
  platformServices: readonly string[] = PLATFORM_LOGIN_SERVICES,
): ProductLogin[] {
  const logins: ProductLogin[] = [];
  for (const service of services) {
    const secret = passwordSecretOf(service);
    logins.push({ service, kind: "app", role: `${schemaOf(service)}_app`, secret, stateKey: service });
    if (platformServices.includes(service)) {
      logins.push({
        service,
        kind: "platform",
        role: `${schemaOf(service)}_platform`,
        secret,
        stateKey: `${service}-platform`,
      });
    }
  }
  return logins;
}
