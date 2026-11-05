import { PauseIcon } from "lucide-react";
import type { ReactNode } from "react";

import { Button } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import type { GhostCompletion } from "./useGhostCompletion.ts";

/**
 * Puts the box and the mirror layer in one grid cell (docs/design/home-ai/design.md,
 * 「提及與灰字的畫法」): the mirror repeats the box's text invisibly and draws the grey text after
 * it, so the caret, the line breaks and the input method stay the textarea's own. It is only
 * there while there is a suggestion. Without completion the box is returned as it is.
 */
export function GhostField({ ghost, children }: { ghost: GhostCompletion; children: ReactNode }) {
  if (!ghost.active) return children;
  const { suggestion } = ghost;
  return (
    <div className="grid [&>*]:col-start-1 [&>*]:row-start-1">
      {children}
      {suggestion ? (
        <div
          ref={ghost.mirrorRef}
          aria-hidden="true"
          // Same box model, font and wrapping as the textarea, or the grey text drifts away from the caret.
          className="pointer-events-none block max-h-60 min-h-8 w-full overflow-hidden p-1 text-base leading-6 break-words whitespace-pre-wrap text-muted-foreground forced-colors:hidden"
        >
          <span className="text-transparent">{suggestion.base}</span>
          <span data-ghost="">{suggestion.text}</span>
          <kbd className="ml-1.5 inline-block rounded border border-input px-1 align-middle font-mono text-[11px] leading-4 max-sm:hidden">
            Tab
          </kbd>
        </div>
      ) : null}
    </div>
  );
}

/** What the box's hint adds about the keys (read once through `aria-describedby`, so the grey text is not announced). */
export function GhostHint({ ghost }: { ghost: GhostCompletion }) {
  const { t } = useI18n();
  return ghost.active ? <> {t("chat.composer.completion_hint")}</> : null;
}

/**
 * The toolbar's part: the accept button for touch screens (there is no Tab key), the note that
 * suggestions are paused by the per-user limit, and a polite announcement once a suggestion was
 * accepted. A failed completion shows nothing: the user did not ask for it.
 */
export function GhostToolbar({ ghost }: { ghost: GhostCompletion }) {
  const { t } = useI18n();
  if (!ghost.active) return null;
  const { suggestion, pausedMinutes } = ghost;
  return (
    <>
      <span role="status" className="contents">
        {ghost.accepted ? <span className="sr-only">{t("chat.composer.completion_accepted")}</span> : null}
        {pausedMinutes !== null ? (
          <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
            <PauseIcon className="size-3.5" aria-hidden="true" />
            {t("chat.composer.completion_paused", { minutes: pausedMinutes })}
          </span>
        ) : null}
      </span>
      {suggestion ? (
        <Button
          variant="outline"
          size="sm"
          className="sm:hidden"
          aria-label={t("chat.composer.completion_accept", { text: suggestion.text.trim() })}
          {...ghost.acceptButtonProps}
        >
          {t("chat.composer.completion_accept_short")}
        </Button>
      ) : null}
    </>
  );
}
