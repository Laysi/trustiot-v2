import { CircleAlertIcon, type LucideIcon, SparklesIcon } from "lucide-react";
import { useId } from "react";

import { askFailureReason } from "../../chat/questions.ts";
import { type I18n, useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { focusRing } from "../ui/button.tsx";
import { Spinner } from "../ui/spinner.tsx";
import { useAskQuestion } from "./useAskQuestion.ts";

/** The question with what it is about written after it, `question（a、b）`: the API takes text only. */
function withContext(i18n: Pick<I18n, "t">, question: string, context: readonly string[] | undefined): string {
  if (context === undefined || context.length === 0) return question;
  return i18n.t("ask.with_context", { question, context: context.join(i18n.t("ask.context_separator")) });
}

/**
 * The 「問 AI」 button (docs/design/home-ai/design.md, 「情境 AI 按鈕」): a pill with a sparkle,
 * one look for every place that asks the assistant a fixed question, so a user learns that this
 * shape uses the assistant (and its quota) and sends at once (decision 4). It sends `question`,
 * with `context` written after it; what the button says is `label`, which is the question itself
 * when it is not given (a button whose label is shorter than what it asks sits in a group that
 * names the subject, `AskGroup`).
 *
 * Where the button is decides where the question goes (`useAskQuestion`): in a conversation it
 * asks in that conversation, elsewhere it starts a new one and opens it, with `back` naming the
 * page to lead back to (decision 5). When the question cannot be sent (no permission, the quota
 * used up, the service paused) the reason stands under the button, tied to it with
 * `aria-describedby` and read out at once (design 12-D).
 */
export function AskAiButton({
  question,
  label,
  context,
  icon: Icon = SparklesIcon,
  size = "default",
  back,
  className,
  buttonClassName,
}: {
  /** What is sent. */
  question: string;
  /** What the button says; the question when absent. */
  label?: string;
  /** What the question is about, as text (a rule, a device, a time): written after the question. */
  context?: readonly string[];
  /** Replaces the sparkle (the suggested questions show their category). */
  icon?: LucideIcon;
  size?: "default" | "sm";
  /** The page the button is on, when it opens a new conversation: the conversation leads back to it. */
  back?: { label: string };
  /** Lays out the button with its error (a list item that fills its row, say). */
  className?: string;
  /** Shapes the button itself. */
  buttonClassName?: string;
}) {
  const i18n = useI18n();
  const { t } = i18n;
  const errorId = useId();
  const ask = useAskQuestion(back);
  // A button that is waiting stays focusable (`aria-disabled`), so the focus does not drop.
  const off = ask.pending || ask.blocked;
  const text = withContext(i18n, question, context);

  return (
    <span className={cn("inline-grid max-w-full min-w-0 justify-items-start gap-1.5", className)}>
      <button
        type="button"
        aria-disabled={off || undefined}
        aria-describedby={ask.error ? errorId : undefined}
        onClick={() => {
          if (off) return;
          ask.reset();
          ask.ask(text);
        }}
        className={cn(
          "inline-flex max-w-full min-w-0 items-center gap-2 rounded-full border border-[color-mix(in_oklab,var(--input)_55%,var(--border))] bg-card text-left leading-snug text-card-foreground transition-colors",
          "hover:bg-brand-soft aria-disabled:cursor-not-allowed aria-disabled:opacity-70 aria-disabled:hover:bg-card",
          "[&_svg]:pointer-events-none [&_svg]:size-4 [&_svg]:shrink-0",
          size === "sm" ? "min-h-8 px-3 py-1 text-[13px]" : "min-h-9 px-3.5 py-1.5 text-sm",
          "max-sm:min-h-11",
          focusRing,
          buttonClassName,
        )}
      >
        {ask.pending ? <Spinner /> : <Icon aria-hidden="true" className="text-primary" />}
        <span className="line-clamp-2 min-w-0 wrap-anywhere">{label ?? question}</span>
        {ask.pending ? <span className="sr-only">{t("ask.sending")}</span> : null}
      </button>
      {ask.error ? (
        <p id={errorId} role="alert" className="flex items-start gap-1.5 text-sm text-destructive">
          <CircleAlertIcon aria-hidden="true" className="mt-0.5 size-4 shrink-0" />
          <span className="min-w-0 wrap-anywhere">
            {t("ask.failed", { reason: askFailureReason(i18n, ask.error) })}
          </span>
        </p>
      ) : null}
    </span>
  );
}
