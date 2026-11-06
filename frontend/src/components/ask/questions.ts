import type { PlatformComponent } from "../../dashboard/spec.ts";
import type { I18n } from "../../i18n/useI18n.ts";

// The questions the context buttons send (docs/design/home-ai/design.md, 「情境 AI 按鈕」). Each is
// a fixed template whose blanks are filled with the device and the time range the button is about,
// never written by a model, so a button asks the same question every time and a test can say what
// it is. A template has two texts: the label the button shows (short, ends in a question mark
// where it is one) and the text that is sent, which names what the label leaves to the page around
// it. The assistant's question API takes the text only (`AskQuestion` in api/openapi.json has
// `question` and nothing else), so the device and the range are written into it; a structured
// context (device ID, range) needs a field the API does not have yet.

type Translate = I18n["t"];

/** What a button says and what it sends. */
export interface AskQuestion {
  label: string;
  text: string;
}

const DAY_MS = 86_400_000;

/** The calendar fields of `instant` in `timeZone` (an IANA name), as two-digit strings (the year as four). */
function zonedFields(instant: number, timeZone: string) {
  const parts = new Intl.DateTimeFormat("en-US", {
    timeZone,
    hourCycle: "h23",
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  }).formatToParts(instant);
  const field = (type: Intl.DateTimeFormatPartTypes) => parts.find((part) => part.type === type)?.value ?? "00";
  return {
    year: field("year"),
    month: field("month"),
    day: field("day"),
    hour: field("hour"),
    minute: field("minute"),
  };
}

/** `instant` as `YYYY-MM-DD HH:mm` on the wall clock of `timeZone`. */
export function instantText(instant: number, timeZone: string): string {
  const { year, month, day, hour, minute } = zonedFields(instant, timeZone);
  return `${year}-${month}-${day} ${hour}:${minute}`;
}

/**
 * A chart's x value, which is a wall-clock time held as the milliseconds of the same fields in
 * UTC (dashboard/format.ts), as `YYYY-MM-DD HH:mm`, or `YYYY-MM-DD` for a chart of whole days.
 */
export function wallText(wallMs: number, dateOnly = false): string {
  const iso = new Date(wallMs).toISOString();
  return dateOnly ? iso.slice(0, 10) : `${iso.slice(0, 10)} ${iso.slice(11, 16)}`;
}

/** What a week reads as in a question: its first and last day, `YYYY-MM-DD`. */
export interface DayRange {
  from: string;
  to: string;
}

/**
 * This week so far and the whole week before it, in `timeZone`. A week runs from Monday to
 * Sunday (the mention menu's weeks, docs/design/home-ai/design.md), so this week ends today.
 */
export function weekRanges(now: number, timeZone: string): { thisWeek: DayRange; lastWeek: DayRange } {
  const { year, month, day } = zonedFields(now, timeZone);
  const today = Date.UTC(Number(year), Number(month) - 1, Number(day));
  const sinceMonday = (new Date(today).getUTCDay() + 6) % 7;
  const monday = today - sinceMonday * DAY_MS;
  const text = (ms: number) => new Date(ms).toISOString().slice(0, 10);
  return {
    thisWeek: { from: text(monday), to: text(today) },
    lastWeek: { from: text(monday - 7 * DAY_MS), to: text(monday - DAY_MS) },
  };
}

const range = (days: DayRange) => `${days.from} – ${days.to}`;

/** 「跟上週比」: the device's week so far against the week before. */
export function compareLastWeek(t: Translate, device: { name: string; now: number; timeZone: string }): AskQuestion {
  const { thisWeek, lastWeek } = weekRanges(device.now, device.timeZone);
  return {
    label: t("ask.device.compare_week.label"),
    text: t("ask.device.compare_week.text", {
      device: device.name,
      this_week: range(thisWeek),
      last_week: range(lastWeek),
      zone: device.timeZone,
    }),
  };
}

/** 「為什麼離線？」: no reading since `lastSeen`. */
export function whyOffline(t: Translate, device: { name: string; lastSeen: number; timeZone: string }): AskQuestion {
  return {
    label: t("ask.device.why_offline.label"),
    text: t("ask.device.why_offline.text", {
      device: device.name,
      since: instantText(device.lastSeen, device.timeZone),
      zone: device.timeZone,
    }),
  };
}

/** 「為什麼沒有資料？」: a device that has stored nothing. */
export function whyNoData(t: Translate, device: { name: string }): AskQuestion {
  return {
    label: t("ask.device.why_no_data.label"),
    text: t("ask.device.why_no_data.text", { device: device.name }),
  };
}

/** What a chart's stretch is about: a device's data point, or a dashboard component by its title. */
export type StretchSubject = { kind: "point"; device: string; point: string } | { kind: "component"; title: string };

/**
 * 「分析這段」: the stretch between two wall-clock times of a chart, as `wallText` writes them,
 * in `timeZone`.
 */
export function analyzeStretch(
  t: Translate,
  stretch: { subject: StretchSubject; from: string; to: string; timeZone: string },
): AskQuestion {
  const { subject } = stretch;
  return {
    label: t("ask.stretch.label"),
    text: t("ask.stretch.text", {
      subject:
        subject.kind === "point"
          ? t("ask.stretch.subject_device", { device: subject.device, point: subject.point })
          : t("ask.stretch.subject_component", { title: subject.title }),
      from: stretch.from,
      to: stretch.to,
      zone: stretch.timeZone,
    }),
  };
}

type ComponentTemplate = "explain" | "compare" | "peak" | "top" | "why_value" | "why_status";

/** The questions each kind of component offers (two or three, design 08). */
const COMPONENT_TEMPLATES: Readonly<Record<PlatformComponent["kind"], readonly ComponentTemplate[]>> = {
  time_series: ["explain", "compare", "peak"],
  bar: ["explain", "compare", "top"],
  kpi: ["why_value", "compare"],
  status: ["why_status", "compare"],
  table: ["explain", "compare"],
};

/**
 * The questions under a component in an answer, from its kind and its title. A component names no
 * device or range (its data comes from datasets, docs/architecture/ai-assistant.md, 「元件模式」),
 * so the title is what ties the question to it. A kind this build has no template for (the types
 * leave none out, but a card is drawn for whatever the version holds) gets none, not an error.
 */
export function componentQuestions(t: Translate, spec: PlatformComponent): AskQuestion[] {
  const templates = COMPONENT_TEMPLATES[spec.kind] as readonly ComponentTemplate[] | undefined;
  return (templates ?? []).map((template) => ({
    label: t(`ask.component.${template}.label`),
    text: t(`ask.component.${template}.text`, { title: spec.title }),
  }));
}

/**
 * What an `AskAiButton` of the alerts and reports pages is given: the question (and the label
 * when the button says more than it asks) and what it is about, which the button writes after the
 * question. Like the templates above they are fixed text, never a model's.
 */
export interface AskProps {
  question: string;
  label?: string;
  context: string[];
}

/** The tenant's devices over the last two weeks: what the AI looks at to suggest alert rules. */
function allDevicesRecently(t: Translate): string[] {
  return [t("ask.alert.context_all_devices"), t("ask.alert.context_14_days")];
}

/** 「請 AI 建議警報規則」: the page head, and the places where there are no alerts or rules yet. */
export function suggestRules(t: Translate): AskProps {
  return { question: t("ask.alert.suggest_rules"), context: allDevicesRecently(t) };
}

/** 「先問 AI 有哪些異常」, on the wall of a plan without alerts. */
export function firstUnusual(t: Translate): AskProps {
  return { question: t("ask.alert.first_unusual"), context: allDevicesRecently(t) };
}

/** 「解釋這條規則？」 on a rule's page. */
export function explainRule(t: Translate, rule: { name: string; version: number }): AskProps {
  return {
    question: t("ask.alert.explain_rule"),
    context: [
      t("ask.alert.context_rule", { rule: rule.name }),
      t("ask.alert.context_version", { version: rule.version }),
    ],
  };
}

/** 「跟上週比？」 for a rule. */
export function compareRuleLastWeek(t: Translate, rule: { name: string; version: number }): AskProps {
  return { ...explainRule(t, rule), question: t("ask.alert.compare_last_week") };
}

/** 「為什麼觸發？」 (「為什麼離線？」 for an offline notice) for a notification; `time` is written already. */
export function whyNotified(
  t: Translate,
  notification: { offline: boolean; rule: string; device: string; time: string },
): AskProps {
  return {
    question: t(notification.offline ? "ask.alert.why_offline" : "ask.alert.why_triggered"),
    context: [
      t("ask.alert.context_rule", { rule: notification.rule }),
      t("ask.alert.context_device", { device: notification.device }),
      t("ask.alert.context_around", { time: notification.time }),
    ],
  };
}

/** 「分析這段」 around one firing of a rule: the button says which, the question carries the rest. */
export function analyzeTrigger(t: Translate, trigger: { rule: string; device: string; time: string }): AskProps {
  return {
    question: t("ask.alert.analyze_stretch"),
    label: t("ask.alert.analyze_stretch_at", { device: trigger.device, time: trigger.time }),
    context: [
      t("ask.alert.context_rule", { rule: trigger.rule }),
      t("ask.alert.context_device", { device: trigger.device }),
      t("ask.alert.context_around", { time: trigger.time }),
    ],
  };
}

/** The three questions under a report; `about` is its title and period. */
export function reportQuestions(t: Translate, about: string): AskProps[] {
  return (["explain", "why_changed", "save_energy"] as const).map((name) => ({
    question: t(`ask.report.${name}`),
    context: [about],
  }));
}

/** 「上個月的用電是多少？」, on the reports wall of a plan without reports. */
export function lastMonthUse(t: Translate): AskProps {
  return { question: t("ask.report.last_month"), context: [] };
}

/** 「本週用電是多少？」, beside the weekly report in the setup checklist. */
export function thisWeekUse(t: Translate): AskProps {
  return { question: t("ask.report.this_week"), context: [] };
}
