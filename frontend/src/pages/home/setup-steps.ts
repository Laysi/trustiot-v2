import type { SetupStepId } from "../../api/pending/setup.ts";
import { DEVICES_PATH } from "../../app/paths.ts";
import type { MessageId } from "../../i18n/messages.ts";

/**
 * Where each step's main button leads (docs/design/tenant-admin/design.md, 「深連結」): the
 * gateway page with its bind dialog open, the form for a new device, the device list, the member
 * page with the invitation dialog open. The first use is chosen on the card, so it leads nowhere.
 * A button is drawn only when the router has the page (`useRouteExists`).
 */
export const STEP_TARGETS: Readonly<Record<SetupStepId, string | null>> = {
  bind_gateway: "/gateways?bind",
  add_device: "/devices/new",
  first_data: DEVICES_PATH,
  first_use: null,
  invite_members: "/members?invite",
};

/** The text each step has: its name, and what its main button does. */
export const STEP_TEXT: Readonly<Record<SetupStepId, { title: MessageId; action: MessageId | null }>> = {
  bind_gateway: { title: "setup.step.bind_gateway.title", action: "setup.step.bind_gateway.action" },
  add_device: { title: "setup.step.add_device.title", action: "setup.step.add_device.action" },
  first_data: { title: "setup.step.first_data.title", action: "setup.step.first_data.action" },
  first_use: { title: "setup.step.first_use.title", action: null },
  invite_members: { title: "setup.step.invite_members.title", action: "setup.step.invite_members.action" },
};
