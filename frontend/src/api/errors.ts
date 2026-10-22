import type { components } from "./schema.gen.ts";

export type ErrorCode = components["schemas"]["ErrorCode"];

/** One rejected request field; `details` holds the values its message refers to. */
export interface FieldError {
  field: string;
  message_key: string;
  details?: Readonly<Record<string, string | number>>;
}

/**
 * The M1 error body (crates/contract/src/error.rs), as the client keeps it: the code, the message
 * key the user sees translated, and the values the message refers to. Unknown fields are ignored,
 * so a newer server can add optional ones.
 */
export interface ErrorBody {
  code: ErrorCode;
  message_key: string;
  details?: Readonly<Record<string, string | number>>;
  field_errors?: readonly FieldError[];
}

/**
 * Every code, in the spec's order. The `Record` makes a code added to api/openapi.json without
 * an entry here a type error; i18n/server-keys.test.ts checks each has a translation.
 */
const CODES: Readonly<Record<ErrorCode, true>> = {
  invalid_request: true,
  validation_failed: true,
  unauthenticated: true,
  permission_denied: true,
  tenant_suspended: true,
  quota_exceeded: true,
  not_found: true,
  conflict: true,
  rate_limited: true,
  internal: true,
  unavailable: true,
  timeout: true,
};
export const ERROR_CODES = Object.keys(CODES) as readonly ErrorCode[];

/** The codes the server marks retryable (`ErrorCode::is_retryable`). */
const RETRYABLE: ReadonlySet<ErrorCode> = new Set(["rate_limited", "unavailable", "timeout"]);

/**
 * A request that did not succeed. `status` is the HTTP status, or 0 when no response arrived.
 * `body` is null when the response was not an M1 error body: the ingress answers 401, 403 and
 * 503 itself with plain text (decision 27).
 */
export class ApiError extends Error {
  readonly status: number;
  readonly body: ErrorBody | null;

  constructor(status: number, body: ErrorBody | null, options?: { cause?: unknown }) {
    super(body ? `HTTP ${status}: ${body.code}` : status === 0 ? "no response" : `HTTP ${status}`, options);
    this.name = "ApiError";
    this.status = status;
    this.body = body;
  }

  /** Worth retrying: the server says so, or no response arrived at all. */
  get retryable(): boolean {
    if (this.status === 0) {
      return true;
    }
    if (this.body) {
      return RETRYABLE.has(this.body.code);
    }
    return this.status === 429 || this.status === 503 || this.status === 504;
  }
}

function isErrorCode(value: unknown): value is ErrorCode {
  return typeof value === "string" && Object.hasOwn(CODES, value);
}

/** Keeps only text and finite numbers: message values are never markup or nested data. */
function parseDetails(value: unknown): Record<string, string | number> | undefined {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return undefined;
  }
  const details: Record<string, string | number> = {};
  for (const [name, item] of Object.entries(value)) {
    if (typeof item === "string" || (typeof item === "number" && Number.isFinite(item))) {
      details[name] = item;
    }
  }
  return Object.keys(details).length > 0 ? details : undefined;
}

/** An M1 error body, or null when `value` is not one (or carries a code this client does not know). */
export function parseErrorBody(value: unknown): ErrorBody | null {
  if (typeof value !== "object" || value === null) {
    return null;
  }
  const record = value as Record<string, unknown>;
  if (!isErrorCode(record.code) || typeof record.message_key !== "string") {
    return null;
  }
  const body: ErrorBody = { code: record.code, message_key: record.message_key };
  const details = parseDetails(record.details);
  if (details) {
    body.details = details;
  }
  if (Array.isArray(record.field_errors)) {
    const fields: FieldError[] = [];
    for (const item of record.field_errors as unknown[]) {
      if (typeof item !== "object" || item === null) continue;
      const entry = item as Record<string, unknown>;
      if (typeof entry.field !== "string" || typeof entry.message_key !== "string") continue;
      const fieldDetails = parseDetails(entry.details);
      fields.push({
        field: entry.field,
        message_key: entry.message_key,
        ...(fieldDetails ? { details: fieldDetails } : {}),
      });
    }
    if (fields.length > 0) {
      body.field_errors = fields;
    }
  }
  return body;
}

/** What a page does about an error. */
export type ErrorKind = "unauthenticated" | "forbidden" | "tenant_suspended" | "unavailable" | "not_found" | "other";

/**
 * Sorts an error for the pages: 503 from the ingress (identity unreachable, decision 27's
 * statusOnError) is "unavailable", not "sign in again".
 */
export function classifyError(error: unknown): ErrorKind {
  if (!(error instanceof ApiError)) {
    return "other";
  }
  const code = error.body?.code;
  if (code === "tenant_suspended") return "tenant_suspended";
  if (code === "unauthenticated" || (code === undefined && error.status === 401)) return "unauthenticated";
  if (code === "permission_denied" || (code === undefined && error.status === 403)) return "forbidden";
  if (code === "not_found" || (code === undefined && error.status === 404)) return "not_found";
  if (
    code === "unavailable" ||
    code === "timeout" ||
    error.status === 0 ||
    error.status === 503 ||
    error.status === 504
  ) {
    return "unavailable";
  }
  return "other";
}
