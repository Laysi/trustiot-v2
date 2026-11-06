import { useMutation } from "@tanstack/react-query";
import { TriangleAlertIcon } from "lucide-react";
import { useEffect, useState } from "react";

import { ApiError } from "../../api/errors.ts";
import { useMembersApi } from "../../api/pending/MembersApiProvider.tsx";
import type { Member, MemberRole } from "../../api/pending/members.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { MemberDialog } from "./MemberDialog.tsx";
import { RoleChoice } from "./RoleChoice.tsx";

/**
 * Changing a member's role (docs/design/tenant-admin/members--role-last-admin). A dialog and not
 * a dropdown in the table, because the change applies at once. The server's refusal (the last
 * admin) is translated and shown in the dialog, which stays open; changing one's own role warns
 * first that only another admin can undo it.
 */
export function ChangeRoleDialog({
  member,
  isSelf,
  onClose,
  onChanged,
  onCloseAutoFocus,
}: {
  /** The member whose role is being changed; `null` while the dialog is closed. */
  member: Member | null;
  isSelf: boolean;
  onClose(): void;
  onChanged(member: Member): void;
  onCloseAutoFocus: (event: Event) => void;
}) {
  const { t, errorMessage } = useI18n();
  const api = useMembersApi();
  const [role, setRole] = useState<MemberRole>("viewer");
  const userId = member?.user_id;
  const current = member?.role;

  const change = useMutation({
    mutationFn: (request: { userId: string; role: MemberRole }) => api.changeRole(request.userId, request.role),
    onSuccess: (changed) => {
      onClose();
      onChanged(changed);
    },
  });

  // Opened for a member: start from their role, with no earlier refusal.
  const { reset } = change;
  useEffect(() => {
    if (userId === undefined || current === undefined) return;
    reset();
    setRole(current);
  }, [userId, current, reset]);

  // A dialog that is closing keeps showing the member it was for.
  const [shown, setShown] = useState<Member | null>(member);
  useEffect(() => {
    if (member) setShown(member);
  }, [member]);

  const name = shown ? (shown.name ?? shown.email) : "";
  const loses = isSelf && shown?.role === "admin" && role !== "admin";
  const refusal = change.isError ? change.error : null;

  return (
    <MemberDialog
      open={member !== null}
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      title={t(isSelf ? "members.role_dialog.title_self" : "members.role_dialog.title", { name })}
      description={t("members.role_dialog.description")}
      busy={change.isPending}
      onCloseAutoFocus={onCloseAutoFocus}
    >
      <form
        noValidate
        className="grid gap-4"
        onSubmit={(event) => {
          event.preventDefault();
          if (member) change.mutate({ userId: member.user_id, role });
        }}
      >
        <RoleChoice value={role} onChange={setRole} disabled={change.isPending} />
        {loses ? (
          <Alert role="status">
            <TriangleAlertIcon aria-hidden="true" />
            <p className="col-start-2 font-medium">{t("members.role_dialog.self_title")}</p>
            <AlertDescription>
              <p>{t("members.role_dialog.self_body")}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        {refusal ? (
          <Alert variant="destructive">
            <TriangleAlertIcon aria-hidden="true" />
            <p className="col-start-2 font-medium">{t("members.role_dialog.failed")}</p>
            <AlertDescription>
              <p>{errorMessage(refusal)}</p>
              {refusal instanceof ApiError && refusal.body ? (
                <p className="text-xs">{t("members.error_code", { code: refusal.body.message_key })}</p>
              ) : null}
            </AlertDescription>
          </Alert>
        ) : null}
        <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
          <Button type="button" variant="outline" disabled={change.isPending} onClick={onClose}>
            {t("members.dialog.cancel")}
          </Button>
          <Button type="submit" disabled={change.isPending || role === shown?.role}>
            {change.isPending ? <Spinner /> : null}
            {change.isPending ? t("members.role_dialog.saving") : t("members.role_dialog.submit")}
          </Button>
        </div>
      </form>
    </MemberDialog>
  );
}
