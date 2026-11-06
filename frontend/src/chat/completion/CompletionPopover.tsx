import {
  CalendarDaysIcon,
  CpuIcon,
  FileTextIcon,
  GaugeIcon,
  SearchIcon,
  TagIcon,
  TriangleAlertIcon,
} from "lucide-react";
import type { ReactNode } from "react";

import { Button } from "../../components/ui/button.tsx";
import { cn } from "../../lib/utils.ts";
import type { Group, Row } from "./groups.ts";
import { type MentionKind, matchSpan } from "./text.ts";

/** Everything the list needs to draw itself and to say what a click, a hover or a button did. */
export interface PopoverModel {
  /** The listbox's ID: the textarea's `aria-controls`. */
  listId: string;
  /** The ID of the option at `index`: the textarea's `aria-activedescendant` names one of them. */
  optionId: (index: number) => string;
  /** The listbox's accessible name. */
  label: string;
  /** A line above the groups (filling a blank: which one, and how many are left). */
  heading: string | null;
  /** The kind buttons, or `null` where the kind is fixed. */
  filtersLabel: string;
  filters: { kind: MentionKind | null; label: string; pressed: boolean }[] | null;
  groups: Group[];
  activeKey: string | null;
  /** What was typed after the `@`: the matching part of a name is marked. */
  highlight: string;
  /** Placeholder rows may show for the groups that are still loading. */
  skeleton: boolean;
  error: { title: string; body: string } | null;
  empty: { title: string; body: string } | null;
  retryLabel: string;
  tenantNote: string;
  keys: { move: string; insert: string; close: string; nextBlank: string | null };
  placement: "below" | "above";
  onChoose: (row: Row) => void;
  onHover: (key: string) => void;
  onFilter: (kind: MentionKind | null) => void;
  onRetry: () => void;
}

const ICONS: Record<Row["kind"], typeof CpuIcon> = {
  device: CpuIcon,
  point: GaugeIcon,
  model: TagIcon,
  range: CalendarDaysIcon,
  template: FileTextIcon,
};

/** The name with the part the query matched in bold and underlined: not by color alone. */
function Highlighted({ name, query }: { name: string; query: string }): ReactNode {
  const span = matchSpan(name, query);
  if (span === null) return name;
  return (
    <>
      {name.slice(0, span[0])}
      <mark className="bg-transparent font-bold text-inherit underline decoration-primary decoration-2 underline-offset-[3px]">
        {name.slice(span[0], span[1])}
      </mark>
      {name.slice(span[1])}
    </>
  );
}

function SkeletonRows({ label }: { label: string }) {
  return (
    <div className="grid gap-1 px-2 pb-2" aria-hidden="true">
      <span className="sr-only">{label}</span>
      {[0, 1, 2].map((row) => (
        <div key={row} className="grid grid-cols-[18px_1fr] items-center gap-2.5 py-1.5">
          <span className="size-[18px] rounded bg-muted" />
          <span className="h-3 w-2/3 animate-pulse rounded bg-muted" />
        </div>
      ))}
    </div>
  );
}

/**
 * The list under the box (design 04, 05 and 06): grouped options, the kind buttons, the states of a
 * lookup (placeholder rows, nothing found, failed) and a footer that says whose items they are. It
 * never takes focus: the box keeps it, and names the active option with `aria-activedescendant`.
 */
export function CompletionPopover({ model, loadingLabel }: { model: PopoverModel; loadingLabel: string }) {
  let index = 0;
  const titleId = (position: number) => `${model.listId}-group-${position}`;
  return (
    // The list is for pointers: the keyboard works the box. Pressing on it must not move focus out of the box.
    // biome-ignore lint/a11y/noStaticElementInteractions: a pointer-only guard against the box losing focus
    <div
      onMouseDown={(event) => event.preventDefault()}
      className={cn(
        "absolute inset-x-0 z-30 overflow-hidden rounded-xl border border-border bg-popover text-popover-foreground shadow-lg",
        model.placement === "below" ? "top-full mt-1.5" : "bottom-full mb-1.5",
      )}
    >
      {model.filters ? (
        // biome-ignore lint/a11y/useSemanticElements: a row of buttons inside the popup; a fieldset adds a frame the design does not draw
        <div role="group" aria-label={model.filtersLabel} className="flex flex-wrap gap-1.5 px-2 pt-2 pb-1">
          {model.filters.map((filter) => (
            <Button
              key={filter.kind ?? "all"}
              type="button"
              size="sm"
              variant={filter.pressed ? "secondary" : "outline"}
              aria-pressed={filter.pressed}
              // Tab inserts the active option, so the buttons are for pointers and touch.
              tabIndex={-1}
              className="h-7 rounded-full px-3 text-xs"
              onClick={() => model.onFilter(filter.kind)}
            >
              {filter.label}
            </Button>
          ))}
        </div>
      ) : null}
      {model.error ? (
        <div role="alert" className="mx-2 mt-1 grid gap-1 rounded-md bg-warning-soft px-3 py-2 text-sm text-foreground">
          <p className="flex items-center gap-2 font-medium">
            <TriangleAlertIcon className="size-4 shrink-0 text-warning" aria-hidden="true" />
            {model.error.title}
          </p>
          <p className="text-muted-foreground">{model.error.body}</p>
          <Button
            type="button"
            size="sm"
            variant="outline"
            tabIndex={-1}
            className="mt-1 w-fit"
            onClick={model.onRetry}
          >
            {model.retryLabel}
          </Button>
        </div>
      ) : null}
      <div
        id={model.listId}
        role="listbox"
        aria-label={model.label}
        className={cn("max-h-[340px] overflow-auto p-1", model.groups.length === 0 && "hidden")}
      >
        {model.heading ? (
          <p className="px-2 pt-2 pb-1 text-xs font-semibold text-muted-foreground">{model.heading}</p>
        ) : null}
        {model.groups.map((group, position) =>
          group.rows.length === 0 && !model.skeleton ? null : (
            // biome-ignore lint/a11y/useSemanticElements: option groups of a listbox must be role="group"; a fieldset is not allowed there
            <div key={group.id} role="group" aria-labelledby={titleId(position)}>
              <div className="flex justify-between gap-2 px-2 pt-2 pb-1 text-xs font-semibold text-muted-foreground">
                <span id={titleId(position)}>{group.title}</span>
                <span className="font-normal">{group.rows.length > 0 ? group.total : loadingLabel}</span>
              </div>
              {group.rows.map((row) => {
                const rowIndex = index;
                index += 1;
                const Icon = ICONS[row.kind];
                const active = row.key === model.activeKey;
                return (
                  // Options are chosen by pointer here and by the box's own key handler: the option itself is not focusable.
                  // biome-ignore lint/a11y/useKeyWithClickEvents: the keyboard works through the textarea (aria-activedescendant)
                  <div
                    key={row.key}
                    id={model.optionId(rowIndex)}
                    role="option"
                    aria-selected={active}
                    tabIndex={-1}
                    onClick={() => model.onChoose(row)}
                    onMouseMove={() => {
                      if (!active) model.onHover(row.key);
                    }}
                    className={cn(
                      "grid cursor-pointer grid-cols-[18px_minmax(0,1fr)] items-center gap-x-2.5 rounded-md px-2 py-1.5 max-sm:min-h-11",
                      active
                        ? "bg-accent text-accent-foreground shadow-[inset_3px_0_0_var(--primary)]"
                        : "hover:bg-accent/60",
                    )}
                  >
                    <Icon
                      className={cn("size-4", active ? "text-primary" : "text-muted-foreground")}
                      aria-hidden="true"
                    />
                    <span className="truncate font-medium" title={row.name}>
                      <Highlighted name={row.name} query={model.highlight} />
                    </span>
                    <span className="col-start-2 truncate text-xs text-muted-foreground">{row.meta}</span>
                  </div>
                );
              })}
              {group.loading && model.skeleton ? <SkeletonRows label={loadingLabel} /> : null}
            </div>
          ),
        )}
      </div>
      {model.empty ? (
        <div role="status" className="grid gap-1.5 px-3 py-4 text-sm">
          <p className="flex items-center gap-2 font-medium">
            <SearchIcon className="size-4 shrink-0" aria-hidden="true" />
            {model.empty.title}
          </p>
          <p className="text-muted-foreground">{model.empty.body}</p>
        </div>
      ) : null}
      <div className="flex flex-wrap items-center gap-x-3.5 gap-y-1 border-t border-border px-3 py-2 text-xs text-muted-foreground">
        <span className="max-sm:hidden" aria-hidden="true">
          ↑ ↓ {model.keys.move}
        </span>
        <span className="max-sm:hidden" aria-hidden="true">
          Enter {model.keys.nextBlank === null ? "/ Tab " : ""}
          {model.keys.insert}
        </span>
        {model.keys.nextBlank ? (
          <span className="max-sm:hidden" aria-hidden="true">
            Tab {model.keys.nextBlank}
          </span>
        ) : null}
        <span className="max-sm:hidden" aria-hidden="true">
          Esc {model.keys.close}
        </span>
        <span className="sm:ml-auto">{model.tenantNote}</span>
      </div>
    </div>
  );
}
