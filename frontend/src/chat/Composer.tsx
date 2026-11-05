import { ArrowUpIcon, SquareIcon } from "lucide-react";
import { type KeyboardEvent, type ReactNode, type Ref, useId } from "react";

import { Alert, AlertDescription } from "../components/ui/alert.tsx";
import { Button } from "../components/ui/button.tsx";
import { Spinner } from "../components/ui/spinner.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { cn } from "../lib/utils.ts";

/**
 * The box a question is typed into (docs/design/chat-dashboards/home.html and conversation.html):
 * Enter sends, Shift + Enter breaks the line, and an Enter that confirms an input method's
 * composition sends nothing. The send button is off while the box is empty or something is
 * being sent or answered; while a question is being answered it is the stop button instead. The
 * box grows with its text (CSS only) up to a limit and then scrolls.
 *
 * It holds no state of its own: the home page and the conversation page own the text, what is
 * being done and what went wrong, so a failed send leaves the text where it was typed. Room
 * for the later additions (mentions, templates, ghost text: Laysi/trustiot-v2#155, #156, #158)
 * is the box itself, which stays a plain textarea they can wrap.
 */
export function Composer({
  label,
  placeholder,
  rows,
  value,
  onChange,
  onSubmit,
  sending,
  answering,
  stopping,
  onStop,
  status,
  error,
  notice,
  textareaRef,
}: {
  /** What the box is called for a screen reader. */
  label: string;
  placeholder: string;
  rows: number;
  value: string;
  onChange: (value: string) => void;
  onSubmit: () => void;
  /** The question is on its way to the server. */
  sending: boolean;
  /** A question of this conversation is being answered: sending is off and the stop button shows. */
  answering: boolean;
  /** The stop request is on its way. */
  stopping: boolean;
  onStop: () => void;
  /** What is going on, read out when it appears ("Looking up readings…"). */
  status?: ReactNode;
  /** Why the last send or stop failed, already translated. */
  error?: string | null;
  /** Something that ended without an answer but is not a failure (the question was stopped). */
  notice?: string | null;
  textareaRef?: Ref<HTMLTextAreaElement>;
}) {
  const { t } = useI18n();
  const hintId = useId();
  const busy = sending || answering;
  const canSend = value.trim() !== "" && !busy;

  const keyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key !== "Enter" || event.shiftKey || event.nativeEvent.isComposing || event.keyCode === 229) return;
    event.preventDefault();
    if (canSend) onSubmit();
  };

  return (
    <div className="grid gap-2">
      {error ? (
        <Alert variant="destructive">
          <AlertDescription className="col-start-1">
            <p>{error}</p>
          </AlertDescription>
        </Alert>
      ) : null}
      {notice ? (
        <Alert role="status">
          <AlertDescription className="col-start-1 text-foreground">
            <p>{notice}</p>
          </AlertDescription>
        </Alert>
      ) : null}
      <form
        onSubmit={(event) => {
          event.preventDefault();
          if (canSend) onSubmit();
        }}
        className={cn(
          "grid gap-2 rounded-xl border border-input bg-card p-3 shadow-sm",
          "focus-within:outline-2 focus-within:outline-offset-2 focus-within:outline-ring",
          // On a phone the box and the send button share a row, and the hint goes away.
          "max-sm:grid-cols-[minmax(0,1fr)_auto] max-sm:items-end max-sm:py-2 max-sm:pr-2",
        )}
      >
        <label htmlFor={`${hintId}-box`} className="sr-only">
          {label}
        </label>
        <textarea
          ref={textareaRef}
          id={`${hintId}-box`}
          rows={rows}
          value={value}
          placeholder={placeholder}
          aria-describedby={hintId}
          readOnly={sending}
          onChange={(event) => onChange(event.target.value)}
          onKeyDown={keyDown}
          className="field-sizing-content block max-h-60 min-h-8 w-full resize-none bg-transparent p-1 text-base leading-6 text-foreground outline-none placeholder:text-muted-foreground"
        />
        <div className="flex items-center justify-between gap-2 max-sm:contents">
          <span id={hintId} className="text-xs text-muted-foreground max-sm:sr-only">
            {t("chat.composer.hint")}
          </span>
          {status ? (
            <p
              role="status"
              className="inline-flex items-center gap-2 text-sm text-muted-foreground max-sm:order-first max-sm:col-span-2"
            >
              {status}
            </p>
          ) : null}
          <div className="ml-auto flex items-center gap-2">
            {answering ? (
              <Button type="button" variant="outline" onClick={onStop} disabled={stopping}>
                {stopping ? <Spinner /> : <SquareIcon />}
                {t(stopping ? "chat.composer.stopping" : "chat.composer.stop")}
              </Button>
            ) : (
              <Button type="submit" size="icon" aria-label={t("chat.composer.send")} disabled={!canSend}>
                <ArrowUpIcon />
              </Button>
            )}
          </div>
        </div>
      </form>
    </div>
  );
}
