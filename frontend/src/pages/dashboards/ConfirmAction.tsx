import type { UseMutationResult } from "@tanstack/react-query";
import { type ReactNode, useState } from "react";

import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "../../components/ui/alert-dialog.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";

/**
 * A write confirmed in a dialog first, as docs/design/tenant-admin/design.md 「共通的互動規則」
 * sets it: the focus starts on Cancel, the button names the action, and a refusal is shown in the
 * dialog, which stays open; while the request runs the dialog cannot be closed.
 */
export function ConfirmAction(props: {
  trigger: ReactNode;
  title: string;
  body: string;
  confirm: string;
  pending: string;
  mutation: UseMutationResult<unknown, unknown, void>;
}) {
  const { t, errorMessage } = useI18n();
  const { mutation } = props;
  const [open, setOpen] = useState(false);
  return (
    <AlertDialog
      open={open}
      onOpenChange={(next) => {
        if (mutation.isPending) return;
        if (next) mutation.reset();
        setOpen(next);
      }}
    >
      <AlertDialogTrigger asChild>{props.trigger}</AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{props.title}</AlertDialogTitle>
          <AlertDialogDescription>{props.body}</AlertDialogDescription>
        </AlertDialogHeader>
        {mutation.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(mutation.error)}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={mutation.isPending}>{t("dashboard_page.cancel")}</AlertDialogCancel>
          <Button
            disabled={mutation.isPending}
            onClick={() => mutation.mutate(undefined, { onSuccess: () => setOpen(false) })}
          >
            {mutation.isPending ? <Spinner /> : null}
            {mutation.isPending ? props.pending : props.confirm}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
