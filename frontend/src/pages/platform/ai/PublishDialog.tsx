import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import { ApiError } from "../../../api/errors.ts";
import { usePromptApi } from "../../../api/pending/PromptApiProvider.tsx";
import type { PromptDetail } from "../../../api/pending/prompts.ts";
import { Alert, AlertDescription, AlertTitle } from "../../../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "../../../components/ui/alert-dialog.tsx";
import { Button } from "../../../components/ui/button.tsx";
import { FormField } from "../../../components/ui/form-field.tsx";
import { Input } from "../../../components/ui/input.tsx";
import { Spinner } from "../../../components/ui/spinner.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { DiffView } from "./DiffView.tsx";
import { AuditNote } from "./ui.tsx";

/** The longest version note (the design's field has a limit of 200). */
export const NOTE_MAX = 200;

/**
 * Publishing a draft as the next version, after the difference to the published one: only new
 * conversations use it. The note is optional (decision Q9). The draft was edited from
 * `baseVersion`; if another version is current by now the service answers 409, the dialog closes
 * and the page says someone published first (`onConflict`). The page shows what the service
 * answered, so the published version is `onPublished`'s argument, not the draft.
 */
export function PublishDialog({
  open,
  onOpenChange,
  service,
  role,
  detail,
  draft,
  baseVersion,
  evaluated,
  onEvaluate,
  onPublished,
  onConflict,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  service: string;
  role: string;
  detail: PromptDetail;
  draft: string;
  baseVersion: number;
  /** A question bank report exists for exactly this draft. */
  evaluated: boolean;
  onEvaluate: () => void;
  onPublished: (detail: PromptDetail) => void;
  onConflict: () => void;
}) {
  const { t, errorMessage } = useI18n();
  const api = usePromptApi();
  const [note, setNote] = useState("");
  const publish = useMutation({
    mutationFn: () =>
      api.publish(service, role, {
        content: draft,
        ...(note.trim() ? { note: note.trim() } : {}),
        expected_version: baseVersion,
      }),
    onSuccess: (published) => {
      onOpenChange(false);
      onPublished(published);
    },
    onError: (error) => {
      if (error instanceof ApiError && error.status === 409) {
        onOpenChange(false);
        onConflict();
      }
    },
  });
  const next = detail.version + 1;

  return (
    <AlertDialog
      open={open}
      onOpenChange={(value) => {
        if (publish.isPending) return;
        if (value) {
          publish.reset();
          setNote("");
        }
        onOpenChange(value);
      }}
    >
      <AlertDialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-2xl">
        <AlertDialogHeader>
          <AlertDialogTitle>{t("platform_ai.publish.title", { role, version: next })}</AlertDialogTitle>
          <AlertDialogDescription>{t("platform_ai.publish.desc", { version: detail.version })}</AlertDialogDescription>
        </AlertDialogHeader>
        <DiffView
          label={t("platform_ai.publish.diff_label", { version: detail.version })}
          before={detail.content}
          after={draft}
          beforeLabel={t("platform_ai.publish.published", { version: detail.version })}
          afterLabel={t("platform_ai.publish.draft")}
        />
        {evaluated ? null : (
          <Alert role="status">
            <AlertTitle>{t("platform_ai.publish.not_evaluated_title")}</AlertTitle>
            <AlertDescription>
              <p>{t("platform_ai.publish.not_evaluated_body")}</p>
            </AlertDescription>
          </Alert>
        )}
        <FormField label={t("platform_ai.publish.note")}>
          {(control) => (
            <Input {...control} value={note} maxLength={NOTE_MAX} onChange={(event) => setNote(event.target.value)} />
          )}
        </FormField>
        <AuditNote>{t("platform_ai.publish.audit", { version: next, previous: detail.version })}</AuditNote>
        {publish.isError && !(publish.error instanceof ApiError && publish.error.status === 409) ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(publish.error)}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={publish.isPending}>{t("platform_ai.publish.back")}</AlertDialogCancel>
          {evaluated ? null : (
            <Button
              variant="outline"
              disabled={publish.isPending}
              onClick={() => {
                onOpenChange(false);
                onEvaluate();
              }}
            >
              {t("platform_ai.publish.evaluate")}
            </Button>
          )}
          <Button disabled={publish.isPending} onClick={() => publish.mutate()}>
            {publish.isPending ? <Spinner /> : null}
            {publish.isPending ? t("platform_ai.publish.pending") : t("platform_ai.publish.submit", { version: next })}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
