import { createApiClient } from "../api/client.ts";
import { type ConversationApi, conversationApi } from "../api/conversations.ts";
import type { components } from "../api/schema.gen.ts";
import { fakeError } from "./fake-dashboards.ts";
import { fakeUuid } from "./fake-devices.ts";

/**
 * The assistant's conversation API (services/assistant/README.md 「對話」), held in memory and
 * answered at the HTTP level: the pages go through the real client (`conversationApi`, zod
 * parsing, the query string and the JSON bodies), and this fake answers with the bodies
 * api/openapi.json describes, by the service's rules (services/assistant/src/api/conversations.rs):
 *
 * - The lists are the caller's conversations, the most recently active first (by time, then ID),
 *   archived ones only with `archived=true`; the deleted list the most recently deleted first,
 *   each restorable for 30 days. `limit` is 1 to 100 (default 50), and a page has a `next_cursor`
 *   when more follow.
 * - A deleted conversation is 404 `not_found` for every request but restore.
 * - A rename trims the title and refuses an empty one, one over 200 characters or one with a
 *   control or text-direction character (422 `conversation.title_invalid`, field `/title`); a
 *   body with neither field is 400 `conversation.update_empty`.
 * - Deleting a conversation whose question is running (`running`) is 409
 *   `conversation.question_running`, and nothing changes.
 *
 * Every request is recorded. `failNext` answers the next request to an endpoint with an error;
 * `hold` keeps requests to the endpoints named waiting until `release`.
 */

type Schemas = components["schemas"];
type Endpoint = "list" | "deleted" | "get" | "update" | "delete" | "restore";

export interface FakeConversation {
  id: string;
  title?: string;
  title_source?: Schemas["ConversationTitleSource"];
  created_at: string;
  last_activity_at: string;
  archived_at?: string;
  deleted_at?: string;
  messages: Schemas["ConversationMessage"][];
  /** A question of it is being answered: it cannot be deleted. */
  running?: boolean;
}

/** One recorded request. */
export interface FakeConversationRequest {
  method: string;
  url: URL;
  body: unknown;
}

const RESTORE_DAYS = 30;
const DAY_MS = 24 * 60 * 60 * 1000;

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

const notFound = () => fakeError(404, "not_found", "error.not_found");

/** The service's title rule (`store::title_is_valid`): visible text, no control or direction characters. */
function titleIsValid(title: string): boolean {
  if (title.length === 0 || [...title].length > 200) return false;
  if (!/[^\s\p{Cf}]/u.test(title)) return false;
  return !/[\p{Cc}\u2028\u2029\u202a-\u202e\u2066-\u2069]/u.test(title);
}

export class FakeConversationServer {
  readonly conversations = new Map<string, FakeConversation>();
  readonly requests: FakeConversationRequest[] = [];
  /** The server's clock, as an ISO time. */
  now: () => string = () => new Date().toISOString();
  private readonly failures = new Map<Endpoint, Response[]>();
  private held: (() => void)[] | null = null;
  private holding: readonly Endpoint[] = [];

  /** A conversation with these messages; the latest activity is `activityAt`. Returns it. */
  add(
    options: {
      title?: string | null;
      titleSource?: Schemas["ConversationTitleSource"];
      createdAt?: string;
      activityAt?: string;
      archivedAt?: string;
      deletedAt?: string;
      messages?: Schemas["ConversationMessage"][];
    } = {},
  ): FakeConversation {
    const created = options.createdAt ?? "2026-11-01T01:00:00Z";
    const title = options.title === undefined ? "Energy this week" : options.title;
    const conversation: FakeConversation = {
      id: fakeUuid(),
      ...(title !== null ? { title, title_source: options.titleSource ?? "user" } : {}),
      created_at: created,
      last_activity_at: options.activityAt ?? created,
      ...(options.archivedAt ? { archived_at: options.archivedAt } : {}),
      ...(options.deletedAt ? { deleted_at: options.deletedAt } : {}),
      messages: options.messages ?? [
        { kind: "question", text: "How did floor 3 use energy this week?" },
        { kind: "answer", text: "Floor 3 used **412 kWh** this week, 6% less than last week." },
      ],
    };
    this.conversations.set(conversation.id, conversation);
    return conversation;
  }

  /** The next request to `endpoint` gets `response`. */
  failNext(endpoint: Endpoint, response: Response): void {
    this.failures.set(endpoint, [...(this.failures.get(endpoint) ?? []), response]);
  }

  /** Requests to `endpoints` wait from now on until `release`. */
  hold(endpoints: readonly Endpoint[]): void {
    this.held ??= [];
    this.holding = endpoints;
  }

  /** Lets every waiting request answer, and stops holding new ones. */
  release(): void {
    const waiting = this.held ?? [];
    this.held = null;
    this.holding = [];
    for (const resume of waiting) resume();
  }

  /** The requests to the list endpoints so far, as their query strings. */
  listRequests(): string[] {
    return this.requests
      .filter((request) => request.method === "GET" && /conversations$/.test(request.url.pathname))
      .map((request) => `${request.url.pathname}${request.url.search}`);
  }

  /** The conversation endpoints, through the real client, against this server. */
  api(): ConversationApi {
    return conversationApi(createApiClient({ fetch: (request) => this.fetch(request) }));
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const text = request.method === "GET" ? "" : await request.text();
    const body: unknown = text === "" ? undefined : JSON.parse(text);
    this.requests.push({ method: request.method, url, body });
    const route = this.route(request.method, url.pathname);
    if (route === null) return notFound();
    await this.wait(route.endpoint, request.signal);
    const failure = this.failures.get(route.endpoint)?.shift();
    if (failure) return failure;
    switch (route.endpoint) {
      case "list":
        return this.list(url.searchParams);
      case "deleted":
        return this.deleted(url.searchParams);
      case "get":
        return this.get(route.id);
      case "update":
        return this.update(route.id, body);
      case "delete":
        return this.remove(route.id);
      case "restore":
        return this.restore(route.id);
    }
  }

  private async wait(endpoint: Endpoint, signal: AbortSignal): Promise<void> {
    const held = this.held;
    if (held === null || !this.holding.includes(endpoint)) return;
    await new Promise<void>((resume, reject) => {
      held.push(resume);
      signal.addEventListener("abort", () => reject(signal.reason), { once: true });
    });
  }

  private route(method: string, path: string): { endpoint: Endpoint; id: string } | null {
    if (path === "/api/assistant/v1/conversations" && method === "GET") return { endpoint: "list", id: "" };
    if (path === "/api/assistant/v1/deleted-conversations" && method === "GET") return { endpoint: "deleted", id: "" };
    const match = /^\/api\/assistant\/v1\/conversations\/([^/]+)(\/restore)?$/.exec(path);
    if (match === null) return null;
    const id = decodeURIComponent(match[1] as string);
    if (match[2] === "/restore") return method === "POST" ? { endpoint: "restore", id } : null;
    const endpoints: Record<string, Endpoint> = { GET: "get", PATCH: "update", DELETE: "delete" };
    const endpoint = endpoints[method];
    return endpoint ? { endpoint, id } : null;
  }

  private summary(conversation: FakeConversation): Schemas["Conversation"] {
    return {
      id: conversation.id,
      title: conversation.title ?? null,
      title_source: conversation.title_source ?? null,
      created_at: conversation.created_at,
      last_activity_at: conversation.last_activity_at,
      archived_at: conversation.archived_at ?? null,
    };
  }

  private visible(id: string): FakeConversation | undefined {
    const conversation = this.conversations.get(id);
    return conversation?.deleted_at === undefined ? conversation : undefined;
  }

  /** A page of `all` from the query's cursor, or the error for a bad `limit` or `cursor`. */
  private page<T>(all: T[], query: URLSearchParams): { items: T[]; next: string | null } | Response {
    const limit = query.has("limit") ? Number(query.get("limit")) : 50;
    if (!Number.isInteger(limit) || limit < 1 || limit > 100) {
      return fakeError(400, "invalid_request", "conversation.limit_invalid", { max: 100 });
    }
    const start = query.has("cursor") ? Number(query.get("cursor")) : 0;
    if (!Number.isInteger(start) || start < 0) return fakeError(400, "invalid_request", "conversation.cursor_invalid");
    const next = start + limit < all.length ? String(start + limit) : null;
    return { items: all.slice(start, start + limit), next };
  }

  private list(query: URLSearchParams): Response {
    const archived = query.get("archived") === "true";
    const all = [...this.conversations.values()]
      .filter((item) => item.deleted_at === undefined && (item.archived_at !== undefined) === archived)
      .sort((a, b) => b.last_activity_at.localeCompare(a.last_activity_at) || b.id.localeCompare(a.id));
    const page = this.page(all, query);
    if (page instanceof Response) return page;
    return json(200, { conversations: page.items.map((item) => this.summary(item)), next_cursor: page.next });
  }

  private deleted(query: URLSearchParams): Response {
    const all = [...this.conversations.values()]
      .filter((item) => item.deleted_at !== undefined)
      .sort((a, b) => (b.deleted_at ?? "").localeCompare(a.deleted_at ?? "") || b.id.localeCompare(a.id));
    const page = this.page(all, query);
    if (page instanceof Response) return page;
    return json(200, {
      conversations: page.items.map((item) => ({
        id: item.id,
        title: item.title ?? null,
        deleted_at: item.deleted_at,
        restorable_until: new Date(Date.parse(item.deleted_at ?? "") + RESTORE_DAYS * DAY_MS).toISOString(),
      })),
      next_cursor: page.next,
    });
  }

  private get(id: string): Response {
    const conversation = this.visible(id);
    if (conversation === undefined) return notFound();
    return json(200, { ...this.summary(conversation), messages: conversation.messages });
  }

  private update(id: string, body: unknown): Response {
    const conversation = this.visible(id);
    if (conversation === undefined) return notFound();
    const change = (body ?? {}) as { title?: unknown; archived?: unknown };
    if (typeof change !== "object" || Array.isArray(change)) {
      return fakeError(400, "invalid_request", "conversation.body_invalid");
    }
    if ((change.title ?? null) === null && (change.archived ?? null) === null) {
      return fakeError(400, "invalid_request", "conversation.update_empty");
    }
    if (typeof change.title === "string") {
      const title = change.title.trim();
      if (!titleIsValid(title)) {
        return json(422, {
          code: "validation_failed",
          message_key: "error.validation_failed",
          field_errors: [{ field: "/title", message_key: "conversation.title_invalid", details: { max: 200 } }],
        });
      }
      conversation.title = title;
      conversation.title_source = "user";
    }
    if (change.archived === true) conversation.archived_at ??= this.now();
    if (change.archived === false) delete conversation.archived_at;
    return json(200, this.summary(conversation));
  }

  private remove(id: string): Response {
    const conversation = this.visible(id);
    if (conversation === undefined) return notFound();
    if (conversation.running) return fakeError(409, "conflict", "conversation.question_running");
    conversation.deleted_at = this.now();
    return new Response(null, { status: 204 });
  }

  private restore(id: string): Response {
    const conversation = this.conversations.get(id);
    if (conversation?.deleted_at === undefined) return notFound();
    delete conversation.deleted_at;
    return json(200, this.summary(conversation));
  }
}
