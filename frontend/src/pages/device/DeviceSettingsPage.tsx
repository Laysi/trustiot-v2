import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { TriangleAlertIcon } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router";

import { useAssetsApi } from "../../api/AssetsApiProvider.tsx";
import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import type { Device } from "../../api/devices.ts";
import { ApiError, classifyError } from "../../api/errors.ts";
import { DEVICES_PATH, devicePath } from "../../app/paths.ts";
import { flashState } from "../../components/FlashNotice.tsx";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import type { MessageId } from "../../i18n/messages.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { tenantName } from "../../session/tenant-name.ts";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { type Problem, problemsOf } from "../form-problems.ts";
import { StatusPage } from "../StatusPage.tsx";
import { WriteDenied } from "../WriteDenied.tsx";
import { DeleteDeviceDialog } from "./DeleteDeviceDialog.tsx";
import { DeviceForm } from "./DeviceForm.tsx";
import { canWriteAssets } from "./DeviceNewPage.tsx";
import {
  type ComparedSetting,
  changedSettings,
  DEVICE_CONFLICT_FIELDS,
  type DeviceFormValues,
  deviceValues,
  updateBody,
  validateDevice,
} from "./deviceForm.ts";
import { useDeviceFormat } from "./format.ts";
import { deviceKey, tenantDevicesKey } from "./queries.ts";
import { useFormChoices } from "./useFormChoices.ts";

const DEVICE_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** The settings form of one device (`/devices/:deviceId/settings`). */
export function DeviceSettingsPage() {
  const { deviceId = "" } = useParams();
  const { t } = useI18n();
  const tenant = useSession().session?.activeTenant;
  if (!canWriteAssets(tenant?.role)) {
    return (
      <WriteDenied
        title={t("device_form.settings_denied_title")}
        body={t("device_form.denied_body", { tenant: tenant ? tenantName(tenant, t) : "" })}
        backTo={DEVICE_ID.test(deviceId) ? devicePath(deviceId.toLowerCase()) : DEVICES_PATH}
        back={t("device_form.back_to_devices")}
      />
    );
  }
  if (!DEVICE_ID.test(deviceId)) return <SettingsNotFound />;
  return <SettingsView key={deviceId.toLowerCase()} deviceId={deviceId.toLowerCase()} />;
}

function SettingsNotFound() {
  const { t } = useI18n();
  const tenant = useSession().session?.activeTenant;
  return (
    <StatusPage
      title={t("device.not_found.title")}
      actions={
        <Button variant="outline" asChild>
          <Link to={DEVICES_PATH}>{t("device_form.back_to_devices")}</Link>
        </Button>
      }
    >
      <p>{t("device.not_found.body", { tenant: tenant ? tenantName(tenant, t) : "" })}</p>
    </StatusPage>
  );
}

/** What a 409 `revision_mismatch` leaves: the inputs the user had, and the revision the server is at. */
interface Conflict {
  mine: DeviceFormValues;
  current: number | undefined;
}

const SETTING_LABELS: Readonly<Record<ComparedSetting, MessageId>> = {
  transport: "device_form.transport",
  host: "device_form.host",
  port: "device_form.port",
  unitId: "device_form.unit_id",
  pollMs: "device_form.poll",
};

function SettingsView({ deviceId }: { deviceId: string }) {
  const { t, errorMessage } = useI18n();
  const devicesApi = useDeviceApi();
  const api = useAssetsApi();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const tenant = useSession().session?.activeTenant;
  const tenantId = tenant?.id ?? "";
  const choices = useFormChoices();

  const device = useQuery({
    queryKey: deviceKey(tenantId, deviceId),
    queryFn: ({ signal }) => devicesApi.getDevice(deviceId, signal),
  });

  const [values, setValues] = useState<DeviceFormValues | null>(null);
  // The revision the inputs were read at: what an update says it saw.
  const [base, setBase] = useState(0);
  const [problems, setProblems] = useState<Problem[]>([]);
  const [rest, setRest] = useState<unknown>(null);
  const [attempt, setAttempt] = useState(0);
  const [conflict, setConflict] = useState<Conflict | null>(null);
  const [ended, setEnded] = useState(false);
  const [saved, setSaved] = useState<Device | null>(null);
  const [deleting, setDeleting] = useState(false);
  const reloaded = useRef(false);

  // The form starts from the device as first read; a later refetch (focus, invalidation) never
  // replaces what the user is typing: only an explicit reload does.
  useEffect(() => {
    if (device.data && values === null) {
      setValues(deviceValues(device.data));
      setBase(device.data.revision);
    }
  }, [device.data, values]);

  const update = useMutation({
    mutationFn: (current: DeviceFormValues) => api.updateDevice(deviceId, updateBody(current, base)),
    onSuccess: async (updated) => {
      queryClient.setQueryData(deviceKey(tenantId, deviceId), updated);
      await queryClient.invalidateQueries({ queryKey: tenantDevicesKey(tenantId) });
      setBase(updated.revision);
      setValues(deviceValues(updated));
      setSaved(updated);
    },
    onError: (error, current) => {
      setSaved(null);
      if (error instanceof ApiError && error.body?.message_key === "device.binding_ended") {
        setEnded(true);
        return;
      }
      if (error instanceof ApiError && error.body?.message_key === "device.revision_mismatch") {
        const latest = error.body.details?.current;
        setConflict({ mine: current, current: typeof latest === "number" ? latest : undefined });
        return;
      }
      const refused = problemsOf(error, DEVICE_CONFLICT_FIELDS);
      setProblems(refused.problems);
      setRest(refused.rest);
      if (refused.problems.length > 0) setAttempt((count) => count + 1);
    },
  });

  const reload = useMutation({
    mutationFn: () => devicesApi.getDevice(deviceId),
    onSuccess: (latest) => {
      queryClient.setQueryData(deviceKey(tenantId, deviceId), latest);
      reloaded.current = true;
      setValues(deviceValues(latest));
      setBase(latest.revision);
      setProblems([]);
    },
  });

  if (device.isPending) {
    return <Spinner label={t("device.loading")} />;
  }
  if (device.isError) {
    if (classifyError(device.error) === "not_found") return <SettingsNotFound />;
    return (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("device.load_failed", { reason: errorMessage(device.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void device.refetch()} disabled={device.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  }
  if (values === null) return <Spinner label={t("device.loading")} />;

  const submit = () => {
    setRest(null);
    setSaved(null);
    const found = validateDevice(values, "edit");
    setProblems(found);
    if (found.length > 0) {
      setAttempt((count) => count + 1);
      return;
    }
    update.mutate(values);
  };

  const mineAfterReload =
    conflict !== null && reloaded.current && reload.isSuccess ? changedSettings(conflict.mine, values) : [];

  const notices = (
    <>
      {saved ? (
        <Alert role="status" className="grid-cols-[1rem_1fr]">
          <AlertDescription className="col-start-2">
            <p className="text-foreground">{t("device_form.saved", { revision: saved.revision })}</p>
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
      {ended ? (
        <Alert variant="destructive" className="grid-cols-[1rem_1fr]">
          <TriangleAlertIcon aria-hidden="true" />
          <AlertTitle>{t("device_form.ended_title")}</AlertTitle>
          <AlertDescription>
            <p>{t("device_form.ended_body")}</p>
          </AlertDescription>
        </Alert>
      ) : null}
      {conflict !== null && !ended ? (
        <Alert variant="destructive" className="grid-cols-[1rem_1fr]">
          <TriangleAlertIcon aria-hidden="true" />
          <AlertTitle>
            {conflict.current === undefined
              ? t("device_form.conflict_title_unknown")
              : t("device_form.conflict_title", { revision: conflict.current })}
          </AlertTitle>
          <AlertDescription>
            <p>{t("device_form.conflict_body")}</p>
            {reload.isError ? <p>{errorMessage(reload.error)}</p> : null}
            {mineAfterReload.length > 0 ? (
              <MineList
                mine={conflict.mine}
                changed={mineAfterReload}
                onApply={() => {
                  setValues(conflict.mine);
                  setConflict(null);
                  reload.reset();
                }}
              />
            ) : (
              <Button variant="outline" size="sm" onClick={() => reload.mutate()} disabled={reload.isPending}>
                {reload.isPending ? <Spinner /> : null}
                {t("device_form.reload")}
              </Button>
            )}
            {reload.isSuccess && mineAfterReload.length === 0 ? (
              <p className="text-foreground">{t("device_form.reloaded_same")}</p>
            ) : null}
          </AlertDescription>
        </Alert>
      ) : null}
    </>
  );

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader
        title={device.data.name}
        meta={t("device_form.settings_description")}
        crumbs={[
          { label: t("devices.title"), to: DEVICES_PATH },
          { label: device.data.name, to: devicePath(deviceId) },
        ]}
        tags={<span className="text-sm text-muted-foreground">{t("device_form.eyebrow")}</span>}
      />
      <DeviceForm
        mode="edit"
        values={values}
        onChange={(patch, pointer) => {
          setValues((current) => (current ? { ...current, ...patch } : current));
          setProblems((current) => current.filter((problem) => problem.pointer !== pointer));
        }}
        problems={problems}
        attempt={attempt}
        submitting={update.isPending}
        locked={ended}
        gateways={choices.gateways}
        models={choices.models}
        limits={choices.limits}
        onSubmit={submit}
        cancelTo={devicePath(deviceId)}
        submitLabel={t("device_form.save")}
        notices={notices}
        extraActions={
          <Button variant="outline" className="text-destructive" onClick={() => setDeleting(true)}>
            {t("device_form.delete")}
          </Button>
        }
      />
      <DeleteDeviceDialog
        device={deleting ? device.data : null}
        onClose={() => setDeleting(false)}
        onDeleted={(name) => {
          void navigate(DEVICES_PATH, { state: flashState("devices.flash.deleted", { name }) });
        }}
      />
    </div>
  );
}

/** After a reload: what the user had typed that differs from the latest, and how to take it back. */
function MineList(props: { mine: DeviceFormValues; changed: readonly ComparedSetting[]; onApply(): void }) {
  const { t } = useI18n();
  const format = useDeviceFormat();
  const show = (setting: ComparedSetting): string => {
    const value = props.mine[setting];
    if (setting === "pollMs") return format.duration(Number(value));
    if (setting === "transport") {
      return t(value === "modbus_tcp" ? "device_form.transport.tcp" : "device_form.transport.rtu");
    }
    return String(value);
  };
  return (
    <div className="grid gap-2">
      <p className="font-medium text-foreground">{t("device_form.mine_title")}</p>
      <ul className="grid gap-0.5">
        {props.changed.map((setting) => (
          <li key={setting}>
            {t("device_form.mine_line", { field: t(SETTING_LABELS[setting]), value: show(setting) })}
          </li>
        ))}
      </ul>
      <Button variant="outline" size="sm" onClick={props.onApply}>
        {t("device_form.mine_apply")}
      </Button>
    </div>
  );
}
