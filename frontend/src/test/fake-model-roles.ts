import { createApiClient } from "../api/client.ts";
import { type ModelProvider, type ModelRole, type ModelRoleApi, modelRoleApi } from "../api/modelRoles.ts";
import { fakeError } from "./fake-dashboards.ts";

/**
 * The AI gateway's model roles as the identity service serves them to platform administrators
 * (services/identity/README.md 「AI 模型角色」), held in memory and answered at the HTTP level, so
 * the page goes through the real client (`modelRoleApi`, zod parsing, the bodies and the query):
 *
 * - `GET` lists every role and provider; a change or reset names the version the caller read, and
 *   another version is 409 `ai_model_role.version_conflict` with `details.current_version`.
 * - A change needs 1 to 8 distinct `<provider>/<model>` (422 `ai_model_role.invalid_models`), of a
 *   configured provider (422 `ai_model_role.unknown_provider`) that speaks the role's protocol (422
 *   `ai_model_role.protocol_mismatch`); an unknown role is 404 `ai_model_role.not_found`.
 * - A reset of a role that has no stored models changes nothing and keeps its version.
 * - Only a platform administrator may call it (403 `permission_denied`): `allowed = false`.
 *
 * `failNext` answers the next request with an error; `outcomeUnknownNext` applies the next change
 * and then answers 504 `ai_model_role.outcome_unknown`, as the identity service does when the AI
 * gateway's answer was lost; `hold` keeps requests waiting until `release`.
 */

export const ANTHROPIC = "anthropic-messages";
export const OPENAI = "openai-decisions";

export interface FakeRequest {
  method: string;
  url: URL;
  body: unknown;
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

const OPUS = "anthropic/claude-opus-5-5";
const SONNET = "anthropic/claude-sonnet-5-5";
const HAIKU = "anthropic/claude-haiku-5-5";

export class FakeModelRoleServer {
  readonly roles = new Map<string, ModelRole>();
  providers: ModelProvider[] = [
    { name: "anthropic", protocol: ANTHROPIC },
    { name: "openai", protocol: OPENAI },
  ];
  readonly requests: FakeRequest[] = [];
  /** Whether the caller is a platform administrator. */
  allowed = true;
  /** The user whose ID the AI gateway records. */
  user = "00000000-0000-4000-8000-0000000000a1";
  now: () => string = () => new Date().toISOString();
  private readonly failures: Response[] = [];
  private unknownNext = false;
  private held: (() => void)[] | null = null;

  /** The demo roles: one changed, one with an ignored list, the others on the configuration. */
  constructor(options: { seed?: "normal" | "empty" } = {}) {
    if ((options.seed ?? "normal") === "empty") return;
    this.addRole({
      role: "main",
      protocol: ANTHROPIC,
      models: [OPUS],
      configured_models: [OPUS, SONNET],
      source: "database",
      version: 14,
      updated_at: "2026-11-01T01:30:00Z",
      updated_by: "user:00000000-0000-4000-8000-0000000000b2",
    });
    this.addRole({ role: "worker", protocol: ANTHROPIC, models: [HAIKU], configured_models: [HAIKU] });
    this.addRole({
      role: "worker_sql",
      protocol: ANTHROPIC,
      models: [SONNET, OPUS],
      configured_models: [SONNET, OPUS],
    });
    this.addRole({
      role: "worker_dash",
      protocol: ANTHROPIC,
      models: [SONNET, OPUS],
      configured_models: [SONNET, OPUS],
    });
    this.addRole({
      role: "router",
      protocol: OPENAI,
      models: ["openai/gpt-6-luna"],
      configured_models: ["openai/gpt-6-luna"],
    });
    this.addRole({
      role: "complete",
      protocol: ANTHROPIC,
      models: [HAIKU],
      configured_models: [HAIKU],
      version: 9,
      updated_at: "2026-10-24T05:12:00Z",
      updated_by: "user:00000000-0000-4000-8000-0000000000b3",
      ignored: { models: ["anthropic-old/claude-haiku-4-5"], reason: "provider anthropic-old is not configured" },
    });
  }

  /** Adds a role; what is left out is a role that was never changed. */
  addRole(role: Pick<ModelRole, "role" | "protocol" | "models" | "configured_models"> & Partial<ModelRole>): ModelRole {
    const full: ModelRole = {
      source: "configuration",
      version: 0,
      ignored: null,
      updated_at: null,
      updated_by: null,
      ...role,
    };
    this.roles.set(full.role, full);
    return full;
  }

  /** The next request gets `response`. */
  failNext(response: Response): void {
    this.failures.push(response);
  }

  /** The next change is applied, and the answer is 504 `ai_model_role.outcome_unknown`. */
  outcomeUnknownNext(): void {
    this.unknownNext = true;
  }

  /** Someone else changes `role` now, as another administrator would (the version goes up). */
  changeBehindBack(role: string, models: string[]): void {
    this.apply(role, models);
  }

  hold(): void {
    this.held ??= [];
  }

  release(): void {
    const waiting = this.held ?? [];
    this.held = null;
    for (const resume of waiting) resume();
  }

  api(): ModelRoleApi {
    return modelRoleApi(createApiClient({ fetch: (request) => this.fetch(request) }));
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const text = request.method === "GET" || request.method === "DELETE" ? "" : await request.text();
    const body: unknown = text === "" ? undefined : JSON.parse(text);
    this.requests.push({ method: request.method, url, body });
    const held = this.held;
    if (held !== null) {
      await new Promise<void>((resume, reject) => {
        held.push(resume);
        request.signal.addEventListener("abort", () => reject(request.signal.reason), { once: true });
      });
    }
    const failure = this.failures.shift();
    if (failure) return failure;
    if (!this.allowed) return fakeError(403, "permission_denied", "error.permission_denied");
    const base = "/api/identity/v1/platform/ai/model-roles";
    if (url.pathname === base && request.method === "GET") {
      return json(200, { roles: [...this.roles.values()], providers: this.providers });
    }
    const match = url.pathname.startsWith(`${base}/`) ? decodeURIComponent(url.pathname.slice(base.length + 1)) : null;
    if (match === null) return fakeError(404, "not_found", "error.not_found");
    const role = this.roles.get(match);
    if (role === undefined) return fakeError(404, "not_found", "ai_model_role.not_found");
    if (request.method === "PUT") {
      const change = body as { models: string[]; expected_version: number };
      if (role.version !== change.expected_version) return this.conflict(role);
      const refusal = this.refuse(role, change.models);
      if (refusal) return refusal;
      const changed = this.apply(match, change.models);
      return this.answer(changed);
    }
    if (request.method === "DELETE") {
      const expected = Number(url.searchParams.get("expected_version"));
      if (role.version !== expected) return this.conflict(role);
      if (role.source === "configuration" && role.ignored === null) return json(200, role);
      const reset: ModelRole = {
        ...role,
        models: role.configured_models,
        source: "configuration",
        ignored: null,
        version: role.version + 1,
        updated_at: this.now(),
        updated_by: `user:${this.user}`,
      };
      this.roles.set(match, reset);
      return this.answer(reset);
    }
    return fakeError(404, "not_found", "error.not_found");
  }

  private answer(role: ModelRole): Response {
    if (this.unknownNext) {
      this.unknownNext = false;
      return fakeError(504, "timeout", "ai_model_role.outcome_unknown");
    }
    return json(200, role);
  }

  private conflict(role: ModelRole): Response {
    return fakeError(409, "conflict", "ai_model_role.version_conflict", { current_version: role.version });
  }

  private refuse(role: ModelRole, models: string[]): Response | null {
    const invalid = models.length < 1 || models.length > 8 || new Set(models).size !== models.length;
    const parsed = models.map((model) => model.split("/"));
    if (invalid || parsed.some((parts) => parts.length !== 2 || !parts[0] || !parts[1] || /[\s,]/.test(parts[1]))) {
      return this.unprocessable("ai_model_role.invalid_models");
    }
    for (const [provider] of parsed) {
      const known = this.providers.find((candidate) => candidate.name === provider);
      if (known === undefined) return this.unprocessable("ai_model_role.unknown_provider");
      if (known.protocol !== role.protocol) return this.unprocessable("ai_model_role.protocol_mismatch");
    }
    return null;
  }

  private unprocessable(messageKey: string): Response {
    return json(422, {
      code: "validation_failed",
      message_key: "error.validation_failed",
      field_errors: [{ field: "/models", message_key: messageKey }],
    });
  }

  private apply(name: string, models: string[]): ModelRole {
    const role = this.roles.get(name) as ModelRole;
    const changed: ModelRole = {
      ...role,
      models,
      source: "database",
      ignored: null,
      version: role.version + 1,
      updated_at: this.now(),
      updated_by: `user:${this.user}`,
    };
    this.roles.set(name, changed);
    return changed;
  }
}

export const preview = {
  name: "Model roles (identity)",
  http: {
    prefixes: ["/api/identity/v1/platform/ai/model-roles"],
    create: (seed: "normal" | "empty") => new FakeModelRoleServer({ seed }),
  },
};
