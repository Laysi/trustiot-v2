import { AtSignIcon, LayoutTemplateIcon } from "lucide-react";
import { type ComponentProps, type ReactNode, type Ref, useMemo } from "react";

import { Button } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { CompletionPopover } from "./CompletionPopover.tsx";
import { findSlots } from "./text.ts";
import type { Completion } from "./useCompletion.ts";

type Piece = { text: string; kind: "plain" | "mention" | "slot" | "slot-selected" | "query" };

/** The text cut into the stretches the mirror draws differently: mentions, blanks and the word being looked up. */
function pieces(value: string, mirror: Completion["mirror"], blanks: { start: number; end: number }[]): Piece[] {
  const marks: { start: number; end: number; kind: Piece["kind"] }[] = [
    ...mirror.mentions.map((mention) => ({ start: mention.start, end: mention.end, kind: "mention" as const })),
    ...blanks.map((blank) => ({
      start: blank.start,
      end: blank.end,
      kind:
        mirror.slotSelection && mirror.slotSelection[0] === blank.start && mirror.slotSelection[1] === blank.end
          ? ("slot-selected" as const)
          : ("slot" as const),
    })),
    ...(mirror.query ? [{ start: mirror.query[0], end: mirror.query[1], kind: "query" as const }] : []),
  ].sort((a, b) => a.start - b.start);
  const result: Piece[] = [];
  let at = 0;
  for (const mark of marks) {
    // Marks never overlap in a well-formed box; one that does is left out rather than drawn twice.
    if (mark.start < at || mark.end > value.length) continue;
    if (mark.start > at) result.push({ text: value.slice(at, mark.start), kind: "plain" });
    result.push({ text: value.slice(mark.start, mark.end), kind: mark.kind });
    at = mark.end;
  }
  if (at < value.length) result.push({ text: value.slice(at), kind: "plain" });
  return result;
}

// None of these change the width of a character: the box's own text is transparent and its caret
// is drawn at the text's real width, so a padding or a bold face here would put the caret off the
// letters (design 「提及與灰字的畫法」). The backgrounds are shadows and outlines, which take no room.
const PIECE_CLASS: Record<Piece["kind"], string | undefined> = {
  plain: undefined,
  mention: "rounded-[3px] bg-brand-soft text-primary shadow-[0_0_0_2px_var(--brand-soft)] [box-decoration-break:clone]",
  query: "underline decoration-dotted underline-offset-4",
  slot: "rounded-sm bg-accent text-muted-foreground outline-1 outline-dashed outline-input",
  "slot-selected": "rounded-sm bg-primary/20 text-foreground outline-1 outline-solid outline-primary",
};

/** One handler that calls `own`, then `theirs` when there is one. */
function both<E>(own: (event: E) => void, theirs: ((event: E) => void) | undefined): (event: E) => void {
  if (theirs === undefined) return own;
  return (event) => {
    own(event);
    theirs(event);
  };
}

/**
 * The composer's textarea with the completion around it: a mirror layer behind the text that gives
 * mentions, blanks and the word being looked up their look, the list under the box, a live
 * region for the result count, and the sentence that tells a screen reader what `@`, `/`, Tab and
 * Escape do (design decisions 7 to 9). The textarea stays a plain textarea, so editing, the input
 * method and the phone's keyboard are the browser's; its own text is transparent and the mirror
 * draws the same text, with the same font and wrapping, in the same grid cell.
 *
 * `tail` is drawn after the text inside the mirror: the place for text that is not in the box yet
 * (the grey continuation of Laysi/trustiot-v2#156).
 */
export function CompletionField({
  completion,
  textareaRef,
  className,
  tail,
  "aria-describedby": describedBy,
  ...props
}: Omit<ComponentProps<"textarea">, "ref" | "value"> & {
  completion: Completion;
  textareaRef?: Ref<HTMLTextAreaElement>;
  value: string;
  tail?: ReactNode;
}) {
  const { t } = useI18n();
  const { value } = props;
  const blanks = useMemo(() => findSlots(value), [value]);
  const cut = pieces(value, completion.mirror, blanks);
  const attach = (node: HTMLTextAreaElement | null) => {
    completion.boxRef.current = node;
    if (typeof textareaRef === "function") textareaRef(node);
    else if (textareaRef) textareaRef.current = node;
  };
  const showLive = completion.popover !== null || completion.liveText !== null;
  return (
    <>
      <div className="grid">
        <div
          ref={completion.mirrorRef}
          aria-hidden="true"
          className="pointer-events-none col-start-1 row-start-1 block max-h-60 min-h-8 overflow-hidden p-1 text-base leading-6 [overflow-wrap:anywhere] [scrollbar-gutter:stable] whitespace-pre-wrap text-foreground forced-colors:hidden"
        >
          {cut.map((piece, index) => (
            // The pieces are positions in the text, in order, and never reorder.
            // biome-ignore lint/suspicious/noArrayIndexKey: see above
            <span key={index} className={PIECE_CLASS[piece.kind]}>
              {piece.text}
            </span>
          ))}
          {tail}
          {"​"}
        </div>
        <textarea
          {...completion.textareaProps}
          {...props}
          // The caller's handlers run after the completion's own instead of replacing them (the
          // grey text of Laysi/trustiot-v2#156 listens to the same clicks, blurs and scrolls). These
          // five are every handler in `Completion["textareaProps"]`: it has no `onSelect` and no
          // composition handlers, so the caller's own pass through the spread above unchanged.
          // A handler added there must be joined here too.
          onChange={both(completion.textareaProps.onChange, props.onChange)}
          onClick={both(completion.textareaProps.onClick, props.onClick)}
          onKeyUp={both(completion.textareaProps.onKeyUp, props.onKeyUp)}
          onBlur={both(completion.textareaProps.onBlur, props.onBlur)}
          onScroll={both(completion.textareaProps.onScroll, props.onScroll)}
          ref={attach}
          aria-describedby={[describedBy, completion.helpId].filter(Boolean).join(" ")}
          className={cn(
            className,
            "col-start-1 row-start-1 [overflow-wrap:anywhere] [scrollbar-gutter:stable] whitespace-pre-wrap",
            // The mirror draws the letters; the box keeps its caret and its selection (translucent, so the mirror shows).
            "text-transparent caret-foreground selection:bg-primary/30 selection:text-transparent forced-colors:text-[CanvasText]",
          )}
        />
      </div>
      <p id={completion.helpId} className="sr-only">
        {t("chat.completion.help")}
      </p>
      {showLive ? (
        <p role="status" className="sr-only">
          {completion.liveText}
        </p>
      ) : null}
      {completion.popover ? (
        <CompletionPopover model={completion.popover} loadingLabel={t("chat.mention.loading")} />
      ) : null}
    </>
  );
}

/** The two buttons of the box's toolbar: mention (`@`) and, where the build has them, the templates (`/`). */
export function CompletionTools({ completion }: { completion: Completion }) {
  const { t } = useI18n();
  return (
    <>
      <Button
        type="button"
        variant="ghost"
        size="icon"
        className="size-8"
        aria-label={t("chat.mention.at_button")}
        onClick={completion.openMentions}
      >
        <AtSignIcon />
      </Button>
      {completion.templatesOffered ? (
        <Button
          type="button"
          variant="ghost"
          size="sm"
          aria-label={t("chat.template.button_label")}
          onClick={completion.openTemplates}
        >
          <LayoutTemplateIcon />
          <span className="max-sm:hidden">{t("chat.template.button")}</span>
        </Button>
      ) : null}
    </>
  );
}
