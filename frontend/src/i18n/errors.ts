import type { ErrorBody, ErrorCode } from "../api/errors.ts";
import { isMessageId, type Locale, type MessageId } from "./messages.ts";
import { type MessageValues, translate } from "./translate.ts";

/**
 * The default message of each M1 code (`ErrorCode::default_message_key`). A code missing here is
 * a type error; server-keys.test.ts checks the keys match the server's.
 */
export const ERROR_CODE_MESSAGES: Readonly<Record<ErrorCode, MessageId>> = {
  invalid_request: "error.invalid_request",
  validation_failed: "error.validation_failed",
  unauthenticated: "error.unauthenticated",
  permission_denied: "error.permission_denied",
  tenant_suspended: "error.tenant_suspended",
  session_changed: "error.session_changed",
  quota_exceeded: "error.quota_exceeded",
  not_found: "error.not_found",
  conflict: "error.conflict",
  rate_limited: "error.rate_limited",
  internal: "error.internal",
  unavailable: "error.unavailable",
  timeout: "error.timeout",
};

/** A message for a response that carried no M1 body (the ingress's own 401, 403 and 503). */
function messageForStatus(status: number): MessageId {
  switch (status) {
    case 0:
      return "app.error.network";
    case 400:
      return "error.invalid_request";
    case 401:
      return "error.unauthenticated";
    case 403:
      return "error.permission_denied";
    case 404:
      return "error.not_found";
    case 409:
      return "error.conflict";
    case 429:
      return "error.rate_limited";
    case 503:
      return "error.unavailable";
    case 504:
      return "error.timeout";
    default:
      return status >= 500 ? "error.internal" : "app.error.unexpected";
  }
}

/**
 * The text to show for an error: the server's message key when this client knows it, otherwise
 * the code's default message, otherwise one chosen by the HTTP status. A raw key is never shown.
 */
export function translateError(locale: Locale, body: ErrorBody | null, status: number): string {
  if (body === null) {
    return translate(locale, messageForStatus(status));
  }
  const values: MessageValues | undefined = body.details;
  if (isMessageId(body.message_key)) {
    return translate(locale, body.message_key, values);
  }
  return translate(locale, ERROR_CODE_MESSAGES[body.code], values);
}

/** The text for one rejected field, falling back to the code's message like `translateError`. */
export function translateFieldError(
  locale: Locale,
  field: { message_key: string; details?: MessageValues },
  code: ErrorCode,
): string {
  if (isMessageId(field.message_key)) {
    return translate(locale, field.message_key, field.details);
  }
  return translate(locale, ERROR_CODE_MESSAGES[code], field.details);
}
