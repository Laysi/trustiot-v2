// Generated from api/openapi.json by frontend/tools/api-client.ts. Do not edit by hand:
// run `bun run --cwd frontend api:generate` after `cargo run -p openapi`.

export interface paths {
    "/api/assets/v1/device-models": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** The tenant's device models. */
        get: operations["listDeviceModels"];
        put?: never;
        /** Creates a device model; `assets.device_model_created` is written in the same transaction. */
        post: operations["createDeviceModel"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assets/v1/device-models/import": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /** Creates a device model from an exported document, under the same rules as a create. */
        post: operations["importDeviceModel"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assets/v1/device-models/{model_id}": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** One device model with its data points. */
        get: operations["getDeviceModel"];
        /**
         * Replaces the data points of a device model; `assets.device_model_updated` is written in the
         *     same transaction.
         */
        put: operations["updateDeviceModel"];
        post?: never;
        /**
         * Deletes a device model that no device uses; `assets.device_model_deleted` is written in the
         *     same transaction.
         */
        delete: operations["deleteDeviceModel"];
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assets/v1/device-models/{model_id}/export": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** One device model as a document to download, which the import accepts. */
        get: operations["exportDeviceModel"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assets/v1/devices": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** The tenant's devices. */
        get: operations["listDevices"];
        put?: never;
        /**
         * Creates a device on a gateway bound to the caller's tenant; `assets.device_created` is
         *     written in the same transaction.
         */
        post: operations["createDevice"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assets/v1/devices/{device_id}": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** One device. */
        get: operations["getDevice"];
        /**
         * Replaces a device's connection settings and poll interval; `assets.device_updated` is
         *     written in the same transaction.
         */
        put: operations["updateDevice"];
        post?: never;
        /** Deletes a device; `assets.device_deleted` is written in the same transaction. */
        delete: operations["deleteDevice"];
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assets/v1/platform/gateways": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** The device pool with each gateway's current binding. Platform administrators only. */
        get: operations["listPoolGateways"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assets/v1/platform/gateways/{hardware_id}": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** One gateway of the device pool with its current binding. Platform administrators only. */
        get: operations["getPoolGateway"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assets/v1/platform/gateways/{hardware_id}/binding": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        /** Binds a gateway of the device pool to a tenant; idempotent. Platform administrators only. */
        put: operations["bindGateway"];
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assets/v1/tenant": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** The caller's tenant, once this service has processed its `identity.tenant_created`. */
        get: operations["getAssetsTenant"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assistant/v1/conversations/{conversation_id}/dashboards": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** A conversation's drafts, for its owner. */
        get: operations["listConversationDashboards"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assistant/v1/dashboards": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** The tenant's saved dashboards. */
        get: operations["listDashboards"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assistant/v1/dashboards/{dashboard_id}": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /**
         * One dashboard with one version (the latest without `version`) and what the caller may do
         *     with it.
         */
        get: operations["getDashboard"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assistant/v1/dashboards/{dashboard_id}/archive": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        /**
         * Archives a saved dashboard: it leaves the tenant's list (unless archived ones are asked for)
         *     and takes no new version until it is unarchived. Archiving it again keeps the first time.
         */
        put: operations["archiveDashboard"];
        post?: never;
        /** Unarchives a saved dashboard. */
        delete: operations["unarchiveDashboard"];
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assistant/v1/dashboards/{dashboard_id}/dataset-runs": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Runs one dataset of one version as the caller and returns its table. Nothing is kept: every
         *     run reads again.
         */
        post: operations["runDashboardDataset"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assistant/v1/dashboards/{dashboard_id}/publish": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Publishes a version of a draft into the tenant's saved dashboards: the first time as a new
         *     saved dashboard, later as the next version of the draft's saved dashboard, when that one's
         *     latest version is the one the published version is based on.
         */
        post: operations["publishDashboard"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assistant/v1/dashboards/{dashboard_id}/restore": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /** Adds a copy of an older version as the dashboard's next version. */
        post: operations["restoreDashboard"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/assistant/v1/dashboards/{dashboard_id}/versions": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** A dashboard's versions, the newest first, without their spec and page. */
        get: operations["listDashboardVersions"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/certs/v1/gateway-registrations": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /** Registers a gateway at the factory and returns its initial certificate. */
        post: operations["registerGateway"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/certs/v1/workstation-certificates": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Issues a workstation certificate for the public key of a CSR. The CA decides everything
         *     else; the private key never reaches the service.
         */
        post: operations["issueWorkstationCertificate"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/certs/v1/workstation-certificates/{serial}/revoke": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Revokes a workstation certificate. Its issuing CA's next scheduled CRL lists it; the
         *     registration API (Laysi/trustiot-v2#79) reads the ledger, so it refuses the certificate at
         *     once.
         */
        post: operations["revokeWorkstationCertificate"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/identity/v1/event-log": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** The caller's tenant's operation log. Administrators of the tenant only. */
        get: operations["getTenantEventLog"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/identity/v1/platform/event-log": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** The platform-wide operation log, for the back office. Platform administrators only. */
        get: operations["getPlatformEventLog"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/identity/v1/platform/tenants/{tenant_id}/event-log": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** One tenant's operation log, for the back office. Platform administrators only. */
        get: operations["getPlatformTenantEventLog"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/identity/v1/platform/tenants/{tenant_id}/members": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        /**
         * Sets a user's role in a tenant, creating the membership when the user has none there.
         *     Platform administrators only, in their own request.
         */
        put: operations["setPlatformTenantMemberRole"];
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/identity/v1/preferences": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** Reads the user's own preferences. */
        get: operations["getPreferences"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        /** Changes the user's own preferences: the fields given, each from its allow list. */
        patch: operations["updatePreferences"];
        trace?: never;
    };
    "/api/identity/v1/sessions": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        post?: never;
        /** Logs out on every device: revokes every session of the user, this one included. */
        delete: operations["logOutEverywhere"];
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/identity/v1/sessions/current": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /**
         * Reads the current session: the user, the tenant it acts in with the user's role there, and
         *     the user's tenants.
         */
        get: operations["getCurrentSession"];
        put?: never;
        post?: never;
        /** Logs out: revokes the session of the request's cookie. */
        delete: operations["logOut"];
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/identity/v1/sessions/current/tenant": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        /**
         * Switches the session to another of the user's tenants; its next request acts there with the
         *     user's role in it.
         */
        put: operations["switchTenant"];
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/identity/v1/tenants": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /** Creates a tenant. Platform administrators only. */
        post: operations["createTenant"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/telemetry/v1/devices/{device_id}/latest": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** The latest value of each data point of a device, from the readings of the raw period. */
        get: operations["getTelemetryLatest"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/telemetry/v1/devices/{device_id}/series": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /**
         * A data point's series over a time range: raw readings, or buckets of an interval that
         *     follow the time zone `tz`, within the queryable period.
         */
        get: operations["getTelemetrySeries"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/telemetry/v1/statistics": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * The statistics of each series over a range, in buckets of the requested time zone, from the
         *     one-minute or one-hour aggregates.
         */
        post: operations["queryTelemetryStatistics"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/api/telemetry/v1/usage": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * The usage of each cumulative series over a range, in buckets of the requested time zone, from
         *     the one-minute or one-hour aggregates, after a check that the series behaves like a counter.
         */
        post: operations["queryTelemetryUsage"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/auth/login": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /** Logs in with an email code, in two steps (see the module documentation). */
        post: operations["logIn"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
}
export type webhooks = Record<string, never>;
export interface components {
    schemas: {
        /** @description The caller's tenant as this service knows it. */
        AssetsTenant: {
            /**
             * Format: date-time
             * @description When this service learned of the tenant, by the database clock.
             */
            created_at: string;
            /** @description The tenant's ID. */
            tenant_id: components["schemas"]["TenantId"];
        };
        /** @description The body of a binding. */
        BindGateway: {
            /** @description The tenant to bind the gateway to. */
            tenant_id: components["schemas"]["TenantId"];
        };
        /**
         * @description A bucket length of the semantic API: whole local minutes, hours, days, Monday weeks and
         *     months of the requested time zone, or the whole range.
         * @enum {string}
         */
        BucketInterval: "1m" | "5m" | "15m" | "1h" | "1d" | "1w" | "1mo" | "total";
        /** @description The answer to `send_code`, the same whether or not a user has the address. */
        CodeSent: {
            /**
             * Format: int32
             * @description Digits in the code.
             */
            code_length: number;
            /**
             * Format: int64
             * @description How long the code is valid, in seconds.
             */
            expires_in_secs: number;
            /**
             * Format: int64
             * @description How long to wait before asking for another code, in seconds.
             */
            resend_after_secs: number;
        };
        /** @description The body of `POST /api/assets/v1/devices`. Unknown fields are refused. */
        CreateDevice: {
            /** @description The gateway that polls it: one currently bound to the caller's tenant. */
            hardware_id: string;
            /** @description See [`Device::host`]. */
            host: string;
            /** @description One of the caller's tenant's device models. */
            model_id: components["schemas"]["DeviceModelId"];
            /** @description The new device's name, unique in the tenant (see [`Device::name`]). */
            name: string;
            /**
             * Format: int32
             * @description See [`Device::poll_interval_ms`]; 60000 when absent.
             */
            poll_interval_ms?: number | null;
            /**
             * Format: int32
             * @description See [`Device::port`].
             */
            port: number;
            /**
             * @description `modbus_tcp`: Modbus TCP to the device. `rtu_over_tcp`: RTU frames over TCP to an RS485 converter; the devices behind one converter (the same host and port on one gateway) share its connection, so they all use this transport and each has its own unit ID.
             * @enum {string}
             */
            transport: "modbus_tcp" | "rtu_over_tcp";
            /**
             * Format: int32
             * @description See [`Device::unit_id`]; 1 when absent.
             */
            unit_id?: number | null;
        };
        /** @description The body of `POST /api/assets/v1/device-models`. Unknown fields are refused. */
        CreateDeviceModel: {
            /** @description The new model's name, unique in the tenant (see [`DeviceModel::name`]). */
            name: string;
            /** @description The data points, at most 1000, in order; may be empty. */
            points: components["schemas"]["DeviceModelPoint"][];
        };
        /** @description The body of `POST /api/identity/v1/tenants`. Unknown fields are ignored. */
        CreateTenant: {
            /**
             * @description What people call the tenant: 1 to 100 characters after trimming, without control or
             *     format characters or line and paragraph separators.
             */
            name: string;
        };
        /** @description The cumulative check over the window read around the range. */
        CumulativeCheck: {
            /** Format: int64 */
            comparisons: number;
            /**
             * Format: int64
             * @description How often the value fell, rose, and was compared with the one before.
             */
            drops: number;
            /**
             * Format: date-time
             * @description The window the check read.
             */
            from: string;
            /** Format: int64 */
            rises: number;
            /** Format: date-time */
            to: string;
            verdict: components["schemas"]["Verdict"];
        };
        /** @description The session of the request. */
        CurrentSession: {
            active_tenant: components["schemas"]["TenantMembership"] | null;
            /** @description The address the user logs in with; `null` for a user who cannot log in by email. */
            email: string | null;
            /** @description Whether the user is a platform administrator (A2), independent of any tenant. */
            platform_admin: boolean;
            /**
             * @description Every tenant the user belongs to, by name; switch with
             *     `PUT /api/identity/v1/sessions/current/tenant`.
             */
            tenants: components["schemas"]["TenantMembership"][];
            /** @description The signed-in user. */
            user_id: components["schemas"]["UserId"];
        };
        /** @description Whether a saved dashboard is archived, after `PUT` or `DELETE …/archive`. */
        DashboardArchiveState: {
            /** Format: date-time */
            archived_at?: string | null;
            dashboard_id: components["schemas"]["DashboardId"];
        };
        /**
         * @description What a column holds.
         * @enum {string}
         */
        DashboardColumnType: "number" | "text" | "time" | "bool";
        /**
         * @description A named number computed from the datasets' results with a formula
         *     ([`super::formula`]).
         */
        DashboardComputation: {
            /** @description At most `formula::MAX_FORMULA_BYTES` bytes. */
            formula: string;
            name: string;
            title: string;
            unit?: string | null;
        };
        /**
         * @description A named fixed value the page shows, with where it comes from: a target, a tariff, an
         *     emission factor, or text with digits in it (a line's name, a standard's number) that the
         *     page must not write as a literal.
         */
        DashboardConstant: {
            name: string;
            /** @description Where the value comes from, at most [`MAX_NOTE_CHARS`] characters. */
            note?: string | null;
            title: string;
            unit?: string | null;
            value: components["schemas"]["DashboardConstantValue"];
        };
        /** @description A constant's value. */
        DashboardConstantValue: number | string;
        /** @description A named query whose result the page shows. */
        DashboardDataset: {
            /** @description What the data is and how it is computed, 1 to [`MAX_DESCRIPTION_CHARS`] characters. */
            description: string;
            /**
             * Format: int32
             * @description The most rows the dataset may return, 1 to [`MAX_DATASET_ROWS`]. A run that returns more
             *     is refused, never cut short.
             */
            max_rows: number;
            name: string;
            source: components["schemas"]["DashboardDatasetSource"];
            /** @description 1 to [`MAX_TITLE_CHARS`] characters. */
            title: string;
        };
        /**
         * @description One run of one dataset.
         *
         *     Internal callers (the check) accept fields they do not know, so fields are only ever added
         *     as optional ones (this crate's `README.md`, 「內部 API」).
         */
        DashboardDatasetResult: {
            /** @description The columns, in order. */
            columns: components["schemas"]["DashboardResultColumn"][];
            /** @description The dataset's name in the spec. */
            dataset: string;
            meta: components["schemas"]["DashboardResultMeta"];
            /**
             * Format: int32
             * @description `rows.len()`.
             */
            row_count: number;
            /** @description The rows, each an array of cells in column order. */
            rows: (number | string | boolean | null)[][];
        };
        /** @description Runs one dataset of one version as the caller. */
        DashboardDatasetRunRequest: {
            /** @description The dataset's name in that version's spec. */
            dataset: string;
            /** Format: int32 */
            version: number;
        };
        /** @description Where a dataset's rows come from. */
        DashboardDatasetSource: {
            /** @enum {string} */
            kind: "sql";
            sql: string;
        } | {
            /** @enum {string} */
            kind: "telemetry_statistics";
            request: components["schemas"]["DashboardSemanticRequest"];
        } | {
            /** @enum {string} */
            kind: "telemetry_usage";
            request: components["schemas"]["DashboardSemanticRequest"];
        };
        /**
         * Format: uuid
         * @description A dashboard of the assistant (Laysi/trustiot-v2#150): a conversation's draft or a saved
         *     one in the tenant's list; its versions are numbered within it.
         *
         *     Serialized as a lowercase hyphenated UUID string.
         */
        DashboardId: string;
        /**
         * @description Whether a dashboard is a conversation's draft or a saved one in the tenant's list.
         * @enum {string}
         */
        DashboardKind: "draft" | "saved";
        /** @description A page of dashboards, the most recently updated first. */
        DashboardList: {
            dashboards: components["schemas"]["DashboardSummary"][];
            /** @description The `cursor` for the next page; absent on the last. */
            next_cursor?: string | null;
        };
        /**
         * @description What the caller may do with the dashboard, decided by the server from the request's role
         *     and the caller's relation to the dashboard; the UI shows only the actions allowed here.
         */
        DashboardPermissions: {
            /** @description A saved dashboard and the caller is an editor or admin. */
            can_archive: boolean;
            /** @description A draft, and the caller is an editor or admin who owns its conversation. */
            can_publish: boolean;
            /**
             * @description A saved dashboard and the caller is an editor or admin; or a draft and the caller owns
             *     its conversation.
             */
            can_restore: boolean;
        };
        /**
         * @description Publishes a draft's version: the first time as a new saved dashboard, then as a new version
         *     of the draft's saved dashboard (refused with 409 `dashboard.saved_changed` unless that one's
         *     latest version is the one the published version is based on: an older version of the
         *     draft, or a restored copy of one, keeps the base it was written on, and a version written
         *     before the draft was first published is based on none).
         */
        DashboardPublishRequest: {
            /**
             * Format: int32
             * @description The draft's version the user looked at; a newer one the tool wrote since is not
             *     published by this request.
             */
            version: number;
        };
        /**
         * @description The SQL endpoint's `queryable_from`: per telemetry view, the time from which the answer
         *     holds every row.
         */
        DashboardQueryablePeriod: {
            /**
             * Format: date-time
             * @description `trustiot.tenant_telemetry`, the raw readings.
             */
            tenant_telemetry: string;
            /**
             * Format: date-time
             * @description `trustiot.tenant_telemetry_1h`, the one-hour cells.
             */
            tenant_telemetry_1h: string;
            /**
             * Format: date-time
             * @description `trustiot.tenant_telemetry_1m`, the one-minute cells.
             */
            tenant_telemetry_1m: string;
        };
        /**
         * @description Restores an older version as a new one (refused with 409 `dashboard.restore_unchanged` when
         *     its content is the latest version's).
         */
        DashboardRestoreRequest: {
            /**
             * Format: int32
             * @description The latest version the user saw; refused with 409 `dashboard.version_conflict` when the
             *     dashboard has a newer one.
             */
            base_version: number;
            /**
             * Format: int32
             * @description The version to restore.
             */
            from_version: number;
        };
        /** @description One column of a [`DatasetResult`]. */
        DashboardResultColumn: {
            /** @description The column's name ([`is_column_name`]); a formula and a reference name it. */
            name: string;
            type: components["schemas"]["DashboardColumnType"];
        };
        /** @description When and how a dataset ran. */
        DashboardResultMeta: {
            /**
             * Format: int32
             * @description How long the run took, in milliseconds, as the assistant measured it.
             */
            duration_ms: number;
            /**
             * Format: date-time
             * @description When the query ran, with the offset its source gave: UTC for SQL, the request's time zone
             *     for a semantic source.
             */
            queried_at: string;
            source: components["schemas"]["DashboardSourceMeta"];
        };
        /**
         * Format: int32
         * @description The version of the sandbox runtime a dashboard version was checked with and runs with: the
         *     page API (`window.tq`), the formula language, d3 and the check rules
         *     (`frontend/sandbox/README.md`). Each version's files are kept unchanged, so a dashboard keeps
         *     working when a newer runtime ships. Serialized as a number, `1` for the first.
         */
        DashboardRuntime: number;
        /**
         * @description The saved dashboard a draft was taken from or published to, and its version the draft's
         *     latest version is based on.
         */
        DashboardSavedBase: {
            dashboard_id: components["schemas"]["DashboardId"];
            /**
             * Format: int32
             * @description Absent when the draft's latest version is based on none of the saved dashboard's
             *     versions: it was written before the draft was first published (other than the version
             *     published), or restored from such a version. Publishing it is refused until the
             *     assistant merges the saved dashboard's latest version into the draft.
             */
            version?: number | null;
        };
        /**
         * @description The bucket length of a semantic request: telemetry-query's `BucketInterval`.
         * @enum {string}
         */
        DashboardSemanticInterval: "1m" | "5m" | "15m" | "1h" | "1d" | "1w" | "1mo" | "total";
        /**
         * @description The `range` of a statistics or usage answer (Laysi/trustiot-v2#142), field for field, so the
         *     assistant reads it straight from the response; `services/telemetry-query/tests` checks that
         *     the two stay alike.
         */
        DashboardSemanticRange: {
            /**
             * Format: date-time
             * @description The start of the queryable period of the table read.
             */
            earliest: string;
            /** @description Whether the range ends after the request's time, so its last bucket is not complete. */
            ends_after_now: boolean;
            /**
             * Format: date-time
             * @description The range read: `from` inclusive, `to` exclusive, in `tz`.
             */
            from: string;
            interval: components["schemas"]["DashboardSemanticInterval"];
            /**
             * Format: date-time
             * @description When the request was answered.
             */
            queried_at: string;
            resolution: components["schemas"]["DashboardSemanticResolution"];
            /** Format: date-time */
            to: string;
            /** @description Whether `from` was moved to the start of the queryable period. */
            truncated: boolean;
            /** @description The time zone the buckets follow. */
            tz: string;
        };
        /**
         * @description The request body of telemetry-query's statistics and usage APIs (Laysi/trustiot-v2#142),
         *     stored as the model wrote it and sent unchanged at every run, so a relative `period` such as
         *     `last_7_days` always reads the latest range. `from` and `to` keep the offset they were
         *     written with. `services/telemetry-query/tests` checks that this type and the API's own
         *     request type accept the same bodies.
         */
        DashboardSemanticRequest: {
            /**
             * Format: date-time
             * @description The range's start, RFC 3339 with an offset (inclusive).
             */
            from?: string | null;
            interval?: components["schemas"]["DashboardSemanticInterval"];
            /**
             * @description A relative period: `today`, `yesterday`, `this_week`, `last_week`, `this_month`,
             *     `last_month`, `this_year`, `last_year`, `last_<N>_days` (1 to 366) or `last_<N>_hours`
             *     (1 to 72). Either this or `from` and `to`.
             */
            period?: string | null;
            /** @description 1 to [`MAX_SERIES`] distinct series. */
            series: components["schemas"]["DashboardSemanticSeries"][];
            /**
             * Format: date-time
             * @description The range's end, RFC 3339 with an offset (exclusive).
             */
            to?: string | null;
            /**
             * @description An IANA time zone name, such as `Asia/Taipei`. Only its form is checked here; the API
             *     checks the name when the dataset first runs.
             */
            tz: string;
        };
        /**
         * @description The aggregate table a semantic answer read.
         * @enum {string}
         */
        DashboardSemanticResolution: "1m" | "1h";
        /** @description One data point of one device in a semantic request. */
        DashboardSemanticSeries: {
            device_id: components["schemas"]["DeviceId"];
            /**
             * @description The data point key: 1 to 128 letters, digits, `.`, `_` or `-`, starting with a letter or
             *     digit.
             */
            point: string;
        };
        /** @description What the source said about the data's range. */
        DashboardSourceMeta: {
            /** @enum {string} */
            kind: "sql";
            queryable_from: components["schemas"]["DashboardQueryablePeriod"];
        } | {
            /** @enum {string} */
            kind: "telemetry_statistics";
            range: components["schemas"]["DashboardSemanticRange"];
        } | {
            /** @enum {string} */
            kind: "telemetry_usage";
            range: components["schemas"]["DashboardSemanticRange"];
        };
        /** @description A version's datasets, computations and constants. */
        DashboardSpec: {
            /** @description At most [`MAX_COMPUTATIONS`]. */
            computations?: components["schemas"]["DashboardComputation"][];
            /** @description At most [`MAX_CONSTANTS`]. */
            constants?: components["schemas"]["DashboardConstant"][];
            /** @description 1 to [`MAX_DATASETS`]. */
            datasets: components["schemas"]["DashboardDataset"][];
        };
        /** @description One dashboard in a list, and the dashboard part of a [`DashboardView`]. */
        DashboardSummary: {
            /**
             * Format: date-time
             * @description When it was archived; absent while it is not. Only saved dashboards are archived.
             */
            archived_at?: string | null;
            /**
             * Format: uuid
             * @description A draft's conversation.
             */
            conversation_id?: string | null;
            /** Format: date-time */
            created_at: string;
            /** @description Who made the dashboard: a draft's conversation owner, or a saved one's first publisher. */
            created_by: components["schemas"]["UserId"];
            dashboard_id: components["schemas"]["DashboardId"];
            kind: components["schemas"]["DashboardKind"];
            /** Format: int32 */
            latest_version: number;
            saved?: components["schemas"]["DashboardSavedBase"] | null;
            /** @description The latest version's title. */
            title: string;
            /**
             * Format: date-time
             * @description When the latest version was written.
             */
            updated_at: string;
        };
        /** @description One version of a dashboard, whole. Versions are only ever added. */
        DashboardVersion: {
            /** Format: date-time */
            created_at: string;
            /**
             * @description Who asked for it: the conversation's owner for [`VersionOrigin::Agent`], the user who
             *     published or restored otherwise.
             */
            created_by: components["schemas"]["UserId"];
            description?: string | null;
            origin: components["schemas"]["DashboardVersionOrigin"];
            /**
             * @description The page: an HTML fragment with its scripts, run only inside the sandbox
             *     (`frontend/sandbox/README.md`).
             */
            page: string;
            /**
             * Format: int32
             * @description For [`VersionOrigin::Restore`], the version restored.
             */
            restored_from?: number | null;
            /**
             * @description The runtime the page was checked with and runs with; a restored or published version
             *     keeps its source's.
             */
            runtime: components["schemas"]["DashboardRuntime"];
            spec: components["schemas"]["DashboardSpec"];
            title: string;
            /** Format: int32 */
            version: number;
        };
        /** @description A version written by publish or restore. */
        DashboardVersionCreated: {
            dashboard_id: components["schemas"]["DashboardId"];
            /** Format: int32 */
            version: number;
        };
        /** @description A page of versions, the newest first. */
        DashboardVersionList: {
            next_cursor?: string | null;
            versions: components["schemas"]["DashboardVersionSummary"][];
        };
        /**
         * @description How a version came to be.
         * @enum {string}
         */
        DashboardVersionOrigin: "agent" | "publish" | "restore";
        /** @description One version in a [`VersionList`]: without its spec and page. */
        DashboardVersionSummary: {
            /** Format: date-time */
            created_at: string;
            created_by: components["schemas"]["UserId"];
            origin: components["schemas"]["DashboardVersionOrigin"];
            /** Format: int32 */
            restored_from?: number | null;
            runtime: components["schemas"]["DashboardRuntime"];
            title: string;
            /** Format: int32 */
            version: number;
        };
        /** @description `GET /dashboards/{dashboard_id}`. */
        DashboardView: {
            dashboard: components["schemas"]["DashboardSummary"];
            permissions: components["schemas"]["DashboardPermissions"];
            version: components["schemas"]["DashboardVersion"];
        };
        /**
         * @description Whether a series had readings in the range.
         * @enum {string}
         */
        DataStatus: "ok" | "no_data";
        /** @description A device: a Modbus endpoint one of the tenant's gateways polls with one of its models. */
        Device: {
            /**
             * Format: date-time
             * @description When the device was created, by the database clock.
             */
            created_at: string;
            /** @description The device's ID; telemetry names the device by it. */
            device_id: components["schemas"]["DeviceId"];
            /**
             * @description The hardware ID of the gateway that polls the device, bound to the tenant; never
             *     changed after creation.
             * @example tg-0001
             */
            hardware_id: string;
            /**
             * @description The IP address or DNS name of the device, or of the converter it is behind; never the
             *     gateway itself (a loopback address or `localhost`).
             * @example 192.0.2.10
             */
            host: string;
            /** @description The device model the gateway reads the device with; never changed after creation. */
            model_id: components["schemas"]["DeviceModelId"];
            /**
             * @description The name, unique in the tenant, never changed after creation: 1-40 lowercase letters or
             *     digits in groups joined by single hyphens, starting with a letter (the naming rule of
             *     models).
             * @example line-1-meter
             */
            name: string;
            /**
             * Format: int32
             * @description How often the gateway polls the device, in milliseconds: 1000 (1 s) to 86400000
             *     (1 day); 60000 (1 minute) when a request leaves it out. A plan may set a longer minimum.
             * @example 60000
             */
            poll_interval_ms: number;
            /**
             * Format: int32
             * @description The TCP port. Converters use many ports (502, 4196, 8886, 8899, ...); no default.
             * @example 502
             */
            port: number;
            /**
             * Format: int64
             * @description 1 at creation, one more with every update; send it back with an update to refuse
             *     overwriting someone else's change.
             */
            revision: number;
            /**
             * @description `modbus_tcp`: Modbus TCP to the device. `rtu_over_tcp`: RTU frames over TCP to an RS485 converter; the devices behind one converter (the same host and port on one gateway) share its connection, so they all use this transport and each has its own unit ID.
             * @enum {string}
             */
            transport: "modbus_tcp" | "rtu_over_tcp";
            /**
             * Format: int32
             * @description The Modbus unit ID (station number): 0-255 for Modbus TCP, 1-247 for RTU over TCP; 1
             *     when a request leaves it out.
             * @example 1
             */
            unit_id: number;
            /**
             * Format: date-time
             * @description When the device was last changed.
             */
            updated_at: string;
        };
        /**
         * Format: uuid
         * @description A device of a tenant (rebuild checklist C2, Laysi/trustiot-v2#100): one Modbus endpoint
         *     a gateway polls, made by the assets service. Unique across tenants; telemetry and the
         *     gateway configuration name the device by it (as text, `protocol::DeviceId`).
         *
         *     Serialized as a lowercase hyphenated UUID string.
         */
        DeviceId: string;
        /** @description The tenant's devices, by name. */
        DeviceList: {
            /** @description Every device of the tenant. */
            devices: components["schemas"]["Device"][];
        };
        /** @description A device model. */
        DeviceModel: {
            /**
             * Format: date-time
             * @description When the model was created, by the database clock.
             */
            created_at: string;
            /** @description The model's ID. */
            model_id: components["schemas"]["DeviceModelId"];
            /**
             * @description The name, unique in the tenant, never changed after creation: 1-40 lowercase letters or
             *     digits in groups joined by single hyphens, starting with a letter.
             * @example power-meter
             */
            name: string;
            /** @description The data points, in order. */
            points: components["schemas"]["DeviceModelPoint"][];
            /**
             * Format: int64
             * @description 1 at creation, one more with every update; send it back with an update to refuse
             *     overwriting someone else's change.
             */
            revision: number;
            /**
             * Format: date-time
             * @description When the model was last changed.
             */
            updated_at: string;
        };
        /**
         * @description A device model as a file: what the export downloads and the import uploads. It holds no
         *     IDs, revisions or times, so it can be imported in another tenant or deployment.
         */
        DeviceModelDocument: {
            /**
             * @description Always `trustiot.device_model`.
             * @example trustiot.device_model
             */
            format: string;
            /**
             * Format: int64
             * @description The document's version; this release writes and reads 1.
             */
            format_version: number;
            /** @description The model's name, under the naming rule of [`DeviceModel::name`]. */
            name: string;
            /** @description The data points, in order. */
            points: components["schemas"]["DeviceModelPoint"][];
        };
        /**
         * Format: uuid
         * @description A device model of a tenant (rebuild checklist C1), made by the assets service; unlike the
         *     model's name, which is unique per tenant only, it is unique across tenants.
         *
         *     Serialized as a lowercase hyphenated UUID string.
         */
        DeviceModelId: string;
        /** @description The tenant's device models, by name. */
        DeviceModelList: {
            /** @description Every model of the tenant. */
            models: components["schemas"]["DeviceModelSummary"][];
        };
        /** @description A data point of a device model: how the gateway reads one value, and what the value means. */
        DeviceModelPoint: {
            /**
             * Format: int32
             * @description The 0-based protocol address of the first coil or register (40001 in a device manual is
             *     address 0). A point's registers end at 65535 at most and do not overlap another point's
             *     of the same function code.
             */
            address: number;
            /**
             * @description Where the bytes of the 32-bit value 0xAABBCCDD end up in the registers: `abcd` big-endian (the default), `badc` bytes swapped in each register, `cdab` registers reversed, `dcba` both.
             * @enum {string}
             */
            byte_order?: "abcd" | "badc" | "cdab" | "dcba";
            /**
             * @description How the registers decode: `bool` (a bit, or a register that is not 0), `u16`/`i16` (1 register), `u32`/`i32`/`f32` (2), `u64`/`i64`/`f64` (4), `string` (`length` registers of ASCII). Function codes 1 and 2 read `bool` only.
             * @enum {string}
             */
            data_type: "bool" | "u16" | "i16" | "u32" | "i32" | "u64" | "i64" | "f32" | "f64" | "string";
            /**
             * @description What the value means, for people and for the AI features: up to 512 characters, without
             *     control characters other than line feed and tab, format characters, or line and
             *     paragraph separators. Trimmed; empty means none.
             */
            description?: string | null;
            /**
             * Format: int32
             * @description The Modbus read function: 1 coils, 2 discrete inputs, 3 holding registers, 4 input
             *     registers.
             */
            function_code: number;
            /**
             * @description The key telemetry uses, unique within the model: 1-64 ASCII letters, digits, `.`, `_`
             *     or `-`, starting with a letter or digit.
             * @example voltage
             */
            key: string;
            /**
             * Format: int32
             * @description For `string` only, which requires it: the number of registers, 2 characters each.
             */
            length?: number | null;
            /**
             * Format: double
             * @description Added after `scale`; finite. Numeric types only.
             */
            offset?: number | null;
            /**
             * Format: double
             * @description Multiplier applied to the raw number (rebuild checklist F5); finite and not 0. Numeric
             *     types only.
             */
            scale?: number | null;
        };
        /** @description A device model in the tenant's list. */
        DeviceModelSummary: {
            /**
             * Format: date-time
             * @description When the model was created.
             */
            created_at: string;
            /** @description The model's ID. */
            model_id: components["schemas"]["DeviceModelId"];
            /** @description The name. */
            name: string;
            /**
             * Format: int64
             * @description How many data points it has.
             */
            point_count: number;
            /**
             * Format: int64
             * @description The current revision.
             */
            revision: number;
            /**
             * Format: date-time
             * @description When the model was last changed.
             */
            updated_at: string;
        };
        /**
         * @description The JSON body of every error response (M1). Clients ignore fields they do not know, so
         *     fields are only ever added as optional ones.
         */
        ErrorBody: {
            /** @description What went wrong; clients branch on this. */
            code: components["schemas"]["ErrorCode"];
            /**
             * @description Values the translated message refers to, for example `{"max": 40}`. Never user-facing
             *     text, internal error text, or personal data. Absent when empty.
             */
            details?: Record<string, unknown>;
            /** @description The request fields that were rejected. Absent when empty. */
            field_errors?: components["schemas"]["FieldError"][];
            /** @description The message the client shows, translated by the client. */
            message_key: components["schemas"]["MessageKey"];
        };
        /**
         * @description What went wrong, as one of a closed set of codes (M1). The HTTP status follows from the
         *     code; a more specific reason goes in the message key, not in a new code.
         * @enum {string}
         */
        ErrorCode: "invalid_request" | "validation_failed" | "unauthenticated" | "permission_denied" | "tenant_suspended" | "quota_exceeded" | "not_found" | "conflict" | "rate_limited" | "internal" | "unavailable" | "timeout";
        /**
         * Format: uuid
         * @description An event, unique across services; consumers deduplicate deliveries by it.
         *
         *     Serialized as a lowercase hyphenated UUID string.
         */
        EventId: string;
        /** @description One page of the log, newest first. */
        EventLogPage: {
            /** @description The events, newest first (by `occurred_at`, then `event_id`). */
            events: components["schemas"]["LoggedEvent"][];
            /** @description Pass it as `cursor` for the next, older page; `null` on the last page. Opaque. */
            next_cursor?: string | null;
        };
        /** @description A rejected field of the request body. */
        FieldError: {
            /** @description Values the message refers to. Absent when empty. */
            details?: Record<string, unknown>;
            /**
             * @description JSON Pointer (RFC 6901) to the rejected value in the request body, for example
             *     `/points/3/address`; an empty string means the whole body.
             */
            field: string;
            /** @description The message the client shows next to the field. */
            message_key: components["schemas"]["MessageKey"];
        };
        /** @description A spacing between two readings of at least [`GAP_FACTOR`] times the usual one. */
        Gap: {
            /**
             * Format: date-time
             * @description The reading before the gap and the one after it.
             */
            from: string;
            /** Format: date-time */
            to: string;
        };
        /** @description A gateway's binding to a tenant. */
        GatewayBinding: {
            /** @description The binding: a new one every time the gateway is bound. */
            binding_id: components["schemas"]["GatewayBindingId"];
            /**
             * Format: date-time
             * @description When it was bound, by the database clock.
             */
            bound_at: string;
            /**
             * @description The gateway's hardware ID.
             * @example tg-0001
             */
            hardware_id: string;
            /** @description The tenant the gateway is bound to. */
            tenant_id: components["schemas"]["TenantId"];
        };
        /**
         * Format: uuid
         * @description One binding of a gateway to a tenant (rebuild checklist D3, Laysi/trustiot-v2#101), made
         *     by the assets service. A gateway gets a new one every time it is bound, so a binding and
         *     its events always belong to one tenant.
         *
         *     Serialized as a lowercase hyphenated UUID string.
         */
        GatewayBindingId: string;
        /** @description A gateway's factory registration, as the service records it. */
        GatewayRegistration: {
            /** @description The issuing CA that signed the initial certificate: the SHA-256 of its public key. */
            ca_key_id: string;
            /**
             * @description The initial certificate, PEM: the gateway's MQTT client certificate, subject
             *     `CN=<hardware_id>`, extended key usage client authentication only.
             */
            certificate: string;
            /**
             * @description The initial certificate, then the issuing CA certificate that signed it (the loaded
             *     issuing CA whose key id is `ca_key_id`), PEM: the chain the gateway presents as its MQTT
             *     client certificate. The root is not in it. When that issuing CA is no longer loaded (a
             *     repeat after rotation removed it, services/certs/README.md, "輪替" step 3), the chain is
             *     the initial certificate alone.
             */
            certificate_chain: string;
            /**
             * @description The SHA-256 of the endorsement key (its DER SubjectPublicKeyInfo), 64 lower-case hex
             *     digits.
             */
            endorsement_key_sha256: string;
            /** @description The gateway's hardware ID. */
            hardware_id: string;
            /**
             * Format: date-time
             * @description When the initial certificate expires; the gateway renews it through the renewal endpoint
             *     before then.
             */
            not_after: string;
            /**
             * Format: date-time
             * @description When the initial certificate becomes valid.
             */
            not_before: string;
            /**
             * @description The SHA-256 of the registered public key (its DER SubjectPublicKeyInfo), 64 lower-case
             *     hex digits.
             */
            public_key_sha256: string;
            /**
             * Format: date-time
             * @description When it was registered.
             */
            registered_at: string;
            /**
             * @description The root CA certificate, PEM: the gateway's only trust anchor for the MQTT broker's
             *     server certificate (services/certs/README.md, "閘道器對 broker 伺服器憑證的信任"). The
             *     root CRL (Laysi/trustiot-v2#560) can join it here as a new field.
             */
            root_certificate: string;
            /** @description The serial number of the initial certificate, lower-case hex. */
            serial: string;
            /** @description The name of the workstation that registered it. */
            workstation: string;
        };
        /**
         * @description How readings are grouped: `raw` (no grouping) or a bucket length.
         * @enum {string}
         */
        Interval: "raw" | "1m" | "5m" | "15m" | "1h" | "3h" | "6h" | "12h" | "1d";
        /** @description The body of `POST /api/certs/v1/workstation-certificates`. Unknown fields are refused. */
        IssueWorkstationCertificate: {
            /**
             * @description A PKCS #10 certificate signing request, PEM (`-----BEGIN CERTIFICATE REQUEST-----`), at
             *     most 4096 bytes as DER, signed with ecdsa-with-SHA256 by the workstation's ECDSA P-256
             *     key. Only its public key is used, once its signature shows the workstation holds the
             *     private key: the CA decides the subject, the extensions and the validity, and the private
             *     key never leaves the workstation.
             */
            certificate_request: string;
            /**
             * @description The workstation's name: 1-48 ASCII letters, digits, `.`, `_` or `-`, starting with a
             *     letter or digit. The certificate's subject is `CN=workstation:<name>`.
             * @example line-a-ws-01
             */
            workstation: string;
        };
        /**
         * @description A language the interface or AI answers may use.
         * @enum {string}
         */
        Language: "auto" | "zh-TW" | "en";
        /** @description The latest value of each data point of a device that has a reading in the raw period. */
        Latest: {
            /**
             * Format: uuid
             * @description The device.
             */
            device_id: string;
            /**
             * Format: date-time
             * @description The start of the period searched (the raw period), UTC.
             */
            earliest: string;
            /** @description One entry per data point, ordered by data point. */
            points: components["schemas"]["LatestPoint"][];
        };
        /** @description The latest value of one data point. */
        LatestPoint: {
            /** @description The data point. */
            point: string;
            /**
             * Format: date-time
             * @description When the reading was taken, UTC.
             */
            time: string;
            /** @description The reading. */
            value: components["schemas"]["Reading"];
        };
        /** @description Who made a logged change. */
        LoggedActor: {
            /** @enum {string} */
            kind: "user";
            /** @description The user. */
            user_id: components["schemas"]["UserId"];
        } | {
            /** @enum {string} */
            kind: "service";
            /** @description The service, for example `assets`. */
            service: string;
        };
        /** @description One event in the log. */
        LoggedEvent: {
            actor?: components["schemas"]["LoggedActor"] | null;
            /** @description What changed: the aggregate's ID. */
            aggregate_id: string;
            /**
             * Format: int64
             * @description The event's number within its aggregate, from 1.
             */
            aggregate_seq: number;
            /** @description What changed: the aggregate's kind, for example `tenant`. */
            aggregate_type: string;
            /** @description The event's ID (UUIDv7). */
            event_id: components["schemas"]["EventId"];
            /**
             * Format: date-time
             * @description When the change happened, by the producer's clock.
             */
            occurred_at: string;
            /** @description The event's payload as the producer wrote it, in the version above; a JSON object. */
            payload: {
                [key: string]: unknown;
            };
            /** @description The service that wrote the event, for example `identity`. */
            producer: string;
            /**
             * Format: date-time
             * @description When the event log stored the event, by the database clock.
             */
            recorded_at: string;
            tenant_id?: components["schemas"]["TenantId"] | null;
            /** @description The event type, for example `identity.tenant_created`. */
            type: string;
            /**
             * Format: int32
             * @description The payload's version.
             */
            version: number;
        };
        /** @description The answer to a valid code; the session cookie is in `Set-Cookie`. */
        LoggedIn: {
            /** @description The validated `redirect_to`, or `/`. */
            redirect_to: string;
        };
        /** @description The body of `POST /auth/login`: a JSON object that names one of the two steps in `step`. Any other field is refused. */
        LoginRequest: {
            /** @description The address to log in with. */
            email: string;
            /** @enum {string} */
            step: "send_code";
        } | {
            /** @description The code, 8 digits. */
            code: string;
            /** @description The address the code was sent to. */
            email: string;
            /** @description Where to go after logging in: a path on this site such as `/devices?page=2`. Default `/`. */
            redirect_to?: string | null;
            /** @enum {string} */
            step: "verify_code";
        };
        /**
         * @description A role in a tenant, as the API takes and returns it: setting a member's role, and the tenants
         *     of the current session.
         * @enum {string}
         */
        MemberRole: "viewer" | "editor" | "admin";
        /** @description A user's membership of a tenant. */
        Membership: {
            previous_role?: components["schemas"]["MemberRole"] | null;
            /** @description The role now. */
            role: components["schemas"]["MemberRole"];
            /** @description The tenant. */
            tenant_id: components["schemas"]["TenantId"];
            /** @description The member. */
            user_id: components["schemas"]["UserId"];
        };
        /**
         * @description Key of the translated message the client shows; never the message itself.
         * @example error.not_found
         */
        MessageKey: string;
        /** @description A gateway of the device pool. */
        PoolGateway: {
            binding?: components["schemas"]["GatewayBinding"] | null;
            /**
             * @description The hardware ID: the common name of its certificates and its MQTT identity.
             * @example tg-0001
             */
            hardware_id: string;
            /**
             * Format: date-time
             * @description When the assets service put it in the pool.
             */
            pooled_at: string;
            /**
             * Format: date-time
             * @description When the certs service registered it at the factory.
             */
            registered_at: string;
            /** @description The production workstation that registered it. */
            workstation: string;
        };
        /** @description A page of the device pool, by hardware ID (byte order). */
        PoolGatewayList: {
            /** @description The gateways. */
            gateways: components["schemas"]["PoolGateway"][];
            /** @description The `after` of the next page; `null` on the last one. */
            next_after?: string | null;
        };
        /** @description A user's preferences. */
        Preferences: {
            /** @description The language AI answers are written in; `auto` answers in the language of the question. */
            ai_language: components["schemas"]["Language"];
            /** @description The interface's language; `auto` follows the browser. */
            language: components["schemas"]["Language"];
            /** @description The interface's colors. */
            theme: components["schemas"]["Theme"];
            /**
             * @description An IANA time zone name such as `Asia/Taipei`, or `auto` to follow the browser.
             * @example Asia/Taipei
             */
            time_zone: string;
        };
        /** @description The range an answer covers. */
        QueryRange: {
            /**
             * Format: date-time
             * @description The start of the queryable period of the table read, at the time of the request.
             */
            earliest: string;
            /**
             * @description Whether the range ends after the request's time (a period still running, such as
             *     `today`): its last bucket is not complete.
             */
            ends_after_now: boolean;
            /**
             * Format: date-time
             * @description The range read: `from` inclusive, `to` exclusive. `from` is later than requested when the
             *     request started before the queryable period (`truncated`).
             */
            from: string;
            interval: components["schemas"]["BucketInterval"];
            /**
             * Format: date-time
             * @description When the request was answered.
             */
            queried_at: string;
            /**
             * @description The aggregate table read: one-minute or one-hour cells. A minimum's or maximum's time is
             *     the start of its cell.
             */
            resolution: components["schemas"]["Resolution"];
            /** Format: date-time */
            to: string;
            /** @description Whether `from` was moved to the start of the queryable period. */
            truncated: boolean;
            /** @description The time zone the buckets follow; every time in the answer has its offset. */
            tz: string;
        };
        /** @description One reading's value, as its type stored it. */
        Reading: {
            /** @enum {string} */
            type: "float";
            /**
             * Format: double
             * @description A floating-point reading; `null` for NaN or an infinity.
             */
            value: number | null;
        } | {
            /** @enum {string} */
            type: "int";
            /**
             * Format: int64
             * @description An integer reading.
             */
            value: number;
        } | {
            /** @enum {string} */
            type: "bool";
            /** @description A boolean reading. */
            value: boolean;
        } | {
            /** @enum {string} */
            type: "string";
            /** @description A text reading. */
            value: string;
        };
        /**
         * @description The body of `POST /api/certs/v1/gateway-registrations` (factory registration, rebuild
         *     checklist D1, decision 12). Unknown fields are refused. The request carries the workstation's
         *     signature in the `trustiot-workstation-*` headers; see the certs service's README
         *     ("出廠登錄").
         */
        RegisterGateway: {
            /**
             * @description A PKCS #10 certificate signing request from the gateway's TPM, PEM (`-----BEGIN
             *     CERTIFICATE REQUEST-----`), at most 4096 bytes as DER, signed with ecdsa-with-SHA256 by
             *     the TPM's ECDSA P-256 key. Its public key is registered and never changes; everything else
             *     in it is ignored.
             */
            certificate_request: string;
            /**
             * @description The public part of the TPM's endorsement key, PEM (`-----BEGIN PUBLIC KEY-----`, an X.509
             *     SubjectPublicKeyInfo of at most 2048 bytes as DER): RSA of 2048 to 4096 bits, or ECDSA
             *     P-256 or P-384. The workstation reads and checks it before it calls; the service records
             *     it to recognise the TPM.
             */
            endorsement_key: string;
            /**
             * @description The gateway's hardware ID, which becomes its identity everywhere: the common name of its
             *     certificates, its MQTT username and the gateway segment of its topics. 1-64 ASCII
             *     letters, digits, `.`, `_` or `-`, starting with a letter or digit. A hardware ID is
             *     registered once, and for now it stays taken after it is decommissioned; whether a
             *     decommissioned hardware ID may be registered again is decided in Laysi/trustiot-v2#115.
             * @example gw-0042
             */
            hardware_id: string;
        };
        /**
         * @description The aggregate table a request reads.
         * @enum {string}
         */
        Resolution: "1m" | "1h";
        /**
         * @description A reason a workstation certificate can be revoked for: the RFC 5280 CRLReason codes that
         *     apply to an end entity, without `certificateHold` (a revocation here is final).
         * @enum {string}
         */
        RevocationReason: "unspecified" | "key_compromise" | "affiliation_changed" | "superseded" | "cessation_of_operation" | "privilege_withdrawn";
        /**
         * @description The body of `POST /api/certs/v1/workstation-certificates/{serial}/revoke`. Unknown fields are
         *     refused.
         */
        RevokeWorkstationCertificate: {
            reason?: components["schemas"]["RevocationReason"] | null;
        };
        /** @description A data point's series. */
        Series: {
            /**
             * Format: uuid
             * @description The device.
             */
            device_id: string;
            /**
             * Format: date-time
             * @description The start of the queryable period for this interval at the time of the request.
             */
            earliest: string;
            /**
             * Format: date-time
             * @description The range queried, `from` inclusive and `to` exclusive, UTC; `from` is later than
             *     requested when the request started before the queryable period.
             */
            from: string;
            /** @description The interval used: the requested one, or the automatic choice. */
            interval: components["schemas"]["Interval"];
            /** @description The data point. */
            point: string;
            /** @description The buckets with readings, in time order; buckets without readings are left out. */
            points: components["schemas"]["SeriesPoint"][];
            /** Format: date-time */
            to: string;
            /** @description Whether `from` was moved to the start of the queryable period. */
            truncated: boolean;
            /** @description The time zone the buckets follow. */
            tz: string;
        };
        /** @description One data point of one device. */
        SeriesKey: {
            /**
             * Format: uuid
             * @description The device.
             */
            device_id: string;
            /**
             * @description The data point key: 1 to 128 letters, digits, `.`, `_` or `-`, starting with a letter or
             *     digit.
             */
            point: string;
        };
        /** @description One bucket of a series (for `raw`, one reading). */
        SeriesPoint: {
            /**
             * Format: double
             * @description The mean of the numeric values.
             */
            avg?: number | null;
            /**
             * Format: int64
             * @description How many readings the bucket holds.
             */
            count: number;
            /** @description The bucket's latest reading. */
            last: components["schemas"]["Reading"];
            /**
             * Format: double
             * @description The largest numeric value.
             */
            max?: number | null;
            /**
             * Format: double
             * @description The smallest numeric value (a bool counts as 0 or 1); `null` when the bucket holds only
             *     text readings.
             */
            min?: number | null;
            /**
             * Format: date-time
             * @description The bucket's start (for `raw`, the reading's time), UTC.
             */
            time: string;
        };
        /**
         * @description A semantic request: which series, which range in which time zone, and the bucket length. The
         *     tenant is the caller's; nothing else can name one.
         */
        SeriesQuery: {
            /**
             * Format: date-time
             * @description The range's start, RFC 3339 with an offset (inclusive), rounded down to the minute.
             */
            from?: string | null;
            /** @description The bucket length; `total` (the default) gives one bucket for the whole range. */
            interval?: components["schemas"]["BucketInterval"];
            /**
             * @description A period relative to the request's time: `today`, `yesterday`, `this_week`, `last_week`,
             *     `this_month`, `last_month`, `this_year`, `last_year` (whole local periods, weeks from
             *     Monday), `last_<N>_days` (N local days with today, 1 to 366) or `last_<N>_hours` (up to
             *     now, 1 to 72). Either this or `from` and `to`.
             */
            period?: string | null;
            /** @description 1 to 20 distinct series. */
            series: components["schemas"]["SeriesKey"][];
            /**
             * Format: date-time
             * @description The range's end, RFC 3339 with an offset (exclusive), rounded down to the minute.
             */
            to?: string | null;
            /**
             * @description An IANA time zone name, such as `Asia/Taipei`: the local hours, days, Monday weeks and
             *     months the buckets and the period follow, and the offset of every time in the answer.
             * @example Asia/Taipei
             */
            tz: string;
        };
        /** @description A series' statistics over the range. */
        SeriesStatistics: {
            /**
             * Format: double
             * @description The mean of the numeric readings; for an on/off point, the share of readings that were on.
             */
            avg?: number | null;
            /**
             * Format: int64
             * @description Readings in the range, and how many of them are numeric (float, int, bool).
             */
            count: number;
            /** Format: uuid */
            device_id: string;
            first?: components["schemas"]["TimedReading"] | null;
            highest?: components["schemas"]["TimedValue"] | null;
            last?: components["schemas"]["TimedReading"] | null;
            lowest?: components["schemas"]["TimedValue"] | null;
            /**
             * Format: double
             * @description The largest value and the start of the cell it was in.
             */
            max?: number | null;
            /** Format: date-time */
            max_time?: string | null;
            /**
             * Format: double
             * @description The smallest value and the start of the cell it was in.
             */
            min?: number | null;
            /** Format: date-time */
            min_time?: string | null;
            /** Format: int64 */
            numeric_count: number;
            point: string;
            /** @description The buckets with readings, in time order; buckets without readings are left out. */
            rows: components["schemas"]["StatisticsRow"][];
            status: components["schemas"]["DataStatus"];
        };
        /** @description A series' usage over the range. */
        SeriesUsage: {
            /**
             * @description Whether a reading before the range was found, so the usage up to the first reading in the
             *     range is counted.
             */
            baseline: boolean;
            check?: components["schemas"]["CumulativeCheck"] | null;
            /** Format: uuid */
            device_id: string;
            /**
             * Format: int64
             * @description Drops in the range, each counted as a reset or wrap that started again from 0: each loses
             *     at most the usage of one sampling interval.
             */
            drops: number;
            first_reading?: components["schemas"]["TimedValue"] | null;
            /**
             * Format: int64
             * @description Gaps in the range, and the longest.
             */
            gaps: number;
            highest?: components["schemas"]["TimedValue"] | null;
            largest_suspicious_step?: components["schemas"]["TimedValue"] | null;
            last_reading?: components["schemas"]["TimedValue"] | null;
            longest_gap?: components["schemas"]["Gap"] | null;
            lowest?: components["schemas"]["TimedValue"] | null;
            point: string;
            /**
             * Format: int64
             * @description Readings in the range.
             */
            readings: number;
            /**
             * @description The buckets with readings, in time order; a bucket without readings is left out (unknown,
             *     not zero), and the usage across a gap is in the first bucket after it.
             */
            rows: components["schemas"]["UsageRow"][];
            /**
             * Format: int64
             * @description Cells left out because their first or last value was not a finite number.
             */
            skipped_cells: number;
            status: components["schemas"]["UsageStatus"];
            /**
             * Format: int64
             * @description Steps much faster than the series usually rises, and the largest: the total includes them,
             *     and they may be a meter misread.
             */
            suspicious_steps: number;
            /**
             * Format: double
             * @description The usage over the range; absent unless `status` is `ok`.
             */
            total?: number | null;
        };
        /**
         * @description The body of `PUT /api/identity/v1/platform/tenants/{tenant_id}/members`. Unknown fields are
         *     ignored. Its `Debug` leaves the address out: it is personal data.
         */
        SetMemberRole: {
            /**
             * @description The email address of an existing user, compared after trimming and in ASCII lower case,
             *     as logging in compares it.
             */
            email: string;
            /** @description The role to give the user in the tenant. */
            role: components["schemas"]["MemberRole"];
        };
        /** @description Statistics of each series. */
        Statistics: {
            range: components["schemas"]["QueryRange"];
            /** @description One entry per requested series, in the request's order. */
            series: components["schemas"]["SeriesStatistics"][];
        };
        /** @description One bucket of a series' statistics. */
        StatisticsRow: {
            /** Format: double */
            avg?: number | null;
            /**
             * Format: int64
             * @description Readings in the bucket.
             */
            count: number;
            /** @description The bucket's latest reading. */
            last: components["schemas"]["Reading"];
            /** Format: double */
            max?: number | null;
            /**
             * Format: double
             * @description The smallest, largest and mean numeric value (a bool counts as 0 or 1); `null` when the
             *     bucket holds only text readings.
             */
            min?: number | null;
            /**
             * Format: date-time
             * @description The bucket's start.
             */
            time: string;
        };
        /** @description The body of `PUT /api/identity/v1/sessions/current/tenant`. */
        SwitchTenant: {
            /** @description One of the user's tenants. */
            tenant_id: components["schemas"]["TenantId"];
        };
        /** @description A tenant. */
        Tenant: {
            /**
             * Format: date-time
             * @description When the tenant was created, by the database clock.
             */
            created_at: string;
            /** @description The name, trimmed. */
            name: string;
            /** @description The tenant's ID (UUIDv7). */
            tenant_id: components["schemas"]["TenantId"];
        };
        /**
         * Format: uuid
         * @description A tenant: one customer organisation; every tenant-scoped row and event carries it.
         *
         *     Serialized as a lowercase hyphenated UUID string.
         */
        TenantId: string;
        /** @description A tenant the user belongs to, with their role there. */
        TenantMembership: {
            /**
             * @description The tenant's name; `null` only for a tenant that has no record, which only data written
             *     outside the API has.
             */
            name: string | null;
            /** @description The user's role in the tenant. */
            role: components["schemas"]["MemberRole"];
            /** @description The tenant. */
            tenant_id: components["schemas"]["TenantId"];
        };
        /**
         * @description The interface's colors.
         * @enum {string}
         */
        Theme: "system" | "light" | "dark";
        /** @description A reading and when it was taken. */
        TimedReading: {
            /** Format: date-time */
            time: string;
            value: components["schemas"]["Reading"];
        };
        /** @description A number and the time it belongs to (a bucket's start, or a reading's time). */
        TimedValue: {
            /** Format: date-time */
            time: string;
            /** Format: double */
            value: number;
        };
        /**
         * @description The body of `PUT /api/assets/v1/devices/{device_id}`: the device's settings, which replace
         *     all of the current ones (a field left out takes its default). Unknown fields are refused.
         */
        UpdateDevice: {
            /** @description If sent, the device's current gateway: a device stays on its gateway. */
            hardware_id?: string | null;
            /** @description See [`Device::host`]. */
            host: string;
            model_id?: components["schemas"]["DeviceModelId"] | null;
            /** @description If sent, the device's current name: names never change. */
            name?: string | null;
            /**
             * Format: int32
             * @description See [`Device::poll_interval_ms`]; 60000 when absent.
             */
            poll_interval_ms?: number | null;
            /**
             * Format: int32
             * @description See [`Device::port`].
             */
            port: number;
            /**
             * Format: int64
             * @description The revision the client last read (`revision` of the device it read): the update is
             *     refused with 409 when the device changed since. Required.
             */
            revision: number;
            /**
             * @description `modbus_tcp`: Modbus TCP to the device. `rtu_over_tcp`: RTU frames over TCP to an RS485 converter; the devices behind one converter (the same host and port on one gateway) share its connection, so they all use this transport and each has its own unit ID.
             * @enum {string}
             */
            transport: "modbus_tcp" | "rtu_over_tcp";
            /**
             * Format: int32
             * @description See [`Device::unit_id`]; 1 when absent.
             */
            unit_id?: number | null;
        };
        /**
         * @description The body of `PUT /api/assets/v1/device-models/{model_id}`: the new data points, which replace
         *     all of the model's. Unknown fields are refused.
         */
        UpdateDeviceModel: {
            /** @description The model's name, if sent, must be its current name: names never change. */
            name?: string | null;
            /** @description The data points, at most 1000, in order; may be empty. */
            points: components["schemas"]["DeviceModelPoint"][];
            /**
             * Format: int64
             * @description The revision the client last read (`revision` of the model it read): the update is
             *     refused with 409 when the model changed since, so concurrent editors never overwrite each
             *     other's points unseen. Required.
             */
            revision: number;
        };
        /**
         * @description The body of `PATCH /api/identity/v1/preferences`: the settings to change. A field left out
         *     keeps its value; `null` and unknown fields are refused.
         */
        UpdatePreferences: {
            /** @description The language AI answers are written in. */
            ai_language?: components["schemas"]["Language"];
            /** @description The interface's language. */
            language?: components["schemas"]["Language"];
            /** @description The interface's colors. */
            theme?: components["schemas"]["Theme"];
            /**
             * @description `auto` or an IANA time zone name, spelled as the tzdb spells it.
             * @example Asia/Taipei
             */
            time_zone?: string;
        };
        /** @description Usage of each series. */
        Usage: {
            range: components["schemas"]["QueryRange"];
            /** @description One entry per requested series, in the request's order. */
            series: components["schemas"]["SeriesUsage"][];
        };
        /** @description One bucket of a series' usage. */
        UsageRow: {
            /** @description The bucket starts after a gap: its usage includes the usage of the gap. */
            after_gap: boolean;
            /**
             * Format: int64
             * @description Drops counted as a reset or wrap in the bucket.
             */
            drops: number;
            /**
             * Format: int64
             * @description Readings in the bucket.
             */
            readings: number;
            /**
             * Format: int64
             * @description Steps in the bucket much faster than the series usually rises (a misread or a replaced
             *     meter): the usage includes them.
             */
            suspicious_steps: number;
            /**
             * Format: date-time
             * @description The bucket's start.
             */
            time: string;
            /**
             * Format: double
             * @description The increase of the counter in the bucket.
             */
            usage: number;
        };
        /**
         * @description Whether a series' usage could be computed.
         * @enum {string}
         */
        UsageStatus: "ok" | "no_data" | "not_numeric" | "not_cumulative";
        /**
         * Format: uuid
         * @description A user account; one user can be a member of several tenants.
         *
         *     Serialized as a lowercase hyphenated UUID string.
         */
        UserId: string;
        /**
         * @description What the cumulative check concluded.
         * @enum {string}
         */
        Verdict: "cumulative" | "too_few_readings" | "not_cumulative";
        /** @description A workstation certificate as the ledger records it. */
        WorkstationCertificate: {
            /** @description The issuing CA that signed it: the SHA-256 of its public key, 64 lower-case hex digits. */
            ca_key_id: string;
            /** @description The certificate, PEM. It goes to the workstation, which keeps it with its private key. */
            certificate: string;
            /**
             * Format: date-time
             * @description When it expires; a renewal is another issue request before then.
             */
            not_after: string;
            /**
             * Format: date-time
             * @description When it becomes valid: five minutes before it was signed.
             */
            not_before: string;
            revocation_reason?: components["schemas"]["RevocationReason"] | null;
            /**
             * Format: date-time
             * @description When it was revoked; absent while it is not.
             */
            revoked_at?: string | null;
            /**
             * @description The serial number, lower-case hex (32 digits for the certificates this service signs):
             *     the `{serial}` of the revoke request. Keep it; it is how the certificate is revoked.
             * @example 4f3a9c1e2b7d6058a1c3e5f708192a3b
             */
            serial: string;
            /** @description The certificate's common name: `workstation:<workstation>`. */
            subject: string;
            /** @description The workstation's name. */
            workstation: string;
        };
    };
    responses: never;
    parameters: never;
    requestBodies: never;
    headers: never;
    pathItems: never;
}
export type $defs = Record<string, never>;
export interface operations {
    listDeviceModels: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The caller's tenant's models, by name. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DeviceModelList"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant or no role in it (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    createDeviceModel: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["CreateDeviceModel"];
            };
        };
        responses: {
            /** @description The model was created; `Location` is its URL. */
            201: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DeviceModel"];
                };
            };
            /** @description The `Content-Type` is not `application/json`, the body is larger than 2 MiB (`device_model.body_too_large`), or it is not a JSON object (`device_model.body_invalid`) (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant, or a role below editor (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The tenant has a model of this name (`conflict`, `device_model.name_taken`). */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A naming or data point rule is broken (`validation_failed`); `field_errors` names each problem with a JSON Pointer and a `device_model.*` key. Nothing was written. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The service does not know the tenant yet (`device_model.tenant_not_ready`), or the database failed (`unavailable`). Nothing was written. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    importDeviceModel: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["DeviceModelDocument"];
            };
        };
        responses: {
            /** @description The model was created; `Location` is its URL. */
            201: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DeviceModel"];
                };
            };
            /** @description The `Content-Type` is not `application/json`, the body is larger than 2 MiB (`device_model.body_too_large`), or it is not a JSON object (`device_model.body_invalid`) (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant, or a role below editor (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The tenant has a model of this name (`conflict`, `device_model.name_taken`). */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The document is not `trustiot.device_model` version 1, or a naming or data point rule is broken (`validation_failed`). Nothing was written. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The service does not know the tenant yet (`device_model.tenant_not_ready`), or the database failed (`unavailable`). Nothing was written. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    getDeviceModel: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The model's ID. */
                model_id: components["schemas"]["DeviceModelId"];
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The model. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DeviceModel"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant or no role in it (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such model in the caller's tenant (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    updateDeviceModel: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The model's ID. */
                model_id: components["schemas"]["DeviceModelId"];
            };
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["UpdateDeviceModel"];
            };
        };
        responses: {
            /** @description The model after the update, at its next revision. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DeviceModel"];
                };
            };
            /** @description The `Content-Type` is not `application/json`, the body is larger than 2 MiB (`device_model.body_too_large`), or it is not a JSON object (`device_model.body_invalid`) (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant, or a role below editor (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such model in the caller's tenant (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The model is no longer at the `revision` sent (`conflict`, `device_model.revision_mismatch`, `details.current`). */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A data point rule is broken, `revision` is missing (`device_model.field_required`) or not a positive integer, or `name` is not the model's (`device_model.name_immutable`) (`validation_failed`). Nothing was written. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). Nothing was written. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    deleteDeviceModel: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The model's ID. */
                model_id: components["schemas"]["DeviceModelId"];
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The model and its data points were deleted. */
            204: {
                headers: {
                    [name: string]: unknown;
                };
                content?: never;
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant, or a role below editor (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such model in the caller's tenant (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A device uses the model (`conflict`, `device_model.in_use`). Nothing was deleted. */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). Nothing was deleted. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    exportDeviceModel: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The model's ID. */
                model_id: components["schemas"]["DeviceModelId"];
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The model as a document, with `Content-Disposition: attachment` and the model's name as the file name. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DeviceModelDocument"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant or no role in it (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such model in the caller's tenant (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    listDevices: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The caller's tenant's devices, by name. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DeviceList"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant or no role in it (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    createDevice: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["CreateDevice"];
            };
        };
        responses: {
            /** @description The device was created; `Location` is its URL. */
            201: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Device"];
                };
            };
            /** @description The `Content-Type` is not `application/json`, the body is larger than 16 KiB (`device.body_too_large`), or it is not a JSON object (`device.body_invalid`) (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant, or a role below editor (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The tenant has a device of this name (`device.name_taken`), or another device of the gateway at the same host and port uses the other transport (`device.transport_mismatch`) or the same unit ID (`device.unit_taken`) (`conflict`). Nothing was written. */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A field rule is broken, the gateway is not bound to the caller's tenant (`device.gateway_not_bound`), or the tenant has no such model (`device.model_not_found`) (`validation_failed`); `field_errors` names each problem with a JSON Pointer and a `device.*` key. Nothing was written. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). Nothing was written. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    getDevice: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The device's ID. */
                device_id: components["schemas"]["DeviceId"];
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The device. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Device"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant or no role in it (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such device in the caller's tenant (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    updateDevice: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The device's ID. */
                device_id: components["schemas"]["DeviceId"];
            };
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["UpdateDevice"];
            };
        };
        responses: {
            /** @description The device after the update, at its next revision. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Device"];
                };
            };
            /** @description The `Content-Type` is not `application/json`, the body is larger than 16 KiB (`device.body_too_large`), or it is not a JSON object (`device.body_invalid`) (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant, or a role below editor (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such device in the caller's tenant (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The device's gateway binding has ended, so it can only be deleted (`device.binding_ended`), the device is no longer at the `revision` sent (`device.revision_mismatch`, `details.current`), or another device of the gateway at the same host and port uses the other transport (`device.transport_mismatch`) or the same unit ID (`device.unit_taken`) (`conflict`). Nothing was written. */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A field rule is broken, `revision` is missing or not a positive integer, or `name`, `hardware_id` or `model_id` is not the device's (`device.name_immutable`, `device.gateway_immutable`, `device.model_immutable`) (`validation_failed`). Nothing was written. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). Nothing was written. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    deleteDevice: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The device's ID. */
                device_id: components["schemas"]["DeviceId"];
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The device was deleted. */
            204: {
                headers: {
                    [name: string]: unknown;
                };
                content?: never;
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant, or a role below editor (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such device in the caller's tenant (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). Nothing was deleted. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    listPoolGateways: {
        parameters: {
            query?: {
                /** @description `unbound` for the gateways bound to no tenant (the ones to bind), `bound` for the bound ones; absent for every gateway. */
                state?: string;
                /** @description The previous page's `next_after`, for the next page. */
                after?: string;
                /** @description Gateways per page, 1 to 500; default 100. */
                limit?: number;
            };
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description A page of the pool, by hardware ID. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["PoolGatewayList"];
                };
            };
            /** @description An unknown or malformed parameter (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description Not a platform administrator, or not the administrator's own request (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    getPoolGateway: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The gateway's hardware ID. */
                hardware_id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The gateway. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["PoolGateway"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description Not a platform administrator, or not the administrator's own request (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The pool has no such gateway (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    bindGateway: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The gateway's hardware ID. */
                hardware_id: string;
            };
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["BindGateway"];
            };
        };
        responses: {
            /** @description The gateway was bound to this tenant already: the existing binding. Nothing was written, so retrying after a lost response is safe. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["GatewayBinding"];
                };
            };
            /** @description The gateway is now bound to the tenant; `assets.gateway_bound` was written in the same transaction. */
            201: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["GatewayBinding"];
                };
            };
            /** @description The `Content-Type` is not `application/json`, the body is larger than 4 KiB (`gateway_binding.body_too_large`), or it is not a JSON object (`gateway_binding.body_invalid`) (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description Not a platform administrator, or not the administrator's own request (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The device pool has no such gateway (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description Another tenant holds the gateway (`conflict`, `gateway_binding.bound_to_another_tenant`). Nothing was written. */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description An unknown field, `tenant_id` missing or not a UUID, or a tenant the assets service does not know (`gateway_binding.tenant_unknown`) (`validation_failed`). Nothing was written. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). Nothing was written; retrying is safe. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    getAssetsTenant: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The caller's tenant. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["AssetsTenant"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The user has not picked or joined a tenant (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The service has not processed the tenant's creation yet (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    listConversationDashboards: {
        parameters: {
            query?: {
                /** @description The `next_cursor` of the page before. */
                cursor?: string;
            };
            header?: never;
            path: {
                /** @description The conversation's ID. */
                conversation_id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The conversation's drafts, the most recently updated first, 50 a page. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DashboardList"];
                };
            };
            /** @description An unknown parameter or a bad cursor (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant or no role in it (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such conversation of the caller's (`not_found`): another user's is the same as none. */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    listDashboards: {
        parameters: {
            query?: {
                /** @description Archived ones too; without it only the ones not archived. */
                include_archived?: boolean;
                /** @description The `next_cursor` of the page before. */
                cursor?: string;
            };
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The saved dashboards, the most recently updated first, 50 a page. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DashboardList"];
                };
            };
            /** @description An unknown parameter, a bad value or a bad cursor (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant or no role in it (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    getDashboard: {
        parameters: {
            query?: {
                /** @description The version; the latest without it. */
                version?: number;
            };
            header?: never;
            path: {
                /** @description The dashboard's ID. */
                dashboard_id: components["schemas"]["DashboardId"];
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The dashboard, the version and the caller's permissions (for the UI; every write checks them again). */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DashboardView"];
                };
            };
            /** @description An unknown parameter or a bad version (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant or no role in it (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such dashboard the caller can see (`not_found`), or no such version (`dashboard.version_not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    archiveDashboard: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The saved dashboard's ID. */
                dashboard_id: components["schemas"]["DashboardId"];
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The dashboard is archived. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DashboardArchiveState"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant, or a viewer (`permission_denied`, `dashboard.editor_required`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such dashboard the caller can see (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A draft (`validation_failed`, `dashboard.saved_required`). */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). Nothing was written. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    unarchiveDashboard: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The saved dashboard's ID. */
                dashboard_id: components["schemas"]["DashboardId"];
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The dashboard is not archived. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DashboardArchiveState"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant, or a viewer (`permission_denied`, `dashboard.editor_required`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such dashboard the caller can see (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A draft (`validation_failed`, `dashboard.saved_required`). */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). Nothing was written. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    runDashboardDataset: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The dashboard's ID. */
                dashboard_id: components["schemas"]["DashboardId"];
            };
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["DashboardDatasetRunRequest"];
            };
        };
        responses: {
            /** @description The dataset's table, read with the caller's tenant, role and queryable period. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DashboardDatasetResult"];
                };
            };
            /** @description The body is not JSON, over 4 KiB, or not exactly the request (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant or no role in it (`permission_denied`); the range lies wholly before the caller's queryable period (`quota_exceeded`, `telemetry.outside_queryable_period`, `details.earliest`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such dashboard the caller can see (`not_found`), or no such version (`dashboard.version_not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such dataset (`dashboard.dataset_unknown`); more rows than the dataset allows (`dashboard.dataset_too_many_rows`, `details.max`); a result over 2 MiB (`dashboard.dataset_result_too_large`); a query telemetry-query refuses now (`dashboard.dataset_refused`); a query too heavy (`telemetry.query_too_heavy`) or with too many points (`telemetry.too_many_points`, `details.max`) (`validation_failed`). */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The tenant ran 60 datasets in the last minute or has 2 running in this replica (`dashboard.runs_limited`, with `Retry-After`), or its query quota is used up (`dashboard.query_quota_exhausted`) (`rate_limited`). */
            429: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The tenant's telemetry account is not ready (`telemetry.account_not_ready`), or the run failed (`dashboard.dataset_failed`) (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The query ran longer than the tenant's limit (`timeout`, `dashboard.dataset_timeout`). */
            504: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    publishDashboard: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The draft's ID. */
                dashboard_id: components["schemas"]["DashboardId"];
            };
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["DashboardPublishRequest"];
            };
        };
        responses: {
            /** @description The saved dashboard and its new version. */
            201: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DashboardVersionCreated"];
                };
            };
            /** @description The body is not JSON, over 4 KiB, or not exactly the request (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant, a viewer (`dashboard.editor_required`), or not the conversation's owner (`dashboard.owner_required`) (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such dashboard the caller can see (`not_found`), or no such version (`dashboard.version_not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The saved dashboard's latest version is not the one the draft's version is based on (`dashboard.saved_changed`, `details.latest`), is archived (`dashboard.archived`), or has 10,000 versions (`dashboard.too_many_versions`) (`conflict`). */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description Not a draft (`validation_failed`, `dashboard.draft_required`). */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The user added 10 versions by publishing and restoring in the last minute, or the tenant's users 30 to its saved dashboards (30 more to its drafts), in this replica (`rate_limited`, `dashboard.versions_limited`, `details.retry_after`, with `Retry-After`). Only a request that passed every other check counts. Nothing was written. */
            429: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). Nothing was written. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    restoreDashboard: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The dashboard's ID. */
                dashboard_id: components["schemas"]["DashboardId"];
            };
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["DashboardRestoreRequest"];
            };
        };
        responses: {
            /** @description The dashboard's new version, a copy of `from_version`. */
            201: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DashboardVersionCreated"];
                };
            };
            /** @description The body is not JSON, over 4 KiB, or not exactly the request (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant, a viewer restoring a saved dashboard (`dashboard.editor_required`), or someone other than a draft's conversation owner (`dashboard.owner_required`) (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such dashboard the caller can see (`not_found`), or no such version (`dashboard.version_not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description `base_version` is not the latest (`dashboard.version_conflict`, `details.latest`), the content of `from_version` is the latest version's (`dashboard.restore_unchanged`), the dashboard is archived (`dashboard.archived`), or it has 10,000 versions (`dashboard.too_many_versions`) (`conflict`). */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The user added 10 versions by publishing and restoring in the last minute, or the tenant's users 30 to its saved dashboards (30 more to its drafts), in this replica (`rate_limited`, `dashboard.versions_limited`, `details.retry_after`, with `Retry-After`). Only a request that passed every other check counts. Nothing was written. */
            429: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). Nothing was written. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    listDashboardVersions: {
        parameters: {
            query?: {
                /** @description The `next_cursor` of the page before. */
                cursor?: string;
            };
            header?: never;
            path: {
                /** @description The dashboard's ID. */
                dashboard_id: components["schemas"]["DashboardId"];
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The versions, the newest first, 50 a page. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["DashboardVersionList"];
                };
            };
            /** @description An unknown parameter or a bad cursor (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant or no role in it (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No such dashboard the caller can see (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    registerGateway: {
        parameters: {
            query?: never;
            header: {
                /** @description The workstation certificate, DER, standard base64 with padding. */
                "trustiot-workstation-certificate": string;
                /** @description When the workstation signed the request, Unix seconds; within 300 seconds of the service's clock. */
                "trustiot-workstation-timestamp": string;
                /** @description 22 to 64 base64url characters, never used twice with the same certificate. */
                "trustiot-workstation-nonce": string;
                /** @description ECDSA P-256 with SHA-256 by the workstation certificate's key over `trustiot-workstation-request-v1`, the method, the path, the timestamp, the nonce and the lower-case hex SHA-256 of the body, one per line; the DER signature in standard base64 with padding. */
                "trustiot-workstation-signature": string;
            };
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["RegisterGateway"];
            };
        };
        responses: {
            /** @description The same hardware ID, public key and endorsement key were registered before: the first registration and its certificate. Nothing was signed or written. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["GatewayRegistration"];
                };
            };
            /** @description Registered: the gateway certificate was signed and recorded with the registration, and `certs.gateway_registered` was written. The certificate's subject is `CN=<hardware_id>`, its extended key usage client authentication only, valid for the configured gateway validity (default 90 days). */
            201: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["GatewayRegistration"];
                };
            };
            /** @description The `Content-Type` is not `application/json`, the body is larger than 16 KiB (`gateway_registration.body_too_large`), or it is not a JSON object (`gateway_registration.body_invalid`) (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description Not a platform administrator (`permission_denied`), or the workstation signature does not hold (`gateway_registration.workstation_refused`, `details.reason`: `missing`, `malformed`, `untrusted`, `expired`, `not_workstation`, `signature_invalid`, `stale`, `revoked` or `replayed`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The hardware ID is registered with another public key (`gateway_registration.public_key_mismatch`) or endorsement key (`gateway_registration.endorsement_key_mismatch`), or was decommissioned (`gateway_registration.decommissioned`); or another gateway has the public key (`gateway_registration.public_key_in_use`) or, active, the endorsement key (`gateway_registration.endorsement_key_in_use`) (`conflict`). Nothing was registered. */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description An unknown or missing field, a hardware ID that breaks the gateway ID rule (`gateway_registration.hardware_id_invalid`), a certificate request that is not one PEM CSR (`gateway_registration.request_invalid`), not for ECDSA P-256 with ecdsa-with-SHA256 (`gateway_registration.request_unsupported`) or not signed by its own key (`gateway_registration.request_signature_invalid`), or an endorsement key that is not one PEM public key (`gateway_registration.endorsement_key_invalid`) or not RSA 2048/3072/4096 or ECDSA P-256/P-384 (`gateway_registration.endorsement_key_unsupported`) (`validation_failed`). Nothing was registered. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No issuing CA is loaded or can sign yet (`gateway_registration.ca_not_ready`), or the database failed (`unavailable`). Nothing was registered. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    issueWorkstationCertificate: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["IssueWorkstationCertificate"];
            };
        };
        responses: {
            /** @description Issued and recorded in the ledger. The subject is `CN=workstation:<workstation>`, the extended key usage the TrustIoT workstation purpose only (never client authentication, so the MQTT broker refuses it), valid for the configured workstation validity (default 180 days). Keep `serial`: it is how the certificate is revoked. */
            201: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["WorkstationCertificate"];
                };
            };
            /** @description The `Content-Type` is not `application/json`, the body is larger than 16 KiB (`workstation_certificate.body_too_large`), or it is not a JSON object (`workstation_certificate.body_invalid`) (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description Not a platform administrator (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description An unknown or missing field, a name that breaks its rule (`workstation_certificate.name_invalid`), or a certificate request that is not one PEM CSR (`workstation_certificate.request_invalid`), not for ECDSA P-256 with ecdsa-with-SHA256 (`workstation_certificate.request_unsupported`) or not signed by its own key (`workstation_certificate.request_signature_invalid`) (`validation_failed`). Nothing was issued. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No issuing CA can sign yet (`workstation_certificate.ca_not_ready`), or the database failed (`unavailable`). Nothing was issued. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    revokeWorkstationCertificate: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The certificate's serial number, hex, as `serial` of the issue response. */
                serial: string;
            };
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["RevokeWorkstationCertificate"];
            };
        };
        responses: {
            /** @description Revoked now; the ledger records the time and the reason, and the issuing CA's next scheduled CRL lists it. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["WorkstationCertificate"];
                };
            };
            /** @description The `Content-Type` is not `application/json`, the body is larger than 16 KiB (`workstation_certificate.body_too_large`), or it is not a JSON object (`workstation_certificate.body_invalid`) (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description Not a platform administrator (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No workstation certificate has this serial number (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description It is already revoked (`conflict`, `workstation_certificate.already_revoked`). */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description An unknown field, or a reason that is not one of the listed ones (`workstation_certificate.reason_invalid`) (`validation_failed`). Nothing was revoked. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). Nothing was revoked. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    getTenantEventLog: {
        parameters: {
            query?: {
                /** @description Only events that occurred at or after this RFC 3339 time. */
                from?: string;
                /** @description Only events that occurred before this RFC 3339 time; after `from`. */
                to?: string;
                /** @description Events per page, 1 to 500; default 100. */
                limit?: number;
                /** @description The previous page's `next_cursor`, for the next, older page. */
                cursor?: string;
            };
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description A page of the tenant's events, newest first. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["EventLogPage"];
                };
            };
            /** @description A malformed parameter, or `from` not before `to` (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The user has no tenant, or is not an administrator of it (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed, or the event log is off (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    getPlatformEventLog: {
        parameters: {
            query?: {
                /** @description Only events that occurred at or after this RFC 3339 time. */
                from?: string;
                /** @description Only events that occurred before this RFC 3339 time; after `from`. */
                to?: string;
                /** @description Events per page, 1 to 500; default 100. */
                limit?: number;
                /** @description The previous page's `next_cursor`, for the next, older page. */
                cursor?: string;
            };
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description A page of the platform-wide events, newest first. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["EventLogPage"];
                };
            };
            /** @description A malformed parameter, or `from` not before `to` (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The user is not a platform administrator (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed, or the event log is off (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    getPlatformTenantEventLog: {
        parameters: {
            query?: {
                /** @description Only events that occurred at or after this RFC 3339 time. */
                from?: string;
                /** @description Only events that occurred before this RFC 3339 time; after `from`. */
                to?: string;
                /** @description Events per page, 1 to 500; default 100. */
                limit?: number;
                /** @description The previous page's `next_cursor`, for the next, older page. */
                cursor?: string;
            };
            header?: never;
            path: {
                /** @description The tenant's ID. An ID no tenant has gives an empty page. */
                tenant_id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description A page of the tenant's events, newest first. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["EventLogPage"];
                };
            };
            /** @description A malformed tenant ID or parameter, or `from` not before `to` (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The user is not a platform administrator (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed, or the event log is off (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    setPlatformTenantMemberRole: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The tenant's ID. */
                tenant_id: string;
            };
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["SetMemberRole"];
            };
        };
        responses: {
            /** @description The user was a member: the role is set, and `identity.member_role_changed` written in the same transaction unless the role was already this one (then nothing was written). */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Membership"];
                };
            };
            /** @description The user was not a member: the membership was created, and `identity.member_role_changed` written in the same transaction. */
            201: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Membership"];
                };
            };
            /** @description The tenant ID is not a UUID, the `Content-Type` is not `application/json`, or the body is over 4096 bytes or not a JSON object with a string `email` and a string `role` (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description Not a platform administrator, or not the administrator's own request: another service exchanged the token (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant has the ID; nothing was written (`not_found`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The email is not an address (`/email`, `member.email_invalid`), no user has it (`/email`, `member.email_unknown`; no user is created), or the role is not `viewer`, `editor` or `admin` (`/role`, `member.role_invalid`) (`validation_failed`). Nothing was written. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed; nothing was written (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    getPreferences: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The user's preferences; the defaults (`auto`, `auto`, `auto`, `system`) for a user who never changed one. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Preferences"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A token another service exchanged for a call on the user's behalf (`permission_denied`): only the user's own requests read their preferences. */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    updatePreferences: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["UpdatePreferences"];
            };
        };
        responses: {
            /** @description The preferences after the change; fields left out kept their value. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Preferences"];
                };
            };
            /** @description The `Content-Type` is not `application/json`, the body is over 1024 bytes, or it is not a JSON object of the four fields with string values; unknown fields and `null` are refused (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session, or the user no longer exists (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A cross-site request (`permission_denied`, message key `security.cross_site_request`), or a token another service exchanged for a call on the user's behalf (`permission_denied`); nothing changed. */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A value outside its allow list (`validation_failed`; `/language` `preferences.language_invalid`, `/ai_language` `preferences.ai_language_invalid`, `/time_zone` `preferences.time_zone_invalid`, `/theme` `preferences.theme_invalid`); nothing changed. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed; nothing changed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    logOutEverywhere: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description Every session of the user is revoked; `Set-Cookie` clears the session cookie. The next request of any of them is refused. */
            204: {
                headers: {
                    [name: string]: unknown;
                };
                content?: never;
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed; nothing was revoked (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    getCurrentSession: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The session: the user, the tenant it acts in with the user's role there (as of this request), and every tenant the user belongs to. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["CurrentSession"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A token another service exchanged for a call on the user's behalf (`permission_denied`): only the user's own requests read the session. */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed, too many reads of users' tenants were waiting, or the platform role has no credentials (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    logOut: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The session is revoked; `Set-Cookie` clears the session cookie. Its next request is refused. */
            204: {
                headers: {
                    [name: string]: unknown;
                };
                content?: never;
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed; nothing was revoked (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    switchTenant: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["SwitchTenant"];
            };
        };
        responses: {
            /** @description The session acts in the tenant from its next request on; the body is the session as it is now. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["CurrentSession"];
                };
            };
            /** @description The `Content-Type` is not `application/json`, the body is over 1024 bytes, or it is not the JSON object `{"tenant_id": "<uuid>"}` (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session, a token that names no session, or a session cookie that is missing, is not the session the token was signed from, or whose session has ended (`unauthenticated`); nothing changed. */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The user is not a member of the tenant, or there is no such tenant (`permission_denied`, message key `session.tenant_not_member`); a cross-site request (message key `security.cross_site_request`); or a token another service exchanged (`permission_denied`). The session keeps its tenant. */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed, too many reads of users' tenants were waiting, or the platform role has no credentials; nothing changed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    createTenant: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["CreateTenant"];
            };
        };
        responses: {
            /** @description The tenant was created, and `identity.tenant_created` written in the same transaction. */
            201: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Tenant"];
                };
            };
            /** @description The `Content-Type` is not `application/json`, or the body is not a JSON object with a string `name` (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The user is not a platform administrator (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The name is empty, longer than 100 characters, or holds a control or format character or a line or paragraph separator (`validation_failed`, field `/name`, `tenant.name_invalid`). */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The database failed; nothing was created (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    getTelemetryLatest: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The device. */
                device_id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The latest values; empty when the device has no reading in the raw period, or is not the tenant's. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Latest"];
                };
            };
            /** @description A malformed device ID (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The user has not picked or joined a tenant (`permission_denied`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The tenant's account has not been created yet (`not_found`, `telemetry.account_not_ready`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The device has more data points than one answer holds (`telemetry.too_many_points`), or the query reads or holds too much (`telemetry.query_too_heavy`). */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The tenant's query quota or concurrency limit is used up (`rate_limited`). */
            429: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description Postgres or ClickHouse failed, or the tenant's ClickHouse account is not as the service creates it (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The query ran into its time limit (`timeout`). */
            504: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    getTelemetrySeries: {
        parameters: {
            query: {
                /** @description The data point key (1 to 128 characters). */
                point: string;
                /** @description The range's start, RFC 3339 (inclusive). Percent-encode a `+` offset, or use `Z`. */
                from: string;
                /** @description The range's end, RFC 3339 (exclusive). */
                to: string;
                /** @description An IANA time zone name, for example `Asia/Taipei`: hour and day buckets start at its local hours and midnights. */
                tz: string;
                /** @description `raw`, `1m`, `5m`, `15m`, `1h`, `3h`, `6h`, `12h` or `1d`; without it, the finest interval that gives at most 1500 buckets. */
                interval?: components["schemas"]["Interval"];
            };
            header?: never;
            path: {
                /** @description The device. */
                device_id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The series. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Series"];
                };
            };
            /** @description A malformed device ID or query string (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant (`permission_denied`), or the range lies wholly before the queryable period (`quota_exceeded`, `telemetry.outside_queryable_period`, detail `earliest`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The tenant's account has not been created yet (`not_found`, `telemetry.account_not_ready`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A parameter breaks a rule (`validation_failed`, detail `parameter`), the series would have too many points (`telemetry.too_many_points`), or the query reads or holds too much (`telemetry.query_too_heavy`). */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The tenant's query quota or concurrency limit is used up (`rate_limited`). */
            429: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description Postgres or ClickHouse failed, or the tenant's ClickHouse account is not as the service creates it (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The query ran into its time limit (`timeout`). */
            504: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    queryTelemetryStatistics: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["SeriesQuery"];
            };
        };
        responses: {
            /** @description The statistics of each series, in the request's order. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Statistics"];
                };
            };
            /** @description The body is not the JSON object described, or has a field it does not describe (`invalid_request`, `telemetry.query_invalid`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant (`permission_denied`), or the range lies wholly before the queryable period (`quota_exceeded`, `telemetry.outside_queryable_period`, detail `earliest`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The tenant's account has not been created yet (`not_found`, `telemetry.account_not_ready`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A field breaks a rule (`validation_failed`, detail `parameter`: `series`, `tz`, `period`, `from`, or `to` with detail `latest` when it ends too far after now), the answer would have too many rows (`telemetry.too_many_points`), or the query reads or holds too much (`telemetry.query_too_heavy`). */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The tenant's query quota or concurrency limit is used up (`rate_limited`). */
            429: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description Postgres or ClickHouse failed, or the tenant's ClickHouse account is not as the service creates it (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The query ran into its time limit (`timeout`). */
            504: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    queryTelemetryUsage: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["SeriesQuery"];
            };
        };
        responses: {
            /** @description The usage of each series, in the request's order; a series that is not a counter or not numeric has a `status` that says so and no usage. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Usage"];
                };
            };
            /** @description The body is not the JSON object described, or has a field it does not describe (`invalid_request`, `telemetry.query_invalid`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No valid session (`unauthenticated`). */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No tenant (`permission_denied`), or the range lies wholly before the queryable period (`quota_exceeded`, `telemetry.outside_queryable_period`, detail `earliest`). */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The tenant's account has not been created yet (`not_found`, `telemetry.account_not_ready`). */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A field breaks a rule (`validation_failed`, detail `parameter`: `series`, `tz`, `period`, `from`, or `to` with detail `latest` when it ends too far after now), the answer would have too many rows (`telemetry.too_many_points`), or the request would read too many one-minute cells (detail `parameter`: `series`) or the query reads or holds too much (`telemetry.query_too_heavy`). */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The tenant's query quota or concurrency limit is used up (`rate_limited`). */
            429: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description Postgres or ClickHouse failed, or the tenant's ClickHouse account is not as the service creates it (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The query ran into its time limit (`timeout`). */
            504: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
    logIn: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["LoginRequest"];
            };
        };
        responses: {
            /** @description `verify_code`: the code matched. `Set-Cookie` holds the session cookie `__Host-trustiot-session` (`HttpOnly`, `Secure`, `SameSite=Lax`, `Path=/`). */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["LoggedIn"];
                };
            };
            /** @description `send_code`: if a user has the address, a code is on its way. The same answer whether or not one has. */
            202: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["CodeSent"];
                };
            };
            /** @description A query string, a `Content-Type` other than `application/json`, a body over 4096 bytes, or a body that is not one of the two steps; unknown fields such as `password` are refused (`invalid_request`). */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description `verify_code`: no usable code for the address, or a wrong code (`unauthenticated`). A code dies after 5 attempts and 10 minutes. */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description A cross-site request: `Sec-Fetch-Site` other than `same-origin`, or without it an `Origin` that is not this site's (`permission_denied`, message key `security.cross_site_request`). Checked before anything else; nothing changed. */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description The email is not an address (`/email`, `login.email_invalid`), the code is not 8 digits (`/code`, `login.code_invalid`), or `redirect_to` is not a path on this site (`/redirect_to`, `login.redirect_invalid`) (`validation_failed`). */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
            /** @description No usable SMTP relay is configured, too many codes are waiting to be sent, the signing keys have not been read, or the database failed; nothing changed (`unavailable`). */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ErrorBody"];
                };
            };
        };
    };
}
