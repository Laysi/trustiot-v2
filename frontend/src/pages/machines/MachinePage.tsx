import { useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ClockIcon,
  CompassIcon,
  FactoryIcon,
  HistoryIcon,
  PencilIcon,
  PlayIcon,
  PlusIcon,
  RepeatIcon,
  SquareIcon,
} from "lucide-react";
import { type ReactNode, useCallback, useEffect, useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router";

import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import { classifyError } from "../../api/errors.ts";
import { useMachineApi } from "../../api/pending/MachineApiProvider.tsx";
import {
  type ChangeEntry,
  type FieldChange,
  isNotInPlan,
  isServiceUnavailable,
  type MachineDetail,
  type MachineRelation,
  type ProductionRun,
  type RunOverlap,
} from "../../api/pending/machines.ts";
import { devicePath, MACHINES_PATH, machinePath } from "../../app/paths.ts";
import { FlashNotice, flashState } from "../../components/FlashNotice.tsx";
import { RowMenu } from "../../components/RowMenu.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useMediaQuery } from "../../lib/use-media-query.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { Tag } from "../dashboards/Tag.tsx";
import { deviceListKey, modelKey } from "../device/queries.ts";
import { CLOCK_TICK_MS, useNow } from "../device/useNow.ts";
import { StatusPage } from "../StatusPage.tsx";
import { AssignDialog } from "./AssignDialog.tsx";
import { canEditMachines } from "./access.ts";
import { ChangeoverDialog } from "./ChangeoverDialog.tsx";
import { ChangeToast, type ToastRecord } from "./ChangeToast.tsx";
import { DeleteMachineDialog } from "./DeleteMachineDialog.tsx";
import { EndRunAction } from "./EndRunAction.tsx";
import { type MachineFormat, useMachineFormat } from "./format.ts";
import { MachineFormDialog } from "./MachineFormDialog.tsx";
import { PlanWall } from "./PlanWall.tsx";
import { PartNumber, RequestError, Toast } from "./parts.tsx";
import { changesKey, machineKey, machineListKey, runsKey } from "./queries.ts";
import { type RelationRole, RelationsDialog, roleOf } from "./RelationsDialog.tsx";
import { type RunForm, RunFormDialog } from "./RunFormDialog.tsx";
import { dayStart, RunTimeline, TIMELINE_DAYS } from "./RunTimeline.tsx";

const DAY = 86_400_000;
/** How many lines of the change record show before "Show all". */
const CHANGES_SHOWN = 5;

type Dialog =
  | { kind: "changeover" }
  | { kind: "edit" }
  | { kind: "delete" }
  | { kind: "assign" }
  | { kind: "relations" }
  | { kind: "run"; form: RunForm };

/**
 * One machine (`/machines/:machineId`, `?run=<run ID>` marks one record): what it is making now
 * with the changeover and end buttons, the last seven days of records with backfill and
 * correction, the change record, the devices and data points that belong to it, and its
 * relations (docs/design/ms4-ms5/machines.html, states 2 to 6 and 8). Editors and administrators
 * see the buttons that change things; a viewer reads only.
 */
export function MachinePage() {
  const { t } = useI18n();
  const { machineId = "" } = useParams();
  const api = useMachineApi();
  const tenant = useSession().session?.activeTenant;
  const tenantId = tenant?.id ?? "";
  const machine = useQuery({
    queryKey: machineKey(tenantId, machineId),
    queryFn: ({ signal }) => api.getMachine(machineId, signal),
  });

  if (machine.isPending) {
    return <PageSkeleton label={t("machines.detail.loading")} />;
  }
  if (machine.isError) {
    if (classifyError(machine.error) === "not_found") return <MachineNotFound />;
    return (
      <div className="grid gap-4">
        <Crumbs />
        {isNotInPlan(machine.error) ? (
          <PlanWall />
        ) : (
          <RequestError error={machine.error} onRetry={() => void machine.refetch()} retrying={machine.isFetching} />
        )}
      </div>
    );
  }
  return <MachineView machine={machine.data} canEdit={canEditMachines(tenant?.role)} />;
}

function Crumbs() {
  const { t } = useI18n();
  return (
    <nav aria-label={t("page.breadcrumbs")}>
      <Link
        to={MACHINES_PATH}
        className={cn("rounded-sm text-sm text-muted-foreground underline underline-offset-4", focusRing)}
      >
        {t("machines.title")}
      </Link>
    </nav>
  );
}

function MachineNotFound() {
  const { t } = useI18n();
  return (
    <StatusPage
      title={t("machines.not_found.title")}
      icon={<CompassIcon aria-hidden="true" />}
      actions={
        <Button variant="outline" asChild>
          <Link to={MACHINES_PATH}>{t("machines.not_found.back")}</Link>
        </Button>
      }
    >
      <p>{t("machines.not_found.body")}</p>
    </StatusPage>
  );
}

function PageSkeleton({ label }: { label: string }) {
  return (
    <section aria-busy="true" className="grid gap-4">
      <p role="status" className="sr-only">
        {label}
      </p>
      <span aria-hidden="true" className="h-8 w-1/3 animate-pulse rounded bg-muted" />
      <span aria-hidden="true" className="h-32 animate-pulse rounded-lg bg-muted" />
      <span aria-hidden="true" className="h-64 animate-pulse rounded-lg bg-muted" />
    </section>
  );
}

function MachineView({ machine, canEdit }: { machine: MachineDetail; canEdit: boolean }) {
  const { t } = useI18n();
  const api = useMachineApi();
  const devicesApi = useDeviceApi();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const tenantId = useSession().session?.activeTenant?.id ?? "";
  const format = useMachineFormat();
  const wide = useMediaQuery("(min-width: 640px)");
  const now = useNow(CLOCK_TICK_MS);
  const [params] = useSearchParams();
  const markedRun = params.get("run");
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [toast, setToast] = useState<ToastRecord | null>(null);
  const closeToast = useCallback(() => setToast(null), []);
  const [showAllChanges, setShowAllChanges] = useState(false);

  const from = dayStart(now, TIMELINE_DAYS - 1, format.timeZone);
  const runs = useQuery({
    queryKey: runsKey(tenantId, machine.machine_id, from),
    queryFn: ({ signal }) =>
      api.listRuns(
        machine.machine_id,
        { from: new Date(from).toISOString(), to: new Date(Date.now() + DAY).toISOString() },
        signal,
      ),
  });
  const changes = useQuery({
    queryKey: changesKey(tenantId, machine.machine_id),
    queryFn: ({ signal }) => api.listChanges(machine.machine_id, signal),
  });
  const machines = useQuery({
    queryKey: machineListKey(tenantId),
    queryFn: ({ signal }) => api.listMachines(signal),
  });
  const devices = useQuery({
    queryKey: deviceListKey(tenantId),
    queryFn: ({ signal }) => devicesApi.listDevices(signal),
  });

  const run = machine.current_run;
  const sortedRuns = [...(runs.data ?? [])].sort((a, b) => Date.parse(b.started_at) - Date.parse(a.started_at));

  // The record the link names: scrolled to and marked once the records are in.
  const markedFound = markedRun !== null && sortedRuns.some((item) => item.run_id === markedRun);
  useEffect(() => {
    if (markedFound) document.getElementById(`run-${markedRun}`)?.scrollIntoView?.({ block: "center" });
  }, [markedFound, markedRun]);

  const openCorrectionOf = async (overlap: RunOverlap) => {
    let found = sortedRuns.find((item) => item.run_id === overlap.runId);
    if (found === undefined) {
      const start = Date.parse(overlap.startedAt);
      const end = overlap.endedAt === null ? start + DAY : Date.parse(overlap.endedAt);
      const around = await api.listRuns(machine.machine_id, {
        from: new Date(start).toISOString(),
        to: new Date(end).toISOString(),
      });
      found = around.find((item) => item.run_id === overlap.runId);
    }
    if (found !== undefined) setDialog({ kind: "run", form: { kind: "correct", run: found } });
  };

  const changeoverButton = (
    <Button size="lg" onClick={() => setDialog({ kind: "changeover" })}>
      {run ? <RepeatIcon aria-hidden="true" /> : <PlayIcon aria-hidden="true" />}
      {run ? t("machines.changeover.button") : t("machines.start.button")}
    </Button>
  );
  const unavailableQueries = [runs, changes].find((query) => query.isError && isServiceUnavailable(query.error));

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <Crumbs />
      <PageHeader
        title={machine.name}
        tags={machine.utility ? <Tag>{t("machines.status.utility")}</Tag> : undefined}
        meta={machine.description}
        actions={
          canEdit ? (
            <>
              <Button variant="ghost" onClick={() => setDialog({ kind: "edit" })}>
                <PencilIcon aria-hidden="true" />
                {t("machines.edit")}
              </Button>
              <RowMenu
                label={t("machines.more", { machine: machine.name })}
                items={[
                  {
                    kind: "action",
                    label: t("machines.delete.menu"),
                    destructive: true,
                    onSelect: () => setDialog({ kind: "delete" }),
                  },
                ]}
              />
            </>
          ) : undefined
        }
      />
      <FlashNotice />
      {unavailableQueries ? <RequestError error={unavailableQueries.error} /> : null}

      {machine.utility ? null : (
        <section
          aria-labelledby="machine-now"
          className="flex flex-wrap items-center justify-between gap-4 rounded-lg bg-brand-soft px-5 py-5 text-foreground"
        >
          <div className="grid gap-1.5">
            <h2 id="machine-now" className="text-sm font-medium text-muted-foreground">
              {t("machines.now.title")}
            </h2>
            {run ? (
              <>
                <PartNumber className="text-3xl">{run.part_number}</PartNumber>
                <p className="flex flex-wrap items-center gap-x-4 gap-y-1 text-sm">
                  <span className="inline-flex items-center gap-1">
                    <ClockIcon aria-hidden="true" className="size-4" />
                    {t("machines.now.started", { time: format.when(run.started_at, now) })}
                  </span>
                  <span>{format.elapsed(now - Date.parse(run.started_at))}</span>
                  {run.work_order ? <span>{t("machines.card.work_order", { order: run.work_order })}</span> : null}
                  {run.target_quantity !== null ? (
                    <span>{t("machines.now.target", { count: run.target_quantity })}</span>
                  ) : null}
                </p>
              </>
            ) : (
              <p className="text-lg font-medium">{t("machines.now.none")}</p>
            )}
          </div>
          {canEdit ? (
            <div className="flex flex-wrap items-center gap-2">
              {wide ? changeoverButton : null}
              {run ? <EndRunAction machine={machine} run={run} /> : null}
            </div>
          ) : null}
        </section>
      )}

      <div className="grid gap-4 lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
        <div className="grid content-start gap-4">
          <Card
            id="machine-runs"
            title={t("machines.runs.title")}
            description={t("machines.runs.description", { zone: format.timeZone })}
            action={
              canEdit ? (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => setDialog({ kind: "run", form: { kind: "backfill" } })}
                >
                  <PlusIcon aria-hidden="true" />
                  {t("machines.runs.backfill")}
                </Button>
              ) : undefined
            }
          >
            {runs.isPending ? (
              <p role="status" className="text-sm text-muted-foreground">
                {t("machines.runs.loading")}
              </p>
            ) : runs.isError ? (
              isServiceUnavailable(runs.error) ? null : (
                <RequestError error={runs.error} onRetry={() => void runs.refetch()} retrying={runs.isFetching} />
              )
            ) : (
              <div className="grid gap-4">
                <RunTimeline runs={sortedRuns} now={now} />
                {markedRun !== null && !markedFound ? (
                  <Alert role="status" className="grid-cols-[1fr]">
                    <AlertDescription>
                      <p>{t("machines.runs.marked_missing")}</p>
                    </AlertDescription>
                  </Alert>
                ) : null}
                {sortedRuns.length === 0 ? (
                  <p role="status" className="text-sm text-muted-foreground">
                    {t("machines.runs.none")}
                  </p>
                ) : (
                  <RunTable
                    runs={sortedRuns}
                    marked={markedRun}
                    canEdit={canEdit}
                    wide={wide}
                    now={now}
                    format={format}
                    onCorrect={(item) => setDialog({ kind: "run", form: { kind: "correct", run: item } })}
                  />
                )}
              </div>
            )}
          </Card>
          <Card id="machine-changes" title={t("machines.changes.title")}>
            {changes.isPending ? (
              <p role="status" className="text-sm text-muted-foreground">
                {t("machines.changes.loading")}
              </p>
            ) : changes.isError ? (
              isServiceUnavailable(changes.error) ? null : (
                <RequestError
                  error={changes.error}
                  onRetry={() => void changes.refetch()}
                  retrying={changes.isFetching}
                />
              )
            ) : changes.data.length === 0 ? (
              <p className="text-sm text-muted-foreground">{t("machines.changes.none")}</p>
            ) : (
              <div className="grid gap-3">
                <ol className="grid gap-3">
                  {(showAllChanges ? changes.data : changes.data.slice(0, CHANGES_SHOWN)).map((entry) => (
                    <ChangeLine key={entry.change_id} entry={entry} now={now} format={format} />
                  ))}
                </ol>
                {changes.data.length > CHANGES_SHOWN ? (
                  <Button
                    variant="link"
                    aria-expanded={showAllChanges}
                    className="justify-self-start"
                    onClick={() => setShowAllChanges((current) => !current)}
                  >
                    {showAllChanges
                      ? t("machines.changes.show_less")
                      : t("machines.changes.show_all", { count: changes.data.length })}
                  </Button>
                ) : null}
              </div>
            )}
          </Card>
        </div>
        <div className="grid content-start gap-4">
          <Card
            id="machine-points"
            title={t("machines.points.title")}
            description={t("machines.points.description")}
            action={
              canEdit ? (
                <Button variant="ghost" size="sm" onClick={() => setDialog({ kind: "assign" })}>
                  {t("machines.points.manage")}
                </Button>
              ) : undefined
            }
          >
            <AssignedPoints machine={machine} deviceList={devices.data} />
          </Card>
          <Card
            id="machine-relations"
            title={t("machines.relations.card_title")}
            action={
              canEdit ? (
                <Button
                  variant="ghost"
                  size="sm"
                  aria-label={t("machines.relations.edit_aria")}
                  onClick={() => setDialog({ kind: "relations" })}
                >
                  {t("machines.relations.edit")}
                </Button>
              ) : undefined
            }
          >
            <RelationList machine={machine} machines={machines.data} />
          </Card>
        </div>
      </div>

      {canEdit && !wide && !machine.utility ? (
        <div className="fixed inset-x-0 bottom-0 z-30 border-t bg-background px-4 py-3">
          <Button
            size="lg"
            className="w-full"
            aria-label={t(run ? "machines.changeover.button_for" : "machines.start.button_for", {
              machine: machine.name,
            })}
            onClick={() => setDialog({ kind: "changeover" })}
          >
            {run ? <RepeatIcon aria-hidden="true" /> : <PlayIcon aria-hidden="true" />}
            {run ? t("machines.changeover.button") : t("machines.start.button")}
          </Button>
        </div>
      ) : null}

      {dialog?.kind === "changeover" ? (
        <ChangeoverDialog
          machine={machine}
          current={run}
          onClose={() => setDialog(null)}
          onDone={(result) => {
            setToast({ machineId: machine.machine_id, result });
            setDialog(null);
          }}
        />
      ) : null}
      {dialog?.kind === "run" ? (
        <RunFormDialog
          // A correction of another record replaces the form, so each one starts from its own values.
          key={dialog.form.kind === "correct" ? dialog.form.run.run_id : "backfill"}
          machine={machine}
          form={dialog.form}
          onClose={() => setDialog(null)}
          onDone={() => setDialog(null)}
          onCorrectOverlap={(overlap) => void openCorrectionOf(overlap)}
        />
      ) : null}
      {dialog?.kind === "edit" ? (
        <MachineFormDialog machine={machine} onClose={() => setDialog(null)} onDone={() => setDialog(null)} />
      ) : null}
      {dialog?.kind === "assign" ? (
        <AssignDialog machine={machine} onClose={() => setDialog(null)} onDone={() => setDialog(null)} />
      ) : null}
      {dialog?.kind === "relations" ? (
        <RelationsDialog
          machineId={machine.machine_id}
          machineName={machine.name}
          relations={machine.relations}
          others={machines.data ?? []}
          onClose={() => setDialog(null)}
          onDone={() => setDialog(null)}
        />
      ) : null}
      {dialog?.kind === "delete" ? (
        <DeleteMachineDialog
          machine={machine}
          onClose={() => setDialog(null)}
          onDeleted={() => {
            void queryClient.removeQueries({ queryKey: machineKey(tenantId, machine.machine_id) });
            void navigate(MACHINES_PATH, { state: flashState("machines.flash.deleted", { name: machine.name }) });
          }}
        />
      ) : null}
      {toast ? (
        <Toast onClose={closeToast}>
          <ChangeToast done={toast} name={machine.name} format={format} />
        </Toast>
      ) : null}
    </div>
  );
}

function Card({
  id,
  title,
  description,
  action,
  children,
}: {
  id: string;
  title: string;
  description?: string;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section aria-labelledby={id} className="rounded-lg border bg-card text-card-foreground">
      <header className="flex flex-wrap items-start justify-between gap-2 border-b px-4 py-3">
        <div className="grid gap-0.5">
          <h2 id={id} className="font-semibold">
            {title}
          </h2>
          {description ? <p className="text-sm text-muted-foreground">{description}</p> : null}
        </div>
        {action}
      </header>
      <div className="px-4 py-4">{children}</div>
    </section>
  );
}

/** The records as a table on a wide screen and as cards on a phone, the one that fits only. */
function RunTable({
  runs,
  marked,
  canEdit,
  wide,
  now,
  format,
  onCorrect,
}: {
  runs: readonly ProductionRun[];
  marked: string | null;
  canEdit: boolean;
  wide: boolean;
  now: number;
  format: MachineFormat;
  onCorrect(run: ProductionRun): void;
}) {
  const { t } = format;
  const tags = (run: ProductionRun) => (
    <>
      {run.ended_at === null ? (
        <Tag icon={<PlayIcon aria-hidden="true" />}>{t("machines.runs.in_progress")}</Tag>
      ) : null}
      {run.source === "backfill" ? <Tag>{t("machines.runs.backfilled")}</Tag> : null}
      {run.revisions > 0 ? (
        <Tag icon={<HistoryIcon aria-hidden="true" />}>{t("machines.runs.corrected", { count: run.revisions })}</Tag>
      ) : null}
    </>
  );
  const correct = (run: ProductionRun) =>
    canEdit ? (
      <Button
        variant="ghost"
        size="sm"
        aria-label={t("machines.runs.correct_for", { part: run.part_number, time: format.when(run.started_at, now) })}
        onClick={() => onCorrect(run)}
      >
        {t("machines.runs.correct")}
      </Button>
    ) : null;
  const end = (run: ProductionRun) => (run.ended_at ? format.when(run.ended_at, now) : "—");
  const markedClass = (run: ProductionRun) => (run.run_id === marked ? "bg-brand-soft" : "");
  if (wide) {
    return (
      <table className="w-full text-sm">
        <caption className="sr-only">{t("machines.runs.caption")}</caption>
        <thead className="text-left text-muted-foreground">
          <tr>
            <th scope="col" className="py-2 pr-3 font-medium">
              {t("machines.runs.column.part")}
            </th>
            <th scope="col" className="px-3 py-2 font-medium">
              {t("machines.runs.column.start")}
            </th>
            <th scope="col" className="px-3 py-2 font-medium">
              {t("machines.runs.column.end")}
            </th>
            <th scope="col" className="px-3 py-2 font-medium">
              {t("machines.runs.column.work_order")}
            </th>
            <td className="py-2 pl-3">
              <span className="sr-only">{t("machines.runs.column.actions")}</span>
            </td>
          </tr>
        </thead>
        <tbody>
          {runs.map((run) => (
            <tr
              key={run.run_id}
              id={`run-${run.run_id}`}
              {...(run.run_id === marked ? { "aria-current": "true" as const } : {})}
              className={cn("border-t", markedClass(run))}
            >
              <th scope="row" className="py-2 pr-3 text-left font-normal">
                <span className="flex flex-wrap items-center gap-2">
                  <PartNumber>{run.part_number}</PartNumber>
                  {tags(run)}
                </span>
              </th>
              <td className="px-3 py-2 whitespace-nowrap">{format.when(run.started_at, now)}</td>
              <td className="px-3 py-2 whitespace-nowrap">{end(run)}</td>
              <td className="px-3 py-2 font-mono">{run.work_order ?? "—"}</td>
              <td className="py-2 pl-3 text-right">{correct(run)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    );
  }
  return (
    <ul aria-label={t("machines.runs.caption")} className="grid divide-y">
      {runs.map((run) => (
        <li
          key={run.run_id}
          id={`run-${run.run_id}`}
          {...(run.run_id === marked ? { "aria-current": "true" as const } : {})}
          className={cn("grid gap-1.5 py-3 text-sm", markedClass(run))}
        >
          <span className="flex flex-wrap items-center gap-2">
            <PartNumber>{run.part_number}</PartNumber>
            {tags(run)}
          </span>
          <span className="text-muted-foreground">
            {t("machines.runs.range", { from: format.when(run.started_at, now), to: end(run) })}
          </span>
          {run.work_order ? (
            <span className="text-muted-foreground">{t("machines.card.work_order", { order: run.work_order })}</span>
          ) : null}
          <span>{correct(run)}</span>
        </li>
      ))}
    </ul>
  );
}

const FIELD_LABELS: Record<FieldChange["field"], Parameters<ReturnType<typeof useI18n>["t"]>[0]> = {
  part_number: "machines.field.part_number",
  started_at: "machines.field.started_at",
  ended_at: "machines.field.ended_at",
  work_order: "machines.field.work_order",
  target_quantity: "machines.field.target_quantity",
};

function ChangeLine({ entry, now, format }: { entry: ChangeEntry; now: number; format: MachineFormat }) {
  const { t } = format;
  let label: string;
  let detail: string;
  switch (entry.kind) {
    case "start":
      label = t("machines.changes.kind.start");
      detail = entry.part_number;
      break;
    case "changeover":
      label = t("machines.changes.kind.changeover");
      detail = `${entry.previous_part_number ?? "—"} → ${entry.part_number}`;
      break;
    case "end":
      label = t("machines.changes.kind.end");
      detail = entry.part_number;
      break;
    case "backfill":
      label = t("machines.changes.kind.backfill");
      detail = t("machines.changes.backfill_detail", {
        part: entry.part_number,
        range: t("machines.runs.range", {
          from: entry.started_at ? format.when(entry.started_at, now) : "—",
          to: entry.ended_at ? format.when(entry.ended_at, now) : "—",
        }),
      });
      break;
    case "correction":
      label = t("machines.changes.kind.correction");
      detail = t("machines.changes.correction_detail", {
        part: entry.part_number,
        changes: entry.changes
          .map((change) =>
            t("machines.changes.field_change", {
              field: t(FIELD_LABELS[change.field]),
              before: format.changeValue(change.field, change.before, now),
              after: format.changeValue(change.field, change.after, now),
            }),
          )
          .join(t("machines.list_separator")),
      });
      break;
  }
  const Icon =
    entry.kind === "correction"
      ? PencilIcon
      : entry.kind === "end"
        ? SquareIcon
        : entry.kind === "backfill"
          ? PlusIcon
          : RepeatIcon;
  return (
    <li className="grid grid-cols-[1rem_1fr] gap-x-3 text-sm">
      <Icon aria-hidden="true" className="mt-0.5 size-4 text-muted-foreground" />
      <div className="grid gap-0.5">
        <p>
          <b>{label}</b> <span className="wrap-anywhere">{detail}</span>
        </p>
        <p className="text-xs text-muted-foreground">
          {[
            entry.actor,
            format.when(entry.at, now),
            entry.reason ? t("machines.changes.reason", { reason: entry.reason }) : null,
          ]
            .filter((part) => part !== null)
            .join(" · ")}
        </p>
      </div>
    </li>
  );
}

/** The devices with a point of this machine, and which points (all, or a few). */
function AssignedPoints({
  machine,
  deviceList,
}: {
  machine: MachineDetail;
  deviceList: readonly { device_id: string; name: string; model_id: string }[] | undefined;
}) {
  const { t } = useI18n();
  const api = useDeviceApi();
  const tenantId = useSession().session?.activeTenant?.id ?? "";
  const byDevice = new Map<string, string[]>();
  for (const item of machine.assignments) {
    byDevice.set(item.device_id, [...(byDevice.get(item.device_id) ?? []), item.point_key]);
  }
  const rows = [...byDevice.entries()].map(([deviceId, keys]) => ({
    deviceId,
    keys,
    device: deviceList?.find((device) => device.device_id === deviceId),
  }));
  const models = useQueries({
    queries: rows.map((row) => ({
      queryKey: modelKey(tenantId, row.device?.model_id ?? ""),
      queryFn: ({ signal }: { signal: AbortSignal }) => api.getDeviceModel(row.device?.model_id ?? "", signal),
      enabled: row.device !== undefined,
    })),
  });
  if (rows.length === 0) return <p className="text-sm text-muted-foreground">{t("machines.points.none")}</p>;
  return (
    <ul className="grid gap-2 text-sm">
      {rows.map((row, index) => {
        const total = models[index]?.data?.points.length;
        const all = total !== undefined && row.keys.length === total;
        return (
          <li key={row.deviceId} className="grid gap-0.5">
            {row.device ? (
              <Link
                to={devicePath(row.deviceId)}
                className={cn(
                  "justify-self-start rounded-sm font-medium underline-offset-4 hover:underline",
                  focusRing,
                )}
              >
                {row.device.name}
              </Link>
            ) : (
              <span className="font-medium">{t("machines.points.unknown_device")}</span>
            )}
            <span className="text-muted-foreground">
              {all
                ? t("machines.points.all", { count: row.keys.length })
                : t("machines.points.some", { points: row.keys.join(t("machines.list_separator")) })}
            </span>
          </li>
        );
      })}
    </ul>
  );
}

const ROLE_KIND: Record<RelationRole, Parameters<ReturnType<typeof useI18n>["t"]>[0]> = {
  power_in: "machines.relations.kind_label.power",
  power_out: "machines.relations.kind_label.power",
  drive_in: "machines.relations.kind_label.drive",
  drive_out: "machines.relations.kind_label.drive",
  flow_up: "machines.relations.kind_label.flow_up",
  flow_down: "machines.relations.kind_label.flow_down",
};
const ROLE_PHRASE: Record<RelationRole, Parameters<ReturnType<typeof useI18n>["t"]>[0] | null> = {
  power_in: "machines.relations.phrase.power_in",
  power_out: "machines.relations.phrase.power_out",
  drive_in: "machines.relations.phrase.drive_in",
  drive_out: "machines.relations.phrase.drive_out",
  flow_up: null,
  flow_down: null,
};

function RelationList({
  machine,
  machines,
}: {
  machine: MachineDetail;
  machines: readonly { machine_id: string; name: string }[] | undefined;
}) {
  const { t } = useI18n();
  if (machine.relations.length === 0) {
    return (
      <div className="grid justify-items-start gap-1 text-sm text-muted-foreground">
        <FactoryIcon aria-hidden="true" className="size-4" />
        <p>{t("machines.relations.none")}</p>
      </div>
    );
  }
  const related = (relation: MachineRelation) =>
    relation.to_machine_id === machine.machine_id ? relation.from_machine_id : relation.to_machine_id;
  return (
    <ul className="grid gap-2 text-sm">
      {machine.relations.map((relation) => {
        const role = roleOf(relation, machine.machine_id);
        const otherId = related(relation);
        const name = machines?.find((item) => item.machine_id === otherId)?.name;
        const phrase = ROLE_PHRASE[role];
        return (
          <li key={relation.relation_id} className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <Tag>{t(ROLE_KIND[role])}</Tag>
            {name ? (
              <Link
                to={machinePath(otherId)}
                className={cn("rounded-sm font-medium underline-offset-4 hover:underline", focusRing)}
              >
                {name}
              </Link>
            ) : (
              <span>{t("machines.relations.unknown_machine")}</span>
            )}
            {phrase ? <span className="text-muted-foreground">{t(phrase)}</span> : null}
          </li>
        );
      })}
    </ul>
  );
}
