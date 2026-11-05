import type { FormEvent, ReactNode, RefObject } from "react";

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
import { cn } from "../../lib/utils.ts";

/**
 * A dialog with a form (docs/design/ms4-ms5/machines.html: changeover, backfill and correction,
 * assignment). It is the alert dialog's primitive (the repository has no other dialog yet) with
 * `role="dialog"`, and on a phone it is a sheet at the bottom of the screen. It cannot be closed
 * while the request runs; the focus starts on `initialFocus` (the field the user types into
 * first), not on Cancel.
 */
export function FormDialog({
  open,
  onOpenChange,
  title,
  description,
  wide = false,
  initialFocus,
  pending,
  submitLabel,
  pendingLabel,
  submitIcon,
  onSubmit,
  children,
}: {
  open: boolean;
  onOpenChange(open: boolean): void;
  title: string;
  description: ReactNode;
  wide?: boolean;
  initialFocus?: RefObject<HTMLElement | null>;
  pending: boolean;
  submitLabel: string;
  pendingLabel: string;
  submitIcon?: ReactNode;
  onSubmit(): void;
  children: ReactNode;
}) {
  const { t } = useI18n();
  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (!pending) onSubmit();
  };
  return (
    <AlertDialog
      open={open}
      onOpenChange={(next) => {
        if (pending && !next) return;
        onOpenChange(next);
      }}
    >
      <AlertDialogContent
        role="dialog"
        onOpenAutoFocus={(event) => {
          if (initialFocus?.current) {
            event.preventDefault();
            initialFocus.current.focus();
          }
        }}
        className={cn(
          "max-h-[calc(100dvh-2rem)] overflow-y-auto",
          wide ? "sm:max-w-xl" : "sm:max-w-md",
          // The phone's bottom sheet.
          "max-sm:top-auto max-sm:bottom-0 max-sm:max-w-full max-sm:translate-y-0 max-sm:rounded-b-none",
        )}
      >
        <form onSubmit={submit} className="grid gap-4" noValidate>
          <AlertDialogHeader>
            <AlertDialogTitle>{title}</AlertDialogTitle>
            <AlertDialogDescription asChild>
              <div className="grid gap-2">{description}</div>
            </AlertDialogDescription>
          </AlertDialogHeader>
          {children}
          <AlertDialogFooter>
            <AlertDialogCancel disabled={pending}>{t("machines.cancel")}</AlertDialogCancel>
            <Button type="submit" disabled={pending}>
              {pending ? <Spinner /> : submitIcon}
              {pending ? pendingLabel : submitLabel}
            </Button>
          </AlertDialogFooter>
        </form>
      </AlertDialogContent>
    </AlertDialog>
  );
}
