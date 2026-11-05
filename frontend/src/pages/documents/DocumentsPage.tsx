import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  BookOpenIcon,
  EyeIcon,
  RefreshCwIcon,
  SearchIcon,
  SearchXIcon,
  TriangleAlertIcon,
  UploadIcon,
} from "lucide-react";
import { type ReactNode, useEffect, useId, useState } from "react";
import { Link, useSearchParams } from "react-router";

import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import { useDocumentApi } from "../../api/pending/DocumentApiProvider.tsx";
import type { DocumentStatus, DocumentSummary } from "../../api/pending/documents.ts";
import { DEVICES_PATH } from "../../app/paths.ts";
import { EmptyState } from "../../components/EmptyState.tsx";
import { FlashNotice } from "../../components/FlashNotice.tsx";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { controlClasses } from "../../components/ui/input.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { modelListKey } from "../device/queries.ts";
import { DeleteDocumentDialog } from "./DeleteDocumentDialog.tsx";
import { DocumentTable } from "./DocumentTable.tsx";
import { startDownload } from "./download.ts";
import { isNotInPlan, isUnavailable } from "./errors.ts";
import { formatBytes } from "./format.ts";
import { ListSkeleton } from "./ListSkeleton.tsx";
import { NotInPlan } from "./NotInPlan.tsx";
import { ProgressBar } from "./ProgressBar.tsx";
import { documentKey, documentListKey, PROCESSING_POLL_MS } from "./queries.ts";
import { UploadDialog } from "./UploadDialog.tsx";
import { UploadRow } from "./UploadRow.tsx";
import { useDeleteDocument } from "./useDeleteDocument.ts";
import { useUploadQueue } from "./useUploadQueue.ts";

const MODEL_PARAM = "model";
const STATUS_PARAM = "status";
const SEARCH_PARAM = "q";
const STATUSES: readonly DocumentStatus[] = ["queued", "processing", "ready", "failed"];

/**
 * The document library (`/documents`, checklist J6, Laysi/trustiot-v2#199): the manuals and
 * fault-code tables uploaded per device model, each with its processing status as the AI sees it,
 * the upload dialog, new versions, retry and deletion. The layout follows
 * docs/design/ms4-ms5/documents.html. A viewer reads the list and nothing else.
 */
export function DocumentsPage() {
  const { t, errorMessage, formatDateTime, locale } = useI18n();
  const api = useDocumentApi();
  const devices = useDeviceApi();
  const queryClient = useQueryClient();
  const { session } = useSession();
  const tenant = session?.activeTenant;
  const tenantId = tenant?.id ?? "";
  const canWrite = tenant !== undefined && tenant !== null && tenant.role !== "viewer";
  const [params, setParams] = useSearchParams();
  const modelFilter = params.get(MODEL_PARAM) ?? "";
  const statusParam = params.get(STATUS_PARAM);
  const statusFilter = STATUSES.find((status) => status === statusParam) ?? "";
  const searchId = useId();
  // The address bar holds the search (replaced, not pushed, as the design says), so a reload or a pasted link keeps it.
  const query = params.get(SEARCH_PARAM) ?? "";
  const [uploadFor, setUploadFor] = useState<{ target: DocumentSummary | null } | null>(null);
  const [deleting, setDeleting] = useState<DocumentSummary | null>(null);
  const [message, setMessage] = useState<{ kind: "ok" | "error"; text: string } | null>(null);

  const list = useQuery({
    queryKey: documentListKey(tenantId),
    queryFn: ({ signal }) => api.listDocuments(signal),
    // A version that is waiting or being processed changes by itself: read the list again.
    refetchInterval: (state) =>
      state.state.data?.documents.some(
        (document) => document.latest_version.status === "queued" || document.latest_version.status === "processing",
      )
        ? PROCESSING_POLL_MS
        : false,
  });
  const models = useQuery({
    queryKey: modelListKey(tenantId),
    queryFn: ({ signal }) => devices.listDeviceModels(signal),
  });
  const modelNames = new Map((models.data ?? []).map((model) => [model.model_id, model.name]));
  const modelLabel = (modelId: string) =>
    modelNames.get(modelId) ?? (models.isPending ? "…" : t("documents.model_unknown"));

  const refresh = () => queryClient.invalidateQueries({ queryKey: documentListKey(tenantId) });
  const queue = useUploadQueue(api, (stored) => {
    void refresh();
    void queryClient.invalidateQueries({ queryKey: documentKey(tenantId, stored.id) });
  });
  const dialogOpen = uploadFor !== null;

  // An upload that finished while the dialog was closed has nothing more to say: the list shows it.
  useEffect(() => {
    if (!dialogOpen) queue.clear(["done"]);
  }, [dialogOpen, queue.clear]);

  const retry = useMutation({
    mutationFn: (document: DocumentSummary) =>
      api.retryProcessing({ documentId: document.id, version: document.latest_version.version }),
    onSuccess: (_result, document) =>
      setMessage({ kind: "ok", text: t("documents.message.retried", { name: document.name }) }),
    onError: (error) => setMessage({ kind: "error", text: errorMessage(error) }),
    onSettled: refresh,
  });
  const download = useMutation({
    mutationFn: async (document: DocumentSummary) => {
      const url = await api.getDownloadUrl({ documentId: document.id, version: document.latest_version.version });
      startDownload(url, `${document.name}.pdf`);
    },
    onError: (error) => setMessage({ kind: "error", text: errorMessage(error) }),
  });
  const remove = useDeleteDocument((document) => {
    setDeleting(null);
    setMessage({ kind: "ok", text: t("documents.message.deleted", { name: document.name }) });
  });

  const documents = list.data?.documents ?? [];
  const search = query.trim().toLowerCase();
  const shown = documents.filter(
    (document) =>
      (modelFilter === "" || document.model_id === modelFilter) &&
      (statusFilter === "" || document.latest_version.status === statusFilter) &&
      (search === "" || document.name.toLowerCase().includes(search)),
  );

  const setParam = (name: string, value: string) =>
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        if (value === "") next.delete(name);
        else next.set(name, value);
        return next;
      },
      { replace: true },
    );

  const openUpload = (target: DocumentSummary | null) => {
    queue.clear(["staged", "rejected", "done"]);
    setUploadFor({ target });
  };

  const header = (
    <PageHeader
      title={t("documents.title")}
      meta={t(canWrite || list.isPending ? "documents.description" : "documents.description_viewer")}
      actions={
        canWrite && list.isSuccess ? (
          <Button onClick={() => openUpload(null)}>
            <UploadIcon aria-hidden="true" />
            {t("documents.upload_button")}
          </Button>
        ) : null
      }
    />
  );

  if (list.isPending) {
    return (
      <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
        {header}
        <ListSkeleton label={t("documents.loading")} />
      </div>
    );
  }
  if (list.data === undefined) {
    if (isNotInPlan(list.error)) {
      return (
        <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
          <PageHeader title={t("documents.title")} />
          <NotInPlan />
        </div>
      );
    }
    return (
      <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
        <PageHeader title={t("documents.title")} />
        <Alert variant="destructive" className="grid-cols-[1rem_1fr_auto] items-center gap-x-3">
          <TriangleAlertIcon aria-hidden="true" />
          <AlertDescription className="col-start-2">
            {isUnavailable(list.error) ? (
              <>
                <p className="font-medium">{t("documents.unavailable.title")}</p>
                <p>{t("documents.unavailable.body")}</p>
              </>
            ) : (
              <p>{t("documents.load_failed", { reason: errorMessage(list.error) })}</p>
            )}
          </AlertDescription>
          <Button variant="outline" onClick={() => void list.refetch()} disabled={list.isFetching}>
            <RefreshCwIcon aria-hidden="true" />
            {t("page.action.retry")}
          </Button>
        </Alert>
      </div>
    );
  }

  const usage = list.data.usage;
  const modelOptions = (models.data ?? []).map((model) => ({ id: model.model_id, name: model.name }));
  const backgroundItems = dialogOpen
    ? []
    : queue.items.filter((item) => item.state === "uploading" || item.state === "failed");

  let body: ReactNode;
  if (documents.length === 0 && queue.items.length === 0) {
    body = (
      <EmptyState
        centered
        icon={<BookOpenIcon aria-hidden="true" />}
        title={t("documents.empty.title")}
        action={
          canWrite ? (
            <Button onClick={() => openUpload(null)}>
              <UploadIcon aria-hidden="true" />
              {t("documents.upload_button")}
            </Button>
          ) : undefined
        }
      >
        <p>{t(canWrite ? "documents.empty.body" : "documents.empty.body_viewer")}</p>
        <p className="text-xs">
          {t("documents.empty.models")}{" "}
          <Link to={DEVICES_PATH} className={cn("rounded-sm underline underline-offset-4", focusRing)}>
            {t("documents.empty.add_device")}
          </Link>
        </p>
      </EmptyState>
    );
  } else if (shown.length === 0 && documents.length > 0) {
    body = (
      <div role="status" className="grid justify-items-center gap-2 rounded-lg border px-4 py-12 text-center">
        <SearchXIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <p className="font-semibold">{t("documents.no_match.title")}</p>
        <p className="text-sm text-muted-foreground">{t("documents.no_match.body")}</p>
      </div>
    );
  } else if (shown.length > 0) {
    body = (
      <DocumentTable
        documents={shown}
        modelLabel={modelLabel}
        canWrite={canWrite}
        me={session?.user.id}
        onRetry={(document) => {
          setMessage(null);
          retry.mutate(document);
        }}
        onNewVersion={openUpload}
        onDownload={(document) => {
          setMessage(null);
          download.mutate(document);
        }}
        onDelete={(document) => {
          remove.reset();
          setDeleting(document);
        }}
        formatTime={formatDateTime}
      />
    );
  }

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      {header}

      <FlashNotice />

      {!canWrite ? (
        <Alert role="status" className="grid-cols-[1rem_1fr]">
          <EyeIcon aria-hidden="true" />
          <AlertTitle>{t("documents.readonly.title")}</AlertTitle>
          <AlertDescription>
            <p>{t("documents.readonly.body")}</p>
          </AlertDescription>
        </Alert>
      ) : null}

      {documents.length > 0 ? (
        <div className="flex flex-wrap items-end gap-3">
          <div className="w-full sm:w-52">
            <label htmlFor={`${searchId}-model`} className="sr-only">
              {t("documents.filter.model")}
            </label>
            <NativeSelect
              id={`${searchId}-model`}
              value={modelFilter}
              onChange={(event) => setParam(MODEL_PARAM, event.target.value)}
            >
              <option value="">{t("documents.filter.all_models")}</option>
              {modelOptions.map((model) => (
                <option key={model.id} value={model.id}>
                  {model.name}
                </option>
              ))}
            </NativeSelect>
          </div>
          <div className="w-full sm:w-44">
            <label htmlFor={`${searchId}-status`} className="sr-only">
              {t("documents.filter.status")}
            </label>
            <NativeSelect
              id={`${searchId}-status`}
              value={statusFilter}
              onChange={(event) => setParam(STATUS_PARAM, event.target.value)}
            >
              <option value="">{t("documents.filter.all_statuses")}</option>
              {STATUSES.map((status) => (
                <option key={status} value={status}>
                  {t(`documents.filter.status_${status}`)}
                </option>
              ))}
            </NativeSelect>
          </div>
          <div className="relative w-full sm:w-72">
            <label htmlFor={searchId} className="sr-only">
              {t("documents.search.label")}
            </label>
            <SearchIcon
              aria-hidden="true"
              className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground"
            />
            <input
              id={searchId}
              type="search"
              value={query}
              placeholder={t("documents.search.placeholder")}
              className={cn(controlClasses, "pl-9")}
              onChange={(event) => setParam(SEARCH_PARAM, event.target.value)}
            />
          </div>
          <div className="grid w-full gap-1 text-xs text-muted-foreground sm:ml-auto sm:w-56">
            <p className="flex justify-between">
              <span>{t("documents.usage.used", { size: formatBytes(usage.used_bytes, locale) })}</span>
              <span>{t("documents.usage.limit", { size: formatBytes(usage.limit_bytes, locale) })}</span>
            </p>
            <ProgressBar
              label={t("documents.usage.label")}
              value={usage.used_bytes}
              max={Math.max(usage.limit_bytes, 1)}
              valueText={t("documents.usage.text", {
                used: formatBytes(usage.used_bytes, locale),
                limit: formatBytes(usage.limit_bytes, locale),
              })}
            />
          </div>
        </div>
      ) : null}

      {backgroundItems.length > 0 ? (
        <section aria-label={t("documents.background.label")} className="grid gap-2">
          <h2 className="text-sm font-medium">{t("documents.background.title")}</h2>
          <ul className="grid gap-2">
            {backgroundItems.map((item) => (
              <UploadRow key={item.id} item={item} onRemove={queue.remove} onRetry={queue.retry} />
            ))}
          </ul>
        </section>
      ) : null}

      {body}

      <p
        role="status"
        className={cn("min-h-4 text-xs", message?.kind === "error" ? "text-destructive" : "text-foreground")}
      >
        {message?.text}
      </p>

      {canWrite ? (
        <>
          <UploadDialog
            open={dialogOpen}
            onOpenChange={(open) => {
              if (open) return;
              setUploadFor(null);
              queue.clear(["staged", "rejected", "done"]);
            }}
            queue={queue}
            models={modelOptions}
            modelsError={models.isError ? models.error : null}
            onRetryModels={() => void models.refetch()}
            documents={documents}
            usage={usage}
            target={uploadFor?.target ?? null}
          />
          <DeleteDocumentDialog
            target={deleting === null ? null : { name: deleting.name, latestVersion: deleting.latest_version.version }}
            pending={remove.isPending}
            error={remove.error}
            onConfirm={() => deleting !== null && remove.mutate(deleting)}
            onClose={() => setDeleting(null)}
          />
        </>
      ) : null}
    </div>
  );
}
