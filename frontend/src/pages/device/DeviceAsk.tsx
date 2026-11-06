import { type ReactNode, useMemo } from "react";

import type { Device } from "../../api/devices.ts";
import { AskAiButton } from "../../components/ask/AskAiButton.tsx";
import { type AskQuestion, compareLastWeek, whyNoData, whyOffline } from "../../components/ask/questions.ts";
import { StretchAsk } from "../../components/ask/StretchAsk.tsx";
import { ComponentExtrasProvider } from "../../dashboard/extensions.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { usePreferences } from "../../preferences/PreferencesProvider.tsx";
import type { DataHealth } from "./health.ts";

/**
 * The device page's 「問 AI」 buttons in the head (docs/design/home-ai/09-device-page-context.html):
 * 「為什麼離線？」 while the data is stalled (or 「為什麼沒有資料？」 when nothing was ever stored)
 * and 「跟上週比」 whenever the device has stored something. Each opens a new conversation with the
 * device and the time range written into the question, and leads back to this page
 * (docs/design/home-ai/design.md, decision 5). Nothing until the page knows whether data comes in.
 */
export function DeviceAsk({ device, health, now }: { device: Pick<Device, "name">; health: DataHealth; now: number }) {
  const { t } = useI18n();
  const { timeZone } = usePreferences();
  const questions: AskQuestion[] = [];
  if (health.kind === "stale") questions.push(whyOffline(t, { name: device.name, lastSeen: health.newest, timeZone }));
  if (health.kind === "none") questions.push(whyNoData(t, { name: device.name }));
  if (health.kind === "ok" || health.kind === "stale") {
    questions.push(compareLastWeek(t, { name: device.name, now, timeZone }));
  }
  if (questions.length === 0) return null;
  return (
    <fieldset className="m-0 flex min-w-0 flex-wrap gap-2 border-0 p-0">
      <legend className="sr-only">{t("ask.device.group", { device: device.name })}</legend>
      {questions.map((question) => (
        <AskAiButton
          key={question.label}
          label={question.label}
          question={question.text}
          back={{ label: device.name }}
        />
      ))}
    </fieldset>
  );
}

/**
 * Gives the history chart below 「分析這段」 for the data point it draws: the stretch the zoom
 * shows is asked about by device, data point and time (design 09). `point` is what the chart
 * calls the data point.
 */
export function DeviceStretchAsk({
  deviceName,
  point,
  children,
}: {
  deviceName: string;
  point: string;
  children: ReactNode;
}) {
  const extras = useMemo(
    () => ({
      stretch: (stretch: { from: number; to: number; dateOnly: boolean }) => (
        <StretchAsk
          subject={{ kind: "point", device: deviceName, point }}
          stretch={stretch}
          back={{ label: deviceName }}
        />
      ),
    }),
    [deviceName, point],
  );
  return <ComponentExtrasProvider extras={extras}>{children}</ComponentExtrasProvider>;
}
