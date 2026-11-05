import {
  CircleCheckIcon,
  CircleOffIcon,
  CircleXIcon,
  PauseCircleIcon,
  SparklesIcon,
  UserIcon,
  XIcon,
} from "lucide-react";
import { type KeyboardEvent, type ReactNode, useRef } from "react";
import { Link } from "react-router";

import type { RuleOrigin } from "../../api/pending/alerts.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import type { RuleState } from "./state.ts";

const badgeClasses =
  "inline-flex items-center gap-1 rounded-sm px-1.5 py-0.5 text-xs font-medium whitespace-nowrap [&_svg]:size-3.5 [&_svg]:shrink-0";

/**
 * Who started the rule (decision Q1): the agent, or a member, whose name is kept. An icon and the
 * words, never colour alone; the brand-soft background carries foreground text only.
 */
export function OriginBadge({ origin, showName = false }: { origin: RuleOrigin; showName?: boolean }) {
  const { t } = useI18n();
  if (origin.kind === "ai") {
    return (
      <span className={cn(badgeClasses, "bg-brand-soft text-foreground")}>
        <SparklesIcon aria-hidden="true" className="text-primary" />
        {t("alerts.origin.ai")}
      </span>
    );
  }
  return (
    <span className={cn(badgeClasses, "bg-secondary text-secondary-foreground")}>
      <UserIcon aria-hidden="true" />
      {showName ? t("alerts.origin.member_named", { name: origin.name }) : t("alerts.origin.member")}
    </span>
  );
}

/** A rule's state as icon and words (design 「顏色與圖示」): `--success` only on an icon that has text beside it. */
export function StateBadge({ state }: { state: RuleState }) {
  const { t } = useI18n();
  switch (state) {
    case "active":
      return (
        <span className={cn(badgeClasses, "text-foreground")}>
          <CircleCheckIcon aria-hidden="true" className="text-success" />
          {t("alerts.state.active")}
        </span>
      );
    case "disabled":
      return (
        <span className={cn(badgeClasses, "bg-muted text-foreground")}>
          <CircleOffIcon aria-hidden="true" />
          {t("alerts.state.disabled")}
        </span>
      );
    case "paused":
      return (
        <span className={cn(badgeClasses, "bg-muted text-foreground")}>
          <PauseCircleIcon aria-hidden="true" />
          {t("alerts.state.paused")}
        </span>
      );
    case "backtest_failed":
      return (
        <span className={cn(badgeClasses, "border border-destructive/50 text-destructive")}>
          <CircleXIcon aria-hidden="true" />
          {t("alerts.state.backtest_failed")}
        </span>
      );
  }
}

/**
 * The "Ask AI" entry (design 「「問 AI」入口」): a pill with the fixed question, which opens a new
 * conversation and sends it with the context chips. The conversation start (Laysi/trustiot-v2#158,
 * #1044) does not exist yet, so the button leads to the conversation home with the question and the
 * chips in the route state, `{ ask: { question, context } }`, for that page to read.
 */
export function AskAiButton({
  question,
  context,
  size = "default",
  label,
}: {
  question: string;
  context: readonly string[];
  size?: "default" | "sm";
  /** The accessible name, when the question alone does not say what it is about. */
  label?: string;
}) {
  return (
    <Link
      to="/"
      state={{ ask: { question, context } }}
      {...(label ? { "aria-label": label } : {})}
      className={cn(
        "inline-flex shrink-0 items-center gap-1.5 rounded-full border border-ask-border bg-card font-medium whitespace-nowrap text-card-foreground transition-colors hover:bg-brand-soft [&_svg]:size-4 [&_svg]:shrink-0",
        size === "sm" ? "h-8 px-3 text-xs" : "h-9 px-4 text-sm",
        focusRing,
      )}
    >
      <SparklesIcon aria-hidden="true" className="text-primary" />
      {question}
    </Link>
  );
}

/**
 * Tabs with the keyboard of the pattern: one tab in the tab order, the arrow keys, Home and End
 * move between them and select (the panel follows the selection, so there is nothing to
 * activate). A part of the page, not a general component: Radix's tabs are not a dependency.
 */
export function Tabs<Id extends string>({
  label,
  value,
  onChange,
  tabs,
  idPrefix,
}: {
  label: string;
  value: Id;
  onChange(id: Id): void;
  tabs: readonly { id: Id; label: string; count?: { text: string; label: string } | undefined }[];
  idPrefix: string;
}) {
  const refs = useRef(new Map<Id, HTMLButtonElement>());
  const move = (event: KeyboardEvent, index: number) => {
    const last = tabs.length - 1;
    const target =
      event.key === "ArrowRight" || event.key === "ArrowDown"
        ? (index + 1) % tabs.length
        : event.key === "ArrowLeft" || event.key === "ArrowUp"
          ? (index + last) % tabs.length
          : event.key === "Home"
            ? 0
            : event.key === "End"
              ? last
              : null;
    if (target === null) return;
    event.preventDefault();
    const next = tabs[target];
    if (next === undefined) return;
    onChange(next.id);
    refs.current.get(next.id)?.focus();
  };
  return (
    <div role="tablist" aria-label={label} className="flex gap-1 border-b">
      {tabs.map((tab, index) => {
        const selected = tab.id === value;
        return (
          <button
            key={tab.id}
            ref={(element) => {
              if (element) refs.current.set(tab.id, element);
              else refs.current.delete(tab.id);
            }}
            id={`${idPrefix}-tab-${tab.id}`}
            type="button"
            role="tab"
            aria-selected={selected}
            aria-controls={`${idPrefix}-panel-${tab.id}`}
            tabIndex={selected ? 0 : -1}
            className={cn(
              "-mb-px inline-flex items-center gap-2 border-b-2 px-3 py-2 text-sm font-medium",
              selected
                ? "border-primary text-foreground"
                : "border-transparent text-muted-foreground hover:text-foreground",
              focusRing,
            )}
            onClick={() => onChange(tab.id)}
            onKeyDown={(event) => move(event, index)}
          >
            {tab.label}
            {tab.count ? (
              <span className="rounded-full bg-secondary px-2 py-0.5 text-xs text-secondary-foreground">
                <span aria-hidden="true">{tab.count.text}</span>
                <span className="sr-only">{tab.count.label}</span>
              </span>
            ) : null}
          </button>
        );
      })}
    </div>
  );
}

/** Two or three choices in a row, one pressed (the filters "unread / all", "all / AI-made / member-made"). */
export function Segmented<Id extends string>({
  label,
  value,
  onChange,
  options,
}: {
  label: string;
  value: Id;
  onChange(id: Id): void;
  options: readonly { id: Id; label: string }[];
}) {
  return (
    <fieldset className="m-0 inline-flex min-w-0 rounded-md border bg-background p-0.5">
      <legend className="sr-only">{label}</legend>
      {options.map((option) => (
        <button
          key={option.id}
          type="button"
          aria-pressed={option.id === value}
          className={cn(
            "rounded-sm px-3 py-1 text-sm font-medium",
            option.id === value ? "bg-accent text-accent-foreground" : "text-muted-foreground hover:text-foreground",
            focusRing,
          )}
          onClick={() => onChange(option.id)}
        >
          {option.label}
        </button>
      ))}
    </fieldset>
  );
}

/** The error of a write that has no dialog of its own (enabling a rule, resuming, marking read), dismissable. */
export function ActionError({ message, onDismiss }: { message: string; onDismiss(): void }) {
  const { t } = useI18n();
  return (
    <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
      <AlertDescription className="col-start-1">
        <p>{message}</p>
      </AlertDescription>
      <Button variant="ghost" size="icon" aria-label={t("alerts.dismiss")} onClick={onDismiss}>
        <XIcon aria-hidden="true" />
      </Button>
    </Alert>
  );
}

/** Rows standing in for a list while it loads, announced once. */
export function ListSkeleton({ label }: { label: string }) {
  return (
    <section aria-busy="true" className="grid gap-4 rounded-lg border bg-card px-4 py-5 sm:px-6">
      <p role="status" className="sr-only">
        {label}
      </p>
      {[0, 1, 2].map((row) => (
        <div key={row} aria-hidden="true" className="grid gap-2">
          <span className="h-3 w-[70%] animate-pulse rounded bg-muted" />
          <span className="h-2.5 w-[40%] animate-pulse rounded bg-muted" />
        </div>
      ))}
    </section>
  );
}

/** A card with a heading, for the rule page's sections. */
export function Section({
  id,
  title,
  note,
  aside,
  children,
}: {
  id: string;
  title: string;
  note?: ReactNode;
  aside?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section aria-labelledby={`${id}-title`} id={id} className="rounded-lg border bg-card text-card-foreground">
      <div className="flex items-start justify-between gap-3 border-b px-4 py-3">
        <div className="grid gap-0.5">
          <h2 id={`${id}-title`} className="font-semibold">
            {title}
          </h2>
          {note ? <p className="text-sm text-muted-foreground">{note}</p> : null}
        </div>
        {aside}
      </div>
      <div className="grid gap-3 px-4 py-4">{children}</div>
    </section>
  );
}
