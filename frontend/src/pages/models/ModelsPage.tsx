import { useMutation, useQuery } from "@tanstack/react-query";
import { PackageIcon } from "lucide-react";
import { type ReactNode, useState } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";

import { useAssetsApi } from "../../api/AssetsApiProvider.tsx";
import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import type { DeviceModelSummary } from "../../api/devices.ts";
import { devicesOfModelPath, modelPath, modelsImportPath, modelsNewPath } from "../../app/paths.ts";
import { FlashNotice, flashState } from "../../components/FlashNotice.tsx";
import { RowMenu } from "../../components/RowMenu.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { downloadJson } from "../../lib/download.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { canWriteAssets } from "../device/DeviceNewPage.tsx";
import { useDeviceFormat } from "../device/format.ts";
import { deviceListKey, modelListKey } from "../device/queries.ts";
import { DeleteModelDialog } from "./DeleteModelDialog.tsx";
import { ModelImportDialog } from "./ModelImportDialog.tsx";

/** `?import` opens the import dialog, so a link (the assistant's, the empty state's) can. */
const IMPORT_PARAM = "import";

/**
 * The tenant's device models (`/models`, checklist C1): name, data points, the devices that use
 * it and when it changed. Editors and administrators get "new", "import JSON" and the row actions
 * (edit, export, delete); a viewer reads the list and opens a model's read-only page, where the
 * export is. Layout and states follow docs/design/tenant-admin/ (`devices--models-*`).
 */
export function ModelsPage() {
  const { t, errorMessage } = useI18n();
  const devicesApi = useDeviceApi();
  const assets = useAssetsApi();
  const tenant = useSession().session?.activeTenant;
  const tenantId = tenant?.id ?? "";
  const canWrite = canWriteAssets(tenant?.role);
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const [deleting, setDeleting] = useState<DeviceModelSummary | null>(null);
  const [exportError, setExportError] = useState<unknown>(null);

  const models = useQuery({
    queryKey: modelListKey(tenantId),
    queryFn: ({ signal }) => devicesApi.listDeviceModels(signal),
  });
  // The devices only count how many use each model: without them the count is a dash.
  const devices = useQuery({
    queryKey: deviceListKey(tenantId),
    queryFn: ({ signal }) => devicesApi.listDevices(signal),
  });
  const using = (modelId: string): number | null =>
    devices.isSuccess ? devices.data.filter((device) => device.model_id === modelId).length : null;

  const importing = canWrite && params.has(IMPORT_PARAM);
  const setImporting = (open: boolean) =>
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        if (open) next.set(IMPORT_PARAM, "");
        else next.delete(IMPORT_PARAM);
        return next;
      },
      { replace: true },
    );

  const exportModel = useMutation({
    mutationFn: async (model: DeviceModelSummary) => {
      downloadJson(`${model.name}.json`, await assets.exportDeviceModel(model.model_id));
    },
    onMutate: () => setExportError(null),
    onError: (error) => setExportError(error),
  });

  const actions = canWrite ? (
    <>
      <Button variant="outline" onClick={() => setImporting(true)}>
        {t("models.import")}
      </Button>
      <Button asChild>
        <Link to={modelsNewPath()}>{t("models.add")}</Link>
      </Button>
    </>
  ) : undefined;

  let body: ReactNode;
  if (models.isPending) {
    body = <ListSkeleton label={t("models.loading")} />;
  } else if (models.isError) {
    body = (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("models.load_failed", { reason: errorMessage(models.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void models.refetch()} disabled={models.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  } else if (models.data.length === 0) {
    body = (
      <section className="grid justify-items-start gap-3 rounded-lg border bg-card px-4 py-8 sm:px-6">
        <PackageIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <h2 className="font-semibold">{t("models.empty.title")}</h2>
        <p className="text-sm text-muted-foreground">{t("models.empty.body")}</p>
        {canWrite ? (
          <>
            <p className="text-sm text-muted-foreground">{t("models.empty.import_hint")}</p>
            <div className="flex flex-wrap gap-2">
              <Button asChild>
                <Link to={modelsNewPath()}>{t("models.add")}</Link>
              </Button>
              <Button variant="outline" asChild>
                <Link to={modelsImportPath()}>{t("models.import")}</Link>
              </Button>
            </div>
          </>
        ) : (
          <p className="text-sm text-muted-foreground">{t("models.empty.viewer")}</p>
        )}
      </section>
    );
  } else {
    body = (
      <ModelList
        models={models.data}
        using={using}
        onExport={canWrite ? (model) => exportModel.mutate(model) : undefined}
        onDelete={canWrite ? setDeleting : undefined}
      />
    );
  }

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader title={t("models.title")} meta={t("models.description")} actions={actions} />
      <FlashNotice />
      {exportError !== null ? (
        <Alert variant="destructive">
          <AlertDescription>
            <p>{t("model_edit.export_failed", { reason: errorMessage(exportError) })}</p>
          </AlertDescription>
        </Alert>
      ) : null}
      {body}
      <DeleteModelDialog
        model={deleting}
        onClose={() => setDeleting(null)}
        onDeleted={(name) => {
          setDeleting(null);
          void navigate(".", { replace: true, state: flashState("models.flash.deleted", { name }) });
        }}
      />
      <ModelImportDialog open={importing} onClose={() => setImporting(false)} />
    </div>
  );
}

function ListSkeleton({ label }: { label: string }) {
  return (
    <section aria-busy="true" className="grid gap-4 rounded-lg border bg-card px-4 py-5 sm:px-6">
      <p role="status" className="sr-only">
        {label}
      </p>
      {[0, 1, 2].map((row) => (
        <div key={row} aria-hidden="true" className="flex items-center justify-between gap-4">
          <span className="h-3 w-[30%] animate-pulse rounded bg-muted" />
          <span className="h-3 w-[10%] animate-pulse rounded bg-muted" />
          <span className="h-3 w-[16%] animate-pulse rounded bg-muted" />
        </div>
      ))}
    </section>
  );
}

/** The models as a table on a wide screen and as a list on a phone; the one that does not fit is hidden. */
function ModelList({
  models,
  using,
  onExport,
  onDelete,
}: {
  models: readonly DeviceModelSummary[];
  using(modelId: string): number | null;
  onExport: ((model: DeviceModelSummary) => void) | undefined;
  onDelete: ((model: DeviceModelSummary) => void) | undefined;
}) {
  const format = useDeviceFormat();
  const { t } = format;
  const withActions = onExport !== undefined && onDelete !== undefined;
  const link = (model: DeviceModelSummary) => (
    <Link
      to={modelPath(model.model_id)}
      className={cn("rounded-sm font-medium wrap-anywhere underline-offset-4 hover:underline", focusRing)}
    >
      {model.name}
    </Link>
  );
  const count = (model: DeviceModelSummary) => {
    const used = using(model.model_id);
    if (used === null) return "—";
    if (used === 0) return "0";
    return (
      <Link
        to={devicesOfModelPath(model.model_id)}
        aria-label={t("models.devices_link", { count: used, name: model.name })}
        className={cn("rounded-sm underline underline-offset-4", focusRing)}
      >
        {used}
      </Link>
    );
  };
  const menu = (model: DeviceModelSummary) =>
    withActions ? (
      <RowMenu
        label={t("models.row_actions", { name: model.name })}
        items={[
          { kind: "link", label: t("models.action.edit"), to: modelPath(model.model_id) },
          { kind: "action", label: t("models.action.export"), onSelect: () => onExport(model) },
          { kind: "action", label: t("models.action.delete"), destructive: true, onSelect: () => onDelete(model) },
        ]}
      />
    ) : null;
  return (
    <section aria-label={t("models.list_label")} className="rounded-lg border bg-card text-card-foreground">
      <table className="hidden w-full text-sm sm:table">
        <caption className="sr-only">{t("models.caption")}</caption>
        <thead className="bg-muted/60 text-left text-muted-foreground">
          <tr>
            <th scope="col" className="px-4 py-2.5 font-medium">
              {t("models.column.model")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
              {t("models.column.points")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
              {t("models.column.devices")}
            </th>
            <th scope="col" className="px-4 py-2.5 font-medium whitespace-nowrap">
              {t("models.column.updated")}
            </th>
            {withActions ? (
              <th scope="col" className="px-4 py-2.5 font-medium">
                {t("devices.column.actions")}
              </th>
            ) : null}
          </tr>
        </thead>
        <tbody>
          {models.map((model) => (
            <tr key={model.model_id} className="border-t">
              <th scope="row" className="max-w-72 px-4 py-3 text-left font-normal">
                {link(model)}
              </th>
              <td className="px-3 py-3">{model.point_count}</td>
              <td className="px-3 py-3">{count(model)}</td>
              <td className="px-4 py-3 whitespace-nowrap">{format.minute(model.updated_at)}</td>
              {withActions ? <td className="px-4 py-1 text-right">{menu(model)}</td> : null}
            </tr>
          ))}
        </tbody>
      </table>
      <ul aria-label={t("models.caption")} className="grid divide-y sm:hidden">
        {models.map((model) => {
          const used = using(model.model_id);
          return (
            <li key={model.model_id} className="grid gap-1.5 px-4 py-3 text-sm">
              <div className="flex items-center justify-between gap-2">
                {link(model)}
                {menu(model)}
              </div>
              <div className="flex flex-wrap gap-x-3 gap-y-0.5 text-xs text-muted-foreground">
                <span>{t("models.points_count", { count: model.point_count })}</span>
                {used === null ? null : <span>{t("models.devices_count", { count: used })}</span>}
                <span>{t("models.updated_at", { time: format.minute(model.updated_at) })}</span>
              </div>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
