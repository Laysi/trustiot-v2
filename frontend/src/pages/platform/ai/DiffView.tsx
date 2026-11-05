import { type ReactNode, useMemo } from "react";

import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { countChanges, type DiffLine, diffLines, type Segment, splitRows, wordSegments } from "./diff.ts";

/** Each changed line's pieces, where a deleted line and the added one replacing it can be compared word by word. */
function lineSegments(lines: readonly DiffLine[]): Map<DiffLine, Segment[]> {
  const result = new Map<DiffLine, Segment[]>();
  let index = 0;
  while (index < lines.length) {
    if ((lines[index] as DiffLine).kind === "context") {
      index += 1;
      continue;
    }
    const deleted: DiffLine[] = [];
    const added: DiffLine[] = [];
    while (index < lines.length && (lines[index] as DiffLine).kind === "del") deleted.push(lines[index++] as DiffLine);
    while (index < lines.length && (lines[index] as DiffLine).kind === "add") added.push(lines[index++] as DiffLine);
    for (let pair = 0; pair < Math.min(deleted.length, added.length); pair += 1) {
      const words = wordSegments((deleted[pair] as DiffLine).text, (added[pair] as DiffLine).text);
      result.set(deleted[pair] as DiffLine, words.old);
      result.set(added[pair] as DiffLine, words.new);
    }
  }
  return result;
}

function Text({ line, segments }: { line: DiffLine; segments: Segment[] | undefined }) {
  if (line.text === "") return <span> </span>;
  if (segments === undefined) return <span>{line.text}</span>;
  return (
    <span>
      {segments.map((segment, index) =>
        segment.changed ? (
          line.kind === "add" ? (
            // biome-ignore lint/suspicious/noArrayIndexKey: segments never reorder
            <ins key={index} className="bg-success/30 no-underline">
              {segment.text}
            </ins>
          ) : (
            // biome-ignore lint/suspicious/noArrayIndexKey: segments never reorder
            <del key={index} className="bg-destructive/25">
              {segment.text}
            </del>
          )
        ) : (
          // biome-ignore lint/suspicious/noArrayIndexKey: segments never reorder
          <span key={index}>{segment.text}</span>
        ),
      )}
    </span>
  );
}

const ROW =
  "grid grid-cols-[2.25rem_1rem_minmax(0,1fr)] gap-x-1 px-2 py-0.5 font-mono text-[0.8125rem] leading-relaxed";

function Row({
  line,
  segments,
  number,
}: {
  line: DiffLine;
  segments: Segment[] | undefined;
  number: number | undefined;
}) {
  const { t } = useI18n();
  return (
    <div className={cn(ROW, line.kind === "add" && "bg-success/15", line.kind === "del" && "bg-destructive/10")}>
      <span aria-hidden="true" className="text-right text-muted-foreground tabular-nums select-none">
        {number ?? ""}
      </span>
      <span aria-hidden="true" className="text-center select-none">
        {line.kind === "add" ? "+" : line.kind === "del" ? "−" : ""}
      </span>
      <span className="min-w-0 whitespace-pre-wrap wrap-anywhere">
        {line.kind !== "context" ? (
          <span className="sr-only">
            {line.kind === "add" ? t("platform_ai.diff.added") : t("platform_ai.diff.deleted")}
          </span>
        ) : null}
        <Text line={line} segments={segments} />
      </span>
    </div>
  );
}

/**
 * Two texts compared line by line, as the design draws it: a plus or minus sign, a background and
 * the words "added" or "deleted" for screen readers on every changed line (never color alone), and
 * the words that changed inside a replaced line marked within it. `merged` is one column; `split`
 * puts the old text beside the new. The texts are printed as text, never as markup.
 */
export function DiffView({
  before,
  after,
  beforeLabel,
  afterLabel,
  mode = "merged",
  label,
}: {
  before: string;
  after: string;
  beforeLabel: ReactNode;
  afterLabel: ReactNode;
  mode?: "merged" | "split";
  label: string;
}) {
  const { t } = useI18n();
  const lines = useMemo(() => diffLines(before, after), [before, after]);
  const segments = useMemo(() => lineSegments(lines), [lines]);
  const { added, deleted } = countChanges(lines);

  return (
    <section aria-label={label} className="overflow-hidden rounded-md border bg-card">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b bg-muted/50 px-3 py-1.5 text-xs text-muted-foreground">
        <span>
          {beforeLabel} → {afterLabel}
        </span>
        <span>{t("platform_ai.diff.counts", { added, deleted })}</span>
      </div>
      {lines.length === 0 ? (
        <p className="px-3 py-3 text-sm text-muted-foreground">{t("platform_ai.diff.empty")}</p>
      ) : mode === "merged" ? (
        <div className="max-h-96 overflow-auto">
          {lines.map((line, index) => (
            <Row
              // biome-ignore lint/suspicious/noArrayIndexKey: the diff is rebuilt as a whole
              key={index}
              line={line}
              segments={segments.get(line)}
              number={line.kind === "del" ? line.oldNo : line.newNo}
            />
          ))}
        </div>
      ) : (
        <div className="grid max-h-96 grid-cols-1 overflow-auto md:grid-cols-2">
          {(["left", "right"] as const).map((side) => (
            <div key={side} className={cn("min-w-0", side === "right" && "border-t md:border-t-0 md:border-l")}>
              <div className="border-b px-3 py-1 text-xs font-medium">{side === "left" ? beforeLabel : afterLabel}</div>
              {splitRows(lines).map((row, index) => {
                const line = row[side];
                return line === null ? (
                  // biome-ignore lint/suspicious/noArrayIndexKey: the diff is rebuilt as a whole
                  <div key={index} aria-hidden="true" className={cn(ROW, "bg-muted/40")}>
                    <span />
                    <span />
                    <span> </span>
                  </div>
                ) : (
                  <Row
                    // biome-ignore lint/suspicious/noArrayIndexKey: the diff is rebuilt as a whole
                    key={index}
                    line={line}
                    segments={segments.get(line)}
                    number={side === "left" ? line.oldNo : line.newNo}
                  />
                );
              })}
            </div>
          ))}
        </div>
      )}
    </section>
  );
}
