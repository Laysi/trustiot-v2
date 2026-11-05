import { type ReactNode, type RefObject, useMemo } from "react";

import { cn } from "../../../lib/utils.ts";
import { findVariables } from "./diff.ts";

/** The text of the field with each `{{name}}` marked: the service's variables softly, the others as errors. */
function Highlighted({ text, allowed }: { text: string; allowed: readonly string[] }) {
  const pieces = useMemo(() => {
    const result: ReactNode[] = [];
    let at = 0;
    for (const variable of findVariables(text)) {
      if (variable.start > at) result.push(text.slice(at, variable.start));
      const known = allowed.includes(variable.name);
      result.push(
        <mark
          key={variable.start}
          className={cn(
            "rounded-sm text-transparent",
            known ? "bg-brand-soft" : "bg-destructive/15 underline decoration-destructive decoration-wavy",
          )}
        >
          {variable.raw}
        </mark>,
      );
      at = variable.end;
    }
    result.push(text.slice(at));
    return result;
  }, [text, allowed]);
  return <>{pieces}</>;
}

const SHARED =
  "col-start-1 row-start-1 min-w-0 px-3 py-2 font-mono text-sm leading-relaxed whitespace-pre-wrap wrap-anywhere";

/**
 * The editable part of a prompt: a plain `<textarea>` over a layer that draws the variables, so
 * the text is typed and read as in any text box (no `contenteditable`). The layer holds the same
 * text in the same font and wraps the same way, with its own text transparent: only the marks
 * behind the variables show. Both sit in one grid cell, so the field grows with its content.
 */
export function PromptField({
  id,
  value,
  onChange,
  allowed,
  invalid,
  describedBy,
  textareaRef,
}: {
  id: string;
  value: string;
  onChange: (value: string) => void;
  allowed: readonly string[];
  invalid: boolean;
  describedBy: string | undefined;
  textareaRef: RefObject<HTMLTextAreaElement | null>;
}) {
  return (
    <div className="grid min-h-40 rounded-md border border-input bg-background focus-within:outline-2 focus-within:outline-offset-2 focus-within:outline-ring has-[[aria-invalid=true]]:border-destructive">
      <div aria-hidden="true" className={cn(SHARED, "pointer-events-none text-transparent")}>
        <Highlighted text={value} allowed={allowed} />
        {"\n"}
      </div>
      <textarea
        ref={textareaRef}
        id={id}
        value={value}
        spellCheck={false}
        onChange={(event) => onChange(event.target.value)}
        {...(invalid ? { "aria-invalid": true } : {})}
        {...(describedBy ? { "aria-describedby": describedBy } : {})}
        className={cn(SHARED, "resize-none overflow-hidden bg-transparent outline-none")}
      />
    </div>
  );
}
