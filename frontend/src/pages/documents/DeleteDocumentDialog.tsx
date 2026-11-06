import { TrashIcon } from "lucide-react";

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
import { Button } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";

/**
 * The confirmation before a document is deleted (design.md 「刪除」): every version, the page
 * images and the index go, the AI stops citing it, and it cannot be undone. A refusal is shown
 * here and the dialog stays; while the request runs it cannot be closed.
 */
export function DeleteDocumentDialog({
  target,
  pending,
  error,
  onConfirm,
  onClose,
}: {
  /** The document to delete (its name and the latest version number, as every version up to it goes with it), or `null` while no dialog is open. */
  target: { name: string; latestVersion: number } | null;
  pending: boolean;
  error: unknown;
  onConfirm(): void;
  onClose(): void;
}) {
  const { t, errorMessage } = useI18n();
  return (
    <AlertDialog
      open={target !== null}
      onOpenChange={(open) => {
        if (!open && !pending) onClose();
      }}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{t("documents.delete.title", { name: target?.name ?? "" })}</AlertDialogTitle>
          <AlertDialogDescription asChild>
            <div className="grid gap-2">
              <p>{t("documents.delete.body_versions", { version: target?.latestVersion ?? 0 })}</p>
              <p>{t("documents.delete.body_citations")}</p>
            </div>
          </AlertDialogDescription>
        </AlertDialogHeader>
        {error !== null && error !== undefined ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{t("documents.delete.failed", { reason: errorMessage(error) })}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={pending}>{t("documents.cancel")}</AlertDialogCancel>
          <Button variant="destructive" disabled={pending} onClick={onConfirm}>
            {pending ? <Spinner /> : <TrashIcon aria-hidden="true" />}
            {pending ? t("documents.delete.pending") : t("documents.delete.confirm")}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
