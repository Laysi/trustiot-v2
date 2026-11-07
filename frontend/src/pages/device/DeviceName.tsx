import { useQuery } from "@tanstack/react-query";
import type { ReactNode } from "react";

import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import { type ComponentExtras, ComponentExtrasProvider } from "../../dashboard/extensions.tsx";
import { useSession } from "../../session/SessionProvider.tsx";
import { deviceListKey } from "./queries.ts";

/** How long the device list is reused before a panel asks again (the same as the mention list's). */
const STALE_MS = 30_000;

/**
 * A device's name, or its ID while the tenant's device list is loading, failed, or does not list
 * the device. The list is the devices page's (same key, so one request and one cache for every
 * name in a panel); every role that can open a dashboard or a conversation may read it
 * (`GET /api/assets/v1/devices` asks for a tenant viewer, docs/architecture/permissions.md).
 */
export function DeviceName({ deviceId }: { deviceId: string }) {
  const api = useDeviceApi();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const devices = useQuery({
    queryKey: deviceListKey(tenant),
    queryFn: ({ signal }) => api.listDevices(signal),
    staleTime: STALE_MS,
  });
  return devices.data?.find((device) => device.device_id === deviceId)?.name ?? deviceId;
}

const EXTRAS: ComponentExtras = { deviceName: (deviceId) => <DeviceName deviceId={deviceId} /> };

/**
 * Lets the source panels below write a device by name (`deviceName` of `ComponentExtras`). The
 * list is read when a panel with a statistics or usage query shows a name, not when the page opens.
 */
export function DeviceNames({ children }: { children: ReactNode }) {
  return <ComponentExtrasProvider extras={EXTRAS}>{children}</ComponentExtrasProvider>;
}
