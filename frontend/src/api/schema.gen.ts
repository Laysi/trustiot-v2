// Generated from api/openapi.json by frontend/tools/api-client.ts. Do not edit by hand:
// run `bun run --cwd frontend api:generate` after `cargo run -p openapi`.

export interface paths {
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
        /** @description The body of `POST /api/identity/v1/tenants`. Unknown fields are ignored. */
        CreateTenant: {
            /**
             * @description What people call the tenant: 1 to 100 characters after trimming, without control or
             *     format characters or line and paragraph separators.
             */
            name: string;
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
            details?: Record<string, never>;
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
        /** @description A rejected field of the request body. */
        FieldError: {
            /** @description Values the message refers to. Absent when empty. */
            details?: Record<string, never>;
            /**
             * @description JSON Pointer (RFC 6901) to the rejected value in the request body, for example
             *     `/points/3/address`; an empty string means the whole body.
             */
            field: string;
            /** @description The message the client shows next to the field. */
            message_key: components["schemas"]["MessageKey"];
        };
        /**
         * @description How readings are grouped: `raw` (no grouping) or a bucket length.
         * @enum {string}
         */
        Interval: "raw" | "1m" | "5m" | "15m" | "1h" | "3h" | "6h" | "12h" | "1d";
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
    };
    responses: never;
    parameters: never;
    requestBodies: never;
    headers: never;
    pathItems: never;
}
export type $defs = Record<string, never>;
export interface operations {
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
}
