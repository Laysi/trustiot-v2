import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { useNavigate, useSearchParams } from "react-router";

import { useAssetsApi } from "../../api/AssetsApiProvider.tsx";
import { DEVICES_PATH } from "../../app/paths.ts";
import { flashState } from "../../components/FlashNotice.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { tenantName } from "../../session/tenant-name.ts";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { type Problem, problemsOf } from "../form-problems.ts";
import { WriteDenied } from "../WriteDenied.tsx";
import { DeviceForm } from "./DeviceForm.tsx";
import { createBody, DEVICE_CONFLICT_FIELDS, newDeviceValues, validateDevice } from "./deviceForm.ts";
import { tenantDevicesKey } from "./queries.ts";
import { useFormChoices } from "./useFormChoices.ts";

/** The roles that may write devices and models (design decision 2): editors and administrators. */
export function canWriteAssets(role: string | undefined): boolean {
  return role === "editor" || role === "admin";
}

/**
 * Adds a device (`/devices/new`, `?gateway=<hardware ID>` chooses the gateway beforehand). A
 * viewer is told why they cannot (`devices--device-new-denied`).
 */
export function DeviceNewPage() {
  const { t } = useI18n();
  const tenant = useSession().session?.activeTenant;
  if (!canWriteAssets(tenant?.role)) {
    return (
      <WriteDenied
        title={t("device_form.denied_title")}
        body={t("device_form.denied_body", { tenant: tenant ? tenantName(tenant, t) : "" })}
        backTo={DEVICES_PATH}
        back={t("device_form.back_to_devices")}
      />
    );
  }
  return <DeviceNewForm />;
}

function DeviceNewForm() {
  const { t, errorMessage } = useI18n();
  const api = useAssetsApi();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const choices = useFormChoices();
  const [values, setValues] = useState(() => newDeviceValues({ gateway: params.get("gateway") ?? "" }));
  const [problems, setProblems] = useState<Problem[]>([]);
  const [rest, setRest] = useState<unknown>(null);
  const [attempt, setAttempt] = useState(0);

  // The only bound gateway is the one to choose; the polling default follows what the plan allows.
  const gatewayIds = choices.gateways.items.map((gateway) => gateway.hardware_id);
  const onlyGateway = gatewayIds.length === 1 ? (gatewayIds[0] ?? "") : "";
  const minPollMs = choices.limits?.minPollIntervalMs;
  useEffect(() => {
    setValues((current) => {
      const defaults = newDeviceValues(minPollMs === undefined ? {} : { minPollMs });
      return {
        ...current,
        hardwareId: current.hardwareId === "" ? onlyGateway : current.hardwareId,
        pollMs: Number(current.pollMs) < (minPollMs ?? 0) ? defaults.pollMs : current.pollMs,
      };
    });
  }, [onlyGateway, minPollMs]);

  const create = useMutation({
    mutationFn: () => api.createDevice(createBody(values)),
    onSuccess: async (device) => {
      await queryClient.invalidateQueries({ queryKey: tenantDevicesKey(tenant) });
      void navigate(DEVICES_PATH, { state: flashState("devices.flash.created", { name: device.name }) });
    },
    onError: (error) => {
      const refused = problemsOf(error, DEVICE_CONFLICT_FIELDS);
      setProblems(refused.problems);
      setRest(refused.rest);
      if (refused.problems.length > 0) setAttempt((count) => count + 1);
    },
  });

  const submit = () => {
    setRest(null);
    const found = validateDevice(values, "create");
    setProblems(found);
    if (found.length > 0) {
      setAttempt((count) => count + 1);
      return;
    }
    create.mutate();
  };

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader
        title={t("device_form.new_title")}
        meta={t("device_form.new_description")}
        crumbs={[{ label: t("devices.title"), to: DEVICES_PATH }]}
      />
      <DeviceForm
        mode="create"
        values={values}
        onChange={(patch, pointer) => {
          setValues((current) => ({ ...current, ...patch }));
          setProblems((current) => current.filter((problem) => problem.pointer !== pointer));
        }}
        problems={problems}
        attempt={attempt}
        submitting={create.isPending}
        gateways={choices.gateways}
        models={choices.models}
        limits={choices.limits}
        onSubmit={submit}
        cancelTo={DEVICES_PATH}
        submitLabel={t("device_form.create")}
        notices={
          rest !== null ? (
            <Alert variant="destructive">
              <AlertDescription>
                <p>{errorMessage(rest)}</p>
              </AlertDescription>
            </Alert>
          ) : null
        }
      />
    </div>
  );
}
