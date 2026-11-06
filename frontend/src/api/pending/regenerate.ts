// Pending Laysi/trustiot-v2#649: replace with a module over the generated client when api/openapi.json has these endpoints.
import type { Client } from "openapi-fetch";

import type { AnswerRegenerator } from "../../pages/conversations/regenerate.tsx";
import type { ApiClient } from "../client.ts";
import { pinnable, unwrap } from "../client.ts";
import { ConversationRunSchema } from "../conversations.ts";
import type { components, paths } from "../schema.gen.ts";

/**
 * The regenerate endpoint of Laysi/trustiot-v2#649, as PR Laysi/trustiot-v2#1083 (branch
 * `feat/649-regenerate`, still open, so this may change) defines it in its `api/openapi.json` and
 * `services/assistant/README.md`:
 *
 * - `POST /api/assistant/v1/conversations/{conversation_id}/regenerate`, body `{ "answer": <seq> }`
 *   with exactly that field, `seq` an integer from 0 to 2,147,483,647: the position the
 *   conversation's last answer has in `GET /conversations/{id}` (`messages[].seq`). A body of
 *   another shape is 400 `conversation.body_invalid`.
 * - 202 with the `ConversationRun` of the question asked again: the conversation's last round
 *   (question, steps, answer) was discarded in the same transaction that started it, and does not
 *   come back whether the run is answered, fails or is stopped.
 * - 409 `conversation.question_running` (a question runs), `question.nothing_to_regenerate` (the
 *   last round has no answer), `question.copied_answer` (the answer was copied from a shared
 *   conversation), `question.regenerate_stale` (`answer` is not the last answer any more: nothing was
 *   discarded), `question.history_full`; 429 `question.running_limited` and `conversation.start_limited`
 *   (with `Retry-After`; regenerations count with the conversations started in a day); 503
 *   `question.busy`; 404 for a conversation that is not the caller's or is deleted. The refusals
 *   before the discard leave everything as it was; a failed or refused delegation of the identity
 *   service (401, 403, 503, 504) comes after it, and the round stays discarded.
 */
const REGENERATE_PATH = "/api/assistant/v1/conversations/{conversation_id}/regenerate";

type ErrorBody = components["schemas"]["ErrorBody"];

/** The spec's shape of the endpoint, written as the generator writes one, for the client below. */
interface PendingPaths {
  [REGENERATE_PATH]: {
    parameters: { query?: never; header?: never; path?: never; cookie?: never };
    get?: never;
    put?: never;
    post: {
      parameters: { query?: never; header?: never; path: { conversation_id: string }; cookie?: never };
      requestBody: { content: { "application/json": { answer: number } } };
      responses: {
        202: {
          headers: { [name: string]: unknown };
          content: { "application/json": components["schemas"]["ConversationRun"] };
        };
        400: { headers: { [name: string]: unknown }; content: { "application/json": ErrorBody } };
        401: { headers: { [name: string]: unknown }; content: { "application/json": ErrorBody } };
        403: { headers: { [name: string]: unknown }; content: { "application/json": ErrorBody } };
        404: { headers: { [name: string]: unknown }; content: { "application/json": ErrorBody } };
        409: { headers: { [name: string]: unknown }; content: { "application/json": ErrorBody } };
        429: { headers: { [name: string]: unknown }; content: { "application/json": ErrorBody } };
        503: { headers: { [name: string]: unknown }; content: { "application/json": ErrorBody } };
        504: { headers: { [name: string]: unknown }; content: { "application/json": ErrorBody } };
      };
    };
    delete?: never;
    options?: never;
    head?: never;
    patch?: never;
    trace?: never;
  };
}

/**
 * The regenerate endpoint over `client` (the real client, so every request names the page's user
 * and tenant like the others do). The generated types have no such path yet, so the client is
 * widened with `PendingPaths`: the same object, the same middleware.
 */
export function regenerateApi(client: ApiClient): AnswerRegenerator {
  return pinnable(client, (client) => {
    const pending = client as unknown as Client<paths & PendingPaths>;
    return {
      regenerate: (conversationId, answer) =>
        unwrap(
          ConversationRunSchema,
          pending.POST(REGENERATE_PATH, {
            params: { path: { conversation_id: conversationId } },
            body: { answer },
          }),
        ),
    };
  });
}
