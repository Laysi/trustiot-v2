import { isMessageId } from "../i18n/messages.ts";
import type { I18n } from "../i18n/useI18n.ts";
import { z } from "../lib/zod.ts";

/**
 * What the router carries from the page that sent a question to the conversation page it opens:
 * the question's text and run, so that page shows the question while it is answered (a question
 * is stored with its answer, not before; Laysi/trustiot-v2#646).
 */
export const AskedState = z.object({ asked: z.object({ runId: z.string(), startedAt: z.string(), text: z.string() }) });

/** Why a question was not sent: the field the service names (`/question`), else its error message. */
export function askFailureReason(i18n: Pick<I18n, "fieldError" | "errorMessage">, error: unknown): string {
  return i18n.fieldError(error, "/question") ?? i18n.errorMessage(error);
}

/**
 * The text for a run that ended without an answer: the message the service names
 * (`latest_run.message_key`, `question.<kind>`), or the general one for a key this client has not
 * heard of. Never the raw key or `result`.
 */
export function runEndMessage(i18n: Pick<I18n, "t">, messageKey: string | null | undefined): string {
  return i18n.t(messageKey && isMessageId(messageKey) ? messageKey : "question.failed");
}
