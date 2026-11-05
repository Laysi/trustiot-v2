import type { MessageId } from "../../../i18n/messages.ts";

const SERVICES: Readonly<Record<string, MessageId>> = {
  assistant: "platform_ai.service.assistant",
  alerts: "platform_ai.service.alerts",
  knowledge: "platform_ai.service.knowledge",
};

/** The message for a service's name, or `null` for a service this build has no name for (its ID is shown). */
export function serviceLabelId(service: string): MessageId | null {
  return SERVICES[service] ?? null;
}
