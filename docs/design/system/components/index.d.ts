// TrustIoT components, as documentation. The existing ones mirror frontend/src/components/ui/*.tsx;
// the dashboard ones (StatusBadge and below) are the proposed contracts from this design system.
import type { ComponentProps, ReactNode } from "react";

/** button.tsx — a plain button defaults to type="button"; asChild styles its only child (a link). */
export interface ButtonProps extends ComponentProps<"button"> {
  variant?: "default" | "destructive" | "outline" | "secondary" | "ghost" | "link";
  size?: "default" | "sm" | "lg" | "icon";
  asChild?: boolean;
}

/** input.tsx — native input with the shared control styles; aria-invalid marks a rejected value. */
export interface InputProps extends ComponentProps<"input"> {}

/** label.tsx — htmlFor is required. */
export interface LabelProps extends ComponentProps<"label"> {
  htmlFor: string;
}

/** form-field.tsx — what a field passes to its control. */
export interface FieldControlProps {
  id: string;
  "aria-describedby"?: string;
  "aria-invalid"?: true;
}

/** form-field.tsx — a labelled control with an optional hint and error. */
export interface FormFieldProps {
  label: ReactNode;
  description?: ReactNode;
  error?: string;
  className?: string;
  children: (control: FieldControlProps) => ReactNode;
}

/** native-select.tsx — the browser's select, styled like Input (Radix Select is refused by the CSP). */
export interface NativeSelectProps extends ComponentProps<"select"> {}

/** card.tsx — Card is a <section>; CardTitle picks its heading level with `as` (h2 by default). */
export interface CardProps extends ComponentProps<"section"> {}
export interface CardTitleProps extends ComponentProps<"h2"> {
  as?: "h1" | "h2" | "h3";
}

/** alert.tsx — errors keep role="alert"; a notice passes role="status". */
export interface AlertProps extends ComponentProps<"div"> {
  variant?: "default" | "destructive";
  role?: "alert" | "status";
}

/** alert-dialog.tsx — Radix Alert Dialog parts: AlertDialog, Trigger, Content, Header, Footer, Title, Description, Cancel. */
export interface AlertDialogProps {
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  children: ReactNode;
}

/** dropdown-menu.tsx — Radix Dropdown Menu parts: DropdownMenu, Trigger, Content, Item, RadioGroup, RadioItem, Label, Separator, Group. */
export interface DropdownMenuItemProps {
  variant?: "default" | "destructive";
  disabled?: boolean;
  onSelect?: (event: Event) => void;
  children: ReactNode;
}

/** spinner.tsx — with a label it is announced as a status. */
export interface SpinnerProps {
  className?: string;
  label?: string;
}

// ---------------------------------------------------------------- 新增（儀表板）

export type Status = "normal" | "warning" | "critical" | "offline";

/** Where a number comes from; SourcePopover shows it. Every text is inserted with textContent. */
export interface DataSource {
  title: string;
  description: string;
  /** ISO-8601 with offset; shown as "2026-11-01 14:32:05（UTC+8）". */
  queriedAt: string;
  range?: { from: string; to: string };
  rowCount?: number;
  sql: string;
}

export interface StatusBadgeProps {
  status: Status;
  variant?: "soft" | "plain";
  /** Overrides the word, e.g. "離線 3 小時"; the icon stays. */
  label?: string;
}

export interface StatTileProps {
  label: string;
  /** Already formatted with Intl. */
  value: string;
  unit?: string;
  status?: Status;
  delta?: { text: string; direction: "up" | "down"; judgement: "good" | "bad" | "neutral" };
  /** At least 12 points, oldest first. */
  trend?: number[];
  stale?: boolean;
  source: DataSource;
}

export interface Threshold {
  value: number;
  status: "warning" | "critical";
}

export interface TimeSeries {
  name: string;
  /** time in ms since the epoch; value null where nothing was stored (the line breaks). */
  points: { time: number; value: number | null }[];
}

export interface TimeSeriesChartProps {
  /** Takes chart-series-1…6 in order; more than six fold into 「其他」. */
  series: TimeSeries[];
  from: number;
  to: number;
  intervalMs: number;
  timeZone: string;
  unit: string;
  kind?: "line" | "area";
  thresholds?: Threshold[];
  /** Names the chart for assistive technology, with how to use it. */
  label: string;
  formatValue(value: number): string;
  formatTick(time: number, step: number): string;
  source: DataSource;
}

export interface BarChartProps {
  categories: { name: string; value: number }[];
  unit: string;
  orientation?: "vertical" | "horizontal";
  label: string;
  formatValue(value: number): string;
  source: DataSource;
}

export interface GaugeProps {
  label: string;
  value: number;
  unit: string;
  min: number;
  max: number;
  thresholds: { warning: number; critical: number };
  source: DataSource;
}

export interface DataTableColumn<Row> {
  key: keyof Row & string;
  header: string;
  align?: "start" | "end";
  sortable?: boolean;
}

export interface DataTableProps<Row> {
  caption: string;
  columns: DataTableColumn<Row>[];
  rows: Row[];
  rowKey: keyof Row & string;
  selectedKey?: string;
  source?: DataSource;
}

export interface SourcePopoverProps {
  source: DataSource;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The button that opened it; focus returns there on close. */
  trigger: ReactNode;
}

// ---------------------------------------------------------------- 對話為主軸（2026-11-01）
// Proposed contracts: the home page is the conversation; dashboards are answers that were pinned.

/** One suggested question, written from the tenant's own devices. */
export interface SuggestedQuestion {
  id: string;
  /** The question as the user would ask it, e.g. 「冷卻水塔 2 號這週為什麼耗電變高？」 */
  text: string;
  /** Why it was suggested, shown under it, e.g. 「冷卻水塔 2 號・用電比上週高 18%」 */
  reason?: string;
}

export interface SuggestedQuestionsProps {
  /** 4–6 on the empty home page; 1–3 as follow-ups under an answer. */
  questions: SuggestedQuestion[];
  variant?: "cards" | "followups";
  /** Fills the composer; it never sends by itself. */
  onPick(question: SuggestedQuestion): void;
}

export interface ChatComposerProps {
  value: string;
  onChange(value: string): void;
  onSend(): void;
  /** While an answer is generated the send button becomes 「停止」. */
  generating?: boolean;
  onStop?(): void;
  /** Visible label; on the home page it may be visually hidden, never removed. */
  label: string;
  placeholder?: string;
  /** Rendered above the field on the empty home page. */
  suggestions?: SuggestedQuestion[];
}

/** One component inside an answer: the same declarative spec a dashboard stores. */
export type AnswerEmbed =
  | { kind: "stat-tiles"; tiles: StatTileProps[] }
  | { kind: "time-series"; title: string; description?: string; chart: TimeSeriesChartProps }
  | { kind: "bar"; title: string; description?: string; chart: BarChartProps }
  | { kind: "gauge"; gauge: GaugeProps }
  | { kind: "table"; table: DataTableProps<Record<string, unknown>> };

export interface MessageAnswerProps {
  /** One to three short sentences, conclusion first; plain text, inserted with textContent. */
  text: string;
  embeds: AnswerEmbed[];
  /** Every number in the answer has one; the 「資料來源」 button opens SourcePopover with it. */
  source: DataSource;
  followups?: SuggestedQuestion[];
  /** Name of the dashboard it was pinned to; absent = not pinned. */
  pinnedTo?: string;
  onPin(): void;
  onOpenDashboard?(): void;
  status?: "generating" | "done" | "error";
}
