import { useQuery } from "@tanstack/react-query";
import { RefreshCwIcon, TriangleAlertIcon, XIcon } from "lucide-react";
import { useState } from "react";

import { useModelRoleApi } from "../../../api/ModelRoleApiProvider.tsx";
import type { ModelProvider, ModelRole } from "../../../api/modelRoles.ts";
import { EmptyState } from "../../../components/EmptyState.tsx";
import { Alert, AlertDescription, AlertTitle } from "../../../components/ui/alert.tsx";
import { Button } from "../../../components/ui/button.tsx";
import { Card, CardContent, CardFooter, CardHeader, CardTitle } from "../../../components/ui/card.tsx";
import type { MessageId } from "../../../i18n/messages.ts";
import { useI18n } from "../../../i18n/useI18n.ts";
import { PageHeader } from "../../dashboards/PageHeader.tsx";
import { MODEL_ROLES_KEY } from "./queries.ts";
import { ChangeRoleDialog, protocolName, ResetRoleDialog } from "./RoleDialogs.tsx";
import { Badge, Loading, Skeleton } from "./ui.tsx";

/** What each known role is for; a role the page has no text for shows no purpose line. */
const PURPOSES: Readonly<Record<string, MessageId>> = {
  main: "platform_ai.roles.purpose.main",
  worker: "platform_ai.roles.purpose.worker",
  worker_sql: "platform_ai.roles.purpose.worker_sql",
  worker_dash: "platform_ai.roles.purpose.worker_dash",
  router: "platform_ai.roles.purpose.router",
  complete: "platform_ai.roles.purpose.complete",
};

/** The role's own stored models are in use, or the configuration's. */
function isChanged(role: ModelRole): boolean {
  return role.source === "database";
}

/** Something the administrator can reset: stored models in use, or a stored list that is ignored. */
function isResettable(role: ModelRole): boolean {
  return isChanged(role) || role.ignored !== null;
}

function RoleCard({
  role,
  providers,
  onChanged,
}: {
  role: ModelRole;
  providers: readonly ModelProvider[];
  onChanged: (role: ModelRole, kind: "set" | "reset") => void;
}) {
  const { t, formatDateTime } = useI18n();
  const purpose = PURPOSES[role.role];
  const facts = [
    role.version === 0 && role.updated_at === null
      ? t("platform_ai.roles.never_changed")
      : t("platform_ai.roles.version_line", { version: role.version }),
    ...(role.version > 0 && role.updated_at ? [formatDateTime(role.updated_at)] : []),
    ...(role.version > 0 && role.updated_by ? [role.updated_by] : []),
  ];
  const headingId = `role-${role.role}`;
  return (
    <Card aria-labelledby={headingId} className="gap-4 py-5">
      <CardHeader>
        <div className="grid min-w-0 gap-0.5">
          <CardTitle id={headingId} className="font-mono text-base wrap-anywhere">
            {role.role}
          </CardTitle>
          {purpose ? <p className="text-sm text-muted-foreground">{t(purpose)}</p> : null}
        </div>
        <div className="flex flex-wrap items-center gap-1.5 self-start">
          <Badge tone="outline">{protocolName(role.protocol)}</Badge>
          <Badge tone={isChanged(role) ? "brand" : "neutral"}>
            {isChanged(role) ? t("platform_ai.roles.changed") : t("platform_ai.roles.default")}
          </Badge>
        </div>
      </CardHeader>
      <CardContent className="grid gap-3">
        {role.ignored ? (
          <Alert role="status">
            <TriangleAlertIcon aria-hidden="true" className="text-warning" />
            <AlertTitle>{t("platform_ai.roles.ignored_title")}</AlertTitle>
            <AlertDescription>
              <p className="wrap-anywhere">
                {t("platform_ai.roles.ignored_body", {
                  models: role.ignored.models.join(", "),
                  reason: role.ignored.reason,
                })}
              </p>
            </AlertDescription>
          </Alert>
        ) : null}
        <ol aria-label={t("platform_ai.roles.models_label", { role: role.role })} className="grid gap-1.5">
          {role.models.map((model, index) => (
            <li key={model} className="flex items-center gap-2 rounded-md border px-3 py-1.5 first:border-primary/50">
              <span aria-hidden="true" className="text-sm text-muted-foreground tabular-nums">
                {index + 1}
              </span>
              <span className="min-w-0 flex-1 font-mono text-sm wrap-anywhere">{model}</span>
              <span className="text-xs text-muted-foreground">
                {index > 0
                  ? t("platform_ai.roles.fallback")
                  : role.ignored
                    ? t("platform_ai.roles.primary_default")
                    : t("platform_ai.roles.primary")}
              </span>
            </li>
          ))}
        </ol>
        {role.models.length === 1 ? (
          <Alert role="status">
            <TriangleAlertIcon aria-hidden="true" className="text-warning" />
            <AlertTitle>{t("platform_ai.roles.no_fallback_title")}</AlertTitle>
            <AlertDescription>
              <p>{t("platform_ai.roles.no_fallback_body")}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        {isChanged(role) ? (
          <p className="text-xs text-muted-foreground wrap-anywhere">
            {t("platform_ai.roles.default_line", { models: role.configured_models.join(" → ") })}
          </p>
        ) : null}
        <p className="text-xs text-muted-foreground wrap-anywhere">{facts.join(" · ")}</p>
      </CardContent>
      <CardFooter className="justify-end">
        {isResettable(role) ? <ResetRoleDialog role={role} onChanged={onChanged} /> : null}
        <ChangeRoleDialog role={role} providers={providers} onChanged={onChanged} />
      </CardFooter>
    </Card>
  );
}

/**
 * The AI gateway's model roles (`/platform/ai/model-roles`, checklist J8 and K1; design:
 * docs/design/platform-admin/ai-model-roles.html): each role's models in the order they are tried,
 * where they come from, and the two changes a platform administrator can make: replace the list or
 * reset it to the configuration's. A change applies to every tenant, so it takes two steps and
 * shows what the server answered (RoleDialogs.tsx).
 */
export function ModelRolesPage() {
  const { t, errorMessage, formatDateTime } = useI18n();
  const api = useModelRoleApi();
  const roles = useQuery({ queryKey: MODEL_ROLES_KEY, queryFn: ({ signal }) => api.list(signal) });
  const [notice, setNotice] = useState<string | null>(null);

  const changed = (role: ModelRole, kind: "set" | "reset") =>
    setNotice(
      t(kind === "set" ? "platform_ai.roles.notice_set" : "platform_ai.roles.notice_reset", {
        role: role.role,
        version: role.version,
      }),
    );

  return (
    <div className="grid gap-6">
      <PageHeader
        title={t("platform_ai.roles.title")}
        meta={t("platform_ai.roles.intro")}
        actions={
          <>
            {roles.dataUpdatedAt > 0 ? (
              <span className="text-xs text-muted-foreground">
                {t("platform_ai.roles.read_at", { time: formatDateTime(new Date(roles.dataUpdatedAt)) })}
              </span>
            ) : null}
            <Button variant="ghost" size="sm" disabled={roles.isFetching} onClick={() => void roles.refetch()}>
              <RefreshCwIcon aria-hidden="true" />
              {t("platform_ai.roles.reload")}
            </Button>
          </>
        }
      />
      {notice ? (
        <Alert role="status" className="grid-cols-[1fr_auto] items-center gap-x-3">
          <AlertDescription className="col-start-1">
            <p className="text-foreground">{notice}</p>
          </AlertDescription>
          <Button variant="ghost" size="icon" aria-label={t("page.action.dismiss")} onClick={() => setNotice(null)}>
            <XIcon aria-hidden="true" />
          </Button>
        </Alert>
      ) : null}
      {roles.isPending ? (
        <Loading label={t("platform_ai.roles.loading")} className="grid gap-4 lg:grid-cols-2">
          <Skeleton className="h-56" />
          <Skeleton className="h-56" />
          <Skeleton className="h-56" />
        </Loading>
      ) : roles.isError ? (
        <Alert variant="destructive">
          <AlertTitle>{t("platform_ai.roles.load_failed")}</AlertTitle>
          <AlertDescription>
            <p>
              {t("platform_ai.roles.load_failed_note")} {errorMessage(roles.error)}
            </p>
            <Button size="sm" variant="outline" onClick={() => void roles.refetch()}>
              {t("platform_ai.roles.retry")}
            </Button>
          </AlertDescription>
        </Alert>
      ) : roles.data.roles.length === 0 ? (
        <EmptyState icon={<TriangleAlertIcon />} title={t("platform_ai.roles.empty")}>
          <p>{t("platform_ai.roles.more_note")}</p>
        </EmptyState>
      ) : (
        <>
          <div className="grid items-start gap-4 lg:grid-cols-2">
            {roles.data.roles.map((role) => (
              <RoleCard key={role.role} role={role} providers={roles.data.providers} onChanged={changed} />
            ))}
          </div>
          <p className="text-xs text-muted-foreground">{t("platform_ai.roles.more_note")}</p>
        </>
      )}
    </div>
  );
}
