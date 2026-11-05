import { useQuery } from "@tanstack/react-query";
import { CircleCheckIcon, EyeIcon, PencilIcon } from "lucide-react";
import { useId, useState } from "react";
import { Link } from "react-router";

import { useDashboardApi } from "../../api/DashboardApiProvider.tsx";
import type { DashboardSummary, DashboardView } from "../../api/dashboards.ts";
import { dashboardPath } from "../../app/paths.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Label } from "../../components/ui/label.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { dashboardVersionKey } from "../dashboards/queries.ts";
import { Tag } from "../dashboards/Tag.tsx";
import { useVersionContent, VersionItems } from "../dashboards/VersionItems.tsx";
import { SaveButton, SaveOutcome, useSaveDraft } from "./savedDrafts.tsx";

/**
 * The conversation's dashboard drafts (docs/design/chat-dashboards/draft.html): the latest draft
 * with its preview, and, when the conversation has several, a choice among them by title. The
 * panel is the draft's own page: it never leaves the conversation, and only an editor or admin is
 * offered "Save to dashboard list". Discarding a draft and the version note are not here: the API
 * has neither (docs/design/chat-dashboards/design.md).
 *
 * `readOnly` is a shared conversation's reader (Laysi/trustiot-v2#1048): the same preview, drawn
 * by the same `VersionItems`, with nothing to save, whatever the reader's role, and no line
 * about who sees the draft (the API gives the reader the draft's `permissions` as all false, and
 * the service refuses a reader's publish).
 */
export function DraftPanel({
  conversationId,
  drafts,
  readOnly = false,
}: {
  conversationId: string;
  drafts: readonly DashboardSummary[];
  readOnly?: boolean;
}) {
  const { t } = useI18n();
  const chooseId = useId();
  const [chosen, setChosen] = useState<string | null>(null);
  // A draft that is gone after the lists were read again falls back to the latest.
  const draft = drafts.find((item) => item.dashboard_id === chosen) ?? drafts[0];
  if (draft === undefined) return null;
  return (
    <div className="grid content-start gap-4 rounded-xl border bg-muted p-4">
      {drafts.length > 1 ? (
        <div className="grid gap-1.5">
          <Label htmlFor={chooseId}>{t("draft.choose")}</Label>
          <NativeSelect id={chooseId} value={draft.dashboard_id} onChange={(event) => setChosen(event.target.value)}>
            {drafts.map((item) => (
              <option key={item.dashboard_id} value={item.dashboard_id}>
                {t("draft.option", { title: item.title, version: item.latest_version })}
              </option>
            ))}
          </NativeSelect>
        </div>
      ) : null}
      {/* Another draft starts from a new state: its own result of saving and its own results. */}
      <Draft key={draft.dashboard_id} conversationId={conversationId} draft={draft} readOnly={readOnly} />
    </div>
  );
}

function Draft({
  conversationId,
  draft,
  readOnly,
}: {
  conversationId: string;
  draft: DashboardSummary;
  readOnly: boolean;
}) {
  const { t, errorMessage } = useI18n();
  const api = useDashboardApi();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  // The version the list names is the one shown and the one saved, so the two are the same.
  const view = useQuery({
    queryKey: dashboardVersionKey(tenant, draft.dashboard_id, draft.latest_version),
    queryFn: ({ signal }) => api.getDashboard(draft.dashboard_id, draft.latest_version, signal),
  });

  if (view.isPending) {
    return <DraftSkeleton label={t("draft.loading")} />;
  }
  if (view.isError) {
    return (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("draft.load_failed", { reason: errorMessage(view.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void view.refetch()} disabled={view.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  }
  // A version's results are its own: another version starts with no results and no outcome.
  return (
    <DraftVersion
      key={view.data.version.version}
      conversationId={conversationId}
      draft={draft}
      view={view.data}
      readOnly={readOnly}
    />
  );
}

function DraftVersion({
  conversationId,
  draft,
  view,
  readOnly,
}: {
  conversationId: string;
  draft: DashboardSummary;
  view: DashboardView;
  readOnly: boolean;
}) {
  const { t } = useI18n();
  const { session } = useSession();
  const role = session?.activeTenant?.role;
  // Only these roles are offered it, as the dashboard list offers archiving (a role this build does
  // not know is a viewer); the server decides again.
  const canPublish = !readOnly && (role === "editor" || role === "admin");
  const { version } = view;
  const titleId = useId();
  const content = useVersionContent(draft.dashboard_id, version);
  // What saving made is shared with the answers that wrote the draft's versions: saved in one
  // place, saved in both (the SavedDraftsProvider of the page).
  const save = useSaveDraft(conversationId, draft.dashboard_id, version.version);
  const { saved } = save;

  return (
    <section aria-labelledby={titleId} className="grid gap-4">
      <div className="grid gap-4 rounded-xl border bg-card p-4 text-card-foreground shadow-sm sm:p-5">
        <div className="flex flex-wrap items-start justify-between gap-x-6 gap-y-3">
          <div className="grid min-w-0 flex-[1_1_14rem] gap-1.5">
            <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
              <h2 id={titleId} className="min-w-0 text-lg font-semibold wrap-anywhere">
                {version.title}
              </h2>
              {saved !== undefined ? (
                <Tag icon={<CircleCheckIcon aria-hidden="true" className="text-success" />}>
                  {t("dashboard_page.status_published", { version: saved.version })}
                </Tag>
              ) : (
                <Tag icon={<PencilIcon aria-hidden="true" />}>{t("draft.status", { version: version.version })}</Tag>
              )}
            </div>
            {/* Once saved, the result below says where it went. */}
            {saved === undefined && !readOnly ? (
              <p className="text-sm text-muted-foreground">
                {draft.saved != null ? t("draft.meta.saved") : t("draft.meta.new")}{" "}
                {draft.saved != null ? (
                  <Link className="underline underline-offset-4" to={dashboardPath(draft.saved.dashboard_id)}>
                    {t("draft.open_saved")}
                  </Link>
                ) : null}
              </p>
            ) : null}
            {version.description ? (
              <p className="text-sm text-pretty wrap-anywhere">{t("draft.note", { note: version.description })}</p>
            ) : null}
          </div>
          {canPublish ? <SaveButton save={save} /> : null}
        </div>

        {!canPublish ? (
          <Alert role="status" className="grid-cols-[1rem_1fr]">
            <EyeIcon aria-hidden="true" />
            <AlertDescription className="text-foreground">
              <p>{t(readOnly ? "draft.reader" : "draft.viewer")}</p>
            </AlertDescription>
          </Alert>
        ) : null}

        <SaveOutcome save={save} />
      </div>

      <div className="overflow-hidden rounded-xl border bg-background">
        <p className="border-b px-4 py-2 text-xs text-muted-foreground">{t("draft.preview_note")}</p>
        <div className="p-4">
          <VersionItems version={version} content={content} label={t("draft.preview_label")} layout="panel" />
        </div>
      </div>
    </section>
  );
}

/** The panel's shape while the draft loads, announced once. */
function DraftSkeleton({ label }: { label: string }) {
  const bar = "animate-pulse rounded bg-muted-foreground/15";
  return (
    <div aria-busy="true" className="grid gap-4">
      <p role="status" className="sr-only">
        {label}
      </p>
      <div aria-hidden="true" className="grid gap-3 rounded-xl border bg-card p-4">
        <span className={`${bar} h-6 w-1/2`} />
        <span className={`${bar} h-4 w-2/3`} />
      </div>
      <div aria-hidden="true" className="grid gap-3 rounded-xl border bg-background p-4 sm:grid-cols-2">
        <span className={`${bar} h-20`} />
        <span className={`${bar} h-20`} />
        <span className={`${bar} h-40 sm:col-span-2`} />
      </div>
    </div>
  );
}
