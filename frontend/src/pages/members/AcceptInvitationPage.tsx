import { useMutation, useQuery } from "@tanstack/react-query";
import { BanIcon, LogOutIcon, MailXIcon, UsersRoundIcon } from "lucide-react";
import { useEffect, useId, useState } from "react";
import { flushSync } from "react-dom";
import { Link, useLocation, useNavigate } from "react-router";

import { ApiError, classifyError } from "../../api/errors.ts";
import { useMembersApi } from "../../api/pending/MembersApiProvider.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { useSignOut } from "../../session/useSignOut.ts";
import { StatusPage } from "../StatusPage.tsx";
import { useDateFormat } from "./format.ts";
import { ROLE_LABEL, ROLE_SUMMARY } from "./roles.tsx";

/** The message keys the member API refuses an invitation link with (api/pending/members.ts). */
const MISMATCH_KEY = "invitation.account_mismatch";

function isMismatch(error: unknown): boolean {
  return error instanceof ApiError && error.body?.message_key === MISMATCH_KEY;
}

/** Expired, revoked, used or never existed: one answer, so the page does not tell which. */
function isInvalid(error: unknown): boolean {
  return classifyError(error) === "not_found";
}

/**
 * Accepting an invitation (`/invitations/accept#<token>`, docs/design/tenant-admin/members--accept
 * and its two refusals). The token is the URL fragment (design decision 3): the page keeps it in
 * memory and takes it out of the address at once, so it is not in the history entry, a bookmark
 * or a screenshot of the address bar, and sends it only in the body of a POST. The page first
 * says what the user would join, as whom and with which role; a click accepts. The page is
 * outside the tenant gate, since the person it is for may belong to no organization yet.
 */
export function AcceptInvitationPage() {
  const navigate = useNavigate();
  const location = useLocation();
  // Read from the fragment, which is removed below: the token stays here for retries. A link to
  // another invitation followed while the page is open brings another fragment, and the page
  // starts over for it (the `key` below), with nothing left of the first.
  const [token, setToken] = useState(() => location.hash.replace(/^#/, ""));

  useEffect(() => {
    if (location.hash !== "") setToken(location.hash.replace(/^#/, ""));
    if (location.hash !== "" || location.search !== "") {
      void navigate({ pathname: location.pathname }, { replace: true });
    }
  }, [location.hash, location.search, location.pathname, navigate]);

  return <AcceptInvitation key={token} token={token} />;
}

function AcceptInvitation({ token }: { token: string }) {
  const { t, errorMessage } = useI18n();
  const api = useMembersApi();
  const navigate = useNavigate();
  const { session, switchTenant } = useSession();
  const signOut = useSignOut();
  const formatDate = useDateFormat();
  // This invitation's own query: the key is not the token (a key is kept in memory and shown by
  // tools), but it is not shared with an earlier invitation's either.
  const instance = useId();

  // Set once the invitation is accepted: it is spent from then on, so the preview must not be asked again
  // (switching organization resets the queries, and the page would flash the "can't be used" page).
  const [joined, setJoined] = useState(false);
  const preview = useQuery({
    queryKey: ["invitation", "preview", instance],
    queryFn: ({ signal }) => api.previewInvitation(token, signal),
    enabled: token !== "" && !joined,
    gcTime: 0,
    staleTime: Number.POSITIVE_INFINITY,
    refetchOnWindowFocus: false,
  });

  // Accepted, but the organization could not be opened; the name is kept because the preview is reset by then.
  const [joinFailure, setJoinFailure] = useState<{ error: unknown; tenant: string } | null>(null);
  const accept = useMutation({
    mutationFn: async () => {
      const accepted = await api.acceptInvitation(token);
      // Rendered now, before the switch below resets the queries.
      flushSync(() => setJoined(true));
      // The session learns of the new organization by switching to it; a failure here still left the user a member.
      try {
        await switchTenant(accepted.tenant_id);
      } catch (error) {
        setJoinFailure({ error, tenant: preview.data?.tenant_name ?? "" });
        return;
      }
      await navigate("/", { replace: true });
    },
  });

  if (joinFailure !== null) {
    return (
      <StatusPage
        title={t("members.accept.joined.title", { tenant: joinFailure.tenant })}
        icon={<UsersRoundIcon />}
        actions={
          <Button asChild>
            <Link to="/">{t("page.action.home")}</Link>
          </Button>
        }
      >
        <p>{t("members.accept.joined.body")}</p>
        <p>{errorMessage(joinFailure.error)}</p>
      </StatusPage>
    );
  }
  if (joined) {
    return (
      <div className="flex justify-center py-16">
        <Spinner label={t("members.accept.accepting")} />
      </div>
    );
  }
  if (token === "" || (preview.isError && isInvalid(preview.error)) || (accept.isError && isInvalid(accept.error))) {
    return (
      <StatusPage title={t("members.accept.invalid.title")} icon={<MailXIcon />}>
        <p>{t("members.accept.invalid.body")}</p>
      </StatusPage>
    );
  }
  if (preview.isError && isMismatch(preview.error)) {
    return (
      <StatusPage
        title={t("members.accept.mismatch.title")}
        icon={<BanIcon />}
        actions={
          <Button disabled={signOut.isPending} onClick={() => signOut.mutate("this")}>
            <LogOutIcon />
            {t("members.accept.mismatch.sign_out")}
          </Button>
        }
      >
        <p>{t("members.accept.mismatch.body", { email: session?.user.email ?? "" })}</p>
        {signOut.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(signOut.error)}</p>
            </AlertDescription>
          </Alert>
        ) : null}
      </StatusPage>
    );
  }
  if (preview.isError) {
    return (
      <StatusPage
        title={t("page.unavailable.title")}
        actions={<Button onClick={() => void preview.refetch()}>{t("page.action.retry")}</Button>}
      >
        <p>{errorMessage(preview.error)}</p>
      </StatusPage>
    );
  }
  if (preview.isPending) {
    return (
      <div className="flex justify-center py-16">
        <Spinner label={t("members.accept.loading")} />
      </div>
    );
  }

  const data = preview.data;
  const inviter = data.invited_by.name ? `${data.invited_by.name} (${data.invited_by.email})` : data.invited_by.email;
  return (
    <section
      aria-labelledby="accept-heading"
      className="mx-auto grid w-full max-w-lg gap-5 rounded-xl border bg-card p-6 text-card-foreground shadow-sm"
    >
      <div className="grid gap-1">
        <p className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          {t("members.accept.eyebrow")}
        </p>
        <h1 id="accept-heading" className="text-2xl font-semibold tracking-tight wrap-anywhere">
          {t("members.accept.title", { tenant: data.tenant_name })}
        </h1>
      </div>
      <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-sm">
        <dt className="text-muted-foreground">{t("members.accept.inviter")}</dt>
        <dd className="wrap-anywhere">{inviter}</dd>
        <dt className="text-muted-foreground">{t("members.accept.role")}</dt>
        <dd>
          {t("members.accept.role_value", { role: t(ROLE_LABEL[data.role]), summary: t(ROLE_SUMMARY[data.role]) })}
        </dd>
        <dt className="text-muted-foreground">{t("members.accept.account")}</dt>
        <dd className="wrap-anywhere">{data.email}</dd>
        <dt className="text-muted-foreground">{t("members.accept.expires")}</dt>
        <dd>{formatDate(data.expires_at)}</dd>
      </dl>
      {accept.isError ? (
        <Alert variant="destructive">
          <AlertDescription>
            <p>{errorMessage(accept.error)}</p>
          </AlertDescription>
        </Alert>
      ) : null}
      <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
        <Button variant="outline" asChild>
          <Link to="/">{t("members.accept.decline")}</Link>
        </Button>
        <Button disabled={accept.isPending} onClick={() => accept.mutate()}>
          {accept.isPending ? <Spinner /> : null}
          {accept.isPending ? t("members.accept.accepting") : t("members.accept.submit")}
        </Button>
      </div>
    </section>
  );
}
