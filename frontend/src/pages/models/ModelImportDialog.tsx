import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useId, useRef, useState } from "react";
import { useNavigate } from "react-router";

import { useAssetsApi } from "../../api/AssetsApiProvider.tsx";
import { modelPath } from "../../app/paths.ts";
import { flashState } from "../../components/FlashNotice.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "../../components/ui/alert-dialog.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { FormField } from "../../components/ui/form-field.tsx";
import { Input } from "../../components/ui/input.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import type { MessageId } from "../../i18n/messages.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { tenantModelsKey } from "../device/queries.ts";
import { type Problem, problemsOf, problemText } from "../form-problems.ts";
import {
  type ImportFile,
  MAX_IMPORT_BYTES,
  MODEL_CONFLICT_FIELDS,
  parseImportFile,
  pointIndexOf,
  validateName,
} from "./modelForm.ts";

const INVALID_FILE: Readonly<Record<"not_json" | "format" | "version" | "shape", MessageId>> = {
  not_json: "model_import.not_json",
  format: "device_model.format_unsupported",
  version: "device_model.format_version_unsupported",
  shape: "model_import.invalid_shape",
};

interface Chosen {
  filename: string;
  size: number;
  file: ImportFile;
}

function kilobytes(bytes: number, locale: string): string {
  return `${new Intl.NumberFormat(locale, { maximumFractionDigits: 1 }).format(bytes / 1024)} KB`;
}

/**
 * Imports a model from a file an export made (`devices--model-import`): the file is read and
 * checked in the browser first (JSON, the document's format and version, the shape of its data
 * points), then shown as a preview with the name it will get, which can be changed when the name is
 * taken. What the service refuses is listed as it says it, one line per problem. An import creates
 * a new model and never replaces one.
 */
export function ModelImportDialog({ open, onClose }: { open: boolean; onClose(): void }) {
  const { t, locale, errorMessage } = useI18n();
  const api = useAssetsApi();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const fileId = useId();
  const picker = useRef<HTMLInputElement>(null);
  const [chosen, setChosen] = useState<Chosen | null>(null);
  const [invalid, setInvalid] = useState<MessageId | null>(null);
  const [name, setName] = useState("");
  const [problems, setProblems] = useState<Problem[]>([]);
  const [rest, setRest] = useState<unknown>(null);

  const reset = () => {
    setChosen(null);
    setInvalid(null);
    setName("");
    setProblems([]);
    setRest(null);
  };

  const submit = useMutation({
    mutationFn: (input: { file: ImportFile; name: string }) =>
      api.importDeviceModel({ ...input.file, name: input.name }),
    onSuccess: async (model) => {
      await queryClient.invalidateQueries({ queryKey: tenantModelsKey(tenant) });
      reset();
      // Closing takes `?import` off the list's address; the navigation after it is the one that stays.
      onClose();
      void navigate(modelPath(model.model_id), { state: flashState("models.flash.imported", { name: model.name }) });
    },
    onError: (error) => {
      const refused = problemsOf(error, MODEL_CONFLICT_FIELDS);
      setProblems(refused.problems);
      setRest(refused.rest);
    },
  });

  const choose = async (file: File | undefined) => {
    reset();
    if (file === undefined) return;
    if (file.size > MAX_IMPORT_BYTES) {
      setInvalid("model_import.too_large");
      return;
    }
    const parsed = parseImportFile(await file.text());
    if (!parsed.ok) {
      setInvalid(INVALID_FILE[parsed.reason]);
      return;
    }
    setChosen({ filename: file.name, size: file.size, file: parsed.file });
    setName(parsed.file.name);
  };

  const send = () => {
    if (chosen === null) return;
    setRest(null);
    const found = validateName(name);
    setProblems(found);
    if (found.length === 0) submit.mutate({ file: chosen.file, name: name.trim() });
  };

  const nameProblem = problems.find((problem) => problem.pointer === "/name");
  // Everything else the service said, one line each, in its own words.
  const listed = problems.filter((problem) => problem.pointer !== "/name");
  const where = (problem: Problem): string => {
    const at = pointIndexOf(problem.pointer);
    return at === null ? t("model_import.whole_file") : t("model_import.data_point", { n: at + 1 });
  };

  return (
    <AlertDialog
      open={open}
      onOpenChange={(next) => {
        if (next || submit.isPending) return;
        reset();
        submit.reset();
        onClose();
      }}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{t("model_import.title")}</AlertDialogTitle>
          <AlertDialogDescription>{t("model_import.body")}</AlertDialogDescription>
        </AlertDialogHeader>

        <div className="grid gap-2">
          <label htmlFor={fileId} className="text-sm font-medium">
            {t("model_import.file")}
          </label>
          <input
            id={fileId}
            ref={picker}
            type="file"
            accept=".json,application/json"
            disabled={submit.isPending}
            className={cn(
              "w-full min-w-0 rounded-md border border-input bg-background px-3 py-1.5 text-sm file:mr-3 file:rounded-sm file:border-0 file:bg-secondary file:px-2 file:py-1 file:text-sm file:font-medium",
              focusRing,
            )}
            onChange={(event) => void choose(event.target.files?.[0])}
          />
          {invalid !== null ? (
            <p role="alert" className="text-sm font-medium text-destructive">
              {t(invalid)}
            </p>
          ) : null}
        </div>

        {chosen !== null ? (
          <div className="grid gap-4">
            <p className="text-sm text-muted-foreground">
              {t("model_import.chosen", { file: chosen.filename, size: kilobytes(chosen.size, locale) })}
            </p>
            <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
              <dt className="text-muted-foreground">{t("model_import.preview.name")}</dt>
              <dd className="font-mono wrap-anywhere">{chosen.file.name}</dd>
              <dt className="text-muted-foreground">{t("model_import.preview.points")}</dt>
              <dd>{t("model_import.preview.points_value", { count: chosen.file.points.length })}</dd>
              <dt className="text-muted-foreground">{t("model_import.preview.version")}</dt>
              <dd>{chosen.file.format_version}</dd>
            </dl>
            <FormField
              label={t("model_import.new_name")}
              description={t("model_edit.name_hint")}
              error={nameProblem ? problemText(locale, nameProblem) : undefined}
            >
              {(control) => (
                <Input
                  {...control}
                  value={name}
                  autoComplete="off"
                  spellCheck={false}
                  onChange={(event) => {
                    setName(event.target.value);
                    setProblems((current) => current.filter((problem) => problem.pointer !== "/name"));
                  }}
                />
              )}
            </FormField>
            {listed.length > 0 ? (
              <Alert variant="destructive" className="grid-cols-[1fr]">
                <AlertDescription className="col-start-1">
                  <p className="font-medium">{t("model_import.refused", { count: listed.length })}</p>
                  <ul className="grid gap-0.5">
                    {listed.map((problem) => (
                      <li key={`${problem.pointer}|${problem.message_key}`}>
                        {t("model_import.refused_line", {
                          where: where(problem),
                          message: problemText(locale, problem),
                        })}
                      </li>
                    ))}
                  </ul>
                </AlertDescription>
              </Alert>
            ) : null}
            {rest !== null ? (
              <Alert variant="destructive" className="grid-cols-[1fr]">
                <AlertDescription className="col-start-1">
                  <p>{errorMessage(rest)}</p>
                </AlertDescription>
              </Alert>
            ) : null}
          </div>
        ) : null}

        <AlertDialogFooter>
          <AlertDialogCancel disabled={submit.isPending}>{t("device_form.cancel")}</AlertDialogCancel>
          <Button disabled={chosen === null || submit.isPending} onClick={send}>
            {submit.isPending ? <Spinner /> : null}
            {submit.isPending ? t("model_import.importing") : t("model_import.submit")}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
