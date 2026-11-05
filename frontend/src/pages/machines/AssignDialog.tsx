import { useMutation, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { SearchIcon } from "lucide-react";
import { type ReactNode, useEffect, useId, useMemo, useRef, useState } from "react";

import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import { useMachineApi } from "../../api/pending/MachineApiProvider.tsx";
import { isServiceUnavailable, type Machine, type PointAssignment } from "../../api/pending/machines.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { controlClasses } from "../../components/ui/input.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { deviceListKey, modelKey } from "../device/queries.ts";
import { FormDialog } from "./FormDialog.tsx";
import { UnavailableNotice } from "./parts.tsx";
import { assignmentsKey, machineListKey, machinesKey } from "./queries.ts";

const pointId = (deviceId: string, pointKey: string) => `${deviceId}\u0000${pointKey}`;

/** A checkbox that can show "some of the points": the property is not an HTML attribute. */
function Check({
  checked,
  mixed = false,
  disabled = false,
  label,
  onChange,
}: {
  checked: boolean;
  mixed?: boolean;
  disabled?: boolean;
  label: string;
  onChange(checked: boolean): void;
}) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (ref.current) ref.current.indeterminate = mixed;
  }, [mixed]);
  return (
    <input
      ref={ref}
      type="checkbox"
      aria-label={label}
      checked={checked}
      disabled={disabled}
      className="size-4 shrink-0 accent-primary"
      onChange={(event) => onChange(event.target.checked)}
    />
  );
}

/**
 * Which data points belong to the machine (docs/design/ms4-ms5/machines.html, state 6): the
 * tenant's devices with their points as checkboxes, a device's points may be shared out between
 * machines, and a point of another machine is shown with that machine's name and cannot be taken
 * here. The tree is nested lists of checkboxes (the design draws `role="tree"`; native controls
 * give the keyboard and screen reader behaviour for free).
 */
export function AssignDialog({ machine, onClose, onDone }: { machine: Machine; onClose(): void; onDone(): void }) {
  const { t, errorMessage } = useI18n();
  const api = useMachineApi();
  const devicesApi = useDeviceApi();
  const queryClient = useQueryClient();
  const tenantId = useSession().session?.activeTenant?.id ?? "";
  const searchId = useId();

  const devices = useQuery({
    queryKey: deviceListKey(tenantId),
    queryFn: ({ signal }) => devicesApi.listDevices(signal),
  });
  const assignments = useQuery({
    queryKey: assignmentsKey(tenantId),
    queryFn: ({ signal }) => api.listAssignments(signal),
    // The answer is what the dialog starts from: never a cached older one.
    gcTime: 0,
    staleTime: 0,
  });
  const machines = useQuery({
    queryKey: machineListKey(tenantId),
    queryFn: ({ signal }) => api.listMachines(signal),
  });
  const modelIds = [...new Set((devices.data ?? []).map((device) => device.model_id))];
  const models = useQueries({
    queries: modelIds.map((modelId) => ({
      queryKey: modelKey(tenantId, modelId),
      queryFn: ({ signal }: { signal: AbortSignal }) => devicesApi.getDeviceModel(modelId, signal),
    })),
  });

  const [selected, setSelected] = useState<ReadonlySet<string> | null>(null);
  const [search, setSearch] = useState("");

  const ready = devices.isSuccess && assignments.isSuccess && models.every((model) => model.isSuccess);
  const owners = useMemo(() => {
    const byPoint = new Map<string, PointAssignment>();
    for (const item of assignments.data ?? []) byPoint.set(pointId(item.device_id, item.point_key), item);
    return byPoint;
  }, [assignments.data]);
  // The points of this machine at the moment the lists are in.
  useEffect(() => {
    if (ready && selected === null) {
      setSelected(
        new Set(
          (assignments.data ?? []).filter((item) => item.machine_id === machine.machine_id).map((item) => pointId(item.device_id, item.point_key)),
        ),
      );
    }
  }, [ready, selected, assignments.data, machine.machine_id]);

  const machineName = (machineId: string) =>
    (machines.data ?? []).find((item) => item.machine_id === machineId)?.name ?? t("machines.assign.other_machine");
  const modelOf = new Map(models.flatMap((model) => (model.data ? [[model.data.model_id, model.data] as const] : [])));

  const mutation = useMutation({
    mutationFn: () =>
      api.setAssignments(
        machine.machine_id,
        [...(selected ?? [])].map((id) => {
          const [device_id = "", point_key = ""] = id.split("\u0000");
          return { device_id, point_key };
        }),
      ),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: machinesKey(tenantId) });
      onDone();
    },
  });

  const toggle = (ids: readonly string[], on: boolean) =>
    setSelected((current) => {
      const next = new Set(current ?? []);
      for (const id of ids) {
        if (on) next.add(id);
        else next.delete(id);
      }
      return next;
    });

  const needle = search.trim().toLowerCase();
  const rows = (devices.data ?? []).map((device) => {
    const points = (modelOf.get(device.model_id)?.points ?? []).map((point) => {
      const id = pointId(device.device_id, point.key);
      const owner = owners.get(id);
      return {
        key: point.key,
        id,
        otherMachine: owner && owner.machine_id !== machine.machine_id ? owner.machine_id : null,
      };
    });
    const matches =
      needle === "" ||
      device.name.toLowerCase().includes(needle) ||
      points.some((point) => point.key.toLowerCase().includes(needle));
    return { device, points, matches };
  });
  const shownRows = rows.filter((row) => row.matches);

  let body: ReactNode;
  const failure = [devices, assignments, machines, ...models].find((query) => query.isError);
  if (failure?.isError) {
    body = isServiceUnavailable(failure.error) ? (
      <UnavailableNotice />
    ) : (
      <Alert variant="destructive">
        <AlertDescription>
          <p>{errorMessage(failure.error)}</p>
        </AlertDescription>
      </Alert>
    );
  } else if (!ready || selected === null) {
    body = <Spinner label={t("machines.assign.loading")} />;
  } else if (rows.length === 0) {
    body = <p className="text-sm text-muted-foreground">{t("machines.assign.no_devices")}</p>;
  } else if (shownRows.length === 0) {
    body = <p role="status" className="text-sm text-muted-foreground">{t("machines.assign.no_match")}</p>;
  } else {
    body = (
      <ul aria-label={t("machines.assign.tree")} className="grid max-h-80 gap-2 overflow-y-auto">
        {shownRows.map(({ device, points }) => {
          const own = points.filter((point) => point.otherMachine === null);
          const chosen = own.filter((point) => selected.has(point.id)).length;
          return (
            <li key={device.device_id} className="grid gap-1.5 rounded-md border px-3 py-2">
              <label className="flex items-center gap-2 text-sm font-medium">
                <Check
                  label={device.name}
                  checked={own.length > 0 && chosen === own.length}
                  mixed={chosen > 0 && chosen < own.length}
                  disabled={own.length === 0}
                  onChange={(on) =>
                    toggle(
                      own.map((point) => point.id),
                      on,
                    )
                  }
                />
                <span className="min-w-0 wrap-anywhere">{device.name}</span>
                <span className="text-xs font-normal text-muted-foreground">
                  {t("machines.assign.count", { chosen, total: own.length })}
                </span>
              </label>
              {points.length === 0 ? (
                <p className="pl-6 text-xs text-muted-foreground">{t("machines.assign.no_points")}</p>
              ) : (
                <ul className="grid gap-1 pl-6">
                  {points.map((point) => (
                    <li key={point.id}>
                      <label
                        className={cn("flex flex-wrap items-center gap-2 text-sm", point.otherMachine && "text-muted-foreground")}
                      >
                        <Check
                          label={`${device.name} · ${point.key}`}
                          checked={selected.has(point.id)}
                          disabled={point.otherMachine !== null}
                          onChange={(on) => toggle([point.id], on)}
                        />
                        <span className="wrap-anywhere">{point.key}</span>
                        {point.otherMachine ? (
                          <span className="text-xs">
                            {t("machines.assign.assigned_to", { machine: machineName(point.otherMachine) })}
                          </span>
                        ) : null}
                      </label>
                    </li>
                  ))}
                </ul>
              )}
            </li>
          );
        })}
      </ul>
    );
  }

  return (
    <FormDialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      title={t("machines.assign.title", { machine: machine.name })}
      description={<p>{t("machines.assign.description")}</p>}
      wide
      pending={mutation.isPending}
      submitLabel={t("machines.save")}
      pendingLabel={t("machines.saving")}
      onSubmit={() => {
        if (selected !== null) mutation.mutate();
      }}
    >
      {mutation.isError ? (
        isServiceUnavailable(mutation.error) ? (
          <UnavailableNotice />
        ) : (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(mutation.error)}</p>
            </AlertDescription>
          </Alert>
        )
      ) : null}
      <div className="relative">
        <label htmlFor={searchId} className="sr-only">
          {t("machines.assign.search")}
        </label>
        <SearchIcon
          aria-hidden="true"
          className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground"
        />
        <input
          id={searchId}
          type="search"
          value={search}
          placeholder={t("machines.assign.search")}
          className={cn(controlClasses, "pl-9")}
          onChange={(event) => setSearch(event.target.value)}
        />
      </div>
      {body}
    </FormDialog>
  );
}
