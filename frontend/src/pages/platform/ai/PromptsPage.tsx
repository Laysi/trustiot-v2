import { useQuery } from "@tanstack/react-query";
import { FileTextIcon } from "lucide-react";
import { Link, useParams } from "react-router";

import { usePromptApi } from "../../../api/pending/PromptApiProvider.tsx";
import type { PromptSummary } from "../../../api/pending/prompts.ts";
import { EmptyState } from "../../../components/EmptyState.tsx";
import { Alert, AlertDescription, AlertTitle } from "../../../components/ui/alert.tsx";
import { Button, focusRing } from "../../../components/ui/button.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { PageHeader } from "../../dashboards/PageHeader.tsx";
import { PromptEditor } from "./PromptEditor.tsx";
import { promptPath } from "./paths.ts";
import { promptsKey } from "./queries.ts";
import { serviceLabelId } from "./serviceLabel.ts";
import { Badge, Loading, Skeleton } from "./ui.tsx";

function useServiceLabel() {
  const { t } = useI18n();
  return (service: string) => {
    const id = serviceLabelId(service);
    return id === null ? service : t(id);
  };
}

function ListEntry({ prompt, current }: { prompt: PromptSummary; current: boolean }) {
  const { t, formatDateTime } = useI18n();
  return (
    <Link
      to={promptPath(prompt.service, prompt.role)}
      {...(current ? { "aria-current": "page" as const } : {})}
      className={cn(
        "grid gap-1 rounded-md px-2.5 py-2 text-sm",
        focusRing,
        current ? "bg-accent" : "hover:bg-accent/60",
      )}
    >
      <span className="flex flex-wrap items-center gap-1.5">
        <span className="font-mono">{prompt.role}</span>
        <Badge tone={prompt.state === "modified" ? "brand" : "neutral"}>
          {prompt.state === "modified"
            ? t("platform_ai.prompts.state_modified")
            : t("platform_ai.prompts.state_default")}
        </Badge>
        {prompt.default_update_pending ? <Badge tone="warning">{t("platform_ai.prompts.state_update")}</Badge> : null}
        {prompt.on_hold ? <Badge tone="outline">{t("platform_ai.prompts.on_hold")}</Badge> : null}
      </span>
      <span className="text-xs text-muted-foreground">
        {t("platform_ai.prompts.list_version", { version: prompt.version, date: formatDateTime(prompt.updated_at) })}
      </span>
    </Link>
  );
}

/**
 * The system prompts of every service that keeps one, in one place (`/platform/ai/prompts`,
 * checklist J8; design: docs/design/platform-admin/ai-prompts.html): the list grouped by service
 * on the left, the selected prompt on the right (`PromptEditor`). A service that is not connected
 * yet (alerts, the knowledge base) shows up here when it is.
 */
export function PromptsPage() {
  const { t, errorMessage } = useI18n();
  const api = usePromptApi();
  const params = useParams();
  const serviceLabel = useServiceLabel();
  const prompts = useQuery({ queryKey: promptsKey, queryFn: ({ signal }) => api.listPrompts(signal) });

  const all = prompts.data?.flatMap((group) => group.prompts) ?? [];
  const selected =
    all.find((prompt) => prompt.service === params.service && prompt.role === params.role) ?? all[0] ?? null;

  return (
    <div className="grid gap-6">
      <PageHeader title={t("platform_ai.prompts.title")} meta={t("platform_ai.prompts.intro")} />
      {prompts.isPending ? (
        <Loading label={t("platform_ai.prompts.loading")} className="grid gap-4 md:grid-cols-[16rem_1fr]">
          <Skeleton className="h-64" />
          <Skeleton className="h-64" />
        </Loading>
      ) : prompts.isError ? (
        <Alert variant="destructive">
          <AlertTitle>{t("platform_ai.prompts.load_failed")}</AlertTitle>
          <AlertDescription>
            <p>{errorMessage(prompts.error)}</p>
            <Button size="sm" variant="outline" onClick={() => void prompts.refetch()}>
              {t("platform_ai.roles.retry")}
            </Button>
          </AlertDescription>
        </Alert>
      ) : selected === null ? (
        <EmptyState icon={<FileTextIcon />} title={t("platform_ai.prompts.empty")}>
          <p>{t("platform_ai.prompts.empty_body")}</p>
        </EmptyState>
      ) : (
        <div className="grid items-start gap-6 md:grid-cols-[16rem_minmax(0,1fr)]">
          <nav aria-label={t("platform_ai.prompts.list_label")} className="grid gap-3 rounded-lg border bg-card p-1.5">
            {prompts.data.map((group) => (
              <div key={group.service} className="grid gap-0.5">
                <div className="px-2.5 pt-1.5 text-xs font-medium text-muted-foreground">
                  {serviceLabel(group.service)}
                </div>
                {group.prompts.map((prompt) => (
                  <ListEntry
                    key={prompt.role}
                    prompt={prompt}
                    current={prompt.service === selected.service && prompt.role === selected.role}
                  />
                ))}
              </div>
            ))}
            <p className="px-2.5 pb-1.5 text-xs text-muted-foreground">{t("platform_ai.prompts.later")}</p>
          </nav>
          <section className="grid min-w-0 gap-3" aria-labelledby="prompt-heading">
            <h2 id="prompt-heading" className="text-lg font-semibold">
              {serviceLabel(selected.service)} · <span className="font-mono">{selected.role}</span>
            </h2>
            <PromptEditor
              key={`${selected.service}/${selected.role}`}
              service={selected.service}
              role={selected.role}
            />
          </section>
        </div>
      )}
    </div>
  );
}
