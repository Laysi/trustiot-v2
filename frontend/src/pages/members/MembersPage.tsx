import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowUpCircleIcon,
  DiamondIcon,
  EllipsisIcon,
  MailIcon,
  PlusIcon,
  ShieldIcon,
  UserPlusIcon,
  XIcon,
} from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { classifyError } from "../../api/errors.ts";
import { useMembersApi } from "../../api/pending/MembersApiProvider.tsx";
import type { Invitation, Member, MemberList } from "../../api/pending/members.ts";
import { SETTINGS_PATH } from "../../app/paths.ts";
import { EmptyState } from "../../components/EmptyState.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "../../components/ui/card.tsx";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "../../components/ui/dropdown-menu.tsx";
import type { MessageId } from "../../i18n/messages.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { tenantName } from "../../session/tenant-name.ts";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { StatusPage } from "../StatusPage.tsx";
import { ChangeRoleDialog } from "./ChangeRoleDialog.tsx";
import { useDateFormat } from "./format.ts";
import { InviteDialog } from "./InviteDialog.tsx";
import { INVITE_PARAM } from "./paths.ts";
import { adminNamesKey, memberListKey } from "./queries.ts";
import { RemoveMemberDialog } from "./RemoveMemberDialog.tsx";
import { Badge, ROLE_LABEL, ROLE_SUMMARY, ROLES } from "./roles.tsx";
import { Toast } from "./Toast.tsx";
import { UsageMeter } from "./UsageMeter.tsx";

const PLAN_LABEL: Readonly<Record<MemberList["plan"], MessageId>> = {
  starter: "members.plan.starter",
  business: "members.plan.business",
  enterprise: "members.plan.enterprise",
  enterprise_on_prem: "members.plan.enterprise_on_prem",
};

const displayName = (member: Pick<Member, "name" | "email">) => member.name ?? member.email;

/**
 * The tenant's members (`/members`, checklist A3, docs/design/tenant-admin/design.md 「3.1」): the
 * members, the invitations waiting for an answer, and the seats the plan allows. Only an admin
 * has it; anyone else who opens the link is told who to ask. Every write goes through the member
 * API (api/pending/members.ts), which has no service behind it yet (Laysi/trustiot-v2#105).
 */
export function MembersPage() {
  const role = useSession().session?.activeTenant?.role;
  return role === "admin" ? <MembersAdmin /> : <MembersDenied role={role} />;
}

/** The page a viewer or editor gets (decision 1): why, and the names of the people who can help. */
function MembersDenied({ role }: { role: Member["role"] | undefined }) {
  const { t } = useI18n();
  const tenant = useSession().session?.activeTenant;
  const api = useMembersApi();
  const admins = useQuery({
    queryKey: adminNamesKey(tenant?.id ?? ""),
    queryFn: ({ signal }) => api.listAdmins(signal),
  });
  const names = admins.data ?? [];
  const where = tenant ? tenantName(tenant, t) : "";
  const roleName = role ? t(ROLE_LABEL[role]) : "";
  return (
    <StatusPage title={t("members.denied.title")} icon={<ShieldIcon />}>
      <p>
        {names.length > 0
          ? t("members.denied.body_admins", {
              tenant: where,
              role: roleName,
              admins: names.join(t("members.denied.separator")),
            })
          : t("members.denied.body", { tenant: where, role: roleName })}
      </p>
    </StatusPage>
  );
}

type Notice = { id: number; title: string; body: string };

function MembersAdmin() {
  const { t, errorMessage } = useI18n();
  const session = useSession().session;
  const tenant = session?.activeTenant ?? null;
  const tenantId = tenant?.id ?? "";
  const selfId = session?.user.id;
  const selfEmail = session?.user.email?.toLowerCase();
  // The signed-in member: by ID, and by email too, which the member list always carries.
  const isSelf = (member: Member) =>
    member.user_id === selfId || (selfEmail !== undefined && member.email.toLowerCase() === selfEmail);
  const api = useMembersApi();
  const queryClient = useQueryClient();
  const [params, setParams] = useSearchParams();
  const formatDate = useDateFormat();

  const list = useQuery({
    queryKey: memberListKey(tenantId),
    queryFn: ({ signal }) => api.listMembers(signal),
  });
  const refresh = () => queryClient.invalidateQueries({ queryKey: memberListKey(tenantId) });

  // The address asks for the dialog (`?invite`); closing it takes the parameter out.
  const [inviteOpen, setInviteOpen] = useState(() => params.has(INVITE_PARAM));
  // A link to `?invite` followed while the page is open (an answer's link) opens it too.
  const asksToInvite = params.has(INVITE_PARAM);
  useEffect(() => {
    if (asksToInvite) setInviteOpen(true);
  }, [asksToInvite]);
  const [roleFor, setRoleFor] = useState<Member | null>(null);
  const [removing, setRemoving] = useState<Member | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [highlight, setHighlight] = useState<string | null>(null);
  const [actionError, setActionError] = useState<unknown>(null);
  const inviteButton = useRef<HTMLButtonElement>(null);
  const membersHeading = useRef<HTMLHeadingElement>(null);
  const noticeId = useRef(0);

  const announce = (title: string, body: string) => {
    noticeId.current += 1;
    setNotice({ id: noticeId.current, title, body });
  };
  const setInvite = (open: boolean) => {
    setInviteOpen(open);
    if (!open && params.has(INVITE_PARAM)) {
      setParams(
        (current) => {
          const next = new URLSearchParams(current);
          next.delete(INVITE_PARAM);
          return next;
        },
        { replace: true },
      );
    }
  };

  const resend = useMutation({
    mutationFn: (invitation: Invitation) => api.resendInvitation(invitation.invitation_id),
    onMutate: () => setActionError(null),
    onSuccess: (invitation) => {
      announce(t("members.toast.resent", { email: invitation.email }), t("members.toast.resent_body"));
      void refresh();
    },
    onError: setActionError,
  });
  const revoke = useMutation({
    mutationFn: (invitation: Invitation) => api.revokeInvitation(invitation.invitation_id),
    onMutate: () => setActionError(null),
    onSuccess: (_result, invitation) => {
      announce(t("members.toast.revoked", { email: invitation.email }), t("members.toast.revoked_body"));
      void refresh();
    },
    onError: setActionError,
  });

  const data = list.data;
  const full = data !== undefined && data.quota.limit !== null && data.quota.used >= data.quota.limit;
  const canInvite = data !== undefined && !full;
  const planName = data ? t(PLAN_LABEL[data.plan]) : "";
  const quotaNote = "quota-note";

  const toFocus = (target: React.RefObject<HTMLElement | null>) => (event: Event) => {
    event.preventDefault();
    target.current?.focus();
  };

  let body: ReactNode;
  if (list.isPending) {
    body = <ListSkeleton label={t("members.loading")} heading={t("members.card.title")} />;
  } else if (list.isError) {
    body = (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("members.load_failed", { reason: errorMessage(list.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void list.refetch()} disabled={list.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  } else {
    const alone = list.data.members.length === 1 && list.data.invitations.length === 0;
    body = (
      <>
        {full ? (
          <div id={quotaNote}>
            <Alert role="status" className="border-warning/50 bg-warning-soft">
              <ArrowUpCircleIcon aria-hidden="true" className="text-warning" />
              <p className="col-start-2 font-medium">
                {t("members.quota_full.title", {
                  plan: planName,
                  used: list.data.quota.used,
                  limit: list.data.quota.limit ?? 0,
                })}
              </p>
              <AlertDescription>
                <p>{t("members.quota_full.body")}</p>
                <Button asChild className="mt-2">
                  <Link to={SETTINGS_PATH}>
                    <ArrowUpCircleIcon />
                    {t("members.quota_full.action")}
                  </Link>
                </Button>
              </AlertDescription>
            </Alert>
          </div>
        ) : null}
        <UsageMeter
          label={t("members.quota.label", { plan: planName })}
          used={list.data.quota.used}
          limit={list.data.quota.limit}
          note={t("members.quota.note")}
          ariaLabel={t("members.quota.bar", { used: list.data.quota.used, limit: list.data.quota.limit ?? 0 })}
        />
        <MemberCard
          members={list.data.members}
          isSelf={isSelf}
          heading={membersHeading}
          tenantLabel={tenant ? tenantName(tenant, t) : ""}
          formatDate={formatDate}
          onChangeRole={setRoleFor}
          onRemove={setRemoving}
        />
        {alone ? (
          <EmptyState
            icon={<UserPlusIcon />}
            title={t("members.empty.title")}
            action={
              <Button disabled={!canInvite} onClick={() => setInvite(true)}>
                <PlusIcon />
                {t("members.empty.action")}
              </Button>
            }
          >
            <p>{t("members.empty.body")}</p>
          </EmptyState>
        ) : null}
        {list.data.invitations.length > 0 ? (
          <InvitationCard
            invitations={list.data.invitations}
            highlight={highlight}
            formatDate={formatDate}
            busy={resend.isPending || revoke.isPending}
            onResend={(invitation) => resend.mutate(invitation)}
            onRevoke={(invitation) => revoke.mutate(invitation)}
          />
        ) : null}
        <RoleHelp />
      </>
    );
  }

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-5">
      <PageHeader
        title={t("members.title")}
        crumbs={[{ label: t("settings.title"), to: SETTINGS_PATH }]}
        meta={t("members.description", { tenant: tenant ? tenantName(tenant, t) : "" })}
        actions={
          <Button
            ref={inviteButton}
            disabled={!canInvite}
            {...(full ? { "aria-describedby": quotaNote } : {})}
            onClick={() => setInvite(true)}
          >
            <PlusIcon />
            {t("members.invite.open")}
          </Button>
        }
      />
      {actionError !== null ? (
        <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
          <AlertDescription className="col-start-1">
            <p>{errorMessage(actionError)}</p>
          </AlertDescription>
          <Button
            variant="ghost"
            size="icon"
            aria-label={t("members.toast.dismiss")}
            onClick={() => setActionError(null)}
          >
            <XIcon />
          </Button>
        </Alert>
      ) : null}
      {body}

      <InviteDialog
        open={inviteOpen && canInvite}
        onOpenChange={setInvite}
        onInvited={(invitation) => {
          setHighlight(invitation.invitation_id);
          announce(t("members.toast.invited", { email: invitation.email }), t("members.toast.invited_body"));
          void refresh();
        }}
        onFailed={(error) => {
          // The plan's limit was reached since the page was read: show the upgrade notice now.
          if (classifyError(error) === "other") void refresh();
        }}
        onCloseAutoFocus={toFocus(inviteButton)}
      />
      <ChangeRoleDialog
        member={roleFor}
        isSelf={roleFor !== null && isSelf(roleFor)}
        onClose={() => setRoleFor(null)}
        onChanged={(member) => {
          announce(
            t("members.toast.role_changed", { name: displayName(member), role: t(ROLE_LABEL[member.role]) }),
            t("members.toast.role_changed_body"),
          );
          void refresh();
        }}
        onCloseAutoFocus={toFocus(membersHeading)}
      />
      <RemoveMemberDialog
        member={removing}
        tenantName={tenant ? tenantName(tenant, t) : ""}
        onClose={() => setRemoving(null)}
        onRemoved={(member) => {
          announce(t("members.toast.removed", { name: displayName(member) }), t("members.toast.removed_body"));
          void refresh();
        }}
        onCloseAutoFocus={toFocus(membersHeading)}
      />
      <Toast
        notice={notice}
        dismissLabel={t("members.toast.dismiss")}
        onDismiss={() => {
          setNotice(null);
          setHighlight(null);
        }}
      />
    </div>
  );
}

/** Rows that stand in for the cards while the list loads; announced once, not row by row. */
function ListSkeleton({ label, heading }: { label: string; heading: string }) {
  return (
    <Card aria-busy="true">
      <CardHeader>
        <CardTitle>{heading}</CardTitle>
      </CardHeader>
      <CardContent className="grid gap-4">
        <p role="status" className="sr-only">
          {label}
        </p>
        {[0, 1, 2, 3].map((row) => (
          <div key={row} aria-hidden="true" className="flex items-center justify-between gap-4">
            <span className="h-3 w-[32%] animate-pulse rounded bg-muted" />
            <span className="h-3 w-[14%] animate-pulse rounded bg-muted" />
            <span className="h-3 w-[16%] animate-pulse rounded bg-muted" />
          </div>
        ))}
      </CardContent>
    </Card>
  );
}

function MemberCard({
  members,
  isSelf,
  heading,
  tenantLabel,
  formatDate,
  onChangeRole,
  onRemove,
}: {
  members: readonly Member[];
  isSelf(member: Member): boolean;
  heading: React.RefObject<HTMLHeadingElement | null>;
  tenantLabel: string;
  formatDate(instant: string): string;
  onChangeRole(member: Member): void;
  onRemove(member: Member): void;
}) {
  const { t } = useI18n();
  const identity = (member: Member) => (
    <span className="flex min-w-0 items-center gap-2">
      <span className="grid min-w-0">
        <span className="font-medium wrap-anywhere">{displayName(member)}</span>
        {member.name !== null ? (
          <span className="text-xs wrap-anywhere text-muted-foreground">{member.email}</span>
        ) : null}
      </span>
      {isSelf(member) ? <Badge strong>{t("members.you")}</Badge> : null}
    </span>
  );
  const menu = (member: Member) => (
    <RowMenu
      member={member}
      isSelf={isSelf(member)}
      onChangeRole={() => onChangeRole(member)}
      onRemove={() => onRemove(member)}
    />
  );
  return (
    <Card className="gap-0 py-0">
      <CardHeader className="py-5">
        <CardTitle ref={heading} tabIndex={-1} className="outline-none">
          {t("members.card.title")}
        </CardTitle>
        <CardDescription>{t("members.card.count", { count: members.length })}</CardDescription>
      </CardHeader>
      <table className="hidden w-full border-t text-sm sm:table">
        <caption className="sr-only">{t("members.card.caption", { tenant: tenantLabel })}</caption>
        <thead className="bg-muted/60 text-left text-muted-foreground">
          <tr>
            <th scope="col" className="px-6 py-2.5 font-medium">
              {t("members.column.member")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium">
              {t("members.column.role")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
              {t("members.column.joined")}
            </th>
            <th scope="col" className="px-6 py-2.5">
              <span className="sr-only">{t("members.column.actions")}</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {members.map((member) => (
            <tr key={member.user_id} className="border-t">
              <td className="max-w-80 px-6 py-3">{identity(member)}</td>
              <td className="px-3 py-3">
                <Badge strong={member.role === "admin"}>{t(ROLE_LABEL[member.role])}</Badge>
              </td>
              <td className="px-3 py-3 whitespace-nowrap text-muted-foreground">{formatDate(member.joined_at)}</td>
              <td className="w-px px-6 py-2">{menu(member)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <ul aria-label={t("members.card.caption", { tenant: tenantLabel })} className="grid divide-y border-t sm:hidden">
        {members.map((member) => (
          <li key={member.user_id} className="grid gap-2 px-4 py-3 text-sm">
            <div className="flex items-start justify-between gap-2">
              {identity(member)}
              {menu(member)}
            </div>
            <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
              <Badge strong={member.role === "admin"}>{t(ROLE_LABEL[member.role])}</Badge>
              <span>{t("members.joined_on", { date: formatDate(member.joined_at) })}</span>
            </div>
          </li>
        ))}
      </ul>
    </Card>
  );
}

function RowMenu({
  member,
  isSelf,
  onChangeRole,
  onRemove,
}: {
  member: Member;
  isSelf: boolean;
  onChangeRole(): void;
  onRemove(): void;
}) {
  const { t } = useI18n();
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" aria-label={t("members.row.manage", { name: displayName(member) })}>
          <EllipsisIcon />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuItem onSelect={onChangeRole}>{t("members.row.change_role")}</DropdownMenuItem>
        {/* Leaving the organization is not this page's action: the user's own row has no removal. */}
        {isSelf ? null : (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuItem variant="destructive" onSelect={onRemove}>
              {t("members.row.remove")}
            </DropdownMenuItem>
          </>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function InvitationCard({
  invitations,
  highlight,
  formatDate,
  busy,
  onResend,
  onRevoke,
}: {
  invitations: readonly Invitation[];
  highlight: string | null;
  formatDate(instant: string): string;
  busy: boolean;
  onResend(invitation: Invitation): void;
  onRevoke(invitation: Invitation): void;
}) {
  const { t } = useI18n();
  const now = Date.now();
  const expired = (invitation: Invitation) => new Date(invitation.expires_at).getTime() <= now;
  const expiry = (invitation: Invitation) =>
    expired(invitation) ? (
      <span className="inline-flex items-center gap-1 text-warning">
        <DiamondIcon aria-hidden="true" className="size-3 fill-current" />
        {t("members.invitations.expired", { date: formatDate(invitation.expires_at) })}
      </span>
    ) : (
      formatDate(invitation.expires_at)
    );
  const actions = (invitation: Invitation) => (
    <div className="flex items-center gap-1">
      <Button variant="ghost" size="sm" disabled={busy} onClick={() => onResend(invitation)}>
        <MailIcon />
        {t(expired(invitation) ? "members.invitations.reinvite" : "members.invitations.resend")}
      </Button>
      <Button
        variant="ghost"
        size="sm"
        disabled={busy}
        aria-label={t("members.invitations.revoke_label", { email: invitation.email })}
        onClick={() => onRevoke(invitation)}
      >
        {t("members.invitations.revoke")}
      </Button>
    </div>
  );
  const rowClass = (invitation: Invitation) =>
    cn(invitation.invitation_id === highlight && "bg-brand-soft shadow-[inset_3px_0_0_var(--brand)]");
  return (
    <Card className="gap-0 py-0">
      <CardHeader className="py-5">
        <CardTitle>{t("members.invitations.title")}</CardTitle>
        <CardDescription>{t("members.invitations.description")}</CardDescription>
      </CardHeader>
      <table className="hidden w-full border-t text-sm sm:table">
        <caption className="sr-only">{t("members.invitations.title")}</caption>
        <thead className="bg-muted/60 text-left text-muted-foreground">
          <tr>
            <th scope="col" className="px-6 py-2.5 font-medium">
              {t("members.invitations.column.email")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium">
              {t("members.column.role")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium">
              {t("members.invitations.column.invited_by")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium">
              {t("members.invitations.column.expires")}
            </th>
            <th scope="col" className="px-6 py-2.5">
              <span className="sr-only">{t("members.column.actions")}</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {invitations.map((invitation) => (
            <tr key={invitation.invitation_id} className={cn("border-t", rowClass(invitation))}>
              <td className="max-w-72 px-6 py-3 wrap-anywhere">{invitation.email}</td>
              <td className="px-3 py-3">
                <Badge>{t(ROLE_LABEL[invitation.role])}</Badge>
              </td>
              <td className="px-3 py-3 text-muted-foreground">{invitation.invited_by}</td>
              <td className="px-3 py-3 whitespace-nowrap text-muted-foreground">{expiry(invitation)}</td>
              <td className="w-px px-4 py-2">{actions(invitation)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <ul aria-label={t("members.invitations.title")} className="grid divide-y border-t sm:hidden">
        {invitations.map((invitation) => (
          <li key={invitation.invitation_id} className={cn("grid gap-2 px-4 py-3 text-sm", rowClass(invitation))}>
            <span className="font-medium wrap-anywhere">{invitation.email}</span>
            <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
              <Badge>{t(ROLE_LABEL[invitation.role])}</Badge>
              <span>{t("members.invitations.invited_by_line", { name: invitation.invited_by })}</span>
              <span>
                {expired(invitation)
                  ? expiry(invitation)
                  : t("members.invitations.expires_line", { date: formatDate(invitation.expires_at) })}
              </span>
            </div>
            {actions(invitation)}
          </li>
        ))}
      </ul>
    </Card>
  );
}

/** What each role may do, folded away until wanted. */
function RoleHelp() {
  const { t } = useI18n();
  return (
    <details className="rounded-xl border bg-card px-5 py-3.5 text-sm text-card-foreground">
      <summary className="cursor-pointer font-medium">{t("members.help.title")}</summary>
      <dl className="mt-3 grid gap-2">
        {[...ROLES].reverse().map((role) => (
          <div key={role} className="grid gap-0.5">
            <dt className="font-medium">{t(ROLE_LABEL[role])}</dt>
            <dd className="text-muted-foreground">{t(ROLE_SUMMARY[role])}</dd>
          </div>
        ))}
      </dl>
      <p className="mt-3 text-muted-foreground">{t("members.help.last_admin")}</p>
    </details>
  );
}
