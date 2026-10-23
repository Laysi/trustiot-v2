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
        get?: never;
        put?: never;
        post?: never;
        /** Logs out: revokes the session of the request's cookie. */
        delete: operations["logOut"];
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
         * @description Key of the translated message the client shows; never the message itself.
         * @example error.not_found
         */
        MessageKey: string;
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
         * Format: uuid
         * @description A user account; one user can be a member of several tenants.
         *
         *     Serialized as a lowercase hyphenated UUID string.
         */
        UserId: string;
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
