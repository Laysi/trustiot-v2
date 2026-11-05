import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { DownloadIcon, TriangleAlertIcon } from "lucide-react";
import { useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router";

import { useAssetsApi } from "../../api/AssetsApiProvider.tsx";
import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import type { DeviceModel } from "../../api/devices.ts";
import { ApiError, classifyError } from "../../api/errors.ts";
import { devicesOfModelPath, MODELS_PATH } from "../../app/paths.ts";
import { ErrorSummary, type SummaryItem } from "../../components/ErrorSummary.tsx";
import { FlashNotice, flashState } from "../../components/FlashNotice.tsx";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { FormField } from "../../components/ui/form-field.tsx";
import { Input } from "../../components/ui/input.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { downloadJson } from "../../lib/download.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { tenantName } from "../../session/tenant-name.ts";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { canWriteAssets } from "../device/DeviceNewPage.tsx";
import { useDeviceFormat } from "../device/format.ts";
import { deviceListKey, modelKey, tenantModelsKey } from "../device/queries.ts";
import { type Problem, problemsOf, problemText } from "../form-problems.ts";
import { StatusPage } from "../StatusPage.tsx";
import { WriteDenied } from "../WriteDenied.tsx";
import { DeleteModelDialog } from "./DeleteModelDialog.tsx";
import {
  MODEL_CONFLICT_FIELDS,
  type PointDraft,
  pointDraft,
  pointIndexOf,
  pointOf,
  validateName,
  validatePoints,
} from "./modelForm.ts";
import { type PointsChange, PointsEditor } from "./PointsEditor.tsx";

const MODEL_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** A new model (`/models/new`); a viewer is told why they cannot. */
export function ModelNewPage() {
  const { t } = useI18n();
  const tenant = useSession().session?.activeTenant;
  if (!canWriteAssets(tenant?.role)) {
    return (
      <WriteDenied
        title={t("model_edit.denied_title")}
        body={t("device_form.denied_body", { tenant: tenant ? tenantName(tenant, t) : "" })}
        backTo={MODELS_PATH}
        back={t("model_edit.back_to_models")}
      />
    );
  }
  return <ModelForm model={null} />;
}

/** One model (`/models/:modelId`): editable for editors and administrators, read-only (with export) for a viewer. */
export function ModelPage() {
  const { modelId = "" } = useParams();
  if (!MODEL_ID.test(modelId)) return <ModelNotFound />;
  return <ModelLoader key={modelId.toLowerCase()} modelId={modelId.toLowerCase()} />;
}

function ModelNotFound() {
  const { t } = useI18n();
  const tenant = useSession().session?.activeTenant;
  return (
    <StatusPage
      title={t("model_edit.not_found_title")}
      actions={
        <Button variant="outline" asChild>
          <Link to={MODELS_PATH}>{t("model_edit.back_to_models")}</Link>
        </Button>
      }
    >
      <p>{t("model_edit.not_found_body", { tenant: tenant ? tenantName(tenant, t) : "" })}</p>
    </StatusPage>
  );
}

function ModelLoader({ modelId }: { modelId: string }) {
  const { t, errorMessage } = useI18n();
  const api = useDeviceApi();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const model = useQuery({
    queryKey: modelKey(tenant, modelId),
    queryFn: ({ signal }) => api.getDeviceModel(modelId, signal),
  });
  if (model.isPending) return <Spinner label={t("model_edit.loading")} />;
  if (model.isError) {
    if (classifyError(model.error) === "not_found") return <ModelNotFound />;
    return (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("model_edit.load_failed", { reason: errorMessage(model.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void model.refetch()} disabled={model.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  }
  return <ModelForm model={model.data} />;
}

/** What a 409 `revision_mismatch` leaves: the revision the server is at. */
interface Conflict {
  current: number | undefined;
}

/**
 * The model form, for a new model (`model` is null) and for an existing one. The name is fixed
 * once the model exists. Saving replaces every data point, with the revision the form read; a
 * model someone else changed since answers 409, and the form offers to reload (the user's edits
 * are dropped then: unlike a device's few settings, a list of data points is not compared field by
 * field).
 */
function ModelForm({ model }: { model: DeviceModel | null }) {
  const { t, locale, errorMessage } = useI18n();
  const format = useDeviceFormat();
  const api = useAssetsApi();
  const devicesApi = useDeviceApi();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const tenant = useSession().session?.activeTenant;
  const tenantId = tenant?.id ?? "";
  const writable = canWriteAssets(tenant?.role);
  const ids = useRef(0);
  const nextId = () => {
    ids.current += 1;
    return ids.current;
  };
  const inputs = useRef(new Map<string, HTMLElement | null>());

  const [name, setName] = useState("");
  const [drafts, setDrafts] = useState<PointDraft[]>(() =>
    (model?.points ?? []).map((point) => pointDraft(point, nextId())),
  );
  const [base, setBase] = useState(model?.revision ?? 0);
  const [problems, setProblems] = useState<Problem[]>([]);
  const [rest, setRest] = useState<unknown>(null);
  const [attempt, setAttempt] = useState(0);
  const [conflict, setConflict] = useState<Conflict | null>(null);
  const [savedRevision, setSavedRevision] = useState<number | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [exportError, setExportError] = useState<unknown>(null);

  // How many devices use the model: what saving or deleting touches. Without the list it is unknown.
  const devices = useQuery({
    queryKey: deviceListKey(tenantId),
    queryFn: ({ signal }) => devicesApi.listDevices(signal),
    enabled: model !== null,
  });
  const using =
    model === null || !devices.isSuccess ? undefined : devices.data.filter((d) => d.model_id === model.model_id).length;

  // The model as the server has it now replaces the form after a save or a reload, never a background refetch.
  const adopt = (latest: DeviceModel) => {
    setDrafts(latest.points.map((point) => pointDraft(point, nextId())));
    setBase(latest.revision);
    setProblems([]);
  };

  const save = useMutation({
    mutationFn: () => {
      const points = drafts.map(pointOf);
      return model === null
        ? api.createDeviceModel({ name: name.trim(), points })
        : api.updateDeviceModel(model.model_id, { revision: base, points });
    },
    onSuccess: async (saved) => {
      queryClient.setQueryData(modelKey(tenantId, saved.model_id), saved);
      await queryClient.invalidateQueries({ queryKey: tenantModelsKey(tenantId) });
      if (model === null) {
        void navigate(MODELS_PATH, { state: flashState("models.flash.created", { name: saved.name }) });
        return;
      }
      adopt(saved);
      setSavedRevision(saved.revision);
    },
    onError: (error) => {
      setSavedRevision(null);
      if (error instanceof ApiError && error.body?.message_key === "device_model.revision_mismatch") {
        const latest = error.body.details?.current;
        setConflict({ current: typeof latest === "number" ? latest : undefined });
        return;
      }
      const refused = problemsOf(error, MODEL_CONFLICT_FIELDS);
      setProblems(refused.problems);
      setRest(refused.rest);
      if (refused.problems.length > 0) setAttempt((count) => count + 1);
    },
  });

  const reload = useMutation({
    mutationFn: () => devicesApi.getDeviceModel(model?.model_id ?? ""),
    onSuccess: (latest) => {
      queryClient.setQueryData(modelKey(tenantId, latest.model_id), latest);
      adopt(latest);
      setConflict(null);
    },
  });

  const submit = () => {
    setRest(null);
    setSavedRevision(null);
    const found = [...(model === null ? validateName(name) : []), ...validatePoints(drafts)];
    setProblems(found);
    if (found.length > 0) {
      setAttempt((count) => count + 1);
      return;
    }
    save.mutate();
  };

  const exportModel = useMutation({
    mutationFn: async () => {
      if (model === null) return;
      const document = await api.exportDeviceModel(model.model_id);
      downloadJson(`${model.name}.json`, document);
    },
    onMutate: () => setExportError(null),
    onError: (error) => setExportError(error),
  });

  const onPointsChange = (next: PointDraft[], change: PointsChange) => {
    setDrafts(next);
    setSavedRevision(null);
    setProblems((current) =>
      "clearAll" in change
        ? current.filter((problem) => pointIndexOf(problem.pointer) === null && problem.pointer !== "/points")
        : current.filter((problem) => !change.clear.includes(problem.pointer)),
    );
  };

  const summary: SummaryItem[] = problems.map((problem, index) => {
    const at = pointIndexOf(problem.pointer);
    const text = problemText(locale, problem);
    const draft = at === null ? undefined : drafts[at];
    return {
      key: `${problem.pointer}-${index}`,
      text:
        at === null
          ? problem.pointer === "/name"
            ? t("form.problem_line", { field: t("model_edit.name"), message: text })
            : text
          : t("model_edit.problem_line", {
              n: at + 1,
              key: draft && draft.key.trim() !== "" ? draft.key.trim() : t("model_edit.card_unnamed"),
              message: text,
            }),
      // A problem of the whole row (`/points/2`) leads to the row's first input.
      focus: () => (inputs.current.get(problem.pointer) ?? inputs.current.get(`/points/${at}/key`))?.focus(),
    };
  });

  const nameProblem = problems.find((problem) => problem.pointer === "/name");
  const listProblem = problems.find((problem) => problem.pointer === "/points");

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader
        title={model?.name ?? t("model_edit.new_title")}
        meta={
          model === null
            ? t("model_edit.new_description")
            : using === undefined
              ? t("model_edit.meta_no_count", { revision: base, time: format.minute(model.updated_at) })
              : t("model_edit.meta", { revision: base, count: using, time: format.minute(model.updated_at) })
        }
        crumbs={[{ label: t("models.title"), to: MODELS_PATH }]}
        tags={model ? <span className="text-sm text-muted-foreground">{t("model_edit.eyebrow")}</span> : undefined}
        actions={
          model ? (
            <>
              <Button variant="outline" onClick={() => exportModel.mutate()} disabled={exportModel.isPending}>
                {exportModel.isPending ? <Spinner /> : <DownloadIcon aria-hidden="true" />}
                {t("model_edit.export")}
              </Button>
              {writable ? (
                <Button variant="outline" className="text-destructive" onClick={() => setDeleting(true)}>
                  {t("model_edit.delete")}
                </Button>
              ) : null}
            </>
          ) : undefined
        }
      />

      {/* An import leaves its notice here, on the model it made. */}
      <FlashNotice />

      {exportError !== null ? (
        <Alert variant="destructive">
          <AlertDescription>
            <p>{t("model_edit.export_failed", { reason: errorMessage(exportError) })}</p>
          </AlertDescription>
        </Alert>
      ) : null}

      {!writable ? (
        <Alert role="status" className="grid-cols-[1fr]">
          <AlertDescription className="col-start-1">
            <p className="text-foreground">{t("model_edit.viewer_note")}</p>
          </AlertDescription>
        </Alert>
      ) : model !== null && using !== undefined ? (
        <p className="text-sm text-muted-foreground">
          {using > 0 ? (
            <>
              {t("model_edit.save_note", { count: using })}{" "}
              <Link to={devicesOfModelPath(model.model_id)} className="rounded-sm underline underline-offset-4">
                {t("model_edit.see_devices")}
              </Link>
            </>
          ) : (
            t("model_edit.save_note_none")
          )}
        </p>
      ) : null}

      {savedRevision !== null ? (
        <Alert role="status" className="grid-cols-[1fr]">
          <AlertDescription className="col-start-1">
            <p className="text-foreground">{t("model_edit.saved", { revision: savedRevision })}</p>
          </AlertDescription>
        </Alert>
      ) : null}
      {rest !== null ? (
        <Alert variant="destructive">
          <AlertDescription>
            <p>{errorMessage(rest)}</p>
          </AlertDescription>
        </Alert>
      ) : null}
      {conflict !== null ? (
        <Alert variant="destructive" className="grid-cols-[1rem_1fr]">
          <TriangleAlertIcon aria-hidden="true" />
          <AlertTitle>
            {conflict.current === undefined
              ? t("model_edit.conflict_title_unknown")
              : t("model_edit.conflict_title", { revision: conflict.current })}
          </AlertTitle>
          <AlertDescription>
            <p>{t("model_edit.conflict_body")}</p>
            {reload.isError ? <p>{errorMessage(reload.error)}</p> : null}
            <Button variant="outline" size="sm" onClick={() => reload.mutate()} disabled={reload.isPending}>
              {reload.isPending ? <Spinner /> : null}
              {t("model_edit.reload")}
            </Button>
          </AlertDescription>
        </Alert>
      ) : null}
      <ErrorSummary items={summary} attempt={attempt} />

      <form
        noValidate
        className="grid gap-6"
        onSubmit={(event) => {
          event.preventDefault();
          if (writable && !save.isPending) submit();
        }}
      >
        {model === null ? (
          <section className="grid gap-4 rounded-lg border bg-card px-4 py-5 sm:px-6">
            <FormField
              label={t("model_edit.name")}
              description={t("model_edit.name_hint")}
              error={nameProblem ? problemText(locale, nameProblem) : undefined}
              className="sm:max-w-sm"
            >
              {(control) => (
                <Input
                  {...control}
                  ref={(element) => {
                    inputs.current.set("/name", element);
                  }}
                  value={name}
                  autoComplete="off"
                  spellCheck={false}
                  onChange={(event) => {
                    setName(event.target.value);
                    setProblems((current) => current.filter((problem) => problem.pointer !== "/name"));
                  }}
                />
              )}
            </FormField>
          </section>
        ) : null}

        <section aria-labelledby="points-heading" className="grid gap-3 rounded-lg border bg-card px-4 py-5 sm:px-6">
          <div className="grid gap-1">
            <h2 id="points-heading" className="font-semibold">
              {t("model_edit.points")}
            </h2>
            <p className="text-sm text-muted-foreground">{t("model_edit.points_hint")}</p>
          </div>
          {listProblem ? (
            <p className="text-sm font-medium text-destructive">{problemText(locale, listProblem)}</p>
          ) : null}
          <PointsEditor
            drafts={drafts}
            {...(writable ? { onChange: onPointsChange } : {})}
            nextId={nextId}
            problems={problems}
            inputs={inputs}
          />
        </section>

        {writable ? (
          <div className="flex flex-wrap items-center justify-end gap-2">
            <Button variant="outline" asChild>
              <Link to={MODELS_PATH}>{t("device_form.cancel")}</Link>
            </Button>
            <Button type="submit" disabled={save.isPending}>
              {save.isPending ? <Spinner /> : null}
              {save.isPending ? t("device_form.saving") : t("model_edit.save")}
            </Button>
          </div>
        ) : null}
      </form>

      <DeleteModelDialog
        model={deleting && model ? model : null}
        onClose={() => setDeleting(false)}
        onDeleted={(deleted) => {
          void navigate(MODELS_PATH, { state: flashState("models.flash.deleted", { name: deleted }) });
        }}
      />
    </div>
  );
}
