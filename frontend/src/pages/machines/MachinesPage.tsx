import { useQuery } from "@tanstack/react-query";
import { FactoryIcon, PlayIcon, PlusIcon, RepeatIcon, SearchIcon, SearchXIcon, SquareIcon } from "lucide-react";
import { type ReactNode, useCallback, useId, useMemo, useState } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";

import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import { useMachineApi } from "../../api/pending/MachineApiProvider.tsx";
import { isNotInPlan, type MachineSummary } from "../../api/pending/machines.ts";
import { machinePath } from "../../app/paths.ts";
import { EmptyState } from "../../components/EmptyState.tsx";
import { FlashNotice, flashState } from "../../components/FlashNotice.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { controlClasses } from "../../components/ui/input.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { Tag } from "../dashboards/Tag.tsx";
import { deviceListKey } from "../device/queries.ts";
import { CLOCK_TICK_MS, useNow } from "../device/useNow.ts";
import { canEditMachines } from "./access.ts";
import { ChangeoverDialog } from "./ChangeoverDialog.tsx";
import { ChangeToast, type ToastRecord } from "./ChangeToast.tsx";
import { EndRunAction } from "./EndRunAction.tsx";
import { useMachineFormat } from "./format.ts";
import { MachineFormDialog } from "./MachineFormDialog.tsx";
import { PlanWall } from "./PlanWall.tsx";
import { PartNumber, RequestError, Toast } from "./parts.tsx";
import { machineListKey } from "./queries.ts";

const SEARCH_PARAM = "q";
const SHOW_PARAM = "show";
type Show = "all" | "running" | "idle";

function parseShow(value: string | null): Show {
  return value === "running" || value === "idle" ? value : "all";
}

/**
 * The tenant's machines (`/machines`, checklist C4 and C5) as the cards of
 * docs/design/ms4-ms5/machines.html: every machine with what it is making now and, for editors,
 * the button that changes it, so a changeover on the shop floor does not need the detail page.
 * A utility (an air compressor) shows no production buttons. Search and the "in production" filter
 * live in the address bar (`?q=`, `?show=running|idle`).
 */
export function MachinesPage() {
  const { t } = useI18n();
  const api = useMachineApi();
  const devicesApi = useDeviceApi();
  const tenant = useSession().session?.activeTenant;
  const tenantId = tenant?.id ?? "";
  const canEdit = canEditMachines(tenant?.role);
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const searchId = useId();
  const now = useNow(CLOCK_TICK_MS);
  const format = useMachineFormat();
  const [changeover, setChangeover] = useState<MachineSummary | null>(null);
  const [creating, setCreating] = useState(false);
  const [done, setDone] = useState<ToastRecord | null>(null);
  const closeToast = useCallback(() => setDone(null), []);

  const machines = useQuery({
    queryKey: machineListKey(tenantId),
    queryFn: ({ signal }) => api.listMachines(signal),
  });
  // Only to name the devices on a card: the page works without them.
  const devices = useQuery({
    queryKey: deviceListKey(tenantId),
    queryFn: ({ signal }) => devicesApi.listDevices(signal),
  });
  const deviceNames = useMemo(
    () => new Map((devices.data ?? []).map((device) => [device.device_id, device.name])),
    [devices.data],
  );

  const show = parseShow(params.get(SHOW_PARAM));
  const query = params.get(SEARCH_PARAM) ?? "";
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

  const all = machines.data ?? [];
  const running = all.filter((machine) => machine.current_run !== null);
  const needle = query.trim().toLowerCase();
  const shown = all.filter((machine) => {
    if (show === "running" && machine.current_run === null) return false;
    if (show === "idle" && (machine.current_run !== null || machine.utility)) return false;
    if (needle === "") return true;
    const run = machine.current_run ?? machine.last_run;
    return machine.name.toLowerCase().includes(needle) || (run?.part_number.toLowerCase().includes(needle) ?? false);
  });

  let body: ReactNode;
  if (machines.isPending) {
    body = <CardsSkeleton label={t("machines.loading")} />;
  } else if (machines.isError) {
    body = isNotInPlan(machines.error) ? (
      <PlanWall />
    ) : (
      <RequestError error={machines.error} onRetry={() => void machines.refetch()} retrying={machines.isFetching} />
    );
  } else if (all.length === 0) {
    body = (
      <EmptyState
        icon={<FactoryIcon aria-hidden="true" />}
        title={t("machines.empty.title")}
        action={
          canEdit ? (
            <Button onClick={() => setCreating(true)}>
              <PlusIcon aria-hidden="true" />
              {t("machines.add")}
            </Button>
          ) : undefined
        }
      >
        <p>{t("machines.empty.body")}</p>
        {canEdit ? null : <p>{t("machines.empty.viewer")}</p>}
      </EmptyState>
    );
  } else if (shown.length === 0) {
    body = (
      <div role="status" className="grid justify-items-center gap-2 rounded-lg border px-4 py-12 text-center">
        <SearchXIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <p className="font-semibold">{t("machines.no_match.title")}</p>
        <p className="text-sm text-muted-foreground">{t("machines.no_match.body")}</p>
      </div>
    );
  } else {
    body = (
      <ul aria-label={t("machines.list_label")} className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
        {shown.map((machine) => (
          <li key={machine.machine_id} className="grid">
            <MachineCard
              machine={machine}
              now={now}
              deviceNames={machine.device_ids.flatMap((id) => deviceNames.get(id) ?? [])}
              canEdit={canEdit}
              onChangeover={() => setChangeover(machine)}
            />
          </li>
        ))}
      </ul>
    );
  }

  const showToolbar = machines.isSuccess && all.length > 0;
  const segments: { id: Show; label: string }[] = [
    { id: "all", label: t("machines.filter.all", { count: all.length }) },
    { id: "running", label: t("machines.filter.running", { count: running.length }) },
    { id: "idle", label: t("machines.filter.idle") },
  ];
  const changedMachine = done ? all.find((machine) => machine.machine_id === done.machineId) : undefined;

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader
        title={t("machines.title")}
        meta={t("machines.description")}
        actions={
          canEdit && machines.isSuccess && all.length > 0 ? (
            <Button variant="outline" onClick={() => setCreating(true)}>
              <PlusIcon aria-hidden="true" />
              {t("machines.add")}
            </Button>
          ) : undefined
        }
      />
      <FlashNotice />
      {showToolbar ? (
        <search className="flex flex-wrap items-center gap-3">
          <div className="relative w-full sm:w-80">
            <label htmlFor={searchId} className="sr-only">
              {t("machines.search.label")}
            </label>
            <SearchIcon
              aria-hidden="true"
              className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground"
            />
            <input
              id={searchId}
              type="search"
              value={query}
              placeholder={t("machines.search.placeholder")}
              className={cn(controlClasses, "pl-9")}
              onChange={(event) => setParam(SEARCH_PARAM, event.target.value)}
            />
          </div>
          <fieldset className="flex flex-wrap gap-1">
            <legend className="sr-only">{t("machines.filter.label")}</legend>
            {segments.map((segment) => (
              <Button
                key={segment.id}
                variant={show === segment.id ? "secondary" : "outline"}
                size="sm"
                aria-pressed={show === segment.id}
                onClick={() => setParam(SHOW_PARAM, segment.id === "all" ? "" : segment.id)}
              >
                {segment.label}
              </Button>
            ))}
          </fieldset>
        </search>
      ) : null}
      {body}
      {creating ? (
        <MachineFormDialog
          onClose={() => setCreating(false)}
          onDone={(saved) => {
            setCreating(false);
            void navigate(machinePath(saved.machine_id), {
              state: flashState("machines.flash.created", { name: saved.name }),
            });
          }}
        />
      ) : null}
      {changeover ? (
        <ChangeoverDialog
          machine={changeover}
          current={changeover.current_run}
          onClose={() => setChangeover(null)}
          onDone={(result) => {
            setDone({ machineId: changeover.machine_id, result });
            setChangeover(null);
          }}
        />
      ) : null}
      {done ? (
        <Toast onClose={closeToast}>
          <ChangeToast done={done} name={changedMachine?.name ?? ""} format={format} />
        </Toast>
      ) : null}
    </div>
  );
}

function MachineCard({
  machine,
  now,
  deviceNames,
  canEdit,
  onChangeover,
}: {
  machine: MachineSummary;
  now: number;
  deviceNames: readonly string[];
  canEdit: boolean;
  onChangeover(): void;
}) {
  const format = useMachineFormat();
  const { t } = format;
  const run = machine.current_run;
  const last = machine.last_run;
  const headingId = useId();
  return (
    <article
      aria-labelledby={headingId}
      className="grid content-start gap-3 rounded-lg border bg-card px-4 py-4 text-card-foreground"
    >
      <div className="flex items-start justify-between gap-2">
        <h2 id={headingId} className="min-w-0 font-semibold wrap-anywhere">
          <Link
            to={machinePath(machine.machine_id)}
            className={cn("rounded-sm underline-offset-4 hover:underline", focusRing)}
          >
            {machine.name}
          </Link>
        </h2>
        {machine.utility ? (
          <Tag>{t("machines.status.utility")}</Tag>
        ) : run ? (
          <Tag icon={<PlayIcon aria-hidden="true" />}>{t("machines.status.running")}</Tag>
        ) : (
          <Tag icon={<SquareIcon aria-hidden="true" />}>{t("machines.status.idle")}</Tag>
        )}
      </div>
      {run ? (
        <div className="grid gap-0.5 text-sm">
          <span className="text-muted-foreground">{t("machines.card.current_part")}</span>
          <PartNumber className="text-xl">{run.part_number}</PartNumber>
          <span className="text-muted-foreground">
            {t("machines.card.run_facts", {
              time: format.when(run.started_at, now),
              elapsed: format.elapsed(now - Date.parse(run.started_at)),
            })}
            {" · "}
            {run.work_order
              ? t("machines.card.work_order", { order: run.work_order })
              : t("machines.card.no_work_order")}
          </span>
        </div>
      ) : machine.utility ? (
        <p className="text-sm text-muted-foreground">{machine.description ?? t("machines.card.utility_hint")}</p>
      ) : last ? (
        <div className="grid gap-0.5 text-sm">
          <span className="text-muted-foreground">{t("machines.card.last_part")}</span>
          <PartNumber className="text-xl">{last.part_number}</PartNumber>
          <span className="text-muted-foreground">
            {last.ended_at ? t("machines.card.ended", { time: format.when(last.ended_at, now) }) : null}
          </span>
        </div>
      ) : (
        <p className="text-sm text-muted-foreground">{t("machines.card.no_runs")}</p>
      )}
      {deviceNames.length > 0 ? (
        <p className="text-xs text-muted-foreground wrap-anywhere">{deviceNames.join(t("machines.list_separator"))}</p>
      ) : null}
      {canEdit && !machine.utility ? (
        <div className="flex flex-wrap gap-2">
          <Button
            onClick={onChangeover}
            aria-label={t(run ? "machines.changeover.button_for" : "machines.start.button_for", {
              machine: machine.name,
            })}
          >
            {run ? <RepeatIcon aria-hidden="true" /> : <PlayIcon aria-hidden="true" />}
            {run ? t("machines.changeover.button") : t("machines.start.button")}
          </Button>
          {run ? <EndRunAction machine={machine} run={run} /> : null}
        </div>
      ) : null}
    </article>
  );
}

/** Cards that stand in for the list while it loads; announced once. */
function CardsSkeleton({ label }: { label: string }) {
  return (
    <section aria-busy="true" className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
      <p role="status" className="sr-only">
        {label}
      </p>
      {[0, 1, 2].map((card) => (
        <div key={card} aria-hidden="true" className="grid gap-3 rounded-lg border bg-card px-4 py-4">
          <span className="h-3 w-2/5 animate-pulse rounded bg-muted" />
          <span className="h-16 animate-pulse rounded bg-muted" />
          <span className="h-3 w-[30%] animate-pulse rounded bg-muted" />
        </div>
      ))}
    </section>
  );
}
