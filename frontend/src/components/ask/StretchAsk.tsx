import { useDashboardEnv } from "../../dashboard/context.tsx";
import type { ChartStretch } from "../../dashboard/extensions.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { AskAiButton } from "./AskAiButton.tsx";
import { analyzeStretch, type StretchSubject, wallText } from "./questions.ts";

/**
 * 「分析這段」: the button beside a zoomed chart's reset button, asking about the stretch the zoom
 * shows (design 08 and 09; the zoom is the chart's selection: drag, wheel or pinch). The chart
 * says where the stretch is in its own x unit, and the question names it in the user's time zone,
 * the one the chart draws in. Must sit inside the chart, where the dashboard's environment is.
 */
export function StretchAsk({
  subject,
  stretch,
  back,
}: {
  subject: StretchSubject;
  stretch: Pick<ChartStretch, "from" | "to" | "dateOnly">;
  /** Set where the button is on a page of its own (the device page), so the conversation leads back. */
  back?: { label: string };
}) {
  const { t } = useI18n();
  const { timeZone } = useDashboardEnv();
  const question = analyzeStretch(t, {
    subject,
    from: wallText(stretch.from, stretch.dateOnly),
    to: wallText(stretch.to, stretch.dateOnly),
    timeZone,
  });
  return <AskAiButton size="sm" label={question.label} question={question.text} {...(back ? { back } : {})} />;
}
