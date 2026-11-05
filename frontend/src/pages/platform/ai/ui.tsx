import { cva, type VariantProps } from "class-variance-authority";
import { ArrowRightIcon, ShieldAlertIcon } from "lucide-react";
import { type ReactNode, useId, useRef } from "react";

import { focusRing } from "../../../components/ui/button.tsx";
import { cn } from "../../../lib/utils.ts";

/*
 * The small pieces the platform AI pages share (docs/design/platform-admin/design.md, 「元件」).
 * The platform back office has its own set in `../ui/` (Laysi/trustiot-v2#132). `StatTile` was the
 * same in both and is taken from there; the others differ, so both sets stay for now: `Badge` (a
 * round tag with an `icon` slot on a tinted ground; `../ui/badge.tsx` is a square tag on
 * `--secondary` with the status color as text), `Meter` (a decorative SVG bar of a percentage
 * beside its number; `../ui/meter.tsx` is a labelled `<progress>` of a value against a limit),
 * `ChangePreview` (two boxes with a label each; `../ui/` has one box with one label), `ImpactList`,
 * `AuditNote` and `Skeleton` (other borders, grounds, icons or corners) and `DataTable` (the cells
 * stack differently from `../ui/stack-table.tsx`). `Tabs` and `Loading` exist only here.
 */

const badgeVariants = cva(
  "inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-xs font-medium whitespace-nowrap text-foreground [&_svg]:size-3 [&_svg]:shrink-0",
  {
    variants: {
      tone: {
        neutral: "border-transparent bg-muted",
        success: "border-transparent bg-success/15 [&_svg]:text-success",
        warning: "border-transparent bg-warning-soft [&_svg]:text-warning",
        danger: "border-transparent bg-destructive/10 [&_svg]:text-destructive",
        brand: "border-transparent bg-brand-soft",
        outline: "border-border bg-background",
      },
    },
    defaultVariants: { tone: "neutral" },
  },
);

/** A status tag: always words, with an icon where the state is not neutral, never color alone. */
export function Badge({
  tone,
  icon,
  children,
  className,
}: VariantProps<typeof badgeVariants> & { icon?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <span data-slot="badge" className={cn(badgeVariants({ tone }), className)}>
      {icon}
      {children}
    </span>
  );
}

/** A thin bar for a share of 100 percent; the number beside it says the same, so it is decoration. */
export function Meter({ percent, tone = "normal" }: { percent: number; tone?: "normal" | "warning" | "danger" }) {
  const width = Math.max(0, Math.min(100, percent));
  return (
    <svg
      aria-hidden="true"
      viewBox="0 0 100 4"
      preserveAspectRatio="none"
      className="mt-1 h-1.5 w-full min-w-16 overflow-hidden rounded-full"
    >
      <rect width="100" height="4" className="fill-muted" />
      <rect
        width={width}
        height="4"
        className={tone === "danger" ? "fill-destructive" : tone === "warning" ? "fill-warning" : "fill-brand-strong"}
      />
    </svg>
  );
}

/** The two boxes of a before-and-after, `change` in the design system. */
export function ChangePreview({
  beforeLabel,
  before,
  afterLabel,
  after,
}: {
  beforeLabel: string;
  before: ReactNode;
  afterLabel: string;
  after: ReactNode;
}) {
  return (
    <div className="grid items-stretch gap-2 sm:grid-cols-[1fr_auto_1fr]">
      <div className="grid min-w-0 gap-1 rounded-md border bg-muted/50 p-3">
        <span className="text-xs text-muted-foreground">{beforeLabel}</span>
        <span className="font-mono text-sm wrap-anywhere">{before}</span>
      </div>
      <ArrowRightIcon
        aria-hidden="true"
        className="size-4 self-center justify-self-center text-muted-foreground max-sm:rotate-90"
      />
      <div className="grid min-w-0 gap-1 rounded-md border bg-muted/50 p-3">
        <span className="text-xs text-muted-foreground">{afterLabel}</span>
        <span className="font-mono text-sm wrap-anywhere">{after}</span>
      </div>
    </div>
  );
}

/** What a change will do, as a short list under a heading. */
export function ImpactList({ title, items }: { title: string; items: readonly ReactNode[] }) {
  return (
    <div className="grid gap-1.5 rounded-md border bg-muted/50 p-3 text-sm">
      <strong>{title}</strong>
      <ul className="grid list-disc gap-1 pl-5 text-muted-foreground">
        {items.map((item, index) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: a fixed list in a fixed order
          <li key={index}>{item}</li>
        ))}
      </ul>
    </div>
  );
}

/** What the operation log will record for the change. */
export function AuditNote({ children }: { children: ReactNode }) {
  return (
    <p className="flex items-start gap-2 rounded-md border border-dashed p-3 text-xs text-muted-foreground">
      <ShieldAlertIcon aria-hidden="true" className="mt-0.5 size-4 shrink-0" />
      <span>{children}</span>
    </p>
  );
}

/** A grey placeholder row while data loads; the table around it keeps its headings. */
export function Skeleton({ className }: { className?: string }) {
  return <div aria-hidden="true" className={cn("animate-pulse rounded-md bg-muted", className)} />;
}

export interface TabItem {
  id: string;
  label: string;
}

/**
 * Tabs that switch what the panel below shows: the arrow keys move between them (automatic
 * activation), Home and End jump to the first and last. The panel is the caller's, labelled by
 * the selected tab through `panelProps`.
 */
export function Tabs({
  label,
  tabs,
  value,
  onChange,
  children,
}: {
  label: string;
  tabs: readonly TabItem[];
  value: string;
  onChange: (id: string) => void;
  children: ReactNode;
}) {
  const base = useId();
  const refs = useRef(new Map<string, HTMLButtonElement>());
  const move = (to: number) => {
    const next = tabs[(to + tabs.length) % tabs.length];
    if (!next) return;
    onChange(next.id);
    refs.current.get(next.id)?.focus();
  };
  return (
    <div className="grid gap-4">
      <div role="tablist" aria-label={label} className="flex gap-1 overflow-x-auto border-b">
        {tabs.map((tab, index) => {
          const selected = tab.id === value;
          return (
            <button
              key={tab.id}
              ref={(element) => {
                if (element) refs.current.set(tab.id, element);
                else refs.current.delete(tab.id);
              }}
              type="button"
              role="tab"
              id={`${base}-tab-${tab.id}`}
              aria-selected={selected}
              aria-controls={`${base}-panel-${tab.id}`}
              tabIndex={selected ? 0 : -1}
              onClick={() => onChange(tab.id)}
              onKeyDown={(event) => {
                if (event.key === "ArrowRight") move(index + 1);
                else if (event.key === "ArrowLeft") move(index - 1);
                else if (event.key === "Home") move(0);
                else if (event.key === "End") move(tabs.length - 1);
                else return;
                event.preventDefault();
              }}
              className={cn(
                "-mb-px rounded-t-md border-b-2 px-3 py-2 text-sm font-medium whitespace-nowrap",
                focusRing,
                selected
                  ? "border-primary text-foreground"
                  : "border-transparent text-muted-foreground hover:text-foreground",
              )}
            >
              {tab.label}
            </button>
          );
        })}
      </div>
      <div role="tabpanel" id={`${base}-panel-${value}`} aria-labelledby={`${base}-tab-${value}`} tabIndex={-1}>
        {children}
      </div>
    </div>
  );
}

/** A table with a hidden caption; on a narrow screen each row becomes a card (`Cell` prints the column's name). */
export function DataTable({
  caption,
  head,
  children,
  className,
}: {
  caption: string;
  head: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <div className="overflow-hidden rounded-lg border bg-card">
      <table className={cn("w-full border-collapse text-sm max-sm:block", className)}>
        <caption className="sr-only">{caption}</caption>
        <thead className="max-sm:sr-only">
          <tr className="border-b bg-muted/50 text-left text-xs text-muted-foreground">{head}</tr>
        </thead>
        <tbody className="max-sm:block">{children}</tbody>
      </table>
    </div>
  );
}

export function Th({ children, align }: { children: ReactNode; align?: "right" }) {
  return (
    <th scope="col" className={cn("px-3 py-2 font-medium", align === "right" && "text-right")}>
      {children}
    </th>
  );
}

export function Row({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <tr className={cn("border-b last:border-b-0 max-sm:grid max-sm:gap-1 max-sm:px-3 max-sm:py-3", className)}>
      {children}
    </tr>
  );
}

/** A cell; `label` is the column's name, shown before the value on a narrow screen. */
export function Cell({
  label,
  align,
  children,
  className,
}: {
  label: string;
  align?: "right";
  children: ReactNode;
  className?: string;
}) {
  return (
    <td
      className={cn(
        "px-3 py-2.5 align-top max-sm:flex max-sm:justify-between max-sm:gap-4 max-sm:p-0",
        align === "right" && "text-right tabular-nums max-sm:text-right",
        className,
      )}
    >
      <span className="text-xs text-muted-foreground sm:hidden">{label}</span>
      <span className="min-w-0 wrap-anywhere max-sm:text-right">{children}</span>
    </td>
  );
}

/** Placeholders while data loads: the region is busy and a hidden status says what is loading. */
export function Loading({ label, className, children }: { label: string; className?: string; children: ReactNode }) {
  return (
    <div aria-busy="true" className={className}>
      <span role="status" className="sr-only">
        {label}
      </span>
      {children}
    </div>
  );
}
