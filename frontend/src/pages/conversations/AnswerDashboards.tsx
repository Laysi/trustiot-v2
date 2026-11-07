import { useQuery } from "@tanstack/react-query";
import { CircleAlertIcon, CircleCheckIcon, PencilIcon } from "lucide-react";
import { type ReactNode, useId, useState } from "react";
import type { ConversationMessage } from "../../api/conversations.ts";
import { useDashboardApi } from "../../api/DashboardApiProvider.tsx";
import type { DashboardView } from "../../api/dashboards.ts";
import { AskInAnswer } from "../../components/ask/AskInAnswer.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useNearViewport } from "../../lib/use-near-viewport.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { dashboardVersionKey } from "../dashboards/queries.ts";
import { Tag } from "../dashboards/Tag.tsx";
import { RUNS_AT_ONCE, RunLimiter, RunLimiterContext } from "../dashboards/useDatasetRuns.ts";
import { useVersionContent, VersionItems } from "../dashboards/VersionItems.tsx";
import { DeviceNames } from "../device/DeviceName.tsx";
import { SaveButton, SavedDraftsProvider, SaveOutcome, useSaveDraft } from "./savedDrafts.tsx";

type DraftVersionRef = NonNullable<ConversationMessage["draft_versions"]>[number];

/**
 * What the conversation page's dashboards share: what was saved during the visit (so saving in an
 * answer shows in the draft panel and the other way round), and the limit on dataset runs, which
 * is the tenant's 2 at once however many dashboards the page draws.
 */
export function ConversationDashboards({ children }: { children: ReactNode }) {
  const [limiter] = useState(() => new RunLimiter(RUNS_AT_ONCE));
  return (
    <RunLimiterContext.Provider value={limiter}>
      <SavedDraftsProvider>
        <DeviceNames>{children}</DeviceNames>
      </SavedDraftsProvider>
    </RunLimiterContext.Provider>
  );
}

/**
 * The draft versions an answer shows: the last one of each draft, in the order the drafts were
 * first written. `draft_versions` lists every version the answer's question wrote, in write
 * order, so a draft written three times appears three times; the first two are what the
 * assistant tried on the way to the last, which is the answer.
 */
export function shownDraftVersions(versions: readonly DraftVersionRef[] | null | undefined): DraftVersionRef[] {
  const last = new Map<string, DraftVersionRef>();
  for (const item of versions ?? []) {
    // Setting an existing key keeps its place: the order is of the first write.
    last.set(item.dashboard_id, item);
  }
  return [...last.values()];
}

/**
 * The dashboards one answer wrote (docs/design/chat-dashboards/conversation.html): each draft's
 * last version, drawn with the same components as the dashboard page and the draft panel, with
 * "Save to dashboard list" for the conversation's owner when they are an editor or admin.
 */
export function AnswerDashboards({
  conversationId,
  versions,
}: {
  conversationId: string;
  versions: readonly DraftVersionRef[] | null | undefined;
}) {
  const shown = shownDraftVersions(versions);
  if (shown.length === 0) return null;
  return (
    <div className="grid min-w-0 gap-4">
      {shown.map((item) => (
        <AnswerDashboard
          key={`${item.dashboard_id}:${item.version}`}
          conversationId={conversationId}
          draftId={item.dashboard_id}
          version={item.version}
        />
      ))}
    </div>
  );
}

function AnswerDashboard({
  conversationId,
  draftId,
  version,
}: {
  conversationId: string;
  draftId: string;
  version: number;
}) {
  const { t, errorMessage } = useI18n();
  const api = useDashboardApi();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  // The same key as the draft panel's, so a version shown in both is read once.
  const view = useQuery({
    queryKey: dashboardVersionKey(tenant, draftId, version),
    queryFn: ({ signal }) => api.getDashboard(draftId, version, signal),
  });
  if (view.data !== undefined) {
    return <ReadAnswerDashboard conversationId={conversationId} draftId={draftId} view={view.data} />;
  }
  if (view.isError) {
    // Only this dashboard is lost; the answer's text and its other dashboards are not.
    return (
      <Alert role="status" className="grid-cols-[1rem_1fr]">
        <CircleAlertIcon aria-hidden="true" />
        <AlertDescription className="text-foreground">
          <p>{t("answer_draft.unreadable", { reason: errorMessage(view.error) })}</p>
        </AlertDescription>
      </Alert>
    );
  }
  return (
    <div aria-busy="true" className="grid gap-3 rounded-xl border bg-card p-4">
      <p role="status" className="sr-only">
        {t("draft.loading")}
      </p>
      <span aria-hidden="true" className="h-6 w-1/2 animate-pulse rounded bg-muted" />
      <span aria-hidden="true" className="h-24 animate-pulse rounded bg-muted" />
    </div>
  );
}

function ReadAnswerDashboard({
  conversationId,
  draftId,
  view,
}: {
  conversationId: string;
  draftId: string;
  view: DashboardView;
}) {
  const { t } = useI18n();
  const titleId = useId();
  const { version } = view;
  // The datasets run once the dashboard is near the window: a long conversation has many answers,
  // the tenant runs 60 datasets a minute and 2 at once (docs/architecture/ai-assistant.md, 「儀表板」).
  const { ref, near } = useNearViewport();
  const content = useVersionContent(draftId, version, near);
  const save = useSaveDraft(conversationId, draftId, version.version);
  return (
    <section
      ref={ref}
      data-answer-dashboard={draftId}
      aria-labelledby={titleId}
      className="grid min-w-0 gap-4 rounded-xl border bg-card p-4 text-card-foreground shadow-sm sm:p-5"
    >
      <div className="flex flex-wrap items-start justify-between gap-x-6 gap-y-3">
        <div className="grid min-w-0 flex-[1_1_14rem] gap-1.5">
          <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
            <h3 id={titleId} className="min-w-0 text-base font-semibold wrap-anywhere">
              {version.title}
            </h3>
            {save.saved !== undefined ? (
              <Tag icon={<CircleCheckIcon aria-hidden="true" className="text-success" />}>
                {t("dashboard_page.status_published", { version: save.saved.version })}
              </Tag>
            ) : (
              <Tag icon={<PencilIcon aria-hidden="true" />}>{t("draft.status", { version: version.version })}</Tag>
            )}
          </div>
          {version.description ? (
            <p className="text-sm text-pretty wrap-anywhere">{t("draft.note", { note: version.description })}</p>
          ) : null}
        </div>
        {/* The server says who may: an editor or admin who owns the conversation. */}
        {view.permissions.can_publish ? <SaveButton save={save} /> : null}
      </div>
      <SaveOutcome save={save} />
      <div className="min-w-0 rounded-lg border bg-background p-3 sm:p-4">
        <AskInAnswer>
          <VersionItems version={version} content={content} label={t("draft.preview_label")} layout="answer" />
        </AskInAnswer>
      </div>
    </section>
  );
}
