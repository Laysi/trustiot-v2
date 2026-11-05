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
 * - Deleting a conversation whose question is running (`running`) stops the question and
 *   deletes the conversation (204), as the service does since Laysi/trustiot-v2#646.
 *
 * - Sharing (Laysi/trustiot-v2#650, services/assistant/src/api/sharing.ts): only the owner shares
 *   and unshares (`PUT` and `DELETE .../share`; sharing again keeps the first time), anyone else
 *   and a deleted conversation get 404; deleting a conversation unshares it, restoring it does not
 *   share it again. Other members' shared, not deleted conversations are listed and read at
 *   `/shared-conversations` (the caller's own never are: 404); a copy (`POST .../copy`) is a new
 *   conversation of the caller's from an own or a shared one, with the source's title and its
 *   questions and answers (what the answer guard removed stays), not shared, not archived, and it
 *   stays when the source is unshared or deleted. A conversation another member owns is added
 *   with `owner`, and `api(user)` is the API of that member over the same store.
 *
 * Every request is recorded. `failNext` answers the next request to an endpoint with an error;
 * `hold` keeps requests to the endpoints named waiting until `release`.
 */

type Schemas = components["schemas"];
type Endpoint =
  | "list"
  | "deleted"
  | "get"
  | "update"
  | "delete"
  | "restore"
  | "share"
  | "unshare"
  | "listShared"
  | "getShared"
  | "copy";

/** The signed-in user of the tests: the owner of a conversation unless `add` names another. */
export const FAKE_USER_ID = "00000000-0000-4000-8000-00000000f001";

export interface FakeConversation {
  id: string;
  title?: string;
  title_source?: Schemas["ConversationTitleSource"];
  created_at: string;
  last_activity_at: string;
  archived_at?: string;
  deleted_at?: string;
  /** When its owner shared it with the tenant. */
  shared_at?: string;
  /** The owner's user ID. */
  owner: string;
  messages: Schemas["ConversationMessage"][];
  /** A question of it is being answered; deleting the conversation stops it. */
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
/** Where the fake resolves a path the answer stream fetches (the page's fetch resolves it against the page). */
const FAKE_ORIGIN = "http://localhost/";

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
      sharedAt?: string;
      /** The owner's user ID: another member's conversation is not the signed-in user's. */
      owner?: string;
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
      ...(options.sharedAt ? { shared_at: options.sharedAt } : {}),
      owner: options.owner ?? FAKE_USER_ID,
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
      .filter((request) => request.method === "GET" && /\/v1\/conversations$/.test(request.url.pathname))
      .map((request) => `${request.url.pathname}${request.url.search}`);
  }

  /** The conversation endpoints, through the real client, against this server, as `user` (the signed-in user by default). */
  api(user: string = FAKE_USER_ID): ConversationApi {
    // The signed-in user's requests go through `fetch`, which a test may wrap (a network failure, say).
    const call = (request: Request) => (user === FAKE_USER_ID ? this.fetch(request) : this.fetchAs(user, request));
    return conversationApi(
      createApiClient({ fetch: call }),
      // The answer stream calls fetch with a path and init, as the page's own fetch is called.
      (url, init) => call(new Request(new URL(url, FAKE_ORIGIN), init)),
    );
  }

  fetch(request: Request): Promise<Response> {
    return this.fetchAs(FAKE_USER_ID, request);
  }

  /** Answers `request` as member `user` of the tenant. */
  async fetchAs(user: string, request: Request): Promise<Response> {
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
        return this.list(user, url.searchParams);
      case "deleted":
        return this.deleted(user, url.searchParams);
      case "get":
        return this.get(user, route.id);
      case "update":
        return this.update(user, route.id, body);
      case "delete":
        return this.remove(user, route.id);
      case "restore":
        return this.restore(user, route.id);
      case "share":
        return this.share(user, route.id, true);
      case "unshare":
        return this.share(user, route.id, false);
      case "listShared":
        return this.listShared(user, url.searchParams);
      case "getShared":
        return this.getShared(user, route.id);
      case "copy":
        return this.copy(user, route.id);
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
    if (path === "/api/assistant/v1/shared-conversations" && method === "GET")
      return { endpoint: "listShared", id: "" };
    const shared = /^\/api\/assistant\/v1\/shared-conversations\/([^/]+)$/.exec(path);
    if (shared !== null) {
      return method === "GET" ? { endpoint: "getShared", id: decodeURIComponent(shared[1] as string) } : null;
    }
    const match = /^\/api\/assistant\/v1\/conversations\/([^/]+)(\/restore|\/share|\/copy)?$/.exec(path);
    if (match === null) return null;
    const id = decodeURIComponent(match[1] as string);
    if (match[2] === "/restore") return method === "POST" ? { endpoint: "restore", id } : null;
    if (match[2] === "/copy") return method === "POST" ? { endpoint: "copy", id } : null;
    if (match[2] === "/share") {
      if (method === "PUT") return { endpoint: "share", id };
      return method === "DELETE" ? { endpoint: "unshare", id } : null;
    }
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
      shared_at: conversation.shared_at ?? null,
    };
  }

  /** The caller's own conversation `id` that is not deleted; another member's is the same as none. */
  private visible(user: string, id: string): FakeConversation | undefined {
    const conversation = this.conversations.get(id);
    return conversation?.deleted_at === undefined && conversation?.owner === user ? conversation : undefined;
  }

  /** A conversation another member shared with the tenant, not deleted: what the shared endpoints give. */
  private sharedWith(user: string, id: string): FakeConversation | undefined {
    const conversation = this.conversations.get(id);
    return conversation !== undefined &&
      conversation.owner !== user &&
      conversation.shared_at !== undefined &&
      conversation.deleted_at === undefined
      ? conversation
      : undefined;
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

  private list(user: string, query: URLSearchParams): Response {
    const archived = query.get("archived") === "true";
    const all = [...this.conversations.values()]
      .filter(
        (item) => item.owner === user && item.deleted_at === undefined && (item.archived_at !== undefined) === archived,
      )
      .sort((a, b) => b.last_activity_at.localeCompare(a.last_activity_at) || b.id.localeCompare(a.id));
    const page = this.page(all, query);
    if (page instanceof Response) return page;
    return json(200, { conversations: page.items.map((item) => this.summary(item)), next_cursor: page.next });
  }

  private deleted(user: string, query: URLSearchParams): Response {
    const all = [...this.conversations.values()]
      .filter((item) => item.owner === user && item.deleted_at !== undefined)
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

  private get(user: string, id: string): Response {
    const conversation = this.visible(user, id);
    if (conversation === undefined) return notFound();
    // The owner's view gives each message its stored position (Laysi/trustiot-v2#649); here
    // every message is one stored message after the other, with no hidden steps between them.
    const messages = conversation.messages.map((message, seq) => ({ ...message, seq }));
    return json(200, { ...this.summary(conversation), messages });
  }

  private update(user: string, id: string, body: unknown): Response {
    const conversation = this.visible(user, id);
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

  private remove(user: string, id: string): Response {
    const conversation = this.visible(user, id);
    if (conversation === undefined) return notFound();
    // Since Laysi/trustiot-v2#646 a delete stops the conversation's running question.
    if (conversation.running) conversation.running = false;
    conversation.deleted_at = this.now();
    // Deleting unshares, and restoring does not share it again.
    delete conversation.shared_at;
    return new Response(null, { status: 204 });
  }

  private restore(user: string, id: string): Response {
    const conversation = this.conversations.get(id);
    if (conversation?.deleted_at === undefined || conversation.owner !== user) return notFound();
    delete conversation.deleted_at;
    return json(200, this.summary(conversation));
  }

  private share(user: string, id: string, shared: boolean): Response {
    const conversation = this.visible(user, id);
    if (conversation === undefined) return notFound();
    if (shared) conversation.shared_at ??= this.now();
    else delete conversation.shared_at;
    return json(200, this.summary(conversation));
  }

  private listShared(user: string, query: URLSearchParams): Response {
    const all = [...this.conversations.values()]
      .filter((item) => this.sharedWith(user, item.id) !== undefined)
      .sort((a, b) => b.last_activity_at.localeCompare(a.last_activity_at) || b.id.localeCompare(a.id));
    const page = this.page(all, query);
    if (page instanceof Response) return page;
    return json(200, {
      conversations: page.items.map((item) => ({
        id: item.id,
        title: item.title ?? null,
        owner_user_id: item.owner,
        shared_at: item.shared_at,
        last_activity_at: item.last_activity_at,
      })),
      next_cursor: page.next,
    });
  }

  private getShared(user: string, id: string): Response {
    const conversation = this.sharedWith(user, id);
    if (conversation === undefined) return notFound();
    return json(200, {
      id: conversation.id,
      title: conversation.title ?? null,
      owner_user_id: conversation.owner,
      shared_at: conversation.shared_at,
      last_activity_at: conversation.last_activity_at,
      messages: conversation.messages,
    });
  }

  private copy(user: string, id: string): Response {
    const source = this.visible(user, id) ?? this.sharedWith(user, id);
    if (source === undefined) return notFound();
    const now = this.now();
    const copy = this.add({
      title: source.title ?? null,
      titleSource: "user",
      createdAt: now,
      activityAt: now,
      owner: user,
      messages: source.messages.map((message) => ({ ...message })),
    });
    return json(201, this.summary(copy));
  }
}

/** The sharing endpoints' prefix: the conversations preview (fake-questions.ts) declares it next to the conversation paths. */
export const SHARED_CONVERSATIONS_PREVIEW_PREFIX = "/api/assistant/v1/shared-conversations";

/**
 * What the UI preview adds for sharing (Laysi/trustiot-v2#1048): one of the user's own
 * conversations that is shared with the tenant, and three that other members shared (the
 * "Shared with me" tab and the read-only page; copying one adds to the user's own). The routes
 * are the server class's, so any server built on `FakeConversationServer` answers them.
 */
export function seedSharedPreviewConversations(server: FakeConversationServer): void {
  const ago = (hours: number) => new Date(Date.now() - hours * 3_600_000).toISOString();
  server.add({
    title: "Weekly energy report draft",
    createdAt: ago(100),
    activityAt: ago(90),
    sharedAt: ago(72),
  });
  server.add({
    title: "Boiler room energy",
    owner: fakeUuid(),
    sharedAt: ago(20),
    createdAt: ago(40),
    activityAt: ago(5),
    messages: [
      { kind: "question", text: "How much energy did the boiler room use yesterday?" },
      {
        kind: "answer",
        text: "The boiler room used **88 kWh** yesterday. The [dashboards](/dashboards) show the trend by hour.",
      },
    ],
  });
  server.add({
    title: "Cooling tower alarms last week",
    owner: fakeUuid(),
    sharedAt: ago(60),
    createdAt: ago(70),
    activityAt: ago(48),
    messages: [
      { kind: "question", text: "Which cooling tower alarmed most last week?" },
      { kind: "answer", text: "Tower 2 alarmed **7 times**, mostly on Thursday afternoon." },
    ],
  });
  server.add({ title: null, owner: fakeUuid(), sharedAt: ago(150), createdAt: ago(160), activityAt: ago(155) });
}

/**
 * The UI preview's conversations: a few active ones, one archived and one deleted, timed back from
 * now so the lists read as recent; `empty` adds none. The preview's own entry is in
 * fake-questions.ts, which answers these paths too (one owner per path prefix), so it calls this.
 */
export function seedPreviewConversations(server: FakeConversationServer, seed: "normal" | "empty"): void {
  if (seed === "normal") {
    const ago = (hours: number) => new Date(Date.now() - hours * 3_600_000).toISOString();
    server.add({
      title: "Line 2 energy this week",
      createdAt: ago(30),
      activityAt: ago(1),
      messages: [
        { kind: "question", text: "How much energy did line 2 use this week?" },
        {
          kind: "answer",
          text: "Line 2 used **1,284 kWh** this week, 4% less than last week. Most of it was on Tuesday and Wednesday.",
        },
      ],
    });
    server.add({
      title: "Cold room temperature alarms",
      createdAt: ago(52),
      activityAt: ago(26),
      messages: [
        { kind: "question", text: "Did the cold room go above 6 °C yesterday?" },
        { kind: "answer", text: "Yes, twice: for 12 minutes at 14:05 and for 7 minutes at 16:40." },
      ],
    });
    server.add({ title: null, createdAt: ago(80), activityAt: ago(79) });
    server.add({
      title: "Compressor maintenance plan",
      createdAt: ago(240),
      activityAt: ago(200),
      archivedAt: ago(150),
    });
    server.add({ title: "Test question", createdAt: ago(300), activityAt: ago(300), deletedAt: ago(48) });
  }
}
