import type { AllTrue, Assert, Equal } from "../lib/type-binding.ts";
import { z } from "../lib/zod.ts";
import type { components } from "./schema.gen.ts";

export type ErrorCode = components["schemas"]["ErrorCode"];

/**
 * Every code, in the spec's order. `ErrorCodeMatchesSpec` makes a code added to or removed from
 * api/openapi.json a type error here; i18n/server-keys.test.ts checks each has a translation.
 */
const ErrorCodeSchema = z.enum([
  "invalid_request",
  "validation_failed",
  "unauthenticated",
  "permission_denied",
  "tenant_suspended",
  "quota_exceeded",
  "not_found",
  "conflict",
  "rate_limited",
  "internal",
  "unavailable",
  "timeout",
]);
export const ERROR_CODES = ErrorCodeSchema.options;

/** Drops the keys whose value is `undefined`, so an absent optional field stays absent. */
function present<T extends Record<string, unknown>>(value: T): T {
  const kept = { ...value };
  for (const key of Object.keys(kept)) {
    if (kept[key] === undefined) {
      delete kept[key];
    }
  }
  return kept;
}

/**
 * `details`: only text and finite numbers are kept, because message values are never markup or
 * nested data; the other members are dropped, not a failure. Nothing left, or not an object at
 * all, leaves it out.
 */
const DetailsSchema = z
  .record(
    z.string(),
    z
      .union([z.string(), z.number().refine(Number.isFinite)])
      .optional()
      .catch(undefined),
  )
  .transform((items) => {
    const kept: Record<string, string | number> = {};
    for (const [name, item] of Object.entries(items)) {
      if (item !== undefined) {
        kept[name] = item;
      }
    }
    return Object.keys(kept).length > 0 ? kept : undefined;
  })
  .optional()
  .catch(undefined);

/** One rejected request field; `details` holds the values its message refers to. */
const FieldErrorSchema = z
  .object({
    field: z.string(),
    message_key: z.string(),
    details: DetailsSchema,
  })
  .transform(present);
export type FieldError = z.output<typeof FieldErrorSchema>;

/**
 * The `field_errors` list: an entry that is not a field error is dropped, not a failure. No
 * entry left, or not a list at all, leaves it out.
 */
const FieldErrorsSchema = z
  .array(z.unknown())
  .transform((items) => {
    const kept: FieldError[] = [];
    for (const item of items) {
      const parsed = FieldErrorSchema.safeParse(item);
      if (parsed.success) {
        kept.push(parsed.data);
      }
    }
    return kept.length > 0 ? kept : undefined;
  })
  .optional()
  .catch(undefined);

/**
 * The M1 error body (crates/contract/src/error.rs), as the client keeps it: the code, the message
 * key the user sees translated, and the values the message refers to. Unknown fields are ignored,
 * so a newer server can add optional ones.
 */
const ErrorBodySchema = z
  .object({
    code: ErrorCodeSchema,
    message_key: z.string(),
    details: DetailsSchema,
    field_errors: FieldErrorsSchema,
  })
  .transform(present);
export type ErrorBody = z.output<typeof ErrorBodySchema>;

// The client keeps `details` narrower than the spec's open object (text and finite numbers
// only), so `details` is bound by assignability and the rest of each type by `Equal`; the key
// sets must be the same, and the two optional keys must stay optional.
type Spec = components["schemas"];
type Rest<T> = Omit<T, "details" | "field_errors">;
type Optional<T, K extends keyof T> = Partial<Pick<T, K>> extends Pick<T, K> ? true : false;
export type ErrorCodeMatchesSpec = Assert<Equal<z.output<typeof ErrorCodeSchema>, ErrorCode>>;
export type FieldErrorMatchesSpec = Assert<
  AllTrue<
    [
      Equal<Rest<FieldError>, Rest<Spec["FieldError"]>>,
      Equal<keyof FieldError, keyof Spec["FieldError"]>,
      Optional<FieldError, "details">,
      FieldError["details"] extends Spec["FieldError"]["details"] ? true : false,
    ]
  >
>;
export type ErrorBodyMatchesSpec = Assert<
  AllTrue<
    [
      Equal<Rest<ErrorBody>, Rest<Spec["ErrorBody"]>>,
      Equal<keyof ErrorBody, keyof Spec["ErrorBody"]>,
      Optional<ErrorBody, "details" | "field_errors">,
      ErrorBody["details"] extends Spec["ErrorBody"]["details"] ? true : false,
      ErrorBody["field_errors"] extends Spec["ErrorBody"]["field_errors"] ? true : false,
    ]
  >
>;

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

/** Where a response differs from its schema, without the received value: only a path and zod's issue code. */
export interface ResponseIssue {
  /** JSON Pointer (RFC 6901) to the member; an empty string means the whole body. */
  path: string;
  /** zod's issue code, for example `invalid_type`. */
  code: string;
}

function pointer(path: readonly PropertyKey[]): string {
  return path.map((key) => `/${String(key).replaceAll("~", "~0").replaceAll("/", "~1")}`).join("");
}

/**
 * A successful (2xx) response whose body does not have the shape the caller's schema says: the
 * server and this page disagree, as during a rolling update, or something other than the service
 * answered. Not an `ApiError`: the server reported no failure, and asking again gets the same
 * answer, so it is not retried (`classifyError` says "other"). The message and `issues` name the
 * members and zod's issue codes, never the received values, which can be tenant data.
 */
export class ResponseShapeError extends Error {
  readonly status: number;
  readonly issues: readonly ResponseIssue[];

  constructor(status: number, issues: readonly ResponseIssue[]) {
    const where = issues.map((issue) => `${issue.path === "" ? "(body)" : issue.path} ${issue.code}`).join(", ");
    super(`HTTP ${status}: response does not match its schema: ${where}`);
    this.name = "ResponseShapeError";
    this.status = status;
    this.issues = issues;
  }

  /** The error for a failed `safeParse`. */
  static from(status: number, error: z.ZodError): ResponseShapeError {
    return new ResponseShapeError(
      status,
      error.issues.map((issue) => ({ path: pointer(issue.path), code: issue.code })),
    );
  }
}

/** An M1 error body, or null when `value` is not one (or carries a code this client does not know). */
export function parseErrorBody(value: unknown): ErrorBody | null {
  const parsed = ErrorBodySchema.safeParse(value);
  return parsed.success ? parsed.data : null;
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
