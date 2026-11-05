import { useMutation } from "@tanstack/react-query";
import { useEffect, useState } from "react";

import { useMembersApi } from "../../api/pending/MembersApiProvider.tsx";
import type { Member } from "../../api/pending/members.ts";
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
import { ROLE_LABEL } from "./roles.tsx";

/**
 * Removing a member (docs/design/tenant-admin/members--remove-confirm): an alert dialog whose
 * focus starts on Cancel and whose button names the action and the member. A refusal (the last
 * admin) is shown in the dialog, which stays open; it cannot be closed while the request runs.
 */
export function RemoveMemberDialog({
  member,
  tenantName,
  onClose,
  onRemoved,
  onCloseAutoFocus,
}: {
  /** The member to remove; `null` while the dialog is closed. */
  member: Member | null;
  tenantName: string;
  onClose(): void;
  onRemoved(member: Member): void;
  onCloseAutoFocus: (event: Event) => void;
}) {
  const { t, errorMessage } = useI18n();
  const api = useMembersApi();
  const remove = useMutation({
    mutationFn: (userId: string) => api.removeMember(userId),
  });
  const { reset } = remove;
  const [shown, setShown] = useState<Member | null>(member);
  useEffect(() => {
    if (!member) return;
    reset();
    setShown(member);
  }, [member, reset]);

  const name = shown ? (shown.name ?? shown.email) : "";
  return (
    <AlertDialog
      open={member !== null}
      onOpenChange={(open) => {
        if (!open && !remove.isPending) onClose();
      }}
    >
      <AlertDialogContent onCloseAutoFocus={onCloseAutoFocus}>
        <AlertDialogHeader>
          <AlertDialogTitle>{t("members.remove.title", { name, tenant: tenantName })}</AlertDialogTitle>
          <AlertDialogDescription>{t("members.remove.body", { name })}</AlertDialogDescription>
        </AlertDialogHeader>
        {shown ? (
          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
            <dt className="text-muted-foreground">{t("members.remove.email")}</dt>
            <dd className="wrap-anywhere">{shown.email}</dd>
            <dt className="text-muted-foreground">{t("members.remove.role")}</dt>
            <dd>{t(ROLE_LABEL[shown.role])}</dd>
          </dl>
        ) : null}
        {remove.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(remove.error)}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={remove.isPending}>{t("members.dialog.cancel")}</AlertDialogCancel>
          <Button
            variant="destructive"
            disabled={remove.isPending}
            onClick={() => {
              if (!member) return;
              remove.mutate(member.user_id, {
                onSuccess: () => {
                  onClose();
                  onRemoved(member);
                },
              });
            }}
          >
            {remove.isPending ? <Spinner /> : null}
            {remove.isPending ? t("members.remove.removing") : t("members.remove.confirm", { name })}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
