import { type ReactNode, useMemo } from "react";

import { type ComponentExtras, ComponentExtrasProvider } from "../../dashboard/extensions.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { DeviceNames } from "../../pages/device/DeviceName.tsx";
import { AskAiButton } from "./AskAiButton.tsx";
import { explainSource } from "./questions.ts";

/**
 * Gives the source panels of the dashboard page 「請 AI 解釋」 (docs/design/chat-dashboards/
 * design.md, 「資料來源面板」): a new conversation that asks where the dataset, computation or
 * constant the panel shows comes from, with the dashboard's name in the question, and a way back
 * to this dashboard. Only for the page of a dashboard: a conversation's panels are inside one
 * already and have their own 「問 AI」 row (design.md, 「和設計稿不同的實作」, Laysi/trustiot-v2#1175).
 * Also lets the panels write a device by name.
 */
export function DashboardSourceAsk({ dashboard, children }: { dashboard: string; children: ReactNode }) {
  const { t } = useI18n();
  const extras = useMemo<ComponentExtras>(
    () => ({
      explainSource: (source) => {
        const question = explainSource(t, { dashboard, title: source.title });
        return <AskAiButton size="sm" label={question.label} question={question.text} back={{ label: dashboard }} />;
      },
    }),
    [t, dashboard],
  );
  return (
    <DeviceNames>
      <ComponentExtrasProvider extras={extras}>{children}</ComponentExtrasProvider>
    </DeviceNames>
  );
}
