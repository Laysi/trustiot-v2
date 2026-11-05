import type { AllTrue, Assert, Equal } from "../lib/type-binding.ts";
import { z } from "../lib/zod.ts";
import type { ApiClient } from "./client.ts";
import { pinnable, unwrap } from "./client.ts";
import type { components, paths } from "./schema.gen.ts";
import { type EventStream, openEventStream, type StreamFetch, type StreamOptions } from "./sse.ts";

type Spec = components["schemas"];

// The assistant's conversation API (services/assistant/README.md 「對話」): the conversation
// history and one conversation's questions and answers. Each schema is bound to the type
// generated from api/openapi.json (lib/type-binding.ts), so a change in the contract fails
// `typecheck` here until the schema follows.

const DateTime = z.string();
const Optional = <T extends z.ZodType>(schema: T) => schema.nullable().optional();

const TitleSourceSchema = z.enum(["provisional", "user", "model"]);
const ConversationSchema = z.object({
  id: z.string(),
  title: Optional(z.string()),
  title_source: Optional(TitleSourceSchema),
  created_at: DateTime,
  last_activity_at: DateTime,
  archived_at: Optional(DateTime),
  shared_at: Optional(DateTime),
});
const ConversationListSchema = z.object({
  conversations: z.array(ConversationSchema),
  next_cursor: Optional(z.string()),
});
// What the answer guard removed from an answer (absent when nothing), and the conversation's
// latest question as its run stands (Laysi/trustiot-v2#646, 「提問」). On an answer, the draft
// dashboard versions its question wrote, in the order written (Laysi/trustiot-v2#1043; absent on
// a question).
const RemovedKindSchema = z.enum(["code", "html", "sql"]);
const AnswerDraftVersionSchema = z.object({ dashboard_id: z.string(), version: z.number().int() });
const MessageSchema = z.object({
  kind: z.enum(["question", "answer"]),
  text: z.string(),
  removed: Optional(z.array(RemovedKindSchema)),
  draft_versions: Optional(z.array(AnswerDraftVersionSchema)),
});
// The caller's own conversation names each message by its position, which the regenerate request
// uses to name the answer (Laysi/trustiot-v2#649, pending: api/openapi.json has no `seq` yet, so it
// is optional here until the spec has it; a shared conversation's readers never get one).
const OwnMessageSchema = MessageSchema.extend({ seq: z.number().int().optional() });
const RunSchema = z.object({
  id: z.string(),
  status: z.enum(["running", "answered", "failed", "cancelled", "interrupted"]),
  result: Optional(z.string()),
  message_key: Optional(z.string()),
  started_at: DateTime,
  deadline_at: DateTime,
  ended_at: Optional(DateTime),
  stop_requested_at: Optional(DateTime),
});
const DetailSchema = z.object({
  id: z.string(),
  title: Optional(z.string()),
  title_source: Optional(TitleSourceSchema),
  created_at: DateTime,
  last_activity_at: DateTime,
  archived_at: Optional(DateTime),
  shared_at: Optional(DateTime),
  messages: z.array(OwnMessageSchema),
  latest_run: Optional(RunSchema),
});
const DeletedSchema = z.object({
  id: z.string(),
  title: Optional(z.string()),
  deleted_at: DateTime,
  restorable_until: DateTime,
});
const DeletedListSchema = z.object({ conversations: z.array(DeletedSchema), next_cursor: Optional(z.string()) });
// What other members shared (Laysi/trustiot-v2#650): the list's rows, and the read-only view with
// the owner's questions and answers. The owner is a user ID: the API has no name lookup.
const SharedSchema = z.object({
  id: z.string(),
  title: Optional(z.string()),
  owner_user_id: z.string(),
  shared_at: DateTime,
  last_activity_at: DateTime,
});
const SharedListSchema = z.object({ conversations: z.array(SharedSchema), next_cursor: Optional(z.string()) });
const SharedDetailSchema = z.object({
  id: z.string(),
  title: Optional(z.string()),
  owner_user_id: z.string(),
  shared_at: DateTime,
  last_activity_at: DateTime,
  messages: z.array(MessageSchema),
});

export type ConversationBodiesMatchSpec = Assert<
  AllTrue<
    [
      Equal<z.output<typeof ConversationSchema>, Spec["Conversation"]>,
      Equal<z.output<typeof ConversationListSchema>, Spec["ConversationList"]>,
      Equal<z.output<typeof AnswerDraftVersionSchema>, Spec["AnswerDraftVersion"]>,
      Equal<z.output<typeof MessageSchema>, Spec["ConversationMessage"]>,
      Equal<z.output<typeof RunSchema>, Spec["ConversationRun"]>,
      Equal<Omit<z.output<typeof OwnMessageSchema>, "seq">, Omit<Spec["ConversationMessage"], "seq">>,
      Equal<Omit<z.output<typeof DetailSchema>, "messages">, Omit<Spec["ConversationDetail"], "messages">>,
      Equal<z.output<typeof DeletedSchema>, Spec["DeletedConversation"]>,
      Equal<z.output<typeof DeletedListSchema>, Spec["DeletedConversationList"]>,
      Equal<z.output<typeof SharedSchema>, Spec["SharedConversation"]>,
      Equal<z.output<typeof SharedListSchema>, Spec["SharedConversationList"]>,
      Equal<z.output<typeof SharedDetailSchema>, Spec["SharedConversationDetail"]>,
    ]
  >
>;

export type Conversation = z.output<typeof ConversationSchema>;
export type ConversationList = z.output<typeof ConversationListSchema>;
export type ConversationMessage = z.output<typeof MessageSchema>;
export type ConversationRun = z.output<typeof RunSchema>;
/** A run as the API gives it (the answer stream's `end` event carries one). */
export const ConversationRunSchema = RunSchema;
export type ConversationDetail = z.output<typeof DetailSchema>;
/** A message of the caller's own conversation: `seq` is its position, once the API gives it. */
export type OwnConversationMessage = z.output<typeof OwnMessageSchema>;
export type DeletedConversation = z.output<typeof DeletedSchema>;
export type DeletedConversationList = z.output<typeof DeletedListSchema>;
export type SharedConversation = z.output<typeof SharedSchema>;
export type SharedConversationList = z.output<typeof SharedListSchema>;
export type SharedConversationDetail = z.output<typeof SharedDetailSchema>;

/** The largest page the lists give (the service's `limit` is 1 to 100). */
export const MAX_PAGE = 100;

/** The conversation endpoints the conversation pages use. */
export interface ConversationApi {
  /** Starts an empty conversation for the caller (the home page's composer). */
  createConversation(): Promise<Conversation>;
  /**
   * Asks a question in a conversation; the answer is worked out in the background (202), so this
   * returns the run, which the conversation's `latest_run` follows until it is no longer running.
   */
  askQuestion(conversationId: string, question: string): Promise<ConversationRun>;
  /** Asks to stop a running question; the run it returns is still `running` until it has stopped. */
  stopQuestion(conversationId: string, runId: string): Promise<ConversationRun>;
  /**
   * Opens the answer stream of a question (server-sent events, `GET .../runs/{run_id}/stream`,
   * Laysi/trustiot-v2#147): `heartbeat`, `text` and `end` events, reconnecting as
   * `openEventStream` does (api/answer-stream.ts reads them).
   */
  streamAnswer(conversationId: string, runId: string, options: Omit<StreamOptions, "fetch">): EventStream;
  /** One page of the caller's conversations, the most recently active first; archived ones only with `archived`. */
  listConversations(
    query: { archived: boolean; cursor?: string; limit?: number },
    signal?: AbortSignal,
  ): Promise<ConversationList>;
  /** One page of the caller's deleted conversations that can still be restored, the most recently deleted first. */
  listDeleted(query: { cursor?: string; limit?: number }, signal?: AbortSignal): Promise<DeletedConversationList>;
  /** A conversation with its questions and answers, oldest first. */
  getConversation(conversationId: string, signal?: AbortSignal): Promise<ConversationDetail>;
  /** Renames, archives or unarchives a conversation (at least one of the two). */
  updateConversation(conversationId: string, change: { title?: string; archived?: boolean }): Promise<Conversation>;
  /** Deletes a conversation; it can be restored for 30 days. */
  deleteConversation(conversationId: string): Promise<void>;
  /** Restores a deleted conversation. */
  restoreConversation(conversationId: string): Promise<Conversation>;
  /** Shares the caller's conversation with every member of the tenant; sharing again keeps the first time. */
  shareConversation(conversationId: string): Promise<Conversation>;
  /** Stops sharing it; copies that already exist stay. */
  unshareConversation(conversationId: string): Promise<Conversation>;
  /** One page of the conversations other members shared, the most recently active first. */
  listShared(query: { cursor?: string; limit?: number }, signal?: AbortSignal): Promise<SharedConversationList>;
  /** A shared conversation, read only: its questions and answers, oldest first. */
  getShared(conversationId: string, signal?: AbortSignal): Promise<SharedConversationDetail>;
  /** Copies the caller's own or a shared conversation into a new conversation of the caller's. */
  copyConversation(conversationId: string): Promise<Conversation>;
}

/** A page query: only what is set goes, since the server refuses a parameter without a value. */
function pageQuery(query: { cursor?: string; limit?: number }) {
  return {
    ...(query.cursor ? { cursor: query.cursor } : {}),
    ...(query.limit !== undefined ? { limit: query.limit } : {}),
  };
}

/** The answer stream's path in the spec; the stream is read with `openEventStream`, not the typed client. */
const ANSWER_STREAM_PATH: keyof paths = "/api/assistant/v1/conversations/{conversation_id}/runs/{run_id}/stream";

/** The answer stream's address of one question. */
export function answerStreamUrl(conversationId: string, runId: string): string {
  return ANSWER_STREAM_PATH.replace("{conversation_id}", encodeURIComponent(conversationId)).replace(
    "{run_id}",
    encodeURIComponent(runId),
  );
}

/**
 * The conversation endpoints, through the typed client (`pinnable`: a page binds its own copy).
 * The answer stream goes through `streamFetch` (the page's `fetch` when absent): a stream names
 * the page's user and tenant through its own `session`, not through the client.
 */
export function conversationApi(client: ApiClient, streamFetch?: StreamFetch): ConversationApi {
  return pinnable(client, (client) => ({
    createConversation: () =>
      unwrap(
        ConversationSchema,
        // Nothing to set: no body, with the JSON content type the client adds.
        client.POST("/api/assistant/v1/conversations"),
      ),
    askQuestion: (conversationId, question) =>
      unwrap(
        RunSchema,
        client.POST("/api/assistant/v1/conversations/{conversation_id}/questions", {
          params: { path: { conversation_id: conversationId } },
          body: { question },
        }),
      ),
    stopQuestion: (conversationId, runId) =>
      unwrap(
        RunSchema,
        // No settings to send: the body is empty, with the JSON content type the client adds.
        client.POST("/api/assistant/v1/conversations/{conversation_id}/runs/{run_id}/cancel", {
          params: { path: { conversation_id: conversationId, run_id: runId } },
        }),
      ),
    streamAnswer: (conversationId, runId, options) =>
      openEventStream(answerStreamUrl(conversationId, runId), {
        ...options,
        ...(streamFetch ? { fetch: streamFetch } : {}),
      }),
    listConversations: ({ archived, ...page }, signal) =>
      unwrap(
        ConversationListSchema,
        client.GET("/api/assistant/v1/conversations", {
          params: { query: { ...(archived ? { archived: true } : {}), ...pageQuery(page) } },
          ...(signal ? { signal } : {}),
        }),
      ),
    listDeleted: (page, signal) =>
      unwrap(
        DeletedListSchema,
        client.GET("/api/assistant/v1/deleted-conversations", {
          params: { query: pageQuery(page) },
          ...(signal ? { signal } : {}),
        }),
      ),
    getConversation: (conversationId, signal) =>
      unwrap(
        DetailSchema,
        client.GET("/api/assistant/v1/conversations/{conversation_id}", {
          params: { path: { conversation_id: conversationId } },
          ...(signal ? { signal } : {}),
        }),
      ),
    updateConversation: (conversationId, change) =>
      unwrap(
        ConversationSchema,
        client.PATCH("/api/assistant/v1/conversations/{conversation_id}", {
          params: { path: { conversation_id: conversationId } },
          body: change,
        }),
      ),
    deleteConversation: (conversationId) =>
      unwrap(
        z.undefined(),
        client.DELETE("/api/assistant/v1/conversations/{conversation_id}", {
          params: { path: { conversation_id: conversationId } },
        }),
      ),
    restoreConversation: (conversationId) =>
      unwrap(
        ConversationSchema,
        // No settings to send: the body is empty, with the JSON content type the client adds.
        client.POST("/api/assistant/v1/conversations/{conversation_id}/restore", {
          params: { path: { conversation_id: conversationId } },
        }),
      ),
    shareConversation: (conversationId) =>
      unwrap(
        ConversationSchema,
        client.PUT("/api/assistant/v1/conversations/{conversation_id}/share", {
          params: { path: { conversation_id: conversationId } },
        }),
      ),
    unshareConversation: (conversationId) =>
      unwrap(
        ConversationSchema,
        client.DELETE("/api/assistant/v1/conversations/{conversation_id}/share", {
          params: { path: { conversation_id: conversationId } },
        }),
      ),
    listShared: (page, signal) =>
      unwrap(
        SharedListSchema,
        client.GET("/api/assistant/v1/shared-conversations", {
          params: { query: pageQuery(page) },
          ...(signal ? { signal } : {}),
        }),
      ),
    getShared: (conversationId, signal) =>
      unwrap(
        SharedDetailSchema,
        client.GET("/api/assistant/v1/shared-conversations/{conversation_id}", {
          params: { path: { conversation_id: conversationId } },
          ...(signal ? { signal } : {}),
        }),
      ),
    copyConversation: (conversationId) =>
      unwrap(
        ConversationSchema,
        // No settings to send: the body is empty, with the JSON content type the client adds.
        client.POST("/api/assistant/v1/conversations/{conversation_id}/copy", {
          params: { path: { conversation_id: conversationId } },
        }),
      ),
  }));
}
