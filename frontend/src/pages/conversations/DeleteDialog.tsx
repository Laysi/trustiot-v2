import type { ReactNode, RefObject } from "react";

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
import { useConversationChange } from "./queries.ts";

/**
 * Deleting a conversation, confirmed first as a destructive action
 * (docs/design/tenant-admin/design.md 「共通的互動規則」): the focus starts on Cancel, the button
 * names the action, a refusal (a question still being answered) is shown in the dialog, which
 * stays open, and while the request runs the dialog cannot be closed. The dialog says that the
 * conversation can be restored for 30 days.
 *
 * It opens from `trigger`, or, without one, when the page sets `open` (from a menu item);
 * `returnFocus` is then where the focus goes back when it closes without deleting.
 */
export function DeleteDialog(props: {
  conversationId: string;
  title: string;
  open: boolean;
  onOpenChange(open: boolean): void;
  onDeleted(): void;
  trigger?: ReactNode;
  returnFocus?: RefObject<HTMLElement | null>;
}) {
  const { t, errorMessage } = useI18n();
  const change = useConversationChange();
  return (
    <AlertDialog
      open={props.open}
      onOpenChange={(next) => {
        if (change.isPending) return;
        if (next) change.reset();
        props.onOpenChange(next);
      }}
    >
      {props.trigger ? <AlertDialogTrigger asChild>{props.trigger}</AlertDialogTrigger> : null}
      <AlertDialogContent
        onCloseAutoFocus={(event) => {
          if (props.trigger || !props.returnFocus?.current) return;
          event.preventDefault();
          props.returnFocus.current.focus();
        }}
      >
        <AlertDialogHeader>
          <AlertDialogTitle>{t("conversations.delete.title", { title: props.title })}</AlertDialogTitle>
          <AlertDialogDescription>{t("conversations.delete.body")}</AlertDialogDescription>
        </AlertDialogHeader>
        {change.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(change.error)}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={change.isPending}>{t("conversations.delete.cancel")}</AlertDialogCancel>
          <Button
            variant="destructive"
            disabled={change.isPending}
            onClick={() =>
              change.mutate(
                { kind: "delete", conversationId: props.conversationId },
                {
                  onSuccess: () => {
                    props.onOpenChange(false);
                    props.onDeleted();
                  },
                },
              )
            }
          >
            {change.isPending ? <Spinner /> : null}
            {change.isPending ? t("conversations.delete.pending") : t("conversations.delete.confirm")}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
