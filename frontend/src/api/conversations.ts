import type { AllTrue, Assert, Equal } from "../lib/type-binding.ts";
import { z } from "../lib/zod.ts";
import type { ApiClient } from "./client.ts";
import { pinnable, unwrap } from "./client.ts";
import type { components } from "./schema.gen.ts";

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
// latest question as its run stands (Laysi/trustiot-v2#646, 「提問」).
const RemovedKindSchema = z.enum(["code", "html", "sql"]);
const MessageSchema = z.object({
  kind: z.enum(["question", "answer"]),
  text: z.string(),
  removed: Optional(z.array(RemovedKindSchema)),
});
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
  messages: z.array(MessageSchema),
  latest_run: Optional(RunSchema),
});
const DeletedSchema = z.object({
  id: z.string(),
  title: Optional(z.string()),
  deleted_at: DateTime,
  restorable_until: DateTime,
});
const DeletedListSchema = z.object({ conversations: z.array(DeletedSchema), next_cursor: Optional(z.string()) });

export type ConversationBodiesMatchSpec = Assert<
  AllTrue<
    [
      Equal<z.output<typeof ConversationSchema>, Spec["Conversation"]>,
      Equal<z.output<typeof ConversationListSchema>, Spec["ConversationList"]>,
      Equal<z.output<typeof MessageSchema>, Spec["ConversationMessage"]>,
      Equal<z.output<typeof RunSchema>, Spec["ConversationRun"]>,
      Equal<z.output<typeof DetailSchema>, Spec["ConversationDetail"]>,
      Equal<z.output<typeof DeletedSchema>, Spec["DeletedConversation"]>,
      Equal<z.output<typeof DeletedListSchema>, Spec["DeletedConversationList"]>,
    ]
  >
>;

export type Conversation = z.output<typeof ConversationSchema>;
export type ConversationList = z.output<typeof ConversationListSchema>;
export type ConversationMessage = z.output<typeof MessageSchema>;
export type ConversationRun = z.output<typeof RunSchema>;
export type ConversationDetail = z.output<typeof DetailSchema>;
export type DeletedConversation = z.output<typeof DeletedSchema>;
export type DeletedConversationList = z.output<typeof DeletedListSchema>;

/** The largest page the lists give (the service's `limit` is 1 to 100). */
export const MAX_PAGE = 100;

/** The conversation endpoints the conversation pages use. */
export interface ConversationApi {
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
}

/** A page query: only what is set goes, since the server refuses a parameter without a value. */
function pageQuery(query: { cursor?: string; limit?: number }) {
  return {
    ...(query.cursor ? { cursor: query.cursor } : {}),
    ...(query.limit !== undefined ? { limit: query.limit } : {}),
  };
}

/** The conversation endpoints, through the typed client (`pinnable`: a page binds its own copy). */
export function conversationApi(client: ApiClient): ConversationApi {
  return pinnable(client, (client) => ({
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
  }));
}
