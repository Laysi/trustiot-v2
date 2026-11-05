import { useQuery } from "@tanstack/react-query";

import { useAssetsApi } from "../../api/AssetsApiProvider.tsx";
import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import { useSession } from "../../session/SessionProvider.tsx";
import type { Choices } from "./DeviceForm.tsx";
import { gatewayChoicesKey, modelListKey, planLimitsKey } from "./queries.ts";

/**
 * What the device form chooses from: the tenant's bound gateways, its models, and the shortest
 * polling interval its plan allows. A failing plan read only loses the disabled options (the
 * server still refuses an interval the plan does not allow), so it is not an error of the form.
 */
export function useFormChoices() {
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const assets = useAssetsApi();
  const devices = useDeviceApi();
  const gateways = useQuery({
    queryKey: gatewayChoicesKey(tenant),
    queryFn: ({ signal }) => assets.listGateways(signal),
  });
  const models = useQuery({
    queryKey: modelListKey(tenant),
    queryFn: ({ signal }) => devices.listDeviceModels(signal),
  });
  const limits = useQuery({
    queryKey: planLimitsKey(tenant),
    queryFn: ({ signal }) => assets.getPlanLimits(signal),
  });
  const gatewayChoices: Choices<NonNullable<typeof gateways.data>[number]> = {
    status: gateways.status,
    items: gateways.data ?? [],
    retry: () => void gateways.refetch(),
  };
  const modelChoices: Choices<NonNullable<typeof models.data>[number]> = {
    status: models.status,
    items: models.data ?? [],
    retry: () => void models.refetch(),
  };
  return { gateways: gatewayChoices, models: modelChoices, limits: limits.data };
}
