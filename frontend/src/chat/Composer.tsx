import { ArrowUpIcon, SquareIcon } from "lucide-react";
import { type KeyboardEvent, type ReactNode, type Ref, useId } from "react";

import { Alert, AlertDescription } from "../components/ui/alert.tsx";
import { Button } from "../components/ui/button.tsx";
import { Spinner } from "../components/ui/spinner.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { cn } from "../lib/utils.ts";
import { CompletionField, CompletionTools } from "./completion/CompletionField.tsx";
import { GhostHint, GhostTail, GhostToolbar } from "./completion/Ghost.tsx";
import { type SubmittedMention, useCompletion } from "./completion/useCompletion.ts";
import { useGhostCompletion } from "./completion/useGhostCompletion.ts";

/**
 * The box a question is typed into (docs/design/chat-dashboards/home.html and conversation.html):
 * Enter sends, Shift + Enter breaks the line, and an Enter that confirms an input method's
 * composition sends nothing. The send button is off while the box is empty or something is
 * being sent or answered; while a question is being answered it is the stop button instead. The
 * box grows with its text (CSS only) up to a limit and then scrolls.
 *
 * It holds no state of its own: the home page and the conversation page own the text, what is
 * being done and what went wrong, so a failed send leaves the text where it was typed.
 *
 * Two completions share the box. Mentions and question templates (Laysi/trustiot-v2#155) wrap it:
 * `CompletionField` draws the mirror layer behind the transparent text, and `CompletionTools` the
 * toolbar buttons. The grey continuation (Laysi/trustiot-v2#156, `useGhostCompletion`) is drawn in
 * that same mirror layer, after the text (`tail`). Keys go to the mention list and the template
 * blanks first, then to the grey text (Tab accepts, Esc dismisses), then to the box's own Enter.
 * While a list is open or blanks are left the grey text is paused (no request, nothing shown), so
 * one Tab is never taken by both.
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
  /** Sends the question; `mentions` are the `@` mentions in it, with what each stands for (Laysi/trustiot-v2#155). */
  onSubmit: (context: { mentions: SubmittedMention[] }) => void;
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
  const completion = useCompletion({ value, onChange });
  const canSend = value.trim() !== "" && !busy && completion.slotsLeft === 0;
  // The mention list and a template's blanks own Tab and Esc while they are there.
  const ghost = useGhostCompletion({
    value,
    disabled: busy,
    paused: completion.popover !== null || completion.slotsLeft > 0,
    textareaRef,
  });
  const { ref: ghostRef, ...ghostHandlers } = ghost.textareaProps;

  const keyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (completion.onKeyDown(event)) return;
    if (ghost.handleKeyDown(event)) return;
    if (event.key !== "Enter" || event.shiftKey || event.nativeEvent.isComposing || event.keyCode === 229) return;
    event.preventDefault();
    if (canSend) onSubmit({ mentions: completion.mentions });
  };

  return (
    <div className="grid gap-2">
      {error ? (
        <Alert variant="destructive" className="grid-cols-[1fr]">
          <AlertDescription className="col-start-1">
            <p>{error}</p>
          </AlertDescription>
        </Alert>
      ) : null}
      {notice ? (
        <Alert role="status" className="grid-cols-[1fr]">
          <AlertDescription className="col-start-1 text-foreground">
            <p>{notice}</p>
          </AlertDescription>
        </Alert>
      ) : null}
      <form
        onSubmit={(event) => {
          event.preventDefault();
          if (canSend) onSubmit({ mentions: completion.mentions });
        }}
        className={cn(
          "relative grid gap-2 rounded-xl border border-input bg-card p-3 shadow-sm",
          "focus-within:outline-2 focus-within:outline-offset-2 focus-within:outline-ring",
          // On a phone the box and the send button share a row, and the hint goes away.
          "max-sm:grid-cols-[minmax(0,1fr)_auto] max-sm:items-end max-sm:py-2 max-sm:pr-2",
        )}
      >
        <label htmlFor={`${hintId}-box`} className="sr-only">
          {label}
        </label>
        <CompletionField
          {...ghostHandlers}
          completion={completion}
          textareaRef={ghostRef}
          tail={<GhostTail ghost={ghost} value={value} />}
          id={`${hintId}-box`}
          rows={rows}
          value={value}
          placeholder={placeholder}
          aria-describedby={hintId}
          readOnly={sending}
          onKeyDown={keyDown}
          className="field-sizing-content block max-h-60 min-h-8 w-full resize-none bg-transparent p-1 text-base leading-6 text-foreground outline-none placeholder:text-muted-foreground"
        />
        <div className="flex items-center justify-between gap-2 max-sm:contents">
          <span id={hintId} className="text-xs text-muted-foreground max-sm:sr-only">
            {t("chat.composer.hint")}
            <GhostHint ghost={ghost} />
          </span>
          {status || completion.notice ? (
            <p
              role="status"
              className="inline-flex items-center gap-2 text-sm text-muted-foreground max-sm:order-first max-sm:col-span-2"
            >
              {status || completion.notice}
            </p>
          ) : null}
          <div className="ml-auto flex items-center gap-2">
            {/* First, so the buttons after it keep their place while the accept button or the note comes and goes. */}
            <GhostToolbar ghost={ghost} />
            <CompletionTools completion={completion} />
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
