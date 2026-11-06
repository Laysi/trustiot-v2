import { ApiError, type ErrorCode } from "../api/errors.ts";
import { translateFieldError } from "../i18n/errors.ts";
import type { Locale, MessageId } from "../i18n/messages.ts";
import type { MessageValues } from "../i18n/translate.ts";

/**
 * One rejected input of a form: where (a JSON Pointer into the request body, as the server
 * names it: `/host`, `/points/2/address`) and why. Client checks and the server's field errors
 * share this form, so the page shows both the same way. The page keeps the problem, not its
 * text, so a change of language translates it again.
 */
export interface Problem {
  pointer: string;
  message_key: string;
  details?: MessageValues;
  code: ErrorCode;
}

/** A problem the page found itself before sending anything. */
export function clientProblem(pointer: string, messageKey: MessageId, details?: MessageValues): Problem {
  return { pointer, message_key: messageKey, ...(details ? { details } : {}), code: "validation_failed" };
}

/** The text of a problem in `locale`. */
export function problemText(locale: Locale, problem: Problem): string {
  return translateFieldError(
    locale,
    { message_key: problem.message_key, ...(problem.details ? { details: problem.details } : {}) },
    problem.code,
  );
}

/**
 * What a refused write says about the form's inputs: the server's `field_errors`, plus the
 * refusals that name no field but are about one (a taken name is a 409 with only a message key;
 * `byMessageKey` says which input each such key belongs to). `rest` is the error when it is not
 * about an input (the whole request failed): the page shows it as a message of its own.
 */
export function problemsOf(
  error: unknown,
  byMessageKey: Readonly<Record<string, string>> = {},
): { problems: Problem[]; rest: unknown } {
  if (!(error instanceof ApiError) || error.body === null) return { problems: [], rest: error };
  const { body } = error;
  const fields = body.field_errors ?? [];
  if (fields.length > 0) {
    return {
      problems: fields.map((field) => ({
        pointer: field.field,
        message_key: field.message_key,
        ...(field.details ? { details: field.details } : {}),
        code: body.code,
      })),
      rest: null,
    };
  }
  const pointer = byMessageKey[body.message_key];
  if (pointer !== undefined) {
    return {
      problems: [
        {
          pointer,
          message_key: body.message_key,
          ...(body.details ? { details: body.details } : {}),
          code: body.code,
        },
      ],
      rest: null,
    };
  }
  return { problems: [], rest: error };
}
