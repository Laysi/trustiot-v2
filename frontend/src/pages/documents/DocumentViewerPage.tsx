import { useMutation, useQuery } from "@tanstack/react-query";
import {
  ChevronLeftIcon,
  ChevronRightIcon,
  DownloadIcon,
  HistoryIcon,
  RefreshCwIcon,
  SearchXIcon,
  Trash2Icon,
  TriangleAlertIcon,
  ZoomInIcon,
  ZoomOutIcon,
} from "lucide-react";
import { useEffect, useId, useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router";

import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import { useDocumentApi } from "../../api/pending/DocumentApiProvider.tsx";
import type { DocumentDetail } from "../../api/pending/documents.ts";
import { EmptyState } from "../../components/EmptyState.tsx";
import { flashState } from "../../components/FlashNotice.tsx";
import { RowMenu } from "../../components/RowMenu.tsx";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { controlClasses } from "../../components/ui/input.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { modelListKey } from "../device/queries.ts";
import { parseBox, positiveInteger } from "./box.ts";
import { DeleteDocumentDialog } from "./DeleteDocumentDialog.tsx";
import { startDownload } from "./download.ts";
import { isNotFound, isNotInPlan } from "./errors.ts";
import { ListSkeleton } from "./ListSkeleton.tsx";
import { NotInPlan } from "./NotInPlan.tsx";
import { PageImage } from "./PageImage.tsx";
import { DOCUMENTS_PATH, documentPath } from "./paths.ts";
import { documentKey } from "./queries.ts";
import { useDeleteDocument } from "./useDeleteDocument.ts";
import { usePageInfo } from "./usePageInfo.ts";
import { VersionStatus } from "./VersionStatus.tsx";

/**
 * One document's page view, `/documents/:documentId?version=&page=&box=` (design.md 「頁面檢視」):
 * the destination of the citations in the assistant's answers. The address names the version, the
 * page and the cited area, and is kept up to date as the user moves (replaced, not pushed); a value
 * that does not fit (an unknown version, a page past the end, a box that is not four numbers) falls
 * back to the document's latest version, its first page and no box. A document that was deleted
 * shows nothing of its content, only that it was deleted.
 */
export function DocumentViewerPage() {
  const { t, errorMessage } = useI18n();
  const { documentId = "" } = useParams();
  const api = useDocumentApi();
  const tenantId = useSession().session?.activeTenant?.id ?? "";
  const detail = useQuery({
    queryKey: documentKey(tenantId, documentId),
    queryFn: ({ signal }) => api.getDocument(documentId, signal),
  });

  const crumbs = [{ label: t("documents.title"), to: DOCUMENTS_PATH }];
  if (detail.isPending) {
    return (
      <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
        <PageHeader title={t("documents.title")} />
        <ListSkeleton label={t("documents.viewer.loading")} />
      </div>
    );
  }
  if (detail.data === undefined) {
    return (
      <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
        <PageHeader title={t("documents.title")} crumbs={[]} />
        {isNotInPlan(detail.error) ? (
          <NotInPlan />
        ) : isNotFound(detail.error) ? (
          <EmptyState
            centered
            icon={<SearchXIcon aria-hidden="true" />}
            title={t("documents.viewer.not_found.title")}
            action={<BackToLibrary />}
          >
            <p>{t("documents.viewer.not_found.body")}</p>
          </EmptyState>
        ) : (
          <Alert variant="destructive" className="grid-cols-[1rem_1fr_auto] items-center gap-x-3">
            <TriangleAlertIcon aria-hidden="true" />
            <AlertDescription className="col-start-2">
              <p>{t("documents.viewer.load_failed", { reason: errorMessage(detail.error) })}</p>
            </AlertDescription>
            <Button variant="outline" onClick={() => void detail.refetch()} disabled={detail.isFetching}>
              <RefreshCwIcon aria-hidden="true" />
              {t("page.action.retry")}
            </Button>
          </Alert>
        )}
      </div>
    );
  }
  if (detail.data.deleted_at !== null) {
    return <DeletedDocument document={detail.data} crumbs={crumbs} />;
  }
  return <Viewer document={detail.data} crumbs={crumbs} />;
}

function BackToLibrary() {
  const { t } = useI18n();
  return (
    <Button asChild variant="outline">
      <Link to={DOCUMENTS_PATH}>{t("documents.viewer.back_to_library")}</Link>
    </Button>
  );
}

/** A deleted document: its name from the tombstone and the day, and no content (decision Q13). */
function DeletedDocument({
  document,
  crumbs,
}: {
  document: DocumentDetail;
  crumbs: readonly { label: string; to: string }[];
}) {
  const { t, formatDateTime } = useI18n();
  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader title={document.name} crumbs={crumbs} />
      <EmptyState
        centered
        icon={<Trash2Icon aria-hidden="true" />}
        title={t("documents.viewer.deleted.title")}
        action={<BackToLibrary />}
      >
        <p>{t("documents.viewer.deleted.body", { time: formatDateTime(document.deleted_at ?? "") })}</p>
      </EmptyState>
    </div>
  );
}

function Viewer({ document, crumbs }: { document: DocumentDetail; crumbs: readonly { label: string; to: string }[] }) {
  const { t, errorMessage, formatDateTime } = useI18n();
  const api = useDocumentApi();
  const devices = useDeviceApi();
  const navigate = useNavigate();
  const tenant = useSession().session?.activeTenant;
  const tenantId = tenant?.id ?? "";
  const canWrite = tenant !== undefined && tenant !== null && tenant.role !== "viewer";
  const [params, setParams] = useSearchParams();
  const pageInputId = useId();
  const [zoomed, setZoomed] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [message, setMessage] = useState<string | null>(null);

  const latest = document.versions[0];
  const requestedVersion = positiveInteger(params.get("version"));
  const shown = document.versions.find((version) => version.version === requestedVersion) ?? latest;
  const models = useQuery({
    queryKey: modelListKey(tenantId),
    queryFn: ({ signal }) => devices.listDeviceModels(signal),
  });
  const modelName = models.data?.find((model) => model.model_id === document.model_id)?.name;

  const pageCount = shown?.page_count ?? null;
  // A page that is not one of the version's falls back to the first, and with it goes the cited
  // area: a box drawn on another page than the one it was cited on would mislead.
  const requestedPage = positiveInteger(params.get("page"));
  const pageFits = requestedPage !== null && pageCount !== null && requestedPage <= pageCount;
  const page = pageFits ? requestedPage : 1;
  // The aside says the page was opened from a citation only when the cited area fits the page
  // image, as the one `PageImage` draws (they read the same page information).
  const rawBox = pageFits ? params.get("box") : null;
  const pageInfo = usePageInfo(document.id, shown?.version ?? 0, page, shown !== undefined && pageCount !== null);
  const cited = pageInfo.data !== undefined && parseBox(rawBox, pageInfo.data.width, pageInfo.data.height) !== null;

  const [pageText, setPageText] = useState(String(page));
  useEffect(() => setPageText(String(page)), [page]);

  const go = (change: { version?: number; page?: number }) => {
    const next = new URLSearchParams(params);
    if (change.version !== undefined) {
      next.set("version", String(change.version));
      next.delete("box");
      if (change.page === undefined) next.set("page", "1");
    }
    if (change.page !== undefined) {
      next.set("page", String(change.page));
      // The cited area is on the cited page only.
      if (change.page !== page) next.delete("box");
    }
    setParams(next, { replace: true });
  };

  const commitPage = () => {
    const wanted = positiveInteger(pageText.trim());
    if (wanted === null || pageCount === null || wanted > pageCount) {
      setPageText(String(page));
      return;
    }
    go({ page: wanted });
  };

  const download = useMutation({
    mutationFn: async () => {
      if (shown === undefined) return;
      startDownload(
        await api.getDownloadUrl({ documentId: document.id, version: shown.version }),
        `${document.name}.pdf`,
      );
    },
    onError: (error) => setMessage(errorMessage(error)),
  });
  const remove = useDeleteDocument((deleted) =>
    navigate(DOCUMENTS_PATH, { replace: true, state: flashState("documents.message.deleted", { name: deleted.name }) }),
  );

  if (latest === undefined || shown === undefined) {
    return (
      <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
        <PageHeader title={document.name} crumbs={crumbs} />
        <p role="status">{t("documents.viewer.no_versions")}</p>
      </div>
    );
  }
  const oldVersion = requestedVersion !== null && shown.version !== latest.version;

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader
        title={document.name}
        crumbs={crumbs}
        meta={
          <span className="flex flex-wrap items-center gap-x-3 gap-y-1">
            <VersionStatus name={document.name} version={shown} />
            <span>
              {[
                modelName ?? (models.isPending ? "…" : t("documents.model_unknown")),
                pageCount === null ? t("documents.pages_counting") : t("documents.pages", { count: pageCount }),
                t("documents.viewer.uploaded", { time: formatDateTime(shown.uploaded_at) }),
              ].join(" · ")}
            </span>
          </span>
        }
        actions={
          <>
            {document.versions.length > 1 ? (
              <>
                <label htmlFor={`${pageInputId}-version`} className="sr-only">
                  {t("documents.viewer.version")}
                </label>
                <NativeSelect
                  id={`${pageInputId}-version`}
                  className="w-auto"
                  value={String(shown.version)}
                  onChange={(event) => go({ version: Number(event.target.value) })}
                >
                  {document.versions.map((version) => (
                    <option key={version.version} value={version.version}>
                      {version.version === latest.version
                        ? t("documents.viewer.version_current", { version: version.version })
                        : t("documents.version", { version: version.version })}
                    </option>
                  ))}
                </NativeSelect>
              </>
            ) : null}
            <Button variant="outline" onClick={() => download.mutate()} disabled={download.isPending}>
              <DownloadIcon aria-hidden="true" />
              {t("documents.viewer.download")}
            </Button>
            {canWrite ? (
              <RowMenu
                label={t("documents.actions_named", { name: document.name })}
                items={[
                  {
                    kind: "action",
                    label: t("documents.delete.menu"),
                    destructive: true,
                    onSelect: () => {
                      remove.reset();
                      setDeleting(true);
                    },
                  },
                ]}
              />
            ) : null}
          </>
        }
      />

      {oldVersion ? (
        <Alert role="status" className="grid-cols-[1rem_1fr_auto] items-center gap-x-3">
          <HistoryIcon aria-hidden="true" />
          <div className="col-start-2">
            <AlertTitle className="col-start-1">
              {t("documents.viewer.old.title", { version: shown.version })}
            </AlertTitle>
            <AlertDescription>
              <p>{t("documents.viewer.old.body", { latest: latest.version })}</p>
            </AlertDescription>
          </div>
          <Button variant="outline" size="sm" onClick={() => go({ version: latest.version })}>
            {t("documents.viewer.old.see_latest", { version: latest.version })}
          </Button>
        </Alert>
      ) : null}

      {message !== null ? (
        <p role="status" className="text-sm text-destructive">
          {message}
        </p>
      ) : null}

      {pageCount === null ? (
        <EmptyState centered icon={<RefreshCwIcon aria-hidden="true" />} title={t("documents.viewer.no_pages.title")}>
          <p>{t("documents.viewer.no_pages.body")}</p>
        </EmptyState>
      ) : (
        <div className="grid items-start gap-4 lg:grid-cols-[7rem_minmax(0,1fr)_16rem]">
          <nav aria-label={t("documents.viewer.thumbs")} className="max-h-[70dvh] overflow-y-auto max-lg:hidden">
            <ul className="grid gap-1">
              {Array.from({ length: pageCount }, (_, index) => index + 1).map((number) => (
                <li key={number}>
                  <Link
                    to={documentPath(document.id, { version: shown.version, page: number })}
                    replace
                    aria-current={number === page ? "page" : undefined}
                    aria-label={t("documents.viewer.go_to_page", { page: number })}
                    className={cn(
                      "block rounded-md border px-3 py-2 text-center text-sm tabular-nums hover:bg-accent",
                      number === page && "border-primary bg-accent font-semibold",
                      focusRing,
                    )}
                  >
                    {number}
                  </Link>
                </li>
              ))}
            </ul>
          </nav>

          <div className="grid min-w-0 gap-3">
            <div className="flex flex-wrap items-center gap-2">
              <Button
                variant="outline"
                size="icon"
                aria-label={t("documents.viewer.previous")}
                disabled={page <= 1}
                onClick={() => go({ page: page - 1 })}
              >
                <ChevronLeftIcon aria-hidden="true" />
              </Button>
              <label htmlFor={pageInputId} className="sr-only">
                {t("documents.viewer.page_number")}
              </label>
              <input
                id={pageInputId}
                inputMode="numeric"
                value={pageText}
                className={cn(controlClasses, "w-16 text-center")}
                onChange={(event) => setPageText(event.target.value)}
                onBlur={commitPage}
                onKeyDown={(event) => {
                  if (event.key === "Enter") commitPage();
                }}
              />
              <span className="text-sm text-muted-foreground">
                {t("documents.viewer.of_pages", { count: pageCount })}
              </span>
              <Button
                variant="outline"
                size="icon"
                aria-label={t("documents.viewer.next")}
                disabled={page >= pageCount}
                onClick={() => go({ page: page + 1 })}
              >
                <ChevronRightIcon aria-hidden="true" />
              </Button>
              <Button variant="ghost" className="ml-auto" aria-pressed={zoomed} onClick={() => setZoomed(!zoomed)}>
                {zoomed ? <ZoomOutIcon aria-hidden="true" /> : <ZoomInIcon aria-hidden="true" />}
                {t(zoomed ? "documents.viewer.zoom_out" : "documents.viewer.zoom_in")}
              </Button>
            </div>
            <div className="overflow-auto">
              <PageImage documentId={document.id} version={shown.version} page={page} rawBox={rawBox} zoomed={zoomed} />
            </div>
          </div>

          <aside aria-labelledby={`${pageInputId}-aside`} className="grid gap-3 rounded-lg border bg-card p-4 text-sm">
            <h2 id={`${pageInputId}-aside`} className="font-semibold">
              {t(cited ? "documents.viewer.aside.cited_title" : "documents.viewer.aside.title")}
            </h2>
            {cited ? <p>{t("documents.viewer.aside.cited_body")}</p> : null}
            <p className="text-muted-foreground">{t("documents.viewer.aside.no_text")}</p>
          </aside>
        </div>
      )}

      {canWrite ? (
        <DeleteDocumentDialog
          target={deleting ? { name: document.name, latestVersion: latest.version } : null}
          pending={remove.isPending}
          error={remove.error}
          onConfirm={() => remove.mutate({ id: document.id, name: document.name })}
          onClose={() => setDeleting(false)}
        />
      ) : null}
    </div>
  );
}
