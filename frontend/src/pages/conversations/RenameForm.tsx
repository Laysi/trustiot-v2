import { type FormEvent, useEffect, useRef, useState } from "react";

import { Button } from "../../components/ui/button.tsx";
import { FormField } from "../../components/ui/form-field.tsx";
import { Input } from "../../components/ui/input.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useConversationChange } from "./queries.ts";

/** The service's longest title, in characters after trimming. */
const MAX_TITLE = 200;

/**
 * Renaming a conversation in place, where its title is (a row of the history, the head of the
 * conversation): the field starts with the current title, selected; Enter or "Save" sends it,
 * Escape or "Cancel" leaves it. The server's refusal (an empty title, a control character) is
 * shown under the field, which keeps what was typed. `onDone` gets the new title, or nothing
 * when renaming was cancelled.
 */
export function RenameForm({
  conversationId,
  current,
  onDone,
  className,
}: {
  conversationId: string;
  /** The title the conversation has; empty while it has none. */
  current: string;
  onDone(renamed?: string): void;
  className?: string;
}) {
  const { t, fieldError, errorMessage } = useI18n();
  const change = useConversationChange();
  const [title, setTitle] = useState(current);
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => {
    input.current?.focus();
    input.current?.select();
  }, []);

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (change.isPending) return;
    const next = title.trim();
    if (next === current.trim()) {
      onDone();
      return;
    }
    change.mutate({ kind: "rename", conversationId, title: next }, { onSuccess: () => onDone(next) });
  };

  // A refused title is said at the field; any other failure (a lost connection) too, with why.
  const error = change.isError
    ? (fieldError(change.error, "/title") ?? t("conversations.rename.failed", { reason: errorMessage(change.error) }))
    : undefined;

  return (
    <form
      onSubmit={submit}
      onKeyDown={(event) => {
        if (event.key === "Escape" && !change.isPending) {
          event.stopPropagation();
          onDone();
        }
      }}
      className={cn("grid gap-2", className)}
    >
      <FormField label={t("conversations.rename.label")} error={error}>
        {(control) => (
          <Input
            {...control}
            ref={input}
            value={title}
            maxLength={MAX_TITLE * 2}
            disabled={change.isPending}
            onChange={(event) => setTitle(event.target.value)}
          />
        )}
      </FormField>
      <div className="flex flex-wrap gap-2">
        <Button type="submit" size="sm" disabled={change.isPending || title.trim() === ""}>
          {change.isPending ? <Spinner /> : null}
          {change.isPending ? t("conversations.rename.saving") : t("conversations.rename.save")}
        </Button>
        <Button type="button" variant="ghost" size="sm" disabled={change.isPending} onClick={() => onDone()}>
          {t("conversations.rename.cancel")}
        </Button>
      </div>
    </form>
  );
}
