import { createApiClient } from "../api/client.ts";
import { type DashboardApi, type DatasetResult, dashboardApi } from "../api/dashboards.ts";
import type { components } from "../api/schema.gen.ts";
import { fixtureData, SPECS } from "../dashboard/fixtures.ts";
import { fakeUuid } from "./fake-devices.ts";

/**
 * The assistant's public dashboard API (services/assistant/README.md 「儀表板」), held in memory
 * and answered at the HTTP level: the pages go through the real client (`dashboardApi`, zod
 * parsing, the query string and the JSON bodies), and this fake answers with the bodies
 * api/openapi.json describes, by the service's rules:
 *
 * - The list is the saved dashboards, the most recently updated first, archived ones only with
 *   `include_archived=true`, `pageSize` to a page with a `next_cursor`.
 * - `GET /dashboards/{id}` is the latest version or `?version=`; one that does not exist is 404
 *   `not_found`, a missing version 404 `dashboard.version_not_found`. `permissions` follow the
 *   caller's `role`: an editor or admin may restore and archive a saved dashboard.
 * - Restore needs an editor (403 `dashboard.editor_required`), refuses an archived dashboard (409
 *   `dashboard.archived`), a `base_version` that is not the latest (409
 *   `dashboard.version_conflict`, `details.latest`) and a copy of the latest content (409
 *   `dashboard.restore_unchanged`); it adds a version whose content is `from_version`'s.
 * - A draft belongs to a conversation (`conversationId`): `GET /conversations/{id}/dashboards` lists
 *   that conversation's drafts, the most recently updated first. Publishing a version of a draft
 *   (`POST /dashboards/{id}/publish`) needs an editor (403 `dashboard.editor_required`), a draft
 *   (422 `dashboard.draft_required`) and an existing version (404 `dashboard.version_not_found`);
 *   the first time it makes a saved dashboard, later it adds a version to the draft's saved
 *   dashboard, refused when that one's latest version is not the one the published version is
 *   based on (409 `dashboard.saved_changed`, `details.latest`) or when it is archived (409
 *   `dashboard.archived`). A version is based on the saved version it was written on
 *   (`FakeVersion.base`), none before the first publish.
 * - A dataset run answers with the dataset's current result (`setResult`), so a test changes the
 *   data between two runs; a run of a dataset the version does not have is 422
 *   `dashboard.dataset_unknown`.
 *
 * Every request is recorded. `failNext` answers the next request to an endpoint (or the next run
 * of one dataset) with an error; `hold` keeps every dataset run waiting until `release`.
 */

type Schemas = components["schemas"];
type Spec = Schemas["DashboardSpec"];
export type FakeRole = "viewer" | "editor" | "admin";
type Endpoint = "list" | "dashboard" | "versions" | "run" | "restore" | "archive" | "drafts" | "publish";

/** One stored version; `spec` is unknown JSON, so a test can store an item this build does not know. */
export interface FakeVersion {
  version: number;
  title: string;
  description?: string;
  spec: Omit<Spec, "components"> & { components?: unknown[] };
  page?: string;
  runtime?: number;
  origin: Schemas["DashboardVersionOrigin"];
  restored_from?: number;
  /** A draft's version: the saved dashboard's version it was written on; none before the first publish. */
  base?: number;
  created_by: string;
  created_at: string;
}

export interface FakeDashboard {
  id: string;
  kind: Schemas["DashboardKind"];
  created_by: string;
  created_at: string;
  updated_at: string;
  archived_at?: string;
  /** A draft's conversation. */
  conversation_id?: string;
  /** A draft's saved dashboard, and the version of it the draft's next version starts from. */
  saved?: { id: string; version?: number };
  versions: FakeVersion[];
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

export function fakeError(status: number, code: string, messageKey: string, details?: Record<string, string | number>) {
  return json(status, { code, message_key: messageKey, ...(details ? { details } : {}) });
}

const notFound = () => fakeError(404, "not_found", "error.not_found");

/** The fixture's datasets, computations and constants with these components. */
export function fixtureSpec(components: unknown[]): FakeVersion["spec"] {
  const data = fixtureData();
  return {
    datasets: [...data.datasets],
    computations: [...data.computations],
    constants: [...data.constants],
    components,
  };
}

/** The fixture's result of each dataset, by name. */
export function fixtureResults(): Map<string, DatasetResult> {
  const results = new Map<string, DatasetResult>();
  for (const [name, state] of Object.entries(fixtureData().results)) {
    if (state.status === "ready") results.set(name, state.result);
  }
  return results;
}

/** One recorded request. */
export interface FakeRequest {
  method: string;
  url: URL;
  body: unknown;
}

export class FakeDashboardServer {
  readonly dashboards = new Map<string, FakeDashboard>();
  /** Every request, in order. */
  readonly requests: FakeRequest[] = [];
  /** The caller's role in the tenant. */
  role: FakeRole = "editor";
  /** The user the requests are from. */
  user = "00000000-0000-4000-8000-0000000000a1";
  /** The list's page size (the service's is 50). */
  pageSize = 50;
  /** The server's clock, as an ISO time. */
  now: () => string = () => new Date().toISOString();
  /** Each dataset's current result, by name; shared by every dashboard. */
  readonly results: Map<string, DatasetResult> = fixtureResults();
  /** Dataset runs in progress now, and the most there were at once. */
  running = 0;
  maxRunning = 0;
  private readonly failures = new Map<string, Response[]>();
  private held: (() => void)[] | null = null;
  private holding: readonly Endpoint[] = [];

  /** A saved dashboard (or a draft) with these versions' specs; returns it. */
  addDashboard(
    options: {
      title?: string;
      kind?: Schemas["DashboardKind"];
      updatedAt?: string;
      archivedAt?: string;
      /** A draft's conversation. */
      conversationId?: string;
      /** A draft's saved dashboard, and the version of it the draft's latest version is based on. */
      saved?: { id: string; version?: number };
      /** Each version's components, the first version first. */
      versions?: unknown[][];
      page?: string;
    } = {},
  ): FakeDashboard {
    const created = "2026-10-20T08:00:00Z";
    const updated = options.updatedAt ?? "2026-11-01T09:30:00Z";
    const componentsOfVersions = options.versions ?? [[SPECS.timeSeries, SPECS.kpi]];
    const dashboard: FakeDashboard = {
      id: fakeUuid(),
      kind: options.kind ?? "saved",
      created_by: this.user,
      created_at: created,
      updated_at: updated,
      ...(options.archivedAt ? { archived_at: options.archivedAt } : {}),
      ...(options.conversationId ? { conversation_id: options.conversationId } : {}),
      ...(options.saved ? { saved: options.saved } : {}),
      versions: componentsOfVersions.map((components, index) => ({
        version: index + 1,
        title: options.title ?? "Energy overview",
        spec: fixtureSpec(components),
        ...(options.page && components.some((item) => (item as { kind?: string }).kind === "custom_page")
          ? { page: options.page, runtime: 1 }
          : {}),
        origin: index === 0 && (options.kind ?? "saved") === "saved" ? "publish" : "agent",
        ...(index === componentsOfVersions.length - 1 && options.saved?.version !== undefined
          ? { base: options.saved.version }
          : {}),
        created_by: this.user,
        created_at: index === componentsOfVersions.length - 1 ? updated : created,
      })),
    };
    this.dashboards.set(dashboard.id, dashboard);
    return dashboard;
  }

  /** The result the next runs of `dataset` answer with. */
  setResult(dataset: string, rows: DatasetResult["rows"], options: { queriedAt?: string } = {}): void {
    const current = this.results.get(dataset);
    if (current === undefined) throw new Error(`no result for ${dataset}`);
    this.results.set(dataset, {
      ...current,
      rows,
      row_count: rows.length,
      meta: { ...current.meta, ...(options.queriedAt ? { queried_at: options.queriedAt } : {}) },
    });
  }

  /** The next request to `endpoint` gets `response`; `run:<dataset>` names the next run of one dataset. */
  failNext(endpoint: Endpoint | `run:${string}`, response: Response): void {
    this.failures.set(endpoint, [...(this.failures.get(endpoint) ?? []), response]);
  }

  /** Requests to `endpoints` (dataset runs unless named) wait from now on until `release`. */
  hold(endpoints: readonly Endpoint[] = ["run"]): void {
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

  /** The dataset runs so far, as `{version, dataset}` bodies. */
  runs(): { version: number; dataset: string }[] {
    return this.requests
      .filter((request) => request.url.pathname.endsWith("/dataset-runs"))
      .map((request) => request.body as { version: number; dataset: string });
  }

  /** The dashboard endpoints, through the real client, against this server. */
  api(): DashboardApi {
    return dashboardApi(createApiClient({ fetch: (request) => this.fetch(request) }));
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const text = request.method === "GET" ? "" : await request.text();
    const body: unknown = text === "" ? undefined : JSON.parse(text);
    this.requests.push({ method: request.method, url, body });
    const route = this.route(request.method, url.pathname);
    if (route === null) return notFound();
    if (route.endpoint !== "run") await this.wait(route.endpoint, request.signal);
    const failure = this.failures.get(route.endpoint)?.shift();
    if (failure) return failure;
    switch (route.endpoint) {
      case "list":
        return this.list(url.searchParams);
      case "dashboard":
        return this.view(route.id, url.searchParams.get("version"));
      case "versions":
        return this.versions(route.id);
      case "run":
        return this.run(route.id, body as { version: number; dataset: string }, request.signal);
      case "restore":
        return this.restore(route.id, body as { from_version: number; base_version: number });
      case "archive":
        return this.archive(route.id, request.method === "PUT");
      case "drafts":
        return this.drafts(route.id);
      case "publish":
        return this.publish(route.id, body as { version: number });
    }
  }

  /** Waits for `release` while `endpoint` is held. */
  private async wait(endpoint: Endpoint, signal: AbortSignal): Promise<void> {
    const held = this.held;
    if (held === null || !this.holding.includes(endpoint)) return;
    await new Promise<void>((resume, reject) => {
      held.push(resume);
      signal.addEventListener("abort", () => reject(signal.reason), { once: true });
    });
  }

  private route(method: string, path: string): { endpoint: Endpoint; id: string } | null {
    if (path === "/api/assistant/v1/dashboards" && method === "GET") return { endpoint: "list", id: "" };
    const conversation = /^\/api\/assistant\/v1\/conversations\/([^/]+)\/dashboards$/.exec(path);
    if (conversation !== null && method === "GET") {
      return { endpoint: "drafts", id: decodeURIComponent(conversation[1] as string) };
    }
    const match = /^\/api\/assistant\/v1\/dashboards\/([^/]+)(\/[a-z-]+)?$/.exec(path);
    if (match === null) return null;
    const id = decodeURIComponent(match[1] as string);
    const routes: Record<string, [string[], Endpoint]> = {
      "": [["GET"], "dashboard"],
      "/versions": [["GET"], "versions"],
      "/dataset-runs": [["POST"], "run"],
      "/restore": [["POST"], "restore"],
      "/archive": [["PUT", "DELETE"], "archive"],
      "/publish": [["POST"], "publish"],
    };
    const found = routes[match[2] ?? ""];
    return found?.[0].includes(method) ? { endpoint: found[1], id } : null;
  }

  private summary(dashboard: FakeDashboard): Schemas["DashboardSummary"] {
    const latest = dashboard.versions.at(-1) as FakeVersion;
    return {
      dashboard_id: dashboard.id,
      kind: dashboard.kind,
      title: latest.title,
      latest_version: latest.version,
      created_by: dashboard.created_by,
      created_at: dashboard.created_at,
      updated_at: dashboard.updated_at,
      ...(dashboard.archived_at ? { archived_at: dashboard.archived_at } : {}),
      ...(dashboard.conversation_id ? { conversation_id: dashboard.conversation_id } : {}),
      ...(dashboard.saved
        ? {
            saved: {
              dashboard_id: dashboard.saved.id,
              ...(dashboard.saved.version !== undefined ? { version: dashboard.saved.version } : {}),
            },
          }
        : {}),
    };
  }

  private editor(): boolean {
    return this.role !== "viewer";
  }

  private list(query: URLSearchParams): Response {
    const archived = query.get("include_archived") === "true";
    const all = [...this.dashboards.values()]
      .filter((dashboard) => dashboard.kind === "saved" && (archived || dashboard.archived_at === undefined))
      .sort((a, b) => (a.updated_at < b.updated_at ? 1 : a.updated_at > b.updated_at ? -1 : 0));
    const start = Number(query.get("cursor") ?? "0");
    const page = all.slice(start, start + this.pageSize);
    const next = start + this.pageSize < all.length ? String(start + this.pageSize) : undefined;
    return json(200, {
      dashboards: page.map((dashboard) => this.summary(dashboard)),
      ...(next ? { next_cursor: next } : {}),
    });
  }

  private view(id: string, version: string | null): Response {
    const dashboard = this.dashboards.get(id);
    if (dashboard === undefined) return notFound();
    const shown =
      version === null
        ? dashboard.versions.at(-1)
        : dashboard.versions.find((item) => String(item.version) === version);
    if (shown === undefined) return fakeError(404, "not_found", "dashboard.version_not_found");
    const saved = dashboard.kind === "saved";
    return json(200, {
      dashboard: this.summary(dashboard),
      version: shown,
      permissions: {
        can_publish: !saved && this.editor(),
        can_restore: !saved || this.editor(),
        can_archive: saved && this.editor(),
      },
    });
  }

  private versions(id: string): Response {
    const dashboard = this.dashboards.get(id);
    if (dashboard === undefined) return notFound();
    return json(200, {
      versions: [...dashboard.versions]
        .reverse()
        .map(({ spec: _spec, page: _page, description: _d, ...summary }) => summary),
    });
  }

  private async run(id: string, body: { version: number; dataset: string }, signal: AbortSignal): Promise<Response> {
    const dashboard = this.dashboards.get(id);
    if (dashboard === undefined) return notFound();
    const version = dashboard.versions.find((item) => item.version === body.version);
    if (version === undefined) return fakeError(404, "not_found", "dashboard.version_not_found");
    if (!version.spec.datasets.some((dataset) => dataset.name === body.dataset)) {
      return fakeError(422, "validation_failed", "dashboard.dataset_unknown");
    }
    this.running += 1;
    this.maxRunning = Math.max(this.maxRunning, this.running);
    try {
      await this.wait("run", signal);
      const failure = this.failures.get(`run:${body.dataset}`)?.shift();
      if (failure) return failure;
      const result = this.results.get(body.dataset);
      return result ? json(200, result) : fakeError(503, "unavailable", "dashboard.dataset_failed");
    } finally {
      this.running -= 1;
    }
  }

  private restore(id: string, body: { from_version: number; base_version: number }): Response {
    const dashboard = this.dashboards.get(id);
    if (dashboard === undefined) return notFound();
    if (!this.editor()) return fakeError(403, "permission_denied", "dashboard.editor_required");
    if (dashboard.archived_at !== undefined) return fakeError(409, "conflict", "dashboard.archived");
    const latest = dashboard.versions.at(-1) as FakeVersion;
    if (body.base_version !== latest.version) {
      return fakeError(409, "conflict", "dashboard.version_conflict", { latest: latest.version });
    }
    const from = dashboard.versions.find((item) => item.version === body.from_version);
    if (from === undefined) return fakeError(404, "not_found", "dashboard.version_not_found");
    const content = (version: FakeVersion) =>
      JSON.stringify([version.title, version.description, version.spec, version.page, version.runtime]);
    if (content(from) === content(latest)) return fakeError(409, "conflict", "dashboard.restore_unchanged");
    const now = this.now();
    const { version: _v, origin: _o, restored_from: _r, created_at: _c, created_by: _b, ...rest } = from;
    const created: FakeVersion = {
      ...rest,
      version: latest.version + 1,
      origin: "restore",
      restored_from: from.version,
      created_by: this.user,
      created_at: now,
    };
    dashboard.versions.push(created);
    dashboard.updated_at = now;
    return json(201, { dashboard_id: dashboard.id, version: created.version });
  }

  private drafts(conversationId: string): Response {
    const all = [...this.dashboards.values()]
      .filter((dashboard) => dashboard.kind === "draft" && dashboard.conversation_id === conversationId)
      .sort((a, b) => (a.updated_at < b.updated_at ? 1 : a.updated_at > b.updated_at ? -1 : 0));
    return json(200, { dashboards: all.map((dashboard) => this.summary(dashboard)) });
  }

  private publish(id: string, body: { version: number }): Response {
    const draft = this.dashboards.get(id);
    if (draft === undefined) return notFound();
    if (!this.editor()) return fakeError(403, "permission_denied", "dashboard.editor_required");
    if (draft.kind !== "draft") return fakeError(422, "validation_failed", "dashboard.draft_required");
    const published = draft.versions.find((item) => item.version === body.version);
    if (published === undefined) return fakeError(404, "not_found", "dashboard.version_not_found");
    const now = this.now();
    const {
      version: _v,
      origin: _o,
      base: _b,
      restored_from: _r,
      created_at: _c,
      created_by: _u,
      ...content
    } = published;
    if (draft.saved === undefined) {
      const saved: FakeDashboard = {
        id: fakeUuid(),
        kind: "saved",
        created_by: this.user,
        created_at: now,
        updated_at: now,
        versions: [{ ...content, version: 1, origin: "publish", created_by: this.user, created_at: now }],
      };
      this.dashboards.set(saved.id, saved);
      draft.saved = { id: saved.id, version: 1 };
      return json(201, { dashboard_id: saved.id, version: 1 });
    }
    const saved = this.dashboards.get(draft.saved.id);
    if (saved === undefined) return notFound();
    if (saved.archived_at !== undefined) return fakeError(409, "conflict", "dashboard.archived");
    const latest = (saved.versions.at(-1) as FakeVersion).version;
    if (published.base !== latest) {
      return fakeError(409, "conflict", "dashboard.saved_changed", { latest });
    }
    const created: FakeVersion = {
      ...content,
      version: latest + 1,
      origin: "publish",
      created_by: this.user,
      created_at: now,
    };
    saved.versions.push(created);
    saved.updated_at = now;
    draft.saved = { id: saved.id, version: created.version };
    return json(201, { dashboard_id: saved.id, version: created.version });
  }

  private archive(id: string, archived: boolean): Response {
    const dashboard = this.dashboards.get(id);
    if (dashboard === undefined) return notFound();
    if (!this.editor()) return fakeError(403, "permission_denied", "dashboard.editor_required");
    if (dashboard.kind !== "saved") return fakeError(422, "validation_failed", "dashboard.saved_required");
    if (archived) {
      dashboard.archived_at ??= this.now();
    } else {
      delete dashboard.archived_at;
    }
    return json(200, {
      dashboard_id: dashboard.id,
      ...(dashboard.archived_at ? { archived_at: dashboard.archived_at } : {}),
    });
  }
}
