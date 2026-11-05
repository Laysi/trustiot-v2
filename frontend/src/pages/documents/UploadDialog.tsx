import { UploadIcon } from "lucide-react";
import { type DragEvent, useId, useRef, useState } from "react";

import type { DocumentSummary, DocumentUsage } from "../../api/pending/documents.ts";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "../../components/ui/alert-dialog.tsx";
import { Button } from "../../components/ui/button.tsx";
import { FormField } from "../../components/ui/form-field.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { documentName, formatBytes } from "./format.ts";
import { UploadRow } from "./UploadRow.tsx";
import type { UploadItem, UploadQueue } from "./useUploadQueue.ts";

export interface ModelOption {
  id: string;
  name: string;
}

type Choice = "new_version" | "new_document";

/**
 * The upload dialog (docs/design/ms4-ms5/design.md 「上傳」): the model first (required: the AI
 * reads these documents only for devices of that model), then PDFs dropped or picked, several at
 * once. Each file is a row; a file with the name of a document of the same model asks whether it
 * is a new version (the default) or a new document. `target` makes it the dialog of "upload a new
 * version" of one document: its model is fixed and every file is a version of it.
 *
 * The files are in the page's queue, so "Continue in the background" closes the dialog and the
 * uploads go on.
 */
export function UploadDialog({
  open,
  onOpenChange,
  queue,
  models,
  modelsError,
  onRetryModels,
  documents,
  usage,
  target,
}: {
  open: boolean;
  onOpenChange(open: boolean): void;
  queue: UploadQueue;
  models: readonly ModelOption[];
  /** Why the model list could not be read, if it could not: the dialog says so instead of "no models". */
  modelsError: unknown;
  onRetryModels(): void;
  documents: readonly DocumentSummary[];
  usage: DocumentUsage;
  target: DocumentSummary | null;
}) {
  const { t, locale, errorMessage } = useI18n();
  const inputId = useId();
  const hintId = useId();
  const input = useRef<HTMLInputElement>(null);
  const [modelId, setModelId] = useState("");
  const [choices, setChoices] = useState<ReadonlyMap<number, Choice>>(new Map());
  const [over, setOver] = useState(false);
  const [onlyOne, setOnlyOne] = useState(false);

  const effectiveModel = target?.model_id ?? modelId;
  const remaining = Math.max(0, usage.limit_bytes - usage.used_bytes);
  const staged = queue.items.filter((item) => item.state === "staged");
  const uploading = queue.items.some((item) => item.state === "uploading");
  const pickedModel = models.find((model) => model.id === effectiveModel);

  const conflictOf = (item: UploadItem): DocumentSummary | undefined =>
    target !== null
      ? undefined
      : documents.find(
          (document) =>
            document.model_id === effectiveModel &&
            document.name.toLowerCase() === documentName(item.file).toLowerCase(),
        );

  const addFiles = (files: FileList | File[] | null) => {
    const picked = Array.from(files ?? []);
    if (picked.length === 0) return;
    if (target === null) {
      queue.stage(picked, remaining);
      return;
    }
    // A new version is one file: a later pick replaces a file that has not started, and nothing is
    // taken while one uploads; the dialog says so when files were left out.
    if (uploading) {
      setOnlyOne(true);
      return;
    }
    setOnlyOne(picked.length > 1);
    queue.clear(["staged", "rejected", "done", "failed"]);
    queue.stage(picked.slice(0, 1), remaining);
  };

  const onDrop = (event: DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    setOver(false);
    addFiles(event.dataTransfer.files);
  };

  const startAll = () => {
    for (const item of staged) {
      const conflict = conflictOf(item);
      const asVersion = conflict !== undefined && (choices.get(item.id) ?? "new_version") === "new_version";
      const documentId = target?.id ?? (asVersion ? conflict.id : undefined);
      queue.start(item.id, { modelId: effectiveModel, ...(documentId === undefined ? {} : { documentId }) });
    }
  };

  const canStart = staged.length > 0 && effectiveModel !== "";
  const title =
    target === null ? t("documents.upload.title") : t("documents.upload.title_version", { name: target.name });

  return (
    <AlertDialog open={open} onOpenChange={onOpenChange}>
      {/* A form dialog, not a confirmation: Radix's alert dialog gives the focus handling and Cancel
          ("continue in the background"), and the role says what it is for. */}
      <AlertDialogContent role="dialog" className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-xl">
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription>{t("documents.upload.description")}</AlertDialogDescription>
        </AlertDialogHeader>

        {modelsError !== null && target === null ? (
          <div
            role="alert"
            className="flex flex-wrap items-center gap-3 rounded-md border border-destructive/50 px-3 py-2 text-sm text-destructive"
          >
            <p>{t("documents.upload.models_failed", { reason: errorMessage(modelsError) })}</p>
            <Button variant="outline" size="sm" onClick={onRetryModels}>
              {t("page.action.retry")}
            </Button>
          </div>
        ) : models.length === 0 && target === null ? (
          <p role="status" className="rounded-md border px-3 py-2 text-sm">
            {t("documents.upload.no_models")}
          </p>
        ) : target === null ? (
          <FormField label={t("documents.upload.model")} description={t("documents.upload.model_hint")}>
            {(control) => (
              <NativeSelect {...control} value={modelId} onChange={(event) => setModelId(event.target.value)}>
                <option value="">{t("documents.upload.model_placeholder")}</option>
                {models.map((model) => (
                  <option key={model.id} value={model.id}>
                    {model.name}
                  </option>
                ))}
              </NativeSelect>
            )}
          </FormField>
        ) : (
          <p className="text-sm">
            {t("documents.upload.model_fixed", { model: pickedModel?.name ?? t("documents.model_unknown") })}
          </p>
        )}

        <fieldset
          onDragOver={(event) => {
            event.preventDefault();
            setOver(true);
          }}
          onDragLeave={() => setOver(false)}
          onDrop={onDrop}
          className={cn(
            "grid justify-items-center gap-1.5 rounded-lg border-2 border-dashed px-4 py-6 text-center",
            over && "border-primary bg-brand-soft",
          )}
        >
          <legend className="sr-only">{t("documents.upload.drop_label")}</legend>
          <UploadIcon aria-hidden="true" className="size-6 text-muted-foreground" />
          <p className="font-medium">{over ? t("documents.upload.drop_over") : t("documents.upload.drop")}</p>
          <p className="text-sm text-muted-foreground">
            <Button variant="link" size="sm" className="h-auto" onClick={() => input.current?.click()}>
              {t("documents.upload.choose")}
            </Button>{" "}
            {t("documents.upload.pdf_only", { remaining: formatBytes(remaining, locale) })}
          </p>
          <label htmlFor={inputId} className="sr-only">
            {t("documents.upload.input_label")}
          </label>
          <input
            ref={input}
            id={inputId}
            type="file"
            accept="application/pdf,.pdf"
            multiple={target === null}
            className="sr-only"
            tabIndex={-1}
            onChange={(event) => {
              addFiles(event.target.files);
              // The same file can be picked again after it was removed.
              event.target.value = "";
            }}
          />
        </fieldset>

        {onlyOne ? (
          <p role="status" className="text-sm">
            {t("documents.upload.one_file")}
          </p>
        ) : null}

        {queue.items.length > 0 ? (
          <ul aria-label={t("documents.upload.files_label")} aria-live="polite" className="grid gap-2">
            {queue.items.map((item) => {
              const conflict = item.state === "staged" ? conflictOf(item) : undefined;
              return (
                <UploadRow key={item.id} item={item} onRemove={queue.remove} onRetry={queue.retry}>
                  {conflict !== undefined ? (
                    <ChoiceFieldset
                      name={`choice-${item.id}`}
                      documentName={conflict.name}
                      nextVersion={conflict.latest_version.version + 1}
                      value={choices.get(item.id) ?? "new_version"}
                      onChange={(choice) => setChoices((current) => new Map(current).set(item.id, choice))}
                    />
                  ) : null}
                </UploadRow>
              );
            })}
          </ul>
        ) : null}

        {!canStart && staged.length > 0 ? (
          <p id={hintId} className="text-sm text-muted-foreground">
            {t("documents.upload.choose_model_first")}
          </p>
        ) : null}

        <AlertDialogFooter>
          <AlertDialogCancel>
            {uploading
              ? t("documents.upload.background")
              : t(staged.length > 0 ? "documents.cancel" : "documents.upload.close")}
          </AlertDialogCancel>
          {staged.length > 0 ? (
            <Button disabled={!canStart} aria-describedby={canStart ? undefined : hintId} onClick={startAll}>
              <UploadIcon aria-hidden="true" />
              {t("documents.upload.start")}
            </Button>
          ) : null}
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

function ChoiceFieldset({
  name,
  documentName,
  nextVersion,
  value,
  onChange,
}: {
  name: string;
  documentName: string;
  nextVersion: number;
  value: Choice;
  onChange(choice: Choice): void;
}) {
  const { t } = useI18n();
  return (
    <fieldset className="grid gap-1 text-xs">
      <legend className="font-medium">{t("documents.upload.exists", { name: documentName })}</legend>
      <label className="flex items-center gap-2">
        <input type="radio" name={name} checked={value === "new_version"} onChange={() => onChange("new_version")} />
        {t("documents.upload.as_version", { version: nextVersion })}
      </label>
      <label className="flex items-center gap-2">
        <input type="radio" name={name} checked={value === "new_document"} onChange={() => onChange("new_document")} />
        {t("documents.upload.as_document")}
      </label>
    </fieldset>
  );
}
