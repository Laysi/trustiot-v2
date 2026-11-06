import type { AllTrue, Assert, Equal } from "../lib/type-binding.ts";
import { z } from "../lib/zod.ts";
import { type ApiClient, pinnable, unwrap } from "./client.ts";
import type { components } from "./schema.gen.ts";

type Spec = components["schemas"];

// The AI gateway's model roles as the identity service serves them to platform administrators
// (services/identity/README.md 「AI 模型角色」), each body bound to the generated type so a change
// in api/openapi.json fails `typecheck` (lib/type-binding.ts).
const ModelSourceSchema = z.enum(["configuration", "database"]);
const IgnoredModelsSchema = z.object({ models: z.array(z.string()), reason: z.string() });
const ModelRoleSchema = z.object({
  role: z.string(),
  protocol: z.string(),
  models: z.array(z.string()),
  configured_models: z.array(z.string()),
  source: ModelSourceSchema,
  version: z.number().int(),
  ignored: IgnoredModelsSchema.nullable(),
  updated_at: z.string().nullable(),
  updated_by: z.string().nullable(),
});
const ModelProviderSchema = z.object({ name: z.string(), protocol: z.string() });
const ModelRolesSchema = z.object({
  roles: z.array(ModelRoleSchema),
  providers: z.array(ModelProviderSchema),
});

export type ModelRoleBodiesMatchSpec = Assert<
  AllTrue<
    [
      Equal<z.output<typeof ModelSourceSchema>, Spec["ModelSource"]>,
      Equal<z.output<typeof IgnoredModelsSchema>, Spec["IgnoredModels"]>,
      Equal<z.output<typeof ModelRoleSchema>, Spec["ModelRole"]>,
      Equal<z.output<typeof ModelProviderSchema>, Spec["ModelProvider"]>,
      Equal<z.output<typeof ModelRolesSchema>, Spec["ModelRoles"]>,
    ]
  >
>;

export type ModelRole = z.output<typeof ModelRoleSchema>;
export type ModelProvider = z.output<typeof ModelProviderSchema>;
export type ModelRoles = z.output<typeof ModelRolesSchema>;

/**
 * The platform administrator's model role endpoints. A change or reset names the `version` the
 * caller read: another version answers 409 `ai_model_role.version_conflict` (`details.current_version`),
 * a result nobody can tell 504 `ai_model_role.outcome_unknown` (read the roles again before sending
 * it a second time).
 */
export interface ModelRoleApi {
  list(signal?: AbortSignal): Promise<ModelRoles>;
  /** Replaces the role's models (1 to 8, `<provider>/<model>`, first choice first). */
  set(role: string, models: readonly string[], expectedVersion: number): Promise<ModelRole>;
  /** Goes back to the models the configuration gives the role. */
  reset(role: string, expectedVersion: number): Promise<ModelRole>;
}

/** The model role endpoints, through the typed client (`pinnable`: a page binds its own copy). */
export function modelRoleApi(client: ApiClient): ModelRoleApi {
  return pinnable(client, (client) => ({
    list: (signal) =>
      unwrap(ModelRolesSchema, client.GET("/api/identity/v1/platform/ai/model-roles", signal ? { signal } : {})),
    set: (role, models, expectedVersion) =>
      unwrap(
        ModelRoleSchema,
        client.PUT("/api/identity/v1/platform/ai/model-roles/{role}", {
          params: { path: { role } },
          body: { models: [...models], expected_version: expectedVersion },
        }),
      ),
    reset: (role, expectedVersion) =>
      unwrap(
        ModelRoleSchema,
        client.DELETE("/api/identity/v1/platform/ai/model-roles/{role}", {
          params: { path: { role }, query: { expected_version: expectedVersion } },
        }),
      ),
  }));
}
