import { PauseIcon } from "lucide-react";

import { Button } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import type { GhostCompletion } from "./useGhostCompletion.ts";

/**
 * The grey text, drawn after the box's text inside the composer's one mirror layer
 * (`CompletionField`'s `tail`; docs/design/home-ai/design.md, 「提及與灰字的畫法」), so the caret,
 * the line breaks and the input method stay the textarea's own. The mirror is `aria-hidden`, so
 * the grey text is not read out (design decision 9). It is drawn only while the suggestion
 * continues the text as it is now: in the one render between an edit and the hook clearing the
 * suggestion, it would otherwise sit after the wrong letters.
 */
export function GhostTail({ ghost, value }: { ghost: GhostCompletion; value: string }) {
  const { suggestion } = ghost;
  if (!ghost.active || suggestion === null || suggestion.base !== value) return null;
  return (
    <>
      <span data-ghost="" className="text-muted-foreground">
        {suggestion.text}
      </span>
      <kbd className="ml-1.5 inline-block rounded border border-input px-1 align-middle font-mono text-[11px] leading-4 text-muted-foreground max-sm:hidden">
        Tab
      </kbd>
    </>
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
