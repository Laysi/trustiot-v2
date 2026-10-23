//! What the migration Job applies: every service with a Postgres schema, the ClickHouse DDL, and
//! the per-tenant role hooks. The files are compiled into the binary, so the Job of a release
//! applies exactly the schema of that release.
//!
//! Adding a service's Postgres migrations: list every file of its `migrations/postgres/` in
//! order in the service's `src/schema.rs` (a [`service_kit::migrations::Schema`]), include that
//! file here with `#[path]` and list the service in [`PRODUCT`] from it (`tests/registry.rs`
//! fails when a file is missing, out of order, or listed twice). The service's readiness check
//! compiles the same file, so the Job applies exactly what the service expects
//! (Laysi/trustiot-v2#402). A new file is a new entry at the end; applied files never change.

/// One SQL file, compiled in. The same type the services' readiness check uses.
pub use service_kit::migrations::Migration;

/// The identity service's files (compiled into the service as well).
#[path = "../../../services/identity/src/schema.rs"]
mod identity_schema;

/// The assets service's files (compiled into the service as well).
#[path = "../../../services/assets/src/schema.rs"]
mod assets_schema;

/// The telemetry-query service's files (compiled into the service as well).
#[path = "../../../services/telemetry-query/src/schema.rs"]
mod telemetry_query_schema;

/// The certs service's files (compiled into the service as well).
#[path = "../../../services/certs/src/schema.rs"]
mod certs_schema;

/// The assistant service's files (compiled into the service as well).
#[path = "../../../services/assistant/src/schema.rs"]
mod assistant_schema;

/// The outbox tables (crates/outbox) in a service's schema.
#[derive(Clone, Copy, Debug, Default)]
pub struct Outbox {
    /// The service writes events: `outbox::install_producer` on every run, then
    /// crates/service-kit/sql/outbox-event.sql and outbox-aggregate.sql once each, recorded under
    /// [`service_kit::migrations::outbox_component`]. Needs a platform role.
    pub producer: bool,
    /// The producers this service reads events from (their names): `outbox::install_consumer`
    /// on every run, and its platform role becomes a reader of each producer's `outbox_event`.
    /// Each one must be an outbox producer of the registry, before or after this service.
    /// Needs a platform role.
    pub consumes: &'static [&'static str],
}

/// A service with its own Postgres schema (decision 4).
#[derive(Clone, Copy, Debug)]
pub struct Service {
    /// The chart's service key, for example `identity` or `telemetry-query`.
    pub name: &'static str,
    /// Whether the service has a platform role (`<schema>_platform`, `PlatformDb`).
    pub platform_role: bool,
    /// The outbox tables it produces or consumes.
    pub outbox: Outbox,
    /// Its `migrations/postgres/` files in apply order.
    pub migrations: &'static [Migration],
}

impl Service {
    /// The schema and the prefix of the role names: the key with `-` replaced by `_`.
    #[must_use]
    pub fn schema(&self) -> String {
        self.name.replace('-', "_")
    }

    /// The migration (owner) role: owns the schema and everything the migrations create.
    #[must_use]
    pub fn owner_role(&self) -> String {
        format!("{}_owner", self.schema())
    }

    /// The service role (`TenantDb`, `BackgroundTenantDb`).
    #[must_use]
    pub fn app_role(&self) -> String {
        format!("{}_app", self.schema())
    }

    /// The platform role (`PlatformDb`), when the service has one.
    #[must_use]
    pub fn platform_role(&self) -> Option<String> {
        self.platform_role
            .then(|| format!("{}_platform", self.schema()))
    }

    /// What the service's readiness check expects (service-kit's `migrations`): the files the
    /// Job records in the service's schema.
    #[must_use]
    pub fn expected(&self) -> service_kit::migrations::Schema {
        service_kit::migrations::Schema {
            service: self.name,
            outbox_producer: self.outbox.producer,
            migrations: self.migrations,
        }
    }

    /// The login roles: the service role, then the platform role.
    #[must_use]
    pub fn login_roles(&self) -> Vec<String> {
        let mut roles = vec![self.app_role()];
        roles.extend(self.platform_role());
        roles
    }
}

/// The hook that rebuilds per-tenant database roles from a tenant table on every run (checklist
/// M8: roles are cluster-wide objects, outside a database backup, so a restored database gets
/// them back from the next run). The AGE roles of decision 21 plug in here in MS4
/// (Laysi/trustiot-v2#176); none is registered before that.
#[derive(Clone, Copy, Debug)]
pub struct TenantRoleHook {
    /// A name for the logs.
    pub name: &'static str,
    /// The service whose owner role reads the tenant table and runs the template (it owns the
    /// table, so row-level security does not hide rows from it).
    pub service: &'static str,
    /// A SELECT that returns one `uuid` column: the tenants that need a role. Static SQL.
    pub tenants: &'static str,
    /// The role is this prefix plus the tenant ID's 32 hex digits, for example
    /// `field_context_t0192...`; at most 63 bytes in all, so the prefix is at most 31.
    pub role_prefix: &'static str,
    /// Run for every tenant after its role exists (created NOLOGIN NOINHERIT when missing),
    /// with `example_tenant_role` replaced by the role and `example_tenant_id` by the tenant ID
    /// as a literal. Idempotent: it runs on every run, for every tenant.
    pub template: &'static str,
}

/// Everything one migration Job applies.
#[derive(Clone, Copy, Debug)]
pub struct Registry {
    /// Services in apply order: a consumer after the producers it reads.
    pub services: &'static [Service],
    /// `services/ingest/migrations/clickhouse/` in apply order.
    pub clickhouse: &'static [Migration],
    /// Per-tenant roles, rebuilt after every service's migrations.
    pub tenant_role_hooks: &'static [TenantRoleHook],
}

impl Registry {
    /// The service called `name`.
    #[must_use]
    pub fn service(&self, name: &str) -> Option<&Service> {
        self.services.iter().find(|service| service.name == name)
    }
}

/// crates/service-kit/sql/database.sql: run once per database before any service connects.
pub const DATABASE: Migration = service_kit::migrations::DATABASE;

/// The outbox policy template (crates/service-kit/sql/outbox-event.sql), applied once per
/// producer with its roles.
pub const OUTBOX_EVENT_TEMPLATE: Migration = service_kit::migrations::OUTBOX_EVENT;

/// The tenant column, row-level security and grants of the outbox's `outbox_aggregate`
/// (crates/service-kit/sql/outbox-aggregate.sql, Laysi/trustiot-v2#413), applied once per
/// producer with its roles, after [`OUTBOX_EVENT_TEMPLATE`]. A producer that ran the Job before
/// this template gets it on its next run, with every existing aggregate in the tenant of its
/// last event.
pub const OUTBOX_AGGREGATE_TEMPLATE: Migration = service_kit::migrations::OUTBOX_AGGREGATE;

/// The product: what the released migration Job applies.
pub const PRODUCT: Registry = Registry {
    services: &[
        // Writes `identity.tenant_created` (Laysi/trustiot-v2#65) with its service role, in the
        // new tenant's transaction, and keeps the event log (Laysi/trustiot-v2#130): it reads the
        // events of every producer in `CONSUMES` (its own, and assets' and certs', which come
        // later in this list) as its platform role, so the chart binds a password to
        // `identity_platform` as well ("trustiot.migrate.platformLogins" in
        // deploy/chart/templates/_migrate.tpl).
        Service {
            name: identity_schema::SCHEMA.service,
            platform_role: true,
            outbox: Outbox {
                producer: identity_schema::SCHEMA.outbox_producer,
                consumes: identity_schema::CONSUMES,
            },
            migrations: identity_schema::SCHEMA.migrations,
        },
        // Reads identity's events as its platform role, so the chart binds a password to
        // `assets_platform` as well ("trustiot.migrate.platformLogins" in
        // deploy/chart/templates/_migrate.tpl), and certs' (Laysi/trustiot-v2#101): every
        // `certs.gateway_registered` puts the gateway in the device pool. Writes
        // `assets.device_model_*` (Laysi/trustiot-v2#99) and `assets.gateway_bound`
        // (Laysi/trustiot-v2#101) with its service role. A producer needs a reader for the outbox
        // policy, and the gateway configuration that will consume these events
        // (Laysi/trustiot-v2#93) does not exist yet, so assets reads its own events for now: no
        // subscription does. When the real consumer is listed here, drop "assets" and the Job
        // revokes the read (it syncs the readers on every run).
        Service {
            name: assets_schema::SCHEMA.service,
            platform_role: true,
            outbox: Outbox {
                producer: assets_schema::SCHEMA.outbox_producer,
                consumes: &["identity", "certs", "assets"],
            },
            migrations: assets_schema::SCHEMA.migrations,
        },
        // Reads identity's events as its platform role, like assets, to create each new
        // tenant's ClickHouse account (Laysi/trustiot-v2#119).
        Service {
            name: telemetry_query_schema::SCHEMA.service,
            platform_role: true,
            outbox: Outbox {
                producer: telemetry_query_schema::SCHEMA.outbox_producer,
                consumes: &["identity"],
            },
            migrations: telemetry_query_schema::SCHEMA.migrations,
        },
        // The ledger of issued certificates and the CRLs (Laysi/trustiot-v2#78): no tenant data.
        // The factory registration (Laysi/trustiot-v2#79) writes the platform-wide
        // `certs.gateway_registered` (a gateway belongs to no tenant at the factory), which only
        // a producer's platform role may write, so the chart binds a password to
        // `certs_platform` as well ("trustiot.migrate.platformLogins" in
        // deploy/chart/templates/_migrate.tpl). identity's event log reads these events.
        Service {
            name: certs_schema::SCHEMA.service,
            platform_role: true,
            outbox: Outbox {
                producer: certs_schema::SCHEMA.outbox_producer,
                consumes: &[],
            },
            migrations: certs_schema::SCHEMA.migrations,
        },
        // The AI assistant's conversations and their append-only history
        // (Laysi/trustiot-v2#553): tenant data under row-level security, no events, and nothing
        // read across tenants, so no platform role. The chart lists `postgres` for the assistant
        // (Laysi/trustiot-v2#592), so the Job lets its `assistant_app` log in like every client's.
        Service {
            name: assistant_schema::SCHEMA.service,
            platform_role: false,
            outbox: Outbox {
                producer: assistant_schema::SCHEMA.outbox_producer,
                consumes: &[],
            },
            migrations: assistant_schema::SCHEMA.migrations,
        },
    ],
    clickhouse: crate::clickhouse_files::FILES,
    tenant_role_hooks: &[],
};
