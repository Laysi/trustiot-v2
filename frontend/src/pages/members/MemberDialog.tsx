import { XIcon } from "lucide-react";
import type { ReactNode } from "react";

import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogHeader,
  AlertDialogTitle,
} from "../../components/ui/alert-dialog.tsx";
import { useI18n } from "../../i18n/useI18n.ts";

/**
 * A dialog of the member page that holds a form (the invitation, the role). The repository has
 * no non-alert dialog yet (design.md 「2」 names a `Dialog` for it), so it is the alert dialog's
 * primitive, which also keeps focus inside and closes on Escape, with the design's close button.
 * It does not close on a click outside, which is what a form with typed input needs, and neither
 * Escape nor the buttons close it while a request is running (`busy`).
 */
export function MemberDialog({
  open,
  onOpenChange,
  title,
  description,
  busy,
  onOpenAutoFocus,
  onCloseAutoFocus,
  children,
}: {
  open: boolean;
  onOpenChange(open: boolean): void;
  title: string;
  description: string;
  busy: boolean;
  /** Where focus goes when the dialog opens; the first field, not the Cancel button the primitive picks. */
  onOpenAutoFocus?: (event: Event) => void;
  onCloseAutoFocus?: (event: Event) => void;
  children: ReactNode;
}) {
  const { t } = useI18n();
  return (
    <AlertDialog
      open={open}
      onOpenChange={(next) => {
        if (busy) return;
        onOpenChange(next);
      }}
    >
      <AlertDialogContent
        onEscapeKeyDown={(event) => {
          if (busy) event.preventDefault();
        }}
        {...(onOpenAutoFocus ? { onOpenAutoFocus } : {})}
        {...(onCloseAutoFocus ? { onCloseAutoFocus } : {})}
      >
        <AlertDialogHeader className="pr-8">
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription>{description}</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogCancel
          disabled={busy}
          aria-label={t("members.dialog.close")}
          className="absolute top-3 right-3 size-8 border-0 p-0"
        >
          <XIcon />
        </AlertDialogCancel>
        {children}
      </AlertDialogContent>
    </AlertDialog>
  );
}
