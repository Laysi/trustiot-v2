import { usePlatformApi } from "../../../api/pending/PlatformApiProvider.tsx";
import type { MemberRole } from "../../../api/pending/platform.ts";
import { useI18n } from "../../../i18n/useI18n.ts";
import { ActionDialog } from "../ui/ActionDialog.tsx";
import { ChangePreview } from "../ui/change-preview.tsx";

/**
 * Confirms setting a user's role in a tenant (member rescue, docs/design/platform-admin/
 * tenant-actions.html): shows the role now (or "not a member") and after, and says that the tenant's
 * admins see the change in their operation log as the platform administrator's. A user with no
 * account is refused by the server (422 `member.email_unknown`): the dialog says so on the email.
 */
export function MemberRoleDialog(props: {
  tenantId: string;
  tenantName: string;
  email: string;
  role: MemberRole;
  /** The role the user has now; `null` when they are not a member. */
  current: MemberRole | null;
  open: boolean;
  onOpenChange(open: boolean): void;
  onDone(message: string): void;
  returnFocus?: () => HTMLElement | null;
}) {
  const { t, fieldError } = useI18n();
  const api = usePlatformApi();
  const joining = props.current === null;
  return (
    <ActionDialog
      open={props.open}
      onOpenChange={props.onOpenChange}
      title={t(joining ? "platform.member.title_join" : "platform.member.title", {
        email: props.email,
        role: props.role,
      })}
      description={t("platform.member.description", { tenant: props.tenantName })}
      audit={t("platform.member.audit")}
      confirmLabel={t(joining ? "platform.member.confirm_join" : "platform.member.confirm", { role: props.role })}
      pendingLabel={t("platform.dialog.working")}
      returnFocus={props.returnFocus}
      describeError={(error) => fieldError(error, "/email")}
      run={() => api.setMemberRole(props.tenantId, { email: props.email, role: props.role })}
      onDone={() => props.onDone(t("platform.member.done", { email: props.email, role: props.role }))}
    >
      <ChangePreview
        label={t("platform.member.role")}
        from={props.current ?? t("platform.member.not_member")}
        to={props.role}
      />
      <p className="text-sm text-muted-foreground">{t("platform.member.token_note")}</p>
    </ActionDialog>
  );
}
