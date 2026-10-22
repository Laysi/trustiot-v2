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
         * @description Key of the translated message the client shows; never the message itself.
         * @example error.not_found
         */
        MessageKey: string;
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
}
