import { useI18n } from "../../i18n/useI18n.ts";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { GatewayPanel } from "./gateways/GatewayPanel.tsx";

/**
 * The health overview and the device pool (`/platform/gateways`, docs/design/platform-admin/
 * gateways.html): every gateway registered at the factory, the waiting ones in the pool and the
 * bound ones with their tenant and health.
 */
export function GatewaysPage() {
  const { t } = useI18n();
  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader title={t("platform.gateways.title")} meta={t("platform.gateways.description")} />
      <GatewayPanel />
    </div>
  );
}
