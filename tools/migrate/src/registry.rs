//! What the migration Job applies: every service with a Postgres schema, the ClickHouse DDL, and
//! the per-tenant role hooks. The files are compiled into the binary, so the Job of a release
//! applies exactly the schema of that release.
//!
//! Adding a service's Postgres migrations: list the service in [`PRODUCT`] with every file of
//! its `migrations/postgres/` in order (`tests/registry.rs` fails when a file is missing, out of
//! order, or listed twice). A new file is a new entry at the end; applied files never change.

/// One SQL file, compiled in.
#[derive(Clone, Copy, Debug)]
pub struct Migration {
    /// File name, for example `0001_identity.sql`; files apply in this order.
    pub name: &'static str,
    /// The file's text.
    pub sql: &'static str,
}

/// The outbox tables (crates/outbox) in a service's schema.
#[derive(Clone, Copy, Debug, Default)]
pub struct Outbox {
    /// The service writes events: `outbox::install_producer` on every run, then
    /// crates/service-kit/sql/outbox-event.sql once. Needs a platform role.
    pub producer: bool,
    /// The producers this service reads events from (their names): `outbox::install_consumer`
    /// on every run, and its platform role becomes a reader of each producer's `outbox_event`.
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
pub const DATABASE: Migration = Migration {
    name: "database.sql",
    sql: include_str!("../../../crates/service-kit/sql/database.sql"),
};

/// The outbox policy template (crates/service-kit/sql/outbox-event.sql), applied once per
/// producer with its roles.
pub const OUTBOX_EVENT_TEMPLATE: Migration = Migration {
    name: "outbox-event.sql",
    sql: include_str!("../../../crates/service-kit/sql/outbox-event.sql"),
};

/// The product: what the released migration Job applies.
pub const PRODUCT: Registry = Registry {
    services: &[
        // Writes `identity.tenant_created` (Laysi/trustiot-v2#65) with its service role, in the
        // new tenant's transaction. Its platform role exists because the outbox policy names one
        // (platform-wide events), but the chart binds no Secret to it, so it stays NOLOGIN.
        Service {
            name: "identity",
            platform_role: true,
            outbox: Outbox {
                producer: true,
                consumes: &[],
            },
            migrations: &[
                Migration {
                    name: "0001_identity.sql",
                    sql: include_str!(
                        "../../../services/identity/migrations/postgres/0001_identity.sql"
                    ),
                },
                Migration {
                    name: "0002_tenants.sql",
                    sql: include_str!(
                        "../../../services/identity/migrations/postgres/0002_tenants.sql"
                    ),
                },
            ],
        },
        // Reads identity's events as its platform role, so the chart binds a password to
        // `assets_platform` as well ("trustiot.migrate.platformLogins" in
        // deploy/chart/templates/_migrate.tpl).
        Service {
            name: "assets",
            platform_role: true,
            outbox: Outbox {
                producer: false,
                consumes: &["identity"],
            },
            migrations: &[Migration {
                name: "0001_assets.sql",
                sql: include_str!("../../../services/assets/migrations/postgres/0001_assets.sql"),
            }],
        },
    ],
    clickhouse: crate::clickhouse_files::FILES,
    tenant_role_hooks: &[],
};
