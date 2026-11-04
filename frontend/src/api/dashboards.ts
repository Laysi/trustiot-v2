import { type ComponentSpec, ComponentSpecSchema, parseComponentSpec } from "../dashboard/spec.ts";
import type { AllTrue, Assert, Equal } from "../lib/type-binding.ts";
import { z } from "../lib/zod.ts";
import type { ApiClient } from "./client.ts";
import { pinnable, unwrap } from "./client.ts";
import type { components } from "./schema.gen.ts";

type Spec = components["schemas"];

// The dashboard page's bodies: the assistant's public dashboard API
// (crates/contract/src/api/dashboard/public.rs, services/assistant/README.md 「儀表板」). Each
// schema is bound to the type generated from api/openapi.json (lib/type-binding.ts), so a change
// in the contract fails `typecheck` here until the schema follows.

const DateTime = z.string();
const Optional = <T extends z.ZodType>(schema: T) => schema.nullable().optional();

const DashboardKindSchema = z.enum(["draft", "saved"]);
const SavedBaseSchema = z.object({ dashboard_id: z.string(), version: Optional(z.number().int()) });
const SummarySchema = z.object({
  dashboard_id: z.string(),
  kind: DashboardKindSchema,
  title: z.string(),
  latest_version: z.number().int(),
  created_by: z.string(),
  created_at: DateTime,
  updated_at: DateTime,
  archived_at: Optional(DateTime),
  conversation_id: Optional(z.string()),
  saved: Optional(SavedBaseSchema),
});
const ListSchema = z.object({ dashboards: z.array(SummarySchema), next_cursor: Optional(z.string()) });

const SemanticIntervalSchema = z.enum(["1m", "5m", "15m", "1h", "1d", "1w", "1mo", "total"]);
const SemanticRequestSchema = z.object({
  series: z.array(z.object({ device_id: z.string(), point: z.string() })),
  tz: z.string(),
  period: Optional(z.string()),
  from: Optional(DateTime),
  to: Optional(DateTime),
  interval: SemanticIntervalSchema.optional(),
});
const DatasetSourceSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("sql"), sql: z.string() }),
  z.object({ kind: z.literal("telemetry_statistics"), request: SemanticRequestSchema }),
  z.object({ kind: z.literal("telemetry_usage"), request: SemanticRequestSchema }),
]);
const DatasetSchema = z.object({
  name: z.string(),
  title: z.string(),
  description: z.string(),
  max_rows: z.number().int(),
  source: DatasetSourceSchema,
});
const ComputationSchema = z.object({
  name: z.string(),
  title: z.string(),
  formula: z.string(),
  unit: Optional(z.string()),
});
const ConstantSchema = z.object({
  name: z.string(),
  title: z.string(),
  value: z.union([z.number(), z.string()]),
  unit: Optional(z.string()),
  note: Optional(z.string()),
});

/** A version's spec, with `component` as the schema of one item of its component list. */
const specSchema = <C extends z.ZodType>(component: C) =>
  z.object({
    datasets: z.array(DatasetSchema),
    computations: z.array(ComputationSchema).optional(),
    constants: z.array(ConstantSchema).optional(),
    components: z.array(component).optional(),
  });

const OriginSchema = z.enum(["agent", "publish", "restore"]);
const versionSchema = <C extends z.ZodType>(component: C) =>
  z.object({
    version: z.number().int(),
    runtime: Optional(z.number().int()),
    title: z.string(),
    description: Optional(z.string()),
    spec: specSchema(component),
    page: Optional(z.string()),
    origin: OriginSchema,
    restored_from: Optional(z.number().int()),
    created_by: z.string(),
    created_at: DateTime,
  });
const PermissionsSchema = z.object({ can_publish: z.boolean(), can_restore: z.boolean(), can_archive: z.boolean() });
const viewSchema = <C extends z.ZodType>(component: C) =>
  z.object({ dashboard: SummarySchema, version: versionSchema(component), permissions: PermissionsSchema });

/** The contract's view, item for item: what the binding below compares with the generated type. */
const ViewSchema = viewSchema(ComponentSpecSchema);

/**
 * An item of the component list as the page reads it: an item this build does not know (a new
 * kind or field from a newer API during a rollout) is `unsupported`, so the page shows that one
 * item can't be shown instead of failing the whole dashboard. The contract's rule that an unknown
 * kind or field is refused, never guessed (docs/architecture/ai-assistant.md, 「兩條路的正確性」),
 * holds item by item.
 */
export type ShownItem = ComponentSpec | { kind: "unsupported" };
const ShownItemSchema = z
  .unknown()
  .transform((value): ShownItem => parseComponentSpec(value) ?? { kind: "unsupported" });
const ShownViewSchema = viewSchema(ShownItemSchema);

const VersionSummarySchema = z.object({
  version: z.number().int(),
  runtime: Optional(z.number().int()),
  title: z.string(),
  origin: OriginSchema,
  restored_from: Optional(z.number().int()),
  created_by: z.string(),
  created_at: DateTime,
});
const VersionListSchema = z.object({ versions: z.array(VersionSummarySchema), next_cursor: Optional(z.string()) });

const SemanticRangeSchema = z.object({
  earliest: DateTime,
  ends_after_now: z.boolean(),
  from: DateTime,
  interval: SemanticIntervalSchema,
  queried_at: DateTime,
  resolution: z.enum(["1m", "1h"]),
  to: DateTime,
  truncated: z.boolean(),
  tz: z.string(),
});
const SourceMetaSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("sql"),
    queryable_from: z.object({
      tenant_telemetry: DateTime,
      tenant_telemetry_1h: DateTime,
      tenant_telemetry_1m: DateTime,
    }),
  }),
  z.object({ kind: z.literal("telemetry_statistics"), range: SemanticRangeSchema }),
  z.object({ kind: z.literal("telemetry_usage"), range: SemanticRangeSchema }),
]);
const DatasetResultSchema = z.object({
  dataset: z.string(),
  columns: z.array(z.object({ name: z.string(), type: z.enum(["number", "text", "time", "bool"]) })),
  rows: z.array(z.array(z.union([z.number(), z.string(), z.boolean(), z.null()]))),
  row_count: z.number().int(),
  meta: z.object({ duration_ms: z.number().int(), queried_at: DateTime, source: SourceMetaSchema }),
});

const ArchiveStateSchema = z.object({ dashboard_id: z.string(), archived_at: Optional(DateTime) });
const VersionCreatedSchema = z.object({ dashboard_id: z.string(), version: z.number().int() });

export type DashboardBodiesMatchSpec = Assert<
  AllTrue<
    [
      Equal<z.output<typeof SummarySchema>, Spec["DashboardSummary"]>,
      Equal<z.output<typeof ListSchema>, Spec["DashboardList"]>,
      Equal<z.output<typeof DatasetSchema>, Spec["DashboardDataset"]>,
      Equal<z.output<typeof ComputationSchema>, Spec["DashboardComputation"]>,
      Equal<z.output<typeof ConstantSchema>, Spec["DashboardConstant"]>,
      Equal<z.output<typeof ViewSchema>, Spec["DashboardView"]>,
      Equal<z.output<typeof VersionListSchema>, Spec["DashboardVersionList"]>,
      Equal<z.output<typeof DatasetResultSchema>, Spec["DashboardDatasetResult"]>,
      Equal<z.output<typeof ArchiveStateSchema>, Spec["DashboardArchiveState"]>,
      Equal<z.output<typeof VersionCreatedSchema>, Spec["DashboardVersionCreated"]>,
    ]
  >
>;

export type DashboardSummary = z.output<typeof SummarySchema>;
export type DashboardList = z.output<typeof ListSchema>;
export type DashboardPermissions = z.output<typeof PermissionsSchema>;
/** `GET /dashboards/{dashboard_id}`, with each item of the component list as `ShownItem`. */
export type DashboardView = z.output<typeof ShownViewSchema>;
export type DashboardVersion = DashboardView["version"];
export type VersionSummary = z.output<typeof VersionSummarySchema>;
export type VersionList = z.output<typeof VersionListSchema>;
export type DatasetResult = z.output<typeof DatasetResultSchema>;
export type ArchiveState = z.output<typeof ArchiveStateSchema>;
export type VersionCreated = z.output<typeof VersionCreatedSchema>;

/** The dashboard endpoints the dashboard pages use. */
export interface DashboardApi {
  /** One page of the tenant's saved dashboards, the most recently updated first. */
  listDashboards(query: { includeArchived: boolean; cursor?: string }, signal?: AbortSignal): Promise<DashboardList>;
  /** A dashboard with one of its versions: the latest when `version` is `undefined`. */
  getDashboard(dashboardId: string, version: number | undefined, signal?: AbortSignal): Promise<DashboardView>;
  /** One page of a dashboard's versions, the newest first. */
  listVersions(dashboardId: string, cursor: string | undefined, signal?: AbortSignal): Promise<VersionList>;
  /** Runs one dataset of one version as the viewer; nothing is cached. */
  runDataset(
    dashboardId: string,
    request: { version: number; dataset: string },
    signal?: AbortSignal,
  ): Promise<DatasetResult>;
  /** Adds a copy of `from_version` as the next version; refused when `base_version` is not the latest. */
  restore(dashboardId: string, request: { from_version: number; base_version: number }): Promise<VersionCreated>;
  /** Archives (`true`) or unarchives a saved dashboard. */
  setArchived(dashboardId: string, archived: boolean): Promise<ArchiveState>;
}

/** The dashboard endpoints, through the typed client (`pinnable`: a page binds its own copy). */
export function dashboardApi(client: ApiClient): DashboardApi {
  return pinnable(client, (client) => ({
    listDashboards: ({ includeArchived, cursor }, signal) =>
      unwrap(
        ListSchema,
        client.GET("/api/assistant/v1/dashboards", {
          // The server refuses a parameter it does not expect a value for, so only what is set goes.
          params: {
            query: { ...(includeArchived ? { include_archived: true } : {}), ...(cursor ? { cursor } : {}) },
          },
          ...(signal ? { signal } : {}),
        }),
      ),
    getDashboard: (dashboardId, version, signal) =>
      unwrap(
        ShownViewSchema,
        client.GET("/api/assistant/v1/dashboards/{dashboard_id}", {
          params: { path: { dashboard_id: dashboardId }, query: version === undefined ? {} : { version } },
          ...(signal ? { signal } : {}),
        }),
      ),
    listVersions: (dashboardId, cursor, signal) =>
      unwrap(
        VersionListSchema,
        client.GET("/api/assistant/v1/dashboards/{dashboard_id}/versions", {
          params: { path: { dashboard_id: dashboardId }, query: cursor ? { cursor } : {} },
          ...(signal ? { signal } : {}),
        }),
      ),
    runDataset: (dashboardId, request, signal) =>
      unwrap(
        DatasetResultSchema,
        client.POST("/api/assistant/v1/dashboards/{dashboard_id}/dataset-runs", {
          params: { path: { dashboard_id: dashboardId } },
          body: request,
          ...(signal ? { signal } : {}),
        }),
      ),
    restore: (dashboardId, request) =>
      unwrap(
        VersionCreatedSchema,
        client.POST("/api/assistant/v1/dashboards/{dashboard_id}/restore", {
          params: { path: { dashboard_id: dashboardId } },
          body: request,
        }),
      ),
    setArchived: (dashboardId, archived) =>
      unwrap(
        ArchiveStateSchema,
        archived
          ? client.PUT("/api/assistant/v1/dashboards/{dashboard_id}/archive", {
              params: { path: { dashboard_id: dashboardId } },
            })
          : client.DELETE("/api/assistant/v1/dashboards/{dashboard_id}/archive", {
              params: { path: { dashboard_id: dashboardId } },
            }),
      ),
  }));
}
